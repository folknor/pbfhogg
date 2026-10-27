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
    /// Where the library's own data-parallel work and writer framing run.
    pub executor: Executor,
    /// The invocation this host runs: its cancellation request, and its
    /// instrumentation. Every command run under this host stops at its next
    /// checkpoint once the token is tripped and returns
    /// [`ErrorKind::Cancelled`](crate::ErrorKind::Cancelled). A token is
    /// sticky and accumulates: give each independent invocation a fresh one.
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
#[derive(Clone, Default)]
pub struct Invocation(Arc<InvocationState>);

#[derive(Default)]
struct InvocationState {
    tripped: std::sync::atomic::AtomicBool,
    #[cfg(feature = "test-hooks")]
    hook: std::sync::OnceLock<CheckHook>,
    #[cfg(feature = "test-hooks")]
    checks: std::sync::atomic::AtomicU64,
    /// The invocation's fault plan: every worker loop already holds the
    /// token, so this is where a test's injected faults reach them.
    #[cfg(feature = "test-hooks")]
    faults: crate::test_hooks::FaultPlan,
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
    /// A fresh, untripped token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
/// The rayon pool the library's parallel work runs on.
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
/// the number of ID-range shards.
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
                });
                assert_eq!(allowance.descriptors, descriptors);
                assert_eq!(allowance.to_string(), format!("{text}{suffix}"));
            }
        }
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
#[derive(Default)]
pub struct OnceKeys {
    keys: std::sync::Mutex<std::collections::HashSet<&'static str>>,
}
#[derive(Clone)]
pub struct Diagnostics {
    sink: Arc<dyn DiagnosticSink>,
    once: Arc<OnceKeys>,
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
        Self::with_once(sink, Arc::new(OnceKeys::default()))
    }
    pub fn with_once(sink: Arc<dyn DiagnosticSink>, once: Arc<OnceKeys>) -> Self {
        Self { sink, once }
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
    /// Record that the event with `key` is already covered, so a later
    /// `diag_once!` with that key stays silent. For a command that has just
    /// reported the same condition in its own, more specific words.
    pub(crate) fn mark_reported(&self, key: &'static str) {
        self.once
            .keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key);
    }
    pub(crate) fn emit_once(
        &self,
        kind: EventKind,
        key: &'static str,
        args: std::fmt::Arguments<'_>,
    ) {
        let first = self
            .once
            .keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key);
        if first {
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
macro_rules! diag_once {
    ($d:expr, $kind:ident, $key:literal, $($args:tt)+) => { $d.emit_once($crate::host::EventKind::$kind, $key, format_args!($($args)+)) };
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
    #[test]
    fn once_keys_share_only_when_requested() {
        let sink = Arc::new(Sink::default());
        let once = Arc::new(OnceKeys::default());
        let first = Diagnostics::with_once(
            Arc::clone(&sink) as Arc<dyn DiagnosticSink>,
            Arc::clone(&once),
        );
        let shared = Diagnostics::with_once(Arc::clone(&sink) as Arc<dyn DiagnosticSink>, once);
        let separate = Diagnostics::new(Arc::clone(&sink) as Arc<dyn DiagnosticSink>);
        for d in [first, shared, separate] {
            d.emit_once(EventKind::Warning, "key", format_args!("warning"));
        }
        assert_eq!(*sink.0.lock().expect("sink"), ["warning", "warning"]);
    }
    #[test]
    fn a_reported_key_silences_a_later_once_event() {
        let sink = Arc::new(Sink::default());
        let d = Diagnostics::new(Arc::clone(&sink) as Arc<dyn DiagnosticSink>);
        d.mark_reported("key");
        d.emit_once(EventKind::Note, "key", format_args!("note"));
        d.emit_once(EventKind::Note, "other", format_args!("other"));
        assert_eq!(*sink.0.lock().expect("sink"), ["other"]);
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
