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

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;

use flate2::write::GzEncoder;

// Fault-injection hooks for tests. Gated behind the `test-hooks`
// Cargo feature; release builds don't compile this module at all.
// Same shape as `parallel_writer::test_hooks`: process-global
// atomics rather than per-instance config because pool workers are
// spawned deep inside `ParallelGzipWriter::new`. Tests serialize
// access via `--test-threads=1`.
#[cfg(feature = "test-hooks")]
pub mod test_hooks {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Pool op count at which the acting worker panics. `u64::MAX` =
    /// disarmed (default). Call [`reset`] at the start AND end of
    /// any test that arms this so sibling tests don't inherit state.
    pub static PANIC_AT_POOL_OP_COUNT: AtomicU64 = AtomicU64::new(u64::MAX);

    /// Monotonic total of compress ops dispatched across the pool.
    /// Each worker `fetch_add(1)` immediately after dequeuing a chunk;
    /// the (post-increment) count is compared to
    /// `PANIC_AT_POOL_OP_COUNT`.
    pub static POOL_OP_COUNT: AtomicU64 = AtomicU64::new(0);

    /// Disarm and zero the counters.
    pub fn reset() {
        PANIC_AT_POOL_OP_COUNT.store(u64::MAX, Ordering::Relaxed);
        POOL_OP_COUNT.store(0, Ordering::Relaxed);
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
}

/// Writer that buffers bytes, dispatches chunks to a worker pool for
/// gzip compression, and emits compressed chunks in order to an inner
/// writer.
///
/// Call [`finish`](Self::finish) to flush the final partial chunk,
/// join the workers + writer thread, and recover the inner writer
/// along with any deferred I/O error. Dropping without `finish` does
/// a best-effort flush (errors can't propagate through `Drop`).
pub struct ParallelGzipWriter<W: Write + Send + 'static> {
    chunk_size: usize,
    current: Vec<u8>,
    next_seq: u64,
    raw_tx: Option<SyncSender<(u64, Job)>>,
    writer_handle: Option<JoinHandle<io::Result<W>>>,
    worker_handles: Vec<JoinHandle<()>>,
}

impl<W: Write + Send + 'static> ParallelGzipWriter<W> {
    /// Spawn the worker pool and writer thread, returning a handle that
    /// implements [`Write`]. `worker_count` is the number of parallel
    /// compressor threads; pick `std::thread::available_parallelism()`
    /// (or a value derived from it) for a CPU-bound workload. Falls
    /// back to 1 if `worker_count == 0`.
    pub fn new(inner: W, chunk_size: usize, worker_count: usize) -> Self {
        let worker_count = worker_count.max(1);
        let raw_cap = worker_count * QUEUE_SLOTS_PER_WORKER;
        let compressed_cap = worker_count * QUEUE_SLOTS_PER_WORKER;
        let (raw_tx, raw_rx) = sync_channel::<(u64, Job)>(raw_cap);
        let (compressed_tx, compressed_rx) = sync_channel::<(u64, Out)>(compressed_cap);

        let raw_rx = std::sync::Arc::new(std::sync::Mutex::new(raw_rx));

        let mut worker_handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let raw_rx = std::sync::Arc::clone(&raw_rx);
            let compressed_tx = compressed_tx.clone();
            worker_handles.push(std::thread::spawn(move || {
                worker_loop(&raw_rx, &compressed_tx);
            }));
        }
        // Drop the extra sender so the last worker closing their sender
        // actually closes the channel on the writer thread.
        drop(compressed_tx);

        let writer_handle = std::thread::spawn(move || writer_loop(inner, compressed_rx));

        Self {
            chunk_size,
            current: Vec::with_capacity(chunk_size),
            next_seq: 0,
            raw_tx: Some(raw_tx),
            writer_handle: Some(writer_handle),
            worker_handles,
        }
    }

    /// Send one job to the pool under the next sequence number.
    fn dispatch(&mut self, job: Job) -> io::Result<()> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let tx = self
            .raw_tx
            .as_ref()
            .ok_or_else(|| io::Error::other("ParallelGzipWriter: already finished"))?;
        tx.send((seq, job))
            .map_err(|_| io::Error::other("ParallelGzipWriter: worker pool dropped"))?;
        Ok(())
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
    pub fn finish(mut self) -> io::Result<W> {
        // Ship the final partial chunk.
        self.flush_current()?;
        // Close the raw channel so workers exit once drained.
        self.raw_tx = None;
        // Join workers first (they must finish before the writer sees a
        // closed compressed channel).
        for h in std::mem::take(&mut self.worker_handles) {
            h.join()
                .map_err(|_| io::Error::other("parallel gzip worker panicked"))?;
        }
        // Writer thread sees closed channel, drains remaining items,
        // returns inner writer with any I/O error.
        let handle = self
            .writer_handle
            .take()
            .ok_or_else(|| io::Error::other("ParallelGzipWriter: writer handle missing"))?;
        handle
            .join()
            .map_err(|_| io::Error::other("parallel gzip writer thread panicked"))?
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
    /// by which worker happened to pick the barrier up.
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
        ack_rx.recv().map_err(|_| {
            io::Error::other(
                "ParallelGzipWriter: writer thread exited before the flush barrier was reached",
            )
        })?
    }
}

impl<W: Write + Send + 'static> Drop for ParallelGzipWriter<W> {
    fn drop(&mut self) {
        // Best-effort: ship the final partial chunk if the writer was
        // not explicitly finished. Errors are swallowed - any caller
        // whose last writes matter (file sinks, network sinks) MUST
        // call `finish()` on the success path to route the final
        // chunk's I/O result and the writer thread's join result
        // through `?`. This Drop only fires reliably for:
        //   - Error bail-outs that propagate an earlier error via `?`
        //     and drop the writer unfinished (primary error dominates;
        //     missing final chunk is moot because the operation failed).
        //   - Panics.
        //   - Tests and throwaway code where durability doesn't matter.
        drop(self.flush_current());
        self.raw_tx = None;
        for h in std::mem::take(&mut self.worker_handles) {
            drop(h.join());
        }
        if let Some(h) = self.writer_handle.take() {
            drop(h.join());
        }
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
fn worker_loop(
    raw_rx: &std::sync::Mutex<Receiver<(u64, Job)>>,
    compressed_tx: &SyncSender<(u64, Out)>,
) {
    loop {
        let item = {
            // Hold the lock only for the recv call.
            let guard = match raw_rx.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            guard.recv()
        };
        let (seq, job) = match item {
            Ok(v) => v,
            Err(_) => return, // channel closed, producer done
        };

        // A barrier carries no payload: forward it in place so the writer
        // thread sees it in seq order behind every earlier chunk. Compressing
        // it would be meaningless and dropping it would hang `flush`.
        let raw = match job {
            Job::Compress(raw) => raw,
            Job::Barrier(ack) => {
                if compressed_tx.send((seq, Out::Barrier(ack))).is_err() {
                    return;
                }
                continue;
            }
        };

        // Test-only: arm via `test_hooks::PANIC_AT_POOL_OP_COUNT`.
        // Simulates an OOM or internal unwrap inside a compress
        // worker. Release builds compile this block out entirely.
        #[cfg(feature = "test-hooks")]
        {
            use std::sync::atomic::Ordering;
            let n = test_hooks::POOL_OP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            if n == test_hooks::PANIC_AT_POOL_OP_COUNT.load(Ordering::Relaxed) {
                panic!("test-hooks: parallel_gzip pool op #{n} (seq {seq}) panicking");
            }
        }

        let compressed = match compress_one(&raw) {
            Ok(v) => v,
            Err(_) => return, // writer thread will see an incomplete stream and error
        };
        if compressed_tx.send((seq, Out::Bytes(compressed))).is_err() {
            return;
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
/// write to inner in sequence. Takes `compressed_rx` by value so the
/// receiver is dropped (and the channel released) on return.
///
/// A write error does **not** return early. It is recorded in `fatal` and the
/// loop keeps draining, for one reason: a producer blocked on a flush barrier
/// further down the stream would otherwise wait on an ack channel whose sender
/// went out of scope with this function, and would learn only that the thread
/// "exited" rather than why. Draining lets every outstanding barrier be
/// answered with the real error. The error is still returned at the end, so
/// `finish()` sees it too. Subsequent chunks are dropped rather than written,
/// so a fatal error does not produce a file with a hole in it.
#[allow(clippy::needless_pass_by_value)]
fn writer_loop<W: Write>(mut inner: W, compressed_rx: Receiver<(u64, Out)>) -> io::Result<W> {
    let mut pending: BTreeMap<u64, Out> = BTreeMap::new();
    let mut expected: u64 = 0;
    let mut fatal: Option<io::Error> = None;
    while let Ok((seq, out)) = compressed_rx.recv() {
        pending.insert(seq, out);
        while let Some(item) = pending.remove(&expected) {
            expected += 1;
            match item {
                Out::Bytes(b) => {
                    if fatal.is_none()
                        && let Err(e) = inner.write_all(&b)
                    {
                        fatal = Some(e);
                    }
                }
                Out::Barrier(ack) => {
                    if fatal.is_none()
                        && let Err(e) = inner.flush()
                    {
                        fatal = Some(e);
                    }
                    // `io::Error` is not `Clone`; the ack carries a faithful
                    // restatement (same `ErrorKind`, same message) while the
                    // original goes on to `finish()`.
                    let reply = fatal.as_ref().map_or(Ok(()), |e| {
                        Err(io::Error::new(
                            e.kind(),
                            format!("parallel gzip writer: {e}"),
                        ))
                    });
                    drop(ack.send(reply));
                }
            }
        }
    }
    if let Some(e) = fatal {
        return Err(e);
    }
    // Producer closed. `pending` should be empty if every seq arrived;
    // if there is a gap, workers died mid-stream.
    if !pending.is_empty() {
        return Err(io::Error::other(format!(
            "parallel gzip writer: {} chunks missing at seq {expected}",
            pending.len()
        )));
    }
    inner.flush()?;
    Ok(inner)
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
