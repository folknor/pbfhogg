//! Per-binary fault-injection test for `diff::parallel::run_shard`.
//!
//! Split out of `tests/fault_injection.rs` (2026-04-25). Each
//! `tests/fault_*.rs` compiles to its own integration-test binary,
//! so the static `PANIC_AT_SHARD_IDX` is per-process and race-free
//! without `#[ignore]` or `--test-threads=1`.

#![allow(clippy::unwrap_used)]

mod common;

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
