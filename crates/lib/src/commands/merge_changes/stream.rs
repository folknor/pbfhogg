//! The default (non-simplify) merge: readers, encoding workers and one
//! ordered coordinator, with output memory bounded by a byte budget rather
//! than by the size of the output.
//!
//! [`collect`] runs the same readers and workers with a caller's encoding
//! and keeps every result instead of writing it (`--simplify` and the OSC
//! tags-filter); its readers reuse a slot as soon as their reader finishes,
//! and credit returns when a result is accepted.
//!
//! # Shape
//!
//! - **Readers.** One standard thread per input, at most `readers` alive,
//!   started strictly in input order (so the head input's reader is always
//!   running or finished). A slot is reused only once its input is complete
//!   (every member written), which also bounds open spill files by the cap. Each drives its document through the one OSC
//!   driver with a [`LexSink`]: framing and every structural check happen
//!   there, but no attribute is interpreted. At an element end past
//!   [`MERGE_CHANGES_BATCH_BYTES`] the reader seals a batch: it takes credit
//!   for the batch's bytes from the [`Ledger`] (waiting if the budget is
//!   spent), then hands its encode job, tagged `(input, seq)`, to the
//!   [`Encoder`], which runs it on the executor within the plan's lanes.
//!   Credit is taken before the sequence number becomes outstanding, and an
//!   input's batches are sealed in order.
//! - **Workers.** Replay a batch into the byte sink, re-emit XML and gzip
//!   it into one self-contained member with its own action sections. They
//!   never block.
//! - **Coordinator.** The only owner of every sink. It writes each member
//!   the moment it is next in order for its own input: the head input's to
//!   the output, every other input's to that input's spill file in the run
//!   scratch, and returns the member's credit then. When the head input has
//!   reported its batch count and every one of its members is written, the
//!   next input's spill file is copied into the output and that input
//!   continues straight to the output.
//!
//! # Why this is live
//!
//! Each input's lowest outstanding sequence number already holds credit and
//! its job runs on a pool that never blocks; the coordinator drains any
//! next-in-order member to some sink without waiting on another input, so
//! credit always comes back. A batch larger than the whole budget is
//! admitted alone once nothing else holds credit.
//!
//! # Memory
//!
//! Admitted batches hold at most [`MERGE_CHANGES_INFLIGHT_BYTES`] of recorded
//! tag bytes; resident memory is a bounded multiple of that (the batch, its
//! descriptors and the encoded member it becomes, which re-emission can make
//! larger than its input), plus per reader one batch under construction, one
//! element, the XML reader's buffer and inflate state. It does not grow with
//! the output. The output that is produced ahead of the head input lives in
//! scratch files, not in memory.
//!
//! # Failure
//!
//! The first failure the coordinator receives is returned, ahead of a
//! cancellation observed after it. Before any scope joins, a guard stops
//! the ledger: that wakes every reader waiting for credit, makes every
//! reader refuse at its next recorded callback (inside a huge element too),
//! and makes queued encode jobs answer without encoding. A stretch of input
//! that produces no callbacks at all is still read to its end, without
//! recording anything. Readers and
//! workers catch panics and answer with an error, so no admitted position
//! goes unanswered. Nothing is published unless the whole merge succeeded.
//!
//! [`MERGE_CHANGES_BATCH_BYTES`]: crate::tunables::MERGE_CHANGES_BATCH_BYTES
//! [`MERGE_CHANGES_INFLIGHT_BYTES`]: crate::tunables::MERGE_CHANGES_INFLIGHT_BYTES

use std::cell::Cell;
use std::collections::BTreeMap;
use std::io::{self, Read, Seek, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Condvar, Mutex, PoisonError};

use crate::osc::xml_parse::{LexBatch, LexSink, parse_osc_path};
use crate::scratch::{RunScratch, ScratchPath};

/// How readers cut batches and how many batch bytes may be in flight. The
/// one exception to the rule that engines read tunables directly: this
/// policy belongs to the streaming mechanism, production callers always
/// pass [`Batching::PRODUCTION`], and only the mechanism's own tests pass
/// boundary configurations. Batch bytes reach the output (each batch is its
/// own member with its own action sections, so they move member and
/// section framing, never the changes or their order); the in-flight
/// budget never does.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Batching {
    /// Raw element bytes at which a reader seals a batch.
    pub(crate) batch_bytes: usize,
    /// Admitted, not yet written batch bytes over every input ([`Ledger`]).
    pub(crate) inflight_bytes: usize,
}

impl Batching {
    pub(crate) const PRODUCTION: Self = Self {
        batch_bytes: crate::tunables::MERGE_CHANGES_BATCH_BYTES,
        inflight_bytes: crate::tunables::MERGE_CHANGES_INFLIGHT_BYTES,
    };
}

/// What one streaming merge needs.
pub(crate) struct Plan<'a> {
    pub(crate) batching: Batching,
    pub(crate) inputs: &'a [&'a Path],
    pub(crate) is_gz: bool,
    /// Upper bound on concurrently alive reader threads.
    pub(crate) readers: usize,
    /// Upper bound on this call's encode jobs running at once on the
    /// executor ([`Lanes`]); `usize::MAX` leaves them unbounded.
    pub(crate) lanes: usize,
    pub(crate) executor: crate::host::ExecutorRef<'a>,
    pub(crate) cancel: &'a crate::host::Recorder,
}

enum Msg<T> {
    Member {
        input: usize,
        seq: usize,
        credit: usize,
        result: crate::Result<T>,
    },
    ReaderDone {
        input: usize,
        batches: usize,
    },
    ReaderFailed(crate::Error),
}

/// The byte budget of admitted, not yet written batches.
struct Ledger {
    state: Mutex<LedgerState>,
    room: Condvar,
    budget: usize,
    wait_ns: AtomicU64,
    /// Set with `stopped`, readable without the lock: readers ask it at
    /// every recorded callback and workers before they encode.
    halted: AtomicBool,
}

struct LedgerState {
    used: usize,
    stopped: bool,
}

impl Ledger {
    fn new(budget: usize) -> Self {
        Self {
            state: Mutex::new(LedgerState {
                used: 0,
                stopped: false,
            }),
            room: Condvar::new(),
            budget,
            wait_ns: AtomicU64::new(0),
            halted: AtomicBool::new(false),
        }
    }

    fn halted(&self) -> bool {
        self.halted.load(Ordering::Relaxed)
    }

    /// Take `n` bytes of credit, waiting for room. Refuses once stopped or
    /// cancelled. A request larger than the budget is granted when nothing
    /// else holds credit.
    fn acquire(&self, n: usize, cancel: &crate::host::Recorder) -> crate::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut waited: Option<crate::counters::Stopwatch> = None;
        loop {
            if state.stopped {
                return Err(crate::Error::internal(
                    "merge-changes stopped after an earlier failure",
                ));
            }
            cancel.check()?;
            if state.used == 0 || state.used + n <= self.budget {
                state.used += n;
                if let Some(watch) = waited {
                    self.wait_ns
                        .fetch_add(watch.elapsed_ns(), Ordering::Relaxed);
                }
                return Ok(());
            }
            waited.get_or_insert_with(crate::counters::Stopwatch::start);
            state = self
                .room
                .wait_timeout(state, crate::tunables::EXECUTOR_IDLE_RECV_TIMEOUT)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn release(&self, n: usize) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.used = state.used.saturating_sub(n);
        drop(state);
        self.room.notify_all();
    }

    fn stop(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.stopped = true;
        self.halted.store(true, Ordering::Relaxed);
        drop(state);
        self.room.notify_all();
    }
}

/// Stops the ledger when dropped: on every exit of the coordinator, return
/// or unwind, before the reader scope joins.
struct StopOnDrop<'a>(&'a Ledger);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// A sealed batch whose credit is taken, waiting for or holding a lane.
struct Pending<T> {
    input: usize,
    seq: usize,
    credit: usize,
    batch: LexBatch,
    tx: Sender<Msg<T>>,
}

/// The encode side of one merge: runs every sealed batch's encode job on the
/// executor, at most `limit` at once ([`Plan::lanes`]), with no pool worker
/// ever waiting for a lane. A sealed batch starts a lane if fewer than
/// `limit` are running and is queued otherwise; a lane that finishes its job
/// hands itself to the oldest queued batch by spawning it, or retires when
/// the queue is empty. The handoff and the retirement happen under the queue
/// lock, so a queued batch always has a running lane ahead of it, and on
/// unwind too ([`Handoff`]'s drop), so no lane is lost. Credit for a queued
/// batch is already taken, so the queue is bounded by the ledger. Every job
/// answers its position on the result channel, which is unbounded, so a job
/// never blocks, even inside the coordinator's `yield_now`. With no limit
/// (`usize::MAX`) a batch is spawned directly, with no lock or handoff.
struct Encoder<'s, T, E> {
    ledger: &'s Ledger,
    encode: &'s E,
    inputs: &'s [&'s Path],
    limit: usize,
    lanes: Mutex<LaneState<T>>,
}

struct LaneState<T> {
    queue: std::collections::VecDeque<Pending<T>>,
    running: usize,
}

impl<'s, T, E> Encoder<'s, T, E>
where
    T: Send + 's,
    E: Fn(&LexBatch, &Path) -> crate::Result<T> + Sync,
{
    fn new(plan: &'s Plan<'s>, ledger: &'s Ledger, encode: &'s E) -> Self {
        Self {
            ledger,
            encode,
            inputs: plan.inputs,
            limit: plan.lanes.max(1),
            lanes: Mutex::new(LaneState {
                queue: std::collections::VecDeque::new(),
                running: 0,
            }),
        }
    }

    /// Run `job` on the executor: at once when the jobs are unbounded
    /// (`Auto`, the default path, with no lane bookkeeping at all), else
    /// within the lane limit.
    fn submit(&'s self, scope: &rayon::Scope<'s>, job: Pending<T>) {
        if self.limit == usize::MAX {
            scope.spawn(move |_| self.run(job));
        } else {
            self.submit_in_lane(scope, job);
        }
    }

    /// The lane-limited path, kept out of the readers' seal so the default
    /// path's code is what it was before lanes existed.
    #[inline(never)]
    fn submit_in_lane(&'s self, scope: &rayon::Scope<'s>, job: Pending<T>) {
        let mut state = self.lanes.lock().unwrap_or_else(PoisonError::into_inner);
        if state.running < self.limit {
            state.running += 1;
            drop(state);
            self.start(scope, job);
        } else {
            state.queue.push_back(job);
        }
    }

    fn start(&'s self, scope: &rayon::Scope<'s>, job: Pending<T>) {
        scope.spawn(move |scope| {
            let _handoff = Handoff {
                encoder: self,
                scope,
            };
            self.run(job);
        });
    }

    /// Encode one batch and answer its position, a panic included.
    fn run(&self, job: Pending<T>) {
        let Pending {
            input,
            seq,
            credit,
            batch,
            tx,
        } = job;
        let path = self.inputs[input];
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if self.ledger.halted() {
                return Err(crate::Error::internal(
                    "merge-changes stopped after an earlier failure",
                ));
            }
            (self.encode)(&batch, path)
        }))
        .unwrap_or_else(|payload| {
            Err(
                crate::error::panic_error("merge-changes worker", payload.as_ref())
                    .context(format!("while merging {}", path.display())),
            )
        });
        drop(tx.send(Msg::Member {
            input,
            seq,
            credit,
            result,
        }));
    }
}

/// Passes a finished lane to the next queued batch, or retires it.
struct Handoff<'a, 's, T, E>
where
    T: Send + 's,
    E: Fn(&LexBatch, &Path) -> crate::Result<T> + Sync,
{
    encoder: &'s Encoder<'s, T, E>,
    scope: &'a rayon::Scope<'s>,
}

impl<'s, T, E> Drop for Handoff<'_, 's, T, E>
where
    T: Send + 's,
    E: Fn(&LexBatch, &Path) -> crate::Result<T> + Sync,
{
    fn drop(&mut self) {
        let mut state = self
            .encoder
            .lanes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match state.queue.pop_front() {
            Some(next) => {
                drop(state);
                self.encoder.start(self.scope, next);
            }
            None => state.running -= 1,
        }
    }
}

/// Run the merge, writing every member in input and batch order to `out`.
/// Returns the number of changes written.
///
/// `scratch` may be `None` exactly when [`super::stream_writes_scratch`] is
/// false for this plan: with one input every member goes to the output, and
/// with one reader the next input becomes the head before any of its members
/// is drained (a slot is reused only at completion, and the loop advances the
/// head before it receives again), so no input is ever pending with a member
/// in hand.
pub(crate) fn run(
    plan: &Plan<'_>,
    scratch: Option<&RunScratch>,
    out: &mut crate::output::OutputWriter,
) -> crate::Result<u64> {
    let is_gz = plan.is_gz;
    let encode = move |batch: &LexBatch, path: &Path| super::encode_batch(batch, path, is_gz);
    let ledger = Ledger::new(plan.batching.inflight_bytes);
    let (tx, rx) = std::sync::mpsc::channel::<Msg<(Vec<u8>, u64)>>();
    let ledger_ref = &ledger;
    let encoder = Encoder::new(plan, ledger_ref, &encode);
    let encoder_ref = &encoder;
    let result = plan.executor.scope(move |scope| {
        crate::threads::scope(|threads| {
            let _stop = StopOnDrop(ledger_ref);
            let spawn_reader = |input: usize| -> crate::Result<()> {
                let tx = tx.clone();
                threads
                    .spawn_named(format!("merge-read-{input}"), move || {
                        read_input(input, plan, scope, encoder_ref, &tx, true);
                    })
                    .map(drop)
                    .map_err(|e| {
                        crate::Error::from(e).context("failed to spawn a merge-changes reader")
                    })
            };
            Coordinator::new(plan, scratch, ledger_ref, out).run(&rx, spawn_reader)
        })
    });
    plan.cancel.counter(
        "merge_changes_credit_wait_ns",
        ledger.wait_ns.load(Ordering::Relaxed),
    );
    result
}

/// Run the readers and workers of [`run`], but collect every batch's
/// `encode` result instead of writing it: per input, in batch order. Credit
/// is returned the moment a result is accepted, so the ledger bounds only
/// batches not yet encoded; what is collected is the caller's to bound.
/// Every input's reader runs to its end before this returns, so an input
/// failure anywhere is returned ahead of anything the caller does next.
pub(crate) fn collect<T, E>(plan: &Plan<'_>, encode: &E) -> crate::Result<Vec<Vec<T>>>
where
    T: Send,
    E: Fn(&LexBatch, &Path) -> crate::Result<T> + Sync,
{
    let ledger = Ledger::new(plan.batching.inflight_bytes);
    let (tx, rx) = std::sync::mpsc::channel::<Msg<T>>();
    let ledger_ref = &ledger;
    let encoder = Encoder::new(plan, ledger_ref, encode);
    let encoder_ref = &encoder;
    let result = plan.executor.scope(move |scope| {
        crate::threads::scope(|threads| {
            let _stop = StopOnDrop(ledger_ref);
            let spawn_reader = |input: usize| -> crate::Result<()> {
                let tx = tx.clone();
                threads
                    .spawn_named(format!("merge-read-{input}"), move || {
                        read_input(input, plan, scope, encoder_ref, &tx, false);
                    })
                    .map(drop)
                    .map_err(|e| {
                        crate::Error::from(e).context("failed to spawn a merge-changes reader")
                    })
            };
            collect_results(plan, ledger_ref, &rx, spawn_reader)
        })
    });
    plan.cancel.counter(
        "merge_changes_credit_wait_ns",
        ledger.wait_ns.load(Ordering::Relaxed),
    );
    result
}

/// The collecting coordinator. A reader slot is reused as soon as its
/// reader has finished: nothing of an input is held open after that.
fn collect_results<T>(
    plan: &Plan<'_>,
    ledger: &Ledger,
    rx: &Receiver<Msg<T>>,
    spawn_reader: impl Fn(usize) -> crate::Result<()>,
) -> crate::Result<Vec<Vec<T>>> {
    let n = plan.inputs.len();
    let readers = plan.readers.clamp(1, n.max(1));
    let mut started = 0;
    while started < readers.min(n) {
        spawn_reader(started)?;
        started += 1;
    }
    let mut slots: Vec<Vec<Option<T>>> = (0..n).map(|_| Vec::new()).collect();
    let mut received = vec![0usize; n];
    let mut batches: Vec<Option<usize>> = vec![None; n];
    let mut complete = 0usize;
    let mut batches_total = 0usize;
    let protocol = |input: usize| {
        crate::Error::internal(format!(
            "merge-changes input {input} answered out of protocol"
        ))
    };
    while complete < n {
        // A message is handled before the cancellation check, so an error
        // it carries is returned rather than replaced by the cancellation.
        match recv(rx, plan.executor)? {
            Msg::Member {
                input,
                seq,
                credit,
                result,
            } => {
                let value = result?;
                ledger.release(credit);
                let slot = &mut slots[input];
                if slot.len() <= seq {
                    slot.resize_with(seq + 1, || None);
                }
                if slot[seq].is_some() || batches[input].is_some_and(|b| seq >= b) {
                    return Err(protocol(input));
                }
                slot[seq] = Some(value);
                received[input] += 1;
                if batches[input] == Some(received[input]) {
                    complete += 1;
                }
            }
            Msg::ReaderDone {
                input,
                batches: count,
            } => {
                if batches[input].is_some() || received[input] > count || slots[input].len() > count
                {
                    return Err(protocol(input));
                }
                batches[input] = Some(count);
                batches_total += count;
                if received[input] == count {
                    complete += 1;
                }
                if started < n {
                    spawn_reader(started)?;
                    started += 1;
                }
            }
            Msg::ReaderFailed(error) => return Err(error),
        }
        plan.cancel.check()?;
    }
    plan.cancel.counter("merge_changes_batches", batches_total);
    slots
        .into_iter()
        .enumerate()
        .map(|(input, slot)| {
            slot.into_iter()
                .collect::<Option<Vec<T>>>()
                .ok_or_else(|| protocol(input))
        })
        .collect()
}

/// One reader thread's whole life. Always ends with exactly one terminal
/// message, sent last, after which it does nothing that can block.
/// `ordered` is true under [`run`]'s coordinator, the only one the
/// `test-hooks` spill-first handshake applies to.
fn read_input<'s, T, E>(
    input: usize,
    plan: &'s Plan<'s>,
    scope: &rayon::Scope<'s>,
    encoder: &'s Encoder<'s, T, E>,
    tx: &Sender<Msg<T>>,
    ordered: bool,
) where
    T: Send + 's,
    E: Fn(&LexBatch, &Path) -> crate::Result<T> + Sync,
{
    let ledger = encoder.ledger;
    let path = plan.inputs[input];
    let cancel = plan.cancel;
    #[cfg(pbfhogg_test_hooks)]
    let stopped = || ledger.halted() || cancel.is_cancelled();
    #[cfg(not(pbfhogg_test_hooks))]
    let _ = ordered;
    let body = || -> crate::Result<usize> {
        #[cfg(pbfhogg_test_hooks)]
        if ordered {
            cancel.faults().before_merge_changes_read(input, stopped)?;
        }
        let seq = Cell::new(0usize);
        let seal = |batch: LexBatch| -> crate::Result<()> {
            #[cfg(pbfhogg_test_hooks)]
            if ordered {
                cancel
                    .faults()
                    .before_merge_changes_seal(input, seq.get(), stopped)?;
            }
            let credit = batch.bytes().max(1);
            ledger.acquire(credit, cancel)?;
            let s = seq.get();
            seq.set(s + 1);
            encoder.submit(
                scope,
                Pending {
                    input,
                    seq: s,
                    credit,
                    batch,
                    tx: tx.clone(),
                },
            );
            Ok(())
        };
        let mut sink = LexSink::new(plan.batching.batch_bytes, seal, || ledger.halted());
        // Deliberately use the shared historical per-file decompression counter.
        // Command-specific names or accumulation would break brokkr comparisons.
        parse_osc_path(path, &mut sink, cancel)?;
        sink.finish()?;
        Ok(seq.get())
    };
    let msg = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(Ok(batches)) => Msg::ReaderDone { input, batches },
        Ok(Err(error)) => Msg::ReaderFailed(error),
        Err(payload) => Msg::ReaderFailed(
            crate::error::panic_error("merge-changes reader", payload.as_ref())
                .context(format!("while reading {}", path.display())),
        ),
    };
    drop(tx.send(msg));
}

/// Where an input's in-order members go.
enum Sink {
    /// Not the head yet and nothing written: no file made.
    Pending,
    /// Not the head yet: members appended to a scratch file.
    Spill(ScratchPath, io::BufWriter<crate::scratch::ScratchFile>),
    /// Not the head yet, every member spilled: the file is closed.
    Closed(ScratchPath),
    /// The head input: members go to the output.
    Output,
}

struct InputState {
    /// Finished members waiting for an earlier one of the same input.
    waiting: BTreeMap<usize, (Vec<u8>, u64, usize)>,
    /// The next sequence number to write.
    next: usize,
    /// The batch count, once the reader has finished.
    batches: Option<usize>,
    changes: u64,
    sink: Sink,
    /// Reader finished and every member written; its reader slot is free.
    complete: bool,
}

struct Coordinator<'a, 'p> {
    plan: &'a Plan<'p>,
    /// `None` only on a plan that provably never spills; see [`run`].
    scratch: Option<&'a RunScratch>,
    ledger: &'a Ledger,
    out: &'a mut crate::output::OutputWriter,
    states: Vec<InputState>,
    head: usize,
    total: u64,
}

impl<'a, 'p> Coordinator<'a, 'p> {
    fn new(
        plan: &'a Plan<'p>,
        scratch: Option<&'a RunScratch>,
        ledger: &'a Ledger,
        out: &'a mut crate::output::OutputWriter,
    ) -> Self {
        let states = (0..plan.inputs.len())
            .map(|i| InputState {
                waiting: BTreeMap::new(),
                next: 0,
                batches: None,
                changes: 0,
                sink: if i == 0 { Sink::Output } else { Sink::Pending },
                complete: false,
            })
            .collect();
        Self {
            plan,
            scratch,
            ledger,
            out,
            states,
            head: 0,
            total: 0,
        }
    }

    fn run(
        mut self,
        rx: &Receiver<Msg<(Vec<u8>, u64)>>,
        spawn_reader: impl Fn(usize) -> crate::Result<()>,
    ) -> crate::Result<u64> {
        let n = self.plan.inputs.len();
        let readers = self.plan.readers.clamp(1, n.max(1));
        let mut started = 0;
        while started < readers.min(n) {
            spawn_reader(started)?;
            started += 1;
        }
        let mut batches_total = 0usize;
        while self.head < n {
            let head = &self.states[self.head];
            if head.batches == Some(head.next) {
                self.advance_head()?;
                continue;
            }
            // A message is handled before the cancellation check, so an error
            // it carries is returned rather than replaced by the cancellation.
            let msg = recv(rx, self.plan.executor)?;
            match msg {
                Msg::Member {
                    input,
                    seq,
                    credit,
                    result,
                } => {
                    let (bytes, changes) = result?;
                    let state = &mut self.states[input];
                    let beyond = state.batches.is_some_and(|b| seq >= b);
                    if seq < state.next || beyond || state.waiting.contains_key(&seq) {
                        return Err(crate::Error::internal(format!(
                            "merge-changes received batch {seq} of input {input} out of protocol"
                        )));
                    }
                    state.waiting.insert(seq, (bytes, changes, credit));
                    self.drain(input)?;
                    if self.close_if_complete(input)? && started < n {
                        spawn_reader(started)?;
                        started += 1;
                    }
                }
                Msg::ReaderDone { input, batches } => {
                    let state = &mut self.states[input];
                    let inconsistent = state.batches.is_some()
                        || state.next > batches
                        || state
                            .waiting
                            .keys()
                            .next_back()
                            .is_some_and(|&s| s >= batches);
                    if inconsistent {
                        return Err(crate::Error::internal(format!(
                            "merge-changes input {input} reported an inconsistent batch count"
                        )));
                    }
                    state.batches = Some(batches);
                    batches_total += batches;
                    if self.close_if_complete(input)? && started < n {
                        spawn_reader(started)?;
                        started += 1;
                    }
                }
                Msg::ReaderFailed(error) => return Err(error),
            }
            self.plan.cancel.check()?;
        }
        self.plan
            .cancel
            .counter("merge_changes_batches", batches_total);
        Ok(self.total)
    }

    /// Mark `input` complete once its reader has finished and every one of
    /// its members is written, returning whether it became complete just
    /// now; the caller then frees its reader slot. A reader slot is reused
    /// only at completion, not when the reader finishes, so open spill
    /// writers stay bounded by the reader cap. A complete input that is not
    /// the head keeps no open file or buffer while it waits: its spill
    /// writer is flushed and closed, and only the path is kept for
    /// promotion.
    fn close_if_complete(&mut self, input: usize) -> crate::Result<bool> {
        let head = self.head;
        let state = &mut self.states[input];
        if state.complete || state.batches != Some(state.next) {
            return Ok(false);
        }
        state.complete = true;
        if input == head {
            return Ok(true);
        }
        if let Sink::Spill(..) = state.sink {
            let Sink::Spill(path, file) = std::mem::replace(&mut state.sink, Sink::Pending) else {
                return Err(crate::Error::internal("merge-changes spill state changed"));
            };
            file.into_inner()
                .map_err(|e| spill_err(&path, e.into_error()))?;
            state.sink = Sink::Closed(path);
        }
        #[cfg(pbfhogg_test_hooks)]
        self.plan.cancel.faults().on_merge_changes_complete(input);
        Ok(true)
    }

    /// Write every member of `input` that is next in order to its sink.
    fn drain(&mut self, input: usize) -> crate::Result<()> {
        loop {
            let state = &mut self.states[input];
            let Some((bytes, changes, credit)) = state.waiting.remove(&state.next) else {
                return Ok(());
            };
            state.next += 1;
            state.changes += changes;
            if matches!(state.sink, Sink::Pending) {
                let path =
                    spill_scratch(self.scratch, input)?.file(&format!("merge-input-{input}"))?;
                let file = crate::scratch::create_read_write(&path, true)
                    .map_err(|e| spill_err(&path, e))?;
                state.sink = Sink::Spill(path, io::BufWriter::with_capacity(SPILL_CHUNK, file));
            }
            match &mut state.sink {
                Sink::Output => {
                    self.out.write_all(&bytes)?;
                    self.total += changes;
                }
                Sink::Spill(path, file) => {
                    file.write_all(&bytes).map_err(|e| spill_err(path, e))?;
                    #[cfg(pbfhogg_test_hooks)]
                    self.plan.cancel.faults().on_merge_changes_spilled(input);
                }
                Sink::Pending | Sink::Closed(_) => {
                    return Err(crate::Error::internal(
                        "merge-changes member for an input with no open spill file",
                    ));
                }
            }
            drop(bytes);
            self.ledger.release(credit);
        }
    }

    /// The head input is complete: report it, then make the next input the
    /// head, copying whatever it already spilled into the output.
    fn advance_head(&mut self) -> crate::Result<()> {
        let done = &self.states[self.head];
        self.plan
            .cancel
            .counter("merge_changes_changes_per_osc", done.changes);
        self.head += 1;
        let Some(state) = self.states.get_mut(self.head) else {
            return Ok(());
        };
        let sink = std::mem::replace(&mut state.sink, Sink::Output);
        // Members already counted while spilling reach the output now.
        self.total += state.changes;
        #[cfg(pbfhogg_test_hooks)]
        let copied = match &sink {
            Sink::Spill(..) => Some(true),
            Sink::Closed(_) => Some(false),
            Sink::Pending | Sink::Output => None,
        };
        let spill = match sink {
            Sink::Spill(path, file) => {
                let file = file
                    .into_inner()
                    .map_err(|e| spill_err(&path, e.into_error()))?;
                Some((path, file))
            }
            Sink::Closed(path) => {
                let file = crate::scratch::open(&path).map_err(|e| spill_err(&path, e))?;
                Some((path, file))
            }
            Sink::Pending | Sink::Output => None,
        };
        if let Some((path, mut file)) = spill {
            let spilled = file
                .seek(io::SeekFrom::Start(0))
                .and_then(|_| file.metadata())
                .map_err(|e| spill_err(&path, e))?
                .len();
            self.plan
                .cancel
                .counter("merge_changes_spill_bytes", spilled);
            let mut buf = vec![0u8; SPILL_CHUNK];
            loop {
                self.plan.cancel.check()?;
                let n = match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(spill_err(&path, e)),
                };
                self.out.write_all(&buf[..n])?;
            }
            drop(file);
            spill_scratch(self.scratch, self.head)?.remove(&path);
        }
        #[cfg(pbfhogg_test_hooks)]
        self.plan
            .cancel
            .faults()
            .on_merge_changes_promoted(self.head, copied);
        Ok(())
    }
}

/// The run scratch a spill of `input` goes in. A spill without one means
/// the caller's `stream_writes_scratch` premise was broken.
fn spill_scratch(scratch: Option<&RunScratch>, input: usize) -> crate::Result<&RunScratch> {
    scratch.ok_or_else(|| {
        crate::Error::internal(format!(
            "merge-changes input {input} spilled on a run given no scratch directory"
        ))
    })
}

/// Spill write and copy-back unit.
const SPILL_CHUNK: usize = 1024 * 1024;

fn spill_err(path: &ScratchPath, e: io::Error) -> crate::Error {
    crate::Error::from(e).context(format!("merge-changes spill file {}", path.display()))
}

/// Receive one message, running queued pool jobs while waiting when the
/// caller is a pool worker (the writer's `recv_permit` shape).
fn recv<T>(rx: &Receiver<Msg<T>>, executor: crate::host::ExecutorRef<'_>) -> crate::Result<Msg<T>> {
    let lost = || crate::Error::internal("merge-changes workers exited without a result");
    loop {
        match rx.try_recv() {
            Ok(item) => return Ok(item),
            Err(TryRecvError::Disconnected) => return Err(lost()),
            Err(TryRecvError::Empty) => match executor.yield_now() {
                Some(rayon::Yield::Executed) => {}
                Some(rayon::Yield::Idle) => {
                    match rx.recv_timeout(crate::tunables::EXECUTOR_IDLE_RECV_TIMEOUT) {
                        Ok(item) => return Ok(item),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return Err(lost()),
                    }
                }
                None => return rx.recv().map_err(|_| lost()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Exercise the executor directly: parsing and production batch sizing
    /// are independent of lane admission. Result ordering belongs to the
    /// coordinator, not the encoder's completion channel.
    #[test]
    fn encode_jobs_stay_on_the_executor_within_their_lanes() {
        let host_owner = crate::host::Host::for_tests();
        let host = host_owner.entered();
        for (threads, lanes) in [(6, 2), (1, 1), (3, 1)] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .expect("build executor");
            let running = AtomicUsize::new(0);
            let started = AtomicUsize::new(0);
            let peak = AtomicUsize::new(0);
            let off_pool = AtomicUsize::new(0);
            let rendezvous = std::sync::Barrier::new(lanes);
            let inputs = [Path::new("synthetic.osc")];
            let plan = Plan {
                batching: Batching::PRODUCTION,
                inputs: &inputs,
                is_gz: false,
                readers: 1,
                lanes,
                executor: crate::host::ExecutorRef::Pool(&pool),
                cancel: &host.invocation,
            };
            let encode = |_: &LexBatch, _: &Path| -> crate::Result<()> {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                if pool.current_thread_index().is_none() {
                    off_pool.fetch_add(1, Ordering::SeqCst);
                }
                // Only the first wave waits. A serial implementation cannot
                // complete the two-lane case; later jobs exercise handoff.
                if started.fetch_add(1, Ordering::SeqCst) < lanes {
                    rendezvous.wait();
                }
                running.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            };
            let ledger = Ledger::new(usize::MAX);
            let encoder = Encoder::new(&plan, &ledger, &encode);
            let (tx, rx) = std::sync::mpsc::channel();
            pool.scope(|scope| {
                for seq in 0..8 {
                    encoder.submit(
                        scope,
                        Pending {
                            input: 0,
                            seq,
                            credit: 0,
                            batch: LexBatch::default(),
                            tx: tx.clone(),
                        },
                    );
                }
            });
            drop(tx);
            let mut answered = Vec::new();
            for message in rx {
                let Msg::Member {
                    input, seq, result, ..
                } = message
                else {
                    panic!("encoder sent a reader message");
                };
                assert_eq!(input, 0);
                result.expect("encode batch");
                answered.push(seq);
            }
            answered.sort_unstable();
            assert_eq!(answered, (0..8).collect::<Vec<_>>());
            assert_eq!(off_pool.load(Ordering::SeqCst), 0);
            assert_eq!(peak.load(Ordering::SeqCst), lanes);
            assert_eq!(running.load(Ordering::SeqCst), 0);
        }
    }
}
