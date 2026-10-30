//! Caller-owned process actions and resources for a library invocation.
pub use crate::process::{FdAllowance, FdReservation};
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
///
/// # Control handle
///
/// An `Invocation` is the control handle: identity, the token and the reads.
/// Holding or leaking one keeps nothing open. What records into the
/// invocation is admitted separately: every public entry that can record (a
/// command, a method of a library value, `cancel`, the end-of-run emitters)
/// holds a call guard for its extent, and every value that can record
/// outside a call (a writer and its threads, a block iterator's pipeline, a
/// decompression pool kept alive by blocks, a bound output) holds a permit
/// until its last recording. Values of those kinds handed to the caller are
/// counted, by kind, in the invocation's census.
///
/// # Sealing
///
/// [`seal`](Self::seal) ends the invocation: it closes admission, takes the
/// final snapshot of its counters and, for an invocation built with
/// [`with_call_sink`](Self::with_call_sink), ends the call stream. It never
/// waits. It refuses while a call is in progress, reports the census while a
/// value issued to the caller can still record, and once sealed every public
/// entry under the invocation is refused with
/// [`ErrorKind::Sealed`](crate::ErrorKind::Sealed) (`cancel` returns
/// [`CancelOutcome::Ended`](crate::publication::CancelOutcome::Ended)).
/// ADR-0020 is the contract.
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
    sink: SinkSlot,
    /// Set once the sink has panicked: later emissions skip it.
    sink_failed: std::sync::atomic::AtomicBool,
    /// The snapshot a successful seal took, set once by the finalizing
    /// thread before it publishes `Sealed` on the word.
    sealed: std::sync::OnceLock<Arc<Snapshot>>,
    /// The keys of the once-per-invocation diagnostics already reported (or
    /// covered by a more specific report). Touched only where such a
    /// diagnostic is about to be emitted, never on a per-element path.
    reported: std::sync::Mutex<std::collections::HashSet<&'static str>>,
    /// The publication execution lock: held by one publication from before
    /// its decision until its row is finalized (hooks, `PUBLISH_START` to
    /// `PUBLISH_END`, the syncs, installs and rollback between), so the
    /// publications of one invocation run one after the other in ordinal
    /// order and each span's `publish_sync_*` samples are its own.
    publishing: std::sync::Mutex<()>,
    /// The publication log (see [`crate::publication`]). Never held across
    /// anything but its own bookkeeping.
    log: std::sync::Mutex<crate::publication::PublicationLog>,
    /// The transition word: lifecycle state, live [`Call`] guards and live
    /// [`Permit`]s in one atomic (see [`word`]). On cache lines of its own:
    /// a per-call method of a library value mints and retires a guard here.
    word: crate::counters::Padded<std::sync::atomic::AtomicU64>,
    /// The census: how many issued values of each [`Kind`] are alive.
    /// Bookkeeping only; nothing is ever called under it.
    census: std::sync::Mutex<[u64; Kind::COUNT]>,
}

/// Where an invocation's markers and counter samples go.
#[derive(Default)]
enum SinkSlot {
    /// Nowhere: the run emits nothing.
    #[default]
    None,
    /// A caller's sink, with no sealing semantics.
    Generic(Arc<dyn crate::debug::InstrumentSink>),
    /// The invocation's own call sink, which a seal ends, and the caller's
    /// tap, which sees every ordinary record after the sink and nothing at
    /// sealing. The tap is latched off on its own when it panics.
    Call {
        sink: crate::debug::CallSink,
        tap: Option<Arc<dyn crate::debug::InstrumentSink>>,
        tap_failed: std::sync::atomic::AtomicBool,
    },
}

/// Run `emit` with its unwinding contained: a panic latches `failed`, and
/// the caught payload is forgotten, not dropped, since its destructor is
/// caller code too and could panic again.
fn contained(failed: &std::sync::atomic::AtomicBool, emit: impl FnOnce()) {
    use std::sync::atomic::Ordering::Relaxed;
    if failed.load(Relaxed) {
        return;
    }
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(emit)) {
        failed.store(true, Relaxed);
        std::mem::forget(payload);
    }
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
            sink: SinkSlot::Generic(sink),
            ..InvocationState::default()
        }))
    }

    /// A fresh, untripped token that owns `sink`, the call stream of one
    /// call on a [`MarkerFifo`](crate::debug::MarkerFifo): every marker and
    /// counter sample of the run goes there, and a successful
    /// [`seal`](Self::seal) ends the stream with the sealing emissions and
    /// the terminal record. Its account is
    /// [`stream_account`](Self::stream_account).
    ///
    /// `tap`, if given, sees every ordinary record after the sink, and
    /// nothing at sealing; a panicking tap is latched off on its own, and the
    /// sink carries on. The tap is the caller's code under the same rules as
    /// any [`InstrumentSink`](crate::debug::InstrumentSink). A tap that holds
    /// this invocation (to cancel it, say) keeps it alive for as long as the
    /// tap lives: clear what the tap holds once the call is over.
    #[must_use]
    pub fn with_call_sink(
        sink: crate::debug::CallSink,
        tap: Option<Arc<dyn crate::debug::InstrumentSink>>,
    ) -> Self {
        Self(Arc::new(InvocationState {
            sink: SinkSlot::Call {
                sink,
                tap,
                tap_failed: std::sync::atomic::AtomicBool::new(false),
            },
            ..InvocationState::default()
        }))
    }

    /// Whether this invocation has a working sink. Emission sites whose value
    /// costs something to compute (an allocator query, a `/proc` sample) ask
    /// first.
    #[inline]
    #[must_use]
    pub fn instrumented(&self) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let sink_ok = !self.0.sink_failed.load(Relaxed);
        match &self.0.sink {
            SinkSlot::None => false,
            SinkSlot::Generic(_) => sink_ok,
            SinkSlot::Call {
                tap, tap_failed, ..
            } => sink_ok || (tap.is_some() && !tap_failed.load(Relaxed)),
        }
    }

    /// The account of this invocation's call sink: how its stream ended and
    /// every line it dropped, the terminal record included. `None` for an
    /// invocation without one ([`new`](Self::new),
    /// [`with_sink`](Self::with_sink)). The state is
    /// [`Open`](crate::debug::StreamState::Open) until a successful seal has
    /// ended the stream, and never changes after that.
    #[must_use]
    pub fn stream_account(&self) -> Option<crate::debug::StreamAccount> {
        match &self.0.sink {
            SinkSlot::Call { sink, .. } => Some(sink.account()),
            SinkSlot::None | SinkSlot::Generic(_) => None,
        }
    }

    /// Admit a public entry: a [`Call`] guard on this invocation for the
    /// entry's extent. Every recording the entry makes goes through it.
    /// Refused with [`ErrorKind::Sealed`](crate::ErrorKind::Sealed) once a
    /// seal has begun. A holder of a capability mints nested ones through its
    /// [`Recorder`] instead, which cannot be refused.
    #[inline]
    pub(crate) fn call(&self) -> crate::Result<Call> {
        match self.mint(word::GUARD) {
            Some(counted) => Ok(Call {
                rec: Recorder(self.clone()),
                counted,
            }),
            None => Err(crate::error::new_sealed()),
        }
    }

    /// Count one capability of `unit`'s field into the word, admitted only
    /// while the invocation is Open: `None` once a seal has begun. `false`
    /// when the field is saturated: the sticky flag is set instead, in the
    /// same Open-conditioned exchange, and the capability is uncounted, so it
    /// never decrements. A set flag keeps the invocation Open for good (the
    /// seal's exchange expects it clear), so a minter that finds it already
    /// set needs no exchange.
    #[inline]
    fn mint(&self, unit: u64) -> Option<bool> {
        use std::sync::atomic::Ordering::{AcqRel, Relaxed};
        let word = &self.0.word.0;
        let mut current = word.load(Relaxed);
        loop {
            if current & word::STATE != word::OPEN {
                return None;
            }
            let (next, counted) = if word::is_full(current, unit) {
                if current & word::SATURATED != 0 {
                    return Some(false);
                }
                (current | word::SATURATED, false)
            } else {
                (current + unit, true)
            };
            match word.compare_exchange_weak(current, next, AcqRel, Relaxed) {
                Ok(_) => return Some(counted),
                Err(seen) => current = seen,
            }
        }
    }

    /// End the invocation. Never waits and never retries; see ADR-0020.
    ///
    /// One exchange takes the invocation from Open, with no call in
    /// progress, no permit alive and no saturated count, to Finalizing; from
    /// then on every admission is refused. The thread that made it takes the
    /// snapshot of the counters ([`for_each_counter`](Self::for_each_counter)
    /// as owned data), ends the call stream if the invocation owns a
    /// [`CallSink`](crate::debug::CallSink) (every counter of the snapshot as
    /// a sealing emission, then the terminal record), and only then marks the
    /// invocation Sealed and returns [`SealOutcome::Sealed`]. It runs no
    /// caller code. A later `seal` returns the same snapshot.
    ///
    /// Otherwise it says why not, from what it observed, in this order: a
    /// call in progress ([`SealRefusal::InProgress`], which is what a seal
    /// from inside a callback of a call gets); another seal finalizing
    /// ([`SealRefusal::Finalizing`]); a finalization that failed
    /// ([`SealRefusal::FinalizationFailed`]: a panic before the snapshot was
    /// taken; the stream stays open and the invocation can never seal); a
    /// saturated count ([`SealRefusal::Saturated`]); and with permits alive,
    /// the census: [`SealOutcome::Outstanding`] when values issued to the
    /// caller are still alive (drop them and seal again),
    /// [`SealRefusal::Unticketed`] when none is (a permit the caller cannot
    /// release, such as one inside a leaked value or a forgotten panic
    /// payload), [`SealRefusal::Busy`] when the census was being updated.
    pub fn seal(&self) -> SealOutcome {
        use std::sync::atomic::Ordering::{AcqRel, Acquire};
        // Acquire: every recording of every retired capability (each retired
        // with `Release`) happens before the snapshot, and a `Sealed` seen on
        // failure carries the stored snapshot with it.
        match self
            .0
            .word
            .0
            .compare_exchange(word::OPEN, word::FINALIZING, AcqRel, Acquire)
        {
            Ok(_) => self.finalize(),
            Err(seen) => self.classify(seen),
        }
    }

    /// Why a seal that did not win the exchange did not, from the word it
    /// observed (see [`seal`](Self::seal) for the order).
    fn classify(&self, seen: u64) -> SealOutcome {
        if word::field(seen, word::GUARD) > 0 {
            return SealOutcome::Refused(SealRefusal::InProgress);
        }
        match seen & word::STATE {
            word::FINALIZING => return SealOutcome::Refused(SealRefusal::Finalizing),
            word::SEALED => {
                // Set before `Sealed` was published, and the exchange that
                // observed `Sealed` acquired it.
                return match self.0.sealed.get() {
                    Some(snapshot) => SealOutcome::Sealed(Arc::clone(snapshot)),
                    None => SealOutcome::Refused(SealRefusal::Finalizing),
                };
            }
            word::FAILED => return SealOutcome::Refused(SealRefusal::FinalizationFailed),
            _ => {}
        }
        if seen & word::SATURATED != 0 {
            return SealOutcome::Refused(SealRefusal::Saturated);
        }
        if word::field(seen, word::PERMIT) == 0 {
            // A strong exchange from Open with nothing counted cannot fail.
            return SealOutcome::Refused(SealRefusal::Busy);
        }
        let census = match self.0.census.try_lock() {
            Ok(census) => *census,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => *poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                return SealOutcome::Refused(SealRefusal::Busy);
            }
        };
        let census = Census {
            session: census[Kind::Session as usize],
            writer: census[Kind::Writer as usize],
            output: census[Kind::Output as usize],
            buffer: census[Kind::Buffer as usize],
        };
        if census.is_empty() {
            SealOutcome::Refused(SealRefusal::Unticketed)
        } else {
            SealOutcome::Outstanding(census)
        }
    }

    /// The winning sealer's finalization, in two regions split by one
    /// commit point. Before it, a panic (the snapshot, a test hook) marks
    /// the invocation Failed: no sealing record exists, so the stream stays
    /// open. The commit point stores the prepared snapshot; after it the
    /// invocation ends Sealed on every path, whatever happens to the stream.
    fn finalize(&self) -> SealOutcome {
        use std::sync::atomic::Ordering::Release;
        let prepared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(feature = "test-hooks")]
            self.0
                .faults
                .seal_step(crate::test_hooks::SealStep::Snapshot);
            let mut counters = Vec::new();
            self.0
                .metrics
                .for_each(|name, value| counters.push((name, value)));
            Arc::new(Snapshot { counters })
        }));
        let snapshot = match prepared {
            Ok(snapshot) => snapshot,
            Err(payload) => {
                std::mem::forget(payload);
                self.0.word.0.store(word::FAILED, Release);
                return SealOutcome::Refused(SealRefusal::FinalizationFailed);
            }
        };
        // The commit point. Only this thread sets it, once.
        drop(self.0.sealed.set(Arc::clone(&snapshot)));
        if let SinkSlot::Call { sink, .. } = &self.0.sink {
            sink.close(&snapshot, || {
                #[cfg(feature = "test-hooks")]
                self.0.faults.seal_step(crate::test_hooks::SealStep::Stream);
            });
        }
        self.0.word.0.store(word::SEALED, Release);
        SealOutcome::Sealed(snapshot)
    }

    /// Retire one counted capability of `unit`'s field. `Release`: every
    /// recording the capability made happens before the count drops.
    #[inline]
    fn retire(&self, unit: u64) {
        self.0
            .word
            .0
            .fetch_sub(unit, std::sync::atomic::Ordering::Release);
    }

    fn census_adjust(&self, kind: Kind, up: bool) {
        let mut census = self
            .0
            .census
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = &mut census[kind as usize];
        *slot = if up {
            slot.saturating_add(1)
        } else {
            slot.saturating_sub(1)
        };
    }

    /// Test hook: the transition word and the census as they stand.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    #[must_use]
    pub fn capabilities(&self) -> Capabilities {
        let current = self.0.word.0.load(std::sync::atomic::Ordering::Acquire);
        let census = *self
            .0
            .census
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Capabilities {
            guards: word::field(current, word::GUARD),
            permits: word::field(current, word::PERMIT),
            saturated: current & word::SATURATED != 0,
            session: census[Kind::Session as usize],
            writer: census[Kind::Writer as usize],
            output: census[Kind::Output as usize],
            buffer: census[Kind::Buffer as usize],
        }
    }

    /// Request cancellation. Every run observing this token (or a clone)
    /// stops at its next checkpoint. Permanent, and safe to call from any
    /// thread (not from a signal handler: it takes the publication log's
    /// lock, which nothing holds for longer than its bookkeeping).
    ///
    /// Cancellation is invocation-wide. Under the publication log it trips
    /// the token and decides every output still pending (bound and not yet
    /// decided) [`RefusedCancelled`](crate::publication::Decision::RefusedCancelled),
    /// in bind order, and says what it found:
    /// [`BeforeDecision`](crate::publication::CancelOutcome::BeforeDecision)
    /// with the first ordinal it refused,
    /// [`AfterDecision`](crate::publication::CancelOutcome::AfterDecision)
    /// with the latest authorized one when nothing was pending (that
    /// publication completes regardless), or
    /// [`NoPublicationPending`](crate::publication::CancelOutcome::NoPublicationPending).
    /// Every decision after it refuses. It never waits for a publication
    /// in progress. To cancel one command among several, give each its own
    /// invocation.
    ///
    /// Once a seal has begun there is nothing left to cancel: it returns
    /// [`Ended`](crate::publication::CancelOutcome::Ended) and trips nothing.
    pub fn cancel(&self) -> crate::publication::CancelOutcome {
        let Ok(call) = self.call() else {
            return crate::publication::CancelOutcome::Ended;
        };
        let mut log = call.log();
        self.0
            .tripped
            .store(true, std::sync::atomic::Ordering::Release);
        log.cancel()
    }

    /// The publication log, poison recovered: no mutation of it can unwind
    /// half done (see [`crate::publication`]). Private: a mutation goes
    /// through [`Recorder::log`], a read copies here.
    fn log_guard(&self) -> std::sync::MutexGuard<'_, crate::publication::PublicationLog> {
        self.0
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Visit every publication this invocation has decided, in ordinal
    /// order. The rows are copied under the log's lock and visited after it
    /// is released, so the visitor may call back into the library. A
    /// publication still running is visited with
    /// [`finished`](crate::publication::Publication::finished) false.
    pub fn for_each_publication(&self, mut visit: impl FnMut(&crate::publication::Publication)) {
        let rows = self.log_guard().rows();
        for row in &rows {
            visit(row);
        }
    }

    /// Run the test-only checkpoint hook without reading the token: the
    /// publication decision reads the token under the log's lock, and a
    /// hook that cancels must not run there.
    #[inline]
    pub(crate) fn checkpoint_hook(&self) {
        #[cfg(feature = "test-hooks")]
        self.run_hook();
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

    /// Visit every engine-level counter this invocation has recorded so
    /// far, with the name it is emitted under on the sidecar stream and its
    /// current value: the read pipeline's `pipeline_*`, the worker pools'
    /// `classify_unordered_*` and `ordered_pool_*`, the writers' `writer_*`
    /// and `fadvise_failures` and `fadvise_input_evictions`. Every pass of the invocation adds into the
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

/// The transition word's layout: two bits of lifecycle state, a sticky
/// saturation flag, the live [`Call`] guards and the live [`Permit`]s. A
/// count at its field's maximum is not incremented: the flag is set and the
/// capability goes uncounted, so a seal can never mistake a saturated count
/// for a true one. Open is zero, so the seal's exchange expects the whole
/// word to be zero: Open, nothing counted, no saturation.
mod word {
    /// The lifecycle state bits.
    pub(super) const STATE: u64 = 0b11;
    pub(super) const OPEN: u64 = 0;
    pub(super) const FINALIZING: u64 = 1;
    pub(super) const SEALED: u64 = 2;
    pub(super) const FAILED: u64 = 3;
    pub(super) const SATURATED: u64 = 1 << 2;
    const GUARD_SHIFT: u32 = 3;
    const GUARD_BITS: u32 = 30;
    const PERMIT_SHIFT: u32 = GUARD_SHIFT + GUARD_BITS;
    const PERMIT_BITS: u32 = 64 - PERMIT_SHIFT;
    /// One guard.
    pub(super) const GUARD: u64 = 1 << GUARD_SHIFT;
    /// One permit.
    pub(super) const PERMIT: u64 = 1 << PERMIT_SHIFT;

    const fn shape(unit: u64) -> (u32, u64) {
        if unit == GUARD {
            (GUARD_SHIFT, (1 << GUARD_BITS) - 1)
        } else {
            (PERMIT_SHIFT, (1 << PERMIT_BITS) - 1)
        }
    }

    /// The count in `unit`'s field of `word`.
    pub(super) const fn field(word: u64, unit: u64) -> u64 {
        let (shift, max) = shape(unit);
        (word >> shift) & max
    }

    /// Whether `unit`'s field of `word` is at its maximum.
    pub(super) const fn is_full(word: u64, unit: u64) -> bool {
        field(word, unit) == shape(unit).1
    }

    #[cfg(test)]
    pub(super) const fn max(unit: u64) -> u64 {
        shape(unit).1
    }
}

/// The kinds of issued value the census counts: every value of these kinds
/// that crossed the API boundary to the caller and still exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A block iterator (`PipelinedBlocks`).
    Session = 0,
    /// A `PbfWriter`.
    Writer = 1,
    /// A bound output (`BoundOutput`, `BoundOutputDir`, `BoundOutputSet`).
    Output = 2,
    /// A pooled block handed to the caller.
    Buffer = 3,
}

impl Kind {
    const COUNT: usize = 4;
}

/// The transition word and census of an invocation, for tests.
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// Live call guards.
    pub guards: u64,
    /// Live permits.
    pub permits: u64,
    /// Whether a count saturated.
    pub saturated: bool,
    /// Issued block iterators alive.
    pub session: u64,
    /// Issued writers alive.
    pub writer: u64,
    /// Issued bound outputs alive.
    pub output: u64,
    /// Issued pooled blocks alive.
    pub buffer: u64,
}

/// What [`Invocation::seal`] found.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum SealOutcome {
    /// The invocation is sealed: nothing records into it any more, and this
    /// is its final snapshot.
    Sealed(Arc<Snapshot>),
    /// Values issued to the caller can still record; drop them and seal
    /// again. The census as the attempt took it.
    Outstanding(Census),
    /// Refused, and why.
    Refused(SealRefusal),
}

/// Why [`Invocation::seal`] refused. See there for the order the reasons are
/// checked in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SealRefusal {
    /// A call under the invocation is in progress (a seal from inside one
    /// of its callbacks gets this). Retry once it has returned.
    InProgress,
    /// Another seal is finalizing the invocation.
    Finalizing,
    /// A finalization failed before its snapshot: the invocation can never
    /// seal, and its stream stays open.
    FinalizationFailed,
    /// A capability count saturated, so the counts cannot prove that nothing
    /// records any more. Never sealable.
    Saturated,
    /// Permits are alive with no issued value to account for them: one the
    /// caller cannot release (a leaked value, a forgotten panic payload).
    Unticketed,
    /// The census was being updated; retry.
    Busy,
}

impl SealRefusal {
    /// One line saying why.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::InProgress => "a call under the invocation is in progress",
            Self::Finalizing => "another seal is finalizing the invocation",
            Self::FinalizationFailed => {
                "an earlier seal failed before its snapshot; the invocation can never seal"
            }
            Self::Saturated => "a capability count saturated; the invocation can never seal",
            Self::Unticketed => {
                "permits are alive with no issued value to account for them (a leaked value)"
            }
            Self::Busy => "the census was being updated",
        }
    }
}

impl std::fmt::Display for SealRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason())
    }
}

/// The values issued to the caller that were still alive at a seal attempt,
/// by kind: block iterators (`session`), writers (`writer`), bound outputs
/// (`output`) and blocks (`buffer`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Census {
    /// Issued block iterators alive.
    pub session: u64,
    /// Issued writers alive.
    pub writer: u64,
    /// Issued bound outputs alive.
    pub output: u64,
    /// Issued blocks alive.
    pub buffer: u64,
}

impl Census {
    /// Whether no issued value was alive.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.session == 0 && self.writer == 0 && self.output == 0 && self.buffer == 0
    }

    /// Visit each kind with a live value, as `(kind, count)`, in a fixed
    /// order. The kind names are `session`, `writer`, `output`, `buffer`.
    pub fn for_each_held(&self, mut visit: impl FnMut(&'static str, u64)) {
        for (kind, n) in [
            ("session", self.session),
            ("writer", self.writer),
            ("output", self.output),
            ("buffer", self.buffer),
        ] {
            if n > 0 {
                visit(kind, n);
            }
        }
    }
}

/// A sealed invocation's final counters: every counter
/// [`Invocation::for_each_counter`] visits, with its value at the seal, in
/// that order. Names are unique.
#[derive(Debug)]
pub struct Snapshot {
    counters: Vec<(&'static str, u64)>,
}

impl Snapshot {
    /// Every counter, as `(name, value)`.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, u64)> + '_ {
        self.counters.iter().copied()
    }

    /// The value of counter `name`, if the snapshot has it.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<u64> {
        self.iter().find(|(n, _)| *n == name).map(|(_, v)| v)
    }

    /// How many counters it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.counters.len()
    }

    /// Whether it holds none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.counters.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn for_tests(counters: Vec<(&'static str, u64)>) -> Self {
        Self { counters }
    }
}

/// The recording interface of an invocation: every mutation of its state
/// (counters, sink delivery and its failure latch, report-once keys, the
/// publication log and its execution lock). Reachable only by borrowing a
/// [`Call`] or a [`Permit`], so the compiler rejects a recorder that holds
/// neither; the control handle, [`Invocation`], has none of it. Derefs to the
/// handle for its reads.
#[repr(transparent)]
pub(crate) struct Recorder(Invocation);

impl std::ops::Deref for Recorder {
    type Target = Invocation;
    #[inline]
    fn deref(&self) -> &Invocation {
        &self.0
    }
}

impl Recorder {
    /// The control handle.
    #[inline]
    pub(crate) fn handle(&self) -> &Invocation {
        &self.0
    }

    /// A nested [`Call`] under this capability's invocation. Never refused:
    /// the live capability this recorder is borrowed from keeps the
    /// invocation Open (a counted one keeps a count above zero, an uncounted
    /// one the sticky saturation flag, and the seal's exchange needs
    /// neither).
    #[inline]
    pub(crate) fn call(&self) -> Call {
        Call {
            rec: Recorder(self.0.clone()),
            counted: self.0.mint(word::GUARD).unwrap_or(false),
        }
    }

    /// A [`Permit`] for a value that records outside any public call, through
    /// background work or its own `Drop`, minted under this capability (so
    /// never refused, as [`call`](Self::call)). Held past its last recording.
    pub(crate) fn permit(&self) -> Permit {
        Permit {
            rec: Recorder(self.0.clone()),
            counted: self.0.mint(word::PERMIT).unwrap_or(false),
        }
    }

    /// A census ticket of `kind` for a value issued to the caller under
    /// this capability.
    pub(crate) fn ticket(&self, kind: Kind) -> Ticket {
        self.0.census_adjust(kind, true);
        Ticket {
            invocation: self.0.clone(),
            kind,
        }
    }

    /// Emit the phase marker `name` to the sink, if there is one.
    #[inline]
    pub(crate) fn marker(&self, name: &str) {
        if !matches!(self.0.0.sink, SinkSlot::None) {
            self.deliver(|sink| sink.marker(name));
        }
    }

    /// Emit counter `name` with `value` to the sink, if there is one. The
    /// wire value is an `i64`: signed values are sent exactly, unsigned ones
    /// saturate at `i64::MAX` rather than wrap, so callers pass them as they
    /// are instead of casting.
    #[inline]
    pub(crate) fn counter(&self, name: &str, value: impl crate::debug::CounterValue) {
        if !matches!(self.0.0.sink, SinkSlot::None) {
            let value = value.to_counter();
            self.deliver(|sink| sink.counter(name, value));
        }
    }

    /// Run `emit` against the sink, and then against the tap if there is
    /// one, each with its unwinding contained: a sink (or tap) that panics is
    /// latched off for the rest of the invocation and the run carries on,
    /// because instrumentation is observational and may neither change a
    /// published result nor turn an unwind into an abort. The caught payload
    /// is forgotten, not dropped: its destructor is caller code too and could
    /// panic again (so a payload holding a capability keeps it alive, and
    /// vetoes a seal, for good). An emission already past the latch when
    /// another thread's panics may still complete; later ones are skipped.
    /// Panics that do not unwind (`panic = "abort"`, a panicking panic hook)
    /// cannot be contained.
    ///
    /// Every emission runs while the capability this recorder is borrowed
    /// from is alive, and that capability retires with `Release` after it
    /// returns: so when a seal's exchange succeeds, every callback into the
    /// sink and the tap has returned, and none can start.
    #[cold]
    #[inline(never)]
    fn deliver(&self, emit: impl Fn(&dyn crate::debug::InstrumentSink)) {
        let state = &self.0.0;
        match &state.sink {
            SinkSlot::None => {}
            SinkSlot::Generic(sink) => contained(&state.sink_failed, || emit(sink.as_ref())),
            SinkSlot::Call {
                sink,
                tap,
                tap_failed,
            } => {
                contained(&state.sink_failed, || emit(sink));
                if let Some(tap) = tap {
                    contained(tap_failed, || emit(tap.as_ref()));
                }
            }
        }
    }

    /// The publication log, poison recovered: no mutation of it can unwind
    /// half done (see [`crate::publication`]).
    pub(crate) fn log(&self) -> std::sync::MutexGuard<'_, crate::publication::PublicationLog> {
        self.0.log_guard()
    }

    /// The invocation's metric sets, for the recording sites. The borrow is
    /// the capability's.
    #[inline]
    pub(crate) fn metrics(&self) -> &crate::counters::InvocationMetrics {
        &self.0.0.metrics
    }

    /// Claim the once-per-invocation diagnostic `key`: `true` the first time
    /// any part of this invocation asks (the caller then emits it), `false`
    /// ever after, and after [`mark_reported`](Self::mark_reported). The
    /// lock is released before the caller emits, so a slow diagnostic sink
    /// never holds it.
    pub(crate) fn first_report(&self, key: &'static str) -> bool {
        self.0
            .0
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
    /// guard is held from before the decision until the row is finalized.
    /// Caller code that runs under it (hooks, the instrument sink) publishing
    /// under the same invocation is refused as re-entry, never waited on. A
    /// poisoned lock is recovered: it guards no state, only exclusion.
    pub(crate) fn publication_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.0
            .0
            .publishing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A public entry's admission: held for the entry's extent, it keeps the
/// invocation from sealing and lends the [`Recorder`]. Not `Clone`: a
/// nested entry mints its own with [`Recorder::call`].
pub(crate) struct Call {
    rec: Recorder,
    counted: bool,
}

impl Call {
    /// A call on a fresh invocation of its own, which nothing else can seal:
    /// for the library's throwaway runs (a probe that reads a header) and for
    /// tests.
    pub(crate) fn detached() -> Self {
        Invocation::new().into_call()
    }
}

impl Invocation {
    /// A call on this invocation, which only this value holds and nobody can
    /// have sealed.
    fn into_call(self) -> Call {
        let counted = self.mint(word::GUARD).unwrap_or(false);
        Call {
            rec: Recorder(self),
            counted,
        }
    }
}

impl std::ops::Deref for Call {
    type Target = Recorder;
    #[inline]
    fn deref(&self) -> &Recorder {
        &self.rec
    }
}

impl Drop for Call {
    #[inline]
    fn drop(&mut self) {
        if self.counted {
            self.rec.0.retire(word::GUARD);
        }
    }
}

/// Permission to record outside a public call, held by a value that records
/// through background work or its own `Drop`, past its last recording.
/// Cloning mints another permit.
pub(crate) struct Permit {
    rec: Recorder,
    counted: bool,
}

impl std::ops::Deref for Permit {
    type Target = Recorder;
    #[inline]
    fn deref(&self) -> &Recorder {
        &self.rec
    }
}

impl Clone for Permit {
    fn clone(&self) -> Self {
        self.rec.permit()
    }
}

impl Drop for Permit {
    #[inline]
    fn drop(&mut self) {
        if self.counted {
            self.rec.0.retire(word::PERMIT);
        }
    }
}

impl std::fmt::Debug for Permit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Permit").field(&self.rec.0).finish()
    }
}

/// One issued value in its invocation's census, retired when the value
/// ceases to exist.
pub(crate) struct Ticket {
    invocation: Invocation,
    kind: Kind,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.invocation.census_adjust(self.kind, false);
    }
}

impl std::fmt::Debug for Ticket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Ticket").field(&self.kind).finish()
    }
}

/// A [`Host`] entered by a public command: the host's facilities, and a
/// [`Call`] admitting the command in place of the host's control handle.
/// Inside a command `host.invocation` is that call, so recording compiles
/// only on an entered host; a helper given a plain `&Host` cannot record.
/// Derefs to the host it entered, so a nested public command takes it as a
/// `&Host` and enters again.
pub(crate) struct Entered<'h> {
    host: &'h Host,
    /// The command's admission, shadowing [`Host::invocation`].
    pub(crate) invocation: Call,
}

impl std::ops::Deref for Entered<'_> {
    type Target = Host;
    #[inline]
    fn deref(&self) -> &Host {
        self.host
    }
}

impl Host {
    /// Admit a public command under this host's invocation: refused with
    /// [`ErrorKind::Sealed`](crate::ErrorKind::Sealed) once it has been
    /// sealed.
    #[inline]
    pub(crate) fn enter(&self) -> crate::Result<Entered<'_>> {
        Ok(Entered {
            host: self,
            invocation: self.invocation.call()?,
        })
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;

    #[test]
    fn calls_and_permits_count_and_retire() {
        let inv = Invocation::new();
        let call = inv.call().expect("open");
        let permit = call.permit();
        let again = permit.clone();
        let c = inv.capabilities();
        assert_eq!((c.guards, c.permits, c.saturated), (1, 2, false));
        drop((call, permit, again));
        assert_eq!(inv.capabilities(), Capabilities::default());
    }

    #[test]
    fn tickets_count_by_kind() {
        let inv = Invocation::new();
        let call = inv.call().expect("open");
        let a = call.ticket(Kind::Buffer);
        let b = call.ticket(Kind::Writer);
        drop(call);
        let c = inv.capabilities();
        assert_eq!((c.buffer, c.writer, c.session, c.output), (1, 1, 0, 0));
        drop((a, b));
        assert_eq!(inv.capabilities(), Capabilities::default());
    }

    fn sealed(outcome: &SealOutcome) -> Arc<Snapshot> {
        match outcome {
            SealOutcome::Sealed(snapshot) => Arc::clone(snapshot),
            other => panic!("not sealed: {other:?}"),
        }
    }

    /// A seal closes admission for good: a public entry is refused with
    /// `Sealed`, cancel ends nothing, and a later seal returns the same
    /// snapshot.
    #[test]
    fn a_sealed_invocation_admits_nothing() {
        let inv = Invocation::new();
        let first = sealed(&inv.seal());
        assert_eq!(first.len(), {
            let mut n = 0;
            inv.for_each_counter(|_, _| n += 1);
            n
        });
        let refused = inv.call().err().expect("refused");
        assert!(matches!(refused.kind(), crate::ErrorKind::Sealed));
        assert_eq!(inv.cancel(), crate::publication::CancelOutcome::Ended);
        assert!(!inv.is_cancelled(), "a cancel after the seal trips nothing");
        assert!(Arc::ptr_eq(&first, &sealed(&inv.seal())));
        assert!(inv.stream_account().is_none(), "no call sink");
    }

    /// The classification order: a call in progress first, then the
    /// census once only permits are alive.
    #[test]
    fn a_refused_seal_says_why() {
        let inv = Invocation::new();
        let call = inv.call().expect("open");
        let permit = call.permit();
        let ticket = call.ticket(Kind::Writer);
        assert!(matches!(
            inv.seal(),
            SealOutcome::Refused(SealRefusal::InProgress)
        ));
        drop(call);
        match inv.seal() {
            SealOutcome::Outstanding(census) => {
                assert_eq!(
                    census,
                    Census {
                        writer: 1,
                        ..Census::default()
                    }
                );
            }
            other => panic!("{other:?}"),
        }
        drop(ticket);
        assert!(matches!(
            inv.seal(),
            SealOutcome::Refused(SealRefusal::Unticketed)
        ));
        drop(permit);
        sealed(&inv.seal());
    }

    /// A saturated count refuses the seal for good, ahead of zero counts.
    #[test]
    fn a_saturated_invocation_never_seals() {
        use std::sync::atomic::Ordering::Relaxed;
        let inv = Invocation::new();
        inv.0
            .word
            .0
            .store(word::max(word::GUARD) * word::GUARD, Relaxed);
        drop(inv.call().expect("open"));
        inv.0
            .word
            .0
            .fetch_and(!(word::max(word::GUARD) * word::GUARD), Relaxed);
        assert!(matches!(
            inv.seal(),
            SealOutcome::Refused(SealRefusal::Saturated)
        ));
        assert!(inv.call().is_ok(), "still open");
    }

    /// Whether the invocation's state is Open, read with `Acquire`.
    fn open(inv: &Invocation) -> bool {
        inv.0.word.0.load(std::sync::atomic::Ordering::Acquire) & word::STATE == word::OPEN
    }

    /// Nesting step by step: a nested call outlives the call it was minted
    /// under and still holds the seal off, and so does a permit minted under
    /// it once both calls are gone (unticketed).
    #[test]
    fn a_nested_capability_holds_the_seal_off() {
        let inv = Invocation::new();
        let outer = inv.call().expect("open");
        let nested = outer.call();
        drop(outer);
        assert!(matches!(
            inv.seal(),
            SealOutcome::Refused(SealRefusal::InProgress)
        ));
        let permit = nested.permit();
        drop(nested);
        assert!(matches!(
            inv.seal(),
            SealOutcome::Refused(SealRefusal::Unticketed)
        ));
        let again = permit.clone();
        drop(permit);
        assert!(open(&inv), "a cloned permit is minted under the live one");
        drop(again);
        sealed(&inv.seal());
    }

    /// The exchange racing retirements and nested mints. Every worker first
    /// holds a call, a nested call and a permit at once, and only then is
    /// the sealer's first attempt is refused as in progress (a rendezvous,
    /// so the race is not won before any worker minted). Workers then loop
    /// minting and retiring until a mint is refused or a bound is reached,
    /// while the sealer retries. Whenever a worker holds a capability it
    /// reads the state as Open: a live capability excludes the seal. Once
    /// sealed, nothing is counted and admission is refused.
    #[test]
    fn the_seal_exchange_races_retirement_and_nested_calls() {
        use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
        const WORKERS: usize = 4;
        for _ in 0..20 {
            let inv = Invocation::new();
            let held = Arc::new(std::sync::Barrier::new(WORKERS + 1));
            let go = Arc::new(std::sync::Barrier::new(WORKERS + 1));
            let seen_closed = Arc::new(AtomicBool::new(false));
            let workers: Vec<_> = (0..WORKERS)
                .map(|_| {
                    let (inv, held, go, seen_closed) = (
                        inv.clone(),
                        Arc::clone(&held),
                        Arc::clone(&go),
                        Arc::clone(&seen_closed),
                    );
                    std::thread::spawn(move || {
                        let call = inv.call().expect("open before the sealer starts");
                        let nested = call.call();
                        let permit = nested.permit();
                        held.wait();
                        go.wait();
                        if !open(&inv) {
                            seen_closed.store(true, SeqCst);
                        }
                        drop((permit, nested, call));
                        // Bounded, so a sealer that keeps losing the race
                        // wins once the workers are done.
                        for _ in 0..5_000 {
                            let Ok(call) = inv.call() else {
                                return;
                            };
                            let nested = call.call();
                            let permit = nested.permit();
                            if !open(&inv) {
                                seen_closed.store(true, SeqCst);
                            }
                            drop((nested, call));
                            if !open(&inv) {
                                seen_closed.store(true, SeqCst);
                            }
                            drop(permit);
                        }
                    })
                })
                .collect();
            held.wait();
            assert!(
                matches!(inv.seal(), SealOutcome::Refused(SealRefusal::InProgress)),
                "every worker holds a call"
            );
            go.wait();
            let snapshot = loop {
                if let SealOutcome::Sealed(snapshot) = inv.seal() {
                    break snapshot;
                }
                std::hint::spin_loop();
            };
            for w in workers {
                w.join().expect("worker");
            }
            assert!(
                !seen_closed.load(SeqCst),
                "a capability was alive while the invocation was not Open"
            );
            assert!(inv.call().is_err());
            assert!(Arc::ptr_eq(&snapshot, &sealed(&inv.seal())));
            let c = inv.capabilities();
            assert_eq!((c.guards, c.permits), (0, 0));
        }
    }

    /// A field at its maximum is not incremented: the sticky flag is set,
    /// the capability goes uncounted, and retiring every counted one leaves
    /// the flag in place.
    #[test]
    fn saturation_is_sticky_and_uncounted() {
        use std::sync::atomic::Ordering::Relaxed;
        for unit in [word::GUARD, word::PERMIT] {
            let inv = Invocation::new();
            inv.0.word.0.store(word::max(unit) * unit, Relaxed);
            assert!(word::is_full(inv.0.word.0.load(Relaxed), unit));
            assert_eq!(inv.mint(unit), Some(false), "uncounted, and admitted");
            let after = inv.0.word.0.load(Relaxed);
            assert_eq!(word::field(after, unit), word::max(unit));
            assert_ne!(after & word::SATURATED, 0);
            for _ in 0..3 {
                inv.retire(unit);
            }
            let c = inv.capabilities();
            assert!(c.saturated, "saturation is never cleared");
            assert_eq!(
                word::field(inv.0.word.0.load(Relaxed), unit),
                word::max(unit) - 3
            );
        }
    }

    #[test]
    fn guard_and_permit_fields_do_not_overlap() {
        let full = word::max(word::GUARD) * word::GUARD;
        assert_eq!(word::field(full, word::PERMIT), 0);
        assert_eq!(full & word::SATURATED, 0);
        let permits = word::max(word::PERMIT) * word::PERMIT;
        assert_eq!(word::field(permits, word::GUARD), 0);
        assert_eq!(permits & word::SATURATED, 0);
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
///
/// Every eviction of an input a run issues counts into its invocation's
/// `fadvise_input_evictions` counter ([`Invocation::for_each_counter`]), so
/// a run under `Keep` can be checked to have asked for none. Not every
/// command evicts under `Evict`: `cat`, which walks raw frames, leaves the
/// page cache alone either way.
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
/// through this executor. Their worker counts come from explicit options,
/// resource inputs or fixed tunables. Pipeline coordinators and consumers may
/// run on the invoking thread, and may dispatch shared rayon tasks through
/// this executor. Merge-changes with `Parallelism::Fixed(n)` runs on a
/// dedicated rayon pool of `n` threads instead of this executor; with
/// `Parallelism::Auto` it uses this executor.
///
/// The rayon pools the library builds itself - the pipelined reader's decode
/// pool and merge-changes' `Fixed(n)` pool - are joined: every one of their
/// worker threads has exited, thread-local destructors included, before the
/// call that built them ends (for `ElementReader::into_blocks_pipelined`,
/// before the returned iterator's drop returns), on success, error and unwind
/// alike. A caller callback can run on one of those workers, and what it
/// leaves there is waited for; see [`crate::debug::InstrumentSink`]. The
/// other dedicated worker sets are `std` threads whose work ends before the
/// call or session does; those a `std::thread::scope` waits for without an
/// explicit join are waited for only to the end of their main function, as
/// that scope does, so their thread-local destructors may still be running
/// when the call returns.
///
/// `Ambient` uses whatever pool the calling thread is in, or rayon's global
/// pool off any pool, exactly as `rayon::spawn` and the par-iterators do.
/// `Pool` confines the work to the caller's pool. A caller can check where a
/// writer's framing ran: the invocation counter
/// `writer_payload_framed_on_executor_items` counts the framed payloads whose
/// framing ran on a worker of the writer's executor, and equals
/// `writer_payload_framed_items` when every one did. Three things follow from
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
    /// This executor, borrowed for one call.
    pub(crate) fn borrowed(&self) -> ExecutorRef<'_> {
        match self {
            Self::Ambient => ExecutorRef::Ambient,
            Self::Pool(pool) => ExecutorRef::Pool(pool),
        }
    }
    /// Run `op` inside the pool (`Ambient`: a direct call).
    pub(crate) fn install<R: Send>(&self, op: impl FnOnce() -> R + Send) -> R {
        self.borrowed().install(op)
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
        self.borrowed().yield_now()
    }
}
/// An executor borrowed for one call: the host's [`Executor`], or a pool the
/// library built for the call
/// ([`JoinedPool`](crate::joined_pool::JoinedPool)), which can be lent but
/// never kept. Its methods wait for the work they start (there is no
/// detached `spawn`); they cannot stop code running inside that work from
/// submitting detached rayon work of its own.
#[derive(Clone, Copy)]
pub(crate) enum ExecutorRef<'a> {
    /// The calling thread's rayon pool, or rayon's global pool off any pool.
    Ambient,
    /// A borrowed pool.
    Pool(&'a rayon::ThreadPool),
}
impl ExecutorRef<'_> {
    /// Run `op` inside the pool (`Ambient`: a direct call).
    pub(crate) fn install<R: Send>(self, op: impl FnOnce() -> R + Send) -> R {
        match self {
            Self::Ambient => op(),
            Self::Pool(pool) => pool.install(op),
        }
    }
    /// Run `op` with a scope whose spawned work runs on the pool (`Ambient`:
    /// `rayon::scope`, i.e. the calling thread's pool, or the global pool off
    /// any pool). Returns once `op` and everything it spawned has finished.
    pub(crate) fn scope<'scope, R: Send>(
        self,
        op: impl FnOnce(&rayon::Scope<'scope>) -> R + Send,
    ) -> R {
        match self {
            Self::Ambient => rayon::scope(op),
            Self::Pool(pool) => pool.scope(op),
        }
    }
    /// Run one pending job of the calling worker's pool, if any. `None` off
    /// the pool.
    pub(crate) fn yield_now(self) -> Option<rayon::Yield> {
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
    /// The event is the workload, not a prescribed response. The CLI's
    /// response is two glibc settings: two arenas (`M_ARENA_MAX`), and a
    /// pinned 32 MiB `M_MMAP_THRESHOLD`, which also disables glibc's sliding
    /// threshold (the trim threshold keeps its current value). The pin keeps
    /// multi-MB build and frame buffers in the shared arenas for reuse rather
    /// than mapping and unmapping them per blob: once the decode buffers are
    /// recycled (read/decompress.rs) nothing else raises the sliding
    /// threshold early, and time-filter at planet spent 20 % more wall in the
    /// kernel without the pin (reference/performance.md, "Decode buffers
    /// sized from the blob and recycled"). Neither setting bounds memory:
    /// arena reuse trades returning freed pages to the OS for not faulting
    /// them in again.
    ///
    /// Only some commands that run worker pools request it. The others were
    /// never checked for cross-thread alloc/free patterns, so not calling it
    /// is not a decision that the cap is unneeded or harmful there. To
    /// settle it, measure those commands on a large input, then either call
    /// this once from the CLI's `main` as process policy and drop the
    /// per-command calls, or document here, per command, why it must not be
    /// capped.
    fn cross_thread_alloc_pool(&self);
    /// Request permission to hold `wanted` descriptors at once. The answer's
    /// [`allowance`](FdReservation::allowance) is how many the work may open
    /// on top of those already open in the process; the library holds the
    /// [`FdReservation`] until the descriptors that request was sized for are
    /// closed, then drops it.
    ///
    /// The snapshot providers ([`NoProcessActions`], and
    /// [`process::raise_nofile_to_hard_cap`](crate::process::raise_nofile_to_hard_cap)
    /// wrapped in [`FdReservation::unreserved`]) subtract a snapshot count of
    /// open descriptors from the soft limit. That is an estimate under
    /// stable descriptor use, not a reservation: two calls probing at once
    /// both see the same free descriptors. A host running descriptor-heavy
    /// calls concurrently uses [`FdPool`], whose grants come out of one
    /// budget and return to it when their reservation drops.
    fn fd_allowance(&self, wanted: u64) -> FdReservation;
}
/// Leave process policy unchanged.
pub struct NoProcessActions;
impl ProcessActions for NoProcessActions {
    fn cross_thread_alloc_pool(&self) {}
    fn fd_allowance(&self, _wanted: u64) -> FdReservation {
        FdReservation::unreserved(crate::process::current_nofile())
    }
}

/// A reserving descriptor provider for a caller running descriptor-heavy
/// calls concurrently: one budget shared by every `Host` holding this pool,
/// from which each request is granted `min(wanted, free)` as an
/// [`FdAllowance::host_grant`], returned to the budget when the request's
/// [`FdReservation`] drops. Concurrent calls therefore never hold grants
/// summing to more than the budget, whatever their timing.
///
/// The budget is the caller's carve-out of the soft `RLIMIT_NOFILE` for
/// the descriptors the library asks for, and nothing else. A request
/// covers only the demand it was sized for: `cat`'s pinned inputs and
/// `cat --dedupe`'s pass-2 sources (each plus a small fixed headroom),
/// `add-locations-to-ways --index-type external`'s stage-1 shard files
/// (released when stage 1 ends). Every other descriptor is the caller's to
/// keep under the soft limit less the budget, summed over every call that
/// can run at once: the caller's own, each call's outputs, scratch, inputs
/// it opens outside a request, later external stages, and the commands
/// that ask for nothing. The pool never reads the limit or counts open
/// descriptors, so its grants depend on the budget and the reservations
/// outstanding alone.
///
/// Grants are first come, first served and never wait: a request finding
/// the budget short is granted what is left, and a command needing all of
/// its request (`cat`) refuses and drops the grant at once.
/// `cross_thread_alloc_pool` is forwarded to `inner`.
pub struct FdPool {
    state: Arc<FdPoolState>,
    inner: Arc<dyn ProcessActions>,
}

struct FdPoolState {
    budget: u64,
    free: std::sync::atomic::AtomicU64,
}

impl FdPool {
    /// A pool of `budget` descriptors, forwarding the other process
    /// actions to `inner`.
    pub fn new(budget: u64, inner: Arc<dyn ProcessActions>) -> Self {
        Self {
            state: Arc::new(FdPoolState {
                budget,
                free: std::sync::atomic::AtomicU64::new(budget),
            }),
            inner,
        }
    }

    /// The budget the pool was built with.
    pub fn budget(&self) -> u64 {
        self.state.budget
    }

    /// Descriptors not granted to any outstanding reservation now. A
    /// snapshot: concurrent requests and releases move it.
    pub fn available(&self) -> u64 {
        self.state.free.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl ProcessActions for FdPool {
    fn cross_thread_alloc_pool(&self) {
        self.inner.cross_thread_alloc_pool();
    }
    fn fd_allowance(&self, wanted: u64) -> FdReservation {
        use std::sync::atomic::Ordering;
        let before = self
            .state
            .free
            .update(Ordering::AcqRel, Ordering::Acquire, |free| {
                free - wanted.min(free)
            });
        let taken = wanted.min(before);
        let state = Arc::clone(&self.state);
        FdReservation::with_release(FdAllowance::host_grant(taken), move || {
            state.free.fetch_add(taken, Ordering::AcqRel);
        })
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

    /// [`for_tests`](Self::for_tests), entered: what a unit test hands the
    /// crate's internal helpers, which take an entered host. The host is
    /// leaked, so the entry can outlive the expression that made it.
    pub(crate) fn entered_for_tests() -> Entered<'static> {
        Self::for_tests().entered_leaked()
    }

    /// This host, leaked and entered, for a unit test that built its own.
    pub(crate) fn entered_leaked(self) -> Entered<'static> {
        Box::leak(Box::new(self))
            .enter()
            .expect("a fresh host's invocation is open")
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
        // Membership is of that very pool: a worker of another pool is not
        // one, nested inside this pool's install or not, and a worker of
        // this pool installed into another is not one while it runs there.
        let other = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("other pool");
        assert!(!other.install(|| executor.on_worker()));
        assert!(!executor.install(|| other.install(|| executor.on_worker())));
        assert!(other.install(|| executor.install(|| executor.on_worker())));
    }
    /// Grants never sum past the budget while held, a short request gets
    /// what is left, each drop returns exactly its grant, and the other
    /// process action goes to the inner provider.
    #[test]
    fn fd_pool_grants_conserve_the_budget() {
        struct Count(std::sync::atomic::AtomicUsize);
        impl ProcessActions for Count {
            fn cross_thread_alloc_pool(&self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            fn fd_allowance(&self, _wanted: u64) -> FdReservation {
                panic!("the pool never forwards a descriptor request")
            }
        }
        let inner = Arc::new(Count(std::sync::atomic::AtomicUsize::new(0)));
        let pool = FdPool::new(100, Arc::clone(&inner) as Arc<dyn ProcessActions>);
        pool.cross_thread_alloc_pool();
        assert_eq!(inner.0.load(std::sync::atomic::Ordering::Relaxed), 1);

        let a = pool.fd_allowance(60);
        let b = pool.fd_allowance(60);
        let c = pool.fd_allowance(1);
        assert_eq!(a.allowance(), FdAllowance::host_grant(60));
        assert_eq!(b.allowance(), FdAllowance::host_grant(40), "what is left");
        assert_eq!(c.allowance(), FdAllowance::host_grant(0));
        assert_eq!(pool.available(), 0);
        drop(b);
        assert_eq!(pool.available(), 40);
        drop((a, c));
        assert_eq!(pool.available(), pool.budget());

        // Every thread holds its grant across a barrier, so all of them are
        // outstanding at once when the sum is taken.
        let threads = 16;
        let barrier = std::sync::Barrier::new(threads);
        let held = std::sync::atomic::AtomicU64::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    let grant = pool.fd_allowance(13);
                    held.fetch_add(
                        grant.allowance().descriptors,
                        std::sync::atomic::Ordering::AcqRel,
                    );
                    barrier.wait();
                    assert_eq!(
                        held.load(std::sync::atomic::Ordering::Acquire),
                        100,
                        "outstanding grants take the whole budget, never more"
                    );
                    assert_eq!(pool.available(), 0);
                    barrier.wait();
                });
            }
        });
        assert_eq!(pool.available(), 100, "every grant came back");
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
///
/// An event can be delivered on a worker of a rayon pool the library built
/// for the call, and that pool is joined before the call ends, unwinding
/// included: rayon work a sink submits to the current pool there, and the
/// destruction of thread-local values it creates there, must not wait for
/// the call to end or for anything the invoking thread holds until it does.
/// [`InstrumentSink`](crate::debug::InstrumentSink) states the same rule.
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
        invocation: &Recorder,
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
        let (first, second) = (Call::detached(), Call::detached());
        d.emit_once(&first, EventKind::Warning, "key", format_args!("first"));
        d.clone()
            .emit_once(&second, EventKind::Warning, "key", format_args!("second"));
        let shared = first.handle().clone().call().expect("open");
        other.emit_once(&shared, EventKind::Warning, "key", format_args!("again"));
        d.emit_once(&second, EventKind::Warning, "key", format_args!("again"));
        assert_eq!(*sink.0.lock().expect("sink"), ["first", "second"]);
    }
    #[test]
    fn a_reported_key_silences_a_later_once_event() {
        let sink = Arc::new(Sink::default());
        let d = Diagnostics::new(Arc::clone(&sink) as Arc<dyn DiagnosticSink>);
        let invocation = Call::detached();
        invocation.mark_reported("key");
        d.emit_once(&invocation, EventKind::Note, "key", format_args!("note"));
        d.emit_once(&invocation, EventKind::Note, "other", format_args!("other"));
        // Another invocation is not covered by the first one's report.
        d.emit_once(
            &Call::detached(),
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
