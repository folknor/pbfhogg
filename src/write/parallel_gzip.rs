//! Parallel gzip writer: chunked producer + rayon compress + ordered drain.
//!
//! Wraps an inner `W: Write + Send + 'static` and implements `Write` by
//! accumulating uncompressed bytes into fixed-size chunks, compressing
//! each chunk in a worker pool, and writing the compressed chunks back
//! to the inner writer in submission order.
//!
//! Gzip supports concatenated members: a file consisting of multiple
//! back-to-back gzip streams (each with its own header + DEFLATE body +
//! trailer) decompresses as a single logical stream. Every standard
//! gzip reader (`gunzip`, `zcat`, `flate2::read::GzDecoder`, Python
//! `gzip`, Node `zlib`) handles this - `pigz` and RFC 1952 both rely
//! on it. Independent compression per chunk costs ~1-3% on the ratio
//! (no cross-chunk dictionary) in exchange for near-linear CPU scaling
//! on the compress side.
//!
//! The only in-crate consumer is `diff --format osc` assembly, which
//! was the serial-tail ceiling on `-j 16` runs (32.8 s at planet, ~10%
//! of the full run wall).

//!
//! ## Failure
//!
//! Every thread of one writer shares a
//! [`FirstFailure`](crate::write::pool_failure::FirstFailure): a compress
//! error, a worker or writer-thread panic (payload message kept), or an
//! inner-writer I/O error is recorded there, first one wins, and that first
//! failure is what `flush`, `write` and `finish` report. Recording also stops
//! the pool: workers forward a [`Out::Dropped`] placeholder instead of
//! compressing, and the writer thread stops writing to the inner writer, but
//! every seq still reaches the writer thread so flush barriers are answered.
//! `finish` and `Drop` join every thread before they return.

use std::io::{self, Write};
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use flate2::write::GzEncoder;

use crate::error::Error;
use crate::reorder_buffer::ReorderBuffer;
pub use crate::tunables::DEFAULT_CHUNK_SIZE;
use crate::tunables::{GZIP_REORDER_PERMIT_POLL, QUEUE_SLOTS_PER_WORKER};
use crate::write::metrics::WriterMetrics;
use crate::write::pool_failure::FirstFailure;

/// Compression level used for every gzip member. `Compression::fast`
/// (DEFLATE level 1) matches what the serial `assemble_osc` path used
/// to hand to `flate2::GzEncoder`, so compressed size is directly
/// comparable in benches. This is OSC gzip policy, independent of the PBF
/// blob `Compression` setting; sharing that setting would also make raw/zstd
/// PBF choices appear applicable to an inherently gzip stream.
// merge_changes and tags-filter select `Compression::fast` at their own
// writers. Moving this value alone would leave those paths with separate
// owners and would imply a shared compression policy that the current tree
// does not have.
fn compression_level() -> flate2::Compression {
    flate2::Compression::fast()
}

/// One unit of work handed to the compressor pool.
///
/// `Barrier` is not compressed: it rides the same seq-ordered path as a
/// chunk so that by the time the writer thread reaches it, every chunk
/// dispatched before it has already been written. See
/// [`ParallelGzipWriter::flush`].
enum Job {
    Compress(Vec<u8>),
    Barrier(SyncSender<io::Result<()>>),
}

/// One unit of output handed to the writer thread, reordered by seq.
enum Out {
    Bytes(Vec<u8>),
    Barrier(SyncSender<io::Result<()>>),
    /// This seq produced no bytes: compressing it failed or panicked, or the
    /// pool had already stopped and the worker skipped it. The failure is in
    /// the shared [`FirstFailure`], recorded before this was sent.
    ///
    /// Every dequeued seq yields exactly one `Out`, so the seq space has no
    /// gap: a gap would park the writer thread short of any flush barrier
    /// queued behind it, and a producer blocked in
    /// [`ParallelGzipWriter::flush`] would hang while the surviving workers
    /// keep the channel open. This used to be a `Drop` guard firing on the
    /// unwind of a panicking worker; the dequeue-to-send span now runs under
    /// `catch_unwind` instead (see [`compress_job`]), so the placeholder is
    /// sent on an ordinary path, after the panic is on record.
    Dropped,
}

/// Writer that buffers bytes, dispatches chunks to a worker pool for
/// gzip compression, and emits compressed chunks in order to an inner
/// writer. A sequence permit is held from dispatch until ordered drain, so a
/// slow early chunk cannot make the reorder buffer retain the rest of the
/// stream.
///
/// Call [`finish`](Self::finish) to flush the final partial chunk,
/// join the workers + writer thread, and recover the inner writer
/// along with any deferred I/O error. Dropping without `finish` does a
/// best-effort flush (errors can't propagate through `Drop`) and still joins
/// every thread; that is the expected path for an error bail-out, where the
/// primary error dominates. The crate's own unit-test builds assert in `Drop`
/// when a non-panicking path skipped `finish`; see `PbfWriter`'s `Drop` for
/// why that check is not enabled in every debug build.
pub struct ParallelGzipWriter<W: Write + Send + 'static> {
    chunk_size: usize,
    current: Vec<u8>,
    /// The next sequence number, which is also the count of jobs (chunks and
    /// barriers) handed to the pool so far: it moves only on a successful
    /// send. `finish` compares it with the writer thread's delivered count.
    next_seq: usize,
    #[cfg(test)]
    /// Whether `finish` has joined the workers and writer thread.
    finished: bool,
    raw_tx: Option<SyncSender<(usize, Job)>>,
    reorder_permit_rx: Receiver<()>,
    /// Joins to the inner writer and the count of sequence numbers it
    /// delivered in order, or `None` with the failure recorded.
    writer_handle: Option<JoinHandle<Option<(W, usize)>>>,
    worker_handles: Vec<JoinHandle<()>>,
    failure: Arc<FirstFailure>,
    /// The invocation the `writer_parallel_gzip_*` counters record into, and
    /// whose fault plan (`crate::test_hooks`) the workers and `flush`
    /// consult; the pool has no cancellation of its own.
    cancel: crate::host::Invocation,
}

impl<W: Write + Send + 'static> ParallelGzipWriter<W> {
    /// Spawn the worker pool and writer thread, returning a handle that
    /// implements [`Write`]. `worker_count` is the number of parallel
    /// compressor threads; pick `std::thread::available_parallelism()`
    /// (or a value derived from it) for a CPU-bound workload. Falls
    /// back to 1 if `worker_count == 0`.
    ///
    /// A thread that fails to spawn does not panic out of here (which would
    /// detach the threads already spawned, there being no `Self` yet whose
    /// `Drop` could join them): the failure is recorded, the writer comes back
    /// already stopped, and the first call that dispatches to the pool - a
    /// `write` that fills a chunk, `flush`, or `finish` - reports it, while
    /// `finish`/`Drop` join whatever did start. A `write` that only buffers
    /// does not check: `write` is called per small fragment on the derive
    /// path, and checking there would put a load on every call to report the
    /// error at most one chunk sooner.
    ///
    /// A zero `chunk_size` is treated as one byte.
    ///
    /// `cancel` is the invocation's token. The pool has no cancellation of
    /// its own; its `writer_parallel_gzip_*` counters record into the
    /// invocation, and under the `test-hooks` feature the workers and `flush`
    /// read the token's fault plan (`crate::test_hooks`).
    pub fn new(
        inner: W,
        chunk_size: usize,
        worker_count: usize,
        cancel: &crate::host::Invocation,
    ) -> Self {
        let cancel = cancel.clone();
        let chunk_size = chunk_size.max(1);
        let worker_count = worker_count.max(1);
        let raw_cap = worker_count * QUEUE_SLOTS_PER_WORKER;
        let compressed_cap = worker_count * QUEUE_SLOTS_PER_WORKER;
        // Bound dispatched sequence numbers to the combined queue and active
        // worker capacity. The producer can additionally own its current
        // chunk and one chunk waiting for admission. Queue capacities alone do
        // not bound `pending`: writer_loop drains completed chunks out of the
        // compressed queue while an earlier sequence is still running.
        //
        // The permits are what make this a bound on seq *distance*, which is
        // what the reorder buffer's window measures: a permit is taken before
        // a seq is assigned and returned only after that seq is handled in
        // order, so on the healthy path every seq the writer thread receives
        // is less than `reorder_window` ahead of its next expected seq. The
        // writer thread's buffer is built with exactly this window, so a
        // breach is refused at insertion rather than tolerated.
        let reorder_window = raw_cap + worker_count + compressed_cap;
        let (reorder_permit_tx, reorder_permit_rx) = sync_channel::<()>(reorder_window);
        for _ in 0..reorder_window {
            // The receiver is live and the channel capacity equals the number
            // of initial permits, so every send fits without blocking.
            reorder_permit_tx.send(()).ok();
        }
        let (raw_tx, raw_rx) = sync_channel::<(usize, Job)>(raw_cap);
        let (compressed_tx, compressed_rx) = sync_channel::<(usize, Out)>(compressed_cap);
        let failure = Arc::new(FirstFailure::new());

        let raw_rx = Arc::new(Mutex::new(raw_rx));

        let mut worker_handles = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let raw_rx = Arc::clone(&raw_rx);
            let compressed_tx = compressed_tx.clone();
            let worker_failure = Arc::clone(&failure);
            #[cfg(feature = "test-hooks")]
            let worker_cancel = cancel.clone();
            let spawned = std::thread::Builder::new().spawn(move || {
                // Backstop only: `compress_job` already catches everything
                // between dequeue and send. Recorded before `compressed_tx`
                // drops, so the writer thread cannot see the disconnect first.
                let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    worker_loop(
                        &raw_rx,
                        &compressed_tx,
                        &worker_failure,
                        #[cfg(feature = "test-hooks")]
                        worker_cancel.faults(),
                    );
                }));
                if let Err(payload) = caught {
                    worker_failure.record_panic("parallel gzip worker", payload.as_ref());
                }
                drop(compressed_tx);
            });
            match spawned {
                Ok(handle) => worker_handles.push(handle),
                Err(e) => {
                    failure.record(
                        Error::from(e).context(format!("spawn parallel gzip worker {index}")),
                    );
                    break;
                }
            }
        }
        // Drop the extra sender so the last worker closing their sender
        // actually closes the channel on the writer thread.
        drop(compressed_tx);

        let writer_failure = Arc::clone(&failure);
        let writer_invocation = cancel.clone();
        let spawned = std::thread::Builder::new().spawn(move || {
            // Backstop as above: `writer_loop` runs every inner-writer call
            // under `guarded`. The receiver is owned out here so it is only
            // dropped after a panic has been recorded.
            let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
                writer_loop(
                    inner,
                    &compressed_rx,
                    &writer_failure,
                    &reorder_permit_tx,
                    reorder_window,
                    &writer_invocation.metrics().writer,
                )
            }));
            let out = caught.unwrap_or_else(|payload| {
                writer_failure.record_panic("parallel gzip writer thread", payload.as_ref());
                None
            });
            drop(compressed_rx);
            out
        });
        let writer_handle = match spawned {
            Ok(handle) => Some(handle),
            Err(e) => {
                failure.record(Error::from(e).context("spawn parallel gzip writer thread"));
                None
            }
        };

        Self {
            chunk_size,
            current: Vec::with_capacity(chunk_size),
            next_seq: 0,
            #[cfg(test)]
            finished: false,
            raw_tx: Some(raw_tx),
            reorder_permit_rx,
            writer_handle,
            worker_handles,
            failure,
            cancel,
        }
    }

    /// The recorded first failure, restated, or `fallback` if (impossibly)
    /// nothing is on record. For producer-side calls that must report now;
    /// `finish` surfaces the original.
    fn pool_error(&self, fallback: &str) -> io::Error {
        self.failure
            .restate()
            .unwrap_or_else(|| Error::internal(format!("ParallelGzipWriter: {fallback}")).into())
    }

    /// Wait for room in the sequence window. A timeout only polls the shared
    /// failure state; it does not reduce the window or add a delay when a
    /// permit is already available. This also lets an unexpected worker exit
    /// release the producer instead of leaving it parked behind a lost seq.
    fn wait_for_reorder_permit(&self) -> io::Result<()> {
        match self.reorder_permit_rx.try_recv() {
            Ok(()) => return Ok(()),
            Err(TryRecvError::Disconnected) => {
                return Err(self.pool_error("writer stopped before releasing reorder capacity"));
            }
            Err(TryRecvError::Empty) => {}
        }

        let start = crate::counters::Stopwatch::start();
        let metrics = &self.cancel.metrics().writer;
        metrics
            .parallel_gzip_reorder_blocked
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let result = loop {
            if self.failure.stopped() {
                break Err(self.pool_error("pool stopped while waiting for reorder capacity"));
            }
            match self
                .reorder_permit_rx
                .recv_timeout(GZIP_REORDER_PERMIT_POLL)
            {
                Ok(()) => break Ok(()),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    break Err(self.pool_error("writer stopped before releasing reorder capacity"));
                }
            }
        };
        start.record_ns(&metrics.parallel_gzip_reorder_wait_ns);
        result
    }

    /// Send one job to the pool under the next sequence number.
    ///
    /// Fails fast once the pool has stopped, so a producer stops feeding a
    /// pipeline that will discard everything it is given.
    fn dispatch(&mut self, job: Job) -> io::Result<()> {
        if self.failure.stopped() {
            return Err(self.pool_error("pool stopped"));
        }
        let tx = self.raw_tx.as_ref().ok_or_else(|| {
            io::Error::from(Error::invalid_argument(
                "ParallelGzipWriter: already finished",
            ))
        })?;
        self.wait_for_reorder_permit()?;
        let seq = self.next_seq;
        if tx.send((seq, job)).is_err() {
            return Err(self.pool_error("worker pool dropped"));
        }
        self.next_seq += 1;
        Ok(())
    }

    /// Close the raw channel and join every thread: workers first (they must
    /// finish before the writer sees a closed compressed channel), then the
    /// writer thread, whose inner writer and delivered count are returned
    /// when it succeeded. Never returns early, so no thread outlives the call.
    fn shutdown(&mut self) -> Option<(W, usize)> {
        self.raw_tx = None;
        for h in std::mem::take(&mut self.worker_handles) {
            if let Err(payload) = h.join() {
                self.failure
                    .record_panic("parallel gzip worker", payload.as_ref());
            }
        }
        match self.writer_handle.take()?.join() {
            Ok(inner) => inner,
            Err(payload) => {
                self.failure
                    .record_panic("parallel gzip writer thread", payload.as_ref());
                None
            }
        }
    }

    /// Dispatch the current buffer as a raw chunk to the worker pool.
    fn flush_current(&mut self) -> io::Result<()> {
        if self.current.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::replace(&mut self.current, Vec::with_capacity(self.chunk_size));
        self.dispatch(Job::Compress(chunk))
    }

    /// Flush any buffered bytes, drop the raw-send channel so workers
    /// drain, join everything, and return the inner writer.
    ///
    /// Every thread is joined before this returns, whatever failed, and the
    /// error returned is the first failure any of them recorded. This used to
    /// `?` out of the join loop on the first panicked worker, reporting a
    /// fixed "worker panicked" with the payload discarded, and ranking
    /// failures by join order rather than by which happened first.
    pub fn finish(mut self) -> io::Result<W> {
        // Ship the final partial chunk.
        let shipped = self.flush_current();
        let inner = self.shutdown();
        #[cfg(test)]
        {
            self.finished = true;
        }
        // Every thread is joined; the record is final.
        if let Some(e) = self.failure.take() {
            return Err(e);
        }
        shipped?;
        let (inner, delivered) = inner.ok_or_else(|| {
            io::Error::from(Error::internal(
                "ParallelGzipWriter: writer thread returned no writer",
            ))
        })?;
        // The trailing-gap proof. The writer thread's own check sees only an
        // interior gap (outputs stranded behind a missing seq); a seq lost at
        // the tail strands nothing. The producer is the one party that knows
        // how many seqs it sent, and after the join there is nothing left to
        // race, so the comparison needs no shared handshake. No path loses a
        // seq without recording a failure today (every dequeued job answers
        // with exactly one `Out`), so this is the guard on that contract.
        if delivered != self.next_seq {
            return Err(Error::internal(format!(
                "ParallelGzipWriter: writer thread delivered {delivered} of {} sequence numbers",
                self.next_seq
            ))
            .into());
        }
        Ok(inner)
    }
}

impl<W: Write + Send + 'static> Write for ParallelGzipWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut offset = 0;
        while offset < buf.len() {
            let space = self.chunk_size - self.current.len();
            let take = space.min(buf.len() - offset);
            self.current.extend_from_slice(&buf[offset..offset + take]);
            offset += take;
            if self.current.len() >= self.chunk_size {
                self.flush_current()?;
            }
        }
        Ok(buf.len())
    }

    /// Push every buffered byte through the pool and the writer thread into
    /// the inner writer, flush the inner writer, and report any error the
    /// journey produced.
    ///
    /// This used to be `Ok(())` - a documented no-op, on the reasoning that
    /// forcing a gzip member boundary on every flush would fragment
    /// compression. The reasoning is sound; returning `Ok(())` was not. A
    /// caller who flushes and checks the result was told the bytes went
    /// somewhere when they were still sitting in `self.current`, and got no
    /// way to find out otherwise - the worst shape an error report can take.
    /// A `Write` implementation may make `flush` cheap; it may not make it a
    /// lie.
    ///
    /// Dispatching the buffered chunk to the pool was the first attempt at a
    /// fix and was still a lie, just a shorter one: `flush` returned before
    /// the compressor had run, before the writer thread had written, and
    /// therefore before any compression or destination-I/O error existed to
    /// report. "Handed to a queue" is not what `Write::flush` promises.
    ///
    /// So `flush` now installs an **acknowledgement barrier**. The buffered
    /// chunk is dispatched, then a `Barrier` job carrying a one-shot ack
    /// channel is dispatched behind it under the next sequence number. The
    /// barrier is not compressed - a worker forwards it untouched - and the
    /// writer thread's reorder buffer will not reach it until every
    /// lower-numbered chunk has been written. On reaching it the writer thread
    /// flushes the inner writer and sends back the result, including any
    /// deferred write error from an earlier chunk. `flush` blocks on that ack.
    ///
    /// Liveness: the producer holds no other channel capacity while it waits,
    /// the pool drains the bounded raw queue, and the writer thread drains the
    /// bounded compressed queue, so there is no cycle. Ordering is by seq, not
    /// by which worker happened to pick the barrier up. The no-cycle argument
    /// does not cover a worker *failure* - a worker that panics mid-chunk
    /// while its siblings keep the channel open would leave a permanent seq
    /// gap in front of the barrier - so a failed chunk fills its slot with an
    /// [`Out::Dropped`] placeholder and the barrier is answered with the
    /// first recorded failure instead of never being reached.
    ///
    /// `finish()` is still the call that closes the pipeline, joins every
    /// thread, and returns the inner writer, and it is what a caller who is
    /// done writing must use. `flush` is for a caller who needs the bytes so
    /// far to be out and any error so far to be visible, mid-stream.
    ///
    /// The fragmentation cost of the forced member boundary is real but is now
    /// the caller's to control by not flushing gratuitously, rather than being
    /// paid for with a silent data loss. In-tree the only consumer is
    /// `diff::derive`, whose `copy_section` flushes once per non-empty
    /// `create` / `modify` / `delete` section - at most three barriers and
    /// three extra gzip members per assembled OSC, against thousands of
    /// members, so the cost is noise. Those three flushes used to be no-ops
    /// that happened to be harmless because `quick_xml` writes straight
    /// through and holds no buffer of its own; they now do what their comment
    /// there always claimed.
    fn flush(&mut self) -> io::Result<()> {
        self.flush_current()?;
        let (ack_tx, ack_rx) = sync_channel::<io::Result<()>>(1);
        self.dispatch(Job::Barrier(ack_tx))?;
        #[cfg(feature = "test-hooks")]
        self.cancel.faults().on_gzip_barrier_queued();
        match ack_rx.recv() {
            Ok(reply) => reply,
            Err(_) => {
                Err(self.pool_error("writer thread exited before the flush barrier was reached"))
            }
        }
    }
}

impl<W: Write + Send + 'static> Drop for ParallelGzipWriter<W> {
    fn drop(&mut self) {
        #[cfg(test)]
        let finished = self.finished;
        // Best-effort cleanup still ships the final partial chunk and joins
        // the threads. Callers that need the final chunk's I/O result must
        // call `finish()` and route its result through `?`. An error bail-out
        // reaching here is expected (the primary error dominates). The
        // unit-test-only assertion below catches a forgotten `finish` in this
        // crate's own tests; a panic unwind is exempt.
        drop(self.flush_current());
        drop(self.shutdown());
        #[cfg(test)]
        debug_assert!(
            std::thread::panicking() || finished,
            "ParallelGzipWriter dropped before finish(); call finish() to observe deferred errors"
        );
    }
}

/// Worker: pull raw chunks, gzip them, send compressed chunks with the
/// original seq so the writer thread can reorder.
///
/// Lock scope is intentionally narrow: only `guard.recv()` runs while
/// the mutex is held, and `mpsc::Receiver::recv` returns `Err` on
/// close rather than panicking. `compress_one` and the downstream
/// `compressed_tx.send` both run after the guard is dropped, so a
/// worker panic in either path cannot poison this mutex and take
/// down the rest of the pool.
///
/// **Every dequeued seq is answered with exactly one `Out`.** The only code
/// between `recv` and `send` is [`compress_job`], which cannot unwind (it
/// runs the panicky part under `catch_unwind`); `recv`, `send` and the
/// poison check return rather than panic. Keep it that way: an unwind
/// between dequeue and send would lose the seq and hang any flush barrier
/// queued behind it, and the outer catch in `new` only records such a panic,
/// it cannot fill the gap.
fn worker_loop(
    raw_rx: &Mutex<Receiver<(usize, Job)>>,
    compressed_tx: &SyncSender<(usize, Out)>,
    failure: &FirstFailure,
    #[cfg(feature = "test-hooks")] faults: &crate::test_hooks::FaultPlan,
) {
    loop {
        let item = {
            // Hold the lock only for the recv call.
            let Ok(guard) = raw_rx.lock() else {
                failure.record(Error::internal(
                    "parallel gzip: the raw chunk queue lock was poisoned",
                ));
                return;
            };
            guard.recv()
        };
        let Ok((seq, job)) = item else {
            return; // channel closed, producer done
        };

        // A barrier carries no payload: forward it in place so the writer
        // thread sees it in seq order behind every earlier chunk. Compressing
        // it would be meaningless and dropping it would hang `flush`. Still
        // forwarded once the pool has stopped, for the same reason.
        let out = match job {
            Job::Barrier(ack) => Out::Barrier(ack),
            // On compression error, compress_job records it in FirstFailure
            // before returning the Dropped placeholder.
            Job::Compress(raw) => compress_job(
                seq,
                &raw,
                failure,
                #[cfg(feature = "test-hooks")]
                faults,
            ),
        };
        if compressed_tx.send((seq, out)).is_err() {
            return;
        }
    }
}

/// Compress one dequeued chunk into its `Out`, never unwinding.
///
/// Once the pool has stopped the chunk is skipped (`Out::Dropped`): its
/// bytes could never be written. A compress error or panic is recorded in
/// `failure` before the placeholder is returned, so the writer thread (and
/// a flush barrier behind it) sees the cause, not just the gap.
fn compress_job(
    seq: usize,
    raw: &[u8],
    failure: &FirstFailure,
    #[cfg(feature = "test-hooks")] faults: &crate::test_hooks::FaultPlan,
) -> Out {
    // One Acquire load per 2 MB chunk.
    if failure.stopped() {
        return Out::Dropped;
    }
    #[cfg(not(feature = "test-hooks"))]
    let _ = seq;
    let compressed = guarded(failure, "parallel gzip worker", || {
        // Test-only: the fault plan's `gzip_panic_at_op`. Simulates an OOM
        // or internal unwrap inside a compress worker. Release builds
        // compile this out entirely.
        #[cfg(feature = "test-hooks")]
        faults.on_gzip_compress(seq);
        compress_one(raw)
    });
    compressed.map_or(Out::Dropped, Out::Bytes)
}

/// Run `f` with its panics caught; record an error or a panic (payload kept,
/// attributed to `who`) in `failure` and return `None`, else `Some`.
/// `catch_unwind` costs nothing on the non-panicking path.
fn guarded<T>(failure: &FirstFailure, who: &str, f: impl FnOnce() -> io::Result<T>) -> Option<T> {
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => Some(v),
        Ok(Err(e)) => {
            failure.record(e);
            None
        }
        Err(payload) => {
            failure.record_panic(who, payload.as_ref());
            None
        }
    }
}

/// Compress one chunk as a standalone gzip member.
///
/// The `io::Result` return type is nominal: the inner writer is a
/// `Vec<u8>` whose `Write` impl is infallible, and flate2 does not
/// return `Err` for allocation failure (it panics/aborts). So the
/// `?` at `write_all` and the `Err` arm of `finish()` are effectively
/// unreachable today. Kept fallible as defensive coding in case the
/// sink is ever swapped for a fallible writer; callers (particularly
/// `worker_loop` above) must continue to handle `Err` as a real error.
fn compress_one(raw: &[u8]) -> io::Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::with_capacity(raw.len() / 2), compression_level());
    enc.write_all(raw)?;
    enc.finish()
}

/// Writer: pull compressed chunks in arbitrary order, reorder by seq,
/// write to inner in sequence. Returns the inner writer and the count of
/// seqs delivered in order on success (`finish` checks that count against
/// what the producer sent); on failure returns `None` with the failure
/// recorded in `failure`.
///
/// This is deliberately not `pipeline::deliver_in_order`, the PBF backends'
/// shared loop, though it uses the same `ReorderBuffer`. That loop carries
/// `OutputChunk`s, records the PBF writer counters (whose meaning is one
/// thing across the PBF backends), and returns on its first error; this one
/// carries flush barriers and failed-chunk placeholders, keeps its own
/// counters, and must keep draining after a failure so every barrier is
/// answered. The reorder window is the permit window, so insertion is
/// checked against a real bound (see `ParallelGzipWriter::new`).
///
/// A failure does **not** return early. Once `failure` has stopped the pool
/// the loop keeps draining, for one reason: a producer blocked on a flush
/// barrier further down the stream would otherwise wait on an ack channel
/// whose sender went out of scope with this function, and would learn only
/// that the thread "exited" rather than why. Draining lets every outstanding
/// barrier be answered with the first recorded failure. Subsequent chunks are
/// dropped rather than written, so a failure does not produce a file with a
/// hole in it.
///
/// Every inner-writer call runs under [`guarded`], so a panicking sink is
/// recorded on the spot - in particular before the barrier ack being
/// answered could be dropped by an unwind. Insertion is `try_push` for the
/// same reason: a stale, duplicate or out-of-window seq is a programming
/// error, but a panic there would drop held barrier acks (the rejected item
/// may itself be one) before the outer catch records anything, and a
/// producer in `flush` would wake to a generic "writer exited" instead of
/// the cause. It is recorded as the failure and the item discarded instead.
fn writer_loop<W: Write>(
    mut inner: W,
    compressed_rx: &Receiver<(usize, Out)>,
    failure: &FirstFailure,
    reorder_permit_tx: &SyncSender<()>,
    reorder_window: usize,
    metrics: &WriterMetrics,
) -> Option<(W, usize)> {
    const WHO: &str = "parallel gzip writer thread";
    let mut pending: ReorderBuffer<Out> =
        ReorderBuffer::with_max_window(reorder_window, reorder_window);
    while let Ok((seq, out)) = compressed_rx.recv() {
        // Once stopped, nothing is inserted again: unordered discards return
        // permits without advancing the buffer's head, so the window proof no
        // longer holds and ordered delivery is over anyway.
        if failure.stopped() {
            discard_pending(&mut pending, failure, reorder_permit_tx);
            discard_failed_out(out, failure, reorder_permit_tx);
            continue;
        }
        if let Err(rejected) = pending.try_push(seq, out) {
            failure.record(Error::internal(format!(
                "parallel gzip writer: {}",
                rejected.reason
            )));
            discard_failed_out(rejected.item, failure, reorder_permit_tx);
            discard_pending(&mut pending, failure, reorder_permit_tx);
            continue;
        }
        crate::counters::record_max(
            &metrics.parallel_gzip_reorder_high_water,
            pending.filled_len() as u64,
        );
        while let Some(item) = pending.pop_ready() {
            match item {
                // Its failure was recorded before it was sent.
                Out::Dropped => {}
                Out::Bytes(b) => {
                    if !failure.stopped() {
                        guarded(failure, WHO, || inner.write_all(&b));
                    }
                }
                Out::Barrier(ack) => {
                    if !failure.stopped() {
                        guarded(failure, WHO, || inner.flush());
                    }
                    // The ack carries a clone of the recorded error (they share
                    // one kind) while the original goes on to `finish()`.
                    let reply = match failure.restate() {
                        Some(e) => Err(e),
                        None => Ok(()),
                    };
                    drop(ack.send(reply));
                }
            }
            reorder_permit_tx.send(()).ok();
        }
    }
    if failure.stopped() {
        discard_pending(&mut pending, failure, reorder_permit_tx);
        return None;
    }
    // Producer closed. Nothing should be held if every seq arrived; an
    // interior gap means a worker exited between dequeue and send, which
    // `worker_loop` is structured never to do. A gap at the tail strands
    // nothing and is `finish`'s check, against the returned count.
    if pending.filled_len() > 0 {
        failure.record(Error::internal(format!(
            "parallel gzip writer: {} outputs stranded behind missing seq {}",
            pending.filled_len(),
            pending.next_seq()
        )));
        discard_pending(&mut pending, failure, reorder_permit_tx);
        return None;
    }
    guarded(failure, WHO, || inner.flush())?;
    Some((inner, pending.next_seq()))
}

/// [`discard_failed_out`] every output the reorder buffer holds, leaving it
/// empty.
fn discard_pending(
    pending: &mut ReorderBuffer<Out>,
    failure: &FirstFailure,
    reorder_permit_tx: &SyncSender<()>,
) {
    for out in pending.drain_filled() {
        discard_failed_out(out, failure, reorder_permit_tx);
    }
}

/// Discard an output after the stream has failed, answer any flush barrier
/// with the first error, and return its sequence permit. Once output is
/// failing, ordered file contents no longer matter, but every waiting producer
/// and barrier still needs to make progress.
///
/// The permit goes back with `try_send`. A refused output may be one the
/// pool should never have produced (a duplicate), which holds no permit of
/// its own, so a blocking return could find the channel full and park this
/// thread for good - `finish` hanging on the join, barriers further down
/// never answered. A full channel needs no replenishment once failure is on
/// record: the producer's permit wait polls the failure state.
fn discard_failed_out(out: Out, failure: &FirstFailure, reorder_permit_tx: &SyncSender<()>) {
    if let Out::Barrier(ack) = out {
        let reply = match failure.restate() {
            Some(e) => Err(e),
            None => Ok(()),
        };
        drop(ack.send(reply));
    }
    reorder_permit_tx.try_send(()).ok();
}

/// Worker-death fault tests, through the fault plan of the token the writer
/// is built under (`crate::test_hooks`).
#[cfg(all(test, feature = "test-hooks"))]
mod fault_tests {
    use super::*;
    use std::sync::atomic::Ordering::SeqCst;

    /// A compress-worker panic must surface through `finish()` as an
    /// `Err` carrying the panic's own message, not a silent truncated
    /// output and not a generic "worker panicked". Arms the hook at pool
    /// op #2 and writes enough bytes to dispatch several chunks so the
    /// panic lands mid-stream with other in-flight work still queued.
    ///
    /// `finish()` used to `?` out of its join loop on the first panicked
    /// worker with a fixed string, discarding the payload and ranking
    /// failures by join order; it now joins every thread and returns the
    /// first recorded failure.
    #[test]
    fn worker_panic_surfaces_via_finish() {
        let cancel = crate::host::Invocation::new();
        cancel.faults().gzip_panic_at_op.store(2, SeqCst);
        // 8 chunks of 1 KB each -> 8 dispatched pool ops across 2
        // workers. Panic on op #2 stops the pool: later chunks are
        // skipped, and writes after the stop fail fast (ignored here).
        let mut gz = ParallelGzipWriter::new(Vec::new(), 1024, 2, &cancel);

        // The catch_unwind is a guard: no panic may escape to the caller.
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            for _ in 0..8 {
                gz.write_all(&[0xAAu8; 1024]).ok();
            }
            gz.finish()
        }));
        match result {
            Ok(Ok(_)) => panic!(
                "ParallelGzipWriter::finish() must not silently succeed when a compress worker panicked"
            ),
            Ok(Err(e)) => {
                let msg = format!("{e}");
                assert!(
                    msg.contains("parallel gzip worker panicked")
                        && msg.contains("test-hooks: parallel_gzip pool op #2"),
                    "finish() must report the first failure with its panic payload, got: {msg}"
                );
            }
            Err(_) => panic!("a compress-worker panic must not unwind into the caller"),
        }
    }

    /// A compress-worker death must surface through `flush()` as an `Err` -
    /// and `flush()` must return at all.
    ///
    /// `finish()` joins the workers, so a panicked worker was always caught
    /// there. `flush()` never joins: it queues an acknowledgement barrier
    /// behind the dispatched chunks and blocks on the ack. Before a failed
    /// chunk's seq was filled with a placeholder (`Out::Dropped`), a worker
    /// that died mid-chunk while a sibling kept the channel open left a
    /// permanent seq gap in front of the barrier, the writer thread parked
    /// short of it forever, and this test hung until the harness watchdog
    /// killed it - which is exactly how the defect would have presented in
    /// production `diff --format osc` runs.
    ///
    /// Two workers, panic on pool op #1: the surviving worker keeps the
    /// compressed channel open, so the hang - not a clean channel-closed
    /// error - is the shape under test. The error must be the first
    /// failure, panic payload included, not a description of the gap.
    #[test]
    fn worker_death_surfaces_via_flush() {
        let cancel = crate::host::Invocation::new();
        let faults = cancel.faults();
        faults.gzip_panic_at_op.store(1, SeqCst);
        // The panicking worker holds off until flush's barrier is queued, so
        // the failure lands in front of an outstanding barrier: the barrier
        // can only be answered if the failed seq is filled. Without this the
        // panic could be recorded before flush starts, flush would fail fast
        // at dispatch, and the barrier path would go untested.
        faults.gzip_panic_waits_for_barrier.store(true, SeqCst);
        let mut gz = ParallelGzipWriter::new(Vec::new(), 1024, 2, &cancel);

        // Four chunks: the first dispatched op panics, later ones ride the
        // surviving worker so the channel stays open across the gap.
        let flush_result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            gz.write_all(&[0xAAu8; 4096]).ok();
            gz.flush()
        }));

        let timed_out = faults.gzip_hook_timed_out.load(SeqCst);
        let barriers = faults.gzip_barriers_queued.load(SeqCst);
        // Teardown first, so a failed assertion below cannot leave the pool
        // running: finish joins the same half-dead pool and reports the
        // recorded worker failure.
        let finished = gz.finish();

        assert!(
            !timed_out && barriers >= 1,
            "the panic must land behind a queued flush barrier \
             (timed out: {timed_out}, barriers queued: {barriers})"
        );
        match flush_result {
            Ok(Ok(())) => panic!(
                "ParallelGzipWriter::flush() must not report success when a \
                 compress worker died before producing its chunk"
            ),
            Ok(Err(e)) => {
                let msg = format!("{e}");
                assert!(
                    msg.contains("parallel gzip worker panicked")
                        && msg.contains("test-hooks: parallel_gzip pool op #1"),
                    "flush() must report the first failure with its panic payload, got: {msg}"
                );
            }
            Err(_) => panic!("a compress-worker panic must not unwind into the caller"),
        }
        assert!(
            finished.is_err(),
            "finish must report the recorded worker failure"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use flate2::read::MultiGzDecoder;
    use std::io::Read;

    /// Compress `bytes` via an owned-sink `ParallelGzipWriter`, return the
    /// gzip payload. The writer takes the sink by value (it must be
    /// `Send + 'static` for the worker thread) and `finish()` hands it
    /// back.
    fn roundtrip(bytes: &[u8], chunk_size: usize, workers: usize) -> Vec<u8> {
        let sink: Vec<u8> = Vec::new();
        let mut gz =
            ParallelGzipWriter::new(sink, chunk_size, workers, &crate::host::Invocation::new());
        gz.write_all(bytes).unwrap();
        let compressed = gz.finish().unwrap();
        let mut out = Vec::new();
        MultiGzDecoder::new(compressed.as_slice())
            .read_to_end(&mut out)
            .unwrap();
        out
    }

    #[test]
    fn empty_stream_produces_empty_output() {
        // No bytes in means no chunks shipped means no gzip members
        // written. That's stricter than zlib would produce, but the
        // one real consumer (`assemble_osc_from_paths`) always emits
        // at least the XML prologue + root element before any
        // parallel gzip member, so a truly empty stream is
        // unreachable in practice. Document the no-op behaviour here.
        let sink: Vec<u8> = Vec::new();
        let mut gz = ParallelGzipWriter::new(sink, 1024, 2, &crate::host::Invocation::new());
        gz.write_all(&[]).unwrap();
        let compressed = gz.finish().unwrap();
        assert!(
            compressed.is_empty(),
            "empty input should produce no output"
        );
    }

    #[test]
    fn small_stream_single_chunk() {
        let payload = b"hello world!".repeat(32);
        let out = roundtrip(&payload, 4096, 2);
        assert_eq!(out, payload);
    }

    #[test]
    fn zero_chunk_size_is_treated_as_one_byte() {
        let payload = b"zero chunk size";
        let out = roundtrip(payload, 0, 2);
        assert_eq!(out, payload);
    }

    #[test]
    fn stream_spans_multiple_chunks() {
        // Force many chunks: payload > 10 * chunk_size.
        #[allow(clippy::cast_possible_truncation)]
        let payload: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
        let out = roundtrip(&payload, 64 * 1024, 4);
        assert_eq!(out, payload);
    }

    #[test]
    fn concatenated_members_decode_as_one() {
        // Explicit multi-member check: write 5 distinct payloads, each
        // one chunk, and confirm the concatenated output is a single
        // logical stream.
        let sink: Vec<u8> = Vec::new();
        let mut gz = ParallelGzipWriter::new(sink, 1024, 2, &crate::host::Invocation::new());
        for i in 0..5u8 {
            let block = [i; 1024];
            gz.write_all(&block).unwrap();
        }
        let compressed = gz.finish().unwrap();
        let mut out = Vec::new();
        MultiGzDecoder::new(compressed.as_slice())
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out.len(), 5 * 1024);
        for (i, chunk) in out.chunks(1024).enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let want = i as u8;
            assert!(chunk.iter().all(|&b| b == want), "chunk {i} not all {want}");
        }
    }

    #[test]
    fn finish_returns_inner_writer() {
        let sink: Vec<u8> = Vec::new();
        let mut gz = ParallelGzipWriter::new(sink, 4096, 2, &crate::host::Invocation::new());
        gz.write_all(b"abc").unwrap();
        gz.write_all(b"def").unwrap();
        let bytes = gz.finish().unwrap();
        let mut out = Vec::new();
        MultiGzDecoder::new(bytes.as_slice())
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, b"abcdef");
    }

    #[test]
    fn write_at_exact_chunk_boundary() {
        // Writes that exactly fill chunk_size must ship the chunk and
        // leave `current` empty - not crash on the next write.
        let sink: Vec<u8> = Vec::new();
        let mut gz = ParallelGzipWriter::new(sink, 128, 2, &crate::host::Invocation::new());
        gz.write_all(&[7u8; 128]).unwrap();
        gz.write_all(&[9u8; 128]).unwrap();
        gz.write_all(&[11u8; 64]).unwrap();
        let bytes = gz.finish().unwrap();
        let mut out = Vec::new();
        MultiGzDecoder::new(bytes.as_slice())
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out.len(), 128 + 128 + 64);
        assert!(out[0..128].iter().all(|&b| b == 7));
        assert!(out[128..256].iter().all(|&b| b == 9));
        assert!(out[256..320].iter().all(|&b| b == 11));
    }

    /// Inner writer that records what actually reached it, how long each
    /// write took to land, and how many times it was flushed.
    ///
    /// The `delay` is the whole point: it makes "dispatched to the pool" and
    /// "written to the inner writer" separable in wall-clock terms, so a
    /// `flush` that only dispatches fails the assertions below rather than
    /// racing past them.
    #[derive(Clone)]
    struct ProbeSink {
        data: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        flushes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        delay: std::time::Duration,
        fail: bool,
    }

    impl ProbeSink {
        fn new(delay_ms: u64, fail: bool) -> Self {
            Self {
                data: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                flushes: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                delay: std::time::Duration::from_millis(delay_ms),
                fail,
            }
        }
    }

    impl Write for ProbeSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            std::thread::sleep(self.delay);
            if self.fail {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "probe sink: ENOSPC",
                ));
            }
            self.data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    /// `flush` must not return until the bytes are in the inner writer.
    ///
    /// The previous fix made `flush` dispatch the buffered chunk to the
    /// compressor pool and return, which passes any test that only checks
    /// "did flush return Ok". The sink here sleeps 40 ms per write, so at the
    /// moment a dispatch-only `flush` returns, nothing has been written yet
    /// and the recorded byte count is zero. Only a flush that waits for the
    /// writer thread sees a complete gzip stream.
    #[test]
    fn flush_waits_for_bytes_to_reach_the_inner_writer() {
        let sink = ProbeSink::new(40, false);
        let seen = std::sync::Arc::clone(&sink.data);
        let flushes = std::sync::Arc::clone(&sink.flushes);
        let mut gz = ParallelGzipWriter::new(sink, 1024, 4, &crate::host::Invocation::new());

        let payload = [42u8; 4096];
        gz.write_all(&payload).unwrap();
        gz.flush().unwrap();

        let recorded = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(
            !recorded.is_empty(),
            "flush returned before any byte reached the inner writer"
        );
        let mut out = Vec::new();
        MultiGzDecoder::new(recorded.as_slice())
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(
            out.len(),
            payload.len(),
            "flush must deliver every buffered byte, not a prefix"
        );
        assert!(
            flushes.load(std::sync::atomic::Ordering::Relaxed) >= 1,
            "flush must also flush the inner writer"
        );
        drop(gz.finish().unwrap());
    }

    /// `flush` must surface a destination-I/O error that happened downstream.
    ///
    /// This is the half a dispatch-only flush structurally cannot deliver:
    /// when `flush` returns before the writer thread has run, the error does
    /// not exist yet, so `Ok(())` is the only answer it can give.
    #[test]
    fn flush_propagates_a_downstream_write_error() {
        let sink = ProbeSink::new(0, true);
        let mut gz = ParallelGzipWriter::new(sink, 1024, 2, &crate::host::Invocation::new());
        gz.write_all(&[7u8; 4096]).unwrap();
        let err = gz.flush().expect_err("flush must report the sink's ENOSPC");
        assert_eq!(err.kind(), io::ErrorKind::StorageFull);
        assert!(
            err.to_string().contains("probe sink: ENOSPC"),
            "flush must report the underlying cause, got {err}"
        );
        assert!(
            gz.finish().is_err(),
            "finish must join the failed writer and preserve its error"
        );
    }

    /// A flush on an empty buffer is still a flush: it must reach the inner
    /// writer. Otherwise `flush_current`'s empty-buffer early return would
    /// quietly reintroduce the no-op for exactly the caller who flushes twice.
    #[test]
    fn flush_with_nothing_buffered_still_reaches_the_inner_writer() {
        let sink = ProbeSink::new(0, false);
        let flushes = std::sync::Arc::clone(&sink.flushes);
        let mut gz = ParallelGzipWriter::new(sink, 1024, 2, &crate::host::Invocation::new());
        gz.flush().unwrap();
        assert!(
            flushes.load(std::sync::atomic::Ordering::Relaxed) >= 1,
            "flush with an empty buffer must still flush the inner writer"
        );
        let _inner = gz.finish().unwrap();
    }

    /// Flushing mid-stream must not corrupt the stream: the barrier rides the
    /// same seq space as the chunks, so an off-by-one there would drop or
    /// reorder a member.
    #[test]
    fn interleaved_flushes_preserve_the_stream() {
        let sink: Vec<u8> = Vec::new();
        let mut gz = ParallelGzipWriter::new(sink, 512, 4, &crate::host::Invocation::new());
        let mut expected = Vec::new();
        for i in 0..8u8 {
            let block = [i; 700];
            gz.write_all(&block).unwrap();
            expected.extend_from_slice(&block);
            gz.flush().unwrap();
        }
        let compressed = gz.finish().unwrap();
        let mut out = Vec::new();
        MultiGzDecoder::new(compressed.as_slice())
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, expected);
    }

    /// A seq outside the permit window is a programming error, and the
    /// barrier carrying it must still be answered - with that cause, which
    /// is on record before the ack is sent - rather than dropped by a panic
    /// for the producer to wake to a generic "writer exited".
    #[test]
    fn an_out_of_window_barrier_is_answered_with_the_recorded_cause() {
        let failure = FirstFailure::new();
        // No spare permit capacity: a blocking permit return on the discard
        // path would park the writer thread here for good.
        let (permit_tx, _permit_rx) = sync_channel::<()>(1);
        let (tx, rx) = sync_channel::<(usize, Out)>(8);
        let (ack_tx, ack_rx) = sync_channel::<io::Result<()>>(1);
        tx.send((0, Out::Bytes(b"held".to_vec()))).unwrap();
        // After seq 0 pops the head is 1, so seq 3 is two past it: outside a
        // window of 2.
        tx.send((3, Out::Barrier(ack_tx))).unwrap();
        drop(tx);
        let out = writer_loop(
            Vec::new(),
            &rx,
            &failure,
            &permit_tx,
            2,
            &WriterMetrics::new(),
        );
        assert!(out.is_none(), "a window breach must fail the stream");
        let reply = ack_rx.recv().expect("the barrier must be answered");
        let msg = reply.expect_err("answered with the failure").to_string();
        assert!(msg.contains("window exceeded"), "got: {msg}");
    }

    /// The writer thread reports how many seqs it delivered in order,
    /// barriers included, for `finish` to check against what was sent.
    #[test]
    fn writer_loop_reports_its_delivered_count() {
        let failure = FirstFailure::new();
        let (permit_tx, _permit_rx) = sync_channel::<()>(8);
        let (tx, rx) = sync_channel::<(usize, Out)>(8);
        let (ack_tx, ack_rx) = sync_channel::<io::Result<()>>(1);
        tx.send((1, Out::Barrier(ack_tx))).unwrap();
        tx.send((0, Out::Bytes(b"ab".to_vec()))).unwrap();
        tx.send((2, Out::Bytes(b"cd".to_vec()))).unwrap();
        drop(tx);
        let metrics = WriterMetrics::new();
        let (inner, delivered) =
            writer_loop(Vec::new(), &rx, &failure, &permit_tx, 4, &metrics).unwrap();
        assert_eq!(inner, b"abcd");
        assert_eq!(delivered, 3);
        ack_rx.recv().unwrap().unwrap();
    }

    #[test]
    fn many_workers_preserve_order() {
        // Spray a specific byte pattern with many workers; order must
        // be preserved by the reorder buffer inside the writer thread.
        let mut payload = Vec::with_capacity(200_000);
        for i in 0..200_000u32 {
            payload.extend_from_slice(&i.to_le_bytes());
        }
        let out = roundtrip(&payload, 1024, 16);
        assert_eq!(out, payload);
    }
}
