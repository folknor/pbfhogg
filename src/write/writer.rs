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
use crate::error::panic_error;
#[cfg(feature = "linux-direct-io")]
use crate::write::copy_range::copy_range;
use crate::write::file_writer::FileWriter;
use crate::write::metrics::WRITER_METRICS;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::sync_channel;

// Compression type moved to the sibling `compression` module; re-exported
// here to preserve the public `crate::writer::Compression` API.
pub use crate::write::compression::{Compression, ParseCompressionError};

// Blob framing / encoding helpers live in the sibling `framing` module.
use super::framing::{FrameScratch, PIPELINE_SCRATCH, encode_blob_body, frame_blob_into};
pub(crate) use super::framing::{
    encode_blob_header_into, frame_blob, frame_blob_pipelined, reframe_raw_with_index,
    strip_blob_header_fields, strip_header_block_fields,
};

// Pipeline plumbing (ordered channel items, sink trait, permit pool, writer
// thread) lives in the sibling `pipeline` module.
use super::pipeline::{
    FileOutputSink, OutputChunk, PermitGuard, PipelineItem, SeqTotal, WRITE_AHEAD, WritePipeline,
    drain_permits, elapsed_ns_u64, record_send_wait, recv_permit, writer_thread,
};
use crate::host::{Diagnostics, Executor};

/// Returned when a write is attempted on a `PbfWriter` whose inner writer was
/// moved into a pipeline writer thread (every pipelined constructor, and every
/// writer after `flush()`).
const WRITER_CONSUMED: &str = "PbfWriter: the pipelined writer owns the output file; \
                               no further writes are possible after flush()";

/// Recover the writer thread's own error after a pipeline `send` failed.
///
/// A `send` on the pipeline channel only fails because the receiver was
/// dropped, which means the writer thread has already exited - so the join
/// returns immediately and carries the real cause: ENOSPC, EIO, a short uring
/// write, a reorder gap. Every one of these call sites used to substitute
/// `io::Error::other("writer thread terminated")`, the caller `?`-bailed on
/// that, `Drop` discarded the join result, and the only description of what
/// actually went wrong was gone. Wrapping the innermost real cause is fine;
/// replacing it is not.
///
/// Takes the join handle, so a later `flush()` will not try to join twice; the
/// error it recovered has already been returned to the caller by then.
fn pipeline_send_error(pipeline: &mut WritePipeline) -> io::Error {
    match pipeline.join_handle.take() {
        Some(handle) => match handle.join() {
            Ok(Err(e)) => e,
            Ok(Ok(())) => io::Error::other(
                "writer thread finished before all blobs were sent; output is incomplete",
            ),
            Err(payload) => panic_error("writer thread", payload.as_ref()),
        },
        // Already joined - flush() or an earlier send failure took the handle
        // and surfaced whatever it found.
        None => io::Error::other("writer thread terminated"),
    }
}

fn flush_wrapped_writer<W: Write>(writer: &mut W) -> io::Result<()> {
    writer.flush()
}

fn sync_buffered_file(writer: &mut BufWriter<File>) -> io::Result<()> {
    writer.flush()?;
    let t_sync = std::time::Instant::now();
    let result = crate::write::file_writer::sync_file(writer.get_ref());
    WRITER_METRICS.record_sync_all(t_sync);
    result
}

/// Take one dispatch permit, blocking until one is free, and return the guard
/// that gives it back.
///
/// Used by the producer's own send paths (`write_raw*`). The rayon framing
/// closures build their [`PermitGuard`] inside the closure instead, because
/// the permit has to outlive the spawn rather than the call.
///
/// Every send path participates in the dispatch bound, including raw writes;
/// see [`WRITE_AHEAD`] for how channel backpressure differs from that bound.
fn acquire_permit(pipeline: &WritePipeline) -> io::Result<PermitGuard> {
    take_permit(pipeline)?;
    Ok(PermitGuard::new(pipeline.permit_tx.clone()))
}

/// Take one dispatch permit through [`recv_permit`], timing the wait into
/// `writer_permit_wait_ns`. The caller owns giving it back.
fn take_permit(pipeline: &WritePipeline) -> io::Result<()> {
    let t_permit = std::time::Instant::now();
    recv_permit(&pipeline.permit_rx, &pipeline.executor)
        .map_err(|_| io::Error::other("pipelined writer permit pool disconnected"))?;
    WRITER_METRICS
        .permit_wait_ns
        .fetch_add(elapsed_ns_u64(t_permit), Relaxed);
    Ok(())
}

/// Test hook: whether the framing closure for `seq` should panic.
#[cfg(feature = "test-hooks")]
fn framing_panics_at(pipeline: &WritePipeline, seq: usize) -> bool {
    pipeline.framing_panic_at == Some(seq)
}
#[cfg(not(feature = "test-hooks"))]
fn framing_panics_at(_pipeline: &WritePipeline, _seq: usize) -> bool {
    false
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
    /// Final operation for the owned synchronous writer. Generic writers only
    /// flush; the path-owned sync constructor also synchronizes the file.
    finalize_writer: fn(&mut W) -> io::Result<()>,
    #[cfg(test)]
    /// Whether all successfully accepted writes have been explicitly flushed.
    finished: bool,
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
    ///
    /// `executor` is where the framing work runs; [`Executor::Ambient`] is
    /// rayon's ambient pool. See [`Executor`] for what handing the writer a
    /// pool of your own means.
    pub fn to_path(
        path: &Path,
        compression: Compression,
        header_block_bytes: &[u8],
        executor: Executor,
    ) -> io::Result<Self> {
        Self::to_path_in(
            path,
            compression,
            header_block_bytes,
            executor,
            Diagnostics::discard(),
        )
    }

    pub(crate) fn to_path_in(
        path: &Path,
        compression: Compression,
        header_block_bytes: &[u8],
        executor: Executor,
        diagnostics: Diagnostics,
    ) -> io::Result<Self> {
        // The buffered backend's writer thread reports nothing.
        drop(diagnostics);
        let writer = FileWriter::buffered(path)?;
        Self::start_pipeline(writer, compression, header_block_bytes, executor)
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
        executor: Executor,
    ) -> io::Result<Self> {
        Self::to_path_direct_in(
            path,
            compression,
            header_block_bytes,
            executor,
            Diagnostics::discard(),
        )
    }

    #[cfg(feature = "linux-direct-io")]
    pub(crate) fn to_path_direct_in(
        path: &Path,
        compression: Compression,
        header_block_bytes: &[u8],
        executor: Executor,
        diagnostics: Diagnostics,
    ) -> io::Result<Self> {
        // The O_DIRECT backend's writer thread reports nothing.
        drop(diagnostics);
        let writer = FileWriter::direct(path)?;
        Self::start_pipeline(writer, compression, header_block_bytes, executor)
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
        executor: Executor,
    ) -> io::Result<Self> {
        Self::to_path_uring_in(
            path,
            compression,
            header_block_bytes,
            executor,
            Diagnostics::discard(),
        )
    }

    #[cfg(feature = "linux-io-uring")]
    pub(crate) fn to_path_uring_in(
        path: &Path,
        compression: Compression,
        header_block_bytes: &[u8],
        executor: Executor,
        diagnostics: Diagnostics,
    ) -> io::Result<Self> {
        use crate::write::uring_writer;

        let framed_header = frame_blob("OSMHeader", header_block_bytes, &compression, None)?;

        // Oneshot channel for init errors from the writer thread.
        let (init_tx, init_rx) = sync_channel(1);

        let (tx, rx) = sync_channel(WRITE_AHEAD);
        let path_owned = path.to_path_buf();
        let expected = SeqTotal::new();
        let thread_expected = expected.clone();
        let handle = std::thread::spawn(move || {
            uring_writer::uring_writer_thread(
                rx,
                path_owned,
                framed_header,
                init_tx,
                &thread_expected,
                diagnostics,
            )
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
                    Err(payload) => panic_error("writer thread (during init)", payload.as_ref()),
                });
            }
        }

        Ok(PbfWriter {
            writer: None,
            compression,
            pipeline: Some(WritePipeline::new(tx, handle, expected, executor)),
            finalize_writer: flush_wrapped_writer::<FileWriter>,
            #[cfg(test)]
            finished: false,
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
        executor: Executor,
    ) -> io::Result<Self> {
        Self::to_path_parallel_in(
            path,
            compression,
            header_block_bytes,
            executor,
            Diagnostics::discard(),
        )
    }

    pub(crate) fn to_path_parallel_in(
        path: &Path,
        compression: Compression,
        header_block_bytes: &[u8],
        executor: Executor,
        diagnostics: Diagnostics,
    ) -> io::Result<Self> {
        use crate::write::parallel_writer;

        let framed_header = frame_blob("OSMHeader", header_block_bytes, &compression, None)?;

        let (init_tx, init_rx) = sync_channel(1);
        let (tx, rx) = sync_channel(WRITE_AHEAD);
        let path_owned = path.to_path_buf();
        let expected = SeqTotal::new();
        let thread_expected = expected.clone();
        let handle = std::thread::spawn(move || {
            parallel_writer::parallel_writer_thread(
                rx,
                path_owned,
                framed_header,
                init_tx,
                &thread_expected,
                diagnostics,
            )
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
                    Err(payload) => {
                        panic_error("parallel writer thread (during init)", payload.as_ref())
                    }
                });
            }
        }

        Ok(PbfWriter {
            writer: None,
            compression,
            pipeline: Some(WritePipeline::new(tx, handle, expected, executor)),
            finalize_writer: flush_wrapped_writer::<FileWriter>,
            #[cfg(test)]
            finished: false,
            scratch: FrameScratch::new(),
        })
    }

    /// Shared pipelined setup: write header, spawn writer thread.
    fn start_pipeline(
        mut writer: FileWriter,
        compression: Compression,
        header_block_bytes: &[u8],
        executor: Executor,
    ) -> io::Result<Self> {
        // Write header synchronously before starting the pipeline.
        let framed_header = frame_blob("OSMHeader", header_block_bytes, &compression, None)?;
        writer.write_all(&framed_header)?;

        // Spawn the writer thread and hand it the writer wrapped in a sink.
        let (tx, rx) = sync_channel(WRITE_AHEAD);
        let expected = SeqTotal::new();
        let thread_expected = expected.clone();
        let handle = std::thread::spawn(move || {
            writer_thread(rx, FileOutputSink::new(writer), &thread_expected)
        });

        Ok(PbfWriter {
            writer: None,
            compression,
            pipeline: Some(WritePipeline::new(tx, handle, expected, executor)),
            finalize_writer: flush_wrapped_writer::<FileWriter>,
            #[cfg(test)]
            finished: false,
            scratch: FrameScratch::new(),
        })
    }
}

impl<W: Write> PbfWriter<W> {
    /// Create a new `PbfWriter` wrapping the given writer.
    ///
    /// If `writer` is backed by a file, callers should wrap it in
    /// `BufWriter` with a 256 KiB capacity for best performance.
    /// PBF blobs are typically 16-64KB compressed, so the default 8KB
    /// `BufWriter` causes excessive write syscalls. See [`to_path`](Self::to_path)
    /// which applies this automatically. For synchronous file output that
    /// owns this buffer and synchronizes it on `flush`, use
    /// [`to_path_sync`](PbfWriter::to_path_sync).
    ///
    /// This constructor calls the supplied writer's ordinary `Write::flush`.
    /// It accepts arbitrary writers, including non-file outputs, so file
    /// durability is opt-in through [`to_path_sync`](PbfWriter::to_path_sync)
    /// and generic callers do not incur an implicit `fsync`.
    pub fn new(writer: W, compression: Compression) -> Self {
        PbfWriter {
            writer: Some(writer),
            compression,
            pipeline: None,
            finalize_writer: flush_wrapped_writer::<W>,
            #[cfg(test)]
            finished: true,
            scratch: FrameScratch::new(),
        }
    }

    #[cfg(test)]
    #[inline(always)]
    fn mark_pending(&mut self) {
        self.finished = false;
    }

    #[cfg(not(test))]
    #[inline(always)]
    fn mark_pending(&mut self) {}

    #[cfg(test)]
    #[inline(always)]
    fn mark_finished(&mut self) {
        self.finished = true;
    }

    #[cfg(not(test))]
    #[inline(always)]
    fn mark_finished(&mut self) {}

    /// Test hook: make the framing work for the `seq`-th data blob (0-based,
    /// counting every blob sent through the pipeline) panic before it frames.
    /// Per instance; a no-op on a sync [`PbfWriter::new`] writer, which has no
    /// framing work.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn test_panic_in_framing_at(&mut self, seq: usize) {
        if let Some(pipeline) = self.pipeline.as_mut() {
            pipeline.framing_panic_at = Some(seq);
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
        let result = self.write_blob("OSMHeader", header_block_bytes);
        if result.is_ok() {
            self.mark_pending();
        }
        result
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
            // A framing closure already found the writer thread gone. Join
            // it now and return its real cause, rather than framing the rest
            // of the dataset into a channel nobody is reading. See
            // `WriterGone`.
            if pipeline.writer_gone.observed() {
                return Err(pipeline_send_error(pipeline));
            }
            take_permit(pipeline)?;
            let seq = pipeline.seq;
            pipeline.seq += 1;
            let compression = self.compression;
            let uncompressed = block_bytes.to_vec();
            let tx = pipeline.tx.clone();
            let permit_tx = pipeline.permit_tx.clone();
            let writer_gone = pipeline.writer_gone.clone();
            let panic_here = framing_panics_at(pipeline, seq);
            pipeline.executor.spawn(move || {
                // Constructed first so it drops last - the permit is released
                // after `tx.send` below, and also on the unwind path if any
                // of the framing work panics. See `PermitGuard`.
                let _permit = PermitGuard::new(permit_tx);
                assert!(!panic_here, "test hook: framing panic at seq {seq}");
                // Keep scanner failures on the existing ordered error channel.
                // Scanning still happens once, inside the framing worker.
                let result =
                    blob_meta::scan_block_ids_and_tags(&uncompressed).and_then(|(index, tags)| {
                        let indexdata = index.map(|idx| idx.serialize());
                        let tagdata = tags.and_then(|ti| ti.serialize());
                        PIPELINE_SCRATCH.with_borrow_mut(|scratch| {
                            frame_blob_into(
                                "OSMData",
                                &uncompressed,
                                &compression,
                                indexdata.as_ref().map(<[u8; 42]>::as_slice),
                                tagdata.as_deref(),
                                None,
                                scratch,
                            )
                        })
                    });
                if let Ok(ref parts) = result {
                    WRITER_METRICS.payload_framed_items.fetch_add(1, Relaxed);
                    WRITER_METRICS
                        .payload_framed_bytes
                        .fetch_add(parts.total_len(), Relaxed);
                }
                let t_send = std::time::Instant::now();
                // A failed send means the writer thread has already exited,
                // and this closure has no `Result` to return it in. Record it
                // so the producer's next call can join and surface the real
                // cause instead of grinding on to `flush()`. See `WriterGone`.
                if tx
                    .send(PipelineItem {
                        seq,
                        data: result.map(OutputChunk::Framed),
                    })
                    .is_err()
                {
                    writer_gone.record();
                }
                record_send_wait(t_send);
            });
            self.mark_pending();
            Ok(())
        } else {
            let (index, tags) = blob_meta::scan_block_ids_and_tags(block_bytes)?;
            let indexdata = index.map(|idx| idx.serialize());
            let tagdata = tags.and_then(|ti| ti.serialize());
            let result = self.write_framed_blob(
                "OSMData",
                block_bytes,
                indexdata.as_ref().map(<[u8; 42]>::as_slice),
                tagdata.as_deref(),
                None,
            );
            if result.is_ok() {
                self.mark_pending();
            }
            result
        }
    }

    /// Write an `OSMData` blob without the `indexdata` / `tagdata`
    /// `BlobHeader` fields.
    ///
    /// `write_primitive_block` always scans the serialized block for the
    /// id range and present tag keys and emits both `indexdata` (field 2,
    /// 42-byte v3 blob index) and `tagdata` (field 4, tag key index). Some
    /// consumers want to produce byte-for-byte PBFs matching third-party
    /// tools that do not emit either field, or to exercise read paths
    /// (`diff_element_stream`, `ElementReader` fallback) that only fire
    /// on non-indexed inputs. This method is the opt-out.
    ///
    /// The `PrimitiveBlock` payload itself is unchanged - only the outer
    /// `BlobHeader` differs. Readers that skip missing optional fields
    /// treat the output identically to an indexed blob.
    // Keep this public: emitting standard PBF without private indexes is a
    // supported writer operation, including for downstream fixture producers.
    #[hotpath::measure]
    pub fn write_primitive_block_no_indexdata(&mut self, block_bytes: &[u8]) -> io::Result<()> {
        if let Some(ref mut pipeline) = self.pipeline {
            // A framing closure already found the writer thread gone. Join
            // it now and return its real cause, rather than framing the rest
            // of the dataset into a channel nobody is reading. See
            // `WriterGone`.
            if pipeline.writer_gone.observed() {
                return Err(pipeline_send_error(pipeline));
            }
            take_permit(pipeline)?;
            let seq = pipeline.seq;
            pipeline.seq += 1;
            let compression = self.compression;
            let uncompressed = block_bytes.to_vec();
            let tx = pipeline.tx.clone();
            let permit_tx = pipeline.permit_tx.clone();
            let writer_gone = pipeline.writer_gone.clone();
            let panic_here = framing_panics_at(pipeline, seq);
            pipeline.executor.spawn(move || {
                // Drops last - releases the permit after `tx.send`, and on
                // the unwind path too. See `PermitGuard`.
                let _permit = PermitGuard::new(permit_tx);
                assert!(!panic_here, "test hook: framing panic at seq {seq}");
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
                // A failed send means the writer thread has already exited,
                // and this closure has no `Result` to return it in. Record it
                // so the producer's next call can join and surface the real
                // cause instead of grinding on to `flush()`. See `WriterGone`.
                if tx
                    .send(PipelineItem {
                        seq,
                        data: result.map(OutputChunk::Framed),
                    })
                    .is_err()
                {
                    writer_gone.record();
                }
                record_send_wait(t_send);
            });
            self.mark_pending();
            Ok(())
        } else {
            let result = self.write_framed_blob("OSMData", block_bytes, None, None, None);
            if result.is_ok() {
                self.mark_pending();
            }
            result
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
            // A framing closure already found the writer thread gone. Join
            // it now and return its real cause, rather than framing the rest
            // of the dataset into a channel nobody is reading. See
            // `WriterGone`.
            if pipeline.writer_gone.observed() {
                return Err(pipeline_send_error(pipeline));
            }
            take_permit(pipeline)?;
            let seq = pipeline.seq;
            pipeline.seq += 1;
            let compression = self.compression;
            let tx = pipeline.tx.clone();
            let tagdata_owned = tagdata.map(<[u8]>::to_vec);
            let way_members_owned = way_members.map(<[u8]>::to_vec);
            let permit_tx = pipeline.permit_tx.clone();
            let writer_gone = pipeline.writer_gone.clone();
            let panic_here = framing_panics_at(pipeline, seq);
            pipeline.executor.spawn(move || {
                // Drops last - releases the permit after `tx.send`, and on
                // the unwind path too. See `PermitGuard`.
                let _permit = PermitGuard::new(permit_tx);
                assert!(!panic_here, "test hook: framing panic at seq {seq}");
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
                // A failed send means the writer thread has already exited,
                // and this closure has no `Result` to return it in. Record it
                // so the producer's next call can join and surface the real
                // cause instead of grinding on to `flush()`. See `WriterGone`.
                if tx
                    .send(PipelineItem {
                        seq,
                        data: result.map(OutputChunk::Framed),
                    })
                    .is_err()
                {
                    writer_gone.record();
                }
                record_send_wait(t_send);
            });
            self.mark_pending();
            Ok(())
        } else {
            let result = self.write_framed_blob(
                "OSMData",
                &block_bytes,
                Some(indexdata.as_slice()),
                tagdata,
                way_members,
            );
            if result.is_ok() {
                self.mark_pending();
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
            // Hold a dispatch permit across the send so raw writes share the
            // dispatch bound with framing tasks. The send can still block;
            // see `WRITE_AHEAD`.
            let _permit = acquire_permit(pipeline)?;
            let seq = pipeline.seq;
            pipeline.seq += 1;
            WRITER_METRICS.payload_raw_items.fetch_add(1, Relaxed);
            WRITER_METRICS
                .payload_raw_bytes
                .fetch_add(raw_framed_bytes.len() as u64, Relaxed);
            let t_send = std::time::Instant::now();
            let sent = pipeline.tx.send(PipelineItem {
                seq,
                data: Ok(OutputChunk::Raw(raw_framed_bytes.to_vec())),
            });
            if sent.is_err() {
                return Err(pipeline_send_error(pipeline));
            }
            record_send_wait(t_send);
            self.mark_pending();
            Ok(())
        } else {
            let result = self.writer_mut()?.write_all(raw_framed_bytes);
            if result.is_ok() {
                self.mark_pending();
            }
            result
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
            // Hold a dispatch permit across the send so raw writes share the
            // dispatch bound with framing tasks. The send can still block;
            // see `WRITE_AHEAD`.
            let _permit = acquire_permit(pipeline)?;
            let seq = pipeline.seq;
            pipeline.seq += 1;
            WRITER_METRICS.payload_raw_items.fetch_add(1, Relaxed);
            WRITER_METRICS
                .payload_raw_bytes
                .fetch_add(raw_framed_bytes.len() as u64, Relaxed);
            let t_send = std::time::Instant::now();
            let sent = pipeline.tx.send(PipelineItem {
                seq,
                data: Ok(OutputChunk::Raw(raw_framed_bytes)),
            });
            if sent.is_err() {
                return Err(pipeline_send_error(pipeline));
            }
            record_send_wait(t_send);
            self.mark_pending();
            Ok(())
        } else {
            let result = self.writer_mut()?.write_all(&raw_framed_bytes);
            if result.is_ok() {
                self.mark_pending();
            }
            result
        }
    }

    /// Write multiple pre-framed raw blob chunks without concatenating them.
    ///
    /// Like [`write_raw_owned`](Self::write_raw_owned) but accepts a list of
    /// owned chunks. The writer thread writes each chunk sequentially.
    /// Used by passthrough coalescers to avoid `extend_from_slice` memcpy.
    pub fn write_raw_chunks(&mut self, chunks: Vec<Vec<u8>>) -> io::Result<()> {
        if let Some(ref mut pipeline) = self.pipeline {
            // Hold a dispatch permit across the send so raw writes share the
            // dispatch bound with framing tasks. The send can still block;
            // see `WRITE_AHEAD`.
            let _permit = acquire_permit(pipeline)?;
            let seq = pipeline.seq;
            pipeline.seq += 1;
            let total_bytes: u64 = chunks.iter().map(|c| c.len() as u64).sum();
            WRITER_METRICS.payload_raw_chunk_items.fetch_add(1, Relaxed);
            WRITER_METRICS
                .payload_raw_chunk_bytes
                .fetch_add(total_bytes, Relaxed);
            let t_send = std::time::Instant::now();
            let sent = pipeline.tx.send(PipelineItem {
                seq,
                data: Ok(OutputChunk::RawChunks(chunks)),
            });
            if sent.is_err() {
                return Err(pipeline_send_error(pipeline));
            }
            record_send_wait(t_send);
            self.mark_pending();
            Ok(())
        } else {
            let w = self.writer_mut()?;
            for chunk in &chunks {
                w.write_all(chunk)?;
            }
            self.mark_pending();
            Ok(())
        }
    }

    /// Flush the underlying writer.
    ///
    /// In pipelined mode, this joins the writer thread and propagates any
    /// deferred compression or I/O errors.
    ///
    /// `flush` finishes the writer. It does not reopen a direct path: a
    /// pipelined `PbfWriter` never held the output file in the first place
    /// (every pipelined constructor moves it into the writer thread), so
    /// there is nothing for a later write to fall back to. Any write after
    /// `flush` on a pipelined writer fails with "the pipelined writer owns
    /// the output file". Calling `flush` twice is harmless.
    ///
    /// After successful writes, `flush` checks completion and reports deferred
    /// errors. File-backed pipelines and [`to_path_sync`](PbfWriter::to_path_sync)
    /// also sync the file (on Linux, a buffered
    /// target that does not support `fsync` - a FIFO, socket or character
    /// device such as `/dev/null` - is not an error); a synchronous writer
    /// created with `new` only calls its underlying `Write::flush`, which does
    /// not guarantee durability. Dropping without flushing performs cleanup
    /// but cannot report errors or a missing tail.
    #[hotpath::measure]
    pub fn flush(&mut self) -> io::Result<()> {
        self.mark_finished();
        let t_flush = std::time::Instant::now();
        if let Some(mut pipeline) = self.pipeline.take() {
            // State the final item count BEFORE dropping the sender, so the
            // store is ordered ahead of the disconnect the writer thread
            // observes. Without it the writer cannot tell a complete stream
            // from one whose last framing task died: both close a channel
            // with an empty reorder buffer. See `SeqTotal`.
            pipeline.expected.announce(pipeline.seq);
            // Drop sender to signal the writer thread that no more items are coming.
            drop(pipeline.tx);
            // On a worker of the executor's pool, let the queued framing
            // closures run before blocking in the join; see `drain_permits`.
            // Off-worker (every CLI path) this is skipped.
            if pipeline.executor.on_worker() {
                drain_permits(&pipeline.permit_rx, &pipeline.executor);
            }
            if let Some(handle) = pipeline.join_handle.take() {
                handle
                    .join()
                    .map_err(|payload| panic_error("writer thread", payload.as_ref()))??;
            }
        }
        if let Some(ref mut w) = self.writer {
            (self.finalize_writer)(w)?;
        }
        WRITER_METRICS
            .flush_ns
            .fetch_add(elapsed_ns_u64(t_flush), Relaxed);
        // Deliberately does NOT emit the writer counters. `WRITER_METRICS` is
        // process-global, so emitting here made a multi-writer command
        // (`multi-extract` opens one writer per region) publish the same
        // running process totals once per region flush, which reads as a
        // per-region breakdown and is not one. The counters are emitted once
        // per process instead - `pbfhogg::write::emit_writer_counters`, which
        // the CLI calls on its single exit path.
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
        self.mark_finished();
        self.writer.take().expect("writer consumed by pipeline")
    }

    /// The inner writer, for the non-pipelined path.
    ///
    /// `writer` is `None` for every pipelined constructor (the file was moved
    /// into the writer thread) and stays `None` after `flush()` joins that
    /// thread. Reaching here in that state means a write was attempted on a
    /// finished pipelined writer - which used to panic out of a public `&mut
    /// self` method. It is a caller error, but a caller error on a public API
    /// is an error to return, not a panic.
    fn writer_mut(&mut self) -> io::Result<&mut W> {
        self.writer
            .as_mut()
            .ok_or_else(|| io::Error::other(WRITER_CONSUMED))
    }

    // wontfix(type-no-stringly): blob_type is &str matching protobuf wire format;
    // only 2 constants ("OSMHeader"/"OSMData"), no real typo risk.
    #[hotpath::measure]
    fn write_blob(&mut self, blob_type: &str, uncompressed: &[u8]) -> io::Result<()> {
        self.write_framed_blob(blob_type, uncompressed, None, None, None)
    }

    /// Encode, compress, and write a blob directly to the writer using reusable
    /// scratch buffers. Eliminates all intermediate `Vec` allocations after
    /// warmup, except that a scratch buffer grown past the framing module's
    /// 4 MiB retention limit is dropped after its blob and regrown on demand.
    fn write_framed_blob(
        &mut self,
        blob_type: &str,
        uncompressed: &[u8],
        indexdata: Option<&[u8]>,
        tagdata: Option<&[u8]>,
        way_members: Option<&[u8]>,
    ) -> io::Result<()> {
        self.scratch.trim_oversize_retained_buffers();
        let result =
            self.write_framed_blob_inner(blob_type, uncompressed, indexdata, tagdata, way_members);
        self.scratch.trim_oversize_retained_buffers();
        result
    }

    fn write_framed_blob_inner(
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
        // Same guard as `frame_blob_into`: the format caps a Blob body at
        // `MAX_BLOB_DATASIZE` and our own reader enforces it, so the sync
        // path must refuse here too or a `PbfWriter::new` writer emits a file
        // that only fails when somebody tries to read it.
        if self.scratch.blob_buf.len() as u64 >= crate::read::blob_wire::MAX_BLOB_DATASIZE {
            return Err(io::Error::other(format!(
                "blob body for {blob_type} is {} bytes, must be smaller than \
                 {} (lower --elements-per-blob)",
                self.scratch.blob_buf.len(),
                crate::read::blob_wire::MAX_BLOB_DATASIZE
            )));
        }
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
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::other(WRITER_CONSUMED))?;
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

impl PbfWriter<BufWriter<File>> {
    /// Create a synchronous writer that owns a 256 KiB buffered output file.
    ///
    /// The returned writer has no header yet. Call
    /// [`write_header`](Self::write_header), write its blocks, then call
    /// [`flush`](Self::flush). `flush` pushes the buffered bytes onward and
    /// synchronizes the file. Use [`PbfWriter::new`] when the caller owns an
    /// arbitrary `Write` value and only needs its ordinary `Write::flush`
    /// behavior.
    pub fn to_path_sync(path: &Path, compression: Compression) -> io::Result<Self> {
        let file = File::create(path)?;
        let buffered = BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file);
        let mut writer = Self::new(buffered, compression);
        writer.finalize_writer = sync_buffered_file;
        Ok(writer)
    }
}

/// Cleanup only: joins the writer thread and reports nothing. A writer that
/// was not [`flush`](PbfWriter::flush)ed may be missing its tail, and `Drop`
/// cannot report the deferred error. Dropping unflushed on an error return is
/// allowed: the caller's primary error dominates.
///
/// The crate's own unit-test builds assert after cleanup when accepted writes
/// reach `Drop` without a flush on a non-panicking path, so a forgotten flush
/// in those tests fails loudly. The check is deliberately not a plain
/// `debug_assertions` one: `Drop` cannot tell a forgotten flush from an
/// ordinary `?` bail-out, and in any debug build (the CLI under the test
/// sweeps, a library consumer's debug build) it would replace the caller's
/// real error with a panic on every error path.
impl<W: Write> Drop for PbfWriter<W> {
    fn drop(&mut self) {
        #[cfg(test)]
        let finished = self.finished;
        if let Some(mut pipeline) = self.pipeline.take() {
            drop(pipeline.tx);
            // As in `flush`: on a worker of the executor's pool the queued
            // framing closures must run before the join can return.
            if pipeline.executor.on_worker() {
                drain_permits(&pipeline.permit_rx, &pipeline.executor);
            }
            if let Some(handle) = pipeline.join_handle.take() {
                // Best-effort join. Any I/O error from the writer thread -
                // including the deferred `sync_all` and (for uring)
                // `set_len` truncation - is silently discarded here;
                // Drop can't surface errors. Callers that care about
                // durability MUST call `flush()` so the join result is
                // routed through `?`. Reaching Drop unflushed means a panic,
                // an earlier-error `?` bail-out (the primary error
                // dominates), or a forgotten flush, which only the unit-test
                // assertion below catches.
                drop(handle.join());
            }
        }
        #[cfg(test)]
        debug_assert!(
            std::thread::panicking() || finished,
            "PbfWriter dropped with unflushed output; call flush() to observe deferred errors"
        );
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
    /// `in_file` is the open input PBF, shared rather than borrowed: the
    /// pipelined path queues this request into the writer thread and returns
    /// immediately, so the input has to stay open until the splice actually
    /// runs. Passing a bare `RawFd` could not express that - see
    /// [`OutputChunk::CopyRange`](crate::write::pipeline::OutputChunk).
    /// `offset` and `len` describe the framed blob's position in the input file.
    ///
    /// Callers must not use this when the output writer is O_DIRECT - use
    /// [`write_raw`](Self::write_raw) instead.
    pub fn write_raw_copy(
        &mut self,
        in_file: &std::sync::Arc<std::fs::File>,
        offset: u64,
        len: u64,
    ) -> io::Result<()> {
        use std::os::unix::io::AsRawFd;
        if let Some(ref mut pipeline) = self.pipeline {
            // Hold a dispatch permit across the send so raw writes share the
            // dispatch bound with framing tasks. The send can still block;
            // see `WRITE_AHEAD`.
            let _permit = acquire_permit(pipeline)?;
            let seq = pipeline.seq;
            pipeline.seq += 1;
            WRITER_METRICS
                .payload_copy_range_items
                .fetch_add(1, Relaxed);
            WRITER_METRICS
                .payload_copy_range_bytes
                .fetch_add(len, Relaxed);
            let t_send = std::time::Instant::now();
            let sent = pipeline.tx.send(PipelineItem {
                seq,
                data: Ok(OutputChunk::CopyRange {
                    in_file: std::sync::Arc::clone(in_file),
                    offset,
                    len,
                }),
            });
            if sent.is_err() {
                return Err(pipeline_send_error(pipeline));
            }
            record_send_wait(t_send);
            self.mark_pending();
            Ok(())
        } else {
            // This is reachable through FileWriter::buffered plus the public
            // generic PbfWriter::new constructor.
            let out_fd = self
                .writer
                .as_mut()
                .ok_or_else(|| io::Error::other(WRITER_CONSUMED))?
                .flush_and_raw_fd()?
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "copy_file_range incompatible with O_DIRECT output",
                    )
                })?;
            let result = copy_range(in_file.as_raw_fd(), out_fd, offset, len);
            if result.is_ok() {
                self.mark_pending();
            }
            result
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{Compression, Executor, PbfWriter, WritePipeline};
    use crate::write::framing::FrameScratch;
    use crate::write::pipeline::{PipelineItem, SeqTotal};
    use std::io;
    use std::sync::mpsc::sync_channel;

    #[test]
    fn malformed_primitive_block_is_not_written_without_an_index() {
        let mut writer = PbfWriter::new(Vec::new(), Compression::None);
        let err = writer.write_primitive_block(&[0x80]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // An empty block is unsupported by the index, but is not malformed.
        writer.write_primitive_block(&[]).unwrap();
        writer.flush().unwrap();
    }

    #[test]
    fn malformed_primitive_block_reaches_pipeline_flush() {
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("malformed.pbf");
        let header = crate::block_builder::HeaderBuilder::new().build().unwrap();
        let mut writer =
            PbfWriter::to_path(&path, Compression::None, &header, Executor::Ambient).unwrap();
        // The worker reports on the ordered channel; enqueue may succeed.
        let queued = writer.write_primitive_block(&[0x80]);
        let flushed = writer.flush();
        let result = queued.and(flushed);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    /// A `PbfWriter` whose writer thread has already exited with `outcome`.
    ///
    /// The receiver is dropped before the writer is handed back, so the very
    /// next send fails - which is exactly the shape of a writer thread that
    /// died on ENOSPC halfway through a planet write.
    fn writer_with_dead_thread(outcome: io::Result<()>) -> PbfWriter<Vec<u8>> {
        let (tx, rx) = sync_channel::<PipelineItem>(1);
        // Drop the receiver before the thread even starts, so every send is
        // guaranteed to fail without a race to synchronise against.
        drop(rx);
        let handle = std::thread::spawn(move || outcome);
        PbfWriter {
            writer: None,
            compression: Compression::None,
            pipeline: Some(WritePipeline::new(
                tx,
                handle,
                SeqTotal::new(),
                Executor::Ambient,
            )),
            finalize_writer: super::flush_wrapped_writer::<Vec<u8>>,
            #[cfg(test)]
            finished: false,
            scratch: FrameScratch::new(),
        }
    }

    /// The writer thread's real error must survive, not be replaced.
    ///
    /// Every send-failure path returned `io::Error::other("writer thread
    /// terminated")`. The caller `?`-bailed, `Drop` discarded the join
    /// result, and the only description of the actual failure - a full disk,
    /// a permission problem - was thrown away. The assertions here are on the
    /// kind and message the *thread* produced, which no synthetic substitute
    /// can supply.
    #[test]
    fn a_dead_writer_thread_surfaces_its_own_error_not_a_generic_one() {
        let mut w = writer_with_dead_thread(Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "no space left on device",
        )));
        let err = w.write_raw(b"blob").expect_err("the send must fail");
        assert_eq!(
            err.kind(),
            io::ErrorKind::StorageFull,
            "the ENOSPC kind must reach the caller"
        );
        assert!(
            err.to_string().contains("no space left on device"),
            "the thread's own message must reach the caller, got: {err}"
        );
        w.flush().unwrap();
    }

    /// A writer thread that exited cleanly mid-stream is still a short file.
    #[test]
    fn a_writer_thread_that_exited_clean_mid_stream_is_reported_as_incomplete() {
        let mut w = writer_with_dead_thread(Ok(()));
        let err = w.write_raw_owned(b"blob".to_vec()).expect_err("send fails");
        assert!(
            err.to_string().contains("output is incomplete"),
            "got: {err}"
        );
        w.flush().unwrap();
    }

    /// Writing after `flush` must be an error, not a panic.
    ///
    /// The `flush` doc promised "subsequent writes go through the direct
    /// (non-pipelined) path". For every pipelined constructor the file was
    /// moved into the writer thread, so `writer` is `None` and the documented
    /// recovery path hit `.expect("writer consumed by pipeline")`.
    #[test]
    fn writing_after_flush_on_a_pipelined_writer_errors_instead_of_panicking() {
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("post-flush.osm.pbf");
        let header = crate::block_builder::HeaderBuilder::new().build().unwrap();
        let mut w =
            PbfWriter::to_path(&path, Compression::None, &header, Executor::Ambient).unwrap();
        w.flush().unwrap();
        let err = w
            .write_raw(b"anything")
            .expect_err("the pipelined writer is finished; this must not panic");
        assert!(
            err.to_string().contains("no further writes are possible"),
            "got: {err}"
        );
    }

    /// A dead writer thread must stop `write_primitive_block` too, not just
    /// the `write_raw*` family.
    ///
    /// `write_primitive_block*` hands its framing to a closure spawned on the
    /// writer's `Executor` that used to end in `drop(tx.send(...))`, so a writer thread
    /// that died on ENOSPC was invisible until `flush()`. Nothing throttled
    /// the producer, because the permit pool releases on closure *completion*,
    /// not on delivery - a full disk let a whole planet-scale dataset be read,
    /// decoded and framed before anything was reported. The loop below is the
    /// producer grinding on: it must terminate with the writer thread's own
    /// error long before the cap.
    #[test]
    fn write_primitive_block_stops_once_the_writer_thread_is_dead() {
        let mut w = writer_with_dead_thread(Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "no space left on device",
        )));
        // A minimal but real PrimitiveBlock: an empty PrimitiveGroup (field 2).
        let block = [0x12u8, 0x00];
        let mut err = None;
        for _ in 0..2_000 {
            if let Err(e) = w.write_primitive_block(&block) {
                err = Some(e);
                break;
            }
            std::thread::yield_now();
        }
        let err = err.expect(
            "the producer must notice the dead writer thread; \
             it ran 2,000 blocks without a single report",
        );
        assert_eq!(
            err.kind(),
            io::ErrorKind::StorageFull,
            "the ENOSPC kind must reach the caller, not a generic substitute"
        );
        assert!(
            err.to_string().contains("no space left on device"),
            "the thread's own message must reach the caller, got: {err}"
        );
        w.flush().unwrap();
    }

    /// The sync (non-pipelined) path must refuse an oversize blob body too.
    ///
    /// The `MAX_BLOB_DATASIZE` guard first landed in `frame_blob_into`, which
    /// covers every pipelined constructor plus `frame_blob` - but
    /// `write_framed_blob` duplicates the framing inline for the
    /// `PbfWriter::new` sync path, so a sync-mode writer kept emitting a body
    /// the crate's own reader refuses ("blob header declares datasize too
    /// big"), with exit zero. Same cap, same knob-naming error message.
    #[test]
    fn the_sync_path_refuses_an_oversize_blob_body() {
        let cap = usize::try_from(crate::read::blob_wire::MAX_BLOB_DATASIZE).unwrap();
        // Incompressible only in the sense that Compression::None keeps the
        // body length; content is irrelevant to the size guard.
        let body = vec![0x5Au8; cap + 1];
        let mut w = PbfWriter::new(Vec::new(), Compression::None);
        let err = w
            .write_header(&body)
            .expect_err("a blob body past the datasize cap must not be written by the sync path");
        let msg = err.to_string();
        assert!(
            msg.contains("blob body for OSMHeader") && msg.contains("--elements-per-blob"),
            "the sync path must name the cap and the knob, got: {msg}"
        );
        w.flush().unwrap();
    }

    /// A queued `CopyRange` must survive the caller dropping its input handle.
    ///
    /// `write_raw_copy` used to take a bare `RawFd`, queued into the writer
    /// thread with no lifetime tie to the `File` that owned it. The caller
    /// closing the input before `flush()` - or simply letting the fd number be
    /// recycled by an unrelated `open` - meant the splice read whatever now
    /// sat at that descriptor: no error, wrong bytes, on the one path whose
    /// entire purpose is byte-exact passthrough.
    ///
    /// The decoy below is what turns that from a race into a certainty: Linux
    /// hands out the lowest free descriptor, so if dropping the handle really
    /// closed the input, the decoy takes its number and the splice copies the
    /// decoy's bytes.
    #[cfg(feature = "linux-direct-io")]
    #[test]
    fn a_queued_copy_range_outlives_the_callers_handle() {
        use std::sync::Arc;

        let dir = crate::path_guard::tests::scratch();
        let input_path = dir.path().join("input.bin");
        let decoy_path = dir.path().join("decoy.bin");
        let out_path = dir.path().join("out.osm.pbf");

        let payload: Vec<u8> = (0..8192u32)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        let decoy = vec![0xEEu8; payload.len()];
        std::fs::write(&input_path, &payload).unwrap();
        std::fs::write(&decoy_path, &decoy).unwrap();

        let header = crate::block_builder::HeaderBuilder::new().build().unwrap();
        let mut w =
            PbfWriter::to_path(&out_path, Compression::None, &header, Executor::Ambient).unwrap();

        let input = Arc::new(std::fs::File::open(&input_path).unwrap());
        w.write_raw_copy(&input, 0, payload.len() as u64).unwrap();

        // The caller is done with its handle. The writer thread has not
        // necessarily run the splice yet.
        drop(input);
        // Claim the descriptor the input would have vacated.
        let _decoy_handle = std::fs::File::open(&decoy_path).unwrap();

        w.flush().unwrap();

        let out = std::fs::read(&out_path).unwrap();
        assert!(
            out.ends_with(&payload),
            "the splice must copy the input the caller named, not whatever \
             later took its descriptor number"
        );
    }

    /// End-of-stream finalization must still `fsync`.
    ///
    /// `FileWriter::flush` stopped meaning `fsync` this round, and
    /// `FileOutputSink::flush` was changed to call the new `sync()` instead so
    /// that a completed write is still durable. Only the first half of that
    /// pair had a test: `flush_pushes_bytes_onward_without_fsyncing` pins
    /// `FileWriter`'s new behaviour, and nothing at all pinned the sink that
    /// has to ask for the durability the writer stopped providing. Revert
    /// `FileOutputSink::flush` to `self.writer.flush()` and every pipelined
    /// write silently loses its fsync with a green suite - which is precisely
    /// the "tested the producer, forgot the consumer" shape.
    #[test]
    fn end_of_stream_finalization_still_fsyncs() {
        use crate::write::metrics::WRITER_METRICS;
        use std::sync::atomic::Ordering::Relaxed;

        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("durable.osm.pbf");
        let header = crate::block_builder::HeaderBuilder::new().build().unwrap();

        let before = WRITER_METRICS.sync_all_calls.load(Relaxed);
        let mut w =
            PbfWriter::to_path(&path, Compression::None, &header, Executor::Ambient).unwrap();
        w.write_primitive_block(&[0x12u8, 0x00]).unwrap();
        w.flush().unwrap();
        assert!(
            WRITER_METRICS.sync_all_calls.load(Relaxed) > before,
            "a completed pipelined write must fsync its output; \
             FileOutputSink::flush has to call FileWriter::sync, not \
             FileWriter::flush"
        );
    }

    /// The same gap, on the owned-bytes and no-indexdata siblings.
    ///
    /// All three `write_primitive_block*` methods carry their own copy of the
    /// rayon-spawn body, so a fix applied to the representative one leaves the
    /// others silently broken. Every shape gets tested.
    #[test]
    fn every_primitive_block_variant_stops_once_the_writer_thread_is_dead() {
        let block = [0x12u8, 0x00];
        let index = crate::blob_meta::BlobIndex {
            kind: crate::blob_meta::ElemKind::Node,
            min_id: 0,
            max_id: 0,
            count: 0,
            bbox: None,
        };

        let mut w = writer_with_dead_thread(Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "owned variant: no space left on device",
        )));
        let mut err = None;
        for _ in 0..2_000 {
            if let Err(e) = w.write_primitive_block_owned(block.to_vec(), index, None, None) {
                err = Some(e);
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            err.is_some_and(|e| e.to_string().contains("owned variant")),
            "write_primitive_block_owned must surface the writer thread's error"
        );
        w.flush().unwrap();

        let mut w = writer_with_dead_thread(Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "no-indexdata variant: no space left on device",
        )));
        let mut err = None;
        for _ in 0..2_000 {
            if let Err(e) = w.write_primitive_block_no_indexdata(&block) {
                err = Some(e);
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            err.is_some_and(|e| e.to_string().contains("no-indexdata variant")),
            "write_primitive_block_no_indexdata must surface the writer thread's error"
        );
        w.flush().unwrap();
    }

    /// Cover the other framing entry points and the raw-send permit path on
    /// the same sole worker, both with flush and during panic cleanup.
    #[test]
    fn every_framing_variant_and_raw_send_completes_on_one_worker() {
        let pool = std::sync::Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .build()
                .unwrap(),
        );
        let dir = crate::path_guard::tests::scratch();
        let header = crate::block_builder::HeaderBuilder::new().build().unwrap();
        let block = [0x12u8, 0x00];
        let index = crate::blob_meta::BlobIndex {
            kind: crate::blob_meta::ElemKind::Node,
            min_id: 0,
            max_id: 0,
            count: 0,
            bbox: None,
        };
        let raw = super::frame_blob("OSMData", &block, &Compression::None, None).unwrap();
        for owned in [false, true] {
            for flush in [false, true] {
                let path = dir.path().join(format!("owned-{owned}-flush-{flush}.pbf"));
                let completion = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    pool.install(|| {
                        let mut writer = PbfWriter::to_path(
                            &path,
                            Compression::None,
                            &header,
                            Executor::Pool(std::sync::Arc::clone(&pool)),
                        )
                        .unwrap();
                        for _ in 0..200 {
                            if owned {
                                writer.write_primitive_block_owned(
                                    block.to_vec(),
                                    index,
                                    None,
                                    None,
                                )
                            } else {
                                writer.write_primitive_block_no_indexdata(&block)
                            }
                            .unwrap();
                        }
                        writer.write_raw(&raw).unwrap();
                        writer.write_raw_owned(raw.clone()).unwrap();
                        writer.write_raw_chunks(vec![raw.clone()]).unwrap();
                        if flush {
                            writer.flush().unwrap();
                        } else {
                            panic!("exercise PbfWriter cleanup during unwind");
                        }
                    });
                }));
                assert_eq!(completion.is_err(), !flush);
                let blobs = crate::BlobReader::from_path(&path)
                    .unwrap()
                    .map(|blob| blob.unwrap())
                    .filter(|blob| blob.get_type() == crate::BlobType::OsmData)
                    .count();
                assert_eq!(blobs, 203, "owned={owned}, flush={flush}");
            }
        }
    }
}
