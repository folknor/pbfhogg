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
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::sync_channel;

// Compression type moved to the sibling `compression` module; re-exported
// here so the existing `crate::writer::Compression` path keeps resolving.
pub use crate::write::compression::{Compression, ParseCompressionError};

// Blob framing / encoding helpers live in the sibling `framing` module.
use super::framing::{FrameScratch, PIPELINE_SCRATCH, encode_blob_body, frame_blob_into};
pub(crate) use super::framing::{
    encode_blob_header_into, frame_blob, frame_blob_pipelined, reframe_raw_with_index,
};

// Pipeline plumbing (ordered channel items, sink trait, permit pool, writer
// thread) lives in the sibling `pipeline` module.
use super::pipeline::{
    FileOutputSink, OutputChunk, PipelineItem, WRITE_AHEAD, WritePipeline, elapsed_ns_u64,
    new_permit_pool, record_send_wait, writer_thread,
};

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
    ///
    /// # Latent blocking scenario
    ///
    /// Startup waits on `init_rx.recv()` until the writer thread either
    /// sends an init result or drops its sender. If a buggy kernel left
    /// the writer thread wedged inside a uring setup syscall
    /// (`register_buffers`, `register_files`) without ever returning, this
    /// recv blocks indefinitely. Not reached on any observed kernel; the
    /// correct remediation, if ever needed, is `recv_timeout` - but
    /// picking a value is fraught (too short kills slow-init on a loaded
    /// host, too long doesn't help the wedged-kernel case) and should be
    /// driven by a real reproducer rather than speculation.
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

    /// Create a pipelined `PbfWriter` that fans disk writes out across
    /// a pool of pwrite-based worker threads on one shared file
    /// descriptor.
    ///
    /// Suited to the production `--compression none` + zstd:1 case
    /// where the single-threaded writer is the observed ceiling even
    /// with `--io-uring` (~1.49 GB/s of ~5 GB/s NVMe peak). The
    /// writer-thread still reorders items in global seq order; each
    /// WriteOp carries its final offset so pool workers run
    /// `pwrite` / `copy_file_range(out_offset)` independently.
    pub fn to_path_parallel(
        path: &Path,
        compression: Compression,
        header_block_bytes: &[u8],
    ) -> io::Result<Self> {
        use crate::write::parallel_writer;

        let framed_header = frame_blob("OSMHeader", header_block_bytes, &compression, None)?;

        let (init_tx, init_rx) = sync_channel(1);
        let (tx, rx) = sync_channel(WRITE_AHEAD);
        let path_owned = path.to_path_buf();
        let handle = std::thread::spawn(move || {
            parallel_writer::parallel_writer_thread(rx, path_owned, framed_header, init_tx)
        });

        // Propagate init errors eagerly - mirrors to_path_uring.
        match init_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                drop(tx);
                drop(handle.join());
                return Err(e);
            }
            Err(_) => {
                drop(tx);
                return Err(match handle.join() {
                    Ok(Ok(())) => {
                        io::Error::other("parallel writer thread exited without init signal")
                    }
                    Ok(Err(e)) => e,
                    Err(_) => io::Error::other("parallel writer thread panicked during init"),
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
            pipeline
                .permit_rx
                .recv()
                .map_err(|_| io::Error::other("pipelined writer permit pool disconnected"))?;
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
                let indexdata = blob_meta::scan_block_ids(&uncompressed).map(|idx| idx.serialize());
                let tagdata = blob_meta::scan_block_tags(&uncompressed).map(|ti| ti.serialize());
                let result = PIPELINE_SCRATCH.with_borrow_mut(|scratch| {
                    frame_blob_into(
                        "OSMData",
                        &uncompressed,
                        &compression,
                        indexdata.as_ref().map(<[u8; 42]>::as_slice),
                        tagdata.as_deref(),
                        None,
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
            let indexdata = blob_meta::scan_block_ids(block_bytes).map(|idx| idx.serialize());
            let tagdata = blob_meta::scan_block_tags(block_bytes).map(|ti| ti.serialize());
            self.write_framed_blob(
                "OSMData",
                block_bytes,
                indexdata.as_ref().map(<[u8; 42]>::as_slice),
                tagdata.as_deref(),
                None,
            )
        }
    }

    /// Write an `OSMData` blob without the `indexdata` / `tagdata`
    /// `BlobHeader` fields.
    ///
    /// `write_primitive_block` always scans the serialized block for the
    /// id range and present tag keys and emits both `indexdata` (field 2,
    /// 42-byte v2 blob index) and `tagdata` (field 3, tag bloom). Some
    /// consumers want to produce byte-for-byte PBFs matching third-party
    /// tools that do not emit either field, or to exercise read paths
    /// (`diff_element_stream`, `ElementReader` fallback) that only fire
    /// on non-indexed inputs. This method is the opt-out.
    ///
    /// The `PrimitiveBlock` payload itself is unchanged - only the outer
    /// `BlobHeader` differs. Readers that skip missing optional fields
    /// treat the output identically to an indexed blob.
    #[hotpath::measure]
    pub fn write_primitive_block_no_indexdata(&mut self, block_bytes: &[u8]) -> io::Result<()> {
        if let Some(ref mut pipeline) = self.pipeline {
            let t_permit = std::time::Instant::now();
            pipeline
                .permit_rx
                .recv()
                .map_err(|_| io::Error::other("pipelined writer permit pool disconnected"))?;
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
                let result = PIPELINE_SCRATCH.with_borrow_mut(|scratch| {
                    frame_blob_into(
                        "OSMData",
                        &uncompressed,
                        &compression,
                        None,
                        None,
                        None,
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
                permit_tx.send(()).ok();
            });
            Ok(())
        } else {
            self.write_framed_blob("OSMData", block_bytes, None, None, None)
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
    ///
    /// The optional `way_members` payload is emitted as `BlobHeader` field 5
    /// (`pbfhogg.WayMembers-v1`); all non-enrichment callers pass `None`.
    #[hotpath::measure]
    pub(crate) fn write_primitive_block_owned(
        &mut self,
        block_bytes: Vec<u8>,
        index: blob_meta::BlobIndex,
        tagdata: Option<&[u8]>,
        way_members: Option<&[u8]>,
    ) -> io::Result<()> {
        let indexdata = index.serialize();
        if let Some(ref mut pipeline) = self.pipeline {
            // Bound in-flight rayon dispatches - see the sibling
            // `write_primitive_block` above and the
            // `PIPELINE_DISPATCH_PERMITS` doc comment for why.
            let t_permit = std::time::Instant::now();
            pipeline
                .permit_rx
                .recv()
                .map_err(|_| io::Error::other("pipelined writer permit pool disconnected"))?;
            WRITER_METRICS
                .permit_wait_ns
                .fetch_add(elapsed_ns_u64(t_permit), Relaxed);
            let seq = pipeline.seq;
            pipeline.seq += 1;
            let compression = self.compression;
            let tx = pipeline.tx.clone();
            let tagdata_owned = tagdata.map(<[u8]>::to_vec);
            let way_members_owned = way_members.map(<[u8]>::to_vec);
            let permit_tx = pipeline.permit_tx.clone();
            rayon::spawn(move || {
                let result = PIPELINE_SCRATCH.with_borrow_mut(|scratch| {
                    frame_blob_into(
                        "OSMData",
                        &block_bytes,
                        &compression,
                        Some(indexdata.as_slice()),
                        tagdata_owned.as_deref(),
                        way_members_owned.as_deref(),
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
                way_members,
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
    #[hotpath::measure]
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
        self.write_framed_blob(blob_type, uncompressed, None, None, None)
    }

    /// Encode, compress, and write a blob directly to the writer using reusable
    /// scratch buffers. Eliminates all intermediate `Vec` allocations after warmup.
    fn write_framed_blob(
        &mut self,
        blob_type: &str,
        uncompressed: &[u8],
        indexdata: Option<&[u8]>,
        tagdata: Option<&[u8]>,
        way_members: Option<&[u8]>,
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
            way_members,
            &mut self.scratch.header_buf,
        )?;
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
                // Best-effort join. Any I/O error from the writer thread -
                // including the deferred `sync_all` and (for uring)
                // `set_len` truncation - is silently discarded here;
                // Drop can't surface errors. Callers that care about
                // durability MUST call `flush()` on the success path so
                // the join result is routed through `?`. Reaching Drop
                // unflushed means either a panic or an earlier-error
                // `?`-bailout; the primary error dominates.
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
            WRITER_METRICS
                .payload_copy_range_items
                .fetch_add(1, Relaxed);
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
