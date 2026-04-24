//! Fault-injection regression tests for pipelines whose test-hooks use
//! **process-global static atomics**. Tests here are `#[ignore]`d by
//! default because the static hooks race with any concurrently-running
//! test that uses the same pipeline incidentally (e.g. a sibling
//! derive-changes test runs while `fault_injection_parallel_gzip_*`
//! has armed the panic count). Run via
//! `brokkr test <name>` which forces `--test-threads=1`, or
//! `cargo test -- --ignored` with the same flag.
//!
//! Pipelines whose fault-injection hooks are per-instance (carried on
//! a public config struct, e.g. `MergeOptions::panic_at_blob_seq`)
//! don't need this treatment - their tests live alongside their
//! pipeline's other integration tests.

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
    ///
    /// Skips cleanly if io_uring init fails due to environment
    /// constraints (most commonly RLIMIT_MEMLOCK on dev hosts that
    /// haven't raised the limit). See TODO.md > "Important: ignored
    /// tests" for the recipe to actually run this.
    #[test]
    #[ignore]
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

#[cfg(feature = "test-hooks")]
mod diff_parallel {
    use std::sync::atomic::Ordering;

    use pbfhogg::diff::{DiffOptions, diff, parallel_test_hooks as diff_hooks};

    use crate::common::{
        TestNode, generate_nodes, snapshot_dir, write_multi_block_test_pbf,
    };

    /// A shard panic inside `diff::parallel::run_shard` must surface
    /// as an `Err` (scope-join captures the panic, cleanup sweeps
    /// every per-shard scratch file) and leave the scratch dir in
    /// the same state we found it.
    #[test]
    #[ignore]
    fn fault_injection_diff_parallel_shard_panic_surfaces_and_sweeps_scratch() {
        diff_hooks::reset();

        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join("old.osm.pbf");
        let new = dir.path().join("new.osm.pbf");

        // Need multiple old-side blobs for plan_shards to produce
        // more than one shard. 40 nodes at 5 per block -> 8 blobs,
        // `jobs: 3` asks for 3 shards -> `plan_shards` produces 3.
        let old_nodes: Vec<TestNode> = generate_nodes(40, 1);
        // Re-generate for the new side and mutate a few coords so
        // the diff has actual work to do. TestNode doesn't derive
        // Clone, so this is cheaper than hand-cloning.
        let mut new_nodes: Vec<TestNode> = generate_nodes(40, 1);
        for (i, n) in new_nodes.iter_mut().enumerate() {
            if i % 5 == 0 {
                n.lat = n.lat.saturating_add(1_000);
            }
        }
        // Force block flushes every 5 nodes -> 8 node blobs per file.
        // plan_shards(jobs=3, old_descs.len=8) returns 3 shards.
        write_multi_block_test_pbf(&old, &old_nodes, &[], &[], 5);
        write_multi_block_test_pbf(&new, &new_nodes, &[], &[], 5);

        // Scratch dir is inferred as `old.parent()` by the diff
        // driver - i.e. the tempdir. Snapshot it before the run so
        // we can assert no per-shard leaked temp files remain after
        // the panic path.
        let before = snapshot_dir(dir.path());

        // Arm shard 1 (middle of three).
        diff_hooks::PANIC_AT_SHARD_IDX.store(1, Ordering::Relaxed);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut out: Vec<u8> = Vec::new();
            let opts = DiffOptions {
                suppress_common: false,
                verbose: false,
                summary: false,
                type_filter: None,
                jobs: 3,
            };
            diff(&old, &new, &mut out, &opts, false)
        }));

        diff_hooks::reset();

        let silently_succeeded = matches!(result, Ok(Ok(_)));
        assert!(
            !silently_succeeded,
            "diff with an armed shard panic must not return Ok"
        );

        // Scratch cleanup: the driver's post-join cleanup pass must
        // have removed every `diff-par-*.txt.tmp` regardless of
        // whether its shard succeeded or panicked. Only the input
        // PBFs should remain.
        let after = snapshot_dir(dir.path());
        let leaked: Vec<_> = after
            .difference(&before)
            .filter(|p| {
                p.to_string_lossy()
                    .contains("diff-par-")
            })
            .collect();
        assert!(
            leaked.is_empty(),
            "scratch leaked shard temp files after panic: {leaked:?}"
        );
        // Also confirm no non-scratch files appeared (the output
        // went into the in-memory Vec; the tempdir should be
        // unchanged).
        let added: Vec<_> = after.difference(&before).collect();
        assert!(
            added.is_empty(),
            "unexpected files added under scratch: {added:?}"
        );
    }
}

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
    #[ignore]
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
