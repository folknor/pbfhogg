//! PBF file writer — blob framing and compression.
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

use crate::blob_index;
use crate::write::file_writer::FileWriter;
use crate::write::metrics::WRITER_METRICS;
use protohoggr::{encode_bytes_field, encode_int32_field};
use flate2::Compress;
use flate2::Compression as FlateCompression;
use flate2::FlushCompress;
use flate2::Status;
use crate::reorder_buffer::ReorderBuffer;
use std::cell::RefCell;
use std::io::{self, Write};
use std::path::Path;
use std::str::FromStr;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::atomic::Ordering::Relaxed;
use std::thread::JoinHandle;

/// Maximum number of framed blobs in-flight before backpressure stalls senders.
pub(crate) const WRITE_AHEAD: usize = 32;

/// Maximum number of in-flight rayon dispatches in the pipelined writer.
///
/// Counting-semaphore cap that bounds how many uncompressed block `Vec<u8>`s
/// can be owned by rayon closures simultaneously (queued, being compressed,
/// or waiting on the bounded output channel). Without this, `rayon::spawn`'s
/// unbounded internal task queue grows without limit when the producer side
/// out-runs compression throughput — exactly what happened in commit
/// `e7219f0` on planet when parallel pass 1 / stage 2a / stage 2d started
/// emitting blocks faster than zlib:6 could drain them, killing pbfhogg via
/// OOM at 26 GB anon RSS.
///
/// Memory bound: `PIPELINE_DISPATCH_PERMITS × max_block_size`. At ~4 MB per
/// uncompressed block and 64 permits that's ~256 MB worst case, small
/// compared to the 2.79 GB stage-2b peak.
pub(crate) const PIPELINE_DISPATCH_PERMITS: usize = 64;

/// Compression algorithm for PBF output blobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Compression {
    /// No compression (raw bytes).
    None,
    /// Zlib compression at the given level (0–9).
    /// Level 6 matches osmium's default (`Z_DEFAULT_COMPRESSION`).
    Zlib(u32),
    /// Zstd compression at the given level (1–22, default 3).
    ///
    /// Zstd decompresses 3-5x faster than zlib at equivalent compression ratios,
    /// making it ideal for read-heavy workflows (planet imports, tile generation).
    /// Level 3 (zstd's default) provides a good balance of compression ratio and
    /// speed. Higher levels (e.g. 19) compress ~10-15% better but are much slower
    /// to write — use for archival PBFs that will be read many times.
    ///
    /// **Compatibility warning:** Not all PBF consumers support zstd yet. As of
    /// 2025, osmium, osm2pgsql, and most tools only read zlib-compressed PBFs.
    /// Use zstd for internal pipelines where you control both writer and reader.
    Zstd(i32),
}

impl Default for Compression {
    fn default() -> Self {
        Compression::Zlib(6)
    }
}

/// Parse error for [`Compression`] string specs.
#[derive(Debug, Clone)]
pub struct ParseCompressionError(String);

impl std::fmt::Display for ParseCompressionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseCompressionError {}

impl FromStr for Compression {
    type Err = ParseCompressionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Self::None),
            "zlib" => Ok(Self::default()),
            "zstd" => Ok(Self::Zstd(3)),
            _ if s.starts_with("zlib:") => {
                let level: u32 = s[5..]
                    .parse()
                    .map_err(|_| ParseCompressionError(format!("invalid zlib level: {s}")))?;
                if level > 9 {
                    return Err(ParseCompressionError(format!(
                        "zlib level must be 0-9, got {level}"
                    )));
                }
                Ok(Self::Zlib(level))
            }
            _ if s.starts_with("zstd:") => {
                let level: i32 = s[5..]
                    .parse()
                    .map_err(|_| ParseCompressionError(format!("invalid zstd level: {s}")))?;
                if !(-7..=22).contains(&level) {
                    return Err(ParseCompressionError(format!(
                        "zstd level must be -7..22, got {level}"
                    )));
                }
                Ok(Self::Zstd(level))
            }
            _ => Err(ParseCompressionError(format!(
                "unknown compression: {s} (expected none, zlib, zlib:LEVEL, zstd, zstd:LEVEL)"
            ))),
        }
    }
}

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
/// and may fail asynchronously — the error is propagated in order via the
/// reorder buffer so the writer thread surfaces it at the correct position.
pub(crate) struct PipelineItem {
    pub(crate) seq: usize,
    pub(crate) data: io::Result<OutputChunk>,
}

/// A sink that consumes ordered [`OutputChunk`]s and writes them to a backend.
///
/// Each backend is free to flatten, batch, or scatter-gather as it sees fit —
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

/// Write pipeline state — active when using pipelined mode.
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

fn elapsed_ns_u64(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// Reusable scratch buffers for blob framing, avoiding per-call allocation.
///
/// After the first blob, all three buffers have sufficient capacity and
/// subsequent calls reuse them without allocating. The zlib and zstd
/// compressors are lazy-initialized on the first blob of each type and
/// reused — avoiding ~312 KB (zlib) or ~512 KB (zstd) of compressor state
/// allocation per blob.
struct FrameScratch {
    /// Blob protobuf body (raw/compressed data fields).
    blob_buf: Vec<u8>,
    /// BlobHeader protobuf.
    header_buf: Vec<u8>,
    /// Intermediate compression output (zlib/zstd only).
    compress_buf: Vec<u8>,
    /// Reusable zlib compressor (lazy-initialized on first zlib blob).
    zlib_compressor: Option<(u32, Compress)>,
    /// Reusable zstd compressor (lazy-initialized on first zstd blob).
    zstd_compressor: Option<(i32, zstd::bulk::Compressor<'static>)>,
}

impl FrameScratch {
    const fn new() -> Self {
        Self {
            blob_buf: Vec::new(),
            header_buf: Vec::new(),
            compress_buf: Vec::new(),
            zlib_compressor: None,
            zstd_compressor: None,
        }
    }
}

thread_local! {
    /// Per-rayon-thread scratch buffers for pipelined blob framing.
    static PIPELINE_SCRATCH: RefCell<FrameScratch> = const { RefCell::new(FrameScratch::new()) };
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
    /// PBF blobs are typically 16–64KB compressed, so the default 8KB
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
                let indexdata = blob_index::scan_block_ids(&uncompressed)
                    .map(|idx| idx.serialize());
                let tagdata = blob_index::scan_block_tags(&uncompressed)
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
            let indexdata = blob_index::scan_block_ids(block_bytes)
                .map(|idx| idx.serialize());
            let tagdata = blob_index::scan_block_tags(block_bytes)
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
    /// pre-computed [`BlobIndex`](crate::blob_index::BlobIndex) and optional
    /// pre-serialized tagdata from
    /// [`BlockBuilder::take_owned`](crate::block_builder::BlockBuilder::take_owned)
    /// instead of rescanning the serialized bytes.
    #[hotpath::measure]
    pub(crate) fn write_primitive_block_owned(
        &mut self,
        block_bytes: Vec<u8>,
        index: blob_index::BlobIndex,
        tagdata: Option<&[u8]>,
    ) -> io::Result<()> {
        let indexdata = index.serialize();
        if let Some(ref mut pipeline) = self.pipeline {
            // Bound in-flight rayon dispatches — see the sibling
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
            });
            Ok(())
        } else {
            self.write_framed_blob(
                "OSMData",
                &block_bytes,
                Some(indexdata.as_slice()),
                tagdata,
            )
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
    // internal invariant — all public callers go through write_blob/write_raw which
    // are only valid in sync mode or before pipeline handoff.
    fn writer_mut(&mut self) -> &mut W {
        self.writer
            .as_mut()
            .expect("writer consumed by pipeline — call flush() first")
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
        // Write the 3 frame parts directly — no intermediate `out` Vec.
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
                // Best-effort join — errors can't be propagated from Drop.
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
    /// Callers must not use this when the output writer is O_DIRECT — use
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
            // Same invariant as writer_mut — programming error if None.
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

/// Copy `len` bytes between file descriptors using `copy_file_range(2)`.
///
/// Uses an explicit input offset (does not change `in_fd`'s file position),
/// safe when `in_fd` is wrapped in a `BufReader` or `DirectReader`.
/// Output uses the fd's current position (sequential write).
#[cfg(feature = "linux-direct-io")]
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub(crate) fn copy_range(
    in_fd: std::os::unix::io::RawFd,
    out_fd: std::os::unix::io::RawFd,
    mut offset: u64,
    mut len: u64,
) -> io::Result<()> {
    while len > 0 {
        let mut off_in = offset as i64;
        // Safety: fds are valid and open. off_in is explicit (doesn't change
        // in_fd position). off_out is NULL (uses out_fd's current position).
        let n = unsafe {
            libc::copy_file_range(
                in_fd,
                &mut off_in,
                out_fd,
                std::ptr::null_mut(),
                len as usize,
                0,
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EXDEV) {
                // Cross-device: fall back to pread+write.
                return copy_range_fallback(in_fd, out_fd, offset, len);
            }
            return Err(err);
        }
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "copy_file_range returned 0",
            ));
        }
        let n = n.cast_unsigned() as u64;
        offset += n;
        len -= n;
    }
    Ok(())
}

/// Fallback for cross-device copies: pread from `in_fd` at `offset`, write to `out_fd`.
#[cfg(feature = "linux-direct-io")]
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn copy_range_fallback(
    in_fd: std::os::unix::io::RawFd,
    out_fd: std::os::unix::io::RawFd,
    mut offset: u64,
    mut len: u64,
) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::io::FromRawFd;

    let mut buf = vec![0u8; 256 * 1024];
    // Wrap in ManuallyDrop so we don't close the fd when done — caller owns it.
    let mut out = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(out_fd) });
    // Read from in_fd using pread (doesn't change file position).
    while len > 0 {
        let chunk = buf.len().min(len as usize);
        let n = unsafe {
            libc::pread(in_fd, buf.as_mut_ptr().cast(), chunk, offset as i64)
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "pread returned 0 during cross-device copy",
            ));
        }
        let n = n.cast_unsigned();
        out.write_all(&buf[..n])?;
        offset += n as u64;
        len -= n as u64;
    }
    Ok(())
}

/// Compress and frame a blob into the complete PBF wire format:
/// `[4-byte BE header_len][BlobHeader bytes][Blob bytes]`.
///
/// Allocates fresh buffers — use only for one-off calls (e.g. header framing).
/// For hot-path data blocks, use `frame_blob_into` (pipelined) or
/// `write_framed_blob` (sync) which reuse scratch buffers.
pub(crate) fn frame_blob(
    blob_type: &str,
    uncompressed: &[u8],
    compression: &Compression,
    indexdata: Option<&[u8]>,
) -> io::Result<Vec<u8>> {
    let mut scratch = FrameScratch::new();
    Ok(frame_blob_into(
        blob_type,
        uncompressed,
        compression,
        indexdata,
        None,
        &mut scratch,
    )?
    .into_vec())
}

/// Compress and frame a blob using per-thread scratch buffers.
///
/// Intended for rayon `par_iter` workers that produce fully framed blobs
/// in the parallel phase, so the sequential write phase can use
/// `write_raw_owned` with bounded backpressure (no second rayon dispatch).
pub(crate) fn frame_blob_pipelined(
    uncompressed: &[u8],
    compression: &Compression,
    indexdata: Option<&[u8]>,
    tagdata: Option<&[u8]>,
) -> io::Result<FramedBlobParts> {
    PIPELINE_SCRATCH.with_borrow_mut(|scratch| {
        frame_blob_into("OSMData", uncompressed, compression, indexdata, tagdata, scratch)
    })
}

/// Compress and frame a blob using reusable scratch buffers.
///
/// Returns an owned `Vec<u8>` suitable for sending through a pipeline channel.
/// The scratch buffers are cleared and reused — after warmup, only the returned
/// `out` Vec is allocated per call.
///
/// NOTE: Buffer recycling pool was attempted to eliminate this per-call
/// allocation via `Arc<Mutex<Vec<Vec<u8>>>>` shared between rayon workers and
/// the writer thread. Regressed throughput by +12% (Germany, Compression::None)
/// due to Mutex contention. The allocator's own thread-local caching handles
/// the cross-thread alloc/free pattern better than an explicit pool.
/// Full analysis: `notes/memory/p6-vectored-writer-framing.md` at 2bf438c.
#[hotpath::measure]
fn frame_blob_into(
    blob_type: &str,
    uncompressed: &[u8],
    compression: &Compression,
    indexdata: Option<&[u8]>,
    tagdata: Option<&[u8]>,
    scratch: &mut FrameScratch,
) -> io::Result<FramedBlobParts> {
    let t_compress = std::time::Instant::now();
    encode_blob_body(uncompressed, compression, scratch)?;
    WRITER_METRICS
        .compress_ns
        .fetch_add(elapsed_ns_u64(t_compress), Relaxed);
    let t_frame = std::time::Instant::now();

    let datasize = i32::try_from(scratch.blob_buf.len()).map_err(|_| {
        io::Error::other(format!("blob datasize overflow: {} bytes", scratch.blob_buf.len()))
    })?;
    encode_blob_header_into(blob_type, datasize, indexdata, tagdata, &mut scratch.header_buf);

    let header_len = u32::try_from(scratch.header_buf.len())
        .map_err(|_| io::Error::other(format!("header too large: {} bytes", scratch.header_buf.len())))?;
    let total_len = 4 + scratch.header_buf.len() + scratch.blob_buf.len();
    WRITER_METRICS
        .frame_ns
        .fetch_add(elapsed_ns_u64(t_frame), Relaxed);
    WRITER_METRICS
        .bytes_framed
        .fetch_add(total_len as u64, Relaxed);

    // Clone (not swap) so `scratch.header_buf` / `scratch.blob_buf` retain
    // their high-water capacity across blobs. Swap would leave the scratch
    // with empty Vecs, forcing both buffers to re-grow from zero on every
    // subsequent blob — breaking the reuse invariant documented above.
    Ok(FramedBlobParts {
        prefix: header_len.to_be_bytes(),
        header: scratch.header_buf.clone(),
        blob: scratch.blob_buf.clone(),
    })
}

/// Encode the Blob protobuf body (optionally compressed) into scratch buffers.
///
/// Uses `compress_buf` as an intermediate for zlib/zstd compression output.
/// Both zlib and zstd compressors are lazy-initialized and reused to avoid
/// ~312 KB (zlib) or ~512 KB (zstd) of compressor state allocation per blob.
fn encode_blob_body(
    uncompressed: &[u8],
    compression: &Compression,
    scratch: &mut FrameScratch,
) -> io::Result<()> {
    scratch.blob_buf.clear();
    match compression {
        Compression::None => {
            // Blob field 1: raw (bytes, len-delimited)
            encode_bytes_field(&mut scratch.blob_buf, 1, uncompressed);
        }
        Compression::Zlib(level) => {
            compress_zlib(uncompressed, *level, scratch)?;
        }
        Compression::Zstd(level) => {
            match &scratch.zstd_compressor {
                Some((cached_level, _)) if *cached_level == *level => {}
                _ => {
                    scratch.zstd_compressor = Some((
                        *level,
                        zstd::bulk::Compressor::new(*level).map_err(io::Error::other)?,
                    ));
                }
            }
            let (_, compressor) = scratch.zstd_compressor.as_mut().expect("just initialized");
            let bound = zstd::zstd_safe::compress_bound(uncompressed.len());
            scratch.compress_buf.clear();
            scratch.compress_buf.reserve(bound);
            compressor.compress_to_buffer(uncompressed, &mut scratch.compress_buf).map_err(io::Error::other)?;
            let raw_size = i32::try_from(uncompressed.len()).map_err(|_| {
                io::Error::other(format!("blob raw_size overflow: {} bytes", uncompressed.len()))
            })?;
            // Blob field 2: raw_size (int32, varint)
            encode_int32_field(&mut scratch.blob_buf, 2, raw_size);
            // Blob field 7: zstd_data (bytes, len-delimited)
            encode_bytes_field(&mut scratch.blob_buf, 7, &scratch.compress_buf);
        }
    }
    Ok(())
}

/// Zlib compression via reusable `flate2::Compress` with `reset()`.
fn compress_zlib(
    uncompressed: &[u8],
    level: u32,
    scratch: &mut FrameScratch,
) -> io::Result<()> {
    let needs_new = match &scratch.zlib_compressor {
        Some((cached_level, _)) => *cached_level != level,
        None => true,
    };
    if needs_new {
        scratch.zlib_compressor = Some((level, Compress::new(FlateCompression::new(level), true)));
    }
    let (_, compressor) = scratch.zlib_compressor.as_mut().expect("just initialized");
    scratch.compress_buf.clear();
    // Zlib worst-case bound: input + ~0.1% + header/trailer.
    scratch.compress_buf.reserve(uncompressed.len() + (uncompressed.len() >> 10) + 64);
    let status = compressor
        .compress_vec(uncompressed, &mut scratch.compress_buf, FlushCompress::Finish)
        .map_err(|e| io::Error::other(format!("zlib compress error: {e}")))?;
    if !matches!(status, Status::StreamEnd) {
        return Err(io::Error::other("zlib compress did not complete in one call"));
    }
    compressor.reset();
    let raw_size = i32::try_from(uncompressed.len()).map_err(|_| {
        io::Error::other(format!("blob raw_size overflow: {} bytes", uncompressed.len()))
    })?;
    encode_int32_field(&mut scratch.blob_buf, 2, raw_size);
    encode_bytes_field(&mut scratch.blob_buf, 3, &scratch.compress_buf);
    Ok(())
}

/// Encode a BlobHeader into `buf` (cleared and reused).
///
/// BlobHeader fields: type (string, field 1), indexdata (bytes, field 2),
/// datasize (int32, field 3), tagdata (bytes, field 4).
///
/// **libosmium compat note:** libosmium 2.23.0 has a signed-char sign-extension
/// bug in `get_size_in_network_byte_order` that rejects any BlobHeader > 127
/// bytes. With indexdata (42 bytes) + tagdata (variable), rewritten blobs
/// routinely exceed this. Filed as <https://github.com/osmcode/libosmium/issues/405>.
/// Not a problem for pbfhogg's own reader or the production pipeline — only
/// affects users who open pbfhogg-generated PBFs with osmium-tool.
fn encode_blob_header_into(
    blob_type: &str,
    datasize: i32,
    indexdata: Option<&[u8]>,
    tagdata: Option<&[u8]>,
    buf: &mut Vec<u8>,
) {
    buf.clear();
    // Field 1: type (string = len-delimited bytes)
    encode_bytes_field(buf, 1, blob_type.as_bytes());
    // Field 2: indexdata (optional bytes)
    if let Some(data) = indexdata {
        encode_bytes_field(buf, 2, data);
    }
    // Field 3: datasize (int32 varint)
    encode_int32_field(buf, 3, datasize);
    // Field 4: tagdata (optional bytes — per-blob tag key index)
    if let Some(data) = tagdata {
        encode_bytes_field(buf, 4, data);
    }
}

/// Re-frame an already-compressed Blob with a new BlobHeader that includes indexdata.
///
/// Takes the raw compressed Blob protobuf bytes (from a passthrough frame) and
/// builds a new frame `[4-byte header_len][BlobHeader][Blob]` with the indexdata
/// field set. The Blob bytes are not modified — only the BlobHeader is rebuilt.
///
/// This is used by merge to add blob-level index metadata to passthrough blobs
/// so that subsequent merges can classify them without decompression.
pub(crate) fn reframe_raw_with_index(
    blob_bytes: &[u8],
    indexdata: &[u8],
    tagdata: Option<&[u8]>,
) -> io::Result<Vec<u8>> {
    let mut header_buf = Vec::new();
    reframe_raw_with_index_scratch(blob_bytes, indexdata, tagdata, &mut header_buf)
}

pub(crate) fn reframe_raw_with_index_scratch(
    blob_bytes: &[u8],
    indexdata: &[u8],
    tagdata: Option<&[u8]>,
    header_buf: &mut Vec<u8>,
) -> io::Result<Vec<u8>> {
    let datasize = i32::try_from(blob_bytes.len()).map_err(|_| {
        io::Error::other(format!("blob datasize overflow: {} bytes", blob_bytes.len()))
    })?;
    header_buf.clear();
    encode_blob_header_into("OSMData", datasize, Some(indexdata), tagdata, header_buf);

    let header_len = u32::try_from(header_buf.len())
        .map_err(|_| io::Error::other(format!("header too large: {} bytes", header_buf.len())))?;
    let total_len = 4 + header_buf.len() + blob_bytes.len();
    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&header_len.to_be_bytes());
    out.extend_from_slice(header_buf);
    out.extend_from_slice(blob_bytes);

    Ok(out)
}
