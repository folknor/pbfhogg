//! Fault injection into ALTW external stage 3, through the fault plan on
//! the run's token (`altw_stage3_panic_at_bucket` and
//! `altw_stage3_refuse_spawn_at`, see `pbfhogg::test_hooks`).

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
    crate::common::with_heavy_ticket(
        1,
        "fault_injection_altw_stage3_bucket_panic_surfaces_and_cleans_scratch",
        |ticket| {
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
            let host = crate::common::heavy_host(ticket);
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
                Err(_) => {
                    panic!("altw external must return a stage-3 panic as an error, not unwind")
                }
            };
            assert!(
                err.contains("test-hooks: altw stage 3 worker")
                    && err.contains("panicking at bucket 1"),
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
        },
    );
}

/// Run altw external with stage 3 worker `worker`'s spawn refused while a
/// stage 4 thread is parked in the router, and return the error. A refusal
/// that left the router without an abort or producer completion would hang
/// here, so the call runs under a hang bound.
fn run_with_refused_spawn(test: &str, worker: usize, hold_bucket0: bool) {
    crate::common::with_heavy_ticket(1, test, |ticket| {
        let _bound = pbfhogg_test_scratch::HangBound::arm(
            pbfhogg::TEST_HANG_BOUND,
            pbfhogg::TEST_HANG_REPORT_GRACE,
            "altw external did not end after a stage 3 spawn refusal",
        );
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        let output = dir.path().join("out.osm.pbf");
        let nodes: Vec<TestNode> = generate_nodes(60, 1);
        let ways: Vec<TestWay> = generate_ways(10, 10_000, 6, 1);
        write_multi_block_test_pbf(&input, &nodes, &ways, &[], 5);

        let host = crate::common::heavy_host(ticket);
        let faults = host.invocation.faults();
        faults
            .altw_stage3_refuse_spawn_at
            .store(worker, Ordering::SeqCst);
        if hold_bucket0 {
            faults.altw_stage3_hold_bucket.store(0, Ordering::SeqCst);
        }

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
                &host,
                crate::common::scratch_in(dir.path()),
            )
        }));
        let err = match result {
            Ok(Err(e)) => e,
            Ok(Ok(_)) => panic!("altw external with a refused stage 3 spawn must not return Ok"),
            Err(_) => panic!("altw external must return a spawn refusal as an error, not unwind"),
        };
        assert!(
            faults.altw_stage3_spawn_refused.load(Ordering::SeqCst),
            "stage 3 never reached the armed worker's spawn; the test proved nothing: {err}"
        );
        assert!(
            !faults.altw_hook_timed_out.load(Ordering::SeqCst),
            "a bounded altw hook wait expired (no stage 4 park observed): {err}"
        );
        assert!(
            matches!(err.kind(), pbfhogg::ErrorKind::Io(_)),
            "the refusal must come back as the I/O error it is: {err}"
        );
        let text = err.to_string();
        assert!(
            text.contains(&format!(
                "test-hooks: stage 3 worker {worker} spawn refused"
            )),
            "the run failed, but not with the refusal: {text}"
        );
        if hold_bucket0 {
            assert!(
                faults.altw_stage3_saw_abort.load(Ordering::SeqCst),
                "the born worker's held bucket never saw the router aborted"
            );
        }
    });
}

/// No stage 3 worker is born: the refusal alone must end stage 4's parked
/// readiness wait.
#[test]
fn a_refused_first_stage3_worker_releases_a_parked_stage4() {
    run_with_refused_spawn(
        "a_refused_first_stage3_worker_releases_a_parked_stage4",
        0,
        false,
    );
}

/// A stage 3 failure aborts the router without tripping the token; stage 4,
/// parked at the first frame of its passthrough prefix (the untagged node
/// blobs under `keep_untagged_nodes`), must stop there rather than copy the
/// prefix before the error surfaces.
#[test]
fn a_router_abort_stops_the_stage4_passthrough_prefix() {
    let test = "a_router_abort_stops_the_stage4_passthrough_prefix";
    crate::common::with_heavy_ticket(1, test, |ticket| {
        let _bound = pbfhogg_test_scratch::HangBound::arm(
            pbfhogg::TEST_HANG_BOUND,
            pbfhogg::TEST_HANG_REPORT_GRACE,
            "altw external did not end after a stage 3 spawn refusal",
        );
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        let output = dir.path().join("out.osm.pbf");
        // Twelve node blobs of five ahead of the way blobs: a prefix of many
        // passthrough frames before way blob 0.
        let nodes: Vec<TestNode> = generate_nodes(60, 1);
        let ways: Vec<TestWay> = generate_ways(10, 10_000, 6, 1);
        write_multi_block_test_pbf(&input, &nodes, &ways, &[], 5);

        let host = crate::common::heavy_host(ticket);
        let faults = host.invocation.faults();
        faults
            .altw_stage3_refuse_spawn_at
            .store(0, Ordering::SeqCst);
        faults
            .altw_stage4_hold_passthrough_at
            .store(0, Ordering::SeqCst);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            add_locations_to_ways(
                &input,
                crate::common::out(&output),
                &AltwOptions {
                    keep_untagged_nodes: true,
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
        let err = match result {
            Ok(Err(e)) => e,
            Ok(Ok(_)) => panic!("altw external with a refused stage 3 spawn must not return Ok"),
            Err(_) => panic!("altw external must return a spawn refusal as an error, not unwind"),
        };
        assert!(
            faults.altw_stage3_spawn_refused.load(Ordering::SeqCst),
            "stage 3 never reached the armed worker's spawn; the test proved nothing: {err}"
        );
        assert!(
            !faults.altw_hook_timed_out.load(Ordering::SeqCst),
            "a bounded altw hook wait expired: {err}"
        );
        assert!(
            faults
                .altw_stage4_passthrough_saw_abort
                .load(Ordering::SeqCst),
            "the held passthrough never saw the router aborted: {err}"
        );
        assert_eq!(
            faults
                .altw_stage4_passthrough_entries
                .load(Ordering::SeqCst),
            1,
            "stage 4 went on to later passthrough frames after the abort: {err}"
        );
        assert_eq!(
            faults.altw_stage4_passthrough_io.load(Ordering::SeqCst),
            0,
            "stage 4 read a passthrough frame after the router aborted: {err}"
        );
    });
}

/// Worker 0 is born and holds bucket 0; refusing worker 1 must stop it and
/// release stage 4 rather than leave worker 0 to the join.
#[test]
fn a_refused_later_stage3_worker_stops_its_born_peers() {
    run_with_refused_spawn(
        "a_refused_later_stage3_worker_stops_its_born_peers",
        1,
        true,
    );
}
