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
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::thread::JoinHandle;

use flate2::write::GzEncoder;

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
    raw_tx: Option<SyncSender<(u64, Vec<u8>)>>,
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
        let (raw_tx, raw_rx) = sync_channel::<(u64, Vec<u8>)>(raw_cap);
        let (compressed_tx, compressed_rx) = sync_channel::<(u64, Vec<u8>)>(compressed_cap);

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

    /// Dispatch the current buffer as a raw chunk to the worker pool.
    fn flush_current(&mut self) -> io::Result<()> {
        if self.current.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::replace(&mut self.current, Vec::with_capacity(self.chunk_size));
        let seq = self.next_seq;
        self.next_seq += 1;
        let tx = self
            .raw_tx
            .as_ref()
            .ok_or_else(|| io::Error::other("ParallelGzipWriter: already finished"))?;
        tx.send((seq, chunk))
            .map_err(|_| io::Error::other("ParallelGzipWriter: worker pool dropped"))?;
        Ok(())
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

    /// `flush` on `Write` is a no-op: forcing a gzip member boundary on
    /// every buffered-writer flush would fragment compression badly.
    /// Call `finish()` to get the bytes out.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<W: Write + Send + 'static> Drop for ParallelGzipWriter<W> {
    fn drop(&mut self) {
        // Best-effort: ship the final partial chunk if the writer was
        // not explicitly finished. Errors are swallowed - callers that
        // care about durability must call `finish()`.
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
fn worker_loop(
    raw_rx: &std::sync::Mutex<Receiver<(u64, Vec<u8>)>>,
    compressed_tx: &SyncSender<(u64, Vec<u8>)>,
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
        let (seq, raw) = match item {
            Ok(v) => v,
            Err(_) => return, // channel closed, producer done
        };
        let compressed = match compress_one(&raw) {
            Ok(v) => v,
            Err(_) => return, // writer thread will see an incomplete stream and error
        };
        if compressed_tx.send((seq, compressed)).is_err() {
            return;
        }
    }
}

/// Compress one chunk as a standalone gzip member.
fn compress_one(raw: &[u8]) -> io::Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::with_capacity(raw.len() / 2), compression_level());
    enc.write_all(raw)?;
    enc.finish()
}

/// Writer: pull compressed chunks in arbitrary order, reorder by seq,
/// write to inner in sequence. Takes `compressed_rx` by value so the
/// receiver is dropped (and the channel released) on return.
#[allow(clippy::needless_pass_by_value)]
fn writer_loop<W: Write>(
    mut inner: W,
    compressed_rx: Receiver<(u64, Vec<u8>)>,
) -> io::Result<W> {
    let mut pending: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut expected: u64 = 0;
    while let Ok((seq, bytes)) = compressed_rx.recv() {
        pending.insert(seq, bytes);
        while let Some(b) = pending.remove(&expected) {
            inner.write_all(&b)?;
            expected += 1;
        }
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
        MultiGzDecoder::new(compressed.as_slice()).read_to_end(&mut out).unwrap();
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
        assert!(compressed.is_empty(), "empty input should produce no output");
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
        MultiGzDecoder::new(compressed.as_slice()).read_to_end(&mut out).unwrap();
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
        MultiGzDecoder::new(bytes.as_slice()).read_to_end(&mut out).unwrap();
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
        MultiGzDecoder::new(bytes.as_slice()).read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), 128 + 128 + 64);
        assert!(out[0..128].iter().all(|&b| b == 7));
        assert!(out[128..256].iter().all(|&b| b == 9));
        assert!(out[256..320].iter().all(|&b| b == 11));
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
