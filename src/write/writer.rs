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
use crate::block_builder::OwnedBlock;
use crate::error::{Error, Result, panic_error};
use crate::output::Completion;
use crate::write::Finish;
use crate::write::copy_range::{CopyOut, copy_range};
use crate::write::file_writer::FileWriter;
use crate::write::metrics::WRITER_METRICS;
use std::borrow::Cow;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::sync_channel;

// Compression type moved to the sibling `compression` module; re-exported
// here to preserve the public `crate::writer::Compression` API.
pub use crate::write::compression::{Compression, ParseCompressionError};

// Blob framing / encoding helpers live in the sibling `framing` module.
use super::framing::{FrameScratch, PIPELINE_SCRATCH, frame_blob_into, frame_in_scratch};
pub(crate) use super::framing::{
    frame_blob, frame_blob_pipelined, reframe_raw_with_index, strip_blob_header_fields,
    strip_header_block_fields,
};

// Pipeline plumbing (ordered channel items, sink trait, permit pool, writer
// thread) lives in the sibling `pipeline` module.
use super::pipeline::{
    FileOutputSink, OutputChunk, PermitGuard, PipelineItem, SeqTotal, WritePipeline, drain_permits,
    record_send_wait, recv_permit, writer_thread,
};
use crate::host::{Diagnostics, Executor};
use crate::tunables::WRITE_AHEAD;

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
/// a generic "writer thread terminated" error, the caller `?`-bailed on
/// that, `Drop` discarded the join result, and the only description of what
/// actually went wrong was gone. Wrapping the innermost real cause is fine;
/// replacing it is not.
///
/// Takes the join handle, so a later `flush()` will not try to join twice; the
/// error it recovered has already been returned to the caller by then.
fn pipeline_send_error(pipeline: &mut WritePipeline) -> Error {
    // The failure is consumed here, so a later `flush` can no longer see it.
    if let Some(completion) = &pipeline.completion {
        completion.fail();
    }
    match pipeline.join_handle.take() {
        Some(handle) => match handle.join() {
            Ok(Err(e)) => e,
            Ok(Ok(())) => Error::internal(
                "writer thread finished before all blobs were sent; output is incomplete",
            ),
            Err(payload) => panic_error("writer thread", payload.as_ref()),
        },
        // Already joined - flush() or an earlier send failure took the handle
        // and surfaced whatever it found.
        None => Error::internal("writer thread terminated"),
    }
}

fn flush_wrapped_writer<W: Write>(writer: &mut W) -> Result<()> {
    Ok(writer.flush()?)
}

fn sync_buffered_file(writer: &mut BufWriter<File>) -> Result<()> {
    writer.flush()?;
    // `sync_file` records the fsync and decides on the original errno before
    // anything converts it.
    Ok(crate::write::file_writer::sync_file(writer.get_ref())?)
}

/// Take one dispatch permit, blocking until one is free, and return the guard
/// that gives it back.
///
/// The wait goes through [`recv_permit`] and is timed into
/// `writer_permit_wait_ns`.
///
/// The producer's own send paths ([`send_raw`]) hold the guard across their
/// send. The framing path (`write_osm_data`) moves it into the spawned
/// closure and binds it there first, so the permit outlives the spawn and is
/// released after the closure's `tx.send` (or on its unwind).
///
/// Every send path participates in the dispatch bound, including raw writes;
/// see [`WRITE_AHEAD`] for how channel backpressure differs from that bound.
fn acquire_permit(pipeline: &WritePipeline) -> Result<PermitGuard> {
    let t_permit = crate::counters::Stopwatch::start();
    recv_permit(&pipeline.permit_rx, &pipeline.executor)
        .map_err(|_| Error::internal("pipelined writer permit pool disconnected"))?;
    t_permit.record_ns(&WRITER_METRICS.permit_wait_ns);
    Ok(PermitGuard::new(pipeline.permit_tx.clone()))
}

/// Reserve the next output slot: a dispatch permit and the sequence number
/// the writer thread will place the item at. Every pipelined write, framed
/// or raw, starts here.
///
/// If a framing closure already found the writer thread gone, join it now
/// and return its real cause, rather than reading, copying or framing the
/// rest of the dataset into a channel nobody is reading (see `WriterGone`).
///
/// Nothing fallible may sit between this and arranging the slot's delivery
/// unless its failure becomes the slot's ordered result: a sequence number
/// that never reaches the writer thread is a gap it reports at end of stream.
fn reserve(pipeline: &mut WritePipeline) -> Result<(PermitGuard, usize)> {
    if pipeline.writer_gone.observed() {
        return Err(pipeline_send_error(pipeline));
    }
    let permit = acquire_permit(pipeline)?;
    let seq = pipeline.seq;
    pipeline.seq += 1;
    Ok((permit, seq))
}

/// An already-framed item the producer sends itself, as the caller handed
/// it over: nothing is copied or measured until [`send_raw`] holds a slot.
enum RawPayload<'a> {
    Borrowed(&'a [u8]),
    Owned(Vec<u8>),
    Chunks(Vec<Vec<u8>>),
    CopyRange {
        in_file: &'a crate::read::input::InputFile,
        offset: u64,
        len: u64,
    },
}

impl RawPayload<'_> {
    /// The item and byte counters this payload is reported under, and its
    /// byte count.
    fn metrics(&self) -> (&'static AtomicU64, &'static AtomicU64, u64) {
        let m = &WRITER_METRICS;
        match self {
            Self::Borrowed(b) => (&m.payload_raw_items, &m.payload_raw_bytes, b.len() as u64),
            Self::Owned(b) => (&m.payload_raw_items, &m.payload_raw_bytes, b.len() as u64),
            Self::Chunks(chunks) => (
                &m.payload_raw_chunk_items,
                &m.payload_raw_chunk_bytes,
                chunks.iter().map(|c| c.len() as u64).sum(),
            ),
            Self::CopyRange { len, .. } => (
                &m.payload_copy_range_items,
                &m.payload_copy_range_bytes,
                *len,
            ),
        }
    }

    /// The channel item. A borrowed payload is copied here.
    fn into_chunk(self) -> OutputChunk {
        match self {
            Self::Borrowed(b) => OutputChunk::Raw(b.to_vec()),
            Self::Owned(b) => OutputChunk::Raw(b),
            Self::Chunks(chunks) => OutputChunk::RawChunks(chunks),
            Self::CopyRange {
                in_file,
                offset,
                len,
            } => OutputChunk::CopyRange {
                in_file: in_file.clone(),
                offset,
                len,
            },
        }
    }
}

/// Send one already-framed item from the producer thread: reserve a slot,
/// count it under its payload counters, and send it with the permit held
/// across the send, so raw writes share the dispatch bound with framing
/// tasks. The send can still block; see [`WRITE_AHEAD`].
///
/// Everything per payload happens after the reservation: the byte count
/// (a walk over the chunks for `Chunks`) before the send timer starts, and
/// building the item (the copy of a borrowed payload) inside it. A failed
/// send joins the writer thread and returns its own error.
fn send_raw(pipeline: &mut WritePipeline, payload: RawPayload<'_>) -> Result<()> {
    let (_permit, seq) = reserve(pipeline)?;
    let (items, bytes, len) = payload.metrics();
    items.fetch_add(1, Relaxed);
    bytes.fetch_add(len, Relaxed);
    let t_send = crate::counters::Stopwatch::start();
    let sent = pipeline.tx.send(PipelineItem {
        seq,
        data: Ok(payload.into_chunk()),
    });
    if sent.is_err() {
        return Err(pipeline_send_error(pipeline));
    }
    record_send_wait(t_send);
    Ok(())
}

/// Where an `OSMData` blob's optional `BlobHeader` fields (indexdata,
/// tagdata, WayMembers) come from. Owned, so the pipelined path moves it
/// into the framing closure without copying.
enum BlobMeta {
    /// Scan the serialized block for its id range and tag keys. Runs on the
    /// framing worker in pipelined mode. A malformed block is an error; a
    /// valid block of a shape the index cannot describe gets no field.
    Scan,
    /// Emit none of the optional fields.
    Bare,
    /// The caller's pre-computed index and payloads, emitted exactly as
    /// given: an absent tagdata or WayMembers is not recomputed.
    Given {
        index: blob_meta::BlobIndex,
        tagdata: Option<Vec<u8>>,
        way_members: Option<Vec<u8>>,
    },
}

impl BlobMeta {
    /// Resolve against `block` and hand the three header fields
    /// (indexdata, tagdata, WayMembers) to `frame`, borrowed for the call.
    fn with_fields<R>(
        &self,
        block: &[u8],
        frame: impl FnOnce(Option<&[u8]>, Option<&[u8]>, Option<&[u8]>) -> Result<R>,
    ) -> Result<R> {
        match self {
            Self::Scan => {
                let (index, tags) = blob_meta::scan_block_ids_and_tags(block)?;
                let indexdata = index.map(|idx| idx.serialize());
                let tagdata = tags.and_then(|ti| ti.serialize());
                frame(
                    indexdata.as_ref().map(<[u8; 42]>::as_slice),
                    tagdata.as_deref(),
                    None,
                )
            }
            Self::Bare => frame(None, None, None),
            Self::Given {
                index,
                tagdata,
                way_members,
            } => {
                let indexdata = index.serialize();
                frame(
                    Some(indexdata.as_slice()),
                    tagdata.as_deref(),
                    way_members.as_deref(),
                )
            }
        }
    }
}

/// Test hook: whether the framing closure for `seq` should panic (the
/// fault plan's `framing_panic_at`, see `crate::test_hooks`).
#[cfg(feature = "test-hooks")]
fn framing_panics_at(cancel: &crate::host::Cancel, seq: usize) -> bool {
    cancel.faults().framing_panics_at(seq)
}
#[cfg(not(feature = "test-hooks"))]
fn framing_panics_at(_cancel: &crate::host::Cancel, _seq: usize) -> bool {
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
    finalize_writer: fn(&mut W) -> Result<()>,
    #[cfg(test)]
    /// Whether all successfully accepted writes have been explicitly flushed.
    finished: bool,
    /// Scratch buffers for sync-mode blob framing (unused in pipelined mode).
    scratch: FrameScratch,
    /// The writing session of the [`Output`](crate::Output) this writer
    /// fills, if any; see [`PbfWriter::with_completion`].
    completion: Option<Completion>,
    /// The file this writer fills, named in every error it returns: the
    /// output's destination when it fills an `Output` (taken from its
    /// [`Completion`]), else the path a path constructor was given. `None`
    /// for a writer over an arbitrary `Write`.
    output_name: Option<std::sync::Arc<Path>>,
    /// The invocation's cancellation token (a fresh, never-tripped one for
    /// the public constructors). Checked before every block a write method
    /// accepts, and by the writer thread between `copy_file_range` calls, so
    /// a long queued splice stops too. `flush`, `Drop` and the teardown they
    /// run (joining the writer thread, draining framing permits) never check
    /// it, so a cancelled writer still shuts down completely.
    cancel: crate::host::Cancel,
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
    ) -> Result<Self> {
        named_path(
            path,
            Self::to_path_in(
                path,
                compression,
                header_block_bytes,
                executor,
                Diagnostics::discard(),
                Finish::Durable,
                crate::host::Cancel::new(),
            ),
        )
    }

    pub(crate) fn to_path_in(
        path: &Path,
        compression: Compression,
        header_block_bytes: &[u8],
        executor: Executor,
        diagnostics: Diagnostics,
        finish: Finish,
        cancel: crate::host::Cancel,
    ) -> Result<Self> {
        // The buffered backend's writer thread reports nothing.
        drop(diagnostics);
        let writer = FileWriter::buffered(path)?;
        Self::start_pipeline(
            writer,
            compression,
            header_block_bytes,
            executor,
            finish,
            cancel,
        )
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
    ) -> Result<Self> {
        named_path(
            path,
            Self::to_path_direct_in(
                path,
                compression,
                header_block_bytes,
                executor,
                Diagnostics::discard(),
                Finish::Durable,
                crate::host::Cancel::new(),
            ),
        )
    }

    #[cfg(feature = "linux-direct-io")]
    pub(crate) fn to_path_direct_in(
        path: &Path,
        compression: Compression,
        header_block_bytes: &[u8],
        executor: Executor,
        diagnostics: Diagnostics,
        finish: Finish,
        cancel: crate::host::Cancel,
    ) -> Result<Self> {
        // The O_DIRECT backend's writer thread reports nothing.
        drop(diagnostics);
        let writer = FileWriter::direct(path)?;
        Self::start_pipeline(
            writer,
            compression,
            header_block_bytes,
            executor,
            finish,
            cancel,
        )
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
    ) -> Result<Self> {
        named_path(
            path,
            Self::to_path_uring_in(
                path,
                compression,
                header_block_bytes,
                executor,
                Diagnostics::discard(),
                Finish::Durable,
                crate::host::Cancel::new(),
            ),
        )
    }

    #[cfg(feature = "linux-io-uring")]
    pub(crate) fn to_path_uring_in(
        path: &Path,
        compression: Compression,
        header_block_bytes: &[u8],
        executor: Executor,
        diagnostics: Diagnostics,
        finish: Finish,
        cancel: crate::host::Cancel,
    ) -> Result<Self> {
        use crate::write::uring_writer;

        // The writer thread reports nothing; its error is its return value.
        drop(diagnostics);
        let framed_header = frame_blob("OSMHeader", header_block_bytes, &compression, None)?;

        // Oneshot channel for init errors from the writer thread.
        let (init_tx, init_rx) = sync_channel(1);

        let (tx, rx) = sync_channel(WRITE_AHEAD);
        let path_owned = path.to_path_buf();
        let expected = SeqTotal::new();
        let thread_expected = expected.clone();
        let thread_cancel = cancel.clone();
        let handle = std::thread::spawn(move || {
            uring_writer::uring_writer_thread(
                rx,
                path_owned,
                framed_header,
                init_tx,
                &thread_expected,
                finish,
                &thread_cancel,
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
                    Ok(Ok(())) => Error::internal("writer thread exited without init signal"),
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
            completion: None,
            output_name: None,
            cancel,
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
    ) -> Result<Self> {
        named_path(
            path,
            Self::to_path_parallel_in(
                path,
                compression,
                header_block_bytes,
                executor,
                Diagnostics::discard(),
                Finish::Durable,
                crate::host::Cancel::new(),
            ),
        )
    }

    pub(crate) fn to_path_parallel_in(
        path: &Path,
        compression: Compression,
        header_block_bytes: &[u8],
        executor: Executor,
        diagnostics: Diagnostics,
        finish: Finish,
        cancel: crate::host::Cancel,
    ) -> Result<Self> {
        use crate::write::parallel_writer;

        // The writer thread reports nothing; its error is its return value.
        drop(diagnostics);
        let framed_header = frame_blob("OSMHeader", header_block_bytes, &compression, None)?;

        let (init_tx, init_rx) = sync_channel(1);
        let (tx, rx) = sync_channel(WRITE_AHEAD);
        let path_owned = path.to_path_buf();
        let expected = SeqTotal::new();
        let thread_expected = expected.clone();
        let thread_cancel = cancel.clone();
        let handle = std::thread::spawn(move || {
            parallel_writer::parallel_writer_thread(
                rx,
                path_owned,
                framed_header,
                init_tx,
                &thread_expected,
                finish,
                &thread_cancel,
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
                        Error::internal("parallel writer thread exited without init signal")
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
            completion: None,
            output_name: None,
            cancel,
        })
    }

    /// Shared pipelined setup: write header, spawn writer thread.
    fn start_pipeline(
        mut writer: FileWriter,
        compression: Compression,
        header_block_bytes: &[u8],
        executor: Executor,
        finish: Finish,
        cancel: crate::host::Cancel,
    ) -> Result<Self> {
        // Write header synchronously before starting the pipeline.
        let framed_header = frame_blob("OSMHeader", header_block_bytes, &compression, None)?;
        writer.write_all(&framed_header)?;

        // Spawn the writer thread and hand it the writer wrapped in a sink.
        let (tx, rx) = sync_channel(WRITE_AHEAD);
        let expected = SeqTotal::new();
        let thread_expected = expected.clone();
        let sink = FileOutputSink::new(writer, finish, cancel.clone());
        let handle = std::thread::spawn(move || writer_thread(rx, sink, &thread_expected));

        Ok(PbfWriter {
            writer: None,
            compression,
            pipeline: Some(WritePipeline::new(tx, handle, expected, executor)),
            finalize_writer: flush_wrapped_writer::<FileWriter>,
            #[cfg(test)]
            finished: false,
            scratch: FrameScratch::new(),
            completion: None,
            output_name: None,
            cancel,
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
            completion: None,
            output_name: None,
            cancel: crate::host::Cancel::new(),
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

    /// Test hook: the fault plan of the token this writer observes (see
    /// [`crate::test_hooks`]). The public path constructors mint a token per
    /// writer, so this is how a test reaches their worker loops; a writer
    /// built for an `Output` shares its invocation's token. Its
    /// `framing_panic_at` makes the framing work for that data blob (0-based,
    /// counting every blob sent through the pipeline) panic before it frames;
    /// a sync [`PbfWriter::new`] writer has no framing work.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    #[must_use]
    pub fn test_faults(&self) -> &crate::test_hooks::FaultPlan {
        self.cancel.faults()
    }

    /// Tie this writer to the writing session of the [`Output`](crate::Output)
    /// it fills. The session finishes only when [`flush`](Self::flush)
    /// succeeds; any failure the writer observes, or taking the inner writer
    /// out, fails it for good, and publication then refuses the output.
    #[must_use]
    pub(crate) fn with_completion(mut self, completion: Completion) -> Self {
        if let Some(pipeline) = self.pipeline.as_mut() {
            pipeline.completion = Some(completion.clone());
        }
        self.output_name = Some(std::sync::Arc::clone(completion.destination()));
        self.completion = Some(completion);
        self
    }

    /// Name this writer's output in `err` (once, however many layers pass
    /// it up). Cold: only a failure builds the name.
    #[cold]
    fn name_output(&self, err: Error) -> Error {
        match &self.output_name {
            Some(name) => err.writing(name),
            None => err,
        }
    }

    /// Fail the output's session on any error a write reports, from wherever
    /// in the method it came (a scan, a permit, framing, the write itself):
    /// a caller that carries on past a lost blob must not be able to certify
    /// the output with a later successful `flush`. A write after a successful
    /// `flush` fails too, and fails the session with it. The error names the
    /// output: a bare "No space left on device" does not say which file.
    fn poison_on_error<T>(&self, result: Result<T>) -> Result<T> {
        result.map_err(|e| {
            if let Some(completion) = &self.completion {
                completion.fail();
            }
            self.name_output(e)
        })
    }

    /// The token this writer observes, for a stage that feeds it and checks
    /// the same token at its own consumer.
    pub(crate) fn cancel_token(&self) -> &crate::host::Cancel {
        &self.cancel
    }

    /// The per-block checkpoint: once the invocation's token is tripped each
    /// write method returns `ErrorKind::Cancelled` before doing any work,
    /// which fails the output's session like any write error and sends the
    /// command down its existing writer-error path.
    #[inline]
    fn check_cancel(&self) -> Result<()> {
        self.cancel.check()
    }

    /// Write the `OSMHeader` blob. Must be the first blob in the file.
    ///
    /// Not needed when using [`to_path`](Self::to_path), which writes the
    /// header in the constructor.
    ///
    /// `header_block_bytes` is a serialized `HeaderBlock` protobuf message,
    /// typically produced by [`HeaderBuilder::build`](crate::block_builder::HeaderBuilder::build).
    pub fn write_header(&mut self, header_block_bytes: &[u8]) -> Result<()> {
        let result = self.write_blob("OSMHeader", header_block_bytes);
        let result = self.poison_on_error(result);
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
    pub fn write_primitive_block(&mut self, block_bytes: &[u8]) -> Result<()> {
        let result = self
            .check_cancel()
            .and_then(|()| self.write_osm_data(Cow::Borrowed(block_bytes), BlobMeta::Scan));
        self.poison_on_error(result)
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
    pub fn write_primitive_block_no_indexdata(&mut self, block_bytes: &[u8]) -> Result<()> {
        let result = self
            .check_cancel()
            .and_then(|()| self.write_osm_data(Cow::Borrowed(block_bytes), BlobMeta::Bare));
        self.poison_on_error(result)
    }

    /// Write an `OSMData` blob, taking ownership of an encoded block.
    ///
    /// Like [`write_primitive_block`](Self::write_primitive_block) but moves
    /// the block bytes and its tagdata and WayMembers payloads into the
    /// pipeline closure instead of copying them, and frames with the block's
    /// pre-computed [`BlobIndex`](crate::blob_meta::BlobIndex) from
    /// [`BlockBuilder::take_owned`](crate::block_builder::BlockBuilder::take_owned)
    /// instead of rescanning the serialized bytes. The block's tagdata and
    /// WayMembers are emitted exactly as given: an absent one is not
    /// recomputed.
    #[hotpath::measure]
    pub(crate) fn write_primitive_block_owned(&mut self, block: OwnedBlock) -> Result<()> {
        let OwnedBlock {
            bytes,
            index,
            tagdata,
            way_members,
        } = block;
        let meta = BlobMeta::Given {
            index,
            tagdata,
            way_members,
        };
        let result = self
            .check_cancel()
            .and_then(|()| self.write_osm_data(Cow::Owned(bytes), meta));
        self.poison_on_error(result)
    }

    /// The one per-block dispatch behind the three `write_primitive_block*`
    /// entry points, which differ only in where the block's index fields
    /// come from ([`BlobMeta`]) and whether the bytes arrive owned.
    ///
    /// Pipelined: reserve a slot (the dispatch permit bounds in-flight
    /// framing work; see `PIPELINE_DISPATCH_PERMITS` for the planet-scale
    /// OOM story behind it), only then take ownership of borrowed bytes,
    /// and frame on the writer's executor. A scan or framing failure travels
    /// as that slot's ordered result. Sync: resolve and frame on the caller,
    /// writing straight from the scratch with no copy of the block.
    fn write_osm_data(&mut self, block: Cow<'_, [u8]>, meta: BlobMeta) -> Result<()> {
        if let Some(ref mut pipeline) = self.pipeline {
            let (permit, seq) = reserve(pipeline)?;
            let compression = self.compression;
            let uncompressed = block.into_owned();
            let tx = pipeline.tx.clone();
            let writer_gone = pipeline.writer_gone.clone();
            let panic_here = framing_panics_at(&self.cancel, seq);
            pipeline.executor.spawn(move || {
                // Bound first so it drops last - the permit is released
                // after `tx.send` below, and also on the unwind path if any
                // of the framing work panics. See `PermitGuard`.
                let _permit = permit;
                assert!(!panic_here, "test hook: framing panic at seq {seq}");
                // A scan runs here, on the framing worker, outside the
                // scratch borrow; its failure is this slot's ordered result.
                let result = meta.with_fields(&uncompressed, |indexdata, tagdata, way_members| {
                    PIPELINE_SCRATCH.with_borrow_mut(|scratch| {
                        frame_blob_into(
                            "OSMData",
                            &uncompressed,
                            &compression,
                            indexdata,
                            tagdata,
                            way_members,
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
                let t_send = crate::counters::Stopwatch::start();
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
            let result = meta.with_fields(&block, |indexdata, tagdata, way_members| {
                self.write_framed_blob("OSMData", &block, indexdata, tagdata, way_members)
            });
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
    pub fn write_raw(&mut self, raw_framed_bytes: &[u8]) -> Result<()> {
        let result = self
            .check_cancel()
            .and_then(|()| self.write_raw_unchecked(raw_framed_bytes));
        self.poison_on_error(result)
    }

    fn write_raw_unchecked(&mut self, raw_framed_bytes: &[u8]) -> Result<()> {
        if let Some(ref mut pipeline) = self.pipeline {
            send_raw(pipeline, RawPayload::Borrowed(raw_framed_bytes))?;
            self.mark_pending();
            Ok(())
        } else {
            let result = self.writer_mut()?.write_all(raw_framed_bytes);
            if result.is_ok() {
                self.mark_pending();
            }
            Ok(result?)
        }
    }

    /// Write pre-framed raw blob bytes, taking ownership of the Vec.
    ///
    /// Like [`write_raw`](Self::write_raw) but moves the Vec into the
    /// pipeline channel instead of copying. Use when the caller already
    /// owns the bytes and won't need them afterwards (e.g. merge passthrough
    /// with `std::mem::take`).
    pub fn write_raw_owned(&mut self, raw_framed_bytes: Vec<u8>) -> Result<()> {
        let result = self
            .check_cancel()
            .and_then(|()| self.write_raw_owned_unchecked(raw_framed_bytes));
        self.poison_on_error(result)
    }

    fn write_raw_owned_unchecked(&mut self, raw_framed_bytes: Vec<u8>) -> Result<()> {
        if let Some(ref mut pipeline) = self.pipeline {
            send_raw(pipeline, RawPayload::Owned(raw_framed_bytes))?;
            self.mark_pending();
            Ok(())
        } else {
            let result = self.writer_mut()?.write_all(&raw_framed_bytes);
            if result.is_ok() {
                self.mark_pending();
            }
            Ok(result?)
        }
    }

    /// Write multiple pre-framed raw blob chunks without concatenating them.
    ///
    /// Like [`write_raw_owned`](Self::write_raw_owned) but accepts a list of
    /// owned chunks. The writer thread writes each chunk sequentially.
    /// Used by passthrough coalescers to avoid `extend_from_slice` memcpy.
    pub fn write_raw_chunks(&mut self, chunks: Vec<Vec<u8>>) -> Result<()> {
        let result = self
            .check_cancel()
            .and_then(|()| self.write_raw_chunks_unchecked(chunks));
        self.poison_on_error(result)
    }

    fn write_raw_chunks_unchecked(&mut self, chunks: Vec<Vec<u8>>) -> Result<()> {
        if let Some(ref mut pipeline) = self.pipeline {
            send_raw(pipeline, RawPayload::Chunks(chunks))?;
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
    /// errors. The public file-backed constructors (`to_path*`,
    /// [`to_path_sync`](PbfWriter::to_path_sync)) also sync the file; a writer
    /// from [`Output::pbf_writer`](crate::Output::pbf_writer) does not, because
    /// the output's publication syncs it (on Linux, a buffered
    /// target that does not support `fsync` - a FIFO, socket or character
    /// device such as `/dev/null` - is not an error); a synchronous writer
    /// created with `new` only calls its underlying `Write::flush`, which does
    /// not guarantee durability. Dropping without flushing performs cleanup
    /// but cannot report errors or a missing tail.
    #[hotpath::measure]
    pub fn flush(&mut self) -> Result<()> {
        let result = self.flush_inner();
        if let Some(completion) = &self.completion {
            if result.is_ok() {
                completion.finish();
                // Finishing is terminal for a writer filling an output: a
                // synchronous writer drops its inner writer (already flushed)
                // so a later write fails instead of buffering bytes that the
                // published file would not contain.
                self.writer = None;
            } else {
                completion.fail();
            }
        }
        result.map_err(|e| self.name_output(e))
    }

    fn flush_inner(&mut self) -> Result<()> {
        self.mark_finished();
        let t_flush = crate::counters::Stopwatch::start();
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
            let finalize = self.finalize_writer;
            crate::write::metrics::with_sync_observer(&self.cancel, || finalize(w))?;
        }
        t_flush.record_ns(&WRITER_METRICS.flush_ns);
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
    /// # Errors
    ///
    /// Returns an invalid-argument error if the writer was consumed by a
    /// pipeline, as every other write method does.
    pub fn into_inner(mut self) -> Result<W> {
        // The inner writer leaves without end-of-stream finalization, so a
        // writer filling an output can no longer certify it.
        if let Some(completion) = &self.completion {
            completion.fail();
        }
        self.mark_finished();
        self.writer
            .take()
            .ok_or_else(|| Error::invalid_argument(WRITER_CONSUMED))
    }

    /// The inner writer, for the non-pipelined path.
    ///
    /// `writer` is `None` for every pipelined constructor (the file was moved
    /// into the writer thread) and stays `None` after `flush()` joins that
    /// thread. Reaching here in that state means a write was attempted on a
    /// finished pipelined writer - which used to panic out of a public `&mut
    /// self` method. It is a caller error, but a caller error on a public API
    /// is an error to return, not a panic.
    fn writer_mut(&mut self) -> Result<&mut W> {
        self.writer
            .as_mut()
            .ok_or_else(|| Error::invalid_argument(WRITER_CONSUMED))
    }

    // wontfix(type-no-stringly): blob_type is &str matching protobuf wire format;
    // only 2 constants ("OSMHeader"/"OSMData"), no real typo risk.
    #[hotpath::measure]
    fn write_blob(&mut self, blob_type: &str, uncompressed: &[u8]) -> Result<()> {
        self.write_framed_blob(blob_type, uncompressed, None, None, None)
    }

    /// Encode, compress, and write a blob directly to the writer using reusable
    /// scratch buffers. Eliminates all intermediate `Vec` allocations after
    /// warmup, except that a scratch buffer grown past the framing module's
    /// 4 MiB retention limit is dropped after its blob and regrown on demand.
    ///
    /// The framing is the pipelined path's own (`frame_in_scratch`); only the
    /// hand-off differs, writing the parts from the scratch instead of
    /// swapping them out, so the size guards cannot drift apart again.
    fn write_framed_blob(
        &mut self,
        blob_type: &str,
        uncompressed: &[u8],
        indexdata: Option<&[u8]>,
        tagdata: Option<&[u8]>,
        way_members: Option<&[u8]>,
    ) -> Result<()> {
        // `frame_in_scratch` trims on entry. Trim again on every exit, error
        // or not, because this path keeps its buffers in the scratch rather
        // than handing them off, so an outlier would otherwise stay pinned in
        // this writer until its next blob.
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
    ) -> Result<()> {
        let frame = frame_in_scratch(
            blob_type,
            uncompressed,
            &self.compression,
            indexdata,
            tagdata,
            way_members,
            &mut self.scratch,
        )?;
        let total_len = 4 + frame.header.len() + frame.blob.len();
        // Write the 3 frame parts directly - no intermediate `out` Vec.
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| Error::invalid_argument(WRITER_CONSUMED))?;
        let t_write = crate::counters::Stopwatch::start();
        writer.write_all(&frame.prefix)?;
        writer.write_all(frame.header)?;
        writer.write_all(frame.blob)?;
        t_write.record_ns(&WRITER_METRICS.write_ns);
        WRITER_METRICS
            .bytes_written
            .fetch_add(total_len as u64, Relaxed);
        Ok(())
    }
}

impl PbfWriter<BufWriter<File>> {
    /// A synchronous writer over the staged file of an
    /// [`Output`](crate::Output): like [`to_path_sync`](Self::to_path_sync),
    /// but `flush` only flushes, because publication syncs the file.
    pub(crate) fn to_staged_file(path: &Path, compression: Compression) -> Result<Self> {
        let file = super::create_backend_file(path)?;
        let buffered = BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file);
        Ok(Self::new(buffered, compression))
    }

    /// Create a synchronous writer that owns a 256 KiB buffered output file.
    ///
    /// The returned writer has no header yet. Call
    /// [`write_header`](Self::write_header), write its blocks, then call
    /// [`flush`](Self::flush). `flush` pushes the buffered bytes onward and
    /// synchronizes the file. Use [`PbfWriter::new`] when the caller owns an
    /// arbitrary `Write` value and only needs its ordinary `Write::flush`
    /// behavior.
    pub fn to_path_sync(path: &Path, compression: Compression) -> Result<Self> {
        let file = super::create_backend_file(path).map_err(|e| Error::from(e).writing(path))?;
        let buffered = BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file);
        let mut writer = Self::new(buffered, compression);
        writer.finalize_writer = sync_buffered_file;
        writer.output_name = Some(std::sync::Arc::from(path));
        Ok(writer)
    }
}

/// A path constructor's result, named for `path`: its errors, and every
/// error the writer returns afterwards.
fn named_path(path: &Path, result: Result<PbfWriter<FileWriter>>) -> Result<PbfWriter<FileWriter>> {
    match result {
        Ok(mut writer) => {
            writer.output_name = Some(std::sync::Arc::from(path));
            Ok(writer)
        }
        Err(e) => Err(e.writing(path)),
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

impl PbfWriter<FileWriter> {
    /// Write a passthrough blob via kernel-space copy (`copy_file_range`).
    ///
    /// Instead of copying blob bytes through userspace, this tells the kernel
    /// to copy directly between file descriptors. On filesystems with reflink
    /// support (btrfs, xfs), this is a metadata-only operation.
    ///
    /// Available in every build; it used to need the `linux-direct-io`
    /// feature. Where the kernel refuses the copy, and on every platform but
    /// Linux, the range is copied in userspace instead. Off Linux that copy
    /// is refused for the input and output being the same file, an
    /// append-mode output, or a range past `i64::MAX`, which Linux would
    /// first offer to the kernel (and accept for disjoint ranges of one
    /// file).
    ///
    /// `in_file` is the open input PBF and its path, shared rather than
    /// borrowed: the pipelined path queues this request into the writer
    /// thread and returns immediately, so the input has to stay open until
    /// the splice actually runs. Passing a bare `RawFd` could not express
    /// that - see [`OutputChunk::CopyRange`](crate::write::pipeline::OutputChunk).
    /// The path names the input when the read or splice fails.
    /// `offset` and `len` describe the framed blob's position in the input file.
    ///
    /// Callers must not use this when the output writer is O_DIRECT - use
    /// [`write_raw`](Self::write_raw) instead. Every library caller decides
    /// the route once, from `OutputBackend::supports_copy_range`, before any
    /// output is written, so `--direct-io` never reaches the sink's own
    /// refusal of a splice into an O_DIRECT file: that refusal is a backstop
    /// for a direct library caller, not where a flag combination is checked.
    pub fn write_raw_copy(
        &mut self,
        in_file: &crate::read::input::InputFile,
        offset: u64,
        len: u64,
    ) -> Result<()> {
        let result = self
            .check_cancel()
            .and_then(|()| self.write_raw_copy_unchecked(in_file, offset, len));
        self.poison_on_error(result)
    }

    fn write_raw_copy_unchecked(
        &mut self,
        in_file: &crate::read::input::InputFile,
        offset: u64,
        len: u64,
    ) -> Result<()> {
        if let Some(ref mut pipeline) = self.pipeline {
            send_raw(
                pipeline,
                RawPayload::CopyRange {
                    in_file,
                    offset,
                    len,
                },
            )?;
            self.mark_pending();
            Ok(())
        } else {
            // This is reachable through FileWriter::buffered plus the public
            // generic PbfWriter::new constructor.
            let out = self
                .writer
                .as_mut()
                .ok_or_else(|| Error::invalid_argument(WRITER_CONSUMED))?
                .flush_and_file()?
                .ok_or_else(|| {
                    Error::invalid_argument("copy_file_range incompatible with O_DIRECT output")
                })?;
            // `copy_range` decides on the original errno (whether the
            // userspace fallback takes over) before anything converts it.
            let result = copy_range(in_file, CopyOut::Cursor(out), offset, len, &self.cancel);
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
    use crate::block_builder::OwnedBlock;
    use crate::error::{Error, ErrorKind, Result};
    use crate::write::framing::FrameScratch;
    use crate::write::pipeline::{PipelineItem, SeqTotal};
    use std::io;
    use std::sync::mpsc::sync_channel;

    /// A disk-full failure as the writer thread would report it.
    fn storage_full(msg: &'static str) -> Error {
        Error::from(io::Error::new(io::ErrorKind::StorageFull, msg))
    }

    fn is_storage_full(err: &Error) -> bool {
        matches!(err.kind(), ErrorKind::Io(e) if e.kind() == io::ErrorKind::StorageFull)
    }

    #[test]
    fn malformed_primitive_block_is_not_written_without_an_index() {
        let mut writer = PbfWriter::new(Vec::new(), Compression::None);
        let err = writer.write_primitive_block(&[0x80]).unwrap_err();
        assert!(matches!(err.kind(), ErrorKind::WireFormat { .. }));
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
        assert!(matches!(
            result.unwrap_err().kind(),
            ErrorKind::WireFormat { .. }
        ));
    }

    /// A `PbfWriter` whose writer thread has already exited with `outcome`.
    ///
    /// The receiver is dropped before the writer is handed back, so the very
    /// next send fails - which is exactly the shape of a writer thread that
    /// died on ENOSPC halfway through a planet write.
    fn writer_with_dead_thread(outcome: Result<()>) -> PbfWriter<Vec<u8>> {
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
            completion: None,
            output_name: None,
            cancel: crate::host::Cancel::new(),
        }
    }

    /// The writer thread's real error must survive, not be replaced.
    ///
    /// Every send-failure path returned a generic "writer thread
    /// terminated" error. The caller `?`-bailed, `Drop` discarded the join
    /// result, and the only description of the actual failure - a full disk,
    /// a permission problem - was thrown away. The assertions here are on the
    /// kind and message the *thread* produced, which no synthetic substitute
    /// can supply.
    #[test]
    fn a_dead_writer_thread_surfaces_its_own_error_not_a_generic_one() {
        let mut w = writer_with_dead_thread(Err(storage_full("no space left on device")));
        let err = w.write_raw(b"blob").expect_err("the send must fail");
        assert!(
            is_storage_full(&err),
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
        let mut w = writer_with_dead_thread(Err(storage_full("no space left on device")));
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
        assert!(
            is_storage_full(&err),
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
    /// `write_framed_blob` then carried its own copy of the framing for the
    /// `PbfWriter::new` sync path, so a sync-mode writer kept emitting a body
    /// the crate's own reader refuses ("blob header declares datasize too
    /// big"), with exit zero. Both paths now share `frame_in_scratch`; this
    /// pins the sync end of that. Same cap, same knob-naming error message.
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
        assert!(
            matches!(err.kind(), ErrorKind::BlobTooLarge { .. }),
            "the sync path must refuse with BlobTooLarge, got: {err}"
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

        let input = crate::read::input::InputFile::from_parts(
            Arc::new(std::fs::File::open(&input_path).unwrap()),
            Arc::from(input_path.as_path()),
        );
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
    ///
    /// Counted on the writer's own token, so exactly one fsync is required:
    /// the process-wide total also moves with every concurrent test that
    /// syncs, and could satisfy a weaker bound with this writer syncing
    /// nothing. `to_path_sync`'s synchronous finalizer is held to the same.
    #[cfg(feature = "test-hooks")]
    #[test]
    fn end_of_stream_finalization_still_fsyncs() {
        use std::sync::atomic::Ordering::Relaxed;

        let dir = crate::path_guard::tests::scratch();
        let header = crate::block_builder::HeaderBuilder::new().build().unwrap();

        let path = dir.path().join("durable.osm.pbf");
        let mut w =
            PbfWriter::to_path(&path, Compression::None, &header, Executor::Ambient).unwrap();
        w.write_primitive_block(&[0x12u8, 0x00]).unwrap();
        w.flush().unwrap();
        assert_eq!(
            w.test_faults().sync_calls.load(Relaxed),
            1,
            "a completed pipelined write must fsync its output exactly once; \
             FileOutputSink::flush has to call FileWriter::sync, not \
             FileWriter::flush"
        );

        let path = dir.path().join("durable-sync.osm.pbf");
        let mut w = PbfWriter::to_path_sync(&path, Compression::None).unwrap();
        w.write_header(&header).unwrap();
        w.write_primitive_block(&[0x12u8, 0x00]).unwrap();
        w.flush().unwrap();
        assert_eq!(
            w.test_faults().sync_calls.load(Relaxed),
            1,
            "a completed synchronous path write must fsync its output exactly once"
        );
    }

    /// An owned block with a fixed node index and no tagdata or WayMembers.
    fn owned_block(bytes: &[u8]) -> OwnedBlock {
        OwnedBlock {
            bytes: bytes.to_vec(),
            index: crate::blob_meta::BlobIndex {
                kind: crate::ElementKind::Node,
                min_id: 0,
                max_id: 0,
                count: 0,
                bbox: None,
            },
            tagdata: None,
            way_members: None,
        }
    }

    /// The same gap, on the owned-bytes and no-indexdata siblings.
    ///
    /// The three `write_primitive_block*` methods share one dispatch, but
    /// each resolves its header fields differently (scan, given, none), and
    /// these three methods once carried a copy of the dispatch each, where a
    /// fix to one missed the others. Every entry point stays tested so a
    /// future split cannot regress one of them silently.
    #[test]
    fn every_primitive_block_variant_stops_once_the_writer_thread_is_dead() {
        let block = [0x12u8, 0x00];

        let mut w =
            writer_with_dead_thread(Err(storage_full("owned variant: no space left on device")));
        let mut err = None;
        for _ in 0..2_000 {
            if let Err(e) = w.write_primitive_block_owned(owned_block(&block)) {
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

        let mut w = writer_with_dead_thread(Err(storage_full(
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
                                writer.write_primitive_block_owned(owned_block(&block))
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
