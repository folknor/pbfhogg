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
use crate::proto::fileformat;
use crate::write::file_writer::FileWriter;
use bytes::Bytes;
use flate2::write::ZlibEncoder;
use flate2::Compression as FlateCompression;
use protobuf::Message;
use std::collections::VecDeque;
use std::io::{self, Write};
use std::path::Path;
use std::sync::mpsc::{sync_channel, SyncSender};
use std::thread::JoinHandle;

/// Maximum number of framed blobs in-flight before backpressure stalls senders.
const WRITE_AHEAD: usize = 32;

/// Compression algorithm for PBF output blobs.
#[derive(Clone, Copy)]
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
enum PipelinePayload {
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
struct PipelineItem {
    seq: usize,
    data: PipelinePayload,
}

/// Write pipeline state — active when using pipelined mode.
struct WritePipeline {
    tx: SyncSender<PipelineItem>,
    seq: usize,
    join_handle: Option<JoinHandle<io::Result<()>>>,
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
pub struct PbfWriter<W: Write> {
    writer: Option<W>,
    compression: Compression,
    pipeline: Option<WritePipeline>,
}

impl PbfWriter<FileWriter> {
    /// Create a `PbfWriter` that writes to a file at the given path.
    pub fn to_path(path: &Path, compression: Compression) -> io::Result<Self> {
        let writer = FileWriter::buffered(path)?;
        Ok(PbfWriter {
            writer: Some(writer),
            compression,
            pipeline: None,
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
        }
    }

    /// Write the `OSMHeader` blob. Must be the first blob in the file.
    ///
    /// Not needed when using [`to_path_pipelined`](Self::to_path_pipelined),
    /// which writes the header in the constructor.
    ///
    /// `header_block_bytes` is a serialized `HeaderBlock` protobuf message,
    /// typically produced by [`build_header`](crate::block_builder::build_header).
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
                let result = frame_blob(
                    "OSMData",
                    &uncompressed,
                    &compression,
                    indexdata.as_ref().map(<[u8; 26]>::as_slice),
                );
                drop(tx.send(PipelineItem { seq, data: PipelinePayload::Bytes(result) }));
            });
            Ok(())
        } else {
            let indexdata = blob_index::scan_block_ids(block_bytes)
                .map(|idx| idx.serialize());
            self.write_blob_with_index(
                "OSMData",
                block_bytes,
                indexdata.as_ref().map(<[u8; 26]>::as_slice),
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
    pub fn into_inner(mut self) -> W {
        self.writer.take().expect("writer consumed by pipeline")
    }

    fn writer_mut(&mut self) -> &mut W {
        self.writer
            .as_mut()
            .expect("writer consumed by pipeline — call flush() first")
    }

    #[hotpath::measure]
    fn write_blob(&mut self, blob_type: &str, uncompressed: &[u8]) -> io::Result<()> {
        let framed = frame_blob(blob_type, uncompressed, &self.compression, None)?;
        self.writer_mut().write_all(&framed)
    }

    fn write_blob_with_index(
        &mut self,
        blob_type: &str,
        uncompressed: &[u8],
        indexdata: Option<&[u8]>,
    ) -> io::Result<()> {
        let framed = frame_blob(blob_type, uncompressed, &self.compression, indexdata)?;
        self.writer_mut().write_all(&framed)
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
/// This is a pure function with no I/O or shared state, suitable for calling
/// from any thread (e.g. rayon tasks for parallel compression).
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
#[hotpath::measure]
pub(crate) fn frame_blob(
    blob_type: &str,
    uncompressed: &[u8],
    compression: &Compression,
    indexdata: Option<&[u8]>,
) -> io::Result<Vec<u8>> {
    // Step 1: Build the Blob protobuf (optionally compressed)
    let mut blob = fileformat::Blob::new();
    match compression {
        Compression::None => {
            blob.data = Some(fileformat::blob::Data::Raw(Bytes::copy_from_slice(
                uncompressed,
            )));
        }
        Compression::Zlib(level) => {
            // Pre-allocate for ~2x compression ratio (conservative estimate).
            // Zlib on OSM data typically achieves 2–10x. Starting from the
            // worst case avoids progressive reallocation during encoding.
            let estimated_compressed_size = uncompressed.len() / 2;
            let mut encoder = ZlibEncoder::new(
                Vec::with_capacity(estimated_compressed_size),
                FlateCompression::new(*level),
            );
            encoder.write_all(uncompressed)?;
            let compressed = encoder.finish()?;
            blob.set_raw_size(uncompressed.len() as i32);
            blob.data = Some(fileformat::blob::Data::ZlibData(Bytes::from(compressed)));
        }
        Compression::Zstd(level) => {
            let compressed =
                zstd::bulk::compress(uncompressed, *level).map_err(io::Error::other)?;
            blob.set_raw_size(uncompressed.len() as i32);
            blob.data = Some(fileformat::blob::Data::ZstdData(Bytes::from(compressed)));
        }
    }

    let blob_bytes = blob.write_to_bytes().map_err(io::Error::other)?;

    // Step 2: Build the BlobHeader
    let mut header = fileformat::BlobHeader::new();
    header.set_type(protobuf::Chars::from(blob_type));
    header.set_datasize(blob_bytes.len() as i32);
    if let Some(idx) = indexdata {
        header.indexdata = Some(Bytes::copy_from_slice(idx));
    }

    let header_bytes = header.write_to_bytes().map_err(io::Error::other)?;

    // Step 3: Assemble [4-byte BE header_len][BlobHeader][Blob]
    let header_len = header_bytes.len() as u32;
    let total_len = 4 + header_bytes.len() + blob_bytes.len();
    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&header_len.to_be_bytes());
    out.extend_from_slice(&header_bytes);
    out.extend_from_slice(&blob_bytes);

    Ok(out)
}

/// Re-frame an already-compressed Blob with a new BlobHeader that includes indexdata.
///
/// Takes the raw compressed Blob protobuf bytes (from a passthrough frame) and
/// builds a new frame `[4-byte header_len][BlobHeader][Blob]` with the indexdata
/// field set. The Blob bytes are not modified — only the BlobHeader is rebuilt.
///
/// This is used by merge to add blob-level index metadata to passthrough blobs
/// so that subsequent merges can classify them without decompression.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub(crate) fn reframe_raw_with_index(
    blob_bytes: &[u8],
    indexdata: &[u8],
) -> io::Result<Vec<u8>> {
    let mut header = fileformat::BlobHeader::new();
    header.set_type(protobuf::Chars::from("OSMData"));
    header.set_datasize(blob_bytes.len() as i32);
    header.indexdata = Some(Bytes::copy_from_slice(indexdata));

    let header_bytes = header.write_to_bytes().map_err(io::Error::other)?;

    let header_len = header_bytes.len() as u32;
    let total_len = 4 + header_bytes.len() + blob_bytes.len();
    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&header_len.to_be_bytes());
    out.extend_from_slice(&header_bytes);
    out.extend_from_slice(blob_bytes);

    Ok(out)
}
