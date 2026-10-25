//! Fault injection into a derive shard worker of the sharded block-pair
//! merge, through the fault plan on the run's token (`derive_shard_panic`,
//! see `pbfhogg::test_hooks`).

#![allow(clippy::unwrap_used)]

use std::sync::atomic::Ordering;

use pbfhogg::diff::derive::derive_changes;

use crate::common::{TestNode, generate_nodes, snapshot_dir, write_multi_block_test_pbf};

/// A panic in a derive shard worker must
/// surface as an `Err` and leave the scratch dir clean of
/// per-shard XML scratch files (creates / modifies / deletes -
/// three patterns per shard).
#[test]
fn fault_injection_derive_parallel_shard_panic_surfaces_and_sweeps_scratch() {
    let dir = crate::common::test_scratch();
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

    // An explicit scratch parent, checked empty after a clean run first so
    // that the faulted run's leftovers cannot be a previous run's.
    let scratch = dir.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    derive_changes(
        &old,
        &new,
        crate::common::out(&osc),
        false,
        false,
        None,
        pbfhogg::Parallelism::from_jobs(3),
        &crate::common::test_host(),
        crate::common::scratch_in(&scratch),
    )
    .unwrap();
    assert!(snapshot_dir(&scratch).is_empty());
    let before = snapshot_dir(dir.path());

    // Arm the middle shard on a fresh token.
    let host = crate::common::test_host();
    host.cancel
        .faults()
        .derive_shard_panic
        .store(1, Ordering::Relaxed);

    // A fresh output name: the clean run published `osc`, and staging over
    // it would be refused before any shard ran (which this test used to
    // mistake for the shard failure).
    let faulted_osc = dir.path().join("faulted.osc.gz");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        derive_changes(
            &old,
            &new,
            crate::common::out(&faulted_osc),
            /* direct_io */ false,
            /* increment_version */ false,
            /* delete timestamp */ None,
            pbfhogg::Parallelism::from_jobs(3),
            &host,
            crate::common::scratch_in(&scratch),
        )
    }));

    // The failure must be the armed shard, surfaced as an error.
    let err = match result {
        Ok(Err(e)) => e.to_string(),
        Ok(Ok(_)) => panic!("derive_changes with an armed shard panic must not return Ok"),
        Err(_) => panic!("derive_changes must return a shard panic as an error, not unwind"),
    };
    assert!(
        err.contains("test-hooks: derive_parallel shard 1 panicking"),
        "the run failed, but not at the armed shard: {err}"
    );
    assert!(!faulted_osc.exists(), "a failed derive published an output");

    // Scratch cleanup: every `derive-*` fragment (creates / modifies /
    // deletes, the run's and each shard's) must be swept after any shard
    // errors.
    let after = snapshot_dir(dir.path());
    assert!(
        snapshot_dir(&scratch).is_empty(),
        "scratch override leaked files"
    );
    let leaked: Vec<_> = after
        .difference(&before)
        .filter(|p| p.to_string_lossy().contains("derive-"))
        .collect();
    assert!(
        leaked.is_empty(),
        "scratch leaked per-shard XML temp files after panic: {leaked:?}"
    );
}
