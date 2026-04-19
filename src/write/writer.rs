//! PBF file writer - blob framing and compression.
//!
//! Writes valid `.osm.pbf` files. The writer handles the low-level blob framing
//! (4-byte header length, BlobHeader, compressed Blob) and delegates block
//! construction to [`BlockBuilder`](crate::block_builder::BlockBuilder).
//!
//! # Pipelined mode
//!
//! [`to_path`](PbfWriter::to_path) creates a writer that compresses blobs in
//! parallel using rayon, with a dedicated writer thread that reorders results
//! back into sequence order. Raw passthrough blobs bypass compression entirely.

use crate::blob_meta;
#[cfg(feature = "linux-direct-io")]
use crate::write::copy_range::copy_range;
use crate::write::file_writer::FileWriter;
use crate::write::metrics::WRITER_METRICS;
use crate::reorder_buffer::ReorderBuffer;
use std::io::{self, Write};
use std::path::Path;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::atomic::Ordering::Relaxed;
use std::thread::JoinHandle;

// Compression type moved to the sibling `compression` module; re-exported
// here so the existing `crate::writer::Compression` path keeps resolving.
pub use crate::write::compression::{Compression, ParseCompressionError};

// Blob framing / encoding helpers live in the sibling `framing` module.
use super::framing::{
    encode_blob_body, encode_blob_header_into, frame_blob_into, FrameScratch, PIPELINE_SCRATCH,
};
pub(crate) use super::framing::{frame_blob, frame_blob_pipelined, reframe_raw_with_index};

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
                let out_fd = self
                    .writer
                    .flush_and_raw_fd()?
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::Unsupported,
                            "copy_file_range incompatible with O_DIRECT output",
                        )
                    })?;
                copy_range(in_fd, out_fd, offset, len)?;
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
struct WritePipeline {
    tx: SyncSender<PipelineItem>,
    seq: usize,
    join_handle: Option<JoinHandle<io::Result<()>>>,
    /// Counting-semaphore permit pool bounding in-flight rayon dispatches.
    /// Main thread `recv()`s a permit before `rayon::spawn`; the rayon
    /// closure `send()`s a permit back when its work completes. See
    /// [`PIPELINE_DISPATCH_PERMITS`] for the memory-bound rationale.
    permit_tx: SyncSender<()>,
    permit_rx: Receiver<()>,
}

/// Build a fresh permit pool pre-filled with `PIPELINE_DISPATCH_PERMITS`
/// tokens. Called during pipelined-writer construction to seed the
/// counting semaphore.
fn new_permit_pool() -> (SyncSender<()>, Receiver<()>) {
    let (permit_tx, permit_rx) = sync_channel::<()>(PIPELINE_DISPATCH_PERMITS);
    for _ in 0..PIPELINE_DISPATCH_PERMITS {
        // `send` on a fresh channel with capacity = N can't fail or block
        // for the first N sends. unwrap is sound here.
        permit_tx.send(()).expect("seeding permit pool on fresh channel");
    }
    (permit_tx, permit_rx)
}

fn record_send_wait(send_start: std::time::Instant) {
    WRITER_METRICS
        .pipeline_send_wait_ns
        .fetch_add(elapsed_ns_u64(send_start), Relaxed);
}

pub(super) fn elapsed_ns_u64(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// Writes PBF files as a sequence of framed, compressed blobs.
///
/// # Usage
///
/// 1. Call [`write_header`](Self::write_header) with a serialized `HeaderBlock`.
/// 2. Call [`write_primitive_block`](Self::write_primitive_block) for each data block.
/// 3. Call [`flush`](Self::flush) when done.
///
/// For merge passthrough, use [`write_raw`](Self::write_raw) to copy unmodified
/// blob bytes directly.
///
/// # Pipelined mode
///
/// Use [`to_path`](Self::to_path) for parallel compression.
/// The header is written eagerly in the constructor; subsequent
/// `write_primitive_block` calls dispatch compression to the rayon pool,
/// and a dedicated writer thread reorders and writes results in sequence.
// wontfix(type-generic-bounds): bound on struct documents intent; removing is breaking
pub struct PbfWriter<W: Write> {
    writer: Option<W>,
    compression: Compression,
    pipeline: Option<WritePipeline>,
    /// Scratch buffers for sync-mode blob framing (unused in pipelined mode).
    scratch: FrameScratch,
}

impl PbfWriter<FileWriter> {
    /// Create a pipelined `PbfWriter` that compresses blobs in parallel.
    ///
    /// Writes the OSMHeader blob synchronously, then spawns a writer thread.
    /// Subsequent [`write_primitive_block`](Self::write_primitive_block) calls
    /// dispatch compression to the rayon pool. Raw passthrough blobs
    /// ([`write_raw`](Self::write_raw)) are sent directly to the writer thread.
    ///
    /// Call [`flush`](Self::flush) when done to join the writer thread and
    /// propagate any I/O errors.
    pub fn to_path(
        path: &Path,
        compression: Compression,
        header_block_bytes: &[u8],
    ) -> io::Result<Self> {
        let writer = FileWriter::buffered(path)?;
        Self::start_pipeline(writer, compression, header_block_bytes)
    }

    /// Create a pipelined `PbfWriter` with `O_DIRECT` for page-cache-free writes.
    ///
    /// All writes bypass the kernel page cache, preventing cache pollution
    /// during planet-scale (80 GB+) PBF writes. Requires a filesystem that
    /// supports `O_DIRECT` (not tmpfs).
    #[cfg(feature = "linux-direct-io")]
    pub fn to_path_direct(
        path: &Path,
        compression: Compression,
        header_block_bytes: &[u8],
    ) -> io::Result<Self> {
        let writer = FileWriter::direct(path)?;
        Self::start_pipeline(writer, compression, header_block_bytes)
    }

    /// Create a pipelined `PbfWriter` that uses io_uring for output I/O.
    ///
    /// The writer thread uses `O_DIRECT` + io_uring `WriteFixed` with
    /// registered page-aligned buffers. This provides maximum throughput
    /// when the pipeline is I/O-bound (e.g. `Compression::None` on fast storage).
    ///
    /// Requires the `linux-io-uring` feature and Linux 5.1+.
    #[cfg(feature = "linux-io-uring")]
    pub fn to_path_uring(
        path: &Path,
        compression: Compression,
        header_block_bytes: &[u8],
    ) -> io::Result<Self> {
        use crate::write::uring_writer;

        let framed_header = frame_blob("OSMHeader", header_block_bytes, &compression, None)?;

        // Oneshot channel for init errors from the writer thread.
        let (init_tx, init_rx) = sync_channel(1);

        let (tx, rx) = sync_channel(WRITE_AHEAD);
        let path_owned = path.to_path_buf();
        let handle = std::thread::spawn(move || {
            uring_writer::uring_writer_thread(rx, path_owned, framed_header, init_tx)
        });

        // Wait for the writer thread to complete initialization.
        // If init fails, we get the error here before returning to the caller.
        match init_rx.recv() {
            Ok(Ok(())) => {} // init succeeded
            Ok(Err(e)) => {
                // Init failed. Join the thread to clean up.
                drop(tx);
                drop(handle.join());
                return Err(e);
            }
            Err(_) => {
                // Thread panicked or exited before sending init result.
                drop(tx);
                return Err(match handle.join() {
                    Ok(Ok(())) => io::Error::other("writer thread exited without init signal"),
                    Ok(Err(e)) => e,
                    Err(_) => io::Error::other("writer thread panicked during init"),
                });
            }
        }

        let (permit_tx, permit_rx) = new_permit_pool();
        Ok(PbfWriter {
            writer: None,
            compression,
            pipeline: Some(WritePipeline {
                tx,
                seq: 0,
                join_handle: Some(handle),
                permit_tx,
                permit_rx,
            }),
            scratch: FrameScratch::new(),
        })
    }

    /// Shared pipelined setup: write header, spawn writer thread.
    fn start_pipeline(
        mut writer: FileWriter,
        compression: Compression,
        header_block_bytes: &[u8],
    ) -> io::Result<Self> {
        // Write header synchronously before starting the pipeline.
        let framed_header = frame_blob("OSMHeader", header_block_bytes, &compression, None)?;
        writer.write_all(&framed_header)?;

        // Spawn the writer thread and hand it the writer wrapped in a sink.
        let (tx, rx) = sync_channel(WRITE_AHEAD);
        let handle = std::thread::spawn(move || writer_thread(rx, FileOutputSink::new(writer)));

        let (permit_tx, permit_rx) = new_permit_pool();
        Ok(PbfWriter {
            writer: None,
            compression,
            pipeline: Some(WritePipeline {
                tx,
                seq: 0,
                join_handle: Some(handle),
                permit_tx,
                permit_rx,
            }),
            scratch: FrameScratch::new(),
        })
    }
}

impl<W: Write> PbfWriter<W> {
    /// Create a new `PbfWriter` wrapping the given writer.
    ///
    /// If `writer` is backed by a file, callers should wrap it in
    /// `BufWriter::with_capacity(256 * 1024, file)` for best performance.
    /// PBF blobs are typically 16-64KB compressed, so the default 8KB
    /// `BufWriter` causes excessive write syscalls. See [`to_path`](Self::to_path)
    /// which applies this automatically.
    pub fn new(writer: W, compression: Compression) -> Self {
        PbfWriter {
            writer: Some(writer),
            compression,
            pipeline: None,
            scratch: FrameScratch::new(),
        }
    }

    /// Write the `OSMHeader` blob. Must be the first blob in the file.
    ///
    /// Not needed when using [`to_path`](Self::to_path), which writes the
    /// header in the constructor.
    ///
    /// `header_block_bytes` is a serialized `HeaderBlock` protobuf message,
    /// typically produced by [`HeaderBuilder::build`](crate::block_builder::HeaderBuilder::build).
    pub fn write_header(&mut self, header_block_bytes: &[u8]) -> io::Result<()> {
        self.write_blob("OSMHeader", header_block_bytes)
    }

    /// Write an `OSMData` blob from a serialized `PrimitiveBlock`.
    ///
    /// In pipelined mode, compression is dispatched to the rayon pool and
    /// this method returns immediately. Errors from compression or I/O are
    /// deferred until [`flush`](Self::flush).
    ///
    /// `block_bytes` is produced by [`BlockBuilder::take`](crate::block_builder::BlockBuilder::take).
    #[hotpath::measure]
    pub fn write_primitive_block(&mut self, block_bytes: &[u8]) -> io::Result<()> {
        if let Some(ref mut pipeline) = self.pipeline {
            // Acquire a dispatch permit before enqueuing new work on rayon.
            // This blocks the caller when `PIPELINE_DISPATCH_PERMITS` blocks
            // are already in flight, preventing rayon's internal task queue
            // from growing without bound (see `PIPELINE_DISPATCH_PERMITS`
            // doc comment for the planet-scale OOM story that motivated
            // this).
            let t_permit = std::time::Instant::now();
            pipeline.permit_rx.recv().map_err(|_| {
                io::Error::other("pipelined writer permit pool disconnected")
            })?;
            WRITER_METRICS
                .permit_wait_ns
                .fetch_add(elapsed_ns_u64(t_permit), Relaxed);
            let seq = pipeline.seq;
            pipeline.seq += 1;
            let compression = self.compression;
            let uncompressed = block_bytes.to_vec();
            let tx = pipeline.tx.clone();
            let permit_tx = pipeline.permit_tx.clone();
            rayon::spawn(move || {
                let indexdata = blob_meta::scan_block_ids(&uncompressed)
                    .map(|idx| idx.serialize());
                let tagdata = blob_meta::scan_block_tags(&uncompressed)
                    .map(|ti| ti.serialize());
                let result = PIPELINE_SCRATCH.with_borrow_mut(|scratch| {
                    frame_blob_into(
                        "OSMData",
                        &uncompressed,
                        &compression,
                        indexdata.as_ref().map(<[u8; 42]>::as_slice),
                        tagdata.as_deref(),
                        scratch,
                    )
                });
                if let Ok(ref parts) = result {
                    WRITER_METRICS.payload_framed_items.fetch_add(1, Relaxed);
                    WRITER_METRICS
                        .payload_framed_bytes
                        .fetch_add(parts.total_len(), Relaxed);
                }
                let t_send = std::time::Instant::now();
                drop(tx.send(PipelineItem {
                    seq,
                    data: result.map(OutputChunk::Framed),
                }));
                record_send_wait(t_send);
                // Release the permit so the main thread can dispatch more
                // work. Must happen AFTER `tx.send` above so the in-flight
                // count stays correct while the result is waiting in the
                // writer channel.
                // Failure here means the main thread already dropped its
                // receiver (shutting down); safe to ignore.
                permit_tx.send(()).ok();
            });
            Ok(())
        } else {
            let indexdata = blob_meta::scan_block_ids(block_bytes)
                .map(|idx| idx.serialize());
            let tagdata = blob_meta::scan_block_tags(block_bytes)
                .map(|ti| ti.serialize());
            self.write_framed_blob(
                "OSMData",
                block_bytes,
                indexdata.as_ref().map(<[u8; 42]>::as_slice),
                tagdata.as_deref(),
            )
        }
    }

    /// Write an `OSMData` blob, taking ownership of the serialized bytes.
    ///
    /// Like [`write_primitive_block`](Self::write_primitive_block) but moves
    /// the `Vec` into the pipeline closure instead of copying, and uses a
    /// pre-computed [`BlobIndex`](crate::blob_meta::BlobIndex) and optional
    /// pre-serialized tagdata from
    /// [`BlockBuilder::take_owned`](crate::block_builder::BlockBuilder::take_owned)
    /// instead of rescanning the serialized bytes.
    #[hotpath::measure]
    pub(crate) fn write_primitive_block_owned(
        &mut self,
        block_bytes: Vec<u8>,
        index: blob_meta::BlobIndex,
        tagdata: Option<&[u8]>,
    ) -> io::Result<()> {
        self.write_primitive_block_owned_inner(block_bytes, index, tagdata, None)
    }

    /// Pool-aware variant of [`write_primitive_block_owned`](Self::write_primitive_block_owned).
    ///
    /// After the framing closure has consumed `block_bytes` (it is cloned
    /// into `FramedBlobParts`), the original `Vec<u8>` is returned to the
    /// pool at closure exit instead of being dropped. Pass the same pool
    /// the caller pulled the buffer from (typically via
    /// `BlockBuilder::take_owned_swap`).
    #[hotpath::measure]
    pub(crate) fn write_primitive_block_owned_pooled(
        &mut self,
        block_bytes: Vec<u8>,
        index: blob_meta::BlobIndex,
        tagdata: Option<&[u8]>,
        pool: std::sync::Arc<crate::write::buf_pool::BlockBufPool>,
    ) -> io::Result<()> {
        self.write_primitive_block_owned_inner(block_bytes, index, tagdata, Some(pool))
    }

    fn write_primitive_block_owned_inner(
        &mut self,
        block_bytes: Vec<u8>,
        index: blob_meta::BlobIndex,
        tagdata: Option<&[u8]>,
        pool: Option<std::sync::Arc<crate::write::buf_pool::BlockBufPool>>,
    ) -> io::Result<()> {
        let indexdata = index.serialize();
        if let Some(ref mut pipeline) = self.pipeline {
            // Bound in-flight rayon dispatches - see the sibling
            // `write_primitive_block` above and the
            // `PIPELINE_DISPATCH_PERMITS` doc comment for why.
            let t_permit = std::time::Instant::now();
            pipeline.permit_rx.recv().map_err(|_| {
                io::Error::other("pipelined writer permit pool disconnected")
            })?;
            WRITER_METRICS
                .permit_wait_ns
                .fetch_add(elapsed_ns_u64(t_permit), Relaxed);
            let seq = pipeline.seq;
            pipeline.seq += 1;
            let compression = self.compression;
            let tx = pipeline.tx.clone();
            let tagdata_owned = tagdata.map(<[u8]>::to_vec);
            let permit_tx = pipeline.permit_tx.clone();
            rayon::spawn(move || {
                let result = PIPELINE_SCRATCH.with_borrow_mut(|scratch| {
                    frame_blob_into(
                        "OSMData",
                        &block_bytes,
                        &compression,
                        Some(indexdata.as_slice()),
                        tagdata_owned.as_deref(),
                        scratch,
                    )
                });
                if let Ok(ref parts) = result {
                    WRITER_METRICS.payload_framed_items.fetch_add(1, Relaxed);
                    WRITER_METRICS
                        .payload_framed_bytes
                        .fetch_add(parts.total_len(), Relaxed);
                }
                let t_send = std::time::Instant::now();
                drop(tx.send(PipelineItem {
                    seq,
                    data: result.map(OutputChunk::Framed),
                }));
                record_send_wait(t_send);
                // Failure here means the main thread already dropped its
                // receiver (shutting down); safe to ignore.
                permit_tx.send(()).ok();
                // Return block_bytes to the pool after frame_blob_into has
                // cloned its contents into FramedBlobParts. This runs at
                // closure exit, which is also when block_bytes would be
                // dropped otherwise.
                if let Some(pool) = pool {
                    pool.put(block_bytes);
                }
            });
            Ok(())
        } else {
            let result = self.write_framed_blob(
                "OSMData",
                &block_bytes,
                Some(indexdata.as_slice()),
                tagdata,
            );
            // Sync path: no rayon closure, return to pool here.
            if let Some(pool) = pool {
                pool.put(block_bytes);
            }
            result
        }
    }

    /// Write pre-framed raw blob bytes directly to the output.
    ///
    /// Used for passthrough of unaffected blocks during merge.
    /// The caller is responsible for providing valid framed bytes:
    /// `[4-byte BE header_len][BlobHeader][Blob]`.
    ///
    /// In pipelined mode, the data is sent directly to the writer thread
    /// (no rayon task needed since there is no compression work).
    pub fn write_raw(&mut self, raw_framed_bytes: &[u8]) -> io::Result<()> {
        if let Some(ref mut pipeline) = self.pipeline {
            let seq = pipeline.seq;
            pipeline.seq += 1;
            WRITER_METRICS.payload_raw_items.fetch_add(1, Relaxed);
            WRITER_METRICS
                .payload_raw_bytes
                .fetch_add(raw_framed_bytes.len() as u64, Relaxed);
            let t_send = std::time::Instant::now();
            pipeline
                .tx
                .send(PipelineItem {
                    seq,
                    data: Ok(OutputChunk::Raw(raw_framed_bytes.to_vec())),
                })
                .map_err(|_| io::Error::other("writer thread terminated"))?;
            record_send_wait(t_send);
            Ok(())
        } else {
            self.writer_mut().write_all(raw_framed_bytes)
        }
    }

    /// Write pre-framed raw blob bytes, taking ownership of the Vec.
    ///
    /// Like [`write_raw`](Self::write_raw) but moves the Vec into the
    /// pipeline channel instead of copying. Use when the caller already
    /// owns the bytes and won't need them afterwards (e.g. merge passthrough
    /// with `std::mem::take`).
    pub fn write_raw_owned(&mut self, raw_framed_bytes: Vec<u8>) -> io::Result<()> {
        if let Some(ref mut pipeline) = self.pipeline {
            let seq = pipeline.seq;
            pipeline.seq += 1;
            WRITER_METRICS.payload_raw_items.fetch_add(1, Relaxed);
            WRITER_METRICS
                .payload_raw_bytes
                .fetch_add(raw_framed_bytes.len() as u64, Relaxed);
            let t_send = std::time::Instant::now();
            pipeline
                .tx
                .send(PipelineItem {
                    seq,
                    data: Ok(OutputChunk::Raw(raw_framed_bytes)),
                })
                .map_err(|_| io::Error::other("writer thread terminated"))?;
            record_send_wait(t_send);
            Ok(())
        } else {
            self.writer_mut().write_all(&raw_framed_bytes)
        }
    }

    /// Write multiple pre-framed raw blob chunks without concatenating them.
    ///
    /// Like [`write_raw_owned`](Self::write_raw_owned) but accepts a list of
    /// owned chunks. The writer thread writes each chunk sequentially.
    /// Used by passthrough coalescers to avoid `extend_from_slice` memcpy.
    pub fn write_raw_chunks(&mut self, chunks: Vec<Vec<u8>>) -> io::Result<()> {
        if let Some(ref mut pipeline) = self.pipeline {
            let seq = pipeline.seq;
            pipeline.seq += 1;
            let total_bytes: u64 = chunks.iter().map(|c| c.len() as u64).sum();
            WRITER_METRICS.payload_raw_chunk_items.fetch_add(1, Relaxed);
            WRITER_METRICS
                .payload_raw_chunk_bytes
                .fetch_add(total_bytes, Relaxed);
            let t_send = std::time::Instant::now();
            pipeline
                .tx
                .send(PipelineItem {
                    seq,
                    data: Ok(OutputChunk::RawChunks(chunks)),
                })
                .map_err(|_| io::Error::other("writer thread terminated"))?;
            record_send_wait(t_send);
            Ok(())
        } else {
            let w = self.writer_mut();
            for chunk in &chunks {
                w.write_all(chunk)?;
            }
            Ok(())
        }
    }

    /// Flush the underlying writer.
    ///
    /// In pipelined mode, this joins the writer thread and propagates any
    /// deferred compression or I/O errors. After flush, the pipeline is
    /// stopped and subsequent writes go through the direct (non-pipelined) path.
    pub fn flush(&mut self) -> io::Result<()> {
        let t_flush = std::time::Instant::now();
        if let Some(mut pipeline) = self.pipeline.take() {
            // Drop sender to signal the writer thread that no more items are coming.
            drop(pipeline.tx);
            if let Some(handle) = pipeline.join_handle.take() {
                handle
                    .join()
                    .map_err(|_| io::Error::other("writer thread panicked"))??;
            }
        }
        if let Some(ref mut w) = self.writer {
            w.flush()?;
        }
        WRITER_METRICS
            .flush_ns
            .fetch_add(elapsed_ns_u64(t_flush), Relaxed);
        WRITER_METRICS.emit();
        Ok(())
    }

    /// Consume the writer and return the inner writer.
    ///
    /// In pipelined mode, the writer was moved to the writer thread and is
    /// not recoverable. Use [`flush`](Self::flush) before dropping instead.
    ///
    /// # Panics
    ///
    /// Panics if the writer was consumed by a pipeline. This is a programming
    /// error (misuse of the API), not a runtime condition.
    pub fn into_inner(mut self) -> W {
        self.writer.take().expect("writer consumed by pipeline")
    }

    // Panics on misuse (calling after pipeline consumed the writer). This is an
    // internal invariant - all public callers go through write_blob/write_raw which
    // are only valid in sync mode or before pipeline handoff.
    fn writer_mut(&mut self) -> &mut W {
        self.writer
            .as_mut()
            .expect("writer consumed by pipeline - call flush() first")
    }

    // wontfix(type-no-stringly): blob_type is &str matching protobuf wire format;
    // only 2 constants ("OSMHeader"/"OSMData"), no real typo risk.
    #[hotpath::measure]
    fn write_blob(&mut self, blob_type: &str, uncompressed: &[u8]) -> io::Result<()> {
        self.write_framed_blob(blob_type, uncompressed, None, None)
    }

    /// Encode, compress, and write a blob directly to the writer using reusable
    /// scratch buffers. Eliminates all intermediate `Vec` allocations after warmup.
    fn write_framed_blob(
        &mut self,
        blob_type: &str,
        uncompressed: &[u8],
        indexdata: Option<&[u8]>,
        tagdata: Option<&[u8]>,
    ) -> io::Result<()> {
        let t_compress = std::time::Instant::now();
        encode_blob_body(uncompressed, &self.compression, &mut self.scratch)?;
        WRITER_METRICS
            .compress_ns
            .fetch_add(elapsed_ns_u64(t_compress), Relaxed);
        let t_frame = std::time::Instant::now();
        let datasize = i32::try_from(self.scratch.blob_buf.len()).map_err(|_| {
            io::Error::other(format!(
                "blob datasize overflow: {} bytes",
                self.scratch.blob_buf.len()
            ))
        })?;
        encode_blob_header_into(
            blob_type,
            datasize,
            indexdata,
            tagdata,
            &mut self.scratch.header_buf,
        );
        let header_len = u32::try_from(self.scratch.header_buf.len()).map_err(|_| {
            io::Error::other(format!(
                "header too large: {} bytes",
                self.scratch.header_buf.len()
            ))
        })?;
        let total_len = 4 + self.scratch.header_buf.len() + self.scratch.blob_buf.len();
        WRITER_METRICS
            .frame_ns
            .fetch_add(elapsed_ns_u64(t_frame), Relaxed);
        WRITER_METRICS
            .bytes_framed
            .fetch_add(total_len as u64, Relaxed);
        // Write the 3 frame parts directly - no intermediate `out` Vec.
        let writer = self.writer.as_mut().expect("writer consumed by pipeline");
        let t_write = std::time::Instant::now();
        writer.write_all(&header_len.to_be_bytes())?;
        writer.write_all(&self.scratch.header_buf)?;
        writer.write_all(&self.scratch.blob_buf)?;
        WRITER_METRICS
            .write_ns
            .fetch_add(elapsed_ns_u64(t_write), Relaxed);
        WRITER_METRICS
            .bytes_written
            .fetch_add(total_len as u64, Relaxed);
        Ok(())
    }
}

impl<W: Write> Drop for PbfWriter<W> {
    fn drop(&mut self) {
        if let Some(mut pipeline) = self.pipeline.take() {
            drop(pipeline.tx);
            if let Some(handle) = pipeline.join_handle.take() {
                // Best-effort join - errors can't be propagated from Drop.
                // Callers should call flush() explicitly to get errors.
                drop(handle.join());
            }
        }
    }
}

#[cfg(feature = "linux-direct-io")]
impl PbfWriter<FileWriter> {
    /// Write a passthrough blob via kernel-space copy (`copy_file_range`).
    ///
    /// Instead of copying blob bytes through userspace, this tells the kernel
    /// to copy directly between file descriptors. On filesystems with reflink
    /// support (btrfs, xfs), this is a metadata-only operation.
    ///
    /// `in_fd` is the input file descriptor (from `FileReader::raw_fd()`).
    /// `offset` and `len` describe the framed blob's position in the input file.
    ///
    /// Callers must not use this when the output writer is O_DIRECT - use
    /// [`write_raw`](Self::write_raw) instead.
    pub fn write_raw_copy(
        &mut self,
        in_fd: std::os::unix::io::RawFd,
        offset: u64,
        len: u64,
    ) -> io::Result<()> {
        if let Some(ref mut pipeline) = self.pipeline {
            let seq = pipeline.seq;
            pipeline.seq += 1;
            WRITER_METRICS.payload_copy_range_items.fetch_add(1, Relaxed);
            WRITER_METRICS
                .payload_copy_range_bytes
                .fetch_add(len, Relaxed);
            let t_send = std::time::Instant::now();
            pipeline
                .tx
                .send(PipelineItem {
                    seq,
                    data: Ok(OutputChunk::CopyRange { in_fd, offset, len }),
                })
                .map_err(|_| io::Error::other("writer thread terminated"))?;
            record_send_wait(t_send);
            Ok(())
        } else {
            // Same invariant as writer_mut - programming error if None.
            let out_fd = self
                .writer
                .as_mut()
                .expect("writer consumed by pipeline")
                .flush_and_raw_fd()?
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "copy_file_range incompatible with O_DIRECT output",
                    )
                })?;
            copy_range(in_fd, out_fd, offset, len)
        }
    }
}

/// Writer thread: receives [`OutputChunk`]s and hands them to the sink in
/// sequence order.
///
/// Uses a shared sequence-number reorder buffer to handle out-of-order
/// arrivals from parallel rayon tasks. Framing errors produced on rayon
/// workers are propagated in order via `io::Result<OutputChunk>`.
#[allow(clippy::needless_pass_by_value)] // Thread entry point owns the sink moved into std::thread::spawn.
fn writer_thread<S: OutputSink>(
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


