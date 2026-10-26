//! A stage 4 failure in altw external must stop stage 3, through the fault
//! plan on the run's token (`altw_stage3_hold_bucket` and the
//! `altw_stage4_*` hooks, see `pbfhogg::test_hooks`).
//!
//! Stage 3 holds slot bucket 0, before any of its work, until the router is
//! aborted, with a bound. Way blob 0's slots start at slot 0 and fit in
//! bucket 0, so while it is held way blob 0 stays unpublished and a stage 4
//! worker waiting for it stays parked. Each test then fails stage 4 and
//! checks the held bucket saw the abort: if stage 4 did not abort the router
//! (or, inside its ordered pool, did so only after joining its workers, who
//! are waiting on the held bucket), the hold runs out its bound instead.

use std::sync::atomic::Ordering;

use pbfhogg::altw::{AltwOptions, IndexType, add_locations_to_ways};
use pbfhogg::write::WriteOptions;

use crate::common::{TestNode, TestWay, generate_ways, write_multi_block_test_pbf};

/// Tagged nodes, so every node blob is decoded by stage 4 (untagged ones
/// would be skipped) and decode ordinal 0 is a node blob, which never waits
/// on the router; then ways whose refs are all present.
fn fixture(dir: &std::path::Path) -> std::path::PathBuf {
    let input = dir.join("in.osm.pbf");
    let nodes: Vec<TestNode> = (1..=60)
        .map(|id| TestNode {
            id,
            lat: 10_000 * i32::try_from(id).expect("small id"),
            lon: 10_000 * i32::try_from(id).expect("small id"),
            tags: vec![("amenity", "bench")],
            meta: None,
        })
        .collect();
    let ways: Vec<TestWay> = generate_ways(10, 10_000, 6, 1);
    write_multi_block_test_pbf(&input, &nodes, &ways, &[], 5);
    input
}

/// Run altw external on the fixture under `host`, returning its error.
fn run_failing(host: &pbfhogg::host::Host) -> String {
    let dir = crate::common::test_scratch();
    let input = fixture(dir.path());
    let output = dir.path().join("out.osm.pbf");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        add_locations_to_ways(
            &input,
            crate::common::out(&output),
            &AltwOptions {
                keep_untagged_nodes: false,
                write: WriteOptions::default(),
                index_type: IndexType::External,
                inject_prepass: false,
                ignore_missing_nodes: true,
            },
            &pbfhogg::HeaderOverrides::default(),
            host,
            crate::common::scratch_in(dir.path()),
        )
    }));
    match result {
        Ok(Err(e)) => e.to_string(),
        Ok(Ok(_)) => panic!("altw external with an armed stage 4 failure must not return Ok"),
        Err(_) => panic!("altw external must return a stage 4 failure as an error, not unwind"),
    }
}

fn assert_stage3_stopped(host: &pbfhogg::host::Host) {
    let faults = host.cancel.faults();
    assert!(
        faults.altw_stage3_bucket_held.load(Ordering::SeqCst),
        "stage 3 never claimed the held bucket; the test proved nothing"
    );
    assert!(
        faults.altw_stage3_saw_abort.load(Ordering::SeqCst),
        "the held stage 3 bucket never saw the router aborted"
    );
    assert!(
        !faults.altw_hook_timed_out.load(Ordering::SeqCst),
        "a bounded altw hook wait expired"
    );
}

/// A failure before stage 4's pool starts aborts the router.
#[test]
fn a_stage4_failure_at_start_stops_stage3() {
    let host = crate::common::test_host();
    let faults = host.cancel.faults();
    faults.altw_stage3_hold_bucket.store(0, Ordering::SeqCst);
    faults
        .altw_stage4_fail_at_start
        .store(true, Ordering::SeqCst);

    let err = run_failing(&host);
    assert!(
        err.contains("test-hooks: stage 4 failing at start"),
        "the run failed, but not with stage 4's error: {err}"
    );
    assert_stage3_stopped(&host);
}

/// A consumer error inside the pool, with a worker waiting on the held
/// bucket, aborts the router before the pool joins that worker.
#[test]
fn a_stage4_consumer_error_releases_its_parked_workers_and_stops_stage3() {
    let host = crate::common::test_host();
    let faults = host.cancel.faults();
    faults.altw_stage3_hold_bucket.store(0, Ordering::SeqCst);
    faults
        .altw_stage4_fail_consume_at
        .store(0, Ordering::SeqCst);

    let err = run_failing(&host);
    assert!(
        err.contains("test-hooks: stage 4 consumer failing at ordinal 0"),
        "the run failed, but not with stage 4's error: {err}"
    );
    assert!(faults.altw_stage4_way0_entered.load(Ordering::SeqCst));
    assert_stage3_stopped(&host);
}

/// The same with a consumer panic: the consumer's guard aborts the router
/// during the unwind, before the pool joins.
#[test]
fn a_stage4_consumer_panic_releases_its_parked_workers_and_stops_stage3() {
    let host = crate::common::test_host();
    let faults = host.cancel.faults();
    faults.altw_stage3_hold_bucket.store(0, Ordering::SeqCst);
    faults
        .altw_stage4_panic_consume_at
        .store(0, Ordering::SeqCst);

    let err = run_failing(&host);
    assert!(
        err.contains("test-hooks: stage 4 consumer panicking at ordinal 0"),
        "the run failed, but not with stage 4's panic: {err}"
    );
    assert!(faults.altw_stage4_way0_entered.load(Ordering::SeqCst));
    assert_stage3_stopped(&host);
}
