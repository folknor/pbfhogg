//! Fault injection into ALTW external stage 3, through the fault plan on
//! the run's token (`altw_stage3_panic_at_bucket`, see `pbfhogg::test_hooks`).

use std::sync::atomic::Ordering;

use pbfhogg::altw::{AltwOptions, IndexType, add_locations_to_ways};
use pbfhogg::write::WriteOptions;

use crate::common::{
    TestNode, TestWay, generate_nodes, generate_ways, snapshot_dir, write_multi_block_test_pbf,
};

/// A stage-3 worker panic during altw external join must surface
/// as an `Err`, wake stage-4 waiters via the `AbortOnDrop` guard
/// (no indefinite hang), and leave no run scratch
/// directory behind after the guard drops.
#[test]
fn fault_injection_altw_stage3_bucket_panic_surfaces_and_cleans_scratch() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    // Multi-blob fixture: enough nodes + ways to produce several
    // slot buckets in stage 3. Block size 5 keeps a realistic
    // number of blobs without blowing out test wall time.
    let nodes: Vec<TestNode> = generate_nodes(60, 1);
    // Ways reference node ids 1..=60 so stage 1's way pass emits
    // real coord requests.
    let ways: Vec<TestWay> = generate_ways(10, 10_000, 6, 1);
    write_multi_block_test_pbf(&input, &nodes, &ways, &[], 5);

    let before = snapshot_dir(dir.path());

    // Arm bucket 1 (middle-ish of stage-3's slot_bucket_count).
    let host = crate::common::test_host();
    host.invocation
        .faults()
        .altw_stage3_panic_at_bucket
        .store(1, Ordering::Relaxed);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        add_locations_to_ways(
            &input,
            crate::common::out(&output),
            &AltwOptions {
                keep_untagged_nodes: false,
                // No `force`: the external index refuses it (it needs the
                // fixture's indexdata), which used to fail this run before
                // stage 3 and pass the test without the hook ever firing.
                write: WriteOptions::default(),
                index_type: IndexType::External,
                inject_prepass: false,
                ignore_missing_nodes: true,
            },
            &pbfhogg::HeaderOverrides::default(),
            &host,
            crate::common::scratch_in(dir.path()),
        )
    }));

    // The failure must be the armed bucket, surfaced as an error.
    let err = match result {
        Ok(Err(e)) => e.to_string(),
        Ok(Ok(_)) => panic!("altw external with an armed stage-3 panic must not return Ok"),
        Err(_) => panic!("altw external must return a stage-3 panic as an error, not unwind"),
    };
    assert!(
        err.contains("test-hooks: altw stage 3 worker") && err.contains("panicking at bucket 1"),
        "the run failed, but not at the armed bucket: {err}"
    );

    // The run scratch directory is swept whole. Assert no `.pbfhogg-run-*`
    // directory remains as a leftover.
    let mut after = snapshot_dir(dir.path());
    // Output file may or may not exist depending on how far the
    // pipeline got; exclude it from the scratch-leak check.
    after.remove(std::path::Path::new("out.osm.pbf"));
    let scratch_leaks: Vec<_> = after
        .difference(&before)
        .filter(|p| p.to_string_lossy().contains(".pbfhogg-run-"))
        .collect();
    assert!(
        scratch_leaks.is_empty(),
        "altw external leaked scratch after panic: {scratch_leaks:?}"
    );
}
