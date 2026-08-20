//! Parallel writer backend: `pwrite`-based output across a pool of
//! writer threads.
//!
//! Motivation (plan item #17): the single-threaded `FileOutputSink`
//! and `uring_writer` backends cap at ~1.5 GB/s on NVMe, far below the
//! ~5 GB/s sequential peak. At planet scale with ~95-120 GB of output
//! bytes this is the wall floor. Splitting the actual disk writes
//! across N threads (each `pwrite`ing at its assigned offset on a
//! shared file descriptor) should scale closer to NVMe peak.
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
use std::io;
use std::os::unix::fs::FileExt;
#[cfg(feature = "linux-direct-io")]
use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::reorder_buffer::ReorderBuffer;
use crate::write::metrics::WRITER_METRICS;
use crate::write::pipeline::{
    OutputChunk, PipelineItem, SeqTotal, WRITE_AHEAD, check_stream_complete,
};

/// Number of worker threads doing `pwrite`s / `copy_file_range`s in
/// parallel. 16 is the empirical planet-bench value (commit `80b37dfb`,
/// 2026-04-21, raising it from 4); the smaller settings leave disk
/// bandwidth on the table. Could become a CLI flag later.
///
/// The doc here used to justify 8 while the constant read 16, because
/// that commit changed the number and rewrote the prose around a value
/// it did not use.
pub(crate) const POOL_SIZE: usize = 16;

/// Per-worker channel capacity. Kept small so the writer thread's
/// dispatch doesn't outrun the pool; each WriteOp holds a full owned
/// `Vec<u8>` (~800 KB for the rewrite path), so
/// `PER_WORKER_CAPACITY` × `POOL_SIZE` × 800 KB ≈ 100 MB buffered
/// across the pool at the current 8 × 16 - within the pipeline's
/// existing memory budget, but note it scales with `POOL_SIZE`.
pub(crate) const PER_WORKER_CAPACITY: usize = 8;

/// One unit of work dispatched from the writer-thread to the pool.
/// Carries the pre-computed output offset; workers do not compute or
/// coordinate offsets among themselves.
enum WriteOp {
    /// Plain `pwrite` at `offset`.
    Write { offset: u64, bytes: Vec<u8> },
    /// Kernel-space `copy_file_range` from `(in_file, src_offset)` to
    /// `(out_fd, out_offset)` for `len` bytes.
    ///
    /// Holds the input as an `Arc<File>` rather than a `RawFd` so the
    /// descriptor cannot be closed - or recycled onto an unrelated file - by
    /// the producer while this op sits in a pool worker's queue.
    #[cfg(feature = "linux-direct-io")]
    CopyRange {
        out_offset: u64,
        in_file: Arc<std::fs::File>,
        src_offset: u64,
        len: u64,
    },
}

/// Shared error slot: the first worker thread to hit an I/O error
/// stores it here; the writer thread checks before returning success.
type ErrSlot = Arc<Mutex<Option<io::Error>>>;

// Fault-injection hooks for tests. Gated behind the `test-hooks`
// Cargo feature; release builds don't compile this module at all.
// Plumbed via process-global atomics rather than a per-instance
// config because pool workers are spawned deep inside
// `parallel_writer_thread_inner` - threading a config field through
// every entry point (PbfWriter::to_path_parallel, merge(), etc.)
// would be invasive. Tests serialize access via `--test-threads=1`
// (enforced by `brokkr test`).
#[cfg(feature = "test-hooks")]
pub mod test_hooks {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Pool op count at which the acting worker panics. `u64::MAX` =
    /// disarmed (default). Set to a target count before invoking the
    /// pipeline; reset via [`reset`] once the test has finished so
    /// sibling tests don't inherit the hook.
    pub static PANIC_AT_POOL_OP_COUNT: AtomicU64 = AtomicU64::new(u64::MAX);

    /// Monotonic total of `WriteOp`s dispatched across the pool. Each
    /// worker does a `fetch_add(1)` immediately after popping an op;
    /// when the resulting (post-increment) count equals
    /// `PANIC_AT_POOL_OP_COUNT`, that worker panics.
    pub static POOL_OP_COUNT: AtomicU64 = AtomicU64::new(0);

    /// Disarm and zero the counters. Call at the start AND end of any
    /// test that touches these hooks.
    pub fn reset() {
        PANIC_AT_POOL_OP_COUNT.store(u64::MAX, Ordering::Relaxed);
        POOL_OP_COUNT.store(0, Ordering::Relaxed);
    }
}

/// Writer-thread entry point. Opens the output file, writes the header
/// synchronously at offset 0, spawns `POOL_SIZE` workers, then loops
/// reading pipeline items in seq order and dispatching per-worker work.
///
/// We intentionally do not call `file.set_len(...)` anywhere on the
/// success path: workers `pwrite` at absolute offsets and the kernel
/// extends EOF to cover the highest offset written. On any worker or
/// dispatch error, `result?` / `err_slot` propagate a failure to the
/// caller - we never claim success with an under-written file, so no
/// truncation is required.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn parallel_writer_thread(
    rx: Receiver<PipelineItem>,
    path: PathBuf,
    framed_header: Vec<u8>,
    init_tx: SyncSender<io::Result<()>>,
    expected: &SeqTotal,
) -> io::Result<()> {
    let result = parallel_writer_thread_inner(rx, path, framed_header, init_tx, expected);
    if let Err(ref e) = result {
        eprintln!("[parallel_writer] thread exit: {e}");
    }
    result
}

#[allow(clippy::needless_pass_by_value)]
fn parallel_writer_thread_inner(
    rx: Receiver<PipelineItem>,
    path: PathBuf,
    framed_header: Vec<u8>,
    init_tx: SyncSender<io::Result<()>>,
    expected: &SeqTotal,
) -> io::Result<()> {
    let file = match File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            let sent = io::Error::new(e.kind(), format!("create {}: {e}", path.display()));
            init_tx.send(Err(sent)).ok();
            return Err(e);
        }
    };

    // Write the header at offset 0. Everything after grows from
    // `framed_header.len()`.
    if let Err(e) = file.write_all_at(&framed_header, 0) {
        init_tx
            .send(Err(io::Error::new(e.kind(), format!("header write: {e}"))))
            .ok();
        return Err(e);
    }

    // Signal init success so the constructor can return.
    init_tx.send(Ok(())).ok();

    let shared_file = Arc::new(file);
    let err_slot: ErrSlot = Arc::new(Mutex::new(None));

    // Spawn the pool.
    let mut worker_txs: Vec<SyncSender<WriteOp>> = Vec::with_capacity(POOL_SIZE);
    let mut worker_handles: Vec<JoinHandle<()>> = Vec::with_capacity(POOL_SIZE);
    for _ in 0..POOL_SIZE {
        let (wtx, wrx) = sync_channel::<WriteOp>(PER_WORKER_CAPACITY);
        worker_txs.push(wtx);
        let file = Arc::clone(&shared_file);
        let err_slot = Arc::clone(&err_slot);
        worker_handles.push(std::thread::spawn(move || {
            worker_loop(wrx, &file, &err_slot);
        }));
    }

    let result = dispatch_loop(
        &rx,
        &worker_txs,
        &err_slot,
        framed_header.len() as u64,
        expected,
    );

    // Close pool channels to signal EOI; join workers.
    drop(worker_txs);
    for handle in worker_handles {
        if let Err(e) = handle.join() {
            return Err(io::Error::other(format!(
                "parallel writer pool panicked: {e:?}"
            )));
        }
    }

    result?;
    // Check for deferred worker errors.
    if let Some(e) = err_slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        return Err(e);
    }

    let t_sync = std::time::Instant::now();
    shared_file.sync_all()?;
    WRITER_METRICS.record_sync_all(t_sync);

    Ok(())
}

/// Main dispatch loop: pop pipeline items in seq order, assign offsets,
/// round-robin WriteOps across the pool.
fn dispatch_loop(
    rx: &Receiver<PipelineItem>,
    worker_txs: &[SyncSender<WriteOp>],
    err_slot: &ErrSlot,
    header_len: u64,
    expected: &SeqTotal,
) -> io::Result<()> {
    let mut pending: ReorderBuffer<io::Result<OutputChunk>> =
        ReorderBuffer::with_capacity(WRITE_AHEAD);
    let mut current_offset: u64 = header_len;
    let mut next_worker: usize = 0;

    loop {
        // Same `writer_recv_wait_ns` accounting as the pipelined and io_uring
        // writer loops. This backend used to omit it, so a parallel-backend
        // run reported as never having waited on its producer - the same
        // one-backend-only counter shape as the `sync_all_calls` gap.
        let t_recv = std::time::Instant::now();
        let item = match rx.recv() {
            Ok(item) => item,
            Err(_) => break,
        };
        WRITER_METRICS.record_recv_wait(t_recv);
        pending.push(item.seq, item.data);
        WRITER_METRICS.record_reorder_high_water(pending.pending_len());

        while let Some(result) = pending.pop_ready() {
            let chunk = result?;
            dispatch_chunk(
                chunk,
                worker_txs,
                &mut current_offset,
                &mut next_worker,
                err_slot,
            )?;
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
    err_slot: &ErrSlot,
) -> io::Result<()> {
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
                err_slot,
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
                err_slot,
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
                    err_slot,
                )?;
                *current_offset += len;
                WRITER_METRICS
                    .bytes_written
                    .fetch_add(len, Ordering::Relaxed);
            }
        }
        #[cfg(feature = "linux-direct-io")]
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
                err_slot,
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
/// If the target worker's channel is closed (worker panicked or
/// returned), surface the first deferred error.
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
    err_slot: &ErrSlot,
) -> io::Result<()> {
    if worker_txs[*next_worker].send(op).is_err() {
        // Worker exited; surface any captured error.
        if let Some(e) = err_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            return Err(e);
        }
        return Err(io::Error::other("parallel writer worker channel closed"));
    }
    *next_worker = (*next_worker + 1) % worker_txs.len();
    Ok(())
}

/// Pool worker: pop `WriteOp`s until channel closes, executing each
/// against the shared file. On error, store in `err_slot` (first
/// writer wins) and exit.
#[allow(clippy::needless_pass_by_value)]
fn worker_loop(rx: Receiver<WriteOp>, file: &File, err_slot: &ErrSlot) {
    loop {
        let op = match rx.recv() {
            Ok(op) => op,
            Err(_) => return,
        };

        // Test-only: arm via `test_hooks::PANIC_AT_POOL_OP_COUNT`.
        // Fires when the (post-increment) op count across the pool
        // matches; simulates an OOM or internal unwrap inside a pool
        // worker. Release builds compile this block out entirely.
        #[cfg(feature = "test-hooks")]
        {
            let n = test_hooks::POOL_OP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            if n == test_hooks::PANIC_AT_POOL_OP_COUNT.load(Ordering::Relaxed) {
                panic!("test-hooks: parallel_writer pool op #{n} panicking");
            }
        }

        let t = std::time::Instant::now();
        let result = match op {
            WriteOp::Write { offset, bytes } => file.write_all_at(&bytes, offset),
            #[cfg(feature = "linux-direct-io")]
            WriteOp::CopyRange {
                out_offset,
                in_file,
                src_offset,
                len,
            } => {
                use std::os::unix::io::AsRawFd;
                let r = copy_range_to_fd(file, in_file.as_raw_fd(), src_offset, out_offset, len);
                // The Arc is held across the splice, so the input cannot be
                // closed out from under the kernel copy.
                drop(in_file);
                r
            }
        };
        WRITER_METRICS.write_ns.fetch_add(
            u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        if let Err(e) = result {
            let mut slot = err_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.is_none() {
                *slot = Some(e);
            }
            return;
        }
    }
}

/// `copy_file_range` with explicit out offset. Loops until `len` bytes
/// are copied, handling short returns per kernel semantics.
#[cfg(feature = "linux-direct-io")]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn copy_range_to_fd(
    out: &File,
    in_fd: RawFd,
    mut src_offset: u64,
    mut out_offset: u64,
    mut remaining: u64,
) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let out_fd = out.as_raw_fd();
    while remaining > 0 {
        let mut src_off_i64 = src_offset as i64;
        let mut out_off_i64 = out_offset as i64;
        let chunk_len = usize::try_from(remaining).unwrap_or(usize::MAX);
        let ret = unsafe {
            libc::copy_file_range(
                in_fd,
                &mut src_off_i64,
                out_fd,
                &mut out_off_i64,
                chunk_len,
                0,
            )
        };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EXDEV) {
                // Cross-device copy: kernel-side splice isn't available
                // between the source and target filesystems. Fall back
                // to pread → pwrite in userspace for the remaining
                // range, using explicit offsets so parallel workers
                // don't race on the output fd's position.
                return copy_range_fallback_pwrite(out, in_fd, src_offset, out_offset, remaining);
            }
            return Err(err);
        }
        if ret == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "copy_file_range returned 0 before completing",
            ));
        }
        let advanced = u64::try_from(ret)
            .map_err(|_| io::Error::other("copy_file_range returned negative advance"))?;
        src_offset += advanced;
        out_offset += advanced;
        remaining -= advanced;
    }
    Ok(())
}

/// Cross-device copy fallback using `pread` + `pwrite` with explicit
/// offsets. Safe to call concurrently from pool workers because
/// `pwrite` doesn't touch the fd's file position.
#[cfg(feature = "linux-direct-io")]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn copy_range_fallback_pwrite(
    out: &File,
    in_fd: RawFd,
    mut src_offset: u64,
    mut out_offset: u64,
    mut remaining: u64,
) -> io::Result<()> {
    let mut buf = vec![0u8; 256 * 1024];
    while remaining > 0 {
        let chunk = buf.len().min(remaining as usize);
        let n = loop {
            let ret =
                unsafe { libc::pread(in_fd, buf.as_mut_ptr().cast(), chunk, src_offset as i64) };
            if ret < 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(err);
            }
            break ret;
        };
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "pread returned 0 during cross-device copy",
            ));
        }
        let n_u = usize::try_from(n).map_err(|_| io::Error::other("pread returned negative"))?;
        out.write_all_at(&buf[..n_u], out_offset)?;
        src_offset += n_u as u64;
        out_offset += n_u as u64;
        remaining -= n_u as u64;
    }
    Ok(())
}

/// Regression test to keep the module compiling under consumer
/// profiles where `linux-direct-io` is off. Exercises only the
/// always-present `Write` branch.
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::TempDir;

    #[test]
    fn parallel_writer_basic() -> io::Result<()> {
        let dir = TempDir::new().map_err(io::Error::other)?;
        let out = dir.path().join("out.bin");

        let (tx, rx) = sync_channel::<PipelineItem>(WRITE_AHEAD);
        let (init_tx, init_rx) = sync_channel::<io::Result<()>>(1);
        let header = b"HDR".to_vec();

        let path = out.clone();
        let seq_total = SeqTotal::new();
        let thread_total = seq_total.clone();
        let handle = std::thread::spawn(move || {
            parallel_writer_thread(rx, path, header, init_tx, &thread_total)
        });

        // Wait for init.
        init_rx
            .recv()
            .map_err(|_| io::Error::other("init channel closed"))??;

        // Send 16 items in seq order, each with a unique payload.
        for i in 0..16u32 {
            let payload = format!("item-{i:02}").into_bytes();
            tx.send(PipelineItem {
                seq: i as usize,
                data: Ok(OutputChunk::Raw(payload)),
            })
            .map_err(|_| io::Error::other("send"))?;
        }
        seq_total.announce(16);
        drop(tx);

        handle
            .join()
            .map_err(|_| io::Error::other("writer thread panicked"))??;

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
        use std::sync::{Arc, Mutex};

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
        let err_slot: ErrSlot = Arc::new(Mutex::new(None));

        let seq_total = SeqTotal::new();
        seq_total.announce(3);
        let result = dispatch_loop(&rx, &worker_txs, &err_slot, 0, &seq_total);
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
        use std::sync::{Arc, Mutex};

        let (worker_tx, worker_rx) = sync_channel::<WriteOp>(4);
        let (tx, rx) = sync_channel::<PipelineItem>(4);
        tx.send(PipelineItem {
            seq: 0,
            data: Ok(OutputChunk::Raw(vec![0u8; 8])),
        })
        .expect("send seq=0");
        drop(tx);

        let worker_txs = vec![worker_tx];
        let err_slot: ErrSlot = Arc::new(Mutex::new(None));
        let seq_total = SeqTotal::new();
        // Two blobs were dispatched upstream; only seq 0 ever arrived.
        seq_total.announce(2);

        let result = dispatch_loop(&rx, &worker_txs, &err_slot, 0, &seq_total);
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
        use std::sync::{Arc, Mutex};

        let (worker_tx, worker_rx) = sync_channel::<WriteOp>(4);
        let (tx, rx) = sync_channel::<PipelineItem>(4);
        tx.send(PipelineItem {
            seq: 0,
            data: Ok(OutputChunk::Raw(vec![0u8; 8])),
        })
        .expect("send seq=0");
        drop(tx);

        let worker_txs = vec![worker_tx];
        let err_slot: ErrSlot = Arc::new(Mutex::new(None));
        let seq_total = SeqTotal::new();
        seq_total.announce(1);

        dispatch_loop(&rx, &worker_txs, &err_slot, 0, &seq_total)
            .expect("a complete one-item stream is not a truncation");
        drop(worker_rx);
    }
}
