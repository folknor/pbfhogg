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
//!   workers, truncates to logical size, `sync_all`, returns.
//!
//! ## Status: scaffold (2026-04-21)
//!
//! Skeleton + worker loop shape committed behind
//! `#![allow(dead_code)]`. `to_path_parallel` / integration into
//! `PbfWriter` and CLI-flag exposure land in a follow-up commit after
//! correctness is sanity-checked on Denmark.

#![allow(dead_code)]

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
use crate::write::pipeline::{OutputChunk, PipelineItem, WRITE_AHEAD};

/// Number of worker threads doing `pwrite`s / `copy_file_range`s in
/// parallel. 4 is a reasonable default for NVMe (saturates queue depth
/// without over-contending for the device's internal parallelism).
/// Could become a CLI flag later.
pub(crate) const POOL_SIZE: usize = 4;

/// Per-worker channel capacity. Kept small so the writer thread's
/// dispatch doesn't outrun the pool; each WriteOp holds a full owned
/// `Vec<u8>` (~800 KB for the rewrite path), so 8 × 4 workers ×
/// 800 KB ≈ 26 MB buffered across the pool - comfortably under the
/// pipeline's existing memory budget.
pub(crate) const PER_WORKER_CAPACITY: usize = 8;

/// One unit of work dispatched from the writer-thread to the pool.
/// Carries the pre-computed output offset; workers do not compute or
/// coordinate offsets among themselves.
enum WriteOp {
    /// Plain `pwrite` at `offset`.
    Write { offset: u64, bytes: Vec<u8> },
    /// Kernel-space `copy_file_range` from `(in_fd, src_offset)` to
    /// `(out_fd, out_offset)` for `len` bytes.
    #[cfg(feature = "linux-direct-io")]
    CopyRange {
        out_offset: u64,
        in_fd: RawFd,
        src_offset: u64,
        len: u64,
    },
}

/// Shared error slot: the first worker thread to hit an I/O error
/// stores it here; the writer thread checks before returning success.
type ErrSlot = Arc<Mutex<Option<io::Error>>>;

/// Writer-thread entry point. Opens the output file, writes the header
/// synchronously at offset 0, spawns `POOL_SIZE` workers, then loops
/// reading pipeline items in seq order and dispatching per-worker work.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn parallel_writer_thread(
    rx: Receiver<PipelineItem>,
    path: PathBuf,
    framed_header: Vec<u8>,
    init_tx: SyncSender<io::Result<()>>,
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
        init_tx.send(Err(io::Error::new(e.kind(), format!("header write: {e}")))).ok();
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
        worker_handles.push(std::thread::spawn(move || worker_loop(wrx, &file, &err_slot)));
    }

    let result = dispatch_loop(
        &rx,
        &worker_txs,
        &err_slot,
        framed_header.len() as u64,
    );

    // Close pool channels to signal EOI; join workers.
    drop(worker_txs);
    for handle in worker_handles {
        if let Err(e) = handle.join() {
            return Err(io::Error::other(format!("parallel writer pool panicked: {e:?}")));
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
    WRITER_METRICS
        .sync_all_ns
        .fetch_add(
            u64::try_from(t_sync.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );

    Ok(())
}

/// Main dispatch loop: pop pipeline items in seq order, assign offsets,
/// round-robin WriteOps across the pool.
fn dispatch_loop(
    rx: &Receiver<PipelineItem>,
    worker_txs: &[SyncSender<WriteOp>],
    err_slot: &ErrSlot,
    header_len: u64,
) -> io::Result<()> {
    let mut pending: ReorderBuffer<io::Result<OutputChunk>> =
        ReorderBuffer::with_capacity(WRITE_AHEAD);
    let mut current_offset: u64 = header_len;
    let mut next_worker: usize = 0;

    while let Ok(item) = rx.recv() {
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
    Ok(())
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
                WriteOp::Write { offset: *current_offset, bytes },
                err_slot,
            )?;
            *current_offset += len;
            WRITER_METRICS.bytes_written.fetch_add(len, Ordering::Relaxed);
        }
        OutputChunk::Raw(bytes) => {
            let len = bytes.len() as u64;
            send_to_worker(
                worker_txs,
                next_worker,
                WriteOp::Write { offset: *current_offset, bytes },
                err_slot,
            )?;
            *current_offset += len;
            WRITER_METRICS.bytes_written.fetch_add(len, Ordering::Relaxed);
        }
        OutputChunk::RawChunks(chunks) => {
            for c in chunks {
                let len = c.len() as u64;
                send_to_worker(
                    worker_txs,
                    next_worker,
                    WriteOp::Write { offset: *current_offset, bytes: c },
                    err_slot,
                )?;
                *current_offset += len;
                WRITER_METRICS.bytes_written.fetch_add(len, Ordering::Relaxed);
            }
        }
        #[cfg(feature = "linux-direct-io")]
        OutputChunk::CopyRange { in_fd, offset: src_offset, len } => {
            send_to_worker(
                worker_txs,
                next_worker,
                WriteOp::CopyRange {
                    out_offset: *current_offset,
                    in_fd,
                    src_offset,
                    len,
                },
                err_slot,
            )?;
            *current_offset += len;
            WRITER_METRICS.bytes_written.fetch_add(len, Ordering::Relaxed);
        }
    }
    Ok(())
}

/// Send `op` to `worker_txs[*next_worker]`, then advance round-robin.
/// If the target worker's channel is closed (worker panicked or
/// returned), surface the first deferred error.
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
        let t = std::time::Instant::now();
        let result = match op {
            WriteOp::Write { offset, bytes } => file.write_all_at(&bytes, offset),
            #[cfg(feature = "linux-direct-io")]
            WriteOp::CopyRange { out_offset, in_fd, src_offset, len } => {
                copy_range_to_fd(file, in_fd, src_offset, out_offset, len)
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
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap, clippy::cast_sign_loss)]
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
            return Err(io::Error::last_os_error());
        }
        if ret == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "copy_file_range returned 0 before completing",
            ));
        }
        let advanced = u64::try_from(ret).map_err(|_| {
            io::Error::other("copy_file_range returned negative advance")
        })?;
        src_offset += advanced;
        out_offset += advanced;
        remaining -= advanced;
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
        let handle = std::thread::spawn(move || parallel_writer_thread(rx, path, header, init_tx));

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
}
