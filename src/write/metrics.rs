use std::sync::atomic::{AtomicBool, AtomicI32, Ordering::Relaxed};

use crate::counters::Stopwatch;

crate::counters::counter_set! {
    pub(crate) struct WriterMetrics {
        pub permit_wait_ns => "writer_permit_wait_ns",
        /// `frame_ns` and `compress_ns` are summed across whichever threads
        /// framed - the executor's workers on a pipelined writer, the caller
        /// on a synchronous one - so they are not components of any one
        /// thread's wall time.
        pub frame_ns => "writer_frame_ns",
        pub compress_ns => "writer_compress_ns",
        pub pipeline_send_wait_ns => "writer_pipeline_send_wait_ns",
        pub recv_wait_ns => "writer_recv_wait_ns",
        pub reorder_high_water => "writer_reorder_high_water",
        /// Time blocked waiting for a sequence permit in `ParallelGzipWriter`.
        pub parallel_gzip_reorder_wait_ns => "writer_parallel_gzip_reorder_wait_ns",
        /// Number of `ParallelGzipWriter` dispatches that waited for a sequence permit.
        pub parallel_gzip_reorder_blocked => "writer_parallel_gzip_reorder_blocked",
        /// Peak filled slots in `ParallelGzipWriter`'s reorder buffer, in
        /// gzip chunks and flush barriers. Kept
        /// apart from `reorder_high_water`, which counts PBF blobs in the
        /// writer backends' reorder buffers.
        pub parallel_gzip_reorder_high_water => "writer_parallel_gzip_reorder_high_water",
        /// Elapsed time of ordered output handoff, on every backend: from a
        /// ready chunk (or the header frame) being handed to the backend's
        /// write path until that call returns, on the thread doing it - the
        /// pipelined writer thread, the constructor for a pipelined header,
        /// or the caller of a synchronous writer. Includes flattening a
        /// framed chunk, memcpy into the backend's buffers, syscalls,
        /// submission, backpressure waits (a full `BufWriter` flushing,
        /// io_uring waiting for a free registered buffer, the `pwrite` pool's
        /// round-robin send blocking on one worker's full queue, which is
        /// head-of-line blocking rather than pool saturation), input reads
        /// and userspace copy fallback when that thread performs a
        /// `CopyRange`, and dropping the consumed payload. Recorded for
        /// operations that return an error too. Framing is `writer_frame_ns`,
        /// and an upstream framing failure is not a handoff.
        ///
        /// Not a partition of the writer's lifetime: io_uring's opportunistic
        /// non-blocking CQE reap between chunks (steady-state backend
        /// maintenance) is outside it, and so is end-of-stream finalization
        /// (the last buffer flush, io_uring drain, `O_DIRECT` tail padding and
        /// truncation, joining the pool, the fsync in `writer_sync_all_ns`).
        /// A small output can fit entirely in backend buffering and spend its
        /// time in finalization instead. Summed across writers, so a process
        /// with several concurrent writers (`multi-extract`) can exceed wall.
        ///
        /// Replaces `writer_write_ns`, which meant writer-thread wall on the
        /// buffered and io_uring backends but summed pool-thread time on the
        /// `pwrite` pool; the pool time is now `writer_pool_write_ns`. The
        /// old name was retired, not redefined, so a name-keyed comparison
        /// against stored runs finds no counterpart instead of a false one.
        pub handoff_ns => "writer_handoff_ns",
        /// `pwrite` / `copy_file_range` execution time summed across the
        /// `pwrite` pool's threads, including dropping the written buffer,
        /// failed operations too. Thread-time: up to the pool size times wall.
        /// Zero on every other backend, which has no pool.
        pub pool_write_ns => "writer_pool_write_ns",
        pub flush_ns => "writer_flush_ns",
        pub bytes_framed => "writer_bytes_framed",
        /// Bytes covered by successful handoff operations, header frame
        /// included, counted per constituent operation (each chunk of a
        /// `RawChunks`, each of a synchronous frame's three writes, a whole
        /// `CopyRange` on success). "Handed to the backend", not "on disk":
        /// the buffered backend counts bytes copied into its `BufWriter`,
        /// io_uring bytes copied into a registered buffer the kernel writes
        /// later, and the `pwrite` pool bytes queued to a worker - so the pool
        /// counting at dispatch is the same boundary as the others, not an
        /// earlier one. Bytes partially accepted inside a failing operation
        /// are not counted, and on a failed run accepted bytes may never reach
        /// the file. On success, for fresh regular-file outputs, it equals the
        /// sum of the output files' sizes (alignment padding never counts).
        pub bytes_written => "writer_bytes_written",

        /// The `payload_*` pairs count what callers submitted, on synchronous
        /// and pipelined writers alike: `OSMData` blobs once framing succeeds
        /// (the header frame is not a payload), raw items and copy ranges as
        /// they are submitted, before the handoff that may then fail.
        pub payload_framed_items => "writer_payload_framed_items",
        pub payload_framed_bytes => "writer_payload_framed_bytes",
        /// The subset of `payload_framed_items` a pipelined writer framed on
        /// a worker of its own executor - for `Executor::Pool`, a thread of
        /// that very pool; for `Ambient`, a thread of any rayon pool -
        /// checked on the thread that ran the framing. A synchronous writer
        /// frames on its caller and never counts here, so equality with
        /// `payload_framed_items` is the observable proof that every payload
        /// was framed on the caller's executor.
        pub payload_framed_on_executor_items => "writer_payload_framed_on_executor_items",
        pub payload_raw_items => "writer_payload_raw_items",
        pub payload_raw_bytes => "writer_payload_raw_bytes",
        pub payload_raw_chunk_items => "writer_payload_raw_chunk_items",
        pub payload_raw_chunk_bytes => "writer_payload_raw_chunk_bytes",
        pub payload_copy_range_items => "writer_payload_copy_range_items",
        pub payload_copy_range_bytes => "writer_payload_copy_range_bytes",
        /// Passthrough ranges the kernel refused to `copy_file_range` (cross
        /// filesystem, unsupported filesystem or kernel, seccomp, non-regular
        /// output; see `copy_range::kernel_copy_refused`) and that were routed
        /// to the userspace copy instead. Non-zero on a planet run is a
        /// performance cliff worth knowing about, not a failure; the CLI
        /// reports it as a note (`report_writer_degradations`).
        pub copy_range_fallback_ranges => "writer_copy_range_fallback_ranges",
        /// Bytes those ranges routed to the userspace fallback, counted when the
        /// range is routed (a fallback that then fails is still counted).
        pub copy_range_fallback_bytes => "writer_copy_range_fallback_bytes",

        /// `buffered_write_*` and `direct_write_*` count successful
        /// `FileWriter::write` calls and the logical bytes they accepted - not
        /// syscalls, not physical bytes (`CopyRange`, internal buffer flushes
        /// and `O_DIRECT` padding bypass them). The synchronous writers over a
        /// plain `BufWriter<File>` or an arbitrary `Write` never go through
        /// `FileWriter`, so zero here does not mean no buffered output.
        pub buffered_write_calls => "writer_buffered_write_calls",
        pub buffered_write_bytes => "writer_buffered_write_bytes",
        pub direct_write_calls => "writer_direct_write_calls",
        pub direct_write_bytes => "writer_direct_write_bytes",
        pub sync_all_ns => "writer_sync_all_ns",
        /// Number of `fsync` calls issued. Separate from `sync_all_ns` because
        /// "did anything fsync at all" is a correctness question - `Write::flush`
        /// must not - while the nanosecond total is a performance one.
        ///
        /// Every fsync goes through [`WriterMetrics::sync_recorded`]; there is
        /// no correct way to bump `sync_all_ns` alone. The pair used to be
        /// maintained by hand at each site and the parallel and io_uring
        /// backends bumped only the nanoseconds, so `writer_sync_all_calls`
        /// reported zero fsyncs for two of the three backends - a benchmark
        /// counter that silently disagreed with its own documentation.
        /// Failed calls are counted too.
        pub sync_all_calls => "writer_sync_all_calls",

        pub uring_submit_calls => "writer_uring_submit_calls",
        pub uring_submit_ns => "writer_uring_submit_ns",
        pub uring_submit_and_wait_calls => "writer_uring_submit_and_wait_calls",
        pub uring_submit_and_wait_ns => "writer_uring_submit_and_wait_ns",

        /// Blobs whose `tagdata` was dropped because the encoded `BlobHeader`
        /// would otherwise have hit the 64 KiB format cap. Non-zero means the
        /// output is correct but some blobs lost their tag skip-index; see
        /// `encode_blob_header_into`.
        pub tagdata_dropped_oversize => "writer_tagdata_dropped_oversize",

        /// Scratch buffers whose capacity exceeded the 4 MiB retention limit and
        /// were handed off or dropped instead of retained by a worker thread.
        pub frame_scratch_capped_buffers => "writer_frame_scratch_capped_buffers",
        /// Sum of the full capacities of buffers capped from thread-local scratch.
        pub frame_scratch_capped_capacity_bytes => "writer_frame_scratch_capped_capacity_bytes",

        /// Time a producer on a worker of the writer's executor pool spent in the
        /// cooperative permit wait (`pipeline::recv_permit`). Zero on every CLI
        /// path, which drives its writers off-worker; non-zero means a
        /// `PbfWriter` is being driven from a rayon worker.
        pub coop_wait_ns => "writer_coop_wait_ns",
    }
    extra {
        /// Latch for [`WriterMetrics::emit_once`], so an invocation emits the
        /// counter set exactly once no matter how many writers it flushes.
        emitted: AtomicBool = AtomicBool::new(false),
        /// The errno of the first `copy_file_range` refusal that routed a
        /// range to the userspace fallback (0 = none yet). Kept for the
        /// run's note (`report_writer_degradations`), not emitted as a
        /// counter: a later refusal may carry a different errno, and only
        /// the first is claimed. Only a kernel refusal on Linux records it.
        pub copy_range_fallback_errno: AtomicI32 = AtomicI32::new(0),
    }
}

impl WriterMetrics {
    /// Record one `fsync` and how long it took: the `sync_all_ns` /
    /// `sync_all_calls` pair, which used to be bumped by hand at each site
    /// and drifted apart. Only [`sync_recorded`](Self::sync_recorded) calls
    /// it.
    fn record_sync_all(&self, elapsed_ns: u64) {
        self.sync_all_ns.fetch_add(elapsed_ns, Relaxed);
        self.sync_all_calls.fetch_add(1, Relaxed);
    }

    /// `fsync` `file` and record it, success or failure.
    ///
    /// The crate's only `fsync` of an output: every backend's durable finish
    /// (buffered and `O_DIRECT` through `FileWriter::sync`, the `pwrite`
    /// pool, io_uring, `to_path_sync`'s finalizer) and publication's syncs of
    /// staged files and directories call it, so a call is counted at the
    /// syscall rather than trusted to its caller, and a failed `fsync` is
    /// counted like a successful one.
    pub(crate) fn sync_recorded(&self, file: &std::fs::File) -> std::io::Result<()> {
        self.sync_timed(file).0
    }

    /// [`sync_recorded`](Self::sync_recorded), also returning the elapsed
    /// nanoseconds it recorded, so a caller that keeps a tally of its own
    /// (publication's `publish_sync_*`) adds exactly the same sample.
    pub(crate) fn sync_timed(&self, file: &std::fs::File) -> (std::io::Result<()>, u64) {
        let start = Stopwatch::start();
        // The funnel itself.
        #[allow(clippy::disallowed_methods)]
        let result = file.sync_all();
        let elapsed_ns = start.elapsed_ns();
        self.record_sync_all(elapsed_ns);
        (result, elapsed_ns)
    }

    /// Record time spent blocked in a writer-thread `recv`.
    ///
    /// Feeds `brokkr sidecar --stalls` via the `writer_recv_wait_ns`
    /// convention. Called by the one ordered-delivery loop every pipelined
    /// backend runs (`pipeline::deliver_in_order`); a backend with a loop of
    /// its own would report as never having stalled on its input.
    ///
    /// Recorded after a successful `recv`, so the final wait that ends in
    /// the channel's disconnect is never counted, on any backend.
    pub fn record_recv_wait(&self, start: Stopwatch) {
        start.record_ns(&self.recv_wait_ns);
    }

    /// Run one handoff operation and record its elapsed time into
    /// `writer_handoff_ns` whatever it returns (see that counter). The one
    /// way every backend times a handoff, so an early `?` cannot skip the
    /// recording on one backend and not another.
    pub(crate) fn time_handoff<R>(&self, op: impl FnOnce() -> R) -> R {
        let start = Stopwatch::start();
        let result = op();
        start.record_ns(&self.handoff_ns);
        result
    }

    /// Count `len` bytes as accepted by the backend (`writer_bytes_written`).
    /// Called after each successful constituent operation.
    #[inline]
    pub(crate) fn record_accepted(&self, len: u64) {
        self.bytes_written.fetch_add(len, Relaxed);
    }

    pub fn record_reorder_high_water(&self, len: usize) {
        crate::counters::record_max(&self.reorder_high_water, len as u64);
    }

    /// Emit the counters, at most once per invocation.
    ///
    /// The set belongs to the invocation, not to any writer, so its numbers
    /// are the invocation's totals and nothing else (see the note at the end
    /// of this module).
    /// `emit` used to be called from `PbfWriter::flush`, which meant a
    /// command that constructs several writers - `multi-extract` builds one
    /// per region, and holds them all open at once - emitted the same running
    /// cumulative totals once per region. Verified on Denmark with
    /// `--regions 3`: three `writer_bytes_framed` samples, all carrying the
    /// combined three-region total. The totals were right; what they appeared
    /// to say (one sample per region, therefore per-region numbers) was not.
    ///
    /// Emitting exactly once removes the false per-writer reading. It does
    /// **not** provide per-writer attribution.
    ///
    /// An invocation that never wrote anything emits nothing at all: the
    /// end-of-invocation emitter runs for every command, including the
    /// read-only ones that never construct a `PbfWriter`, and those would
    /// otherwise publish a full set of zeroes that a sidecar reader has to
    /// recognise and discard.
    ///
    /// Returns `true` if this call did the emitting.
    pub fn emit_once(&self, invocation: &crate::host::Recorder) -> bool {
        if self.emitted.swap(true, Relaxed) {
            return false;
        }
        self.emit(invocation)
    }

    /// Returns `true` if anything was emitted. Emits one captured snapshot:
    /// the values the all-zero gate checks are the values published.
    fn emit(&self, invocation: &crate::host::Recorder) -> bool {
        let mut counters: Vec<(&'static str, u64)> = Vec::new();
        self.for_each(|name, value| counters.push((name, value)));

        // An all-zero set means no writer ever ran in this invocation. Say
        // nothing rather than publishing thirty zeroes for every `inspect`.
        if counters.iter().all(|&(_, value)| value == 0) {
            return false;
        }
        for (name, value) in counters {
            invocation.counter(name, value);
        }
        true
    }
}

// # These are invocation totals, not per-writer numbers
//
// Every `writer_*` sidecar counter is a total across every `PbfWriter` and
// output backend one invocation ran (the set lives on its
// `crate::host::Invocation`, `crate::counters::InvocationMetrics`), plus
// publication's syncs of its staged outputs (`writer_sync_all_*`): sums for
// the counts and times, the maximum for the `*_high_water` counters. For the
// single-writer commands - which is nearly all of them - that distinction is
// invisible. For `multi-extract`, which opens one `PbfWriter` per region and
// writes to all of them interleaved from parallel scopes, it means
// per-region attribution is simply not available: `writer_frame_ns` for a
// five-region run is the five regions' framing time added together. Two
// invocations, concurrent or not, record into two sets.
//
// Emitting once per invocation (`WriterMetrics::emit_once`) makes that
// reading unambiguous rather than merely true.
//
// # Why not one instance per writer
//
// Weighed and declined. The sidecar reads these names as totals (`*_wait_ns`
// are summed into `brokkr sidecar --stalls` as strictly monotonic values,
// maximum per name), so per-writer instances would have to be folded back
// into one total before emission and would change nothing a sidecar reader
// sees; emitting them per writer, or under per-writer names, would break
// `--stalls` and every name-keyed comparison against stored runs. A test that
// needs one run's exact count reads its own invocation's set.

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::atomic::Ordering::Relaxed;

    /// `record_sync_all` moves both halves of the pair.
    ///
    /// Tested on the funnel rather than on one backend on purpose: a counter
    /// verified through a single backend is exactly how `sync_all_calls` came
    /// to be right in `FileWriter` and zero in the other two. On a fresh
    /// instance, so the counts are exact.
    #[test]
    fn record_sync_all_bumps_calls_and_nanos_together() {
        let metrics = super::WriterMetrics::new();
        let start = crate::counters::Stopwatch::start();
        std::thread::sleep(std::time::Duration::from_millis(2));
        metrics.record_sync_all(start.elapsed_ns());
        assert_eq!(
            metrics.sync_all_calls.load(Relaxed),
            1,
            "record_sync_all must count the call"
        );
        assert!(
            metrics.sync_all_ns.load(Relaxed) >= 2_000_000,
            "record_sync_all must accumulate the elapsed time"
        );
    }

    /// `emit_once` emits exactly once, however many times it is called.
    ///
    /// Tested on a fresh instance, whose latch no other test can have
    /// tripped.
    #[test]
    fn emit_once_latches_after_the_first_call() {
        let metrics = super::WriterMetrics::new();
        // Record some writer activity first. On a pristine instance every
        // counter is zero, and `emit_once` correctly declines to emit that -
        // so asserting `emit_once() == true` on a fresh instance would fail
        // for a reason that has nothing to do with the latch.
        metrics.record_accepted(4096);
        assert!(
            metrics.emit_once(&crate::host::Call::detached()),
            "the first emit_once must be the one that emits"
        );
        for _ in 0..4 {
            assert!(
                !metrics.emit_once(&crate::host::Call::detached()),
                "emit_once must not emit a second time; that is exactly the \
                 per-region re-emission it exists to stop"
            );
        }
    }

    /// An invocation with no writer activity emits nothing.
    ///
    /// Emission is at the end of the invocation, which every command reaches -
    /// including `inspect` and the other read-only ones that never construct
    /// a `PbfWriter`. Without this gate each of them would publish a full set
    /// of zeroed `writer_*` counters.
    #[test]
    fn an_untouched_metrics_set_emits_nothing() {
        let metrics = super::WriterMetrics::new();
        assert!(
            !metrics.emit_once(&crate::host::Call::detached()),
            "an all-zero counter set must not be emitted"
        );
        // ...and the gate must be on the values, not a second latch: once
        // there is real activity, emission happens.
        let active = super::WriterMetrics::new();
        active.bytes_framed.fetch_add(1, Relaxed);
        assert!(
            active.emit_once(&crate::host::Call::detached()),
            "a counter set with real activity must still emit"
        );
    }
}
