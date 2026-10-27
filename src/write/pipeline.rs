//! Pipeline plumbing for [`PbfWriter`](super::writer::PbfWriter): ordered
//! output chunks, the sink trait, the bounded permit pool that caps in-flight
//! rayon dispatches, and the writer thread that reorders and drains results
//! to the underlying sink.
//!
//! All items here are internal wiring for the write path; nothing in this
//! module is part of the public library API.

use crate::error::{Error, Result};
use crate::reorder_buffer::ReorderBuffer;
use crate::tunables::{EXECUTOR_IDLE_RECV_TIMEOUT, PIPELINE_DISPATCH_PERMITS, WRITE_AHEAD};
use crate::write::file_writer::FileWriter;
use crate::write::metrics::WriterMetrics;
use std::io::Write;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;

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
    /// Kernel-space copy from the input file (avoids a userspace copy for
    /// passthrough).
    ///
    /// The input is carried as an `Arc<File>`, not a `RawFd`. A raw descriptor
    /// number queued into another thread's channel has no lifetime tie to the
    /// `File` that owns it: close the input before `flush()`, or let the fd
    /// number be recycled by an unrelated `open`, and the copy splices
    /// whatever now sits at that descriptor - no error, wrong bytes, from a
    /// path whose whole point is byte-exact passthrough. Every in-tree caller
    /// happened to keep its input alive, but nothing made them. An `Arc` costs
    /// one refcount per passthrough run and makes the guarantee structural.
    ///
    /// The input travels with its path ([`InputFile`](crate::read::input::InputFile)),
    /// so a failed read or splice names the file and the byte range.
    CopyRange {
        in_file: crate::read::input::InputFile,
        offset: u64,
        len: u64,
    },
}

/// The three owned parts of a framed blob:
/// `4 B big-endian header_len | BlobHeader | Blob protobuf body`.
///
/// Every backend flattens the parts into one buffer via
/// [`into_vec`](Self::into_vec) before writing; none issues a scatter-gather
/// write.
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
/// `data` is `Result<OutputChunk>` because framing runs on rayon workers
/// and may fail asynchronously - the error is propagated in order via the
/// reorder buffer so the writer thread surfaces it at the correct position.
pub(crate) struct PipelineItem {
    pub(crate) seq: usize,
    pub(crate) data: Result<OutputChunk>,
}

/// A backend's byte path: what it does with the bytes [`deliver_in_order`]
/// hands it, one operation at a time, in output order.
///
/// The sink owns only what differs per backend - copying into a `BufWriter`
/// or registered buffers, queueing `pwrite`s at an offset, splicing a copy
/// range. Receiving, reordering, the handoff timer, accepted-byte accounting
/// and the end-of-stream completeness proof are the loop's, so every backend
/// gets them identically (see `decisions/0014-output-backends-stay-separate.md`).
/// Finalization (last flush, ring drain, pool join, fsync) is not part of
/// the sink: each backend does it after the loop returns.
pub(crate) trait OutputSink {
    /// Names the backend in the end-of-stream errors.
    const BACKEND: &'static str;

    /// Accept one whole buffer of output bytes, consuming it.
    fn write_owned(&mut self, bytes: Vec<u8>) -> Result<()>;

    /// Accept `len` bytes of `in_file` from `offset` as the next output.
    fn copy_range(
        &mut self,
        in_file: crate::read::input::InputFile,
        offset: u64,
        len: u64,
    ) -> Result<()>;

    /// Steady-state maintenance after each received item has been pushed and
    /// its ready run handed off: io_uring's non-blocking CQE reap, the
    /// `pwrite` pool's stopped check. Outside every timer (see
    /// `writer_handoff_ns`). Not called before the first receive, where io_uring
    /// could otherwise reap a failed header write ahead of the stream's own
    /// first error.
    fn after_drain(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Hand one ordered chunk to `sink`; the caller times it as one handoff.
/// Every chunk is flattened to owned buffers, and accepted bytes are counted
/// per successful constituent operation (`writer_bytes_written`). A free
/// function rather than a trait method, so no backend can override the
/// bookkeeping.
fn hand_off<S: OutputSink>(
    sink: &mut S,
    chunk: OutputChunk,
    metrics: &WriterMetrics,
) -> Result<()> {
    fn accept<S: OutputSink>(sink: &mut S, bytes: Vec<u8>, metrics: &WriterMetrics) -> Result<()> {
        let len = bytes.len() as u64;
        sink.write_owned(bytes)?;
        metrics.record_accepted(len);
        Ok(())
    }
    match chunk {
        OutputChunk::Framed(parts) => accept(sink, parts.into_vec(), metrics),
        OutputChunk::Raw(bytes) => accept(sink, bytes, metrics),
        OutputChunk::RawChunks(chunks) => chunks
            .into_iter()
            .try_for_each(|chunk| accept(sink, chunk, metrics)),
        OutputChunk::CopyRange {
            in_file,
            offset,
            len,
        } => {
            sink.copy_range(in_file, offset, len)?;
            metrics.record_accepted(len);
            Ok(())
        }
    }
}

/// `write_all` `bytes` and, on success, count them as accepted
/// (`writer_bytes_written`). The per-operation byte accounting shared by the
/// buffered sink and the synchronous `PbfWriter`.
pub(crate) fn write_accepted<W: Write + ?Sized>(
    writer: &mut W,
    bytes: &[u8],
    metrics: &WriterMetrics,
) -> Result<()> {
    writer.write_all(bytes)?;
    metrics.record_accepted(bytes.len() as u64);
    Ok(())
}

/// Default sink that wraps a [`FileWriter`] (buffered or O_DIRECT).
///
/// Flattens `Framed` chunks via [`FramedBlobParts::into_vec`] before handing
/// them to the underlying writer.
pub(crate) struct FileOutputSink {
    writer: FileWriter,
    finish: super::Finish,
    /// Checked between `copy_file_range` calls of a queued splice; the
    /// writer's counters record into its invocation.
    cancel: crate::host::Invocation,
}

impl FileOutputSink {
    pub(crate) fn new(
        writer: FileWriter,
        finish: super::Finish,
        cancel: crate::host::Invocation,
    ) -> Self {
        Self {
            writer,
            finish,
            cancel,
        }
    }
}

impl OutputSink for FileOutputSink {
    const BACKEND: &'static str = "pipelined writer";

    fn write_owned(&mut self, bytes: Vec<u8>) -> Result<()> {
        Ok(self.writer.write_all(&bytes)?)
    }

    fn copy_range(
        &mut self,
        in_file: crate::read::input::InputFile,
        offset: u64,
        len: u64,
    ) -> Result<()> {
        let out = self.writer.flush_and_file()?.ok_or_else(|| {
            Error::invalid_argument("copy_file_range incompatible with O_DIRECT output")
        })?;
        super::copy_range::copy_range(
            &in_file,
            super::copy_range::CopyOut::Cursor(out),
            offset,
            len,
            &self.cancel,
        )?;
        // Hold the input open until the splice has actually happened.
        drop(in_file);
        Ok(())
    }
}

impl FileOutputSink {
    /// End of stream: finalize *and* make durable.
    ///
    /// This is the pipeline's finalization, not a `Write::flush`, so it asks
    /// for the durability explicitly. The two used to be the same method on
    /// [`FileWriter`], which meant every incidental `flush()` anywhere bought
    /// an `fsync`; the sibling uring and parallel backends `sync_all` by name
    /// at this point.
    ///
    /// A `Deferred` sink only finishes the file (for `O_DIRECT`, the tail
    /// padding and truncation); its owner syncs it.
    fn finish(mut self) -> Result<()> {
        match self.finish {
            super::Finish::Durable => self.writer.sync(),
            super::Finish::Deferred => Ok(self.writer.flush()?),
        }
    }
}

/// Write pipeline state - active when using pipelined mode.
pub(super) struct WritePipeline {
    pub(super) tx: SyncSender<PipelineItem>,
    pub(super) seq: usize,
    pub(super) join_handle: Option<JoinHandle<Result<()>>>,
    /// Counting-semaphore permit pool bounding in-flight rayon dispatches.
    /// The producer takes a permit ([`recv_permit`]) before spawning on the
    /// executor; the framing closure `send()`s it back when its work
    /// completes. See
    /// [`PIPELINE_DISPATCH_PERMITS`] for the memory-bound rationale.
    pub(super) permit_tx: SyncSender<()>,
    pub(super) permit_rx: Receiver<()>,
    /// End-of-stream handshake shared with the writer thread. `flush`
    /// announces `seq` into it before dropping `tx`; see [`SeqTotal`].
    pub(super) expected: SeqTotal,
    /// Death notice from the framing closures; see [`WriterGone`].
    pub(super) writer_gone: WriterGone,
    /// Where the framing closures run, and whether the producer is on one of
    /// its workers (see [`recv_permit`]).
    pub(super) executor: crate::host::Executor,
    /// The output's writing session, when this writer is driven by an
    /// [`Output`](crate::Output). A failure recovered from the writer thread
    /// poisons it, so the later `flush` (which finds no thread left to join
    /// and would otherwise succeed) cannot certify the output.
    pub(super) completion: Option<crate::output::Completion>,
    /// The invocation whose writer counters the producer side records into.
    pub(super) invocation: crate::host::Invocation,
}

impl WritePipeline {
    /// A pipeline over a freshly spawned writer thread, with a full permit
    /// pool and nothing sent yet.
    pub(super) fn new(
        tx: SyncSender<PipelineItem>,
        join_handle: JoinHandle<Result<()>>,
        expected: SeqTotal,
        executor: crate::host::Executor,
        invocation: crate::host::Invocation,
    ) -> Self {
        let (permit_tx, permit_rx) = new_permit_pool();
        Self {
            tx,
            seq: 0,
            join_handle: Some(join_handle),
            permit_tx,
            permit_rx,
            expected,
            writer_gone: WriterGone::new(),
            executor,
            completion: None,
            invocation,
        }
    }

    /// Spawn a writer thread running `body` over a fresh channel and
    /// end-of-stream handshake, and wrap it in a pipeline.
    pub(super) fn spawn<F>(
        executor: crate::host::Executor,
        invocation: crate::host::Invocation,
        body: F,
    ) -> Self
    where
        F: FnOnce(Receiver<PipelineItem>, &SeqTotal) -> Result<()> + Send + 'static,
    {
        let (tx, rx) = sync_channel(WRITE_AHEAD);
        let expected = SeqTotal::new();
        let thread_expected = expected.clone();
        let handle = std::thread::spawn(move || body(rx, &thread_expected));
        Self::new(tx, handle, expected, executor, invocation)
    }

    /// The writer counters of this pipeline's invocation.
    pub(super) fn metrics(&self) -> &WriterMetrics {
        &self.invocation.metrics().writer
    }

    /// [`spawn`](Self::spawn) for a thread that does its own setup (opening
    /// the file, registering buffers) and reports the outcome through the
    /// `init_tx` it is handed before entering its loop. An init failure is
    /// returned here, before the caller gets a writer; `label` names the
    /// thread in the errors for a thread that died without reporting.
    ///
    /// The wait on the init result is unbounded; see
    /// `PbfWriter::to_path_uring` for why that is deliberate.
    pub(super) fn spawn_with_init<F>(
        label: &str,
        executor: crate::host::Executor,
        invocation: crate::host::Invocation,
        body: F,
    ) -> Result<Self>
    where
        F: FnOnce(Receiver<PipelineItem>, SyncSender<Result<()>>, &SeqTotal) -> Result<()>
            + Send
            + 'static,
    {
        let (init_tx, init_rx) = sync_channel(1);
        let mut pipeline = Self::spawn(executor, invocation, move |rx, expected| {
            body(rx, init_tx, expected)
        });
        match init_rx.recv() {
            Ok(Ok(())) => Ok(pipeline),
            Ok(Err(e)) => {
                // Init failed. Join the thread to clean up.
                let handle = pipeline.join_handle.take();
                drop(pipeline);
                if let Some(h) = handle {
                    drop(h.join());
                }
                Err(e)
            }
            Err(_) => {
                // Thread panicked or exited before sending init result.
                let handle = pipeline.join_handle.take();
                drop(pipeline);
                Err(match handle.map(JoinHandle::join) {
                    Some(Ok(Ok(()))) => {
                        Error::internal(format!("{label} exited without init signal"))
                    }
                    Some(Ok(Err(e))) => e,
                    Some(Err(payload)) => crate::error::panic_error(
                        &format!("{label} (during init)"),
                        payload.as_ref(),
                    ),
                    None => Error::internal(format!("{label} handle missing")),
                })
            }
        }
    }
}

/// Take one dispatch permit.
///
/// Off the executor's workers - every CLI path - this is a plain blocking
/// `recv`, today's path byte for byte. On a worker of the executor's pool a
/// blocking `recv` could park the only thread able to run the framing work
/// that returns the permit (a one-thread pool deadlocks outright), so the wait
/// is cooperative: run the pool's pending jobs while none is free, and nap for
/// a millisecond at a time when there are none. The cooperative time is added
/// to `writer_coop_wait_ns`, which is therefore zero unless a `PbfWriter` is
/// driven from a rayon worker.
pub(super) fn recv_permit(
    rx: &Receiver<()>,
    executor: &crate::host::Executor,
    metrics: &WriterMetrics,
) -> std::result::Result<(), std::sync::mpsc::RecvError> {
    use std::sync::mpsc::{RecvError, RecvTimeoutError, TryRecvError};
    if !executor.on_worker() {
        return rx.recv();
    }
    let start = crate::counters::Stopwatch::start();
    let result = loop {
        match rx.try_recv() {
            Ok(()) => break Ok(()),
            Err(TryRecvError::Disconnected) => break Err(RecvError),
            Err(TryRecvError::Empty) => match executor.yield_now() {
                Some(rayon::Yield::Executed) => {}
                Some(rayon::Yield::Idle) => match rx.recv_timeout(EXECUTOR_IDLE_RECV_TIMEOUT) {
                    Ok(()) => break Ok(()),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break Err(RecvError),
                },
                None => break rx.recv(),
            },
        }
    };
    start.record_ns(&metrics.coop_wait_ns);
    result
}

/// Take back every one of the [`PIPELINE_DISPATCH_PERMITS`] tokens through
/// [`recv_permit`], which means every framing closure has finished. Stops
/// early if the pool disconnects.
///
/// `flush` and `Drop` call this before joining the writer thread when the
/// producer is on a worker of the executor's pool: the writer thread is
/// waiting for items that framing closures queued on that pool are still to
/// produce, and joining from the pool's only free thread would never let them
/// run.
pub(super) fn drain_permits(
    rx: &Receiver<()>,
    executor: &crate::host::Executor,
    metrics: &WriterMetrics,
) {
    for _ in 0..PIPELINE_DISPATCH_PERMITS {
        if recv_permit(rx, executor, metrics).is_err() {
            break;
        }
    }
}

/// Build a fresh permit pool pre-filled with `PIPELINE_DISPATCH_PERMITS`
/// tokens. Called during pipelined-writer construction to seed the
/// counting semaphore.
#[expect(
    clippy::expect_used,
    reason = "the first N sends on a fresh channel of capacity N, whose receiver is held here, cannot fail or block"
)]
fn new_permit_pool() -> (SyncSender<()>, Receiver<()>) {
    let (permit_tx, permit_rx) = sync_channel::<()>(PIPELINE_DISPATCH_PERMITS);
    for _ in 0..PIPELINE_DISPATCH_PERMITS {
        permit_tx
            .send(())
            .expect("seeding permit pool on fresh channel");
    }
    (permit_tx, permit_rx)
}

/// Holds one dispatch permit for the lifetime of a rayon framing closure and
/// returns it on drop.
///
/// The release has to happen on the unwind path too. A rayon closure that
/// panics never reaches a trailing `permit_tx.send(())`, and the permit pool
/// is a counting semaphore with no other source of tokens - so after
/// [`PIPELINE_DISPATCH_PERMITS`] such panics the next
/// `write_primitive_block` blocks forever on `permit_rx.recv()` with no
/// timeout and no diagnostic. Binding the guard first in the closure body
/// also puts the release after the `tx.send` of the framed result, which
/// is where it must be so the in-flight count stays correct while the result
/// waits in the writer channel.
pub(super) struct PermitGuard(SyncSender<()>);

impl PermitGuard {
    pub(super) fn new(permit_tx: SyncSender<()>) -> Self {
        Self(permit_tx)
    }
}

impl Drop for PermitGuard {
    fn drop(&mut self) {
        // Failure here means the main thread already dropped its receiver
        // (shutting down); safe to ignore.
        self.0.send(()).ok();
    }
}

/// The framing closures' death notice: "the writer thread is already gone".
///
/// The counterpart to [`SeqTotal`]. `SeqTotal` carries information *forward*
/// from the producer to the writer thread at end of stream; this carries it
/// *back* from the framing closures to the producer, mid-stream.
///
/// `write_raw` and `write_raw_copy` send from the producer's own thread, so a
/// failed send is a `Result` they can return - and `pipeline_send_error` in
/// `writer.rs` joins the dead writer thread and surfaces its real cause.
/// `write_primitive_block*` cannot:
/// the send happens inside a framing closure spawned on the writer's
/// `Executor`, whose return value goes
/// nowhere, so it ended in `drop(tx.send(...))` and a writer thread that died
/// on ENOSPC stayed invisible until `flush()`. Nothing throttles the producer
/// either - the permit pool keeps handing out permits because the closures
/// keep *completing* - so a full disk let an entire planet-scale dataset be
/// read, decoded and framed before anything was reported.
///
/// The closure therefore records the failed send here, and the next
/// `write_primitive_block*` on the producer thread reads the flag, joins the
/// writer, and returns its real error. Reporting lags by at most the blocks
/// already in flight; correctness is unchanged either way, since `flush()`
/// always joined and reported in the end.
#[derive(Clone, Default)]
pub(crate) struct WriterGone(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl WriterGone {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record that a send to the writer thread failed, which happens only
    /// because the receiver was dropped, i.e. the thread has already exited.
    pub(super) fn record(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(super) fn observed(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Sentinel meaning "the producer never announced a total".
const UNANNOUNCED: usize = usize::MAX;

/// The producer's end-of-stream handshake: how many [`PipelineItem`]s the
/// writer thread must have delivered before the output file is complete.
///
/// Channel closure alone cannot answer that question. A framing task that
/// dies takes its `tx` clone with it, so the channel still closes cleanly and
/// the reorder buffer is left holding only the items *after* the gap. When
/// the gap is at the tail - the final task dies, or the only task dies -
/// there are no later items, `pending_len()` is zero, and every backend's
/// reorder loop used to flush and return `Ok(())` on a file short by one or
/// more blobs. The count is the missing information, so the producer states
/// it explicitly rather than the writer inferring it.
///
/// [`PbfWriter::flush`](super::writer::PbfWriter::flush) announces the total
/// *before* dropping its sender, so the store always happens-before the
/// disconnect the writer thread observes. The `Drop` path deliberately does
/// not announce: it is already unwinding from some other error, its join
/// result is discarded, and a spurious count mismatch there would only muddy
/// the primary error.
#[derive(Clone)]
pub(crate) struct SeqTotal(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl SeqTotal {
    pub(crate) fn new() -> Self {
        Self(std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
            UNANNOUNCED,
        )))
    }

    /// State the final item count. Called once, before the producer's sender
    /// is dropped.
    pub(crate) fn announce(&self, total: usize) {
        self.0.store(total, std::sync::atomic::Ordering::SeqCst);
    }

    fn announced(&self) -> Option<usize> {
        match self.0.load(std::sync::atomic::Ordering::SeqCst) {
            UNANNOUNCED => None,
            total => Some(total),
        }
    }
}

impl Default for SeqTotal {
    fn default() -> Self {
        Self::new()
    }
}

/// End-of-stream completeness check shared by every pipeline backend.
///
/// `pending_len` and `delivered` come from [`deliver_in_order`]'s reorder
/// buffer after its receive loop breaks on channel close. Two distinct
/// failures:
///
/// * `pending_len > 0` - an *interior* gap. Items arrived that could never
///   pop because an earlier seq never showed up.
/// * `delivered < total` - a *trailing* gap. Nothing is blocked because
///   nothing came after the missing seq; only the announced total reveals it.
///
/// Both mean the file on disk is short. Neither may be reported as success.
fn check_stream_complete(
    backend: &str,
    pending_len: usize,
    delivered: usize,
    expected: &SeqTotal,
) -> Result<()> {
    if pending_len > 0 {
        return Err(Error::internal(format!(
            "{backend}: channel closed with {pending_len} item(s) still in reorder buffer; \
             an upstream framer dropped without sending an earlier seq"
        )));
    }
    if let Some(total) = expected.announced()
        && delivered < total
    {
        return Err(Error::internal(format!(
            "{backend}: channel closed after {delivered} of {total} blob(s); \
             an upstream framer dropped without sending the final seq(s)"
        )));
    }
    Ok(())
}

pub(super) fn record_send_wait(send_start: crate::counters::Stopwatch, metrics: &WriterMetrics) {
    send_start.record_ns(&metrics.pipeline_send_wait_ns);
}

/// The one ordered-delivery loop every pipelined backend runs: receive
/// [`PipelineItem`]s, put them back in sequence order, hand each chunk to
/// `sink`, and on channel close prove the stream complete.
///
/// Uses a shared sequence-number reorder buffer to handle out-of-order
/// arrivals from parallel rayon tasks. Framing errors produced on rayon
/// workers are propagated in order via `Result<OutputChunk>`. For pipeline
/// items, every backend records `writer_recv_wait_ns`,
/// `writer_reorder_high_water`, `writer_handoff_ns` (around [`hand_off`]
/// only; the sink's [`after_drain`](OutputSink::after_drain) is outside it)
/// and `writer_bytes_written` here; the header frame and the synchronous
/// writer account for themselves at their own sites.
///
/// Returns once the stream is proved complete, without finalizing: on `Ok`
/// the caller finishes the output (flush, drain, join, sync). On an error it
/// must not finish it as a success, though teardown still runs (the pool
/// joins every worker, io_uring drains its ring). A dead
/// upstream framer closes the channel exactly the way a finished one does,
/// so finalizing on close alone would turn a file short by an arbitrary
/// number of blobs into a successful write - for io_uring and `O_DIRECT`,
/// padded and `set_len`'d into a well-formed-looking one. Hence
/// [`check_stream_complete`], for an interior gap and for a missing tail.
pub(super) fn deliver_in_order<S: OutputSink>(
    rx: &Receiver<PipelineItem>,
    sink: &mut S,
    expected: &SeqTotal,
    metrics: &WriterMetrics,
) -> Result<()> {
    let mut pending: ReorderBuffer<Result<OutputChunk>> = ReorderBuffer::with_capacity(WRITE_AHEAD);

    loop {
        let t_recv = crate::counters::Stopwatch::start();
        let item = match rx.recv() {
            Ok(item) => item,
            Err(_) => break,
        };
        metrics.record_recv_wait(t_recv);
        pending.push(item.seq, item.data);
        metrics.record_reorder_high_water(pending.pending_len());

        // Drain consecutive ready items from the front.
        while let Some(result) = pending.pop_ready() {
            let chunk = result?;
            metrics.time_handoff(|| hand_off(sink, chunk, metrics))?;
        }

        sink.after_drain()?;
    }

    check_stream_complete(
        S::BACKEND,
        pending.pending_len(),
        pending.next_seq(),
        expected,
    )
}

/// Writer thread of the buffered and `O_DIRECT` backends: deliver, then
/// finish the file.
///
/// On an error the sink is NOT finished, so an `O_DIRECT` sink is finalised
/// by `DirectWriter::Drop` calling `flush_final` best-effort - the short file
/// still gets padded and `set_len`'d. That is deliberate. The padding is not
/// what makes a truncated file look plausible: every backend writes whole
/// blobs, so a short file ends on a blob boundary and reads as a valid
/// shorter PBF regardless. Skipping the `O_DIRECT` finalisation would only
/// leave a partial page of alignment zeros on the end, which is harder to
/// diagnose, not easier. The thing that must not be lost is the *report*,
/// and that is the error returned here.
#[allow(clippy::needless_pass_by_value)] // Thread entry point owns the receiver moved into std::thread::spawn.
pub(super) fn writer_thread(
    rx: Receiver<PipelineItem>,
    mut sink: FileOutputSink,
    expected: &SeqTotal,
) -> Result<()> {
    let invocation = sink.cancel.clone();
    deliver_in_order(&rx, &mut sink, expected, &invocation.metrics().writer)?;
    sink.finish()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{OutputChunk, OutputSink, PipelineItem, SeqTotal, WRITE_AHEAD, deliver_in_order};
    use crate::error::Result;
    use std::sync::mpsc::sync_channel;

    /// Records what actually reached the backend, and how often the loop ran
    /// the sink's between-items maintenance.
    struct RecordingSink {
        written: Vec<Vec<u8>>,
        after_drains: usize,
    }

    impl OutputSink for RecordingSink {
        const BACKEND: &'static str = "recording sink";

        fn write_owned(&mut self, bytes: Vec<u8>) -> Result<()> {
            self.written.push(bytes);
            Ok(())
        }

        fn copy_range(
            &mut self,
            _in_file: crate::read::input::InputFile,
            _offset: u64,
            _len: u64,
        ) -> Result<()> {
            panic!("test fixture only sends Raw chunks")
        }

        fn after_drain(&mut self) -> Result<()> {
            self.after_drains += 1;
            Ok(())
        }
    }

    /// Drive `deliver_in_order` over `seqs`, optionally announcing a final
    /// item count the way `PbfWriter::flush` does before dropping its sender.
    /// An `Err` is what keeps every backend from finalizing the file.
    ///
    /// `announce: None` models the `Drop` path, which never announces.
    fn run(seqs: &[usize], announce: Option<usize>) -> (Result<()>, Vec<Vec<u8>>, usize) {
        let (tx, rx) = sync_channel::<PipelineItem>(WRITE_AHEAD);
        for &seq in seqs {
            tx.send(PipelineItem {
                seq,
                data: Ok(OutputChunk::Raw(vec![u8::try_from(seq).unwrap()])),
            })
            .unwrap();
        }
        let expected = SeqTotal::new();
        if let Some(total) = announce {
            expected.announce(total);
        }
        drop(tx);
        let mut sink = RecordingSink {
            written: Vec::new(),
            after_drains: 0,
        };
        let metrics = crate::write::metrics::WriterMetrics::new();
        let result = deliver_in_order(&rx, &mut sink, &expected, &metrics);
        (result, sink.written, sink.after_drains)
    }

    /// The between-items hook runs once per received item and never before
    /// the first receive: io_uring reaps there, and a reap ahead of the first
    /// item could surface a failed header write before the stream's own first
    /// error.
    #[test]
    fn after_drain_runs_once_per_received_item_and_never_before_the_first() {
        let (result, _written, after_drains) = run(&[], Some(0));
        result.unwrap();
        assert_eq!(after_drains, 0, "nothing received, so no maintenance");

        // Seq 2 arrives first and pops nothing; the hook still runs for it.
        let (result, written, after_drains) = run(&[2, 0, 1], Some(3));
        result.unwrap();
        assert_eq!(written, vec![vec![0u8], vec![1u8], vec![2u8]]);
        assert_eq!(after_drains, 3);
    }

    /// A seq gap at channel close is silent truncation and must be an error.
    ///
    /// This is the headline defect the default backend carried while both
    /// sibling backends guarded it: a framer that never sent seq 1 left
    /// everything after it stranded in the reorder buffer, the loop broke on
    /// channel close, flushed, and returned `Ok(())`. The output file was
    /// short by an arbitrary number of blobs and the command exited zero.
    #[test]
    fn an_interior_sequence_gap_at_channel_close_fails_instead_of_reporting_success() {
        let (result, written, _) = run(&[0, 2, 3], Some(4));
        let err = result.expect_err(
            "seq 1 never arrived, so seqs 2 and 3 were dropped - that is a \
             short file and must not be reported as a successful write",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("still in reorder buffer"),
            "the error must say what was stranded, got: {msg}"
        );
        assert_eq!(
            written,
            vec![vec![0u8]],
            "only seq 0 was contiguous, so only seq 0 should have been written"
        );
    }

    /// The interior guard must stand on its own, with no announced total.
    ///
    /// `Drop` never announces, so on that path `pending_len` is the only
    /// evidence available and it still has to be read.
    #[test]
    fn an_interior_gap_is_caught_even_when_no_total_was_announced() {
        let (result, _written, _) = run(&[0, 2, 3], None);
        let msg = result
            .expect_err("an interior gap is visible without any announced total")
            .to_string();
        assert!(
            msg.contains("still in reorder buffer"),
            "expected the stranded-items diagnosis, got: {msg}"
        );
    }

    /// A gap at the TAIL strands nothing, so `pending_len` stays zero.
    ///
    /// This is the shape the reorder-buffer check alone cannot see and the
    /// first version of this guard missed: it was written and tested against
    /// the interior gap `[0, 2, 3]`, which implies nothing about the tail.
    /// If the last framing task dies, seq 0 arrives, pops cleanly, the buffer
    /// empties, the channel closes - and the file is one blob short with a
    /// zero exit status. Only the announced total distinguishes it from a
    /// complete two-item run.
    #[test]
    fn a_dropped_final_task_fails_even_though_nothing_is_stranded() {
        let (result, written, _) = run(&[0], Some(2));
        let msg = result
            .expect_err(
                "seq 1 was the final task and never arrived; the file is short \
                 by one blob and must not be reported as a successful write",
            )
            .to_string();
        assert!(
            msg.contains("after 1 of 2"),
            "the error must name what arrived and what was expected, got: {msg}"
        );
        assert_eq!(
            written,
            vec![vec![0u8]],
            "seq 0 is contiguous and is legitimately written; the defect is \
             calling that a complete file"
        );
    }

    /// The degenerate tail case: the only task dies, so nothing arrives at
    /// all and the writer sees an immediately-closed, entirely empty channel.
    ///
    /// Indistinguishable from a legitimate header-only write except by the
    /// announced total.
    #[test]
    fn a_single_dropped_task_fails_rather_than_writing_a_header_only_file() {
        let (result, written, _) = run(&[], Some(1));
        let msg = result
            .expect_err("the one and only blob never arrived")
            .to_string();
        assert!(
            msg.contains("after 0 of 1"),
            "expected the count mismatch, got: {msg}"
        );
        assert!(written.is_empty());
    }

    /// The guard must fire on a gap, not on every clean run.
    #[test]
    fn a_complete_sequence_still_writes_and_succeeds() {
        let (result, written, _) = run(&[2, 0, 1], Some(3));
        result.unwrap();
        assert_eq!(written, vec![vec![0u8], vec![1u8], vec![2u8]]);
    }

    /// Passing counterpart to `a_dropped_final_task_fails...`: the same two
    /// item shape with the tail actually present must succeed. Without this,
    /// a regression that failed every run would still pass the gap tests.
    #[test]
    fn a_complete_two_item_run_with_its_tail_present_succeeds() {
        let (result, written, _) = run(&[0, 1], Some(2));
        result.unwrap();
        assert_eq!(written, vec![vec![0u8], vec![1u8]]);
    }

    /// Passing counterpart to the degenerate case: a writer that legitimately
    /// emits no data blobs at all (header-only output) must still succeed.
    #[test]
    fn an_announced_empty_stream_succeeds() {
        let (result, written, _) = run(&[], Some(0));
        result.unwrap();
        assert!(written.is_empty());
    }
}
