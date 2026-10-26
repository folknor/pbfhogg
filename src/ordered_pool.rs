//! The ordered worker pool: tasks run on worker threads in any order, and the
//! calling thread consumes their results strictly in task order, with the
//! results in flight bounded by an admission window.
//!
//! Every pool that hands results to its caller in schedule order runs here
//! (classify's ordered phase, renumber's rewrite stages, extract, tags-filter
//! pass 2, altw passthrough and stage 4, ...), or, where its work is not one
//! result per task (`commands::blob_merge`'s streamed overlap runs), shares
//! the admission gate. The read pipeline (`read::pipeline`) is the deliberate
//! exception: its permit rides with each decoded item and is released when
//! the item is delivered or dropped, which fits rayon tasks and filtered
//! items, and it is already a hard bound.
//!
//! # Contract
//!
//! - **Bounded.** Task `k` may only be worked once `k < consumed + window`,
//!   where `consumed` counts results the consumer has *finished* with. So at
//!   most `window` tasks are admitted and not yet consumed - queued, held in
//!   the reorder buffer, or in the consumer's hands - however slow one task
//!   is. The bound counts tasks, not bytes: a task whose result is large
//!   (multi-extract's per-region blocks) holds that much per slot.
//! - **Two limits, two jobs.** Results travel to the consumer through a
//!   channel of `min(window, POOL_RESULT_CHANNEL_DEPTH)` slots, the depth
//!   every hand-built pool before this engine used. When the consumer is the
//!   bottleneck, workers block sending into that full channel, each holding
//!   one finished result, so ordinary run-ahead stays near the channel depth
//!   plus one result per worker, and the wait shows where the old pools
//!   showed it (stage 4's `s4_send_ms`, the `WAIT_*_SEND` spans). The window
//!   is the hard bound behind a slow head: while the consumer waits for task
//!   `consumed` it keeps draining the channel into its reorder buffer, so
//!   later results pile up there, up to the window, and workers then park at
//!   the admission gate; that backlog stays alive until it is consumed, so
//!   the total bound, slow head or not, is the window. A channel as deep as
//!   the window would never fill,
//!   since no more than a window of results is ever in flight, which once
//!   left every send-side counter unable to see backpressure.
//! - **Ordered failures.** Worker results travel as `Result<T>` and surface in
//!   task order: the reported error is the lowest failing task's, or the
//!   consumer's own at its position, whichever comes first in task order.
//!   Claims are a monotone `fetch_add`, so when task `k` fails every task
//!   below it has been claimed, was admitted inside the window and stays
//!   admitted (`consumed` only grows), so each is worked and delivered: a
//!   worker failure stops new claims but never abandons a lower one.
//! - **Prompt exit.** Any return from the consumer side - success, an error,
//!   a cancellation, a panic - closes the gate and drops the result receiver
//!   before the scope joins: parked workers wake to a closed gate, workers in
//!   `send` fail it. Work already admitted and running is not interrupted
//!   (altw stage 4 may sit in its router's readiness wait); the pool does not
//!   promise to cut that short.
//! - **Cancellation** is checked on the calling thread only: before each wait
//!   for a result, on each arrival, before each consumed item, after the
//!   channel closes, and on an empty run. Workers never check it, so an
//!   admitted task is never abandoned while the consumer could still wait
//!   for it.
//! - **Panics** in a worker close the gate, so the others stop, and re-raise
//!   when the scope joins; a consumer panic does the same through its guard.
//!   After a worker panic the consumer learns of it only once every other
//!   worker has finished its admitted task and gone (the missing task then
//!   fails the run), so a worker parked on a wait outside the pool keeps the
//!   run alive until that wait ends.
//!
//! # Stall counters
//!
//! Every run records where it blocked into one process-wide set
//! ([`OrderedPoolMetrics`]): workers parked at the admission gate, workers
//! blocked sending into a full result channel, and the consumer blocked on
//! an empty one. These are `*_wait_ns` counters for `brokkr sidecar
//! --stalls`, separate from the per-site [`Waits`] timers a pool inherited.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use crate::error::Error;
use crate::tunables::{
    ORDERED_POOL_WINDOW_MIN, ORDERED_POOL_WINDOW_PER_WORKER, POOL_RESULT_CHANNEL_DEPTH,
};

crate::counters::counter_set! {
    /// Blocking time in every ordered pool run, for `brokkr sidecar
    /// --stalls`.
    ///
    /// Each named field is a `*_wait_ns` stall counter recorded only on a
    /// genuine block: an admission that is immediately possible, a send that
    /// finds room and a receive that finds a result touch no clock. Sums
    /// across threads, so a total can exceed wall time.
    ///
    /// One set for the engine, not one per command or phase, as with the
    /// unordered classify pools' set (`scan::classify`): the question these
    /// answer is about the mechanism, and per-caller names would need a name
    /// threaded through every site. The set is emitted as the process totals
    /// so far at the end of every run (success, failure or unwind), so each
    /// emission lands inside the caller's own phase markers where it has
    /// them. Every counter only grows, so the last emission is the process
    /// total, which is what `--stalls` reads. The step from the previous
    /// emission is one run's share only while runs are sequential: two
    /// concurrent runs (two library calls, or nested pools) add into the same
    /// totals.
    ///
    /// The unnamed counters are never emitted; they count how often each
    /// slow path was entered, which lets a test wait until a block has
    /// provably begun.
    pub(crate) struct OrderedPoolMetrics {
        /// A worker parked at the admission gate: its claimed task is a full
        /// window ahead of the consumer, which is behind a slow head (the
        /// reorder buffer holds the rest of the window).
        window_wait_ns => "ordered_pool_window_wait_ns",
        window_parks,
        /// A worker blocked sending its result into a full result channel:
        /// the consumer behind the workers. Recorded also when the blocked
        /// send ends because the consumer went away.
        result_send_wait_ns => "ordered_pool_result_send_wait_ns",
        result_send_blocks,
        /// The consumer blocked on an empty result channel: idle for want of
        /// a finished task (workers, or a wait they are parked on, behind the
        /// consumer).
        result_recv_wait_ns => "ordered_pool_result_recv_wait_ns",
        result_recv_blocks,
    }
}

/// Process totals of every ordered pool run (see [`OrderedPoolMetrics`]).
static ORDERED_POOL_METRICS: OrderedPoolMetrics = OrderedPoolMetrics::new();

/// Emits a metrics set when dropped: after the run's scope has joined its
/// workers, whether the run returned or unwound.
struct EmitOnDrop<'a>(&'a OrderedPoolMetrics);

impl Drop for EmitOnDrop<'_> {
    fn drop(&mut self) {
        self.0.emit_all();
    }
}

/// The default window for `workers` workers: `ORDERED_POOL_WINDOW_PER_WORKER`
/// each, at least `ORDERED_POOL_WINDOW_MIN`.
pub(crate) fn default_window(workers: usize) -> NonZeroUsize {
    let window = workers
        .saturating_mul(ORDERED_POOL_WINDOW_PER_WORKER)
        .max(ORDERED_POOL_WINDOW_MIN);
    NonZeroUsize::new(window).unwrap_or(NonZeroUsize::MIN)
}

/// The admission gate: how many results the consumer has finished with, and
/// whether claiming has stopped or the run is closed.
///
/// Every wait predicate lives under the one mutex and every transition
/// notifies all waiters, so a waiter cannot miss the change that would
/// release it. `stopped` mirrors "claims stopped or closed" for the
/// lock-free check a worker makes before claiming; it is only a fast exit,
/// never the wait predicate. A worker can pass that check and claim just
/// after another sets it; the claim still goes through [`OrderedGate::admit`],
/// which judges it under the lock, so the race is harmless.
pub(crate) struct OrderedGate {
    state: Mutex<GateState>,
    cv: Condvar,
    stopped: AtomicBool,
}

struct GateState {
    /// Tasks below this have been consumed.
    consumed: usize,
    /// A worker failed: no new claims, but an already-claimed task inside
    /// the window is still worked (eligibility wins), so every task below
    /// the failing one is delivered.
    stop_claims: bool,
    /// The consumer is gone (or a thread panicked): nothing more is
    /// admitted, inside the window or not.
    closed: bool,
}

impl OrderedGate {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(GateState {
                consumed: 0,
                stop_claims: false,
                closed: false,
            }),
            cv: Condvar::new(),
            stopped: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> MutexGuard<'_, GateState> {
        // Two counters and two flags, all still meaningful after a panic
        // elsewhere; the panic itself surfaces at the join.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Lock-free: claiming has stopped or the run is closed.
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// A task failed: stop new claims, keep admitting the claimed ones
    /// inside the window.
    pub(crate) fn stop_claims(&self) {
        self.stopped.store(true, Ordering::Release);
        self.lock().stop_claims = true;
        self.cv.notify_all();
    }

    /// The consumer is gone: admit nothing more.
    pub(crate) fn close(&self) {
        self.stopped.store(true, Ordering::Release);
        self.lock().closed = true;
        self.cv.notify_all();
    }

    /// The consumer has finished with every task below `consumed`.
    pub(crate) fn advance(&self, consumed: usize) {
        self.lock().consumed = consumed;
        self.cv.notify_all();
    }

    /// Block until task `position` is inside the window (`true`), or until
    /// it can never be worked (`false`): the run closed, or claims stopped
    /// while `position` was still outside the window.
    ///
    /// With `metrics`, a call that has to wait adds its whole wait, across
    /// every wakeup until it returns either way, to `window_wait_ns`; a
    /// call that can answer at once touches no clock.
    pub(crate) fn admit(
        &self,
        position: usize,
        window: NonZeroUsize,
        metrics: Option<&OrderedPoolMetrics>,
    ) -> bool {
        let mut state = self.lock();
        let mut parked: Option<crate::counters::Stopwatch> = None;
        let admitted = loop {
            if state.closed {
                break false;
            }
            if position < state.consumed.saturating_add(window.get()) {
                break true;
            }
            if state.stop_claims {
                break false;
            }
            if parked.is_none()
                && let Some(m) = metrics
            {
                m.window_parks.fetch_add(1, Ordering::Relaxed);
                parked = Some(crate::counters::Stopwatch::start());
            }
            state = self.cv.wait(state).unwrap_or_else(PoisonError::into_inner);
        };
        drop(state);
        if let (Some(parked), Some(m)) = (parked, metrics) {
            parked.record_ns(&m.window_wait_ns);
        }
        admitted
    }
}

/// Closes the gate when dropped. The consumer holds one for its whole loop,
/// installed before the first worker spawns, so every exit - a return, `?`,
/// a panic, a failed spawn - wakes workers parked in [`OrderedGate::admit`].
/// Dropping the result receiver alone only wakes workers parked in `send`.
pub(crate) struct CloseOnDrop<'a>(pub(crate) &'a OrderedGate);

impl Drop for CloseOnDrop<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Closes the gate if the holding thread unwinds, so the other workers stop
/// claiming and a dispatcher or consumer parked on the gate wakes. Workers
/// install it before running any caller code (their local init included).
pub(crate) struct CloseOnUnwind<'a>(pub(crate) &'a OrderedGate);

impl Drop for CloseOnUnwind<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.close();
        }
    }
}

/// A timed counter and the unit it accumulates in. Millisecond counters
/// truncate per sample, as `crate::counters::Stopwatch` documents, so a pool
/// that inherited a `_ms` counter keeps recording it the way it always was.
#[derive(Clone, Copy)]
pub(crate) enum Timer<'a> {
    Ns(&'a AtomicU64),
    Ms(&'a AtomicU64),
}

impl Timer<'_> {
    fn record(self, since: crate::counters::Stopwatch) {
        match self {
            Timer::Ns(c) => since.record_ns(c),
            Timer::Ms(c) => since.record_ms(c),
        }
    }
}

/// The per-site counters a pool kept from before this engine, under their
/// old names and units. Every field is optional; a pool with no counter set
/// of its own passes `Waits::default()`. A timer that is set times every
/// call, blocked or not, as it always did (one that is not set reads no
/// clock); the engine's own gated stall counters are
/// [`OrderedPoolMetrics`], recorded for every run whatever is passed here.
#[derive(Clone, Copy, Default)]
pub(crate) struct Waits<'a> {
    /// Workers parked because the consumer is a full window behind.
    pub(crate) window: Option<Timer<'a>>,
    /// The consumer idle for want of a finished task.
    pub(crate) recv: Option<Timer<'a>>,
    /// Workers handing a result to the consumer (blocked only when the
    /// result channel is full), timed around the whole send.
    pub(crate) send: Option<Timer<'a>>,
    /// Entered only when a send finds the result channel full (after a
    /// failed `try_send`), so unblocked sends never touch it.
    pub(crate) send_stall: Option<&'a crate::counters::StallGauge>,
    /// High-water of the result channel's depth as the workers count it: up
    /// before a send, down after the consumer's receive. Blocked senders
    /// count, so it can exceed the channel's capacity by up to one per
    /// worker; and a sender counts before it tries, so reaching a value
    /// does not prove the channel filled. The send stall counters are that
    /// evidence.
    pub(crate) queue_high_water: Option<&'a AtomicU64>,
}

/// The calling thread's side of an ordered run: one object, so the hooks can
/// all reach the same writer and state.
pub(crate) trait OrderedConsumer<T> {
    /// Before task `task`'s result is unwrapped. Runs even when that result is
    /// an error, so output that belongs before the task (a passthrough frame
    /// between two decoded blobs) is written - and can fail - in its true
    /// position.
    fn before_item(&mut self, task: usize) -> crate::Result<()> {
        let _ = task;
        Ok(())
    }

    /// Task `task`'s successful result.
    fn consume(&mut self, task: usize, item: T) -> crate::Result<()>;

    /// Advisory: the window's edge moved to `edge` (the first task not yet
    /// admissible; `consumed + window`, clamped to the task count). Called
    /// once before the workers start and then after each consumed result,
    /// before that result's credit is published, so read-ahead advice for
    /// the newly admissible tasks is issued before they can be claimed. It
    /// cannot control admission.
    fn on_advance(&mut self, edge: usize) {
        let _ = edge;
    }
}

/// Adapts a closure to [`OrderedConsumer`] for runs that need only `consume`.
pub(crate) struct ConsumeFn<F>(pub(crate) F);

impl<T, F: FnMut(usize, T) -> crate::Result<()>> OrderedConsumer<T> for ConsumeFn<F> {
    fn consume(&mut self, task: usize, item: T) -> crate::Result<()> {
        (self.0)(task, item)
    }
}

/// The output-schedule bookkeeping of a run whose workers decode only some
/// positions of an output schedule, the rest being passthrough frames the
/// consumer copies itself (extract, multi-extract's node pass, altw
/// passthrough and stage 4).
///
/// The pool runs over the decode items alone, numbered densely by decode
/// ordinal, so passthrough costs no claim, no channel message and no window
/// slot, and workers start decoding at once while the consumer copies a
/// passthrough prefix. The consumer walks the full schedule: in
/// [`OrderedConsumer::before_item`] for decode ordinal `j` it emits
/// [`Interleaved::before`]`(j)` - every passthrough position between the
/// previous decode item and this one - and after the pool succeeds it emits
/// [`Interleaved::rest`]. Output order, and the order failures surface in,
/// are therefore the full schedule's: a passthrough frame before a failed
/// decode is still written, and can still fail, first.
///
/// It borrows the decode positions, so the workers can index the same slice
/// to find their task's output position.
pub(crate) struct Interleaved<'a> {
    /// Output position of each decode item, strictly increasing.
    decode_at: &'a [usize],
    /// Length of the full output schedule.
    len: usize,
    /// The first output position not yet emitted.
    next: usize,
}

impl<'a> Interleaved<'a> {
    /// `decode_at` lists the output position of each decode item, in order;
    /// every other position below `len` is passthrough.
    pub(crate) fn new(decode_at: &'a [usize], len: usize) -> Self {
        debug_assert!(
            decode_at.windows(2).all(|w| w[0] < w[1]) && decode_at.last().is_none_or(|&p| p < len),
            "decode positions must be strictly increasing and inside the schedule"
        );
        Self {
            decode_at,
            len,
            next: 0,
        }
    }

    /// Number of decode items: the pool's task count.
    pub(crate) fn decode_count(&self) -> usize {
        self.decode_at.len()
    }

    /// The passthrough positions before decode item `ordinal` not yet
    /// emitted; afterwards the decode item itself counts as emitted. Called
    /// once per ordinal, in order.
    pub(crate) fn before(&mut self, ordinal: usize) -> std::ops::Range<usize> {
        let at = self.decode_at[ordinal];
        let range = self.next..at;
        self.next = at + 1;
        range
    }

    /// The trailing passthrough positions after the last decode item (the
    /// whole schedule when there is none).
    pub(crate) fn rest(&mut self) -> std::ops::Range<usize> {
        let range = self.next..self.len;
        self.next = self.len;
        range
    }
}

/// What every worker of one run shares: its result sender, the gate, the
/// claim counter and the reporting hooks.
struct WorkerShared<'a, T> {
    tx: std::sync::mpsc::SyncSender<(usize, crate::Result<T>)>,
    gate: &'a OrderedGate,
    next: &'a AtomicUsize,
    queue_depth: &'a AtomicUsize,
    n_tasks: usize,
    window: NonZeroUsize,
    waits: Waits<'a>,
    metrics: &'a OrderedPoolMetrics,
}

impl<T> WorkerShared<'_, T> {
    /// One worker's loop: claim, wait for admission, work, send, until the
    /// tasks run out, claiming stops or the consumer goes.
    fn run(&self, mut work: impl FnMut(usize) -> crate::Result<T>) {
        loop {
            if self.gate.is_stopped() {
                return;
            }
            let task = self.next.fetch_add(1, Ordering::Relaxed);
            if task >= self.n_tasks {
                return;
            }
            // A claimed task always reaches `admit`: returning between the
            // claim and the admission could strand a task below a failing one.
            let timed = self
                .waits
                .window
                .map(|timer| (timer, crate::counters::Stopwatch::start()));
            let admitted = self.gate.admit(task, self.window, Some(self.metrics));
            if let Some((timer, t)) = timed {
                timer.record(t);
            }
            if !admitted {
                return;
            }
            let result = work(task);
            if result.is_err() {
                self.gate.stop_claims();
            }
            if !self.send(task, result) {
                return;
            }
        }
    }

    /// Hand one result to the consumer; `false` when it is gone.
    fn send(&self, task: usize, result: crate::Result<T>) -> bool {
        let timed = self
            .waits
            .send
            .map(|timer| (timer, crate::counters::Stopwatch::start()));
        if let Some(high_water) = self.waits.queue_high_water {
            let depth = self.queue_depth.fetch_add(1, Ordering::Relaxed) + 1;
            crate::counters::record_max(high_water, depth as u64);
        }
        match self.tx.try_send((task, result)) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(item)) => {
                let _stall = self
                    .waits
                    .send_stall
                    .map(crate::counters::StallGauge::track);
                let blocked = crate::counters::Stopwatch::start();
                self.metrics
                    .result_send_blocks
                    .fetch_add(1, Ordering::Relaxed);
                let sent = self.tx.send(item);
                blocked.record_ns(&self.metrics.result_send_wait_ns);
                if sent.is_err() {
                    return false;
                }
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return false,
        }
        if let Some((timer, t)) = timed {
            timer.record(t);
        }
        true
    }
}

/// How a run is sized and observed.
#[derive(Clone, Copy)]
pub(crate) struct PoolConfig<'a> {
    pub(crate) window: NonZeroUsize,
    pub(crate) cancel: &'a crate::host::Cancel,
    pub(crate) waits: Waits<'a>,
}

/// Run `work` for every task in `0..n_tasks` on one worker thread per element
/// of `shards`, and hand each result to `consumer` on the calling thread in
/// task order. See the module docs for the contract.
///
/// `shards` is caller-owned per-worker state (`Send`, returned to the caller
/// when the run ends, so per-worker teardown such as flushing shard writers
/// happens after the join). `local` builds each worker's thread-local state
/// inside its thread, with no `Send` bound (a `BlockBuilder`, decode scratch,
/// a `DecompressPool`); it is called with the worker's index.
pub(crate) fn run_ordered<S, L, T, I, W, C>(
    n_tasks: usize,
    shards: &mut [S],
    config: PoolConfig<'_>,
    local: I,
    work: W,
    consumer: &mut C,
) -> crate::Result<()>
where
    S: Send,
    T: Send,
    I: Fn(usize) -> L + Sync,
    W: Fn(&mut S, &mut L, usize) -> crate::Result<T> + Sync,
    C: OrderedConsumer<T>,
{
    run_ordered_in(
        n_tasks,
        shards,
        config,
        local,
        work,
        consumer,
        &ORDERED_POOL_METRICS,
    )
}

/// [`run_ordered`] recording into `metrics`: production passes the process
/// set, a test its own.
fn run_ordered_in<S, L, T, I, W, C>(
    n_tasks: usize,
    shards: &mut [S],
    config: PoolConfig<'_>,
    local: I,
    work: W,
    consumer: &mut C,
    metrics: &OrderedPoolMetrics,
) -> crate::Result<()>
where
    S: Send,
    T: Send,
    I: Fn(usize) -> L + Sync,
    W: Fn(&mut S, &mut L, usize) -> crate::Result<T> + Sync,
    C: OrderedConsumer<T>,
{
    let PoolConfig { window, cancel, .. } = config;
    cancel.check()?;
    if n_tasks == 0 {
        return Ok(());
    }
    if shards.is_empty() {
        return Err(Error::internal("ordered pool started with no workers"));
    }
    consumer.on_advance(window.get().min(n_tasks));
    let gate = OrderedGate::new();
    let next = AtomicUsize::new(0);
    // Result-channel depth, kept only when its high-water is reported.
    let queue_depth = AtomicUsize::new(0);
    // Declared before the scope, so it drops after the scope has joined the
    // workers, on a return or an unwind alike.
    let _emit = EmitOnDrop(metrics);

    std::thread::scope(|scope| -> crate::Result<()> {
        // A local of this closure, so every return drops it before the scope
        // joins the workers. See the module docs ("Two limits, two jobs") for
        // why it is not as deep as the window.
        let depth = window.get().min(POOL_RESULT_CHANNEL_DEPTH);
        let (tx, rx) = std::sync::mpsc::sync_channel::<(usize, crate::Result<T>)>(depth);
        // Installed before the first spawn, so even a failed spawn closes the
        // gate for the workers already parked on it. Declared after `rx`, so
        // it drops first: the gate closes, then the receiver goes.
        let _close = CloseOnDrop(&gate);
        for (worker, shard) in shards.iter_mut().enumerate() {
            let shared = WorkerShared {
                tx: tx.clone(),
                gate: &gate,
                next: &next,
                queue_depth: &queue_depth,
                n_tasks,
                window,
                waits: config.waits,
                metrics,
            };
            let (work, local) = (&work, &local);
            scope.spawn(move || {
                let _close = CloseOnUnwind(shared.gate);
                let mut local = local(worker);
                shared.run(|task| work(shard, &mut local, task));
            });
        }
        drop(tx);

        deliver(&rx, &gate, &queue_depth, n_tasks, config, metrics, consumer)
    })
}

/// The calling thread's side of a run: receive results, put them in task
/// order and hand them to `consumer`, returning credit to the gate after
/// each. Ends when every worker has gone, or at the first error in task
/// order.
fn deliver<T, C: OrderedConsumer<T>>(
    rx: &std::sync::mpsc::Receiver<(usize, crate::Result<T>)>,
    gate: &OrderedGate,
    queue_depth: &AtomicUsize,
    n_tasks: usize,
    config: PoolConfig<'_>,
    metrics: &OrderedPoolMetrics,
    consumer: &mut C,
) -> crate::Result<()> {
    let PoolConfig {
        window,
        cancel,
        waits,
    } = config;
    let mut reorder =
        crate::reorder_buffer::ReorderBuffer::with_max_window(window.get(), window.get());
    // The gate's count of consumed tasks. Kept apart from the reorder
    // buffer's `next_seq`, which moves when an item is popped, before it is
    // consumed.
    let mut consumed = 0_usize;
    loop {
        cancel.check()?;
        let timed = waits
            .recv
            .map(|timer| (timer, crate::counters::Stopwatch::start()));
        let msg = recv_counted(rx, metrics);
        if let Some((timer, t)) = timed {
            timer.record(t);
        }
        let Ok((task, result)) = msg else {
            break;
        };
        if waits.queue_high_water.is_some() {
            queue_depth.fetch_sub(1, Ordering::Relaxed);
        }
        cancel.check()?;
        // `push` panics on a stale, duplicate or out-of-window position.
        // None can reach it: every task is claimed once from the counter,
        // and only worked once `task < consumed + window`, where `consumed`
        // never passes the buffer's `next_seq`.
        reorder.push(task, result);
        while let Some(result) = reorder.pop_ready() {
            cancel.check()?;
            consumer.before_item(consumed)?;
            consumer.consume(consumed, result?)?;
            consumed += 1;
            consumer.on_advance(consumed.saturating_add(window.get()).min(n_tasks));
            gate.advance(consumed);
        }
    }
    cancel.check()?;
    if consumed != n_tasks {
        // Only a worker that died without sending gets here; its panic takes
        // precedence when the scope joins.
        return Err(Error::internal(format!(
            "ordered pool: a worker exited without delivering task {consumed} of {n_tasks}"
        )));
    }
    Ok(())
}

/// `rx.recv()`, adding a wait on an empty channel to the run's receive
/// stall counter. A receive that finds a result, or finds every worker gone,
/// touches no clock.
fn recv_counted<M>(
    rx: &std::sync::mpsc::Receiver<M>,
    metrics: &OrderedPoolMetrics,
) -> Result<M, std::sync::mpsc::RecvError> {
    match rx.try_recv() {
        Ok(msg) => Ok(msg),
        Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(std::sync::mpsc::RecvError),
        Err(std::sync::mpsc::TryRecvError::Empty) => {
            metrics.result_recv_blocks.fetch_add(1, Ordering::Relaxed);
            let blocked = crate::counters::Stopwatch::start();
            let received = rx.recv();
            blocked.record_ns(&metrics.result_recv_wait_ns);
            received
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Run `f` on its own thread and fail the test if it has not returned
    /// within a few seconds. The defect these tests pin is a hang, and a
    /// hang should fail as one rather than eat the harness watchdog.
    fn within_deadline<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // The receiver is gone only if the deadline already failed.
            tx.send(f()).ok();
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the pool hung instead of returning")
    }

    fn run_with(
        n: usize,
        workers: usize,
        cancel: &crate::host::Cancel,
        work: impl Fn(usize) -> crate::Result<usize> + Sync,
        consumer: &mut impl OrderedConsumer<usize>,
    ) -> crate::Result<()> {
        let mut shards = vec![(); workers];
        run_ordered(
            n,
            &mut shards,
            PoolConfig {
                window: default_window(workers),
                cancel,
                waits: Waits::default(),
            },
            |_| (),
            |(), (), task| work(task),
            consumer,
        )
    }

    fn run(
        n: usize,
        workers: usize,
        work: impl Fn(usize) -> crate::Result<usize> + Sync,
        consume: impl FnMut(usize) -> crate::Result<()>,
    ) -> crate::Result<()> {
        let mut consume = consume;
        run_with(
            n,
            workers,
            &crate::host::Cancel::new(),
            work,
            &mut ConsumeFn(|_, v| consume(v)),
        )
    }

    #[test]
    fn delivers_every_result_in_task_order() {
        let got = within_deadline(|| {
            let mut got = Vec::new();
            run(5_000, 4, Ok, |v| {
                got.push(v);
                Ok(())
            })
            .unwrap();
            got
        });
        assert_eq!(got, (0..5_000).collect::<Vec<_>>());
    }

    #[test]
    fn zero_tasks_is_ok() {
        within_deadline(|| run(0, 4, Ok, |_| Ok(())).unwrap());
    }

    #[test]
    fn zero_tasks_still_checks_cancellation() {
        let cancel = crate::host::Cancel::new();
        cancel.cancel();
        let err = run_with(0, 4, &cancel, Ok, &mut ConsumeFn(|_, _| Ok(()))).unwrap_err();
        assert!(matches!(err.kind(), crate::ErrorKind::Cancelled));
    }

    /// The hang shape: a failure with far more than a channel's worth of
    /// work left. The old hand-built pools parked every worker in `send`.
    #[test]
    fn a_worker_error_mid_stream_ends_the_run_with_that_error() {
        let err = within_deadline(|| {
            run(
                20_000,
                4,
                |i| {
                    if i == 700 {
                        Err(Error::internal(format!("bad blob {i}")))
                    } else {
                        Ok(i)
                    }
                },
                |_| Ok(()),
            )
            .unwrap_err()
        });
        assert_eq!(err.to_string(), "bad blob 700");
    }

    /// Several failures: the reported one is the lowest task, every time,
    /// even when a later one finishes first.
    #[test]
    fn the_lowest_failing_task_is_the_one_reported() {
        for _ in 0..20 {
            let err = within_deadline(|| {
                run(
                    5_000,
                    6,
                    |i| {
                        if i == 300 {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        if i == 300 || i == 301 || i == 2_000 {
                            Err(Error::internal(format!("bad blob {i}")))
                        } else {
                            Ok(i)
                        }
                    },
                    |_| Ok(()),
                )
                .unwrap_err()
            });
            assert_eq!(err.to_string(), "bad blob 300");
        }
    }

    /// The consumer's own failure ends the run just as promptly.
    #[test]
    fn a_consumer_error_ends_the_run() {
        let err = within_deadline(|| {
            run(20_000, 4, Ok, |v| {
                if v == 50 {
                    Err(Error::internal("disk full"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err()
        });
        assert_eq!(err.to_string(), "disk full");
    }

    #[test]
    fn an_error_in_the_last_task_is_reported() {
        let err = within_deadline(|| {
            run(
                1_000,
                4,
                |i| {
                    if i == 999 {
                        Err(Error::internal("last"))
                    } else {
                        Ok(i)
                    }
                },
                |_| Ok(()),
            )
            .unwrap_err()
        });
        assert_eq!(err.to_string(), "last");
    }

    /// `before_item` runs for a task whose result is an error, ahead of the
    /// error, so a failure belonging before that task wins.
    #[test]
    fn before_item_runs_ahead_of_a_failed_result() {
        struct Consumer;
        impl OrderedConsumer<usize> for Consumer {
            fn before_item(&mut self, task: usize) -> crate::Result<()> {
                if task == 10 {
                    return Err(Error::internal("passthrough before 10 failed"));
                }
                Ok(())
            }
            fn consume(&mut self, _: usize, _: usize) -> crate::Result<()> {
                Ok(())
            }
        }
        let err = within_deadline(|| {
            run_with(
                100,
                4,
                &crate::host::Cancel::new(),
                |i| {
                    if i == 10 {
                        Err(Error::internal("decode 10 failed"))
                    } else {
                        Ok(i)
                    }
                },
                &mut Consumer,
            )
            .unwrap_err()
        });
        assert_eq!(err.to_string(), "passthrough before 10 failed");
    }

    /// One slow task while the others race ahead: the window bounds how far
    /// they get. Credit returns only after `consume`, so nothing is ever
    /// further ahead of the consumed count than the window.
    #[test]
    fn a_slow_task_does_not_let_the_others_run_unbounded() {
        let peak = within_deadline(|| {
            let peak = AtomicUsize::new(0);
            let consumed = AtomicUsize::new(0);
            run(
                3_000,
                4,
                |i| {
                    if i == 0 {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    let ahead = i.saturating_sub(consumed.load(Ordering::SeqCst));
                    peak.fetch_max(ahead, Ordering::SeqCst);
                    Ok(i)
                },
                |_| {
                    consumed.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .unwrap();
            peak.load(Ordering::SeqCst)
        });
        let window = default_window(4).get();
        assert!(
            peak < window,
            "a worker ran {peak} tasks ahead of the consumer; the window ({window}) must bound it"
        );
    }

    /// The edge hook sees the initial edge once, then every advance, and
    /// never passes the task count.
    #[test]
    fn on_advance_tracks_the_window_edge() {
        struct Consumer(Vec<usize>);
        impl OrderedConsumer<usize> for Consumer {
            fn consume(&mut self, _: usize, _: usize) -> crate::Result<()> {
                Ok(())
            }
            fn on_advance(&mut self, edge: usize) {
                self.0.push(edge);
            }
        }
        let edges = within_deadline(|| {
            let mut consumer = Consumer(Vec::new());
            run_with(40, 1, &crate::host::Cancel::new(), Ok, &mut consumer).unwrap();
            consumer.0
        });
        let window = default_window(1).get();
        let expected: Vec<usize> = (0..=40).map(|c| (c + window).min(40)).collect();
        assert_eq!(edges, expected);
    }

    /// A worker panic must not hang the pool: the others stop claiming and
    /// the panic resurfaces at join.
    #[test]
    fn a_worker_panic_surfaces_instead_of_hanging() {
        let outcome = within_deadline(|| {
            std::panic::catch_unwind(|| {
                run(
                    20_000,
                    4,
                    |i| {
                        assert!(i != 100, "injected worker panic");
                        Ok(i)
                    },
                    |_| Ok(()),
                )
            })
            .is_err()
        });
        assert!(outcome, "a worker panic must propagate, not be swallowed");
    }

    /// Caller-owned shards come back with every worker's contribution.
    #[test]
    fn shards_are_returned_to_the_caller() {
        let mut shards = vec![0_usize; 3];
        run_ordered(
            1_000,
            &mut shards,
            PoolConfig {
                window: default_window(3),
                cancel: &crate::host::Cancel::new(),
                waits: Waits::default(),
            },
            |_| (),
            |shard: &mut usize, (), task| {
                *shard += 1;
                Ok(task)
            },
            &mut ConsumeFn(|_, _| Ok(())),
        )
        .unwrap();
        assert_eq!(shards.iter().sum::<usize>(), 1_000);
    }

    // ---- the result channel and the stall counters ------------------------
    //
    // These run with a window wider than `POOL_RESULT_CHANNEL_DEPTH`, so the
    // result channel is shallower than the window and can fill, and record
    // into a set of their own. Each waits for the slow path it is about to
    // test to have been entered (the set's unnamed counters), never for a
    // sleep to have probably been long enough.

    /// Poll `cond` for up to the deadline; whether it came true.
    fn eventually(cond: impl Fn() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !cond() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        true
    }

    fn run_in(
        n: usize,
        workers: usize,
        window: usize,
        metrics: &OrderedPoolMetrics,
        work: impl Fn(usize) -> crate::Result<usize> + Sync,
        consume: impl FnMut(usize) -> crate::Result<()>,
    ) -> crate::Result<()> {
        let mut consume = consume;
        let mut shards = vec![(); workers];
        run_ordered_in(
            n,
            &mut shards,
            PoolConfig {
                window: NonZeroUsize::new(window).unwrap(),
                cancel: &crate::host::Cancel::new(),
                waits: Waits::default(),
            },
            |_| (),
            |(), (), task| work(task),
            &mut ConsumeFn(|_, v| consume(v)),
            metrics,
        )
    }

    const WIDE_WINDOW: usize = 4 * POOL_RESULT_CHANNEL_DEPTH;
    const WIDE_TASKS: usize = WIDE_WINDOW + 50;

    /// A consumer stuck on its first result while the workers race on: the
    /// channel fills, a worker blocks in `send`, and once the consumer moves
    /// on every result still arrives, in order. A channel as deep as the
    /// window could never fill here.
    #[test]
    fn a_full_result_channel_blocks_senders_and_still_delivers_in_order() {
        let (got, saw_block, send_wait) = within_deadline(|| {
            let metrics = OrderedPoolMetrics::new();
            let mut got = Vec::new();
            let mut saw_block = false;
            run_in(WIDE_TASKS, 4, WIDE_WINDOW, &metrics, Ok, |v| {
                if v == 0 {
                    saw_block =
                        eventually(|| metrics.result_send_blocks.load(Ordering::Relaxed) > 0);
                }
                got.push(v);
                Ok(())
            })
            .unwrap();
            (
                got,
                saw_block,
                metrics.result_send_wait_ns.load(Ordering::Relaxed),
            )
        });
        assert!(saw_block, "no worker blocked on the full result channel");
        assert!(send_wait > 0, "the blocked send recorded no wait");
        assert_eq!(got, (0..WIDE_TASKS).collect::<Vec<_>>());
    }

    /// The consumer fails while workers are blocked sending: the run ends
    /// with its error, the blocked senders are released by the dropped
    /// receiver, and a send that ended that way still records its wait.
    #[test]
    fn a_consumer_error_releases_blocked_senders() {
        let (err, send_wait) = within_deadline(|| {
            let metrics = OrderedPoolMetrics::new();
            let err = run_in(WIDE_TASKS, 4, WIDE_WINDOW, &metrics, Ok, |v| {
                assert_eq!(v, 0, "the consumer fails on its first result");
                assert!(eventually(|| metrics
                    .result_send_blocks
                    .load(Ordering::Relaxed)
                    > 0));
                Err(Error::internal("disk full"))
            })
            .unwrap_err();
            (err, metrics.result_send_wait_ns.load(Ordering::Relaxed))
        });
        assert_eq!(err.to_string(), "disk full");
        assert!(send_wait > 0, "the released send recorded no wait");
    }

    /// The same with a consumer panic: it surfaces, it does not hang. The
    /// handshake's outcome is kept apart from the panic, so a failed
    /// handshake cannot pass as the injected panic.
    #[test]
    fn a_consumer_panic_with_blocked_senders_surfaces() {
        let (panicked, saw_block) = within_deadline(|| {
            let metrics = OrderedPoolMetrics::new();
            let saw_block = AtomicBool::new(false);
            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_in(WIDE_TASKS, 4, WIDE_WINDOW, &metrics, Ok, |_| {
                    saw_block.store(
                        eventually(|| metrics.result_send_blocks.load(Ordering::Relaxed) > 0),
                        Ordering::SeqCst,
                    );
                    panic!("injected consumer panic");
                })
            }))
            .is_err();
            (panicked, saw_block.load(Ordering::SeqCst))
        });
        assert!(saw_block, "no worker blocked on the full result channel");
        assert!(
            panicked,
            "a consumer panic must propagate, not be swallowed"
        );
    }

    /// A later task fails first, the lowest failing task fails afterwards,
    /// and the channel filled on the way: the reported error is still the
    /// lowest task's.
    #[test]
    fn the_lowest_failure_wins_past_a_full_channel() {
        let (err, ordered) = within_deadline(|| {
            let metrics = OrderedPoolMetrics::new();
            let later_failed = AtomicBool::new(false);
            let ordered = AtomicBool::new(false);
            let err = run_in(
                WIDE_TASKS,
                4,
                WIDE_WINDOW,
                &metrics,
                |i| match i {
                    5 => {
                        ordered.store(
                            eventually(|| later_failed.load(Ordering::SeqCst)),
                            Ordering::SeqCst,
                        );
                        Err(Error::internal("bad 5"))
                    }
                    50 => {
                        later_failed.store(true, Ordering::SeqCst);
                        Err(Error::internal("bad 50"))
                    }
                    _ => Ok(i),
                },
                |v| {
                    if v == 0 {
                        assert!(eventually(|| metrics
                            .result_send_blocks
                            .load(Ordering::Relaxed)
                            > 0));
                    }
                    Ok(())
                },
            )
            .unwrap_err();
            (err, ordered.load(Ordering::SeqCst))
        });
        assert!(ordered, "task 50 never failed ahead of task 5");
        assert_eq!(err.to_string(), "bad 5");
    }

    /// A slow head: the others run a full window ahead and park at the gate,
    /// which records the wait; the run then completes in order.
    #[test]
    fn a_slow_head_parks_workers_at_the_gate() {
        let (got, parked, window_wait) = within_deadline(|| {
            let metrics = OrderedPoolMetrics::new();
            let parked = AtomicBool::new(false);
            let mut got = Vec::new();
            run_in(
                50,
                2,
                8,
                &metrics,
                |i| {
                    if i == 0 {
                        parked.store(
                            eventually(|| metrics.window_parks.load(Ordering::Relaxed) > 0),
                            Ordering::SeqCst,
                        );
                    }
                    Ok(i)
                },
                |v| {
                    got.push(v);
                    Ok(())
                },
            )
            .unwrap();
            (
                got,
                parked.load(Ordering::SeqCst),
                metrics.window_wait_ns.load(Ordering::Relaxed),
            )
        });
        assert!(parked, "no worker parked at the gate behind the slow head");
        assert!(window_wait > 0, "the parked admission recorded no wait");
        assert_eq!(got, (0..50).collect::<Vec<_>>());
    }

    /// A consumer waiting on a slow worker records its receive wait; a run
    /// whose one task is admitted at once and sent into an empty channel
    /// records no window or send wait at all.
    #[test]
    fn an_idle_consumer_records_its_wait_and_nothing_else_blocks() {
        let (waited, recv_wait, window_wait, send_wait) = within_deadline(|| {
            let metrics = OrderedPoolMetrics::new();
            let waited = AtomicBool::new(false);
            run_in(
                1,
                1,
                WIDE_WINDOW,
                &metrics,
                |i| {
                    waited.store(
                        eventually(|| metrics.result_recv_blocks.load(Ordering::Relaxed) > 0),
                        Ordering::SeqCst,
                    );
                    Ok(i)
                },
                |_| Ok(()),
            )
            .unwrap();
            (
                waited.load(Ordering::SeqCst),
                metrics.result_recv_wait_ns.load(Ordering::Relaxed),
                metrics.window_wait_ns.load(Ordering::Relaxed),
                metrics.result_send_wait_ns.load(Ordering::Relaxed),
            )
        });
        assert!(waited, "the consumer never waited for the slow worker");
        assert!(recv_wait > 0, "the consumer's wait was not recorded");
        assert_eq!(window_wait, 0);
        assert_eq!(send_wait, 0);
    }

    /// The emitted names are the ones `--stalls` reads; the slow-path
    /// tallies are not emitted.
    #[test]
    fn the_stall_counters_are_emitted_under_their_names() {
        let mut names = Vec::new();
        OrderedPoolMetrics::new().for_each(|name, _| names.push(name));
        assert_eq!(
            names,
            [
                "ordered_pool_window_wait_ns",
                "ordered_pool_result_send_wait_ns",
                "ordered_pool_result_recv_wait_ns",
            ]
        );
    }
}
