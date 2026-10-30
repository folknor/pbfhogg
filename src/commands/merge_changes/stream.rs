//! The default (non-simplify) merge: readers, encoding workers and one
//! ordered coordinator, with output memory bounded by a byte budget rather
//! than by the size of the output.
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
//!   spent), then spawns its encode job on the pool tagged `(input, seq)`.
//!   Credit is taken before the sequence number becomes outstanding, and an
//!   input's batches are sealed in order.
//! - **Workers.** Replay a batch into the owned sink, re-emit XML and gzip
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

/// What one streaming merge needs.
pub(super) struct Plan<'a> {
    pub(super) inputs: &'a [&'a Path],
    pub(super) is_gz: bool,
    /// Upper bound on concurrently alive reader threads.
    pub(super) readers: usize,
    pub(super) executor: crate::host::ExecutorRef<'a>,
    pub(super) scratch: &'a RunScratch,
    pub(super) cancel: &'a crate::host::Recorder,
}

enum Msg {
    Member {
        input: usize,
        seq: usize,
        credit: usize,
        result: crate::Result<(Vec<u8>, u64)>,
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

/// Run the merge, writing every member in input and batch order to `out`.
/// Returns the number of changes written.
pub(super) fn run(plan: &Plan<'_>, out: &mut crate::output::OutputWriter) -> crate::Result<u64> {
    let ledger = Ledger::new(crate::tunables::MERGE_CHANGES_INFLIGHT_BYTES);
    let (tx, rx) = std::sync::mpsc::channel::<Msg>();
    let ledger_ref = &ledger;
    let result = plan.executor.scope(move |scope| {
        std::thread::scope(|threads| {
            let _stop = StopOnDrop(ledger_ref);
            let spawn_reader = |input: usize| -> crate::Result<()> {
                let tx = tx.clone();
                std::thread::Builder::new()
                    .name(format!("merge-read-{input}"))
                    .spawn_scoped(threads, move || {
                        read_input(input, plan, scope, ledger_ref, &tx);
                    })
                    .map(drop)
                    .map_err(|e| {
                        crate::Error::from(e).context("failed to spawn a merge-changes reader")
                    })
            };
            Coordinator::new(plan, ledger_ref, out).run(&rx, spawn_reader)
        })
    });
    plan.cancel.counter(
        "merge_changes_credit_wait_ns",
        ledger.wait_ns.load(Ordering::Relaxed),
    );
    result
}

/// One reader thread's whole life. Always ends with exactly one terminal
/// message, sent last, after which it does nothing that can block.
fn read_input<'s>(
    input: usize,
    plan: &'s Plan<'s>,
    scope: &rayon::Scope<'s>,
    ledger: &'s Ledger,
    tx: &Sender<Msg>,
) {
    let path = plan.inputs[input];
    let is_gz = plan.is_gz;
    let cancel = plan.cancel;
    let body = || -> crate::Result<usize> {
        let seq = Cell::new(0usize);
        let seal = |batch: LexBatch| -> crate::Result<()> {
            let credit = batch.bytes().max(1);
            ledger.acquire(credit, cancel)?;
            let s = seq.get();
            seq.set(s + 1);
            let tx = tx.clone();
            scope.spawn(move |_| {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if ledger.halted() {
                        return Err(crate::Error::internal(
                            "merge-changes stopped after an earlier failure",
                        ));
                    }
                    super::encode_batch(&batch, path, is_gz)
                }))
                .unwrap_or_else(|payload| {
                    Err(
                        crate::error::panic_error("merge-changes worker", payload.as_ref())
                            .context(format!("while merging {}", path.display())),
                    )
                });
                drop(tx.send(Msg::Member {
                    input,
                    seq: s,
                    credit,
                    result,
                }));
            });
            Ok(())
        };
        let mut sink = LexSink::new(crate::tunables::MERGE_CHANGES_BATCH_BYTES, seal, || {
            ledger.halted()
        });
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
    Spill(ScratchPath, io::BufWriter<std::fs::File>),
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
    ledger: &'a Ledger,
    out: &'a mut crate::output::OutputWriter,
    states: Vec<InputState>,
    head: usize,
    total: u64,
}

impl<'a, 'p> Coordinator<'a, 'p> {
    fn new(
        plan: &'a Plan<'p>,
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
            ledger,
            out,
            states,
            head: 0,
            total: 0,
        }
    }

    fn run(
        mut self,
        rx: &Receiver<Msg>,
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
                let path = self.plan.scratch.file(&format!("merge-input-{input}"))?;
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
            self.plan.scratch.remove(&path);
        }
        Ok(())
    }
}

/// Spill write and copy-back unit.
const SPILL_CHUNK: usize = 1024 * 1024;

fn spill_err(path: &ScratchPath, e: io::Error) -> crate::Error {
    crate::Error::from(e).context(format!("merge-changes spill file {}", path.display()))
}

/// Receive one message, running queued pool jobs while waiting when the
/// caller is a pool worker (the writer's `recv_permit` shape).
fn recv(rx: &Receiver<Msg>, executor: crate::host::ExecutorRef<'_>) -> crate::Result<Msg> {
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
