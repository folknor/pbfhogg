//! Pipeline plumbing for [`PbfWriter`](super::writer::PbfWriter): ordered
//! output chunks, the sink trait, the bounded permit pool that caps in-flight
//! rayon dispatches, and the writer thread that reorders and drains results
//! to the underlying sink.
//!
//! All items here are internal wiring for the write path; nothing in this
//! module is part of the public library API.

use crate::error::{Error, Result};
use crate::reorder_buffer::ReorderBuffer;
use crate::tunables::{PIPELINE_DELIVERY_WINDOW, PIPELINE_DISPATCH_PERMITS, WRITE_AHEAD};
use crate::write::file_writer::FileWriter;
use crate::write::metrics::WriterMetrics;
use std::io::Write;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;

/// One unit of output the writer thread can consume, independent of backend.
///
/// `Raw` is the common case: one whole frame, either assembled by a framing
/// job (a compressed `OSMData` blob, see `framing::frame_blob_into`) or
/// pre-framed by the caller. `RawChunks` carries several pre-framed buffers
/// (passthrough). `CopyRange` asks the backend to splice bytes from another
/// fd via `copy_file_range`.
pub(crate) enum OutputChunk {
    /// One whole frame's bytes.
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

impl OutputChunk {
    /// Owned output-buffer capacity this chunk holds, in bytes: what
    /// `writer_reorder_bytes_high_water` counts per occupied slot. Capacity,
    /// not length, because a caller-supplied raw buffer can carry spare
    /// capacity that is just as resident; a `RawChunks` adds its outer vector
    /// of buffer descriptors. A `CopyRange` holds no output buffer.
    fn buffer_capacity(&self) -> u64 {
        match self {
            Self::Raw(bytes) => bytes.capacity() as u64,
            Self::RawChunks(chunks) => {
                let outer = (chunks.capacity() as u64) * (size_of::<Vec<u8>>() as u64);
                chunks
                    .iter()
                    .fold(outer, |sum, chunk| sum + chunk.capacity() as u64)
            }
            Self::CopyRange { .. } => 0,
        }
    }
}

/// [`OutputChunk::buffer_capacity`] of one reorder-buffer entry; a framing
/// error holds no output buffer.
fn slot_buffer_capacity(entry: &Result<OutputChunk>) -> u64 {
    entry.as_ref().map_or(0, OutputChunk::buffer_capacity)
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

/// The writer thread's end of a pipeline: the item receiver and the only
/// sender of the delivery window ([`PIPELINE_DELIVERY_WINDOW`]).
///
/// [`deliver_in_order`] returns one window token per item it hands to the
/// sink. Keeping the window's sender here, and nowhere else, makes the two
/// die together: `items` is declared first so it drops first, and once the
/// writer thread lets go of its inbox a producer waiting for a token sees a
/// disconnect instead of waiting on a head that will never be delivered.
pub(crate) struct Inbox {
    pub(crate) items: Receiver<PipelineItem>,
    window: SyncSender<()>,
}

impl Inbox {
    /// An inbox over `items` whose window tokens go nowhere: for tests that
    /// drive [`deliver_in_order`] directly, with no producer to admit.
    #[cfg(test)]
    pub(crate) fn detached(items: Receiver<PipelineItem>) -> Self {
        let (window, _) = sync_channel(0);
        Self { items, window }
    }
}

/// A fresh pipeline's channels: the item sender, the producer's window
/// receiver pre-filled with [`PIPELINE_DELIVERY_WINDOW`] tokens, and the
/// writer thread's [`Inbox`].
#[expect(
    clippy::expect_used,
    reason = "the first N sends on a fresh channel of capacity N, whose receiver is held here, cannot fail or block"
)]
pub(super) fn new_channels() -> (SyncSender<PipelineItem>, Receiver<()>, Inbox) {
    let (tx, items) = sync_channel(WRITE_AHEAD);
    let (window, window_rx) = sync_channel::<()>(PIPELINE_DELIVERY_WINDOW);
    for _ in 0..PIPELINE_DELIVERY_WINDOW {
        window
            .send(())
            .expect("seeding delivery window on fresh channel");
    }
    (tx, window_rx, Inbox { items, window })
}

/// One reserved output position: the obligation to deliver exactly one
/// result for `seq`.
///
/// Created by the reservation that assigns the sequence number, so no seq
/// exists without one. [`send`](Self::send) discharges it whatever the
/// outcome (a failed send proves the writer thread is gone). Dropped
/// undischarged - a framing job that panicked, a closure dropped unrun, a
/// producer unwinding between reservation and send - it sends an ordered
/// error for its seq instead. Under the delivery window a missing seq is not
/// a gap reported at end of stream: the head would never be delivered, the
/// window would never refill, and the producer would wait forever. The
/// fallback send may block on a full channel, which is safe because the
/// writer thread receives in any order on its own thread.
pub(super) struct Slot {
    pub(super) seq: usize,
    tx: Option<SyncSender<PipelineItem>>,
}

impl Slot {
    pub(super) fn new(seq: usize, tx: SyncSender<PipelineItem>) -> Self {
        Self { seq, tx: Some(tx) }
    }

    /// Send this slot's result. `Err` means the writer thread has exited.
    pub(super) fn send(mut self, data: Result<OutputChunk>) -> std::result::Result<(), ()> {
        match self.tx.take() {
            Some(tx) => tx
                .send(PipelineItem {
                    seq: self.seq,
                    data,
                })
                .map_err(|_| ()),
            None => Err(()),
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            // A failed send means the writer thread is gone; nothing to tell.
            tx.send(PipelineItem {
                seq: self.seq,
                data: Err(Error::internal(format!(
                    "pipelined writer: output item {} ended without a result",
                    self.seq
                ))),
            })
            .ok();
        }
    }
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
/// Every chunk arrives as owned buffers, and accepted bytes are counted
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
pub(crate) struct FileOutputSink {
    writer: FileWriter,
    finish: super::Finish,
    /// Checked between `copy_file_range` calls of a queued splice; the
    /// writer's counters record into its invocation.
    cancel: crate::host::Permit,
}

impl FileOutputSink {
    pub(crate) fn new(
        writer: FileWriter,
        finish: super::Finish,
        cancel: crate::host::Permit,
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
    /// executor; the framing closure `send()`s it back as the last thing it
    /// does, which also makes the pool the writer's barrier on its framing
    /// jobs ([`wait_for_framing`](Self::wait_for_framing)). See
    /// [`PIPELINE_DISPATCH_PERMITS`] for what it does and does not bound.
    pub(super) permit_tx: SyncSender<()>,
    pub(super) permit_rx: Receiver<()>,
    /// The producer's side of the delivery window: one token per item from
    /// reservation to ordered handoff. Its only sender is the writer
    /// thread's [`Inbox`]; see [`PIPELINE_DELIVERY_WINDOW`].
    pub(super) window_rx: Receiver<()>,
    /// End-of-stream handshake shared with the writer thread. `flush`
    /// announces `seq` into it before dropping `tx`; see [`SeqTotal`].
    pub(super) expected: SeqTotal,
    /// Death notice from the framing closures; see [`WriterGone`].
    pub(super) writer_gone: WriterGone,
    /// The original failure recovered by a producer after the thread exited.
    pub(super) recovered_error: Option<Error>,
    /// Where the framing closures run, and whether the producer is on one of
    /// its workers (see [`recv_permit`]).
    pub(super) executor: crate::host::Executor,
    /// The output's writing session, when this writer is driven by an
    /// [`Output`](crate::Output). A failure recovered from the writer thread
    /// poisons it immediately, before the caller can publish the output.
    pub(super) completion: Option<crate::output::Completion>,
    /// The invocation whose writer counters the producer side records into.
    pub(super) invocation: crate::host::Permit,
}

impl WritePipeline {
    /// A pipeline over a freshly spawned writer thread, with a full permit
    /// pool and nothing sent yet.
    pub(super) fn new(
        tx: SyncSender<PipelineItem>,
        window_rx: Receiver<()>,
        join_handle: JoinHandle<Result<()>>,
        expected: SeqTotal,
        executor: crate::host::Executor,
        invocation: crate::host::Permit,
    ) -> Self {
        let (permit_tx, permit_rx) = new_permit_pool();
        Self {
            tx,
            seq: 0,
            join_handle: Some(join_handle),
            permit_tx,
            permit_rx,
            window_rx,
            expected,
            writer_gone: WriterGone::new(),
            recovered_error: None,
            executor,
            completion: None,
            invocation,
        }
    }

    /// Spawn a writer thread running `body` over a fresh channel and
    /// end-of-stream handshake, and wrap it in a pipeline.
    pub(super) fn spawn<F>(
        executor: crate::host::Executor,
        invocation: crate::host::Permit,
        body: F,
    ) -> Self
    where
        F: FnOnce(Inbox, &SeqTotal) -> Result<()> + Send + 'static,
    {
        let (tx, window_rx, inbox) = new_channels();
        let expected = SeqTotal::new();
        let thread_expected = expected.clone();
        #[cfg(pbfhogg_test_hooks)]
        let exit = WriterExit(crate::host::Invocation::clone(&invocation));
        #[allow(
            clippy::disallowed_methods,
            reason = "a session thread: the writer keeps its handle and joins it at its end"
        )]
        let handle = std::thread::spawn(move || {
            // Bound before the body runs and dropped after it returns or
            // unwinds, so after the body's inbox is gone.
            #[cfg(pbfhogg_test_hooks)]
            let _exit = exit;
            body(inbox, &thread_expected)
        });
        Self::new(tx, window_rx, handle, expected, executor, invocation)
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
        invocation: crate::host::Permit,
        body: F,
    ) -> Result<Self>
    where
        F: FnOnce(Inbox, SyncSender<Result<()>>, &SeqTotal) -> Result<()> + Send + 'static,
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

/// Reports a pipelined writer thread's end to its invocation's fault plan
/// when dropped (hook builds only). Holds an invocation handle, not a
/// permit, so it never keeps the invocation from sealing.
#[cfg(pbfhogg_test_hooks)]
struct WriterExit(crate::host::Invocation);

#[cfg(pbfhogg_test_hooks)]
impl Drop for WriterExit {
    fn drop(&mut self) {
        self.0.faults().on_writer_thread_exit();
    }
}

/// Take one dispatch permit.
///
/// Off the executor's workers - every CLI path - this is a plain blocking
/// `recv`, today's path byte for byte. On a worker of the executor's pool a
/// blocking `recv` could park the only thread able to run the framing work
/// that returns the permit (a one-thread pool deadlocks outright), so the wait
/// is cooperative: run the pool's pending jobs while none is free, then block
/// once the pool reports idle. The cooperative time is added to
/// `writer_coop_wait_ns`, which is therefore zero unless a `PbfWriter` is
/// driven from a rayon worker.
///
/// Blocking after `Yield::Idle` needs no timed recheck, by these properties
/// of the jobs a permit (or a delivery-window token) waits on. Framing jobs
/// are spawned only by the producer, which is this thread and cannot spawn
/// while it waits. `yield_now` searches this worker's deque, every other
/// worker's (retrying a contended steal) and the injector, and a steal
/// executes the job it takes rather than moving it to another deque; so
/// idle means every framing job spawned so far has finished or is running
/// on some thread. A framing job neither spawns nor yields: it frames,
/// sends its slot to the writer thread and drops its `PermitGuard`, whose
/// send is the notification; window tokens come back from the writer thread
/// over the same kind of channel, and its exit disconnects it. If framing
/// jobs ever spawn follow-up work or are spawned from another thread, this
/// argument breaks and the wait needs a notification for them.
pub(super) fn recv_permit(
    rx: &Receiver<()>,
    executor: &crate::host::Executor,
    metrics: &WriterMetrics,
) -> std::result::Result<(), std::sync::mpsc::RecvError> {
    use std::sync::mpsc::{RecvError, TryRecvError};
    if !executor.on_worker() {
        return rx.recv();
    }
    let start = crate::counters::Stopwatch::start();
    // loop-ends: runs pending pool jobs while empty, then blocks in recv;
    // only the producer spawns framing jobs, so Idle means none can return
    // a permit except through the channel
    let result = loop {
        match rx.try_recv() {
            Ok(()) => break Ok(()),
            Err(TryRecvError::Disconnected) => break Err(RecvError),
            Err(TryRecvError::Empty) => match executor.yield_now() {
                Some(rayon::Yield::Executed) => {}
                Some(rayon::Yield::Idle) | None => break rx.recv(),
            },
        }
    };
    start.record_ns(&metrics.coop_wait_ns);
    result
}

impl WritePipeline {
    /// Wait until every framing job this pipeline dispatched has ended its
    /// work: take back every one of the [`PIPELINE_DISPATCH_PERMITS`] tokens
    /// through [`recv_permit`], then return them all.
    ///
    /// The proof is conservation. Every token is in the pool, held by a
    /// producer-side send, or owned by a framing job's [`PermitGuard`], which
    /// a job drops after everything else it holds (its invocation, sender and
    /// block bytes; see `write_osm_data`), on return and on unwind. The
    /// producer is the only acquirer and the thread running this, so holding
    /// all of them at once means no job still holds its invocation or bytes
    /// or can record a counter. That is the whole claim: rayon's own job
    /// machinery, and a pool's non-aborting `panic_handler` after a framing
    /// panic, may still be running on the pool. All are taken before any is
    /// returned (returning each as it arrives could see the same free token
    /// again and again), and returning them makes the barrier repeatable: a
    /// `flush` or `Drop` after a failed write runs it again, instantly.
    ///
    /// Every way a pipelined writer ends runs this before it joins the writer
    /// thread - `flush`, `Drop`, and a write that recovers the writer thread's
    /// error (`pipeline_send_error`). The writer thread stops at its first
    /// error ([`deliver_in_order`]) and drops its receiver, so joining it
    /// proves nothing about the jobs still queued behind it; without this they
    /// would go on framing, and moving the invocation's counters, after the
    /// writer had returned. Not a cancellation checkpoint (ADR-0019): queued
    /// work winds down.
    ///
    /// On a worker of the executor's pool the wait is cooperative (see
    /// [`recv_permit`]), so a producer on the pool's only free thread still
    /// lets the queued jobs run. Off it the wait blocks, and so depends on the
    /// executor running the jobs independently of this thread, as the join of
    /// a successful stream always did.
    pub(super) fn wait_for_framing(&self) {
        let metrics = self.metrics();
        let mut taken = 0;
        #[cfg(pbfhogg_test_hooks)]
        self.invocation
            .faults()
            .on_framing_wait(taken, PIPELINE_DISPATCH_PERMITS);
        while taken < PIPELINE_DISPATCH_PERMITS {
            // The pipeline holds `permit_tx`, so the pool never disconnects;
            // a disconnect could only mean no job is left to wait for.
            if recv_permit(&self.permit_rx, &self.executor, metrics).is_err() {
                break;
            }
            taken += 1;
            #[cfg(pbfhogg_test_hooks)]
            self.invocation
                .faults()
                .on_framing_wait(taken, PIPELINE_DISPATCH_PERMITS);
        }
        #[cfg(pbfhogg_test_hooks)]
        self.invocation.faults().after_framing_wait();
        for _ in 0..taken {
            // Cannot block: the channel holds `PIPELINE_DISPATCH_PERMITS`
            // and every token is in this thread's hand. Cannot fail: the
            // pipeline holds the receiver.
            self.permit_tx.send(()).ok();
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
/// also puts the release after the `tx.send` of the framed result, so a
/// permit counts its job until the result is in the channel. From then on
/// the result waits in the channel or the writer's reorder buffer, which
/// permits do not bound (the bound there is the delivery window, [`PIPELINE_DELIVERY_WINDOW`]).
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
/// `writer_reorder_high_water`, `writer_reorder_filled_high_water`,
/// `writer_reorder_bytes_high_water`, `writer_handoff_ns` (around [`hand_off`]
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
///
/// Each item handed to the sink returns one delivery-window token to the
/// producer, after the whole handoff succeeds. That is what bounds the
/// reorder buffer: the producer reserves at most
/// [`PIPELINE_DELIVERY_WINDOW`] items past the last one handed off, and the
/// buffer's next seq is never behind that, so its window is pinned to the
/// same value and a push past it is a broken admission invariant, not load.
/// The token send cannot block (each returned token was taken by the item
/// being returned, so the channel has room) and a failed one only means the
/// producer is gone.
pub(super) fn deliver_in_order<S: OutputSink>(
    inbox: &Inbox,
    sink: &mut S,
    expected: &SeqTotal,
    metrics: &WriterMetrics,
) -> Result<()> {
    let mut pending: ReorderBuffer<Result<OutputChunk>> =
        ReorderBuffer::with_max_window(WRITE_AHEAD, PIPELINE_DELIVERY_WINDOW);
    // Sum of `slot_buffer_capacity` over the occupied slots: added on push,
    // subtracted on pop, and nothing mutates an entry in between, so it is
    // exact. Local to this loop; an early return abandons it with the buffer.
    let mut pending_bytes: u64 = 0;

    loop {
        let t_recv = crate::counters::Stopwatch::start();
        let item = match inbox.items.recv() {
            Ok(item) => item,
            Err(_) => break,
        };
        metrics.record_recv_wait(t_recv);
        let item_bytes = slot_buffer_capacity(&item.data);
        pending.push(item.seq, item.data);
        pending_bytes += item_bytes;
        metrics.record_reorder_high_water(
            pending.pending_len(),
            pending.filled_len(),
            pending_bytes,
        );

        // Drain consecutive ready items from the front.
        while let Some(result) = pending.pop_ready() {
            pending_bytes -= slot_buffer_capacity(&result);
            let chunk = result?;
            metrics.time_handoff(|| hand_off(sink, chunk, metrics))?;
            inbox.window.send(()).ok();
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
    inbox: Inbox,
    mut sink: FileOutputSink,
    expected: &SeqTotal,
) -> Result<()> {
    let invocation = sink.cancel.clone();
    deliver_in_order(&inbox, &mut sink, expected, &invocation.metrics().writer)?;
    sink.finish()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{
        Inbox, OutputChunk, OutputSink, PIPELINE_DELIVERY_WINDOW, PipelineItem, SeqTotal, Slot,
        WRITE_AHEAD, deliver_in_order, new_channels,
    };
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
        let metrics = crate::write::metrics::WriterMetrics::new();
        run_with(seqs, announce, &metrics)
    }

    fn run_with(
        seqs: &[usize],
        announce: Option<usize>,
        metrics: &crate::write::metrics::WriterMetrics,
    ) -> (Result<()>, Vec<Vec<u8>>, usize) {
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
        let result = deliver_in_order(&Inbox::detached(rx), &mut sink, &expected, metrics);
        (result, sink.written, sink.after_drains)
    }

    /// Every item handed to the sink gives its window token back, and an
    /// item stuck behind a missing head does not: the window refills exactly
    /// as far as delivery got.
    #[test]
    fn the_window_refills_only_as_far_as_ordered_delivery() {
        let (tx, window_rx, inbox) = new_channels();
        // Reserve three positions the way the producer does.
        // loop-ends: three reservations from a full window; never waits
        for _ in 0..3 {
            window_rx.try_recv().unwrap();
        }
        for seq in [0, 2] {
            tx.send(PipelineItem {
                seq,
                data: Ok(OutputChunk::Raw(vec![0])),
            })
            .unwrap();
        }
        drop(tx);
        let mut sink = RecordingSink {
            written: Vec::new(),
            after_drains: 0,
        };
        let metrics = crate::write::metrics::WriterMetrics::new();
        let result = deliver_in_order(&inbox, &mut sink, &SeqTotal::new(), &metrics);
        assert!(result.is_err(), "seq 1 never arrived");
        assert_eq!(sink.written.len(), 1);
        assert_eq!(
            window_rx.try_iter().count(),
            PIPELINE_DELIVERY_WINDOW - 2,
            "only seq 0's token returns; seq 2 waits behind the missing seq 1"
        );
    }

    /// A slot dropped without sending (a panicked or never-run framing job)
    /// still delivers an ordered error for its seq, so the head is never
    /// silently missing.
    #[test]
    fn a_dropped_slot_sends_an_ordered_error() {
        let (tx, rx) = sync_channel::<PipelineItem>(1);
        drop(Slot::new(7, tx));
        let item = rx.recv().unwrap();
        assert_eq!(item.seq, 7);
        let err = item.data.err().unwrap();
        assert!(err.to_string().contains("ended without a result"), "{err}");
        assert!(rx.recv().is_err(), "exactly one result per slot");
    }

    /// `writer_reorder_high_water` is the seq span, gaps included;
    /// `writer_reorder_filled_high_water` is the occupied slots. Both are
    /// sampled after the push, so in-order delivery reads one for each.
    #[test]
    fn reorder_counters_separate_span_from_occupancy() {
        use std::sync::atomic::Ordering::Relaxed;

        let metrics = crate::write::metrics::WriterMetrics::new();
        let (result, _, _) = run_with(&[0, 1, 2], Some(3), &metrics);
        result.unwrap();
        assert_eq!(metrics.reorder_high_water.load(Relaxed), 1);
        assert_eq!(metrics.reorder_filled_high_water.load(Relaxed), 1);

        // Seq 3 first: span 4 (seqs 0..=3) with one slot filled. Each later
        // push is sampled before it drains, so the filled peak is 2 (seq 3
        // plus the arrival), while the span never exceeds 4.
        let metrics = crate::write::metrics::WriterMetrics::new();
        let (result, _, _) = run_with(&[3, 0, 1, 2], Some(4), &metrics);
        result.unwrap();
        assert_eq!(metrics.reorder_high_water.load(Relaxed), 4);
        assert_eq!(metrics.reorder_filled_high_water.load(Relaxed), 2);
        // Each fixture frame is `vec![seq]`, one byte of capacity.
        assert_eq!(metrics.reorder_bytes_high_water.load(Relaxed), 2);
    }

    /// `writer_reorder_bytes_high_water` counts owned buffer capacity per
    /// occupied slot: spare capacity counts, a `RawChunks` adds its outer
    /// vector, and a framing error holds nothing. Popping subtracts exactly
    /// what the push added, so a later peak is not inflated by delivered
    /// items, and the error pop ends the loop with the error.
    #[test]
    fn reorder_bytes_count_capacity_per_occupied_slot() {
        use std::sync::atomic::Ordering::Relaxed;

        let metrics = crate::write::metrics::WriterMetrics::new();
        let (tx, rx) = sync_channel::<PipelineItem>(WRITE_AHEAD);
        let send = |seq, data| tx.send(PipelineItem { seq, data }).unwrap();
        let spare = Vec::with_capacity(1000);
        let mut outer = Vec::with_capacity(4);
        outer.push(Vec::with_capacity(300));
        outer.push(Vec::with_capacity(200));
        let outer_bytes = 4 * size_of::<Vec<u8>>() as u64;

        // Seqs 1 and 2 wait behind seq 0: 1000 + chunks, peak with seq 0's
        // 10 on top, all three then delivered.
        send(1, Ok(OutputChunk::Raw(spare)));
        send(2, Ok(OutputChunk::RawChunks(outer)));
        send(0, Ok(OutputChunk::Raw(Vec::with_capacity(10))));
        // An error slot behind seq 3 adds nothing; seq 3 alone is far below
        // the earlier peak, and the error is what the loop returns.
        send(4, Err(crate::error::Error::internal("framing failed")));
        send(3, Ok(OutputChunk::Raw(Vec::with_capacity(7))));
        drop(tx);

        let mut sink = RecordingSink {
            written: Vec::new(),
            after_drains: 0,
        };
        let result = deliver_in_order(&Inbox::detached(rx), &mut sink, &SeqTotal::new(), &metrics);
        let err = result.expect_err("seq 4 is a framing error");
        assert!(err.to_string().contains("framing failed"), "{err}");
        assert_eq!(
            metrics.reorder_bytes_high_water.load(Relaxed),
            1000 + 300 + 200 + outer_bytes + 10
        );
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
