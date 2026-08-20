use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub(crate) struct WriterMetrics {
    pub permit_wait_ns: AtomicU64,
    pub frame_ns: AtomicU64,
    pub compress_ns: AtomicU64,
    pub pipeline_send_wait_ns: AtomicU64,
    pub recv_wait_ns: AtomicU64,
    pub reorder_high_water: AtomicU64,
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
    pub uring_cq_wait_ns: AtomicU64,

    /// Blobs whose `tagdata` was dropped because the encoded `BlobHeader`
    /// would otherwise have hit the 64 KiB format cap. Non-zero means the
    /// output is correct but some blobs lost their tag skip-index; see
    /// `encode_blob_header_into`.
    pub tagdata_dropped_oversize: AtomicU64,
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
            uring_cq_wait_ns: AtomicU64::new(0),
            tagdata_dropped_oversize: AtomicU64::new(0),
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

    pub fn emit(&self) {
        macro_rules! emit {
            ($name:literal, $field:ident) => {
                crate::debug::emit_counter(
                    $name,
                    i64::try_from(self.$field.load(Relaxed)).unwrap_or(i64::MAX),
                );
            };
        }

        emit!("writer_permit_wait_ns", permit_wait_ns);
        emit!("writer_frame_ns", frame_ns);
        emit!("writer_compress_ns", compress_ns);
        emit!("writer_pipeline_send_wait_ns", pipeline_send_wait_ns);
        emit!("writer_recv_wait_ns", recv_wait_ns);
        emit!("writer_reorder_high_water", reorder_high_water);
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
        emit!("writer_uring_cq_wait_ns", uring_cq_wait_ns);
        emit!("writer_tagdata_dropped_oversize", tagdata_dropped_oversize);
    }
}

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
        const SOURCES: [(&str, &str); 5] = [
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
