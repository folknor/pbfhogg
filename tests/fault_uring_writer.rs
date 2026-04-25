//! Per-binary fault-injection test for `write::uring_writer`.
//!
//! Split out of `tests/fault_injection.rs` (2026-04-25). Each
//! `tests/fault_*.rs` compiles to its own integration-test binary,
//! so the static `PANIC_AT_*` atomics are per-process and race-free
//! without `#[ignore]` or `--test-threads=1`. See
//! `notes/testing.md` > "Fault-injection split".
//!
//! The test still skips cleanly on hosts where io_uring init fails
//! due to environment constraints (low `RLIMIT_MEMLOCK`, old kernel,
//! missing WriteFixed support); that is an environment skip, not
//! the kind of `#[ignore]` the static-atomic story required.

#![allow(clippy::unwrap_used)]

mod common;

#[cfg(all(feature = "test-hooks", feature = "linux-io-uring"))]
mod uring_writer {
    use std::sync::atomic::Ordering;

    use pbfhogg::block_builder::{self, BlockBuilder};
    use pbfhogg::write::uring_writer_test_hooks as uring_hooks;
    use pbfhogg::writer::{Compression, PbfWriter};

    /// Distinguish "uring unavailable on this host" (low
    /// `RLIMIT_MEMLOCK`, old kernel, missing WriteFixed support)
    /// from real writer bugs. Mirrors the pattern used by the
    /// existing uring roundtrip tests.
    fn is_uring_init_unavailable(err: &std::io::Error) -> bool {
        let os = err.raw_os_error();
        let msg = err.to_string();
        matches!(err.kind(), std::io::ErrorKind::Unsupported)
            || os == Some(libc::ENOSYS)
            || os == Some(libc::EPERM)
            || os == Some(libc::ENOMEM)
            || msg.contains("RLIMIT_MEMLOCK")
            || msg.contains("kernel does not support")
    }

    /// A panic in the uring writer thread mid-stream must surface
    /// via `PbfWriter::flush()` (which joins the writer thread and
    /// routes the join result through `?`) as an `Err`, never a
    /// silent short file.
    #[test]
    fn fault_injection_uring_writer_dispatch_panic_surfaces_via_flush() {
        uring_hooks::reset();

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("uring_fault.osm.pbf");

        let header = block_builder::HeaderBuilder::new().build().expect("header");

        // Construct the uring writer. Init may fail on hosts with
        // insufficient MEMLOCK; treat that as "environment not
        // suitable for this test" and return early rather than
        // claiming a failure the environment caused.
        let mut writer = match PbfWriter::to_path_uring(&path, Compression::default(), &header) {
            Ok(w) => w,
            Err(e) if is_uring_init_unavailable(&e) => {
                eprintln!("io_uring not available, skipping: {e}");
                uring_hooks::reset();
                return;
            }
            Err(e) => panic!("unexpected io_uring init error: {e}"),
        };

        // Arm: panic on dispatch #3. Low enough to land mid-stream
        // with more items still in the pipeline.
        uring_hooks::PANIC_AT_DISPATCH_COUNT.store(3, Ordering::Relaxed);

        // Dispatch 8+ blocks so multiple items flow through the
        // reorder loop. Each block = one PipelineItem = one
        // dispatch. The header was already written before the hook
        // was armed (it's synchronous).
        let write_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for batch in 0..8_i64 {
                let mut bb = BlockBuilder::new();
                for id in 0..5 {
                    let node_id = batch * 100 + id + 1;
                    bb.add_node(
                        node_id,
                        123_456_789,
                        -987_654_321,
                        std::iter::empty::<(&str, &str)>(),
                        None,
                    );
                }
                let bytes = bb.take().expect("block_builder take").expect("block");
                writer.write_primitive_block(bytes)?;
            }
            writer.flush()
        }));

        uring_hooks::reset();

        match write_result {
            Ok(Ok(())) => panic!(
                "uring_writer dispatch panic must not produce a silent success"
            ),
            Ok(Err(_)) => {
                // Expected: flush() returned Err carrying the
                // joined-thread panic.
            }
            Err(_) => {
                // Also acceptable: panic propagates directly via
                // write_primitive_block's send path (channel drop).
            }
        }
    }
}
