//! Pipeline plumbing for [`PbfWriter`](super::writer::PbfWriter): ordered
//! output chunks, the sink trait, the bounded permit pool that caps in-flight
//! rayon dispatches, and the writer thread that reorders and drains results
//! to the underlying sink.
//!
//! All items here are internal wiring for the write path; nothing in this
//! module is part of the public library API.

use crate::reorder_buffer::ReorderBuffer;
use crate::write::file_writer::FileWriter;
use crate::write::metrics::WRITER_METRICS;
use std::io::{self, Write};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;

/// Maximum number of framed blobs in-flight before backpressure stalls senders.
pub(crate) const WRITE_AHEAD: usize = 32;

/// Maximum number of in-flight rayon dispatches in the pipelined writer.
///
/// Counting-semaphore cap that bounds how many uncompressed block `Vec<u8>`s
/// can be owned by rayon closures simultaneously (queued, being compressed,
/// or waiting on the bounded output channel). Without this, `rayon::spawn`'s
/// unbounded internal task queue grows without limit when the producer side
/// out-runs compression throughput - exactly what happened in commit
/// `e7219f0` on planet when parallel pass 1 / stage 2a / stage 2d started
/// emitting blocks faster than zlib:6 could drain them, killing pbfhogg via
/// OOM at 26 GB anon RSS.
///
/// Memory bound: `PIPELINE_DISPATCH_PERMITS × max_block_size`. At ~4 MB per
/// uncompressed block and 64 permits that's ~256 MB worst case, small
/// compared to the 2.79 GB stage-2b peak.
pub(crate) const PIPELINE_DISPATCH_PERMITS: usize = 64;

/// One unit of output the writer thread can consume, independent of backend.
///
/// `Framed` is the common case (a compressed `OSMData` blob). `Raw` and
/// `RawChunks` carry pre-framed bytes (passthrough). `CopyRange` asks the
/// backend to splice bytes from another fd via `copy_file_range`.
pub(crate) enum OutputChunk {
    /// Framed blob parts, not yet concatenated.
    Framed(FramedBlobParts),
    /// Pre-framed raw blob bytes.
    Raw(Vec<u8>),
    /// Multiple framed blob chunks, written sequentially. Avoids
    /// concatenating passthrough frames into a single Vec.
    RawChunks(Vec<Vec<u8>>),
    /// Kernel-space copy from input fd (avoids userspace copy for passthrough).
    #[cfg(feature = "linux-direct-io")]
    CopyRange {
        in_fd: std::os::unix::io::RawFd,
        offset: u64,
        len: u64,
    },
}

/// The three owned parts of a framed blob:
/// `4 B big-endian header_len | BlobHeader | Blob protobuf body`.
///
/// Kept split so backends that support scatter-gather I/O can issue one
/// `writev` without an intermediate concat, while backends that need a
/// single buffer flatten locally via [`into_vec`](Self::into_vec).
pub(crate) struct FramedBlobParts {
    pub(crate) prefix: [u8; 4],
    pub(crate) header: Vec<u8>,
    pub(crate) blob: Vec<u8>,
}

impl FramedBlobParts {
    pub(crate) fn total_len(&self) -> u64 {
        (self.prefix.len() + self.header.len() + self.blob.len()) as u64
    }

    /// Flatten the three parts into a single owned `Vec<u8>`.
    ///
    /// Compatibility escape hatch for callers that want one contiguous buffer.
    pub(crate) fn into_vec(self) -> Vec<u8> {
        let total_len = self.prefix.len() + self.header.len() + self.blob.len();
        let mut out = Vec::with_capacity(total_len);
        out.extend_from_slice(&self.prefix);
        out.extend_from_slice(&self.header);
        out.extend_from_slice(&self.blob);
        out
    }
}

/// A chunk with its sequence number, ready for the writer thread.
///
/// `data` is `io::Result<OutputChunk>` because framing runs on rayon workers
/// and may fail asynchronously - the error is propagated in order via the
/// reorder buffer so the writer thread surfaces it at the correct position.
pub(crate) struct PipelineItem {
    pub(crate) seq: usize,
    pub(crate) data: io::Result<OutputChunk>,
}

/// A sink that consumes ordered [`OutputChunk`]s and writes them to a backend.
///
/// Each backend is free to flatten, batch, or scatter-gather as it sees fit -
/// the pipeline machinery only cares about ordering and error propagation.
pub(crate) trait OutputSink {
    fn write_chunk(&mut self, chunk: OutputChunk) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
}

/// Default sink that wraps a [`FileWriter`] (buffered or O_DIRECT).
///
/// Flattens `Framed` chunks via [`FramedBlobParts::into_vec`] before handing
/// them to the underlying writer.
pub(crate) struct FileOutputSink {
    writer: FileWriter,
}

impl FileOutputSink {
    pub(crate) fn new(writer: FileWriter) -> Self {
        Self { writer }
    }
}

impl OutputSink for FileOutputSink {
    fn write_chunk(&mut self, chunk: OutputChunk) -> io::Result<()> {
        match chunk {
            OutputChunk::Framed(parts) => {
                let len = parts.total_len();
                let bytes = parts.into_vec();
                let t_write = std::time::Instant::now();
                self.writer.write_all(&bytes)?;
                WRITER_METRICS
                    .write_ns
                    .fetch_add(elapsed_ns_u64(t_write), Relaxed);
                WRITER_METRICS.bytes_written.fetch_add(len, Relaxed);
                Ok(())
            }
            OutputChunk::Raw(bytes) => {
                let len = bytes.len() as u64;
                let t_write = std::time::Instant::now();
                self.writer.write_all(&bytes)?;
                WRITER_METRICS
                    .write_ns
                    .fetch_add(elapsed_ns_u64(t_write), Relaxed);
                WRITER_METRICS.bytes_written.fetch_add(len, Relaxed);
                Ok(())
            }
            OutputChunk::RawChunks(chunks) => {
                let total_bytes: u64 = chunks.iter().map(|chunk| chunk.len() as u64).sum();
                let t_write = std::time::Instant::now();
                for chunk in &chunks {
                    self.writer.write_all(chunk)?;
                }
                WRITER_METRICS
                    .write_ns
                    .fetch_add(elapsed_ns_u64(t_write), Relaxed);
                WRITER_METRICS.bytes_written.fetch_add(total_bytes, Relaxed);
                Ok(())
            }
            #[cfg(feature = "linux-direct-io")]
            OutputChunk::CopyRange { in_fd, offset, len } => {
                let t_write = std::time::Instant::now();
                let out_fd = self.writer.flush_and_raw_fd()?.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "copy_file_range incompatible with O_DIRECT output",
                    )
                })?;
                super::copy_range::copy_range(in_fd, out_fd, offset, len)?;
                WRITER_METRICS
                    .write_ns
                    .fetch_add(elapsed_ns_u64(t_write), Relaxed);
                WRITER_METRICS.bytes_written.fetch_add(len, Relaxed);
                Ok(())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// Write pipeline state - active when using pipelined mode.
pub(super) struct WritePipeline {
    pub(super) tx: SyncSender<PipelineItem>,
    pub(super) seq: usize,
    pub(super) join_handle: Option<JoinHandle<io::Result<()>>>,
    /// Counting-semaphore permit pool bounding in-flight rayon dispatches.
    /// Main thread `recv()`s a permit before `rayon::spawn`; the rayon
    /// closure `send()`s a permit back when its work completes. See
    /// [`PIPELINE_DISPATCH_PERMITS`] for the memory-bound rationale.
    pub(super) permit_tx: SyncSender<()>,
    pub(super) permit_rx: Receiver<()>,
}

/// Build a fresh permit pool pre-filled with `PIPELINE_DISPATCH_PERMITS`
/// tokens. Called during pipelined-writer construction to seed the
/// counting semaphore.
pub(super) fn new_permit_pool() -> (SyncSender<()>, Receiver<()>) {
    let (permit_tx, permit_rx) = sync_channel::<()>(PIPELINE_DISPATCH_PERMITS);
    for _ in 0..PIPELINE_DISPATCH_PERMITS {
        // `send` on a fresh channel with capacity = N can't fail or block
        // for the first N sends. unwrap is sound here.
        permit_tx
            .send(())
            .expect("seeding permit pool on fresh channel");
    }
    (permit_tx, permit_rx)
}

pub(super) fn record_send_wait(send_start: std::time::Instant) {
    WRITER_METRICS
        .pipeline_send_wait_ns
        .fetch_add(elapsed_ns_u64(send_start), Relaxed);
}

pub(crate) fn elapsed_ns_u64(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// Writer thread: receives [`OutputChunk`]s and hands them to the sink in
/// sequence order.
///
/// Uses a shared sequence-number reorder buffer to handle out-of-order
/// arrivals from parallel rayon tasks. Framing errors produced on rayon
/// workers are propagated in order via `io::Result<OutputChunk>`.
#[allow(clippy::needless_pass_by_value)] // Thread entry point owns the sink moved into std::thread::spawn.
pub(super) fn writer_thread<S: OutputSink>(
    rx: std::sync::mpsc::Receiver<PipelineItem>,
    mut sink: S,
) -> io::Result<()> {
    let mut pending: ReorderBuffer<io::Result<OutputChunk>> =
        ReorderBuffer::with_capacity(WRITE_AHEAD);

    loop {
        let t_recv = std::time::Instant::now();
        let item = match rx.recv() {
            Ok(item) => item,
            Err(_) => break,
        };
        WRITER_METRICS
            .recv_wait_ns
            .fetch_add(elapsed_ns_u64(t_recv), Relaxed);
        pending.push(item.seq, item.data);
        WRITER_METRICS.record_reorder_high_water(pending.pending_len());

        // Drain consecutive ready items from the front.
        while let Some(result) = pending.pop_ready() {
            let chunk = result?;
            sink.write_chunk(chunk)?;
        }
    }

    sink.flush()?;
    Ok(())
}
