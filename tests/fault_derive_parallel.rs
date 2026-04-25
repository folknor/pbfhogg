//! Per-binary fault-injection test for `diff::derive_parallel::run_shard`.
//!
//! Split out of `tests/fault_injection.rs` (2026-04-25). Each
//! `tests/fault_*.rs` compiles to its own integration-test binary,
//! so the static `PANIC_AT_SHARD_IDX` is per-process and race-free
//! without `#[ignore]` or `--test-threads=1`.

#![allow(clippy::unwrap_used)]

mod common;

#[cfg(feature = "test-hooks")]
mod derive_parallel {
    use std::sync::atomic::Ordering;

    use pbfhogg::diff::derive::derive_changes;
    use pbfhogg::diff::derive_parallel_test_hooks as derive_hooks;

    use crate::common::{
        TestNode, generate_nodes, snapshot_dir, write_multi_block_test_pbf,
    };

    /// A shard panic inside `diff::derive_parallel::run_shard` must
    /// surface as an `Err` and leave the scratch dir clean of
    /// per-shard XML scratch files (creates / modifies / deletes -
    /// three patterns per shard).
    #[test]
    fn fault_injection_derive_parallel_shard_panic_surfaces_and_sweeps_scratch() {
        derive_hooks::reset();

        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join("old.osm.pbf");
        let new = dir.path().join("new.osm.pbf");
        let osc = dir.path().join("changes.osc.gz");

        // Multi-blob fixtures so plan_shards produces > 1 shard.
        let old_nodes: Vec<TestNode> = generate_nodes(40, 1);
        let mut new_nodes: Vec<TestNode> = generate_nodes(40, 1);
        for (i, n) in new_nodes.iter_mut().enumerate() {
            if i % 5 == 0 {
                n.lat = n.lat.saturating_add(1_000);
            }
        }
        write_multi_block_test_pbf(&old, &old_nodes, &[], &[], 5);
        write_multi_block_test_pbf(&new, &new_nodes, &[], &[], 5);

        // `scratch_dir` is inferred as `osc.parent()` by the derive
        // driver, i.e. the tempdir.
        let before = snapshot_dir(dir.path());

        // Arm middle shard.
        derive_hooks::PANIC_AT_SHARD_IDX.store(1, Ordering::Relaxed);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            derive_changes(
                &old,
                &new,
                &osc,
                /* direct_io */ false,
                /* increment_version */ false,
                /* update_timestamp */ false,
                /* jobs */ 3,
            )
        }));

        derive_hooks::reset();

        let silently_succeeded = matches!(result, Ok(Ok(_)));
        assert!(
            !silently_succeeded,
            "derive_changes with an armed shard panic must not return Ok"
        );

        // Scratch cleanup: the driver must sweep every `derive-par-*`
        // (creates / modifies / deletes) after any shard errors.
        let after = snapshot_dir(dir.path());
        let leaked: Vec<_> = after
            .difference(&before)
            .filter(|p| p.to_string_lossy().contains("derive-par-"))
            .collect();
        assert!(
            leaked.is_empty(),
            "scratch leaked per-shard XML temp files after panic: {leaked:?}"
        );
    }
}
