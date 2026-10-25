//! Parallel writer backend: `pwrite`-based output across a pool of
//! writer threads.
//!
//! Motivation: the single-threaded `FileOutputSink` and `uring_writer`
//! backends cap at ~1.5 GB/s on NVMe, far below the ~5 GB/s sequential
//! peak. At planet scale with ~95-120 GB of output bytes this is the
//! wall floor. Splitting the actual disk writes across N threads (each
//! `pwrite`ing at its assigned offset on a shared file descriptor)
//! should scale closer to NVMe peak.
//!
//! ## Design
//!
//! - One **writer-thread** receives `PipelineItem` in send-order, uses a
//!   `ReorderBuffer` (capacity `WRITE_AHEAD`) to pop in global-seq
//!   order, computes each item's final byte offset by accumulating
//!   lengths, and dispatches a `WriteOp` (Write { offset, bytes } |
//!   CopyRange { out_offset, in_fd, src_offset, len }) to the pool.
//! - A **pool** of `POOL_SIZE` worker threads pops `WriteOp`s from
//!   their per-worker bounded channel and runs `pwrite` / kernel-space
//!   `copy_file_range` at the pre-computed offset. Writes complete in
//!   any order; file contents are correct because offsets were
//!   assigned in seq order on the writer thread.
//! - On channel close, writer thread closes pool channels, joins pool
//!   workers, `sync_all`s, returns. It does **not** truncate: workers
//!   `pwrite` at absolute offsets and the kernel extends EOF to the
//!   highest offset written, so there is no over-allocated tail to cut
//!   back. This bullet used to say "truncates to logical size", which
//!   contradicted `parallel_writer_thread`'s own doc and the premise of
//!   `decisions/0014-output-backends-stay-separate.md`, whose argument
//!   turns on this backend sharing none of the accumulate / align /
//!   `set_len` shape the other backends are assumed to duplicate.
//!
//! ## Status: live, and the default backend
//!
//! This is not scaffolding. `PbfWriter::to_path_parallel` is integrated
//! and this backend is the **default** for `apply-changes` and for
//! `add-locations-to-ways` pass 2. The module doc carried "Status:
//! scaffold (2026-04-21) ... integration lands in a follow-up commit"
//! long after that follow-up commit landed, and the module-wide
//! `#![allow(dead_code)]` that the scaffold needed outlived it too -
//! suppressing genuine dead-code detection across a backend on the hot
//! path. Both are gone; anything here that is truly unused must now say
//! so at its own definition, with a reason.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::{Scope, ScopedJoinHandle};

use crate::error::{Error, Result};
use crate::reorder_buffer::ReorderBuffer;
use crate::tunables::{PER_WORKER_CAPACITY, POOL_SIZE, WRITE_AHEAD};
use crate::write::metrics::WRITER_METRICS;
use crate::write::pipeline::{OutputChunk, PipelineItem, SeqTotal, check_stream_complete};
use crate::write::pool_failure::FirstFailure;

/// One unit of work dispatched from the writer-thread to the pool.
/// Carries the pre-computed output offset; workers do not compute or
/// coordinate offsets among themselves.
enum WriteOp {
    /// Plain `pwrite` at `offset`.
    Write { offset: u64, bytes: Vec<u8> },
    /// Kernel-space `copy_file_range` from `(in_file, src_offset)` to
    /// `(out_fd, out_offset)` for `len` bytes.
    ///
    /// Holds the input as an owned handle rather than a `RawFd` so the
    /// descriptor cannot be closed - or recycled onto an unrelated file - by
    /// the producer while this op sits in a pool worker's queue, and with
    /// its path, so a failed read or splice names the file.
    CopyRange {
        out_offset: u64,
        in_file: crate::read::input::InputFile,
        src_offset: u64,
        len: u64,
    },
}

/// Writer-thread entry point. Opens the output file, writes the header
/// synchronously at offset 0, spawns `POOL_SIZE` workers, then loops
/// reading pipeline items in seq order and dispatching per-worker work.
///
/// We intentionally do not call `file.set_len(...)` anywhere on the
/// success path: workers `pwrite` at absolute offsets and the kernel
/// extends EOF to cover the highest offset written. On any worker or
/// dispatch error the first failure propagates to the caller - we never
/// claim success with an under-written file, so no truncation is required.
///
/// # Failure and thread lifetime
///
/// The pool runs inside `std::thread::scope`, and every worker is joined
/// explicitly before this returns, whatever went wrong - so no pool thread
/// outlives the call and nothing is still writing to `path` once the caller
/// sees the result. This used to return on the first panicked join, which
/// detached the rest of the pool: those workers kept draining their queues
/// into the output after `apply-changes` had already reported failure, and
/// a caller that deleted or reused the path raced them.
///
/// The first failure wins and is what surfaces: a worker's I/O error, a
/// worker or dispatcher panic (with its payload message), an upstream
/// error, or a stream gap. It is recorded in a shared
/// [`FirstFailure`](crate::write::pool_failure::FirstFailure), which also
/// stops the rest of the pool: workers discard the ops still queued to them
/// instead of writing them, and the dispatcher stops dispatching. See that
/// module for the record-before-disconnect ordering that keeps a consequence
/// ("worker channel closed") from outranking its cause.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn parallel_writer_thread(
    rx: Receiver<PipelineItem>,
    path: PathBuf,
    framed_header: Vec<u8>,
    init_tx: SyncSender<Result<()>>,
    expected: &SeqTotal,
    finish: super::Finish,
    cancel: &crate::host::Cancel,
) -> Result<()> {
    // A failure is not logged here: it is the thread's return value, which the
    // writer's `flush` (or the failed send that noticed the thread was gone)
    // hands to the caller, so the operator sees it exactly once.
    parallel_writer_thread_inner(rx, path, framed_header, init_tx, expected, finish, cancel)
}

#[allow(clippy::needless_pass_by_value)]
fn parallel_writer_thread_inner(
    rx: Receiver<PipelineItem>,
    path: PathBuf,
    framed_header: Vec<u8>,
    init_tx: SyncSender<Result<()>>,
    expected: &SeqTotal,
    finish: super::Finish,
    cancel: &crate::host::Cancel,
) -> Result<()> {
    let file = match super::create_backend_file(&path) {
        Ok(f) => f,
        Err(e) => {
            // The same typed error goes to the constructor waiting on init
            // and out as the thread's result.
            let error = Error::from(e).context(format!("create {}", path.display()));
            init_tx.send(Err(error.clone())).ok();
            return Err(error);
        }
    };

    // Write the header at offset 0. Everything after grows from
    // `framed_header.len()`.
    if let Err(e) = file.write_all_at(&framed_header, 0) {
        let error = Error::from(e).context("header write");
        init_tx.send(Err(error.clone())).ok();
        return Err(error);
    }

    // Signal init success so the constructor can return.
    init_tx.send(Ok(())).ok();

    let failure = FirstFailure::new();
    let header_len = framed_header.len() as u64;
    std::thread::scope(|s| run_pool(s, &rx, &file, &failure, header_len, expected, cancel));

    // Every pool thread has been joined; the record is final.
    if let Some(e) = failure.take() {
        return Err(e.into());
    }

    if finish == super::Finish::Durable {
        // A path-constructor writer's own durability.
        crate::write::metrics::with_sync_observer(cancel, || WRITER_METRICS.sync_recorded(&file))?;
    }

    Ok(())
}

/// Spawn the pool inside `s`, run the dispatcher on this thread, then close
/// the pool channels and join every worker. Every failure - spawn, dispatch,
/// dispatcher panic, worker error or panic - lands in `failure`; nothing
/// returns early past the join loop.
fn run_pool<'scope, 'env>(
    s: &'scope Scope<'scope, 'env>,
    rx: &Receiver<PipelineItem>,
    file: &'scope File,
    failure: &'scope FirstFailure,
    header_len: u64,
    expected: &SeqTotal,
    cancel: &'scope crate::host::Cancel,
) {
    let mut worker_txs: Vec<SyncSender<WriteOp>> = Vec::with_capacity(POOL_SIZE);
    let mut worker_handles: Vec<ScopedJoinHandle<'scope, ()>> = Vec::with_capacity(POOL_SIZE);
    for index in 0..POOL_SIZE {
        let (wtx, wrx) = sync_channel::<WriteOp>(PER_WORKER_CAPACITY);
        match std::thread::Builder::new()
            .spawn_scoped(s, move || pool_worker(index, wrx, file, failure, cancel))
        {
            Ok(handle) => {
                worker_txs.push(wtx);
                worker_handles.push(handle);
            }
            Err(e) => {
                failure.record(
                    Error::from(e).context(format!("spawn parallel writer pool worker {index}")),
                );
                break;
            }
        }
    }

    if !failure.stopped() {
        // A dispatcher panic is caught so its payload is recorded (and the
        // workers stopped) before the join loop, instead of unwinding past
        // it: `scope` would still join the workers, but only after they had
        // drained their queues into the file, and a worker failure recorded
        // earlier would be lost to the re-raised dispatcher panic.
        let dispatched = std::panic::catch_unwind(AssertUnwindSafe(|| {
            dispatch_loop(rx, &worker_txs, failure, header_len, expected)
        }));
        match dispatched {
            Ok(Ok(())) => {}
            Ok(Err(e)) => failure.record(e),
            Err(payload) => failure.record_panic("parallel writer dispatcher", payload.as_ref()),
        }
    }

    // Close pool channels to signal end of input, then join EVERY worker.
    // No early return: a worker left unjoined would still be writing to the
    // output after the caller had been handed the result.
    drop(worker_txs);
    for (index, handle) in worker_handles.into_iter().enumerate() {
        #[cfg(feature = "test-hooks")]
        cancel.faults().before_pool_join(index);
        #[cfg(not(feature = "test-hooks"))]
        let _ = index;
        // `pool_worker` catches its own panics, so this is a backstop for a
        // panic outside that catch (in the payload's own destructor, say).
        if let Err(payload) = handle.join() {
            failure.record_panic("parallel writer pool worker", payload.as_ref());
        }
    }
}

/// Main dispatch loop: pop pipeline items in seq order, assign offsets,
/// round-robin WriteOps across the pool.
///
/// Returns as soon as `failure` reports a stopped pool. Any error it
/// returns is recorded by the caller; when it is a consequence of a pool
/// failure (a closed worker channel, a stop) the cause is already recorded
/// and outranks it.
fn dispatch_loop(
    rx: &Receiver<PipelineItem>,
    worker_txs: &[SyncSender<WriteOp>],
    failure: &FirstFailure,
    header_len: u64,
    expected: &SeqTotal,
) -> Result<()> {
    let mut pending: ReorderBuffer<Result<OutputChunk>> = ReorderBuffer::with_capacity(WRITE_AHEAD);
    let mut current_offset: u64 = header_len;
    let mut next_worker: usize = 0;

    loop {
        // One Acquire load per pipeline item. A worker failure is otherwise
        // only seen when the next send to that worker fails, up to a full
        // round-robin pass later.
        if failure.stopped() {
            return Err(Error::internal(
                "parallel writer stopped after a pool failure",
            ));
        }
        // Same `writer_recv_wait_ns` accounting as the pipelined and io_uring
        // writer loops. This backend used to omit it, so a parallel-backend
        // run reported as never having waited on its producer - the same
        // one-backend-only counter shape as the `sync_all_calls` gap.
        let t_recv = crate::counters::Stopwatch::start();
        let item = match rx.recv() {
            Ok(item) => item,
            Err(_) => break,
        };
        WRITER_METRICS.record_recv_wait(t_recv);
        pending.push(item.seq, item.data);
        WRITER_METRICS.record_reorder_high_water(pending.pending_len());

        while let Some(result) = pending.pop_ready() {
            let chunk = result?;
            dispatch_chunk(chunk, worker_txs, &mut current_offset, &mut next_worker)?;
        }
    }
    // Channel closed cleanly. That proves nothing about completeness on its
    // own: an upstream framer that died took its `tx` clone with it, so the
    // close looks identical either way. Returning `Ok(())` without checking
    // would claim success for a truncated file - and the check has to cover
    // BOTH a gap that stranded later items and a gap at the tail that
    // stranded nothing. See `check_stream_complete`.
    check_stream_complete(
        "parallel writer",
        pending.pending_len(),
        pending.next_seq(),
        expected,
    )
}

/// Dispatch one `OutputChunk` to the pool. Handles each variant,
/// updates `current_offset`, and rotates `next_worker`.
fn dispatch_chunk(
    chunk: OutputChunk,
    worker_txs: &[SyncSender<WriteOp>],
    current_offset: &mut u64,
    next_worker: &mut usize,
) -> Result<()> {
    match chunk {
        OutputChunk::Framed(parts) => {
            let bytes = parts.into_vec();
            let len = bytes.len() as u64;
            send_to_worker(
                worker_txs,
                next_worker,
                WriteOp::Write {
                    offset: *current_offset,
                    bytes,
                },
            )?;
            *current_offset += len;
            WRITER_METRICS
                .bytes_written
                .fetch_add(len, Ordering::Relaxed);
        }
        OutputChunk::Raw(bytes) => {
            let len = bytes.len() as u64;
            send_to_worker(
                worker_txs,
                next_worker,
                WriteOp::Write {
                    offset: *current_offset,
                    bytes,
                },
            )?;
            *current_offset += len;
            WRITER_METRICS
                .bytes_written
                .fetch_add(len, Ordering::Relaxed);
        }
        OutputChunk::RawChunks(chunks) => {
            for c in chunks {
                let len = c.len() as u64;
                send_to_worker(
                    worker_txs,
                    next_worker,
                    WriteOp::Write {
                        offset: *current_offset,
                        bytes: c,
                    },
                )?;
                *current_offset += len;
                WRITER_METRICS
                    .bytes_written
                    .fetch_add(len, Ordering::Relaxed);
            }
        }
        OutputChunk::CopyRange {
            in_file,
            offset: src_offset,
            len,
        } => {
            send_to_worker(
                worker_txs,
                next_worker,
                WriteOp::CopyRange {
                    out_offset: *current_offset,
                    in_file,
                    src_offset,
                    len,
                },
            )?;
            *current_offset += len;
            WRITER_METRICS
                .bytes_written
                .fetch_add(len, Ordering::Relaxed);
        }
    }
    Ok(())
}

/// Send `op` to `worker_txs[*next_worker]`, then advance round-robin.
///
/// A closed channel means that worker exited, which it only does after
/// recording its failure (or on a stop, which also implies a recorded
/// failure). The error returned here is the consequence; the caller
/// records it and the cause, already recorded, outranks it.
///
/// Deliberately does NOT reroute a dead worker's `op` to a healthy
/// worker: each `op` carries the absolute file offset it must pwrite
/// to, and earlier `op`s routed to the now-dead worker cover the
/// offsets immediately before this one. Rerouting would leave a hole
/// in the file at those earlier offsets. Surfacing the worker's
/// captured error is the correct response.
fn send_to_worker(
    worker_txs: &[SyncSender<WriteOp>],
    next_worker: &mut usize,
    op: WriteOp,
) -> Result<()> {
    if worker_txs[*next_worker].send(op).is_err() {
        return Err(Error::internal("parallel writer worker channel closed"));
    }
    *next_worker = (*next_worker + 1) % worker_txs.len();
    Ok(())
}

/// Body of one pool thread: run [`worker_loop`] with its panics caught.
///
/// The panic is recorded BEFORE `rx` is dropped, so the dispatcher can only
/// see this worker's channel close after the cause is on record - otherwise
/// its "worker channel closed" could be recorded first and mask the panic.
fn pool_worker(
    index: usize,
    rx: Receiver<WriteOp>,
    file: &File,
    failure: &FirstFailure,
    cancel: &crate::host::Cancel,
) {
    #[cfg(feature = "test-hooks")]
    let _live = cancel.faults().live_pool_worker();
    let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
        worker_loop(index, &rx, file, failure, cancel);
    }));
    if let Err(payload) = caught {
        failure.record_panic("parallel writer pool worker", payload.as_ref());
    }
    drop(rx);
}

/// Pool worker: pop `WriteOp`s until channel closes, executing each
/// against the shared file. On error, record it in `failure` (first
/// failure wins) and exit. Once the pool is stopped, exits without running
/// the op it just popped; the ops still queued are dropped with `rx`.
fn worker_loop(
    index: usize,
    rx: &Receiver<WriteOp>,
    file: &File,
    failure: &FirstFailure,
    cancel: &crate::host::Cancel,
) {
    #[cfg(not(feature = "test-hooks"))]
    let _ = index;
    while let Ok(op) = rx.recv() {
        // One Acquire load per op - next to two Relaxed RMWs on the shared
        // `WRITER_METRICS` and a multi-hundred-KB pwrite.
        if failure.stopped() {
            return;
        }

        // Test-only: the run's fault plan (`crate::test_hooks`). Simulates
        // an OOM or internal unwrap inside a pool worker, and parks siblings
        // for the join handshake. Release builds compile this out entirely.
        #[cfg(feature = "test-hooks")]
        cancel.faults().on_pool_op(index);

        let t = crate::counters::Stopwatch::start();
        let result = match op {
            WriteOp::Write { offset, bytes } => {
                file.write_all_at(&bytes, offset).map_err(Error::from)
            }
            WriteOp::CopyRange {
                out_offset,
                in_file,
                src_offset,
                len,
            } => {
                // Explicit output offset: the fd's position is never touched,
                // so the pool's concurrent copies and writes cannot race on
                // it, and a refused copy falls back to `pwrite`s at the same
                // offset.
                let r = super::copy_range::copy_range(
                    &in_file,
                    super::copy_range::CopyOut::At(file, out_offset),
                    src_offset,
                    len,
                    cancel,
                );
                // The Arc is held across the splice, so the input cannot be
                // closed out from under the kernel copy.
                drop(in_file);
                r
            }
        };
        t.record_ns(&WRITER_METRICS.write_ns);
        if let Err(e) = result {
            failure.record(e);
            return;
        }
    }
}

/// The pool's `Write` path, end to end, in every feature configuration.
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn parallel_writer_basic() -> Result<()> {
        let dir = crate::path_guard::tests::scratch();
        let out = dir.path().join("out.bin");

        let (tx, rx) = sync_channel::<PipelineItem>(WRITE_AHEAD);
        let (init_tx, init_rx) = sync_channel::<Result<()>>(1);
        let header = b"HDR".to_vec();

        let path = out.clone();
        let seq_total = SeqTotal::new();
        let thread_total = seq_total.clone();
        let handle = std::thread::spawn(move || {
            parallel_writer_thread(
                rx,
                path,
                header,
                init_tx,
                &thread_total,
                crate::write::Finish::Durable,
                &crate::host::Cancel::new(),
            )
        });

        // Wait for init.
        init_rx
            .recv()
            .map_err(|_| Error::internal("init channel closed"))??;

        // Send 16 items in seq order, each with a unique payload.
        for i in 0..16u32 {
            let payload = format!("item-{i:02}").into_bytes();
            tx.send(PipelineItem {
                seq: i as usize,
                data: Ok(OutputChunk::Raw(payload)),
            })
            .map_err(|_| Error::internal("send"))?;
        }
        seq_total.announce(16);
        drop(tx);

        handle
            .join()
            .map_err(|_| Error::internal("writer thread panicked"))??;

        let mut actual = Vec::new();
        std::fs::File::open(&out)?.read_to_end(&mut actual)?;

        let mut expected: Vec<u8> = b"HDR".to_vec();
        for i in 0..16u32 {
            expected.extend_from_slice(&format!("item-{i:02}").into_bytes());
        }
        assert_eq!(actual, expected);
        Ok(())
    }

    /// Regression: when the pipeline channel closes cleanly but the
    /// reorder buffer still holds items blocked on a missing earlier
    /// seq (an upstream framer panicked / was dropped), `dispatch_loop`
    /// must surface the gap as an error instead of returning `Ok(())`
    /// and producing a silently-truncated file.
    ///
    /// Simulates the failure mode by sending a single item with
    /// `seq=2` (no seq=0 or seq=1) and immediately dropping the
    /// sender. With `seq=2` alone, `pop_ready` cannot advance and
    /// the item sits in `pending` indefinitely; the channel-close
    /// `recv` error then drops us out of the outer loop.
    #[test]
    fn dispatch_loop_surfaces_gap_at_channel_close() {
        let (tx, rx) = sync_channel::<PipelineItem>(4);
        tx.send(PipelineItem {
            seq: 2,
            data: Ok(OutputChunk::Raw(vec![0u8; 8])),
        })
        .expect("send seq=2");
        drop(tx);

        // Empty worker_txs is fine: seq=2 can't pop (gap at 0,1) so
        // dispatch_chunk is never called.
        let worker_txs: Vec<SyncSender<WriteOp>> = Vec::new();
        let failure = FirstFailure::new();

        let seq_total = SeqTotal::new();
        seq_total.announce(3);
        let result = dispatch_loop(&rx, &worker_txs, &failure, 0, &seq_total);
        let err = result.expect_err("gap at channel close must surface as Err");
        let msg = err.to_string();
        assert!(
            msg.contains("reorder buffer"),
            "error should name the reorder-buffer state, got: {msg}",
        );
    }

    /// The tail-gap shape the reorder buffer cannot see.
    ///
    /// `dispatch_loop_surfaces_gap_at_channel_close` covers an INTERIOR gap,
    /// where the surviving items pile up behind the missing seq and
    /// `pending_len` is non-zero. If the FINAL framer dies instead, seq 0
    /// pops cleanly, the buffer drains to empty, and the channel closes with
    /// nothing stranded - so `pending_len` alone reports a short file as a
    /// complete one. Only the announced total catches it.
    #[test]
    fn dispatch_loop_surfaces_a_missing_tail_with_an_empty_reorder_buffer() {
        let (worker_tx, worker_rx) = sync_channel::<WriteOp>(4);
        let (tx, rx) = sync_channel::<PipelineItem>(4);
        tx.send(PipelineItem {
            seq: 0,
            data: Ok(OutputChunk::Raw(vec![0u8; 8])),
        })
        .expect("send seq=0");
        drop(tx);

        let worker_txs = vec![worker_tx];
        let failure = FirstFailure::new();
        let seq_total = SeqTotal::new();
        // Two blobs were dispatched upstream; only seq 0 ever arrived.
        seq_total.announce(2);

        let result = dispatch_loop(&rx, &worker_txs, &failure, 0, &seq_total);
        let msg = result
            .expect_err("a missing final seq is a short file, not a success")
            .to_string();
        assert!(
            msg.contains("after 1 of 2"),
            "error must name what arrived vs what was expected, got: {msg}",
        );
        drop(worker_rx);
    }

    /// Passing counterpart: the same one-item stream, correctly announced,
    /// must still succeed. Guards against a regression that fails every run.
    #[test]
    fn dispatch_loop_accepts_a_complete_stream() {
        let (worker_tx, worker_rx) = sync_channel::<WriteOp>(4);
        let (tx, rx) = sync_channel::<PipelineItem>(4);
        tx.send(PipelineItem {
            seq: 0,
            data: Ok(OutputChunk::Raw(vec![0u8; 8])),
        })
        .expect("send seq=0");
        drop(tx);

        let worker_txs = vec![worker_tx];
        let failure = FirstFailure::new();
        let seq_total = SeqTotal::new();
        seq_total.announce(1);

        dispatch_loop(&rx, &worker_txs, &failure, 0, &seq_total)
            .expect("a complete one-item stream is not a truncation");
        drop(worker_rx);
    }
}
