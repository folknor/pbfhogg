use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

pub(crate) struct WriterMetrics {
    pub permit_wait_ns: AtomicU64,
    pub frame_ns: AtomicU64,
    pub compress_ns: AtomicU64,
    pub pipeline_send_wait_ns: AtomicU64,
    pub recv_wait_ns: AtomicU64,
    pub reorder_high_water: AtomicU64,
    /// Time blocked waiting for a sequence permit in `ParallelGzipWriter`.
    pub parallel_gzip_reorder_wait_ns: AtomicU64,
    /// Number of `ParallelGzipWriter` dispatches that waited for a sequence permit.
    pub parallel_gzip_reorder_blocked: AtomicU64,
    /// Peak `ParallelGzipWriter` reorder-map length, in gzip chunks. Kept
    /// apart from `reorder_high_water`, which counts PBF blobs in the
    /// writer backends' reorder buffers.
    pub parallel_gzip_reorder_high_water: AtomicU64,
    pub write_ns: AtomicU64,
    pub flush_ns: AtomicU64,
    pub bytes_framed: AtomicU64,
    pub bytes_written: AtomicU64,

    pub payload_framed_items: AtomicU64,
    pub payload_framed_bytes: AtomicU64,
    pub payload_raw_items: AtomicU64,
    pub payload_raw_bytes: AtomicU64,
    pub payload_raw_chunk_items: AtomicU64,
    pub payload_raw_chunk_bytes: AtomicU64,
    pub payload_copy_range_items: AtomicU64,
    pub payload_copy_range_bytes: AtomicU64,

    pub buffered_write_calls: AtomicU64,
    pub buffered_write_bytes: AtomicU64,
    pub direct_write_calls: AtomicU64,
    pub direct_write_bytes: AtomicU64,
    pub sync_all_ns: AtomicU64,
    /// Number of `fsync` calls issued. Separate from `sync_all_ns` because
    /// "did anything fsync at all" is a correctness question - `Write::flush`
    /// must not - while the nanosecond total is a performance one.
    ///
    /// Every backend that fsyncs must record it through
    /// [`WriterMetrics::record_sync_all`]; there is no correct way to bump
    /// `sync_all_ns` alone. The pair used to be maintained by hand at each
    /// site and the parallel and io_uring backends bumped only the
    /// nanoseconds, so `writer_sync_all_calls` reported zero fsyncs for two
    /// of the three backends - a benchmark counter that silently disagreed
    /// with its own documentation.
    pub sync_all_calls: AtomicU64,

    pub uring_submit_calls: AtomicU64,
    pub uring_submit_ns: AtomicU64,
    pub uring_submit_and_wait_calls: AtomicU64,
    pub uring_submit_and_wait_ns: AtomicU64,

    /// Blobs whose `tagdata` was dropped because the encoded `BlobHeader`
    /// would otherwise have hit the 64 KiB format cap. Non-zero means the
    /// output is correct but some blobs lost their tag skip-index; see
    /// `encode_blob_header_into`.
    pub tagdata_dropped_oversize: AtomicU64,

    /// Scratch buffers whose capacity exceeded the 4 MiB retention limit and
    /// were handed off or dropped instead of retained by a worker thread.
    pub frame_scratch_capped_buffers: AtomicU64,
    /// Sum of the full capacities of buffers capped from thread-local scratch.
    pub frame_scratch_capped_capacity_bytes: AtomicU64,

    /// Time a producer on a worker of the writer's executor pool spent in the
    /// cooperative permit wait (`pipeline::recv_permit`). Zero on every CLI
    /// path, which drives its writers off-worker; non-zero means a
    /// `PbfWriter` is being driven from a rayon worker.
    pub coop_wait_ns: AtomicU64,

    /// Latch for [`WriterMetrics::emit_once`], so a process emits the
    /// counter set exactly once no matter how many writers it flushes.
    emitted: AtomicBool,
}

impl WriterMetrics {
    const fn new() -> Self {
        Self {
            permit_wait_ns: AtomicU64::new(0),
            frame_ns: AtomicU64::new(0),
            compress_ns: AtomicU64::new(0),
            pipeline_send_wait_ns: AtomicU64::new(0),
            recv_wait_ns: AtomicU64::new(0),
            reorder_high_water: AtomicU64::new(0),
            parallel_gzip_reorder_wait_ns: AtomicU64::new(0),
            parallel_gzip_reorder_blocked: AtomicU64::new(0),
            parallel_gzip_reorder_high_water: AtomicU64::new(0),
            write_ns: AtomicU64::new(0),
            flush_ns: AtomicU64::new(0),
            bytes_framed: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            payload_framed_items: AtomicU64::new(0),
            payload_framed_bytes: AtomicU64::new(0),
            payload_raw_items: AtomicU64::new(0),
            payload_raw_bytes: AtomicU64::new(0),
            payload_raw_chunk_items: AtomicU64::new(0),
            payload_raw_chunk_bytes: AtomicU64::new(0),
            payload_copy_range_items: AtomicU64::new(0),
            payload_copy_range_bytes: AtomicU64::new(0),
            buffered_write_calls: AtomicU64::new(0),
            buffered_write_bytes: AtomicU64::new(0),
            direct_write_calls: AtomicU64::new(0),
            direct_write_bytes: AtomicU64::new(0),
            sync_all_ns: AtomicU64::new(0),
            sync_all_calls: AtomicU64::new(0),
            uring_submit_calls: AtomicU64::new(0),
            uring_submit_ns: AtomicU64::new(0),
            uring_submit_and_wait_calls: AtomicU64::new(0),
            uring_submit_and_wait_ns: AtomicU64::new(0),
            tagdata_dropped_oversize: AtomicU64::new(0),
            frame_scratch_capped_buffers: AtomicU64::new(0),
            frame_scratch_capped_capacity_bytes: AtomicU64::new(0),
            coop_wait_ns: AtomicU64::new(0),
            emitted: AtomicBool::new(false),
        }
    }

    /// Record one completed `fsync` and how long it took.
    ///
    /// The single funnel for the `sync_all_ns` / `sync_all_calls` pair. Call
    /// this from every backend that syncs, rather than touching either field
    /// directly, so the two cannot drift apart again.
    pub fn record_sync_all(&self, start: std::time::Instant) {
        self.sync_all_ns.fetch_add(
            u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Relaxed,
        );
        self.sync_all_calls.fetch_add(1, Relaxed);
    }

    /// `fsync` `file` and record it through [`record_sync_all`]
    /// (Self::record_sync_all), success or failure. The one way publication
    /// (`crate::output`) syncs a staged file or a directory, so those syncs
    /// show up in the sidecar like the writers' own.
    pub(crate) fn sync_recorded(&self, file: &std::fs::File) -> std::io::Result<()> {
        let start = std::time::Instant::now();
        let result = file.sync_all();
        self.record_sync_all(start);
        result
    }

    /// Record time spent blocked in a writer-thread `recv`.
    ///
    /// Feeds `brokkr sidecar --stalls` via the `writer_recv_wait_ns`
    /// convention. Every backend's writer loop must call it, or that backend
    /// reports as never having stalled on its input.
    pub fn record_recv_wait(&self, start: std::time::Instant) {
        self.recv_wait_ns.fetch_add(
            u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Relaxed,
        );
    }

    pub fn record_reorder_high_water(&self, len: usize) {
        let len = len as u64;
        let mut current = self.reorder_high_water.load(Relaxed);
        while len > current {
            match self
                .reorder_high_water
                .compare_exchange_weak(current, len, Relaxed, Relaxed)
            {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
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

    /// Returns `true` if anything was emitted.
    fn emit(&self) -> bool {
        let mut counters: Vec<(&'static str, i64)> = Vec::new();
        macro_rules! emit {
            ($name:literal, $field:ident) => {
                counters.push((
                    $name,
                    i64::try_from(self.$field.load(Relaxed)).unwrap_or(i64::MAX),
                ));
            };
        }

        emit!("writer_permit_wait_ns", permit_wait_ns);
        emit!("writer_frame_ns", frame_ns);
        emit!("writer_compress_ns", compress_ns);
        emit!("writer_pipeline_send_wait_ns", pipeline_send_wait_ns);
        emit!("writer_recv_wait_ns", recv_wait_ns);
        emit!("writer_reorder_high_water", reorder_high_water);
        emit!(
            "writer_parallel_gzip_reorder_wait_ns",
            parallel_gzip_reorder_wait_ns
        );
        emit!(
            "writer_parallel_gzip_reorder_blocked",
            parallel_gzip_reorder_blocked
        );
        emit!(
            "writer_parallel_gzip_reorder_high_water",
            parallel_gzip_reorder_high_water
        );
        emit!("writer_write_ns", write_ns);
        emit!("writer_flush_ns", flush_ns);
        emit!("writer_bytes_framed", bytes_framed);
        emit!("writer_bytes_written", bytes_written);

        emit!("writer_payload_framed_items", payload_framed_items);
        emit!("writer_payload_framed_bytes", payload_framed_bytes);
        emit!("writer_payload_raw_items", payload_raw_items);
        emit!("writer_payload_raw_bytes", payload_raw_bytes);
        emit!("writer_payload_raw_chunk_items", payload_raw_chunk_items);
        emit!("writer_payload_raw_chunk_bytes", payload_raw_chunk_bytes);
        emit!("writer_payload_copy_range_items", payload_copy_range_items);
        emit!("writer_payload_copy_range_bytes", payload_copy_range_bytes);

        emit!("writer_buffered_write_calls", buffered_write_calls);
        emit!("writer_buffered_write_bytes", buffered_write_bytes);
        emit!("writer_direct_write_calls", direct_write_calls);
        emit!("writer_direct_write_bytes", direct_write_bytes);
        emit!("writer_sync_all_ns", sync_all_ns);
        emit!("writer_sync_all_calls", sync_all_calls);

        emit!("writer_uring_submit_calls", uring_submit_calls);
        emit!("writer_uring_submit_ns", uring_submit_ns);
        emit!(
            "writer_uring_submit_and_wait_calls",
            uring_submit_and_wait_calls
        );
        emit!("writer_uring_submit_and_wait_ns", uring_submit_and_wait_ns);
        emit!("writer_tagdata_dropped_oversize", tagdata_dropped_oversize);
        emit!(
            "writer_frame_scratch_capped_buffers",
            frame_scratch_capped_buffers
        );
        emit!(
            "writer_frame_scratch_capped_capacity_bytes",
            frame_scratch_capped_capacity_bytes
        );
        emit!("writer_coop_wait_ns", coop_wait_ns);

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
/// Every `writer_*` sidecar counter is a sum across every `PbfWriter` the
/// process ever constructed. For the single-writer commands - which is nearly
/// all of them - that distinction is invisible. For `multi-extract`, which
/// opens one `PbfWriter` per region and writes to all of them interleaved
/// from parallel scopes, it means per-region attribution is simply not
/// available: `writer_frame_ns` for a five-region run is the five regions'
/// framing time added together.
///
/// Emitting once per process ([`WriterMetrics::emit_once`]) makes that
/// reading unambiguous rather than merely true. Real per-writer attribution
/// is a separate, larger change and is **not** done: it needs a
/// `&WriterMetrics` handle threaded through every recording site - roughly
/// 85 of them across `framing.rs`, `writer.rs`, `pipeline.rs`,
/// `file_writer.rs`, `parallel_writer.rs` and `uring_writer.rs` - including
/// the rayon-worker framing path, which reaches its scratch through a
/// thread-local and today has no way to name the writer it is framing for.
/// A per-instance scheme would also have to answer what the emitted counter
/// names look like when N writers are live, since the current names are
/// unsuffixed and every existing sidecar comparison depends on them.
pub(crate) static WRITER_METRICS: WriterMetrics = WriterMetrics::new();

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::WRITER_METRICS;
    use std::sync::atomic::Ordering::Relaxed;

    /// `record_sync_all` moves both halves of the pair.
    ///
    /// Tested on the funnel rather than on one backend on purpose: a counter
    /// verified through a single backend is exactly how `sync_all_calls` came
    /// to be right in `FileWriter` and zero in the other two.
    #[test]
    fn record_sync_all_bumps_calls_and_nanos_together() {
        let calls_before = WRITER_METRICS.sync_all_calls.load(Relaxed);
        let ns_before = WRITER_METRICS.sync_all_ns.load(Relaxed);
        let start = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(2));
        WRITER_METRICS.record_sync_all(start);
        assert!(
            WRITER_METRICS.sync_all_calls.load(Relaxed) > calls_before,
            "record_sync_all must count the call"
        );
        assert!(
            WRITER_METRICS.sync_all_ns.load(Relaxed) > ns_before,
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

    /// Nothing but the one process-exit hook may emit the writer counters.
    ///
    /// The defect this guards is not "emit is wrong" - it is "emit is called
    /// from a per-writer code path", which no test of `emit_once` can see and
    /// which shows up only as N sidecar samples of a process total that a
    /// reader takes for N per-writer numbers. `PbfWriter::flush` was that
    /// call site. Restore it and this fails.
    ///
    /// Both spellings of the call are scanned for. Checking only
    /// `WRITER_METRICS.emit` would cover just the variant the defect
    /// happened to take: a backend that re-introduced per-writer emission as
    /// `crate::write::emit_writer_counters()` would slip through, and the
    /// `emit_once` latch would then make the *first* writer's flush publish
    /// partial totals and turn the process-exit call into a no-op - the same
    /// wrong numbers, reached by the other spelling.
    #[test]
    fn writer_counters_are_emitted_only_from_the_process_exit_hook() {
        const SOURCES: [(&str, &str); 6] = [
            ("direct_writer.rs", include_str!("direct_writer.rs")),
            ("file_writer.rs", include_str!("file_writer.rs")),
            ("parallel_writer.rs", include_str!("parallel_writer.rs")),
            ("uring_writer.rs", include_str!("uring_writer.rs")),
            ("pipeline.rs", include_str!("pipeline.rs")),
            ("writer.rs", include_str!("writer.rs")),
        ];
        for (name, src) in SOURCES {
            let squashed: String = src.chars().filter(|c| !c.is_whitespace()).collect();
            for spelling in ["WRITER_METRICS.emit", "emit_writer_counters("] {
                assert!(
                    !squashed.contains(spelling),
                    "{name} emits the writer counters via `{spelling}`; \
                     emission belongs to pbfhogg::write::emit_writer_counters, \
                     called once per process, because WRITER_METRICS is a \
                     process-global total"
                );
            }
        }
        // That the CLI does call the hook on its exit path (or "nothing
        // emits" is satisfied by nothing emitting at all) is checked by the
        // CLI package's own unit tests, which own that source.
    }

    /// No backend may touch `sync_all_ns` or `recv_wait_ns` directly.
    ///
    /// The bug this guards is not "the funnel is wrong" - it is "one backend
    /// bypassed the funnel", which no test of the funnel can see and which
    /// only shows up as a benchmark counter that reads zero for two of three
    /// backends. Checking the sources is the check that matches the defect:
    /// restore a hand-rolled `.sync_all_ns.fetch_add` at any call site and
    /// this fails.
    ///
    /// `emit`/`record_*`/the struct definition live in this file, which is
    /// deliberately not scanned.
    #[test]
    fn backends_record_paired_counters_only_through_the_funnel() {
        // `direct_writer.rs` is scanned even though it records nothing today:
        // it is the one backend file that owns a bare `File`, which makes it
        // the most natural landing spot for a future hand-rolled
        // `sync_all_ns.fetch_add` - exactly the bypass this scan exists for.
        const SOURCES: [(&str, &str); 6] = [
            ("direct_writer.rs", include_str!("direct_writer.rs")),
            ("file_writer.rs", include_str!("file_writer.rs")),
            ("parallel_writer.rs", include_str!("parallel_writer.rs")),
            ("uring_writer.rs", include_str!("uring_writer.rs")),
            ("pipeline.rs", include_str!("pipeline.rs")),
            ("writer.rs", include_str!("writer.rs")),
        ];
        for (name, src) in SOURCES {
            // Squash whitespace so a rustfmt-wrapped
            // `WRITER_METRICS\n.sync_all_ns\n.fetch_add(..)` still matches.
            // Plain *reads* of a counter (tests assert on them) stay legal.
            let squashed: String = src.chars().filter(|c| !c.is_whitespace()).collect();
            for field in ["sync_all_ns", "sync_all_calls", "recv_wait_ns"] {
                assert!(
                    !squashed.contains(&format!("{field}.fetch_add")),
                    "{name} increments WRITER_METRICS.{field} directly; use \
                     record_sync_all / record_recv_wait so the paired \
                     counters cannot drift apart"
                );
            }
        }
    }
}
