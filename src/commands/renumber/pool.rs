//! The ordered work-stealing pool every parallel renumber stage runs on.
//!
//! Pass 1, stage 2d and R2d share one shape: workers claim blob indices from
//! an atomic counter, rewrite the blob, and hand the result to the calling
//! thread, which writes results strictly in index order. This module is that
//! shape, written once, with three properties the three hand-rolled copies it
//! replaces did not have:
//!
//! - **An error ends the run.** The result channel and its receiver live
//!   inside `thread::scope`, so every consumer exit drops the receiver before
//!   the scope joins its workers, and a cancellation flag stops workers from
//!   claiming more work. The old copies kept the receiver alive past the
//!   scope and had no flag: a mid-stream error with more than a channel's
//!   worth of blobs left parked every worker in `send` forever while the
//!   scope waited to join them.
//! - **Results in flight are bounded.** A claimed index may only be worked
//!   once it is inside a window of `window` indices past the last one the
//!   consumer took. Without it a single slow blob let the other workers run
//!   arbitrarily far ahead, every finished result piling up in the reorder
//!   buffer (and eventually tripping its runaway-window panic).
//! - **The reported error is deterministic.** It is always the error of the
//!   lowest failing index (worker panics excepted). Claims are a monotone
//!   `fetch_add`, so when index `k` fails every index below `k` has been
//!   claimed, and an index inside the window is always worked and delivered,
//!   cancellation or not: only claims still *outside* the window are
//!   abandoned, and those are all above `k` (`k` was inside the window when
//!   it ran, and the window only moves forward).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use super::super::Result;
use super::{StageCounters, add_ns};
use crate::blob_meta::BlobIndex;

/// Result window per worker. Large enough that a worker is never parked
/// behind a slightly slow neighbour, small enough that the buffered
/// rewritten blobs stay a few hundred MB at worst.
const WINDOW_PER_WORKER: usize = 8;
const WINDOW_MIN: usize = 32;

/// The admission gate: the window position and the cancellation flag, both
/// under one mutex so a waiter's predicate check and the update that wakes
/// it cannot interleave (no lost wakeup). `aborted` mirrors `abort` for the
/// lock-free check workers make before each claim.
struct Gate {
    state: Mutex<GateState>,
    cv: Condvar,
    aborted: AtomicBool,
}

struct GateState {
    /// Indices below this have been taken by the consumer.
    consumed: usize,
    abort: bool,
}

impl Gate {
    fn new() -> Self {
        Self {
            state: Mutex::new(GateState {
                consumed: 0,
                abort: false,
            }),
            cv: Condvar::new(),
            aborted: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> MutexGuard<'_, GateState> {
        // A panic while holding this lock leaves two plain integers behind,
        // which are still meaningful; the panic itself surfaces at join.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn abort(&self) {
        self.aborted.store(true, Ordering::Release);
        self.lock().abort = true;
        self.cv.notify_all();
    }

    fn advance(&self, consumed: usize) {
        self.lock().consumed = consumed;
        self.cv.notify_all();
    }

    /// Block until `idx` is inside the window. `false` means the run was
    /// cancelled while `idx` was still outside it, and the claim is
    /// abandoned. Eligibility wins over cancellation: an index inside the
    /// window is always worked, which is what keeps every index below a
    /// failing one deliverable.
    fn admit(&self, idx: usize, window: usize) -> bool {
        let mut state = self.lock();
        loop {
            if idx < state.consumed.saturating_add(window) {
                return true;
            }
            if state.abort {
                return false;
            }
            state = self.cv.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// Cancels the run when dropped. Held by the consumer for its whole loop, so
/// every exit - success, `?` on an error, or a panic in the consumer or the
/// reorder buffer - wakes workers parked in the window gate. Dropping the
/// receiver alone only wakes workers parked in `send`.
struct CancelOnDrop<'a>(&'a Gate);

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Cancels the run if a worker unwinds, so the rest stop claiming.
struct CancelOnUnwind<'a>(&'a Gate);

impl Drop for CancelOnUnwind<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.abort();
        }
    }
}

/// Run `work` for every index in `0..n_tasks` on one thread per element of
/// `states`, and hand each `Ok` result to `consume` on the calling thread in
/// index order.
///
/// Returns the first error in index order: a worker's `Err`, or `consume`'s.
/// Either one ends the run promptly; workers stop claiming, and results
/// already in flight are dropped. A worker panic resurfaces as a panic when
/// the scope joins.
pub(super) fn run_ordered<S, T, W, C>(
    n_tasks: usize,
    states: &mut [S],
    counters: &StageCounters,
    work: W,
    mut consume: C,
) -> Result<()>
where
    S: Send,
    T: Send,
    W: Fn(&mut S, usize) -> std::result::Result<T, String> + Sync,
    C: FnMut(T) -> Result<()>,
{
    if n_tasks == 0 {
        return Ok(());
    }
    if states.is_empty() {
        return Err("renumber: worker pool started with no workers".into());
    }
    let window = states
        .len()
        .saturating_mul(WINDOW_PER_WORKER)
        .max(WINDOW_MIN);
    let gate = Gate::new();
    let next = AtomicUsize::new(0);

    std::thread::scope(|scope| -> Result<()> {
        // Created inside the scope: the receiver is a local of this closure,
        // so it is dropped on every return path before the scope joins.
        let (tx, rx) =
            std::sync::mpsc::sync_channel::<(usize, std::result::Result<T, String>)>(window);
        // Installed before the first spawn, so even a failed `scope.spawn`
        // (thread creation panics) wakes the workers already parked in the
        // window gate. Declared after `rx`, so it drops first: workers are
        // cancelled, then the receiver goes.
        let _cancel = CancelOnDrop(&gate);
        for state in states.iter_mut() {
            let tx = tx.clone();
            let (gate, next, work) = (&gate, &next, &work);
            scope.spawn(move || {
                let _cancel = CancelOnUnwind(gate);
                loop {
                    if gate.aborted.load(Ordering::Acquire) {
                        return;
                    }
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= n_tasks {
                        return;
                    }
                    let t = Instant::now();
                    let admitted = gate.admit(idx, window);
                    add_ns(&counters.window_wait_ns, t);
                    if !admitted {
                        return;
                    }
                    let result = work(state, idx);
                    if result.is_err() {
                        gate.abort();
                    }
                    if tx.send((idx, result)).is_err() {
                        return;
                    }
                }
            });
        }
        drop(tx);

        let mut reorder = crate::reorder_buffer::ReorderBuffer::with_max_window(window, window);
        loop {
            let t = Instant::now();
            let msg = rx.recv();
            add_ns(&counters.consumer_recv_wait_ns, t);
            let Ok((idx, item)) = msg else {
                break;
            };
            reorder.push(idx, item);
            while let Some(item) = reorder.pop_ready() {
                gate.advance(reorder.next_seq());
                let value = item.map_err(Box::<dyn std::error::Error>::from)?;
                consume(value)?;
            }
        }
        if reorder.next_seq() != n_tasks {
            // Only a worker that died without sending gets here; its panic
            // takes precedence when the scope joins.
            return Err(format!(
                "renumber: a worker exited without delivering blob {} of {n_tasks}",
                reorder.next_seq()
            )
            .into());
        }
        Ok(())
    })
}

/// One rewritten blob, as a parallel stage's worker hands it to the writer.
pub(super) struct Rewritten {
    /// The block to write, or `None` for a blob that holds no elements.
    pub(super) block: Option<(Vec<u8>, BlobIndex)>,
    /// First and last *input* ID the blob held, `None` when empty.
    pub(super) span: Option<(i64, i64)>,
    pub(super) count: u64,
    pub(super) orphans: u64,
}

/// What a rewrite stage reports once every blob is written.
pub(super) struct StageTotals {
    pub(super) written: u64,
    pub(super) orphans: u64,
}

/// Enforces that input IDs of one element kind strictly increase across
/// blobs, in schedule order.
///
/// Renumber assigns a new ID by *position* (blob base plus offset) while
/// every reference is resolved by *rank* in the set of input IDs. The two
/// agree exactly when the IDs strictly increase over the whole kind section;
/// a duplicate or a step backwards silently points references at the wrong
/// element. Within a blob the rewriters check each element against the one
/// before; this is the cross-blob half, fed with the IDs the blob actually
/// held rather than its indexdata range, which nothing verifies.
pub(super) struct StrictOrder {
    kind: &'static str,
    last: Option<i64>,
}

impl StrictOrder {
    pub(super) fn new(kind: &'static str) -> Self {
        Self { kind, last: None }
    }

    pub(super) fn admit(&mut self, span: Option<(i64, i64)>) -> Result<()> {
        let Some((first, last)) = span else {
            return Ok(());
        };
        if let Some(prev) = self.last
            && first <= prev
        {
            return Err(super::wire_rewrite::order_error(self.kind, prev, first).into());
        }
        self.last = Some(last);
        Ok(())
    }
}

/// Run one parallel rewrite stage: `work` rewrites blob `idx` on a worker,
/// and the calling thread checks cross-blob order and writes the results in
/// schedule order. Counters are emitted under `prefix` once the stage ends.
pub(super) fn run_rewrite_stage<S, W>(
    kind: &'static str,
    prefix: &str,
    n_tasks: usize,
    states: &mut [S],
    counters: &StageCounters,
    writer: &mut crate::writer::PbfWriter<crate::write::file_writer::FileWriter>,
    work: W,
) -> Result<StageTotals>
where
    S: Send,
    W: Fn(&mut S, usize) -> std::result::Result<Rewritten, String> + Sync,
{
    let mut order = StrictOrder::new(kind);
    let mut totals = StageTotals {
        written: 0,
        orphans: 0,
    };
    run_ordered(n_tasks, states, counters, work, |item: Rewritten| {
        order.admit(item.span)?;
        if let Some((bytes, index)) = item.block {
            let t = Instant::now();
            // Deliberately write no tagdata: terminal renumber output is not
            // tag-filtered. Carrying headers or rebuilding keys would add per-blob
            // or per-tag work and change the historical output bytes.
            writer.write_primitive_block_owned(bytes, index, None, None)?;
            add_ns(&counters.consumer_write_ns, t);
        }
        // Counted after the write, so on a mid-stream error the totals cover
        // only output actually emitted.
        totals.written = totals.written.saturating_add(item.count);
        totals.orphans = totals.orphans.saturating_add(item.orphans);
        Ok(())
    })?;
    counters.emit(prefix);
    Ok(totals)
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

    fn run(
        n: usize,
        workers: usize,
        work: impl Fn(usize) -> std::result::Result<usize, String> + Sync,
        consume: impl FnMut(usize) -> Result<()>,
    ) -> std::result::Result<(), String> {
        let counters = StageCounters::new();
        let mut states = vec![(); workers];
        run_ordered(n, &mut states, &counters, |_, idx| work(idx), consume)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn delivers_every_result_in_index_order() {
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

    /// The hang shape: a failure with far more than a channel's worth of
    /// work left. The old pools parked every worker in `send` and hung.
    #[test]
    fn a_worker_error_mid_stream_ends_the_run_with_that_error() {
        let err = within_deadline(|| {
            run(
                20_000,
                4,
                |i| {
                    if i == 700 {
                        Err(format!("bad blob {i}"))
                    } else {
                        Ok(i)
                    }
                },
                |_| Ok(()),
            )
            .unwrap_err()
        });
        assert_eq!(err, "bad blob 700");
    }

    /// Several failures: the reported one is the lowest index, every time.
    #[test]
    fn the_lowest_failing_index_is_the_one_reported() {
        for _ in 0..20 {
            let err = within_deadline(|| {
                run(
                    5_000,
                    6,
                    |i| {
                        if i == 300 || i == 301 || i == 2_000 {
                            Err(format!("bad blob {i}"))
                        } else {
                            Ok(i)
                        }
                    },
                    |_| Ok(()),
                )
                .unwrap_err()
            });
            assert_eq!(err, "bad blob 300");
        }
    }

    /// The consumer's own failure (a writer error, an ordering violation)
    /// must end the run just as promptly.
    #[test]
    fn a_consumer_error_ends_the_run() {
        let err = within_deadline(|| {
            run(20_000, 4, Ok, |v| {
                if v == 50 {
                    Err("disk full".into())
                } else {
                    Ok(())
                }
            })
            .unwrap_err()
        });
        assert_eq!(err, "disk full");
    }

    #[test]
    fn an_error_in_the_last_task_is_reported() {
        let err = within_deadline(|| {
            run(
                1_000,
                4,
                |i| if i == 999 { Err("last".into()) } else { Ok(i) },
                |_| Ok(()),
            )
            .unwrap_err()
        });
        assert_eq!(err, "last");
    }

    /// One slow blob while the others race ahead. Without the window the
    /// other workers buffered every later result behind it and, past the
    /// reorder buffer's runaway limit, panicked.
    #[test]
    fn a_slow_blob_does_not_let_the_others_run_unbounded() {
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
        // The gate admits `i < consumed + window`; the test's own `consumed`
        // is bumped just after the pool publishes its, so it may lag by one.
        let window = WINDOW_MIN.max(4 * WINDOW_PER_WORKER);
        assert!(
            peak <= window,
            "a worker ran {peak} blobs ahead of the consumer; the window ({window}) must bound it"
        );
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

    #[test]
    fn strict_order_refuses_a_step_back_or_a_repeat_across_blobs() {
        let mut order = StrictOrder::new("node");
        order.admit(Some((1, 5))).unwrap();
        order.admit(None).unwrap();
        order.admit(Some((6, 9))).unwrap();
        assert!(
            order.admit(Some((9, 12))).is_err(),
            "a repeat must be refused"
        );
        let mut order = StrictOrder::new("node");
        order.admit(Some((10, 20))).unwrap();
        assert!(
            order.admit(Some((3, 4))).is_err(),
            "a step back must be refused"
        );
    }
}
