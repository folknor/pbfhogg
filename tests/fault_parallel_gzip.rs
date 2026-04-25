//! Per-binary fault-injection test for `write::parallel_gzip`.
//!
//! Split out of `tests/fault_injection.rs` (2026-04-25). Each
//! `tests/fault_*.rs` compiles to its own integration-test binary,
//! so the static `PANIC_AT_POOL_OP_COUNT` is per-process and
//! race-free without `#[ignore]` or `--test-threads=1`.

#![allow(clippy::unwrap_used)]

mod common;

#[cfg(feature = "test-hooks")]
mod parallel_gzip {
    use std::io::Write;
    use std::sync::atomic::Ordering;

    use pbfhogg::write::parallel_gzip_test_hooks as gz_hooks;

    /// A compress-worker panic must surface through `finish()` as an
    /// `Err`, not a silent truncated output. Arms the hook at pool op
    /// #2 and writes enough bytes to dispatch several chunks so the
    /// panic lands mid-stream with other in-flight work still queued.
    #[test]
    fn fault_injection_parallel_gzip_worker_panic_surfaces_via_finish() {
        gz_hooks::reset();

        // 8 chunks of 1 KB each -> 8 dispatched pool ops across 2
        // workers. Panic on op #2 leaves ops 3+ either in the raw
        // channel (never compressed, lost on worker exit) or already
        // sent to other workers (compressed, but writer detects the
        // gap).
        let sink: Vec<u8> = Vec::new();
        let mut gz = gz_hooks::ParallelGzipWriter::new(sink, 1024, 2);
        gz_hooks::PANIC_AT_POOL_OP_COUNT.store(2, Ordering::Relaxed);

        // Writes up to chunk_size per call; each write past the
        // boundary dispatches the current chunk to the pool.
        // Dispatching is wrapped in catch_unwind because a panic can
        // surface either mid-write (worker_loop panic propagates
        // through the channel) or on finish().
        let write_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for _ in 0..8 {
                gz.write_all(&vec![0xAAu8; 1024]).ok();
            }
            gz.finish()
        }));

        gz_hooks::reset();

        match write_result {
            Ok(Ok(_)) => panic!(
                "ParallelGzipWriter::finish() must not silently succeed when a compress worker panicked"
            ),
            Ok(Err(e)) => {
                // Expected: finish() returned Err. The diagnostic
                // should reference either a joined-worker panic or
                // missing chunks at a seq boundary.
                let msg = format!("{e}");
                let acceptable = msg.contains("parallel gzip worker panicked")
                    || msg.contains("chunks missing at seq")
                    || msg.contains("worker pool dropped");
                assert!(
                    acceptable,
                    "unexpected error message from finish(): {msg}"
                );
            }
            Err(_) => {
                // Also acceptable: the panic propagates directly
                // (catch_unwind caught it). The writer did not claim
                // success, which is the invariant this test locks.
            }
        }
    }
}
