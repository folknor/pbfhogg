//! Sidecar counter machinery.
//!
//! The shared pieces below; counter code uses them rather than hand-rolling
//! its own. Plain values (stats fields, schedule lengths, local tallies) need
//! none of them: `Invocation::counter` takes unsigned integers directly
//! and saturates, so they are passed without an `as i64` cast (a source check
//! in `scripts/check_source_invariants.py` refuses one inside its arguments).
//!
//! - [`counter_set!`] declares a set of `AtomicU64` counters from one list
//!   that pairs every field with the exact name it is emitted under, so a
//!   counter is spelled once (field, sidecar name and emission order all come
//!   from the same line) and every sidecar name stays greppable verbatim.
//! - [`record_max`] is the "track the maximum" update.
//! - [`Stopwatch`] turns elapsed time into a `u64` for timed counters.
//! - [`lock_recording_wait`], [`send_recording_wait`] and
//!   [`recv_recording_wait`] are the gated blocking operations behind a
//!   `*_wait_ns` stall counter (see their section below).
//! - [`StallGauge`] is the depth-gated `WAIT_*` marker span.
//!
//! # Names are a contract
//!
//! Stored brokkr results are compared by counter name across commits, so a
//! refactor keeps every emitted counter's name, unit, recording point and
//! aggregation, and the `WAIT_*` span names. That includes per-sample
//! truncation where an existing `_ms` counter uses it (see [`Stopwatch`]); it
//! does not ask new counters to truncate. A counter whose meaning has to
//! change is retired and replaced under a new name rather than redefined, so
//! a name-keyed comparison against old rows finds no counterpart instead of
//! a false one; the retirement is recorded in `reference/performance/README.md`.
//! `scripts/counter_names_diff.py <before> <after>` diffs two stored runs'
//! counter and marker names; equal names do not prove equal units or
//! recording, which the diff itself is read for.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, RecvError, SendError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant};

/// Declare a set of sidecar counters.
///
/// ```ignore
/// crate::counters::counter_set! {
///     /// Docs for the set.
///     pub(super) struct StageCounters {
///         /// Docs for one counter.
///         pread_ms => "s9_pread_ms",
///         // Accumulated in nanoseconds, emitted in milliseconds.
///         write_ns => "s9_write_ms" / 1_000_000,
///         // No name: counted and read in place, never emitted by the set.
///         scanned,
///     }
///     // Optional non-counter fields, with their (const) initialisers.
///     extra {
///         emitted: AtomicBool = AtomicBool::new(false),
///     }
/// }
/// ```
///
/// Generates the struct (one `AtomicU64` per counter, then the extra fields),
/// a `const fn new()`, `Default`, and two methods: `for_each(|name, value|)`
/// (crate-visible, read-only), which visits every counter in declaration
/// order with its emitted name and its value (loaded, then divided by the
/// optional divisor), and the **private** `emit_all(invocation)`, which
/// publishes that visit through the invocation's sink (saturating at
/// `i64::MAX`).
/// A private inherent method is reachable only from the module that invokes
/// the macro and its children, so a set whose emission is restricted to one
/// route (the end-of-invocation emitters) keeps that route as its only way
/// out.
///
/// A counter written without `=> name` is declared in place (so it keeps its
/// position among its neighbours) but neither visited nor emitted: its
/// owner reads it directly. The `extra` block is for fields that are not
/// `AtomicU64` counters at all.
///
/// Attributes on a counter are copied to its field declaration only; do not
/// put `#[cfg]` on a counter (the initialiser and the visit would not see
/// it). A counter that only some builds bump stays declared and reads zero.
macro_rules! counter_set {
    (
        $(#[$sattr:meta])*
        $vis:vis struct $name:ident {
            $(
                $(#[$fattr:meta])*
                $fvis:vis $field:ident $(=> $emit:literal $(/ $div:literal)?)?
            ),* $(,)?
        }
        $(extra {
            $(
                $(#[$xattr:meta])*
                $xvis:vis $xfield:ident : $xty:ty = $xinit:expr
            ),* $(,)?
        })?
    ) => {
        $(#[$sattr])*
        $vis struct $name {
            $(
                $(#[$fattr])*
                $fvis $field: ::std::sync::atomic::AtomicU64,
            )*
            $($(
                $(#[$xattr])*
                $xvis $xfield: $xty,
            )*)?
        }

        #[allow(dead_code)]
        impl $name {
            $vis const fn new() -> Self {
                Self {
                    $($field: ::std::sync::atomic::AtomicU64::new(0),)*
                    $($($xfield: $xinit,)*)?
                }
            }

            /// Visit every named counter in declaration order: its emitted
            /// name and its current value, divided by its divisor if it has
            /// one. Unnamed counters are skipped.
            pub(crate) fn for_each(&self, mut visit: impl FnMut(&'static str, u64)) {
                $($(
                    visit(
                        $emit,
                        self.$field.load(::std::sync::atomic::Ordering::Relaxed) $(/ $div)?,
                    );
                )?)*
            }

            /// Emit every counter to `invocation`'s sink, in declaration
            /// order.
            fn emit_all(&self, invocation: &$crate::host::Recorder) {
                if invocation.instrumented() {
                    self.for_each(|name, value| invocation.counter(name, value));
                }
            }
        }

        impl ::std::default::Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}
pub(crate) use counter_set;

/// One metric set on cache lines of its own.
///
/// The sets of an [`InvocationMetrics`] sit in the invocation's allocation
/// beside its cancellation flag, which every checkpoint reads on many
/// threads. Aligning each set to 128 bytes (two lines, for the adjacent-line
/// prefetcher) keeps a hot counter from sharing a line with the flag or with
/// another set; within a set the counters share lines exactly as they did
/// when each set was its own static.
#[repr(align(128))]
#[derive(Default)]
pub(crate) struct Padded<T>(pub(crate) T);

impl<T> std::ops::Deref for Padded<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.0
    }
}

/// Every engine-level metric set of one invocation.
///
/// These used to be process-global statics that were never reset, so two
/// library calls in one process, concurrent or one after the other, added
/// into the same totals (ADR-0016 says the library owns no process state).
/// Each [`crate::host::Invocation`] now owns one of these, and every pass of
/// that invocation records into it: a run that reads twice still reports the
/// sum of both passes under the same names, and the CLI, one invocation per
/// process, reports exactly the totals it did when the sets were statics.
///
/// Each counter only grows for the life of the invocation (sums for counts
/// and waits, maxima for the high-water and peak counters), which is what
/// `brokkr sidecar --stalls` assumes when it takes the maximum per name.
///
/// Command-local sets (renumber's, altw's, apply-changes's) are not here:
/// they were always locals of their run.
#[derive(Default)]
pub(crate) struct InvocationMetrics {
    /// An explicit zero STATX_DIOALIGN report on a direct open in this run.
    pub(crate) direct_io_zero_alignment: std::sync::atomic::AtomicBool,
    pub(crate) pipeline: Padded<crate::read::pipeline_metrics::PipelineMetrics>,
    pub(crate) decompress: Padded<crate::read::pipeline_metrics::DecompressPoolMetrics>,
    pub(crate) unordered: Padded<crate::scan::classify::UnorderedPoolMetrics>,
    pub(crate) ordered: Padded<crate::ordered_pool::OrderedPoolMetrics>,
    pub(crate) writer: Padded<crate::write::metrics::WriterMetrics>,
    pub(crate) fadvise: Padded<crate::fadvise::FadviseMetrics>,
    /// Latch for the end-of-invocation read emission
    /// (`crate::read::emit_read_counters`), so repeat calls are no-ops.
    pub(crate) read_emitted: std::sync::atomic::AtomicBool,
}

impl InvocationMetrics {
    /// Visit every counter of every set, in a fixed order, with the name it
    /// is emitted under and its current value.
    pub(crate) fn for_each(&self, mut visit: impl FnMut(&'static str, u64)) {
        self.pipeline.for_each(&mut visit);
        self.decompress.for_each(&mut visit);
        self.unordered.for_each(&mut visit);
        self.ordered.for_each(&mut visit);
        self.writer.for_each(&mut visit);
        self.fadvise.for_each(&mut visit);
    }
}

/// Raise `field` to `candidate` if `candidate` is larger.
///
/// Hand-written on purpose, not `AtomicU64::fetch_max`. On x86-64 there is no
/// atomic max instruction: `fetch_max` compiles to a `lock cmpxchg` loop that
/// performs the locked read-modify-write on every call, taking the cache line
/// exclusive even when the maximum does not move. This loads first and only
/// attempts the compare-exchange while the candidate is larger, so in steady
/// state (the maximum reached early, later candidates smaller) a call is a
/// plain load and the line can stay shared across the workers reporting into
/// it. When the candidate is larger the two cost the same (`fetch_max` also
/// loads before its loop). The high-water and peak counters on the per-blob
/// read, write and altw paths all go through here; none of them has been
/// measured either way.
#[inline]
pub(crate) fn record_max(field: &AtomicU64, candidate: u64) {
    let mut current = field.load(Relaxed);
    while candidate > current {
        match field.compare_exchange_weak(current, candidate, Relaxed, Relaxed) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

/// A started clock for a timed counter.
///
/// Elapsed time as a `u64`: nanoseconds and milliseconds both saturate at `u64::MAX` (unreachable in practice: about 584 years of
/// nanoseconds). Every method reads the clock afresh and none resets the
/// start, so one stopwatch can feed several counters; a caller that needs the
/// *same* sample in more than one place (a total and a maximum, or two
/// counters that must agree exactly) takes it once with
/// [`elapsed_ns`](Self::elapsed_ns) / [`elapsed_ms`](Self::elapsed_ms) and
/// reuses it.
///
/// Millisecond counters truncate **per sample**: each recording adds whole
/// milliseconds, so a sample under 1 ms adds nothing and a counter fed by
/// many short samples undercounts. That is deliberate continuity - the
/// existing `_ms` counters were kept with this meaning so they stay
/// comparable with earlier result sets, and nanosecond twins were added where
/// resolution mattered. New timed counters should accumulate nanoseconds and,
/// if they must be reported in milliseconds, divide at emission (the
/// `counter_set!` divisor).
#[derive(Clone, Copy, Debug)]
#[must_use]
pub(crate) struct Stopwatch(Instant);

impl Stopwatch {
    #[inline]
    pub(crate) fn start() -> Self {
        Self(Instant::now())
    }

    /// Nanoseconds since the start.
    #[inline]
    pub(crate) fn elapsed_ns(&self) -> u64 {
        Self::duration_ns(self.0.elapsed())
    }

    /// Whole milliseconds since the start (truncated, see the type docs).
    #[inline]
    pub(crate) fn elapsed_ms(&self) -> u64 {
        Self::duration_ms(self.0.elapsed())
    }

    #[inline]
    fn duration_ns(elapsed: Duration) -> u64 {
        u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
    }

    #[inline]
    fn duration_ms(elapsed: Duration) -> u64 {
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
    }

    /// Add the nanoseconds since the start to `into`.
    #[inline]
    pub(crate) fn record_ns(&self, into: &AtomicU64) {
        into.fetch_add(self.elapsed_ns(), Relaxed);
    }

    /// Add the whole milliseconds since the start to `into`.
    #[inline]
    pub(crate) fn record_ms(&self, into: &AtomicU64) {
        into.fetch_add(self.elapsed_ms(), Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Gated blocking operations for `*_wait_ns` stall counters
// ---------------------------------------------------------------------------
//
// `brokkr sidecar --stalls` reads every `*_wait_ns` counter as a strictly
// monotonic total of time some thread spent blocked on another (the maximum
// per name, the suffix stripped for the category). The convention is one
// atomic add per genuine blocking event, so these try the non-blocking form
// first and start a clock only when it fails: an operation that does not
// block costs exactly the attempt (one `try_lock`, `try_send` or `try_recv`,
// which is the same compare-exchange or slot claim the blocking form makes on
// its own fast path) and touches no clock and no counter. Only the slow path
// reads the clock twice and adds once.
//
// Several older `*_wait_ns` counters (the read pipeline's, the ordered pool's
// timers) time every call, blocked or not; they keep that for continuity.
// New stall counters go through these.

/// Lock `mutex`, adding the time spent blocked behind another holder to
/// `wait_ns`. An uncontended lock records nothing.
///
/// Poison-tolerant, like every caller it serves: a poisoned mutex yields its
/// guard, on the fast path and the slow one alike. Use it only where the
/// caller would otherwise write `lock().unwrap_or_else(PoisonError::into_inner)`.
#[inline]
pub(crate) fn lock_recording_wait<'a, T>(
    mutex: &'a Mutex<T>,
    wait_ns: &AtomicU64,
) -> MutexGuard<'a, T> {
    lock_recording_wait_observing(mutex, wait_ns, || {}, |watch| watch.elapsed_ns())
}

// Static callbacks expose slow-path entry and its elapsed sample to tests.
// Production uses a no-op observer and the same stopwatch read as before;
// neither callback is invoked on the fast path.
#[inline]
fn lock_recording_wait_observing<'a, T>(
    mutex: &'a Mutex<T>,
    wait_ns: &AtomicU64,
    on_slow_path: impl FnOnce(),
    elapsed_ns: impl FnOnce(Stopwatch) -> u64,
) -> MutexGuard<'a, T> {
    match mutex.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => {
            let blocked = Stopwatch::start();
            on_slow_path();
            let guard = mutex.lock().unwrap_or_else(PoisonError::into_inner);
            wait_ns.fetch_add(elapsed_ns(blocked), Relaxed);
            guard
        }
    }
}

/// `tx.send(item)`, adding the time spent blocked on a full channel to
/// `wait_ns`. A send that finds room, or finds the receiver gone, records
/// nothing; a blocked send that then fails because the receiver went away
/// records its wait.
#[inline]
pub(crate) fn send_recording_wait<T>(
    tx: &SyncSender<T>,
    item: T,
    wait_ns: &AtomicU64,
) -> Result<(), SendError<T>> {
    send_recording_wait_observing(tx, item, wait_ns, || {}, |watch| watch.elapsed_ns())
}

#[inline]
pub(crate) fn send_recording_wait_observing<T>(
    tx: &SyncSender<T>,
    item: T,
    wait_ns: &AtomicU64,
    on_slow_path: impl FnOnce(),
    elapsed_ns: impl FnOnce(Stopwatch) -> u64,
) -> Result<(), SendError<T>> {
    match tx.try_send(item) {
        Ok(()) => Ok(()),
        Err(TrySendError::Disconnected(item)) => Err(SendError(item)),
        Err(TrySendError::Full(item)) => {
            let blocked = Stopwatch::start();
            on_slow_path();
            let sent = tx.send(item);
            wait_ns.fetch_add(elapsed_ns(blocked), Relaxed);
            sent
        }
    }
}

/// `rx.recv()`, adding the time spent blocked on an empty channel to
/// `wait_ns`. A receive that finds an item, or finds every sender gone,
/// records nothing; a blocked receive that ends in the channel closing
/// records its wait (it was waiting for the last sender to finish).
#[inline]
pub(crate) fn recv_recording_wait<T>(
    rx: &Receiver<T>,
    wait_ns: &AtomicU64,
) -> Result<T, RecvError> {
    recv_recording_wait_observing(rx, wait_ns, || {}, |watch| watch.elapsed_ns())
}

#[inline]
pub(crate) fn recv_recording_wait_observing<T>(
    rx: &Receiver<T>,
    wait_ns: &AtomicU64,
    on_slow_path: impl FnOnce(),
    elapsed_ns: impl FnOnce(Stopwatch) -> u64,
) -> Result<T, RecvError> {
    match rx.try_recv() {
        Ok(item) => Ok(item),
        Err(TryRecvError::Disconnected) => Err(RecvError),
        Err(TryRecvError::Empty) => {
            let blocked = Stopwatch::start();
            on_slow_path();
            let received = rx.recv();
            wait_ns.fetch_add(elapsed_ns(blocked), Relaxed);
            received
        }
    }
}

/// Depth-gated stall span shared by concurrent blockers.
///
/// `brokkr sidecar --durations` pairs `WAIT_<CATEGORY>_START/_END` markers
/// by name, which breaks if N threads emit interleaved pairs for the
/// same category. (These spans do not feed `--stalls`, which reads
/// `*_wait_ns` counters; a blocking point meant to show there needs a
/// counter, and emitting both is fine.) The gauge collapses concurrent
/// blockers into one non-overlapping span per busy period: START when depth
/// goes 0 -> 1, END when it returns to 0. The mutex is held only across the
/// depth transition + marker emit, and callers must gate `track()` behind a
/// try_* fast path so unblocked operations never touch it.
pub(crate) struct StallGauge {
    start_marker: &'static str,
    end_marker: &'static str,
    depth: std::sync::Mutex<u64>,
    /// The invocation whose sink receives the span: a permit, since a
    /// guard can end the span from a worker's `Drop`.
    invocation: crate::host::Permit,
}

/// RAII exit for [`StallGauge::track`]; decrements depth on drop so
/// early returns and error paths cannot leak an open WAIT span.
pub(crate) struct StallGuard<'a>(&'a StallGauge);

impl StallGauge {
    pub(crate) fn new(
        start_marker: &'static str,
        end_marker: &'static str,
        invocation: &crate::host::Recorder,
    ) -> Self {
        Self {
            start_marker,
            end_marker,
            depth: std::sync::Mutex::new(0),
            invocation: invocation.permit(),
        }
    }

    /// Enter the stall; the returned guard exits it on drop.
    pub(crate) fn track(&self) -> StallGuard<'_> {
        let mut depth = self
            .depth
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *depth == 0 {
            self.invocation.marker(self.start_marker);
        }
        *depth += 1;
        StallGuard(self)
    }
}

impl Drop for StallGuard<'_> {
    fn drop(&mut self) {
        let mut depth = self
            .0
            .depth
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *depth = depth.saturating_sub(1);
        if *depth == 0 {
            self.0.invocation.marker(self.0.end_marker);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

    super::counter_set! {
        struct Probe {
            plain => "probe_plain",
            unnamed,
            scaled_ns => "probe_scaled_ms" / 1_000_000,
        }
        extra {
            latch: AtomicBool = AtomicBool::new(false),
        }
    }

    #[test]
    fn counter_set_visits_in_order_with_divisor() {
        let probe = Probe::new();
        probe.plain.fetch_add(7, Relaxed);
        probe.unnamed.fetch_add(5, Relaxed);
        probe.scaled_ns.fetch_add(3_999_999, Relaxed);
        let mut seen = Vec::new();
        probe.for_each(|name, value| seen.push((name, value)));
        assert_eq!(seen, [("probe_plain", 7), ("probe_scaled_ms", 3)]);
        assert_eq!(probe.unnamed.load(Relaxed), 5);
        assert!(!probe.latch.load(Relaxed));
    }

    #[test]
    fn record_max_only_raises() {
        let field = AtomicU64::new(5);
        super::record_max(&field, 3);
        assert_eq!(field.load(Relaxed), 5);
        super::record_max(&field, 9);
        assert_eq!(field.load(Relaxed), 9);
    }

    #[test]
    fn elapsed_conversion_is_exact_and_saturates() {
        use super::Stopwatch;
        use std::time::Duration;

        let sample = Duration::from_nanos(2_999_999);
        assert_eq!(Stopwatch::duration_ns(sample), 2_999_999);
        assert_eq!(Stopwatch::duration_ms(sample), 2);
        assert_eq!(Stopwatch::duration_ms(Duration::from_nanos(999_999)), 0);
        assert_eq!(Stopwatch::duration_ns(Duration::ZERO), 0);
        assert_eq!(Stopwatch::duration_ns(Duration::MAX), u64::MAX);
        assert_eq!(Stopwatch::duration_ms(Duration::MAX), u64::MAX);
    }

    /// An operation that does not block records nothing: not a clock read's
    /// worth, exactly zero. This is the property the stall counters' cost
    /// argument rests on.
    #[test]
    fn unblocked_operations_record_no_wait() {
        let wait = AtomicU64::new(0);

        let mutex = std::sync::Mutex::new(5_u32);
        assert_eq!(*super::lock_recording_wait(&mutex, &wait), 5);

        let (tx, rx) = std::sync::mpsc::sync_channel::<u32>(1);
        super::send_recording_wait(&tx, 7, &wait).expect("room in the channel");
        assert_eq!(super::recv_recording_wait(&rx, &wait), Ok(7));
        drop(tx);
        assert_eq!(
            super::recv_recording_wait(&rx, &wait),
            Err(std::sync::mpsc::RecvError),
            "a closed channel answers at once"
        );
        let (tx, rx) = std::sync::mpsc::sync_channel::<u32>(1);
        drop(rx);
        assert!(super::send_recording_wait(&tx, 1, &wait).is_err());

        assert_eq!(wait.load(Relaxed), 0);
    }

    /// The lock is poison-tolerant, as its callers are.
    #[test]
    fn a_poisoned_lock_still_yields_its_guard() {
        fn poison(mutex: &std::sync::Mutex<u32>) {
            let _guard = mutex.lock().expect("not yet poisoned");
            panic!("poison the mutex");
        }
        let mutex = std::sync::Mutex::new(3_u32);
        std::thread::scope(|scope| {
            let poisoner = scope.spawn(|| poison(&mutex));
            assert!(poisoner.join().is_err());
        });
        assert!(mutex.is_poisoned());
        let wait = AtomicU64::new(0);
        assert_eq!(*super::lock_recording_wait(&mutex, &wait), 3);
        assert_eq!(wait.load(Relaxed), 0);
    }

    // The observer runs only after try_* declines the fast path. Release
    // there, rather than guessing when a peer has reached a blocking call.
    // It is a no-op in production and is never called on the fast path.
    #[test]
    fn a_blocked_lock_records_its_wait() {
        let mutex = std::sync::Mutex::new(1_u32);
        let held = mutex.lock().expect("unpoisoned");
        let wait = AtomicU64::new(0);
        let mut observed = false;
        let guard = super::lock_recording_wait_observing(
            &mutex,
            &wait,
            || {
                observed = true;
                drop(held);
            },
            |_| 17,
        );
        assert_eq!(*guard, 1);
        assert!(observed);
        assert_eq!(wait.load(Relaxed), 17);
    }

    #[test]
    fn a_blocked_send_records_its_wait() {
        let wait = AtomicU64::new(0);
        let (tx, rx) = std::sync::mpsc::sync_channel::<u32>(1);
        tx.send(1).expect("room for the first item");
        let mut observed = false;
        super::send_recording_wait_observing(
            &tx,
            2,
            &wait,
            || {
                observed = true;
                assert_eq!(rx.recv(), Ok(1));
            },
            |_| 17,
        )
        .expect("the receiver drains");
        assert!(observed);
        assert_eq!(wait.load(Relaxed), 17);
        assert_eq!(rx.recv(), Ok(2));
    }

    #[test]
    fn a_blocked_recv_records_its_wait() {
        let wait = AtomicU64::new(0);
        let (tx, rx) = std::sync::mpsc::sync_channel::<u32>(1);
        let mut observed = false;
        assert_eq!(
            super::recv_recording_wait_observing(
                &rx,
                &wait,
                || {
                    observed = true;
                    tx.send(9).expect("the test is listening");
                },
                |_| 17,
            ),
            Ok(9)
        );
        assert!(observed);
        assert_eq!(wait.load(Relaxed), 17);
    }
}
