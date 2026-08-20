//! Pipeline plumbing for [`PbfWriter`](super::writer::PbfWriter): ordered
//! output chunks, the sink trait, the bounded permit pool that caps in-flight
//! rayon dispatches, and the writer thread that reorders and drains results
//! to the underlying sink.
//!
//! All items here are internal wiring for the write path; nothing in this
//! module is part of the public library API.

use crate::reorder_buffer::ReorderBuffer;
use crate::write::file_writer::FileWriter;
use crate::write::metrics::WRITER_METRICS;
use std::io::{self, Write};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;

/// Maximum number of framed blobs in-flight before backpressure stalls senders.
pub(crate) const WRITE_AHEAD: usize = 32;

/// Maximum number of in-flight rayon dispatches in the pipelined writer.
///
/// Counting-semaphore cap that bounds how many uncompressed block `Vec<u8>`s
/// can be owned by rayon closures simultaneously (queued, being compressed,
/// or waiting on the bounded output channel). Without this, `rayon::spawn`'s
/// unbounded internal task queue grows without limit when the producer side
/// out-runs compression throughput - exactly what happened in commit
/// `e7219f0` on planet when parallel pass 1 / stage 2a / stage 2d started
/// emitting blocks faster than zlib:6 could drain them, killing pbfhogg via
/// OOM at 26 GB anon RSS.
///
/// Memory bound: `PIPELINE_DISPATCH_PERMITS × max_block_size`. At ~4 MB per
/// uncompressed block and 64 permits that's ~256 MB worst case, small
/// compared to the 2.79 GB stage-2b peak.
pub(crate) const PIPELINE_DISPATCH_PERMITS: usize = 64;

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
    /// Kernel-space copy from input fd (avoids userspace copy for passthrough).
    #[cfg(feature = "linux-direct-io")]
    CopyRange {
        in_fd: std::os::unix::io::RawFd,
        offset: u64,
        len: u64,
    },
}

/// The three owned parts of a framed blob:
/// `4 B big-endian header_len | BlobHeader | Blob protobuf body`.
///
/// Kept split so backends that support scatter-gather I/O can issue one
/// `writev` without an intermediate concat, while backends that need a
/// single buffer flatten locally via [`into_vec`](Self::into_vec).
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
/// `data` is `io::Result<OutputChunk>` because framing runs on rayon workers
/// and may fail asynchronously - the error is propagated in order via the
/// reorder buffer so the writer thread surfaces it at the correct position.
pub(crate) struct PipelineItem {
    pub(crate) seq: usize,
    pub(crate) data: io::Result<OutputChunk>,
}

/// A sink that consumes ordered [`OutputChunk`]s and writes them to a backend.
///
/// Each backend is free to flatten, batch, or scatter-gather as it sees fit -
/// the pipeline machinery only cares about ordering and error propagation.
pub(crate) trait OutputSink {
    fn write_chunk(&mut self, chunk: OutputChunk) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
}

/// Default sink that wraps a [`FileWriter`] (buffered or O_DIRECT).
///
/// Flattens `Framed` chunks via [`FramedBlobParts::into_vec`] before handing
/// them to the underlying writer.
pub(crate) struct FileOutputSink {
    writer: FileWriter,
}

impl FileOutputSink {
    pub(crate) fn new(writer: FileWriter) -> Self {
        Self { writer }
    }
}

impl OutputSink for FileOutputSink {
    fn write_chunk(&mut self, chunk: OutputChunk) -> io::Result<()> {
        match chunk {
            OutputChunk::Framed(parts) => {
                let len = parts.total_len();
                let bytes = parts.into_vec();
                let t_write = std::time::Instant::now();
                self.writer.write_all(&bytes)?;
                WRITER_METRICS
                    .write_ns
                    .fetch_add(elapsed_ns_u64(t_write), Relaxed);
                WRITER_METRICS.bytes_written.fetch_add(len, Relaxed);
                Ok(())
            }
            OutputChunk::Raw(bytes) => {
                let len = bytes.len() as u64;
                let t_write = std::time::Instant::now();
                self.writer.write_all(&bytes)?;
                WRITER_METRICS
                    .write_ns
                    .fetch_add(elapsed_ns_u64(t_write), Relaxed);
                WRITER_METRICS.bytes_written.fetch_add(len, Relaxed);
                Ok(())
            }
            OutputChunk::RawChunks(chunks) => {
                let total_bytes: u64 = chunks.iter().map(|chunk| chunk.len() as u64).sum();
                let t_write = std::time::Instant::now();
                for chunk in &chunks {
                    self.writer.write_all(chunk)?;
                }
                WRITER_METRICS
                    .write_ns
                    .fetch_add(elapsed_ns_u64(t_write), Relaxed);
                WRITER_METRICS.bytes_written.fetch_add(total_bytes, Relaxed);
                Ok(())
            }
            #[cfg(feature = "linux-direct-io")]
            OutputChunk::CopyRange { in_fd, offset, len } => {
                let t_write = std::time::Instant::now();
                let out_fd = self.writer.flush_and_raw_fd()?.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "copy_file_range incompatible with O_DIRECT output",
                    )
                })?;
                super::copy_range::copy_range(in_fd, out_fd, offset, len)?;
                WRITER_METRICS
                    .write_ns
                    .fetch_add(elapsed_ns_u64(t_write), Relaxed);
                WRITER_METRICS.bytes_written.fetch_add(len, Relaxed);
                Ok(())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// Write pipeline state - active when using pipelined mode.
pub(super) struct WritePipeline {
    pub(super) tx: SyncSender<PipelineItem>,
    pub(super) seq: usize,
    pub(super) join_handle: Option<JoinHandle<io::Result<()>>>,
    /// Counting-semaphore permit pool bounding in-flight rayon dispatches.
    /// Main thread `recv()`s a permit before `rayon::spawn`; the rayon
    /// closure `send()`s a permit back when its work completes. See
    /// [`PIPELINE_DISPATCH_PERMITS`] for the memory-bound rationale.
    pub(super) permit_tx: SyncSender<()>,
    pub(super) permit_rx: Receiver<()>,
    /// End-of-stream handshake shared with the writer thread. `flush`
    /// announces `seq` into it before dropping `tx`; see [`SeqTotal`].
    pub(super) expected: SeqTotal,
}

/// Build a fresh permit pool pre-filled with `PIPELINE_DISPATCH_PERMITS`
/// tokens. Called during pipelined-writer construction to seed the
/// counting semaphore.
pub(super) fn new_permit_pool() -> (SyncSender<()>, Receiver<()>) {
    let (permit_tx, permit_rx) = sync_channel::<()>(PIPELINE_DISPATCH_PERMITS);
    for _ in 0..PIPELINE_DISPATCH_PERMITS {
        // `send` on a fresh channel with capacity = N can't fail or block
        // for the first N sends. unwrap is sound here.
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
/// timeout and no diagnostic. Constructing the guard first in the closure
/// body also puts the release after the `tx.send` of the framed result, which
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
/// `pending_len` and `delivered` come from the backend's reorder buffer after
/// its receive loop breaks on channel close. Two distinct failures:
///
/// * `pending_len > 0` - an *interior* gap. Items arrived that could never
///   pop because an earlier seq never showed up.
/// * `delivered < total` - a *trailing* gap. Nothing is blocked because
///   nothing came after the missing seq; only the announced total reveals it.
///
/// Both mean the file on disk is short. Neither may be reported as success.
pub(crate) fn check_stream_complete(
    backend: &str,
    pending_len: usize,
    delivered: usize,
    expected: &SeqTotal,
) -> io::Result<()> {
    if pending_len > 0 {
        return Err(io::Error::other(format!(
            "{backend}: channel closed with {pending_len} item(s) still in reorder buffer; \
             an upstream framer dropped without sending an earlier seq"
        )));
    }
    if let Some(total) = expected.announced()
        && delivered < total
    {
        return Err(io::Error::other(format!(
            "{backend}: channel closed after {delivered} of {total} blob(s); \
             an upstream framer dropped without sending the final seq(s)"
        )));
    }
    Ok(())
}

pub(super) fn record_send_wait(send_start: std::time::Instant) {
    WRITER_METRICS
        .pipeline_send_wait_ns
        .fetch_add(elapsed_ns_u64(send_start), Relaxed);
}

pub(crate) fn elapsed_ns_u64(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// Writer thread: receives [`OutputChunk`]s and hands them to the sink in
/// sequence order.
///
/// Uses a shared sequence-number reorder buffer to handle out-of-order
/// arrivals from parallel rayon tasks. Framing errors produced on rayon
/// workers are propagated in order via `io::Result<OutputChunk>`.
#[allow(clippy::needless_pass_by_value)] // Thread entry point owns the sink moved into std::thread::spawn.
pub(super) fn writer_thread<S: OutputSink>(
    rx: std::sync::mpsc::Receiver<PipelineItem>,
    mut sink: S,
    expected: &SeqTotal,
) -> io::Result<()> {
    let mut pending: ReorderBuffer<io::Result<OutputChunk>> =
        ReorderBuffer::with_capacity(WRITE_AHEAD);

    loop {
        let t_recv = std::time::Instant::now();
        let item = match rx.recv() {
            Ok(item) => item,
            Err(_) => break,
        };
        WRITER_METRICS
            .recv_wait_ns
            .fetch_add(elapsed_ns_u64(t_recv), Relaxed);
        pending.push(item.seq, item.data);
        WRITER_METRICS.record_reorder_high_water(pending.pending_len());

        // Drain consecutive ready items from the front.
        while let Some(result) = pending.pop_ready() {
            let chunk = result?;
            sink.write_chunk(chunk)?;
        }
    }

    // Channel closed. Flushing unconditionally here would finalise a file
    // that is short by an arbitrary number of blobs and report it as a
    // successful write, so the stream has to be proved complete first - both
    // for an interior gap and for a missing tail. See
    // [`check_stream_complete`].
    //
    // On this error path the sink is NOT flushed, so an O_DIRECT sink is
    // finalised by `DirectWriter::Drop` calling `flush_final` best-effort -
    // the short file still gets padded and `set_len`'d. That is deliberate.
    // The padding is not what makes a truncated file look plausible: every
    // backend writes whole blobs, so a short file ends on a blob boundary and
    // reads as a valid shorter PBF regardless. Skipping the O_DIRECT
    // finalisation would only leave a partial page of alignment zeros on the
    // end, which is harder to diagnose, not easier. The thing that must not
    // be lost is the *report*, and that is the error returned here.
    check_stream_complete(
        "pipelined writer",
        pending.pending_len(),
        pending.next_seq(),
        expected,
    )?;

    sink.flush()?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{OutputChunk, OutputSink, PipelineItem, SeqTotal, WRITE_AHEAD, writer_thread};
    use std::io;
    use std::sync::mpsc::sync_channel;

    /// Records what actually reached the backend, and whether it was flushed.
    struct RecordingSink {
        written: Vec<Vec<u8>>,
        flushed: bool,
    }

    impl OutputSink for RecordingSink {
        fn write_chunk(&mut self, chunk: OutputChunk) -> io::Result<()> {
            match chunk {
                OutputChunk::Raw(bytes) => self.written.push(bytes),
                _ => panic!("test fixture only sends Raw chunks"),
            }
            Ok(())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushed = true;
            Ok(())
        }
    }

    /// Drive `writer_thread` over `seqs`, optionally announcing a final item
    /// count the way `PbfWriter::flush` does before dropping its sender.
    ///
    /// `announce: None` models the `Drop` path, which never announces.
    fn run(seqs: &[usize], announce: Option<usize>) -> (io::Result<()>, Vec<Vec<u8>>, bool) {
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
            flushed: false,
        };
        // `writer_thread` is a plain function - run it on this thread so the
        // sink can be inspected afterwards through the borrowed adapter.
        let result = writer_thread(rx, SinkHandle(&mut sink), &expected);
        (result, sink.written, sink.flushed)
    }

    /// Borrowed adapter so the test can inspect the sink after the loop
    /// returns, which `writer_thread`'s owned-sink signature otherwise
    /// forbids.
    struct SinkHandle<'a>(&'a mut RecordingSink);

    impl OutputSink for SinkHandle<'_> {
        fn write_chunk(&mut self, chunk: OutputChunk) -> io::Result<()> {
            self.0.write_chunk(chunk)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
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
        let (result, written, flushed) = run(&[0, 2, 3], Some(4));
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
        assert!(
            !flushed,
            "a truncated write must not be finalised by a flush that makes it \
             look complete"
        );
    }

    /// The interior guard must stand on its own, with no announced total.
    ///
    /// `Drop` never announces, so on that path `pending_len` is the only
    /// evidence available and it still has to be read.
    #[test]
    fn an_interior_gap_is_caught_even_when_no_total_was_announced() {
        let (result, _written, flushed) = run(&[0, 2, 3], None);
        let msg = result
            .expect_err("an interior gap is visible without any announced total")
            .to_string();
        assert!(
            msg.contains("still in reorder buffer"),
            "expected the stranded-items diagnosis, got: {msg}"
        );
        assert!(!flushed, "a truncated write must not be flushed");
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
        let (result, written, flushed) = run(&[0], Some(2));
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
        assert!(
            !flushed,
            "a short file must not be finalised by a flush that makes it look \
             complete"
        );
    }

    /// The degenerate tail case: the only task dies, so nothing arrives at
    /// all and the writer sees an immediately-closed, entirely empty channel.
    ///
    /// Indistinguishable from a legitimate header-only write except by the
    /// announced total.
    #[test]
    fn a_single_dropped_task_fails_rather_than_writing_a_header_only_file() {
        let (result, written, flushed) = run(&[], Some(1));
        let msg = result
            .expect_err("the one and only blob never arrived")
            .to_string();
        assert!(
            msg.contains("after 0 of 1"),
            "expected the count mismatch, got: {msg}"
        );
        assert!(written.is_empty());
        assert!(!flushed);
    }

    /// The guard must fire on a gap, not on every clean run.
    #[test]
    fn a_complete_sequence_still_writes_and_flushes() {
        let (result, written, flushed) = run(&[2, 0, 1], Some(3));
        result.unwrap();
        assert_eq!(written, vec![vec![0u8], vec![1u8], vec![2u8]]);
        assert!(flushed, "a complete run must still flush the sink");
    }

    /// Passing counterpart to `a_dropped_final_task_fails...`: the same two
    /// item shape with the tail actually present must succeed. Without this,
    /// a regression that failed every run would still pass the gap tests.
    #[test]
    fn a_complete_two_item_run_with_its_tail_present_succeeds() {
        let (result, written, flushed) = run(&[0, 1], Some(2));
        result.unwrap();
        assert_eq!(written, vec![vec![0u8], vec![1u8]]);
        assert!(flushed);
    }

    /// Passing counterpart to the degenerate case: a writer that legitimately
    /// emits no data blobs at all (header-only output) must still succeed.
    #[test]
    fn an_announced_empty_stream_succeeds() {
        let (result, written, flushed) = run(&[], Some(0));
        result.unwrap();
        assert!(written.is_empty());
        assert!(
            flushed,
            "a header-only file is complete and must be flushed"
        );
    }
}
