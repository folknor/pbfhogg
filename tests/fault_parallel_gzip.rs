//! Per-binary fault-injection test for `write::parallel_gzip`.
//!
//! Split out of the former `fault_injection.rs` (2026-04-25). This file is
//! its own `[[test]]` target in `Cargo.toml`,
//! so the static `PANIC_AT_POOL_OP_COUNT` is per-process and
//! race-free without `#[ignore]` or `--test-threads=1`.

#![allow(clippy::unwrap_used)]

mod common;

#[cfg(feature = "static-test-hooks")]
mod parallel_gzip {
    use std::io::Write;
    use std::sync::atomic::Ordering;

    use pbfhogg::write::parallel_gzip_test_hooks as gz_hooks;

    /// A compress-worker panic must surface through `finish()` as an
    /// `Err` carrying the panic's own message, not a silent truncated
    /// output and not a generic "worker panicked". Arms the hook at pool
    /// op #2 and writes enough bytes to dispatch several chunks so the
    /// panic lands mid-stream with other in-flight work still queued.
    ///
    /// `finish()` used to `?` out of its join loop on the first panicked
    /// worker with a fixed string, discarding the payload and ranking
    /// failures by join order; it now joins every thread and returns the
    /// first recorded failure.
    #[test]
    fn fault_injection_parallel_gzip_worker_panic_surfaces_via_finish() {
        gz_hooks::reset();

        // 8 chunks of 1 KB each -> 8 dispatched pool ops across 2
        // workers. Panic on op #2 stops the pool: later chunks are
        // skipped, and writes after the stop fail fast (ignored here).
        let sink: Vec<u8> = Vec::new();
        let mut gz = gz_hooks::ParallelGzipWriter::new(sink, 1024, 2);
        gz_hooks::PANIC_AT_POOL_OP_COUNT.store(2, Ordering::Relaxed);

        // Writes up to chunk_size per call; each write past the
        // boundary dispatches the current chunk to the pool. The
        // catch_unwind is a guard: no panic may escape to the caller.
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
                let msg = format!("{e}");
                assert!(
                    msg.contains("parallel gzip worker panicked")
                        && msg.contains("test-hooks: parallel_gzip pool op #2"),
                    "finish() must report the first failure with its panic payload, got: {msg}"
                );
            }
            Err(_) => panic!("a compress-worker panic must not unwind into the caller"),
        }
    }
}
