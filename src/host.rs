//! Caller-owned process actions and resources for a library invocation.
pub use crate::process::FdAllowance;
use std::{num::NonZeroUsize, sync::Arc};

/// Explicit facilities supplied by the caller. There is deliberately no default.
#[derive(Clone)]
pub struct Host {
    pub diagnostics: Diagnostics,
    pub process: Arc<dyn ProcessActions>,
    pub resources: Arc<dyn Resources>,
    /// Whether the library's own file readers drop the input's pages from the
    /// page cache behind the read head.
    pub page_cache: PageCachePolicy,
    /// Where the library's shared rayon tasks run: writer framing and command
    /// parallel-iterator work. Dedicated pipeline workers do not run here; see
    /// [`Executor`].
    pub executor: Executor,
    /// The invocation this host runs: its cancellation request, and its
    /// instrumentation. Every command run under this host stops at its next
    /// checkpoint once the token is tripped and returns
    /// [`ErrorKind::Cancelled`](crate::ErrorKind::Cancelled). A token is
    /// sticky, its counters accumulate and its once-only diagnostics stay
    /// reported: give each independent invocation a fresh one.
    pub invocation: Invocation,
}

/// One invocation of the library: the token every part of a run holds.
///
/// Cloning shares the invocation: every clone observes
/// [`cancel`](Self::cancel) on any of them. Tripping is permanent, so a token
/// belongs to one invocation (or a group of calls meant to stop together,
/// which are then one invocation in every respect); there is no reset,
/// because workers still unwinding from the last request would race it.
///
/// The library never trips a token and never installs a signal handler
/// (ADR-0016): the caller decides what stops a run. The CLI trips its token
/// from SIGINT and SIGTERM; an embedding caller can trip one from a timeout,
/// a UI or a request deadline.
///
/// Commands observe the token only at points they already pass through
/// (a blob read or dispatched, a result received from a pool, a stage
/// boundary, a write of an output block, the publication decision) with a
/// relaxed load, so an untripped token costs nothing measurable. A tripped
/// one ends the run through the same path a failure takes: workers wind
/// down, scratch and staging are removed, nothing is published, and the
/// call returns [`ErrorKind::Cancelled`](crate::ErrorKind::Cancelled).
///
/// # Instrumentation
///
/// The invocation is also the unit of instrumentation. Every engine counter
/// a run records lands in its invocation
/// ([`for_each_counter`](Self::for_each_counter)), and every phase marker and
/// counter sample it emits goes to the invocation's
/// [`InstrumentSink`](crate::debug::InstrumentSink), chosen at construction:
/// [`new`](Self::new) has none, and the run emits nothing;
/// [`with_sink`](Self::with_sink) delivers every marker and counter of the
/// run there. The CLI hands its one invocation a
/// [`MarkerFifo`](crate::debug::MarkerFifo), the sidecar profiler's FIFO.
/// Two invocations are two streams; a caller that wants several runs on one
/// stream gives each its own sink forwarding to a shared collector, so it can
/// tell them apart.
///
/// The invocation is also the unit of the diagnostics a run reports only
/// once (see [`Diagnostics`]): every pass of the run shares its report-once
/// keys, whichever `Diagnostics` handle it reports through, so several calls
/// grouped under one invocation report each such diagnostic once between
/// them, and separate invocations each report their own.
#[derive(Clone, Default)]
pub struct Invocation(Arc<InvocationState>);

#[derive(Default)]
struct InvocationState {
    /// On cache lines of its own: every checkpoint reads it on many threads,
    /// and the `Arc`'s reference counts at the head of the allocation move
    /// whenever a pool or a framing job clones the token.
    tripped: crate::counters::Padded<std::sync::atomic::AtomicBool>,
    #[cfg(feature = "test-hooks")]
    hook: std::sync::OnceLock<CheckHook>,
    #[cfg(feature = "test-hooks")]
    checks: std::sync::atomic::AtomicU64,
    /// The invocation's fault plan: every worker loop already holds the
    /// token, so this is where a test's injected faults reach them.
    #[cfg(feature = "test-hooks")]
    faults: crate::test_hooks::FaultPlan,
    /// Every engine-level metric set of the invocation. Each set is on cache
    /// lines of its own (`counters::Padded`), apart from `tripped`.
    metrics: crate::counters::InvocationMetrics,
    /// Where the run's markers and counter samples go, if anywhere.
    sink: Option<Arc<dyn crate::debug::InstrumentSink>>,
    /// Set once the sink has panicked: later emissions skip it.
    sink_failed: std::sync::atomic::AtomicBool,
    /// The keys of the once-per-invocation diagnostics already reported (or
    /// covered by a more specific report). Touched only where such a
    /// diagnostic is about to be emitted, never on a per-element path.
    reported: std::sync::Mutex<std::collections::HashSet<&'static str>>,
    /// Held across one publication's span (`PUBLISH_START` to `PUBLISH_END`
    /// and the syncs and installs between), so two publications of one
    /// invocation never overlap and each span's `publish_sync_*` samples are
    /// its own on the stream.
    publishing: std::sync::Mutex<()>,
}

/// A test-only callback run at every checkpoint with the checkpoint's
/// 1-based ordinal; it may block (a test parks the run there until it is
/// cancelled).
#[cfg(feature = "test-hooks")]
type CheckHook = Box<dyn Fn(u64, &Invocation) + Send + Sync>;

impl std::fmt::Debug for Invocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Invocation")
            .field(&self.is_cancelled())
            .finish()
    }
}

impl Invocation {
    /// A fresh, untripped token with no instrumentation sink: the run's
    /// counters are still recorded (and readable), but nothing is emitted.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh, untripped token whose run delivers every phase marker and
    /// counter sample to `sink` (see [`InstrumentSink`](crate::debug::InstrumentSink)
    /// for its contract).
    #[must_use]
    pub fn with_sink(sink: Arc<dyn crate::debug::InstrumentSink>) -> Self {
        Self(Arc::new(InvocationState {
            sink: Some(sink),
            ..InvocationState::default()
        }))
    }

    /// Whether this invocation has a working sink. Emission sites whose value
    /// costs something to compute (an allocator query, a `/proc` sample) ask
    /// first.
    #[inline]
    #[must_use]
    pub fn instrumented(&self) -> bool {
        self.0.sink.is_some()
            && !self
                .0
                .sink_failed
                .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Emit the phase marker `name` to the sink, if there is one.
    #[inline]
    pub(crate) fn marker(&self, name: &str) {
        if self.0.sink.is_some() {
            self.deliver(|sink| sink.marker(name));
        }
    }

    /// Emit counter `name` with `value` to the sink, if there is one. The
    /// wire value is an `i64`: signed values are sent exactly, unsigned ones
    /// saturate at `i64::MAX` rather than wrap, so callers pass them as they
    /// are instead of casting.
    #[inline]
    pub(crate) fn counter(&self, name: &str, value: impl crate::debug::CounterValue) {
        if self.0.sink.is_some() {
            let value = value.to_counter();
            self.deliver(|sink| sink.counter(name, value));
        }
    }

    /// Run `emit` against the sink with its unwinding contained: a sink that
    /// panics is latched off for the rest of the invocation and the run
    /// carries on, because instrumentation is observational and may neither
    /// change a published result nor turn an unwind into an abort. The
    /// caught payload is forgotten, not dropped: its destructor is caller
    /// code too and could panic again. An emission already past the latch
    /// when another thread's panics may still complete; later ones are
    /// skipped. Panics that do not unwind (`panic = "abort"`, a panicking
    /// panic hook) cannot be contained.
    #[cold]
    #[inline(never)]
    fn deliver(&self, emit: impl FnOnce(&dyn crate::debug::InstrumentSink)) {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(sink) = self.0.sink.as_deref() else {
            return;
        };
        if self.0.sink_failed.load(Relaxed) {
            return;
        }
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| emit(sink)))
        {
            self.0.sink_failed.store(true, Relaxed);
            std::mem::forget(payload);
        }
    }

    /// Request cancellation. Every run observing this token (or a clone)
    /// stops at its next checkpoint. Idempotent, lock-free and safe to call
    /// from any thread.
    pub fn cancel(&self) {
        self.0
            .tripped
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Whether cancellation has been requested. An `Acquire` load: whatever
    /// the tripping thread did before [`cancel`](Self::cancel) is visible
    /// once this returns `true`.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.tripped.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The checkpoint: `Err(Cancelled)` once the token is tripped. A relaxed
    /// load and a branch, called only where the run already passes. Public
    /// so a caller's own work between library calls (a CLI mode that reads
    /// only a header, a loop over several commands) can stop with the same
    /// typed error the commands return.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Cancelled`](crate::ErrorKind::Cancelled) once the token
    /// is tripped.
    #[inline]
    pub fn check(&self) -> crate::Result<()> {
        #[cfg(feature = "test-hooks")]
        self.run_hook();
        if self.0.tripped.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(crate::error::new_cancelled());
        }
        Ok(())
    }

    /// Run a read-only command's `body` between its entry check and its
    /// completion decision: a token tripped before the call, or by the time
    /// the work is done, makes the call return `Cancelled`; one tripped after
    /// that leaves the completed result standing. (A command with an output
    /// has its completion decision in publication instead.)
    pub(crate) fn decided<T>(&self, body: impl FnOnce() -> crate::Result<T>) -> crate::Result<T> {
        self.check()?;
        let value = body()?;
        self.check()?;
        Ok(value)
    }

    /// Whether `other` is this invocation (a clone of the same token).
    pub(crate) fn same_invocation(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// The invocation's metric sets, for the recording sites.
    #[inline]
    pub(crate) fn metrics(&self) -> &crate::counters::InvocationMetrics {
        &self.0.metrics
    }

    /// Claim the once-per-invocation diagnostic `key`: `true` the first time
    /// any part of this invocation asks (the caller then emits it), `false`
    /// ever after, and after [`mark_reported`](Self::mark_reported). The
    /// lock is released before the caller emits, so a slow diagnostic sink
    /// never holds it.
    pub(crate) fn first_report(&self, key: &'static str) -> bool {
        self.0
            .reported
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key)
    }

    /// Record that the once-per-invocation diagnostic `key` is already
    /// covered, so a later `diag_once!` with that key under this invocation
    /// stays silent. For a command that has just reported the same condition
    /// in its own, more specific words.
    pub(crate) fn mark_reported(&self, key: &'static str) {
        self.first_report(key);
    }

    /// Serialize one publication of this invocation against any other: the
    /// guard is held from before `PUBLISH_START` until after `PUBLISH_END`.
    /// Caller code that runs under it (the instrument sink) must not publish
    /// under the same invocation, which its contract already forbids. A
    /// poisoned lock is recovered: it guards no state, only the span's
    /// exclusion.
    pub(crate) fn publication_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.0
            .publishing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Visit every engine-level counter this invocation has recorded so
    /// far, with the name it is emitted under on the sidecar stream and its
    /// current value: the read pipeline's `pipeline_*`, the worker pools'
    /// `classify_unordered_*` and `ordered_pool_*`, the writers' `writer_*`
    /// and `fadvise_failures`. Every pass of the invocation adds into the
    /// same counters (sums for counts and waits, maxima for the high-water
    /// and peak counters), and nothing else does.
    ///
    /// A live snapshot, safe to take while the run is going: each value is
    /// read on its own, so two counters are not read at one instant. The
    /// values are final once the call has returned and every block it handed
    /// out has been dropped (a dropped block's buffer returning to its pool
    /// can still count a pool drop). Peaks of thread-local scratch observe
    /// whatever the worker thread retained, from this invocation or an
    /// earlier one on the same pool.
    pub fn for_each_counter(&self, visit: impl FnMut(&'static str, u64)) {
        self.0.metrics.for_each(visit);
    }

    #[cfg(feature = "test-hooks")]
    #[cold]
    fn run_hook(&self) {
        if let Some(hook) = self.0.hook.get() {
            let ordinal = self
                .0
                .checks
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            hook(ordinal, self);
        }
    }

    /// Test hook: run `hook` at every checkpoint with its 1-based ordinal.
    /// Set once; a second call is ignored.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn on_check(&self, hook: impl Fn(u64, &Invocation) + Send + Sync + 'static) {
        drop(self.0.hook.set(Box::new(hook)));
    }

    /// Test hook: how many checkpoints have run under an installed
    /// [`on_check`](Self::on_check) hook.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    #[must_use]
    pub fn checks_passed(&self) -> u64 {
        self.0.checks.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Test hook: this token's fault plan, shared by every clone. Arm it on
    /// the token a test passes in its `Host`; see [`crate::test_hooks`].
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    #[must_use]
    pub fn faults(&self) -> &crate::test_hooks::FaultPlan {
        &self.0.faults
    }
}

/// A reader that observes a cancellation token before every `read`: for the
/// bulk copies of the library's own scratch files into a sink that is not
/// cancellation-aware (a caller's `Write`, stdout). One relaxed load per read
/// call, so not for per-element paths. A tripped token is
/// `ErrorKind::Cancelled` carried in an `io::Error` of kind `Other`, which no
/// `Read` or `Write` layer retries.
pub(crate) struct CancellableRead<'a, R> {
    inner: R,
    cancel: &'a Invocation,
}

impl<'a, R> CancellableRead<'a, R> {
    pub(crate) fn new(inner: R, cancel: &'a Invocation) -> Self {
        Self { inner, cancel }
    }
}

impl<R: std::io::Read> std::io::Read for CancellableRead<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.cancel.check().map_err(std::io::Error::from)?;
        self.inner.read(buf)
    }
}
/// Page-cache policy for the input files a command reads.
///
/// The page cache is shared by every process on the host, so dropping an
/// input's pages is host policy, not library behaviour: a second pass, a
/// concurrent reader or a later run then reads cold. The library's own scratch
/// files are not governed by this policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageCachePolicy {
    /// Advise `POSIX_FADV_DONTNEED` over the consumed prefix after every blob
    /// (buffered reads only), so a one-pass read of a file larger than RAM does
    /// not displace everything else in the page cache.
    Evict,
    /// Leave the page cache alone.
    Keep,
}
/// The execution context for the library's shared rayon tasks: `PbfWriter`
/// framing and the parallel-iterator work in commands (repack, degrade,
/// simple extract, the geocode index builder, merge-changes).
///
/// It does not confine all library computation and imposes no aggregate
/// concurrency limit. Dedicated pipelines (the pipelined reader's decode
/// pool, the ordered and classify worker pools, per-stage threads) create
/// separate worker sets that belong to the call, or to the returned session
/// such as a block iterator or a writer, rather than scheduling those workers
/// through this executor; their threads may exit after the call returns. Their
/// worker counts come from explicit options, resource inputs or fixed
/// tunables. Pipeline coordinators and consumers may run on the invoking
/// thread, and may dispatch shared rayon tasks through this executor.
/// Merge-changes with `Parallelism::Fixed(n)` runs on a dedicated rayon pool
/// of `n` threads instead of this executor; with `Parallelism::Auto` it uses
/// this executor.
///
/// `Ambient` uses whatever pool the calling thread is in, or rayon's global
/// pool off any pool, exactly as `rayon::spawn` and the par-iterators do.
/// `Pool` confines the work to the caller's pool. Three things follow from
/// handing the library a pool:
///
/// - Thread-local framing and decompression scratch persists on whichever
///   pool's threads ran it, for as long as those threads live. Release every
///   owner of the pool, including its writers, to let those workers exit and
///   free that scratch; dropping a pool does not join its workers.
/// - Disposition of a panic in a `PbfWriter`'s framing work is the pool's
///   `panic_handler`. With no handler, rayon aborts the process; with one,
///   the lost blob surfaces as an error from `PbfWriter::flush`.
/// - A `PbfWriter` driven from a worker of the executor's pool waits for its
///   framing work cooperatively, so even a one-thread pool cannot deadlock on
///   it. A writer moved between pools mid-stream under `Ambient` is not
///   covered by that wait.
///
/// Every way a `PbfWriter` ends - `flush`, a write that reports its writer
/// thread's failure, `Drop` - waits for all the framing work it dispatched,
/// errors included, so none of it frames, holds the writer's invocation or
/// moves its counters after the writer is gone (rayon's own bookkeeping, and
/// a non-aborting `panic_handler` after a framing panic, may still run).
/// That wait needs the pool to run the work: off the pool's workers it
/// blocks, so a pool whose every worker waits on the thread ending the writer
/// never lets it return.
#[derive(Clone, Debug)]
pub enum Executor {
    /// The calling thread's rayon pool, or rayon's global pool off any pool.
    Ambient,
    /// The caller's own pool.
    Pool(Arc<rayon::ThreadPool>),
}
impl Executor {
    /// Run `op` inside the pool (`Ambient`: a direct call).
    pub(crate) fn install<R: Send>(&self, op: impl FnOnce() -> R + Send) -> R {
        match self {
            Self::Ambient => op(),
            Self::Pool(pool) => pool.install(op),
        }
    }
    /// Run `op` with a scope whose spawned work runs on the pool (`Ambient`:
    /// `rayon::scope`, i.e. the calling thread's pool, or the global pool off
    /// any pool). Returns once `op` and everything it spawned has finished.
    pub(crate) fn scope<'scope, R: Send>(
        &self,
        op: impl FnOnce(&rayon::Scope<'scope>) -> R + Send,
    ) -> R {
        match self {
            Self::Ambient => rayon::scope(op),
            Self::Pool(pool) => pool.scope(op),
        }
    }
    /// Queue `op` on the pool (`Ambient`: `rayon::spawn`).
    pub(crate) fn spawn(&self, op: impl FnOnce() + Send + 'static) {
        match self {
            Self::Ambient => rayon::spawn(op),
            Self::Pool(pool) => pool.spawn(op),
        }
    }
    /// Whether the calling thread is a worker of this executor's pool
    /// (`Ambient`: of any rayon pool).
    pub(crate) fn on_worker(&self) -> bool {
        match self {
            Self::Ambient => rayon::current_thread_index().is_some(),
            Self::Pool(pool) => pool.current_thread_index().is_some(),
        }
    }
    /// Run one pending job of the calling worker's pool, if any. `None` off
    /// the pool.
    pub(crate) fn yield_now(&self) -> Option<rayon::Yield> {
        match self {
            Self::Ambient => rayon::yield_now(),
            Self::Pool(pool) => pool.yield_now(),
        }
    }
}
/// Invocation count and position are part of the contract, never deduplicated or hoisted.
pub trait ProcessActions: Send + Sync {
    /// A pool will allocate and free buffers on different threads.
    ///
    /// The motivating measurement is renumber on planet: blocks allocated on
    /// worker threads and freed on compression threads grew glibc arenas to
    /// tens of GB of anonymous RSS uncapped, under 1 GB with the cap (the
    /// figures are in renumber's call site).
    ///
    /// Only some commands that run worker pools request it. The others were
    /// never checked for cross-thread alloc/free patterns, so not calling it
    /// is not a decision that the cap is unneeded or harmful there. To
    /// settle it, measure those commands on a large input, then either call
    /// this once from the CLI's `main` as process policy and drop the
    /// per-command calls, or document here, per command, why it must not be
    /// capped.
    fn cross_thread_alloc_pool(&self);
    /// Request permission to hold `wanted` descriptors. The answer is how
    /// many the run may open on top of those already open in the process.
    ///
    /// The built-in providers ([`NoProcessActions`], and
    /// [`process::raise_nofile_to_hard_cap`](crate::process::raise_nofile_to_hard_cap))
    /// subtract a snapshot count of open descriptors from the soft limit.
    /// That is an estimate under stable descriptor use, not a reservation:
    /// two calls probing at once both see the same free descriptors. A host
    /// running descriptor-heavy calls concurrently returns
    /// [`FdAllowance::host_grant`] with a share it reserves for the duration
    /// of the call and releases when the call returns, computing free
    /// capacity as the limit less the occupied descriptors less the
    /// reservations not yet opened.
    fn fd_allowance(&self, wanted: u64) -> FdAllowance;
}
/// Leave process policy unchanged.
pub struct NoProcessActions;
impl ProcessActions for NoProcessActions {
    fn cross_thread_alloc_pool(&self) {}
    fn fd_allowance(&self, _wanted: u64) -> FdAllowance {
        crate::process::current_nofile()
    }
}
/// Per-call resource probes, invoked where the library needs the value.
pub trait Resources: Send + Sync {
    fn parallelism(&self) -> Option<NonZeroUsize>;
    fn memory_available(&self) -> Option<u64>;
}
/// Probe the current machine. Linux memory availability is capped by visible
/// cgroup v2 ancestor headroom (`memory.max - memory.current`). This treats
/// charged page cache conservatively rather than assuming it is reclaimable.
/// Cgroup v1 limits and ancestors hidden by a delegated mount are not observed.
pub struct MachineProbe;
impl Resources for MachineProbe {
    fn parallelism(&self) -> Option<NonZeroUsize> {
        std::thread::available_parallelism().ok()
    }
    fn memory_available(&self) -> Option<u64> {
        let meminfo = crate::process::read_kernel_file("/proc/meminfo").ok()?;
        let line = meminfo.lines().find(|l| l.starts_with("MemAvailable:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        let available = kb.saturating_mul(1024);
        #[cfg(target_os = "linux")]
        let available = machine_memory_available(available)?;
        Some(available)
    }
}

// Resolve the unified hierarchy in this process's mount namespace, including
// delegated mounts whose root is below the hierarchy root. Check ancestors:
// an unlimited leaf can still be constrained by its parent. Hidden ancestors
// outside the mount namespace cannot be observed by this best-effort probe.
#[cfg(target_os = "linux")]
fn machine_memory_available(mut available: u64) -> Option<u64> {
    let membership = crate::process::read_kernel_file("/proc/self/cgroup").ok()?;
    let Some(group) = membership.lines().find_map(|line| line.strip_prefix("0::")) else {
        return Some(available); // cgroup v1 has no unified memory controller
    };
    let mounts = crate::process::read_kernel_file("/proc/self/mountinfo").ok()?;
    let mut found = false;
    for line in mounts.lines() {
        let Some((fields, fs)) = line.split_once(" - ") else {
            continue;
        };
        if fs.split_whitespace().next() != Some("cgroup2") {
            continue;
        }
        let fields: Vec<_> = fields.split_whitespace().collect();
        let (Some(root), Some(mount)) = (fields.get(3), fields.get(4)) else {
            continue;
        };
        use std::os::unix::ffi::OsStringExt;
        let root = std::path::PathBuf::from(std::ffi::OsString::from_vec(
            crate::process::decode_mountinfo_path(root.as_bytes()),
        ));
        let mount = std::path::PathBuf::from(std::ffi::OsString::from_vec(
            crate::process::decode_mountinfo_path(mount.as_bytes()),
        ));
        let Ok(relative) = std::path::Path::new(group).strip_prefix(&root) else {
            continue;
        };
        // A namespace-inaccessible membership (for example /../foo) must
        // never escape the mounted controller when joined to its mountpoint.
        if relative
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return None;
        }
        found = true;
        available = cap_by_cgroup_ancestors(&mount, mount.join(relative), available)?;
    }
    // No visible unified mount means its limits cannot be established.
    found.then_some(available)
}

/// Walk from `path` up to and including the mounted hierarchy root `mount`,
/// capping `available` by each limited group's headroom.
#[cfg(target_os = "linux")]
fn cap_by_cgroup_ancestors(
    mount: &std::path::Path,
    mut path: std::path::PathBuf,
    mut available: u64,
) -> Option<u64> {
    loop {
        match crate::process::read_kernel_file(path.join("memory.max")) {
            Ok(maximum) if maximum.trim() == "max" => {}
            Ok(maximum) => {
                let current = crate::process::read_kernel_file(path.join("memory.current")).ok()?;
                available = cgroup_memory_available(available, &maximum, &current)?;
            }
            // The hierarchy root or a disabled memory controller has no
            // memory.max. Other failures leave the budget unknown.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        if path == mount {
            return Some(available);
        }
        if !path.pop() {
            return None;
        }
    }
}

#[cfg(target_os = "linux")]
fn cgroup_memory_available(available: u64, maximum: &str, current: &str) -> Option<u64> {
    if maximum.trim() == "max" {
        return Some(available);
    }
    let maximum: u64 = maximum.trim().parse().ok()?;
    let current: u64 = current.trim().parse().ok()?;
    Some(available.min(maximum.saturating_sub(current)))
}

/// Fixed resource values for isolated or reproducible runs.
pub struct Fixed {
    pub parallelism: Option<NonZeroUsize>,
    pub memory_available: Option<u64>,
}
impl Resources for Fixed {
    fn parallelism(&self) -> Option<NonZeroUsize> {
        self.parallelism
    }
    fn memory_available(&self) -> Option<u64> {
        self.memory_available
    }
}
pub(crate) fn sized_workers(
    r: &dyn Resources,
    reserve: usize,
    min: usize,
    fallback: usize,
) -> usize {
    r.parallelism()
        .map(|n| n.get().saturating_sub(reserve).max(min))
        .unwrap_or(fallback)
}

/// How many worker threads a command may run its worker pools with.
///
/// The one shape every command with a `-j/--jobs` knob takes. `Auto` is not
/// one rule: each command sizes itself as it always has (a reserve of cores
/// for its scanner, drain or writer threads, a minimum, a fallback when the
/// host cannot say how many cores it has), and that sizing lives with the
/// command. `Fixed(1)` runs a command's sequential path where it has one;
/// `Fixed(n)` caps its worker pools at `n` threads. For `diff`, `n` is also
/// the number of ID-range shards. For `merge_changes`, `Fixed(n)` runs the
/// command on a dedicated rayon pool of `n` threads instead of
/// [`Host::executor`]; `Auto` uses the executor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Parallelism {
    /// The command sizes its own pools from the host.
    #[default]
    Auto,
    /// At most this many worker threads.
    Fixed(NonZeroUsize),
}

impl Parallelism {
    /// The CLI spelling: `0` is [`Parallelism::Auto`], any other count is
    /// [`Parallelism::Fixed`].
    #[must_use]
    pub const fn from_jobs(jobs: usize) -> Self {
        match NonZeroUsize::new(jobs) {
            Some(n) => Self::Fixed(n),
            None => Self::Auto,
        }
    }

    /// The explicit cap, if one was asked for.
    #[must_use]
    pub const fn fixed(self) -> Option<usize> {
        match self {
            Self::Auto => None,
            Self::Fixed(n) => Some(n.get()),
        }
    }

    /// The cap, or `auto()` - the command's own sizing - when none was asked for.
    pub(crate) fn resolve(self, auto: impl FnOnce() -> usize) -> usize {
        self.fixed().unwrap_or_else(auto)
    }
}
#[derive(Clone)]
pub(crate) struct ResourcesHandle(pub(crate) Arc<dyn Resources>);
impl std::fmt::Debug for ResourcesHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResourcesHandle")
    }
}
#[cfg(test)]
impl Host {
    /// Fixed resources: the same machine on every host, so no unit-test
    /// outcome depends on the runner's core count or free memory.
    pub(crate) fn for_tests() -> Self {
        Self {
            diagnostics: Diagnostics::discard(),
            process: Arc::new(NoProcessActions),
            resources: Arc::new(Fixed {
                parallelism: NonZeroUsize::new(4),
                memory_available: Some(8 << 30),
            }),
            page_cache: PageCachePolicy::Keep,
            executor: Executor::Ambient,
            invocation: Invocation::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn cgroup_memory_budget_caps_host_and_parent_headroom() {
        assert_eq!(cgroup_memory_available(256, "8", "3"), Some(5));
        assert_eq!(cgroup_memory_available(4, "8", "3"), Some(4));
        assert_eq!(cgroup_memory_available(256, "8", "9"), Some(0));
        assert_eq!(cgroup_memory_available(256, "max", "unused"), Some(256));
        assert_eq!(cgroup_memory_available(256, "bad", "3"), None);
        assert_eq!(cgroup_memory_available(256, "8", "bad"), None);
        let leaf = cgroup_memory_available(256, "max", "0").expect("leaf");
        assert_eq!(cgroup_memory_available(leaf, "8", "3"), Some(5));
    }

    #[test]
    fn worker_sizing_respects_fixed_resources() {
        for (reserve, min, fallback) in [(1, 1, 3), (2, 1, 4), (2, 2, 4), (0, 1, 8), (0, 1, 4)] {
            for n in [None, Some(1), Some(2), Some(16)] {
                let fixed = Fixed {
                    parallelism: n.and_then(NonZeroUsize::new),
                    memory_available: None,
                };
                assert_eq!(
                    sized_workers(&fixed, reserve, min, fallback),
                    n.map_or(fallback, |v| v.saturating_sub(reserve).max(min))
                );
            }
        }
    }
    #[test]
    fn executor_installs_and_knows_its_workers() {
        assert_eq!(Executor::Ambient.install(|| 7), 7);
        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .build()
                .expect("pool"),
        );
        let executor = Executor::Pool(Arc::clone(&pool));
        assert!(
            !executor.on_worker(),
            "the test thread is not a pool worker"
        );
        assert!(executor.yield_now().is_none());
        assert!(
            executor.install(|| executor.on_worker()),
            "install must run on a worker of the pool"
        );
    }
    #[test]
    fn descriptor_allowance_narration() {
        use crate::process::{FdAdjustment::*, FdCount, FdProbe};
        for (adjustment, text) in [
            (None, "RLIMIT_NOFILE: 4096 (not adjusted)"),
            (
                Some(ProbeFailed),
                "RLIMIT_NOFILE: probe failed; assuming 4096",
            ),
            (
                Some(AlreadyAtCap),
                "RLIMIT_NOFILE: 4096 (already at hard cap; no adjustment)",
            ),
            (
                Some(Raised { from: 1024 }),
                "RLIMIT_NOFILE: 1024 -> 4096 (soft raised to hard cap)",
            ),
            (
                Some(RaiseFailed),
                "RLIMIT_NOFILE: 4096 (raise to hard cap rejected; still at soft cap)",
            ),
        ] {
            for (open, descriptors, suffix) in [
                (
                    FdCount::Counted(6),
                    4090,
                    "; 6 open, 4090 available".to_owned(),
                ),
                (
                    FdCount::Uncounted,
                    4096,
                    "; open descriptors not counted".to_owned(),
                ),
                (
                    FdCount::Exhausted(libc::EMFILE),
                    0,
                    format!(
                        "; no descriptor free to count open ones ({})",
                        std::io::Error::from_raw_os_error(libc::EMFILE)
                    ),
                ),
                (
                    FdCount::Failed(libc::EIO),
                    0,
                    format!(
                        "; counting open descriptors failed ({})",
                        std::io::Error::from_raw_os_error(libc::EIO)
                    ),
                ),
            ] {
                let allowance = FdAllowance::from_probe(FdProbe {
                    soft_limit: 4096,
                    adjustment,
                    open,
                    hard_limit: Some(4096),
                });
                assert_eq!(allowance.descriptors, descriptors);
                assert_eq!(allowance.to_string(), format!("{text}{suffix}"));
            }
        }
        // An unadjusted probe names a hard limit it could be raised to.
        let unraised = FdAllowance::from_probe(FdProbe {
            soft_limit: 1024,
            adjustment: None,
            open: FdCount::Counted(4),
            hard_limit: Some(524_288),
        });
        assert_eq!(
            unraised.to_string(),
            "RLIMIT_NOFILE: 1024 (not adjusted; hard limit 524288); 4 open, 1020 available"
        );
        assert_eq!(
            FdAllowance::host_grant(512).to_string(),
            "descriptor allowance: 512 (host grant)"
        );
    }
}

/// A diagnostic event without a line terminator.
pub struct Event<'a> {
    pub kind: EventKind,
    pub key: &'static str,
    pub args: std::fmt::Arguments<'a>,
}
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Progress,
    Warning,
    Note,
    Summary,
    Developer,
}
/// A sink receives one event per line and may serve concurrent callers.
pub trait DiagnosticSink: Send + Sync {
    fn event(&self, event: &Event<'_>);
}
/// Where a run's diagnostic events go: a handle on a [`DiagnosticSink`].
///
/// Cloning shares the sink and nothing else. A `Diagnostics` holds no run
/// state: the diagnostics a run reports only once (a missing-indexdata note,
/// an unrecognised-indexdata warning, a writer degradation) are once per
/// [`Invocation`], whose report-once keys every part of the run shares. Two
/// invocations handed one `Diagnostics` (or clones of it) each report their
/// own; several calls grouped under one `Invocation` report each such
/// diagnostic once between them.
#[derive(Clone)]
pub struct Diagnostics {
    sink: Arc<dyn DiagnosticSink>,
}
impl std::fmt::Debug for Diagnostics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Diagnostics")
    }
}
struct Discard;
impl DiagnosticSink for Discard {
    fn event(&self, _event: &Event<'_>) {}
}
impl Diagnostics {
    pub fn new(sink: Arc<dyn DiagnosticSink>) -> Self {
        Self { sink }
    }
    pub fn discard() -> Self {
        Self::new(Arc::new(Discard))
    }
    pub fn summary_writer(&self, key: &'static str) -> SummaryWriter<'_> {
        SummaryWriter {
            diagnostics: self,
            key,
            bytes: Vec::new(),
        }
    }
    pub(crate) fn emit(&self, kind: EventKind, key: &'static str, args: std::fmt::Arguments<'_>) {
        self.sink.event(&Event { kind, key, args });
    }
    /// Emit the event once per `invocation`: the first `diag_once!` with
    /// `key` under that invocation (or a clone of it) reaches the sink, every
    /// later one, and any after [`Invocation::mark_reported`], stays silent.
    pub(crate) fn emit_once(
        &self,
        invocation: &Invocation,
        kind: EventKind,
        key: &'static str,
        args: std::fmt::Arguments<'_>,
    ) {
        if invocation.first_report(key) {
            self.emit(kind, key, args);
        }
    }
}
/// Converts byte-oriented summary output into one event per completed line.
pub struct SummaryWriter<'a> {
    diagnostics: &'a Diagnostics,
    key: &'static str,
    bytes: Vec<u8>,
}
impl std::io::Write for SummaryWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(buf);
        while let Some(end) = self.bytes.iter().position(|b| *b == b'\n') {
            self.diagnostics.emit(
                EventKind::Summary,
                self.key,
                format_args!("{}", String::from_utf8_lossy(&self.bytes[..end])),
            );
            self.bytes.drain(..=end);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Drop for SummaryWriter<'_> {
    fn drop(&mut self) {
        if !self.bytes.is_empty() {
            self.diagnostics.emit(
                EventKind::Summary,
                self.key,
                format_args!("{}", String::from_utf8_lossy(&self.bytes)),
            );
        }
    }
}
macro_rules! diag {
    ($d:expr, $kind:ident, $key:literal, $($args:tt)+) => { $d.emit($crate::host::EventKind::$kind, $key, format_args!($($args)+)) };
}
/// `diag_once!(diagnostics, invocation, Kind, "key", ...)`: emit once per
/// invocation (see [`Diagnostics::emit_once`]).
macro_rules! diag_once {
    ($d:expr, $inv:expr, $kind:ident, $key:literal, $($args:tt)+) => { $d.emit_once($inv, $crate::host::EventKind::$kind, $key, format_args!($($args)+)) };
}
pub(crate) use {diag, diag_once};

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    use std::io::Write;
    #[derive(Default)]
    struct Sink(std::sync::Mutex<Vec<String>>);
    impl DiagnosticSink for Sink {
        fn event(&self, e: &Event<'_>) {
            self.0.lock().expect("sink").push(e.args.to_string());
        }
    }
    /// Report-once keys belong to the invocation, not the `Diagnostics`
    /// handle: one handle (or its clones) under two invocations reports
    /// twice, two handles under one invocation (or its clones) once.
    #[test]
    fn once_keys_are_per_invocation() {
        let sink = Arc::new(Sink::default());
        let d = Diagnostics::new(Arc::clone(&sink) as Arc<dyn DiagnosticSink>);
        let other = Diagnostics::new(Arc::clone(&sink) as Arc<dyn DiagnosticSink>);
        let (first, second) = (Invocation::new(), Invocation::new());
        d.emit_once(&first, EventKind::Warning, "key", format_args!("first"));
        d.clone()
            .emit_once(&second, EventKind::Warning, "key", format_args!("second"));
        let shared = first.clone();
        other.emit_once(&shared, EventKind::Warning, "key", format_args!("again"));
        d.emit_once(&second, EventKind::Warning, "key", format_args!("again"));
        assert_eq!(*sink.0.lock().expect("sink"), ["first", "second"]);
    }
    #[test]
    fn a_reported_key_silences_a_later_once_event() {
        let sink = Arc::new(Sink::default());
        let d = Diagnostics::new(Arc::clone(&sink) as Arc<dyn DiagnosticSink>);
        let invocation = Invocation::new();
        invocation.mark_reported("key");
        d.emit_once(&invocation, EventKind::Note, "key", format_args!("note"));
        d.emit_once(&invocation, EventKind::Note, "other", format_args!("other"));
        // Another invocation is not covered by the first one's report.
        d.emit_once(
            &Invocation::new(),
            EventKind::Note,
            "key",
            format_args!("fresh"),
        );
        assert_eq!(*sink.0.lock().expect("sink"), ["other", "fresh"]);
    }
    #[test]
    fn summary_writer_splits_lines_and_keeps_partial_on_drop() {
        let sink = Arc::new(Sink::default());
        let d = Diagnostics::new(Arc::clone(&sink) as Arc<dyn DiagnosticSink>);
        {
            let mut w = d.summary_writer("summary");
            writeln!(w, "one").expect("write");
            writeln!(w, "two").expect("write");
            write!(w, "par").expect("write");
            writeln!(w, "tial").expect("write");
            write!(w, "tail").expect("write");
        }
        assert_eq!(
            *sink.0.lock().expect("sink"),
            ["one", "two", "partial", "tail"]
        );
    }
}
