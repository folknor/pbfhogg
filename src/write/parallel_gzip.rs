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

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use flate2::write::GzEncoder;

use crate::write::pool_failure::FirstFailure;

// Fault-injection hooks for tests. Gated behind the `static-test-hooks`
// Cargo feature and `not(test)`: product builds and the library's own
// unit-test harness never compile this module.
// Same shape as `parallel_writer::test_hooks`: process-global
// atomics rather than per-instance config because pool workers are
// spawned deep inside `ParallelGzipWriter::new`. A test that arms them
// owns its own `[[test]]` binary (see reference/testing.md).
#[cfg(all(feature = "static-test-hooks", not(test)))]
pub mod test_hooks {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    /// Pool op count at which the acting worker panics. `u64::MAX` =
    /// disarmed (default). Call [`reset`] at the start AND end of
    /// any test that arms this so sibling tests don't inherit state.
    pub static PANIC_AT_POOL_OP_COUNT: AtomicU64 = AtomicU64::new(u64::MAX);

    /// Monotonic total of compress ops dispatched across the pool.
    /// Each worker `fetch_add(1)` immediately after dequeuing a chunk;
    /// the (post-increment) count is compared to
    /// `PANIC_AT_POOL_OP_COUNT`.
    pub static POOL_OP_COUNT: AtomicU64 = AtomicU64::new(0);

    /// When set, the worker that is about to panic first waits (bounded)
    /// until a flush barrier has been queued, so the failure provably lands
    /// in front of an outstanding barrier rather than before `flush` starts.
    pub static PANIC_WAITS_FOR_BARRIER: AtomicBool = AtomicBool::new(false);

    /// Flush barriers accepted into the raw queue.
    pub static BARRIERS_QUEUED: AtomicU64 = AtomicU64::new(0);

    /// Set if a bounded hook wait expired - the handshake did not happen,
    /// and a test relying on it proved nothing.
    pub static HOOK_TIMED_OUT: AtomicBool = AtomicBool::new(false);

    /// Disarm and zero the counters.
    pub fn reset() {
        PANIC_AT_POOL_OP_COUNT.store(u64::MAX, Ordering::SeqCst);
        POOL_OP_COUNT.store(0, Ordering::SeqCst);
        PANIC_WAITS_FOR_BARRIER.store(false, Ordering::SeqCst);
        BARRIERS_QUEUED.store(0, Ordering::SeqCst);
        HOOK_TIMED_OUT.store(false, Ordering::SeqCst);
    }

    /// Called by a worker after it dequeues a chunk, before compressing it.
    pub(super) fn on_compress(seq: u64) {
        let n = POOL_OP_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
        if n == PANIC_AT_POOL_OP_COUNT.load(Ordering::SeqCst) {
            if PANIC_WAITS_FOR_BARRIER.load(Ordering::SeqCst) {
                let deadline = Instant::now() + Duration::from_secs(10);
                while BARRIERS_QUEUED.load(Ordering::SeqCst) == 0 {
                    if Instant::now() >= deadline {
                        HOOK_TIMED_OUT.store(true, Ordering::SeqCst);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            panic!("test-hooks: parallel_gzip pool op #{n} (seq {seq}) panicking");
        }
    }

    /// Called by `flush` once its barrier is in the raw queue.
    pub(super) fn on_barrier_queued() {
        BARRIERS_QUEUED.fetch_add(1, Ordering::SeqCst);
    }
}

/// Default chunk size: 2 MB uncompressed per gzip member. Larger chunks
/// amortize the 18-byte per-member framing and let DEFLATE find more
/// cross-sequence matches; smaller chunks expose more parallelism at
/// the worker pool. 2 MB is the `pigz` default and matches reviewer
/// consensus for typical text-like workloads.
pub const DEFAULT_CHUNK_SIZE: usize = 2 * 1024 * 1024;

/// Bounded channel depth for both raw-in and compressed-out queues.
/// Sized at `2 * worker_count` so a full queue on either side
/// backpressures the producer thread naturally without starving the
/// pool.
const QUEUE_SLOTS_PER_WORKER: usize = 2;

/// Compression level used for every gzip member. `Compression::fast`
/// (DEFLATE level 1) matches what the serial `assemble_osc` path used
/// to hand to `flate2::GzEncoder`, so compressed size is directly
/// comparable in benches.
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
/// writer.
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
    next_seq: u64,
    #[cfg(test)]
    /// Whether `finish` has joined the workers and writer thread.
    finished: bool,
    raw_tx: Option<SyncSender<(u64, Job)>>,
    writer_handle: Option<JoinHandle<Option<W>>>,
    worker_handles: Vec<JoinHandle<()>>,
    failure: Arc<FirstFailure>,
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
    pub fn new(inner: W, chunk_size: usize, worker_count: usize) -> Self {
        let chunk_size = chunk_size.max(1);
        let worker_count = worker_count.max(1);
        let raw_cap = worker_count * QUEUE_SLOTS_PER_WORKER;
        let compressed_cap = worker_count * QUEUE_SLOTS_PER_WORKER;
        let (raw_tx, raw_rx) = sync_channel::<(u64, Job)>(raw_cap);
        let (compressed_tx, compressed_rx) = sync_channel::<(u64, Out)>(compressed_cap);
        let failure = Arc::new(FirstFailure::new());

        let raw_rx = Arc::new(Mutex::new(raw_rx));

        let mut worker_handles = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let raw_rx = Arc::clone(&raw_rx);
            let compressed_tx = compressed_tx.clone();
            let worker_failure = Arc::clone(&failure);
            let spawned = std::thread::Builder::new().spawn(move || {
                // Backstop only: `compress_job` already catches everything
                // between dequeue and send. Recorded before `compressed_tx`
                // drops, so the writer thread cannot see the disconnect first.
                let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    worker_loop(&raw_rx, &compressed_tx, &worker_failure);
                }));
                if let Err(payload) = caught {
                    worker_failure.record_panic("parallel gzip worker", payload.as_ref());
                }
                drop(compressed_tx);
            });
            match spawned {
                Ok(handle) => worker_handles.push(handle),
                Err(e) => {
                    failure.record(io::Error::new(
                        e.kind(),
                        format!("spawn parallel gzip worker {index}: {e}"),
                    ));
                    break;
                }
            }
        }
        // Drop the extra sender so the last worker closing their sender
        // actually closes the channel on the writer thread.
        drop(compressed_tx);

        let writer_failure = Arc::clone(&failure);
        let spawned = std::thread::Builder::new().spawn(move || {
            // Backstop as above: `writer_loop` runs every inner-writer call
            // under `guarded`. The receiver is owned out here so it is only
            // dropped after a panic has been recorded.
            let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
                writer_loop(inner, &compressed_rx, &writer_failure)
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
                failure.record(io::Error::new(
                    e.kind(),
                    format!("spawn parallel gzip writer thread: {e}"),
                ));
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
            writer_handle,
            worker_handles,
            failure,
        }
    }

    /// The recorded first failure, restated, or `fallback` if (impossibly)
    /// nothing is on record. For producer-side calls that must report now;
    /// `finish` surfaces the original.
    fn pool_error(&self, fallback: &str) -> io::Error {
        self.failure
            .restate()
            .unwrap_or_else(|| io::Error::other(format!("ParallelGzipWriter: {fallback}")))
    }

    /// Send one job to the pool under the next sequence number.
    ///
    /// Fails fast once the pool has stopped, so a producer stops feeding a
    /// pipeline that will discard everything it is given.
    fn dispatch(&mut self, job: Job) -> io::Result<()> {
        if self.failure.stopped() {
            return Err(self.pool_error("pool stopped"));
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        let tx = self
            .raw_tx
            .as_ref()
            .ok_or_else(|| io::Error::other("ParallelGzipWriter: already finished"))?;
        if tx.send((seq, job)).is_err() {
            return Err(self.pool_error("worker pool dropped"));
        }
        Ok(())
    }

    /// Close the raw channel and join every thread: workers first (they must
    /// finish before the writer sees a closed compressed channel), then the
    /// writer thread, whose inner writer is returned when it succeeded.
    /// Never returns early, so no thread outlives the call.
    fn shutdown(&mut self) -> Option<W> {
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
        inner
            .ok_or_else(|| io::Error::other("ParallelGzipWriter: writer thread returned no writer"))
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
        #[cfg(all(feature = "static-test-hooks", not(test)))]
        test_hooks::on_barrier_queued();
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
    raw_rx: &Mutex<Receiver<(u64, Job)>>,
    compressed_tx: &SyncSender<(u64, Out)>,
    failure: &FirstFailure,
) {
    loop {
        let item = {
            // Hold the lock only for the recv call.
            let Ok(guard) = raw_rx.lock() else {
                failure.record(io::Error::other(
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
            Job::Compress(raw) => compress_job(seq, &raw, failure),
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
fn compress_job(seq: u64, raw: &[u8], failure: &FirstFailure) -> Out {
    // One Acquire load per 2 MB chunk.
    if failure.stopped() {
        return Out::Dropped;
    }
    #[cfg(not(all(feature = "static-test-hooks", not(test))))]
    let _ = seq;
    let compressed = guarded(failure, "parallel gzip worker", || {
        // Test-only: arm via `test_hooks::PANIC_AT_POOL_OP_COUNT`.
        // Simulates an OOM or internal unwrap inside a compress
        // worker. Release builds compile this out entirely.
        #[cfg(all(feature = "static-test-hooks", not(test)))]
        test_hooks::on_compress(seq);
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
/// write to inner in sequence. Returns the inner writer on success; on
/// failure returns `None` with the failure recorded in `failure`.
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
/// answered could be dropped by an unwind.
fn writer_loop<W: Write>(
    mut inner: W,
    compressed_rx: &Receiver<(u64, Out)>,
    failure: &FirstFailure,
) -> Option<W> {
    const WHO: &str = "parallel gzip writer thread";
    let mut pending: BTreeMap<u64, Out> = BTreeMap::new();
    let mut expected: u64 = 0;
    while let Ok((seq, out)) = compressed_rx.recv() {
        pending.insert(seq, out);
        while let Some(item) = pending.remove(&expected) {
            expected += 1;
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
                    // `io::Error` is not `Clone`; the ack carries a faithful
                    // restatement (same `ErrorKind`, same message) while the
                    // original goes on to `finish()`.
                    let reply = match failure.restate() {
                        Some(e) => Err(e),
                        None => Ok(()),
                    };
                    drop(ack.send(reply));
                }
            }
        }
    }
    if failure.stopped() {
        return None;
    }
    // Producer closed. `pending` should be empty if every seq arrived;
    // a gap means a worker exited between dequeue and send, which
    // `worker_loop` is structured never to do.
    if !pending.is_empty() {
        failure.record(io::Error::other(format!(
            "parallel gzip writer: {} chunks missing at seq {expected}",
            pending.len()
        )));
        return None;
    }
    guarded(failure, WHO, || inner.flush())?;
    Some(inner)
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
        let mut gz = ParallelGzipWriter::new(sink, chunk_size, workers);
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
        let mut gz = ParallelGzipWriter::new(sink, 1024, 2);
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
        let mut gz = ParallelGzipWriter::new(sink, 1024, 2);
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
        let mut gz = ParallelGzipWriter::new(sink, 4096, 2);
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
        let mut gz = ParallelGzipWriter::new(sink, 128, 2);
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
        let mut gz = ParallelGzipWriter::new(sink, 1024, 4);

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
        let mut gz = ParallelGzipWriter::new(sink, 1024, 2);
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
        let mut gz = ParallelGzipWriter::new(sink, 1024, 2);
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
        let mut gz = ParallelGzipWriter::new(sink, 512, 4);
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
