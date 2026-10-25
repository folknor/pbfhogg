use std::sync::atomic::{AtomicBool, AtomicI32, Ordering::Relaxed};

use crate::counters::Stopwatch;

crate::counters::counter_set! {
    pub(crate) struct WriterMetrics {
        pub permit_wait_ns => "writer_permit_wait_ns",
        pub frame_ns => "writer_frame_ns",
        pub compress_ns => "writer_compress_ns",
        pub pipeline_send_wait_ns => "writer_pipeline_send_wait_ns",
        pub recv_wait_ns => "writer_recv_wait_ns",
        pub reorder_high_water => "writer_reorder_high_water",
        /// Time blocked waiting for a sequence permit in `ParallelGzipWriter`.
        pub parallel_gzip_reorder_wait_ns => "writer_parallel_gzip_reorder_wait_ns",
        /// Number of `ParallelGzipWriter` dispatches that waited for a sequence permit.
        pub parallel_gzip_reorder_blocked => "writer_parallel_gzip_reorder_blocked",
        /// Peak `ParallelGzipWriter` reorder-map length, in gzip chunks. Kept
        /// apart from `reorder_high_water`, which counts PBF blobs in the
        /// writer backends' reorder buffers.
        pub parallel_gzip_reorder_high_water => "writer_parallel_gzip_reorder_high_water",
        pub write_ns => "writer_write_ns",
        pub flush_ns => "writer_flush_ns",
        pub bytes_framed => "writer_bytes_framed",
        pub bytes_written => "writer_bytes_written",

        pub payload_framed_items => "writer_payload_framed_items",
        pub payload_framed_bytes => "writer_payload_framed_bytes",
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
        /// Latch for [`WriterMetrics::emit_once`], so a process emits the
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
    fn record_sync_all(&self, start: Stopwatch) {
        start.record_ns(&self.sync_all_ns);
        self.sync_all_calls.fetch_add(1, Relaxed);
        #[cfg(feature = "test-hooks")]
        sync_observer::observe();
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
        let start = Stopwatch::start();
        // The funnel itself.
        #[allow(clippy::disallowed_methods)]
        let result = file.sync_all();
        self.record_sync_all(start);
        result
    }

    /// Record time spent blocked in a writer-thread `recv`.
    ///
    /// Feeds `brokkr sidecar --stalls` via the `writer_recv_wait_ns`
    /// convention. Every backend's writer loop must call it, or that backend
    /// reports as never having stalled on its input.
    pub fn record_recv_wait(&self, start: Stopwatch) {
        start.record_ns(&self.recv_wait_ns);
    }

    pub fn record_reorder_high_water(&self, len: usize) {
        crate::counters::record_max(&self.reorder_high_water, len as u64);
    }

    /// Emit the counters, at most once per process.
    ///
    /// [`WriterMetrics`] is a process-global static that no writer instance
    /// owns, so its numbers are process totals and nothing else. `emit` used
    /// to be called from `PbfWriter::flush`, which meant a command that
    /// constructs several writers - `multi-extract` builds one per region,
    /// and holds them all open at once - emitted the same running cumulative
    /// totals once per region. Verified on Denmark with `--regions 3`: three
    /// `writer_bytes_framed` samples, all carrying the combined
    /// three-region total. The totals were right; what they appeared to say
    /// (one sample per region, therefore per-region numbers) was not.
    ///
    /// Emitting exactly once removes the false per-writer reading. It does
    /// **not** provide per-writer attribution - see the module docs on
    /// [`WRITER_METRICS`] for what that would take.
    ///
    /// A process that never wrote anything emits nothing at all: moving
    /// emission to the process exit hook means every command reaches it,
    /// including the read-only ones that never construct a `PbfWriter`, and
    /// those would otherwise publish a full set of zeroes that a sidecar
    /// reader has to recognise and discard.
    ///
    /// Returns `true` if this call did the emitting.
    pub fn emit_once(&self) -> bool {
        if self.emitted.swap(true, Relaxed) {
            return false;
        }
        self.emit()
    }

    /// Returns `true` if anything was emitted. Emits one captured snapshot:
    /// the values the all-zero gate checks are the values published.
    fn emit(&self) -> bool {
        let mut counters: Vec<(&'static str, u64)> = Vec::new();
        self.for_each(|name, value| counters.push((name, value)));

        // An all-zero set means no writer ever ran in this process. Say
        // nothing rather than publishing thirty zeroes for every `inspect`.
        if counters.iter().all(|&(_, value)| value == 0) {
            return false;
        }
        for (name, value) in counters {
            crate::debug::emit_counter(name, value);
        }
        true
    }
}

/// Process-global writer counters.
///
/// # These are process totals, not per-writer numbers
///
/// Every `writer_*` sidecar counter is a total across every `PbfWriter` and
/// output backend the process ever ran, plus publication's syncs of staged
/// outputs (`writer_sync_all_*`): sums for the counts and times, the maximum
/// for the `*_high_water` counters. For the single-writer commands - which is
/// nearly all of them - that distinction is invisible. For `multi-extract`,
/// which opens one `PbfWriter` per region and writes to all of them
/// interleaved from parallel scopes, it means per-region attribution is
/// simply not available: `writer_frame_ns` for a five-region run is the five
/// regions' framing time added together. A library embedding that runs
/// several commands in one process, or concurrently, gets their combined
/// totals (`docs/guide/advanced.md` says so).
///
/// Emitting once per process ([`WriterMetrics::emit_once`]) makes that
/// reading unambiguous rather than merely true.
///
/// # Why not one instance per writer
///
/// Weighed and declined. The sidecar reads these names as process totals
/// (`*_wait_ns` are summed into `brokkr sidecar --stalls` as strictly
/// monotonic values, maximum per name), so per-writer instances would have
/// to be folded back into one process total before emission and would
/// change nothing a sidecar reader sees; emitting them per writer, or under
/// per-writer names, would break `--stalls` and every name-keyed comparison
/// against stored runs. What per-instance ownership would buy is
/// attribution nobody consumes, at the price of a handle at every recording
/// site, including the framing jobs on the executor, which reach their
/// scratch through a thread-local and have no writer to name. Tests that
/// need an exact count of one run's fsyncs observe it per token instead
/// (see [`with_sync_observer`]) rather than reading deltas of this total,
/// which concurrent tests also move.
pub(crate) static WRITER_METRICS: WriterMetrics = WriterMetrics::new();

/// Run `f` with every `fsync` recorded on this thread also counted into
/// `cancel`'s fault plan (`FaultPlan::sync_calls`), so a test can assert the
/// exact number of fsyncs one run performed while other tests sync
/// concurrently. Installed by each scope that finishes or publishes an
/// output on its own thread: `FileOutputSink::flush`, the `pwrite` pool's
/// and io_uring's durable finish, `PbfWriter`'s synchronous finalizer, and
/// the three `publish` methods. A thread-local selects the active plan; the
/// plan owns the count. The scope is a closure so the previous selection is
/// always restored, on unwind too.
///
/// Without `test-hooks` this is `f()` and compiles to nothing.
#[cfg(feature = "test-hooks")]
pub(crate) fn with_sync_observer<R>(cancel: &crate::host::Cancel, f: impl FnOnce() -> R) -> R {
    sync_observer::with(cancel, f)
}

/// See the `test-hooks` twin.
#[cfg(not(feature = "test-hooks"))]
#[inline(always)]
pub(crate) fn with_sync_observer<R>(_cancel: &crate::host::Cancel, f: impl FnOnce() -> R) -> R {
    f()
}

#[cfg(feature = "test-hooks")]
mod sync_observer {
    use std::cell::RefCell;

    use crate::host::Cancel;

    thread_local! {
        /// The token whose plan counts this thread's fsyncs, if any. An owned
        /// clone, so nothing can dangle.
        static ACTIVE: RefCell<Option<Cancel>> = const { RefCell::new(None) };
    }

    /// Puts the previous selection back when the scope ends.
    struct Restore(Option<Cancel>);

    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            ACTIVE.with_borrow_mut(|active| *active = previous);
        }
    }

    pub(super) fn with<R>(cancel: &Cancel, f: impl FnOnce() -> R) -> R {
        let previous = ACTIVE.with_borrow_mut(|active| active.replace(cancel.clone()));
        let _restore = Restore(previous);
        // No borrow is held while `f` runs.
        f()
    }

    pub(super) fn observe() {
        ACTIVE.with_borrow(|active| {
            if let Some(cancel) = active {
                cancel
                    .faults()
                    .sync_calls
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::atomic::Ordering::Relaxed;

    /// `record_sync_all` moves both halves of the pair.
    ///
    /// Tested on the funnel rather than on one backend on purpose: a counter
    /// verified through a single backend is exactly how `sync_all_calls` came
    /// to be right in `FileWriter` and zero in the other two. On a fresh
    /// instance, so the counts are exact: the global total moves with every
    /// concurrent test that syncs.
    #[test]
    fn record_sync_all_bumps_calls_and_nanos_together() {
        let metrics = super::WriterMetrics::new();
        let start = crate::counters::Stopwatch::start();
        std::thread::sleep(std::time::Duration::from_millis(2));
        metrics.record_sync_all(start);
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
    /// Tested on a fresh instance rather than on the global static, because
    /// the global's latch may already have been tripped by another test in
    /// the same binary - which would make an assertion against it pass for
    /// the wrong reason.
    #[test]
    fn emit_once_latches_after_the_first_call() {
        let metrics = super::WriterMetrics::new();
        // Record some writer activity first. On a pristine instance every
        // counter is zero, and `emit_once` correctly declines to emit that -
        // so asserting `emit_once() == true` on a fresh instance would fail
        // for a reason that has nothing to do with the latch.
        metrics.bytes_written.fetch_add(4096, Relaxed);
        assert!(
            metrics.emit_once(),
            "the first emit_once must be the one that emits"
        );
        for _ in 0..4 {
            assert!(
                !metrics.emit_once(),
                "emit_once must not emit a second time; that is exactly the \
                 per-region re-emission it exists to stop"
            );
        }
    }

    /// A process with no writer activity emits nothing.
    ///
    /// Emission moved to the CLI exit path, which every command reaches -
    /// including `inspect` and the other read-only ones that never construct
    /// a `PbfWriter`. Without this gate each of them would publish a full set
    /// of zeroed `writer_*` counters.
    #[test]
    fn an_untouched_metrics_set_emits_nothing() {
        let metrics = super::WriterMetrics::new();
        assert!(
            !metrics.emit_once(),
            "an all-zero counter set must not be emitted"
        );
        // ...and the gate must be on the values, not a second latch: once
        // there is real activity, emission happens.
        let active = super::WriterMetrics::new();
        active.bytes_framed.fetch_add(1, Relaxed);
        assert!(
            active.emit_once(),
            "a counter set with real activity must still emit"
        );
    }
}
