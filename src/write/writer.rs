//! PBF file writer — blob framing and compression.
//!
//! Writes valid `.osm.pbf` files. The writer handles the low-level blob framing
//! (4-byte header length, BlobHeader, compressed Blob) and delegates block
//! construction to [`BlockBuilder`](crate::block_builder::BlockBuilder).
//!
//! # Pipelined mode
//!
//! [`to_path_pipelined`](PbfWriter::to_path_pipelined) creates a writer that
//! compresses blobs in parallel using rayon, with a dedicated writer thread that
//! reorders results back into sequence order. Raw passthrough blobs bypass
//! compression entirely.

use crate::blob_index;
use crate::write::file_writer::FileWriter;
use protohoggr::{encode_bytes_field, encode_int32_field};
use flate2::Compress;
use flate2::Compression as FlateCompression;
use flate2::FlushCompress;
use flate2::Status;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, Write};
use std::path::Path;
use std::sync::mpsc::{sync_channel, SyncSender};
use std::thread::JoinHandle;

/// Maximum number of framed blobs in-flight before backpressure stalls senders.
pub(crate) const WRITE_AHEAD: usize = 32;

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

/// Payload for the writer pipeline channel.
pub(crate) enum PipelinePayload {
    /// Framed blob bytes ready to write.
    Bytes(io::Result<Vec<u8>>),
    /// Kernel-space copy from input fd (avoids userspace copy for passthrough).
    #[cfg(feature = "linux-direct-io")]
    CopyRange {
        in_fd: std::os::unix::io::RawFd,
        offset: u64,
        len: u64,
    },
}

/// A framed blob with its sequence number, ready for the writer thread.
pub(crate) struct PipelineItem {
    pub(crate) seq: usize,
    pub(crate) data: PipelinePayload,
}

/// Write pipeline state — active when using pipelined mode.
struct WritePipeline {
    tx: SyncSender<PipelineItem>,
    seq: usize,
    join_handle: Option<JoinHandle<io::Result<()>>>,
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
    zlib_compressor: Option<Compress>,
    /// Reusable zstd compressor (lazy-initialized on first zstd blob).
    zstd_compressor: Option<zstd::bulk::Compressor<'static>>,
}

thread_local! {
    /// Per-rayon-thread scratch buffers for pipelined blob framing.
    static PIPELINE_SCRATCH: RefCell<FrameScratch> = const { RefCell::new(FrameScratch {
        blob_buf: Vec::new(),
        header_buf: Vec::new(),
        compress_buf: Vec::new(),
        zlib_compressor: None,
        zstd_compressor: None,
    }) };
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
/// Use [`to_path_pipelined`](Self::to_path_pipelined) for parallel compression.
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
    /// Create a `PbfWriter` that writes to a file at the given path.
    pub fn to_path(path: &Path, compression: Compression) -> io::Result<Self> {
        let writer = FileWriter::buffered(path)?;
        Ok(PbfWriter {
            writer: Some(writer),
            compression,
            pipeline: None,
            scratch: FrameScratch { blob_buf: Vec::new(), header_buf: Vec::new(), compress_buf: Vec::new(), zlib_compressor: None, zstd_compressor: None },
        })
    }

    /// Create a `PbfWriter` with `O_DIRECT` for page-cache-free writes.
    ///
    /// All writes bypass the kernel page cache, preventing cache pollution
    /// during planet-scale (80 GB+) PBF writes. Requires a filesystem that
    /// supports `O_DIRECT` (not tmpfs).
    #[cfg(feature = "linux-direct-io")]
    pub fn to_path_direct(path: &Path, compression: Compression) -> io::Result<Self> {
        let writer = FileWriter::direct(path)?;
        Ok(PbfWriter {
            writer: Some(writer),
            compression,
            pipeline: None,
            scratch: FrameScratch { blob_buf: Vec::new(), header_buf: Vec::new(), compress_buf: Vec::new(), zlib_compressor: None, zstd_compressor: None },
        })
    }

    /// Create a pipelined `PbfWriter` that compresses blobs in parallel.
    ///
    /// Writes the OSMHeader blob synchronously, then spawns a writer thread.
    /// Subsequent [`write_primitive_block`](Self::write_primitive_block) calls
    /// dispatch compression to the rayon pool. Raw passthrough blobs
    /// ([`write_raw`](Self::write_raw)) are sent directly to the writer thread.
    ///
    /// Call [`flush`](Self::flush) when done to join the writer thread and
    /// propagate any I/O errors.
    pub fn to_path_pipelined(
        path: &Path,
        compression: Compression,
        header_block_bytes: &[u8],
    ) -> io::Result<Self> {
        let writer = FileWriter::buffered(path)?;
        Self::start_pipeline(writer, compression, header_block_bytes)
    }

    /// Create a pipelined `PbfWriter` with `O_DIRECT` for page-cache-free writes.
    #[cfg(feature = "linux-direct-io")]
    pub fn to_path_pipelined_direct(
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
    pub fn to_path_pipelined_uring(
        path: &Path,
        compression: Compression,
        header_block_bytes: &[u8],
        sqpoll: bool,
    ) -> io::Result<Self> {
        use crate::write::uring_writer;

        let framed_header = frame_blob("OSMHeader", header_block_bytes, &compression, None)?;

        // Oneshot channel for init errors from the writer thread.
        let (init_tx, init_rx) = sync_channel(1);

        let (tx, rx) = sync_channel(WRITE_AHEAD);
        let path_owned = path.to_path_buf();
        let handle = std::thread::spawn(move || {
            uring_writer::uring_writer_thread(rx, path_owned, framed_header, init_tx, sqpoll)
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

        Ok(PbfWriter {
            writer: None,
            compression,
            pipeline: Some(WritePipeline {
                tx,
                seq: 0,
                join_handle: Some(handle),
            }),
            scratch: FrameScratch { blob_buf: Vec::new(), header_buf: Vec::new(), compress_buf: Vec::new(), zlib_compressor: None, zstd_compressor: None },
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

        // Spawn the writer thread and hand it the writer.
        let (tx, rx) = sync_channel(WRITE_AHEAD);
        let handle = std::thread::spawn(move || writer_thread(rx, writer));

        Ok(PbfWriter {
            writer: None,
            compression,
            pipeline: Some(WritePipeline {
                tx,
                seq: 0,
                join_handle: Some(handle),
            }),
            scratch: FrameScratch { blob_buf: Vec::new(), header_buf: Vec::new(), compress_buf: Vec::new(), zlib_compressor: None, zstd_compressor: None },
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
            scratch: FrameScratch { blob_buf: Vec::new(), header_buf: Vec::new(), compress_buf: Vec::new(), zlib_compressor: None, zstd_compressor: None },
        }
    }

    /// Write the `OSMHeader` blob. Must be the first blob in the file.
    ///
    /// Not needed when using [`to_path_pipelined`](Self::to_path_pipelined),
    /// which writes the header in the constructor.
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
            let seq = pipeline.seq;
            pipeline.seq += 1;
            let compression = self.compression;
            let uncompressed = block_bytes.to_vec();
            let tx = pipeline.tx.clone();
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
                drop(tx.send(PipelineItem { seq, data: PipelinePayload::Bytes(result) }));
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
    pub fn write_primitive_block_owned(
        &mut self,
        block_bytes: Vec<u8>,
        index: blob_index::BlobIndex,
        tagdata: Option<&[u8]>,
    ) -> io::Result<()> {
        let indexdata = index.serialize();
        if let Some(ref mut pipeline) = self.pipeline {
            let seq = pipeline.seq;
            pipeline.seq += 1;
            let compression = self.compression;
            let tx = pipeline.tx.clone();
            let tagdata_owned = tagdata.map(<[u8]>::to_vec);
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
                drop(tx.send(PipelineItem { seq, data: PipelinePayload::Bytes(result) }));
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
            pipeline
                .tx
                .send(PipelineItem {
                    seq,
                    data: PipelinePayload::Bytes(Ok(raw_framed_bytes.to_vec())),
                })
                .map_err(|_| io::Error::other("writer thread terminated"))?;
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
            pipeline
                .tx
                .send(PipelineItem {
                    seq,
                    data: PipelinePayload::Bytes(Ok(raw_framed_bytes)),
                })
                .map_err(|_| io::Error::other("writer thread terminated"))?;
            Ok(())
        } else {
            self.writer_mut().write_all(&raw_framed_bytes)
        }
    }

    /// Flush the underlying writer.
    ///
    /// In pipelined mode, this joins the writer thread and propagates any
    /// deferred compression or I/O errors. After flush, the pipeline is
    /// stopped and subsequent writes go through the direct (non-pipelined) path.
    pub fn flush(&mut self) -> io::Result<()> {
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
        encode_blob_body(
            uncompressed,
            &self.compression,
            &mut self.scratch,
        )?;
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
        // Write the 3 frame parts directly — no intermediate `out` Vec.
        let writer = self.writer.as_mut().expect("writer consumed by pipeline");
        writer.write_all(&header_len.to_be_bytes())?;
        writer.write_all(&self.scratch.header_buf)?;
        writer.write_all(&self.scratch.blob_buf)?;
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
            pipeline
                .tx
                .send(PipelineItem {
                    seq,
                    data: PipelinePayload::CopyRange { in_fd, offset, len },
                })
                .map_err(|_| io::Error::other("writer thread terminated"))?;
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

/// Writer thread: receives framed blobs and writes them in sequence order.
///
/// Uses a VecDeque reorder buffer (same pattern as the read-side pipeline)
/// to handle out-of-order arrivals from parallel rayon tasks.
///
/// Specialized to `FileWriter` (not generic `W: Write`) to support
/// `CopyRange` payloads that require `flush_and_raw_fd()`.
fn writer_thread(
    rx: std::sync::mpsc::Receiver<PipelineItem>,
    mut writer: FileWriter,
) -> io::Result<()> {
    let mut next_seq: usize = 0;
    let mut pending: VecDeque<Option<PipelinePayload>> =
        VecDeque::with_capacity(WRITE_AHEAD);

    for item in rx {
        let slot_idx = item.seq - next_seq;
        if slot_idx >= pending.len() {
            pending.resize_with(slot_idx + 1, || None);
        }
        pending[slot_idx] = Some(item.data);

        // Drain consecutive ready items from the front.
        loop {
            let front_is_filled = pending.front().is_some_and(Option::is_some);
            if !front_is_filled {
                break;
            }
            #[allow(clippy::unwrap_used)]
            let payload = pending.pop_front().unwrap().unwrap();
            next_seq += 1;
            match payload {
                PipelinePayload::Bytes(result) => writer.write_all(&result?)?,
                #[cfg(feature = "linux-direct-io")]
                PipelinePayload::CopyRange { in_fd, offset, len } => {
                    let out_fd = writer
                        .flush_and_raw_fd()?
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::Unsupported,
                                "copy_file_range incompatible with O_DIRECT output",
                            )
                        })?;
                    copy_range(in_fd, out_fd, offset, len)?;
                }
            }
        }
    }

    writer.flush()?;
    Ok(())
}

/// Copy `len` bytes between file descriptors using `copy_file_range(2)`.
///
/// Uses an explicit input offset (does not change `in_fd`'s file position),
/// safe when `in_fd` is wrapped in a `BufReader` or `DirectReader`.
/// Output uses the fd's current position (sequential write).
#[cfg(feature = "linux-direct-io")]
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn copy_range(
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
            return Err(io::Error::last_os_error());
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
    let mut scratch = FrameScratch {
        blob_buf: Vec::new(),
        header_buf: Vec::new(),
        compress_buf: Vec::new(),
        zlib_compressor: None,
        zstd_compressor: None,
    };
    frame_blob_into(blob_type, uncompressed, compression, indexdata, None, &mut scratch)
}

/// Compress and frame a blob using reusable scratch buffers.
///
/// Returns an owned `Vec<u8>` suitable for sending through a pipeline channel.
/// The scratch buffers are cleared and reused — after warmup, only the returned
/// `out` Vec is allocated per call.
#[hotpath::measure]
fn frame_blob_into(
    blob_type: &str,
    uncompressed: &[u8],
    compression: &Compression,
    indexdata: Option<&[u8]>,
    tagdata: Option<&[u8]>,
    scratch: &mut FrameScratch,
) -> io::Result<Vec<u8>> {
    encode_blob_body(uncompressed, compression, scratch)?;

    let datasize = i32::try_from(scratch.blob_buf.len()).map_err(|_| {
        io::Error::other(format!("blob datasize overflow: {} bytes", scratch.blob_buf.len()))
    })?;
    encode_blob_header_into(blob_type, datasize, indexdata, tagdata, &mut scratch.header_buf);

    let header_len = u32::try_from(scratch.header_buf.len())
        .map_err(|_| io::Error::other(format!("header too large: {} bytes", scratch.header_buf.len())))?;
    let total_len = 4 + scratch.header_buf.len() + scratch.blob_buf.len();
    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&header_len.to_be_bytes());
    out.extend_from_slice(&scratch.header_buf);
    out.extend_from_slice(&scratch.blob_buf);

    Ok(out)
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
            if scratch.zstd_compressor.is_none() {
                scratch.zstd_compressor = Some(
                    zstd::bulk::Compressor::new(*level).map_err(io::Error::other)?,
                );
            }
            let Some(compressor) = scratch.zstd_compressor.as_mut() else {
                unreachable!()
            };
            scratch.compress_buf = compressor.compress(uncompressed).map_err(io::Error::other)?;
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
    let compressor = scratch.zlib_compressor.get_or_insert_with(|| {
        Compress::new(FlateCompression::new(level), true)
    });
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
    let datasize = i32::try_from(blob_bytes.len()).map_err(|_| {
        io::Error::other(format!("blob datasize overflow: {} bytes", blob_bytes.len()))
    })?;
    let mut header_buf = Vec::new();
    encode_blob_header_into("OSMData", datasize, Some(indexdata), tagdata, &mut header_buf);

    let header_len = u32::try_from(header_buf.len())
        .map_err(|_| io::Error::other(format!("header too large: {} bytes", header_buf.len())))?;
    let total_len = 4 + header_buf.len() + blob_bytes.len();
    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&header_len.to_be_bytes());
    out.extend_from_slice(&header_buf);
    out.extend_from_slice(blob_bytes);

    Ok(out)
}
