//! The ordered worker pool: tasks run on worker threads in any order, and the
//! calling thread consumes their results strictly in task order, with the
//! results in flight bounded by an admission window.
//!
//! Every pool that hands results to its caller in schedule order runs here
//! (classify's ordered phase, renumber's rewrite stages, extract, tags-filter
//! pass 2, altw passthrough and stage 4, ...), or, where its work is not one
//! result per task but a stream of them (`commands::blob_merge`'s overlap
//! runs), on [`run_ordered_streams`] (see "Streamed runs" below). The read
//! pipeline (`read::pipeline`) is the deliberate
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
//!   `send` fail it. Work already admitted and running is not interrupted;
//!   the pool cannot see a wait outside it (altw stage 4's router readiness
//!   wait). A failing run calls [`OrderedConsumer::abandon`] before it joins,
//!   so a consumer whose workers can park on such a wait can release it.
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
//!   run alive until that wait ends, unless the worker that panicked
//!   releases the wait during its own unwind (altw stage 4 guards each
//!   worker body to abort its router). The pool does not turn a task panic
//!   into an ordered failure: `abandon` would then still wait for every
//!   lower task, a lower one parked on that same wait included.
//!
//! # Stall counters
//!
//! Every run records where it blocked into its invocation's set
//! ([`OrderedPoolMetrics`]): workers parked at the admission gate, workers
//! blocked sending into a full result channel, and the consumer blocked on
//! an empty one. These are `*_wait_ns` counters for `brokkr sidecar
//! --stalls`, separate from the per-site [`Waits`] timers a pool inherited.
//!
//! # Streamed runs
//!
//! [`run_ordered_streams`] runs tasks that each emit any number of items
//! followed by one terminal, and hands the consumer every item of task `k`
//! before any of task `k + 1`, streaming: a task's items reach the consumer
//! while it is still running. Each task has its own channel of `depth`
//! messages, so a task the consumer is not reading yet can only run `depth`
//! messages ahead; a shared channel would let later tasks fill it while the
//! consumer waits on the head. The admission gate does not fit: a worker
//! would claim a task whose channel the consumer must already hold, and
//! publishing per-task senders through a second shared table needs its own
//! closure protocol. So the consumer dispatches instead:
//!
//! - Tasks are a count known up front ([`run_ordered_streams`]) or come from
//!   a source called on the consumer's thread as tasks are issued, each with
//!   its own payload ([`run_ordered_streams_from`], the diff's work units); a
//!   source failure is an ordered failure at the position of the task it did
//!   not produce.
//! - It issues task `k` by making its channel, keeping the receiver (in task
//!   order) and queueing `(k, sender)` on a FIFO job queue. It issues the
//!   first `window` tasks up front and task `k + window` once it has
//!   finished with task `k`, and drops the queue's sender after the last
//!   issue. At most `window` tasks are issued and unfinished, so the queue
//!   (capacity `window`) never refuses an issue, and at most `window x
//!   depth` messages are buffered, plus one blocked send per worker.
//! - Workers take jobs FIFO, run the task, send its items and then exactly
//!   one terminal (`Ok(end)` or the task's error).
//! - Progress: job `k` is queued before every later job and taken first, so
//!   while the consumer waits on task `k` its worker is running it or the
//!   next free worker will take it, and no worker waits on any task but its
//!   own.
//! - A worker that dies holding task `k`'s sender drops it, which the
//!   consumer sees as a disconnect without a terminal; if every worker
//!   dies, the job queue's receiver goes and the queued senders with it.
//! - Failures surface in task order: `before_task(k)` first, then an item
//!   write error, then task `k`'s terminal, with cancellation checked before
//!   and after every receive and a worker panic re-raised at the join. A
//!   failing task does not stop the issue of later ones (there is no claim
//!   to stop): until the consumer reaches it, up to a window of later tasks
//!   may start, speculatively.
//! - Any return or unwind of the consumer side drops every receiver and the
//!   queue's sender before the join, which fails blocked sends and closes
//!   the queue, and sets an abandonment flag workers check before taking a
//!   job and that [`Emitter::abandoned`] exposes to long tasks. The flag
//!   wakes nobody; the dropped channel ends do. A worker unwinding sets it
//!   too.
//!
//! A streamed run records the result send and receive waits above, and two
//! counters of its own for the job queue (`ordered_pool_job_lock_wait_ns`,
//! `ordered_pool_job_recv_wait_ns`); it never records `window_wait_ns`,
//! since no worker there holds a task outside the window.

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
    /// threaded through every site. One set per invocation
    /// (`crate::counters::InvocationMetrics`), emitted as the invocation's
    /// totals so far at the end of every run (success, failure or unwind), so
    /// each emission lands inside the caller's own phase markers where it has
    /// them. Every counter only grows, so the last emission is the
    /// invocation's total, which is what `--stalls` reads. The step from the
    /// previous emission is one run's share only while the invocation's runs
    /// are sequential: concurrent runs of one invocation (nested pools) add
    /// into the same totals; two invocations never do.
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
        /// send ends because the consumer went away. In a streamed run the
        /// channel is the task's own and the message an item or the task's
        /// terminal.
        result_send_wait_ns => "ordered_pool_result_send_wait_ns",
        result_send_blocks,
        /// The consumer blocked on an empty result channel: idle for want of
        /// a finished task (workers, or a wait they are parked on, behind the
        /// consumer). In a streamed run, idle on the head task's own channel
        /// for its next item or its terminal (the head task's worker behind
        /// the consumer).
        result_recv_wait_ns => "ordered_pool_result_recv_wait_ns",
        result_recv_blocks,
        /// Streamed runs only ([`run_ordered_streams`]): a worker queued on
        /// the job receiver's mutex behind a peer holding it. The holder
        /// keeps the lock across its receive, so while the queue is empty
        /// every other idle worker queues here: this is roughly
        /// `ordered_pool_job_recv_wait_ns` times the idle peers, plus the
        /// mutex's own cost, which is all of it only while the receive wait
        /// stays near zero. The two together are worker idle time for want
        /// of an issued task.
        job_lock_wait_ns => "ordered_pool_job_lock_wait_ns",
        job_lock_blocks,
        /// Streamed runs only: a worker holding the job receiver blocked on
        /// an empty job queue, for want of an issued task (the window full
        /// behind the consumer, or the consumer still issuing the first
        /// window). Not an admission wait: these workers hold no task.
        job_recv_wait_ns => "ordered_pool_job_recv_wait_ns",
        job_recv_blocks,
    }
}

/// Emits a metrics set when dropped: after the run's scope has joined its
/// workers, whether the run returned or unwound.
struct EmitOnDrop<'a>(&'a OrderedPoolMetrics, &'a crate::host::Recorder);

impl Drop for EmitOnDrop<'_> {
    fn drop(&mut self) {
        self.0.emit_all(self.1);
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
struct OrderedGate {
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
    fn new() -> Self {
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
    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// A task failed: stop new claims, keep admitting the claimed ones
    /// inside the window.
    fn stop_claims(&self) {
        self.stopped.store(true, Ordering::Release);
        self.lock().stop_claims = true;
        self.cv.notify_all();
    }

    /// The consumer is gone: admit nothing more.
    fn close(&self) {
        self.stopped.store(true, Ordering::Release);
        self.lock().closed = true;
        self.cv.notify_all();
    }

    /// The consumer has finished with every task below `consumed`.
    fn advance(&self, consumed: usize) {
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
    fn admit(
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
struct CloseOnDrop<'a>(&'a OrderedGate);

impl Drop for CloseOnDrop<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Closes the gate if the holding thread unwinds, so the other workers stop
/// claiming and the ones parked on the gate wake. Workers install it before
/// running any caller code (their local init included).
struct CloseOnUnwind<'a>(&'a OrderedGate);

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

    /// The run is failing with `error` (a worker's, surfacing in task order;
    /// the consumer's own; a cancellation; a worker that never delivered).
    /// Called on the calling thread once that error leaves the delivery
    /// loop, before the gate closes, the receiver drops and the workers are
    /// joined, so a consumer whose workers can park on a wait outside the
    /// pool (altw stage 4's router readiness) can release it. Every task
    /// below a worker's failing one has been consumed by then, so releasing
    /// the wait cannot fail an earlier task.
    ///
    /// Not called on success, on a panic (a consumer that needs the release
    /// then guards its own callbacks against a consumer panic, and its
    /// worker bodies against a worker panic, as altw stage 4 does), or for
    /// a failure before
    /// the workers start (an empty pool, a cancellation already tripped).
    /// It must neither wait for the workers nor panic: it runs ahead of
    /// their join, and a panic would replace `error`.
    fn abandon(&mut self, error: &Error) {
        let _ = error;
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
    pub(crate) cancel: &'a crate::host::Recorder,
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
        &config.cancel.metrics().ordered,
    )
}

/// [`run_ordered`] recording into `metrics`: production passes the
/// invocation's set, a test its own.
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
    let _emit = EmitOnDrop(metrics, cancel);

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

        let delivered = deliver(&rx, &gate, &queue_depth, n_tasks, config, metrics, consumer);
        // Still before `_close` and `rx` drop and the scope joins: a consumer
        // can release workers parked on a wait the pool cannot see.
        if let Err(error) = &delivered {
            consumer.abandon(error);
        }
        delivered
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

// ---------------------------------------------------------------------------
// Streamed runs
// ---------------------------------------------------------------------------

/// A message on one streamed task's channel: items, then exactly one
/// terminal. A channel that closes without the terminal is a worker that
/// died mid-task, never a shorter task.
enum StreamMsg<T, E> {
    Item(T),
    End(crate::Result<E>),
}

/// The calling thread's side of a [`run_ordered_streams`] run. Called in
/// task order: `before_task(k)`, every item of task `k`, `finish_task(k)`.
pub(crate) trait StreamConsumer<T, E> {
    /// Before task `task`'s first item, even when that task failed, so output
    /// that belongs before it is written (and can fail) in its true position.
    fn before_task(&mut self, task: usize) -> crate::Result<()> {
        let _ = task;
        Ok(())
    }

    /// One item of task `task`, in the order the task emitted it.
    fn item(&mut self, task: usize, item: T) -> crate::Result<()>;

    /// Task `task` ended successfully with `end`.
    fn finish_task(&mut self, task: usize, end: E) -> crate::Result<()>;
}

/// How a streamed run is sized.
#[derive(Clone, Copy)]
pub(crate) struct StreamConfig<'a> {
    /// Tasks issued and not yet finished with, at most.
    pub(crate) window: NonZeroUsize,
    /// Messages one task's channel holds (its terminal included) before its
    /// worker blocks.
    pub(crate) depth: usize,
    pub(crate) cancel: &'a crate::host::Recorder,
}

/// A streamed task's way out: its items go to the consumer through it.
/// Used on the task's own worker thread only.
pub(crate) struct Emitter<'a, T, E> {
    tx: std::sync::mpsc::SyncSender<StreamMsg<T, E>>,
    abandoned: &'a AtomicBool,
    metrics: &'a OrderedPoolMetrics,
    /// A send failed: the consumer is gone. Latched, so a task that ignores
    /// the error still ends its worker.
    gone: std::cell::Cell<bool>,
}

impl<T, E> Emitter<'_, T, E> {
    /// Hand one item to the consumer, blocking while the task's channel is
    /// full. Fails once the consumer is gone.
    pub(crate) fn send(&self, item: T) -> crate::Result<()> {
        if self.gone.get() || !self.deliver(StreamMsg::Item(item)) {
            return Err(Error::internal(
                "ordered pool: the consumer of a streamed task is gone",
            ));
        }
        Ok(())
    }

    /// The run is being abandoned (the consumer has returned or a thread
    /// unwound), or a send already failed: nothing this task produces from
    /// here can be delivered, so a long task may stop early.
    pub(crate) fn abandoned(&self) -> bool {
        self.gone.get() || self.abandoned.load(Ordering::Acquire)
    }

    /// Send `msg`, recording a blocked send; `false` (and latched) when the
    /// consumer is gone.
    fn deliver(&self, msg: StreamMsg<T, E>) -> bool {
        let sent = match self.tx.try_send(msg) {
            Ok(()) => true,
            Err(std::sync::mpsc::TrySendError::Full(msg)) => {
                self.metrics
                    .result_send_blocks
                    .fetch_add(1, Ordering::Relaxed);
                let blocked = crate::counters::Stopwatch::start();
                let sent = self.tx.send(msg).is_ok();
                blocked.record_ns(&self.metrics.result_send_wait_ns);
                sent
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
        };
        if !sent {
            self.gone.set(true);
        }
        sent
    }
}

/// Sets the abandonment flag when dropped: the consumer side's guard, so
/// every return and unwind of it raises the flag.
struct AbandonOnDrop<'a>(&'a AtomicBool);

impl Drop for AbandonOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Sets the abandonment flag if the holding worker unwinds.
struct AbandonOnUnwind<'a>(&'a AtomicBool);

impl Drop for AbandonOnUnwind<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.store(true, Ordering::Release);
        }
    }
}

/// A worker's next job, or `Err` once the queue is closed and empty. Waiting
/// for the lock behind a peer and on an empty queue are recorded apart, each
/// only when it blocks (and tallied, so a test can wait for it to begin).
/// The mutex guards nothing a panic could leave half updated, so a poisoned
/// one is used as is.
fn next_job<J>(
    jobs: &std::sync::Mutex<std::sync::mpsc::Receiver<J>>,
    metrics: &OrderedPoolMetrics,
) -> Result<J, std::sync::mpsc::RecvError> {
    let guard = match jobs.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            metrics.job_lock_blocks.fetch_add(1, Ordering::Relaxed);
            let blocked = crate::counters::Stopwatch::start();
            let guard = jobs.lock().unwrap_or_else(PoisonError::into_inner);
            blocked.record_ns(&metrics.job_lock_wait_ns);
            guard
        }
    };
    match guard.try_recv() {
        Ok(job) => Ok(job),
        Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(std::sync::mpsc::RecvError),
        Err(std::sync::mpsc::TryRecvError::Empty) => {
            metrics.job_recv_blocks.fetch_add(1, Ordering::Relaxed);
            let blocked = crate::counters::Stopwatch::start();
            let job = guard.recv();
            blocked.record_ns(&metrics.job_recv_wait_ns);
            job
        }
    }
}

/// Run `work` for every task in `0..n_tasks` on `workers` worker threads,
/// each task emitting items through its [`Emitter`] and ending with
/// `Ok(end)` or an error, and hand every item to `consumer` on the calling
/// thread in task order, task by task. See the module docs ("Streamed runs")
/// for the protocol, the memory bound and the failure order.
///
/// `local` builds each worker's state on its own thread, with its index.
pub(crate) fn run_ordered_streams<L, T, E, I, W, C>(
    n_tasks: usize,
    workers: usize,
    config: StreamConfig<'_>,
    local: I,
    work: W,
    consumer: &mut C,
) -> crate::Result<()>
where
    T: Send,
    E: Send,
    I: Fn(usize) -> L + Sync,
    W: Fn(&mut L, usize, &Emitter<'_, T, E>) -> crate::Result<E> + Sync,
    C: StreamConsumer<T, E>,
{
    run_ordered_streams_in(
        n_tasks,
        workers,
        config,
        local,
        work,
        consumer,
        &config.cancel.metrics().ordered,
    )
}

/// [`run_ordered_streams`] recording into `metrics`: production passes the
/// invocation's set, a test its own.
fn run_ordered_streams_in<L, T, E, I, W, C>(
    n_tasks: usize,
    workers: usize,
    config: StreamConfig<'_>,
    local: I,
    work: W,
    consumer: &mut C,
    metrics: &OrderedPoolMetrics,
) -> crate::Result<()>
where
    T: Send,
    E: Send,
    I: Fn(usize) -> L + Sync,
    W: Fn(&mut L, usize, &Emitter<'_, T, E>) -> crate::Result<E> + Sync,
    C: StreamConsumer<T, E>,
{
    let mut issued = 0_usize;
    let mut source = || {
        if issued == n_tasks {
            return Ok(None);
        }
        issued += 1;
        Ok(Some(()))
    };
    run_streams_core(
        &mut source,
        workers,
        config,
        local,
        |local: &mut L, task, (), emitter: &Emitter<'_, T, E>| work(local, task, emitter),
        consumer,
        metrics,
    )
}

/// [`run_ordered_streams`] over tasks a source produces as the run goes,
/// each carrying its own payload, instead of a count known up front.
///
/// `source` runs on the calling thread whenever a task can be issued: up to
/// `window` up front, then one each time the consumer finishes a task. It
/// answers `Ok(Some(payload))` for the next task, `Ok(None)` once there are
/// no more, or an error, which becomes a failure at the position of the task
/// it did not produce: every earlier task is still consumed first, so an
/// earlier task's or the consumer's own failure outranks it, as in the
/// module's ordered-failure contract. After `None` or an error the source is
/// not called again. A source that blocks (on a planner's queue) delays the
/// consumer, never a worker: a worker holds its whole payload.
pub(crate) fn run_ordered_streams_from<P, L, T, E, I, W, C>(
    source: &mut dyn FnMut() -> crate::Result<Option<P>>,
    workers: usize,
    config: StreamConfig<'_>,
    local: I,
    work: W,
    consumer: &mut C,
) -> crate::Result<()>
where
    P: Send,
    T: Send,
    E: Send,
    I: Fn(usize) -> L + Sync,
    W: Fn(&mut L, usize, P, &Emitter<'_, T, E>) -> crate::Result<E> + Sync,
    C: StreamConsumer<T, E>,
{
    run_streams_core(
        source,
        workers,
        config,
        local,
        work,
        consumer,
        &config.cancel.metrics().ordered,
    )
}

/// A queued streamed task: its index, its payload and the sender of its
/// channel.
type PayloadJob<P, T, E> = (usize, P, std::sync::mpsc::SyncSender<StreamMsg<T, E>>);

/// Where issuing stands.
enum Issue {
    /// The source may still produce tasks.
    Open,
    /// The source answered `None`.
    Done,
    /// The source failed at the task it would have issued next.
    Failed(Error),
}

/// The one streamed-run engine behind [`run_ordered_streams`] and
/// [`run_ordered_streams_from`].
#[allow(clippy::too_many_lines)]
fn run_streams_core<P, L, T, E, I, W, C>(
    source: &mut dyn FnMut() -> crate::Result<Option<P>>,
    workers: usize,
    config: StreamConfig<'_>,
    local: I,
    work: W,
    consumer: &mut C,
    metrics: &OrderedPoolMetrics,
) -> crate::Result<()>
where
    P: Send,
    T: Send,
    E: Send,
    I: Fn(usize) -> L + Sync,
    W: Fn(&mut L, usize, P, &Emitter<'_, T, E>) -> crate::Result<E> + Sync,
    C: StreamConsumer<T, E>,
{
    let StreamConfig {
        window,
        depth,
        cancel,
    } = config;
    cancel.check()?;
    // The first task is pulled before any thread starts, so an empty run
    // (and a source failing at once) starts nothing.
    let first = match source() {
        Ok(Some(payload)) => payload,
        Ok(None) => return Ok(()),
        Err(e) => return Err(e),
    };
    if workers == 0 {
        return Err(Error::internal("ordered pool started with no workers"));
    }
    let abandoned = AtomicBool::new(false);
    // Declared before the scope, so it drops after the scope has joined the
    // workers, on a return or an unwind alike.
    let _emit = EmitOnDrop(metrics, cancel);

    std::thread::scope(|scope| -> crate::Result<()> {
        // Every channel end below is a local of this closure, so any return
        // drops it before the scope joins the workers.
        let (job_tx, job_rx) = std::sync::mpsc::sync_channel::<PayloadJob<P, T, E>>(window.get());
        let job_rx = std::sync::Arc::new(std::sync::Mutex::new(job_rx));
        let mut job_tx = Some(job_tx);
        let mut pending: std::collections::VecDeque<std::sync::mpsc::Receiver<StreamMsg<T, E>>> =
            std::collections::VecDeque::with_capacity(window.get());
        let mut next_to_issue = 0_usize;
        let mut state = Issue::Open;
        issue_job(
            job_tx.as_ref(),
            first,
            depth,
            &mut pending,
            &mut next_to_issue,
        );
        refill(
            source,
            job_tx.as_ref(),
            window.get(),
            depth,
            &mut pending,
            &mut next_to_issue,
            &mut state,
        );
        // On `None` or a source error the job queue closes, so idle workers
        // exit once it drains.
        if !matches!(state, Issue::Open) {
            job_tx = None;
        }
        // Declared after every channel end, so it drops first: the flag is
        // up before the receivers and the queue's sender go.
        let _abandon = AbandonOnDrop(&abandoned);

        for worker in 0..workers {
            let jobs = std::sync::Arc::clone(&job_rx);
            let (work, local, abandoned) = (&work, &local, &abandoned);
            scope.spawn(move || {
                // Before any caller code, and dropped after the local state.
                let _unwind = AbandonOnUnwind(abandoned);
                let mut local = local(worker);
                loop {
                    if abandoned.load(Ordering::Acquire) {
                        break;
                    }
                    let job = next_job(&jobs, metrics);
                    let Ok((task, payload, tx)) = job else { break };
                    let emitter = Emitter {
                        tx,
                        abandoned,
                        metrics,
                        gone: std::cell::Cell::new(false),
                    };
                    let end = work(&mut local, task, payload, &emitter);
                    if emitter.gone.get() || !emitter.deliver(StreamMsg::End(end)) {
                        break;
                    }
                }
            });
        }
        // Workers hold the only job receivers now, so their exit drops any
        // task sender still queued.
        drop(job_rx);

        let mut task = 0_usize;
        while let Some(rx) = pending.front() {
            cancel.check()?;
            consumer.before_task(task)?;
            loop {
                cancel.check()?;
                let msg = recv_counted(rx, metrics);
                cancel.check()?;
                match msg {
                    Ok(StreamMsg::Item(item)) => consumer.item(task, item)?,
                    Ok(StreamMsg::End(Ok(end))) => {
                        consumer.finish_task(task, end)?;
                        break;
                    }
                    Ok(StreamMsg::End(Err(e))) => return Err(e),
                    Err(_) => {
                        // Only a worker that died holding the task gets
                        // here; its panic takes precedence at the join.
                        return Err(Error::internal(format!(
                            "ordered pool: a worker ended streamed task {task} without \
                             finishing it"
                        )));
                    }
                }
            }
            pending.pop_front();
            task += 1;
            if matches!(state, Issue::Open) {
                refill(
                    source,
                    job_tx.as_ref(),
                    window.get(),
                    depth,
                    &mut pending,
                    &mut next_to_issue,
                    &mut state,
                );
                if !matches!(state, Issue::Open) {
                    job_tx = None;
                }
            }
        }
        cancel.check()?;
        match state {
            Issue::Failed(e) => Err(e),
            Issue::Open | Issue::Done => Ok(()),
        }
    })
}

/// Issue one task: make its channel, keep the receiver in task order and
/// queue the job.
fn issue_job<P, T, E>(
    job_tx: Option<&std::sync::mpsc::SyncSender<PayloadJob<P, T, E>>>,
    payload: P,
    depth: usize,
    pending: &mut std::collections::VecDeque<std::sync::mpsc::Receiver<StreamMsg<T, E>>>,
    next_to_issue: &mut usize,
) {
    let (tx, rx) = std::sync::mpsc::sync_channel(depth);
    pending.push_back(rx);
    if let Some(job_tx) = job_tx {
        // Never full (see the module docs). Refused only once every worker
        // is gone, which drops this task's sender with the job, so the
        // consumer sees it disconnect.
        job_tx.send((*next_to_issue, payload, tx)).ok();
    }
    *next_to_issue += 1;
}

/// Pull tasks from `source` while the window has room and the source is
/// open.
#[allow(clippy::too_many_arguments)]
fn refill<P, T, E>(
    source: &mut dyn FnMut() -> crate::Result<Option<P>>,
    job_tx: Option<&std::sync::mpsc::SyncSender<PayloadJob<P, T, E>>>,
    window: usize,
    depth: usize,
    pending: &mut std::collections::VecDeque<std::sync::mpsc::Receiver<StreamMsg<T, E>>>,
    next_to_issue: &mut usize,
    state: &mut Issue,
) {
    while matches!(state, Issue::Open) && pending.len() < window {
        match source() {
            Ok(Some(payload)) => issue_job(job_tx, payload, depth, pending, next_to_issue),
            Ok(None) => *state = Issue::Done,
            Err(e) => *state = Issue::Failed(e),
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
            // Carry a panic over as a value so it is re-raised with its own
            // payload rather than read as a hang. The receiver is gone only
            // if the deadline already failed.
            tx.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)))
                .ok();
        });
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(value)) => value,
            Ok(Err(payload)) => std::panic::resume_unwind(payload),
            Err(_) => panic!("the pool hung: no return within the deadline"),
        }
    }

    fn run_with(
        n: usize,
        workers: usize,
        cancel: &crate::host::Recorder,
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
            &crate::host::Call::detached(),
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
        let cancel = crate::host::Invocation::new();
        cancel.cancel();
        let call = cancel.call().unwrap();
        let err = run_with(0, 4, &call, Ok, &mut ConsumeFn(|_, _| Ok(()))).unwrap_err();
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
                &crate::host::Call::detached(),
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
            run_with(40, 1, &crate::host::Call::detached(), Ok, &mut consumer).unwrap();
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
                cancel: &crate::host::Call::detached(),
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
                cancel: &crate::host::Call::detached(),
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
                "ordered_pool_job_lock_wait_ns",
                "ordered_pool_job_recv_wait_ns",
            ]
        );
    }

    // ---- streamed runs ----------------------------------------------------

    /// Streamed consumer recording everything it sees, failing where told.
    #[derive(Default)]
    struct Recorder {
        items: Vec<(usize, usize)>,
        ends: Vec<(usize, usize)>,
        before: Vec<usize>,
        fail_before: Option<usize>,
        fail_item: Option<(usize, usize)>,
    }

    impl StreamConsumer<usize, usize> for Recorder {
        fn before_task(&mut self, task: usize) -> crate::Result<()> {
            if self.fail_before == Some(task) {
                return Err(Error::internal(format!("before {task} failed")));
            }
            self.before.push(task);
            Ok(())
        }
        fn item(&mut self, task: usize, item: usize) -> crate::Result<()> {
            if self.fail_item == Some((task, item)) {
                return Err(Error::internal(format!("write {task}/{item} failed")));
            }
            self.items.push((task, item));
            Ok(())
        }
        fn finish_task(&mut self, task: usize, end: usize) -> crate::Result<()> {
            self.ends.push((task, end));
            Ok(())
        }
    }

    fn streams_in(
        n: usize,
        workers: usize,
        window: usize,
        depth: usize,
        metrics: &OrderedPoolMetrics,
        work: impl Fn(usize, &Emitter<'_, usize, usize>) -> crate::Result<usize> + Sync,
        consumer: &mut Recorder,
    ) -> crate::Result<()> {
        run_ordered_streams_in(
            n,
            workers,
            StreamConfig {
                window: NonZeroUsize::new(window).unwrap(),
                depth,
                cancel: &crate::host::Call::detached(),
            },
            |_| (),
            |(), task, emitter| work(task, emitter),
            consumer,
            metrics,
        )
    }

    /// Task `task` emits `task % 7` items then ends with its item count.
    fn emit_some(task: usize, emitter: &Emitter<'_, usize, usize>) -> crate::Result<usize> {
        for i in 0..task % 7 {
            emitter.send(i)?;
        }
        Ok(task % 7)
    }

    /// Every item of every task, in task order then emission order, with
    /// more tasks than the window and items past the channel depth.
    #[test]
    fn streams_deliver_every_item_in_task_order() {
        let consumer = within_deadline(|| {
            let mut consumer = Recorder::default();
            streams_in(
                500,
                4,
                8,
                2,
                &OrderedPoolMetrics::new(),
                emit_some,
                &mut consumer,
            )
            .unwrap();
            consumer
        });
        let expected: Vec<(usize, usize)> = (0..500)
            .flat_map(|t| (0..t % 7).map(move |i| (t, i)))
            .collect();
        assert_eq!(consumer.items, expected);
        assert_eq!(
            consumer.ends,
            (0..500).map(|t| (t, t % 7)).collect::<Vec<_>>()
        );
        assert_eq!(consumer.before, (0..500).collect::<Vec<_>>());
    }

    #[test]
    fn streams_with_zero_tasks_start_nothing() {
        let mut consumer = Recorder::default();
        streams_in(
            0,
            0,
            1,
            1,
            &OrderedPoolMetrics::new(),
            emit_some,
            &mut consumer,
        )
        .unwrap();
        assert!(consumer.before.is_empty());
    }

    /// A later task failing first does not win: the reported error is the
    /// lowest failing task's, after every item of the tasks before it.
    #[test]
    fn a_streamed_failure_surfaces_in_task_order() {
        let (err, consumer) = within_deadline(|| {
            let mut consumer = Recorder::default();
            let err = streams_in(
                200,
                4,
                8,
                2,
                &OrderedPoolMetrics::new(),
                |task, emitter| {
                    if task == 20 {
                        std::thread::sleep(Duration::from_millis(20));
                        return Err(Error::internal("task 20 failed"));
                    }
                    if task == 23 {
                        return Err(Error::internal("task 23 failed"));
                    }
                    emit_some(task, emitter)
                },
                &mut consumer,
            )
            .unwrap_err();
            (err, consumer)
        });
        assert_eq!(err.to_string(), "task 20 failed");
        assert_eq!(consumer.ends.len(), 20, "every task below 20 finished");
        assert_eq!(consumer.before.last(), Some(&20));
    }

    /// `before_task` runs ahead of a failed task, and an item's write error
    /// ahead of its task's own failure.
    #[test]
    fn streamed_consumer_failures_take_their_position() {
        let err = within_deadline(|| {
            let mut consumer = Recorder {
                fail_before: Some(5),
                ..Recorder::default()
            };
            streams_in(
                50,
                3,
                6,
                1,
                &OrderedPoolMetrics::new(),
                |task, emitter| {
                    if task == 5 {
                        return Err(Error::internal("task 5 failed"));
                    }
                    emit_some(task, emitter)
                },
                &mut consumer,
            )
            .unwrap_err()
        });
        assert_eq!(err.to_string(), "before 5 failed");

        let err = within_deadline(|| {
            let mut consumer = Recorder {
                fail_item: Some((6, 2)),
                ..Recorder::default()
            };
            streams_in(
                50,
                3,
                6,
                1,
                &OrderedPoolMetrics::new(),
                |task, emitter| {
                    emit_some(task, emitter)?;
                    if task == 6 {
                        return Err(Error::internal("task 6 failed"));
                    }
                    Ok(0)
                },
                &mut consumer,
            )
            .unwrap_err()
        });
        assert_eq!(err.to_string(), "write 6/2 failed");
    }

    /// The consumer failing while workers are blocked on full channels ends
    /// the run promptly, and a long task sees the run abandoned.
    #[test]
    fn a_streamed_consumer_error_releases_and_abandons_workers() {
        let (err, send_wait, saw_abandoned) = within_deadline(|| {
            let metrics = OrderedPoolMetrics::new();
            let saw_abandoned = AtomicBool::new(false);
            let mut consumer = Recorder {
                fail_item: Some((0, 0)),
                ..Recorder::default()
            };
            let err = streams_in(
                40,
                4,
                8,
                1,
                &metrics,
                |task, emitter| {
                    if task == 0 {
                        // Hold the consumer's first item back until another
                        // task has blocked on its full channel.
                        assert!(eventually(|| metrics
                            .result_send_blocks
                            .load(Ordering::Relaxed)
                            > 0));
                        emitter.send(0)?;
                        return Ok(1);
                    }
                    if task == 1 {
                        // A long task: it stops once the run is abandoned.
                        assert!(eventually(|| emitter.abandoned()));
                        saw_abandoned.store(true, Ordering::SeqCst);
                        return Ok(0);
                    }
                    for i in 0..10 {
                        emitter.send(i)?;
                    }
                    Ok(10)
                },
                &mut consumer,
            )
            .unwrap_err();
            (
                err,
                metrics.result_send_wait_ns.load(Ordering::Relaxed),
                saw_abandoned.load(Ordering::SeqCst),
            )
        });
        assert_eq!(err.to_string(), "write 0/0 failed");
        assert!(send_wait > 0, "the blocked send recorded no wait");
        assert!(saw_abandoned, "the long task never saw the run abandoned");
    }

    /// A worker panic surfaces at the join rather than hanging the consumer
    /// on the task the dead worker held.
    #[test]
    fn a_streamed_worker_panic_surfaces_instead_of_hanging() {
        let panicked = within_deadline(|| {
            std::panic::catch_unwind(|| {
                let mut consumer = Recorder::default();
                streams_in(
                    300,
                    4,
                    8,
                    2,
                    &OrderedPoolMetrics::new(),
                    |task, emitter| {
                        assert!(task != 30, "injected worker panic");
                        emit_some(task, emitter)
                    },
                    &mut consumer,
                )
            })
            .is_err()
        });
        assert!(panicked, "a worker panic must propagate, not be swallowed");
    }

    /// Every worker panicking: the queued tasks' senders go with the job
    /// queue, so the consumer is not left waiting on one.
    #[test]
    fn every_streamed_worker_panicking_does_not_hang() {
        let panicked = within_deadline(|| {
            std::panic::catch_unwind(|| {
                let mut consumer = Recorder::default();
                streams_in(
                    100,
                    2,
                    8,
                    2,
                    &OrderedPoolMetrics::new(),
                    |_, _| panic!("every worker panics"),
                    &mut consumer,
                )
            })
            .is_err()
        });
        assert!(panicked);
    }

    /// A consumer idle on the head task records its receive wait; with a
    /// window of one, the idle workers block on the empty job queue (one
    /// holding the lock) and behind it on the lock, each recorded apart.
    /// Task 0 holds until all three blocks have begun.
    #[test]
    fn streamed_waits_are_recorded_where_they_block() {
        let (blocked, recv_wait, job_recv_wait, job_lock_wait, window_wait) =
            within_deadline(|| {
                let metrics = OrderedPoolMetrics::new();
                let blocked = AtomicBool::new(false);
                let mut consumer = Recorder::default();
                streams_in(
                    3,
                    4,
                    1,
                    2,
                    &metrics,
                    |task, emitter| {
                        if task == 0 {
                            blocked.store(
                                eventually(|| {
                                    metrics.result_recv_blocks.load(Ordering::Relaxed) > 0
                                        && metrics.job_recv_blocks.load(Ordering::Relaxed) > 0
                                        && metrics.job_lock_blocks.load(Ordering::Relaxed) > 0
                                }),
                                Ordering::SeqCst,
                            );
                        }
                        emit_some(task, emitter)
                    },
                    &mut consumer,
                )
                .unwrap();
                (
                    blocked.load(Ordering::SeqCst),
                    metrics.result_recv_wait_ns.load(Ordering::Relaxed),
                    metrics.job_recv_wait_ns.load(Ordering::Relaxed),
                    metrics.job_lock_wait_ns.load(Ordering::Relaxed),
                    metrics.window_wait_ns.load(Ordering::Relaxed),
                )
            });
        assert!(
            blocked,
            "the consumer and the idle workers never all blocked"
        );
        assert!(
            recv_wait > 0,
            "the consumer's wait on the head was not recorded"
        );
        assert!(
            job_recv_wait > 0,
            "the worker on the empty job queue recorded no wait"
        );
        assert!(
            job_lock_wait > 0,
            "the workers behind the job lock recorded no wait"
        );
        assert_eq!(
            window_wait, 0,
            "a streamed run never records an admission wait"
        );
    }
}
