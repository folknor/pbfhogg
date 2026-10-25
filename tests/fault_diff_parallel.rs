//! Fault injection into a text diff shard worker of the sharded block-pair
//! merge, through the fault plan on the run's token (`diff_shard_panic`,
//! see `pbfhogg::test_hooks`).

#![allow(clippy::unwrap_used)]

use std::sync::atomic::Ordering;

use pbfhogg::diff::{DiffOptions, diff};

use crate::common::{TestNode, generate_nodes, snapshot_dir, write_multi_block_test_pbf};

/// A panic in a text diff shard worker must surface
/// as an `Err` (scope-join captures the panic, cleanup sweeps
/// every per-shard scratch file) and leave the scratch dir in
/// the same state we found it.
#[test]
fn fault_injection_diff_parallel_shard_panic_surfaces_and_sweeps_scratch() {
    let dir = crate::common::test_scratch();
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

    let options = DiffOptions {
        suppress_common: false,
        verbose: false,
        type_filter: None,
        parallelism: pbfhogg::Parallelism::from_jobs(3),
    };

    // An explicit scratch parent, checked empty after a clean run first so
    // that the faulted run's leftovers cannot be a previous run's.
    let scratch = dir.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    diff(
        &old,
        &new,
        &mut Vec::new(),
        &options,
        false,
        &crate::common::test_host(),
        Some(crate::common::scratch_in(&scratch)),
    )
    .unwrap();
    assert!(snapshot_dir(&scratch).is_empty());
    let before = snapshot_dir(dir.path());

    // Arm shard 1 (middle of three) on a fresh token.
    let host = crate::common::test_host();
    host.cancel
        .faults()
        .diff_shard_panic
        .store(1, Ordering::Relaxed);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut out: Vec<u8> = Vec::new();
        diff(
            &old,
            &new,
            &mut out,
            &options,
            false,
            &host,
            Some(crate::common::scratch_in(&scratch)),
        )
    }));

    // The failure must be the armed shard, surfaced as an error.
    let err = match result {
        Ok(Err(e)) => e.to_string(),
        Ok(Ok(_)) => panic!("diff with an armed shard panic must not return Ok"),
        Err(_) => panic!("diff must return a shard panic as an error, not unwind"),
    };
    assert!(
        err.contains("test-hooks: diff/parallel shard 1 panicking"),
        "the diff failed, but not at the armed shard: {err}"
    );

    // Scratch cleanup: the driver's post-join cleanup pass must
    // have removed every `diff-par-*.txt.tmp` regardless of
    // whether its shard succeeded or panicked. Only the input
    // PBFs should remain.
    let after = snapshot_dir(dir.path());
    assert!(
        snapshot_dir(&scratch).is_empty(),
        "scratch override leaked files"
    );
    let leaked: Vec<_> = after
        .difference(&before)
        .filter(|p| p.to_string_lossy().contains("diff-par-"))
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

    // The count-only driver (`diff_stats`) runs the same shard workers
    // without scratch: the panic surfaces as an error there too, and
    // nothing appears on disk. Same armed token.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pbfhogg::diff::diff_stats(
            &old,
            &new,
            None,
            pbfhogg::Parallelism::from_jobs(3),
            false,
            &host,
        )
    }));
    let err = match result {
        Ok(Err(e)) => e,
        other => panic!("diff_stats with an armed shard panic must return Err: {other:?}"),
    };
    assert!(err.to_string().contains("shard worker"), "{err}");
    assert_eq!(snapshot_dir(dir.path()), before);
}
