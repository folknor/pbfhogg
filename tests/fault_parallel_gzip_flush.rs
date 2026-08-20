//! Per-binary fault-injection test for `write::parallel_gzip`'s `flush`.
//!
//! Its own binary rather than a second test in `fault_parallel_gzip.rs`
//! because the fault hook is a per-process static: two tests arming
//! `PANIC_AT_POOL_OP_COUNT` in one binary would race under the default
//! parallel test runner. Same split rationale as the other `fault_*.rs`
//! files.

#![allow(clippy::unwrap_used)]

mod common;

#[cfg(feature = "test-hooks")]
mod parallel_gzip_flush {
    use std::io::Write;
    use std::sync::atomic::Ordering;

    use pbfhogg::write::parallel_gzip_test_hooks as gz_hooks;

    /// A compress-worker death must surface through `flush()` as an `Err` -
    /// and `flush()` must return at all.
    ///
    /// `finish()` joins the workers, so a panicked worker was always caught
    /// there. `flush()` never joins: it queues an acknowledgement barrier
    /// behind the dispatched chunks and blocks on the ack. Before the
    /// `Out::WorkerDied` notice existed, a worker that died mid-chunk while a
    /// sibling kept the channel open left a permanent seq gap in front of the
    /// barrier, the writer thread parked short of it forever, and this test
    /// hung until the harness watchdog killed it - which is exactly how the
    /// defect would have presented in production `diff --format osc` runs.
    ///
    /// Two workers, panic on pool op #1: the surviving worker keeps the
    /// compressed channel open, so the hang - not a clean channel-closed
    /// error - is the shape under test.
    #[test]
    fn fault_injection_parallel_gzip_worker_death_surfaces_via_flush() {
        gz_hooks::reset();

        let sink: Vec<u8> = Vec::new();
        let mut gz = gz_hooks::ParallelGzipWriter::new(sink, 1024, 2);
        gz_hooks::PANIC_AT_POOL_OP_COUNT.store(1, Ordering::Relaxed);

        // Four chunks: the first dispatched op panics, later ones ride the
        // surviving worker so the channel stays open across the gap.
        let flush_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            gz.write_all(&[0xAAu8; 4096]).ok();
            gz.flush()
        }));

        gz_hooks::reset();

        match flush_result {
            Ok(Ok(())) => panic!(
                "ParallelGzipWriter::flush() must not report success when a \
                 compress worker died before producing its chunk"
            ),
            Ok(Err(e)) => {
                let msg = format!("{e}");
                let acceptable = msg.contains("compress worker died")
                    || msg.contains("worker pool dropped")
                    || msg.contains("writer thread exited");
                assert!(acceptable, "unexpected error message from flush(): {msg}");
            }
            Err(_) => {
                // The panic propagated through a blocking send; the writer
                // did not claim success, which is the invariant under test.
            }
        }

        // Teardown must not hang either: Drop's best-effort flush and joins
        // run against the same half-dead pool.
        drop(gz);
    }
}
