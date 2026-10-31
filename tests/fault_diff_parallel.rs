//! The block-pair diff's work units under the fault plan on the run's token
//! (see `pbfhogg::test_hooks`): a panicking unit worker (`diff_unit_panic`),
//! and the answer at every unit size (`diff_unit_target_bytes`), which cuts
//! a small fixture at every blob boundary the planner allows.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::atomic::Ordering;

use pbfhogg::diff::{DiffOptions, diff};

use crate::common::{
    TestNode, TestWay, generate_nodes, generate_nodes_with_negatives, generate_ways, snapshot_dir,
    write_multi_block_test_pbf,
};

/// Old and new fixtures with blob boundaries that never line up (old cut
/// every `old_block` elements, new every `new_block`), so cuts land inside
/// the other side's blobs: deleted, created and modified nodes, and ways.
fn write_pair(
    dir: &Path,
    negatives: bool,
    old_block: usize,
    new_block: usize,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let old = dir.join(format!("old-{negatives}.osm.pbf"));
    let new = dir.join(format!("new-{negatives}.osm.pbf"));
    let nodes = |n: usize| -> Vec<TestNode> {
        if negatives {
            generate_nodes_with_negatives(n / 4, n - n / 4)
        } else {
            generate_nodes(n, 1)
        }
    };
    let old_nodes: Vec<TestNode> = nodes(120)
        .into_iter()
        .enumerate()
        // Every 9th node exists only in the new file (created there).
        .filter(|(i, _)| i % 9 != 4)
        .map(|(_, n)| n)
        .collect();
    let new_nodes: Vec<TestNode> = nodes(120)
        .into_iter()
        .enumerate()
        // Every 11th node exists only in the old file (deleted).
        .filter(|(i, _)| i % 11 != 7)
        .map(|(i, mut n)| {
            if i % 5 == 0 {
                n.lat = n.lat.saturating_add(1_000);
            }
            n
        })
        .collect();
    let old_ways: Vec<TestWay> = generate_ways(30, 1_000, 3, 1);
    let new_ways: Vec<TestWay> = generate_ways(30, 1_000, 3, 1)
        .into_iter()
        .enumerate()
        .filter(|(i, _)| i % 7 != 3)
        .map(|(i, mut w)| {
            if i % 4 == 0 {
                w.refs.reverse();
            }
            w
        })
        .collect();
    write_multi_block_test_pbf(&old, &old_nodes, &old_ways, &[], old_block);
    write_multi_block_test_pbf(&new, &new_nodes, &new_ways, &[], new_block);
    (old, new)
}

/// The text and counts of one diff at `jobs` workers and `unit_bytes`
/// (0 keeps the default target).
fn diff_text(old: &Path, new: &Path, jobs: usize, unit_bytes: usize, verbose: bool) -> String {
    let host = crate::common::test_host();
    host.invocation
        .faults()
        .diff_unit_target_bytes
        .store(unit_bytes, Ordering::Relaxed);
    let options = DiffOptions {
        suppress_common: false,
        verbose,
        type_filter: None,
        parallelism: pbfhogg::Parallelism::from_jobs(jobs),
    };
    let mut out = Vec::new();
    let stats = diff(old, new, &mut out, &options, false, &host)
        .unwrap_or_else(|e| panic!("jobs {jobs}, unit {unit_bytes}: {}", e.report()));
    format!("{}{stats:?}", String::from_utf8_lossy(&out))
}

/// Cutting every unit at the first boundary the planner allows (a one-byte
/// target), or never within a kind (the default target), gives the same
/// text at every worker count, with negative IDs as with positive ones.
#[test]
fn the_answer_does_not_depend_on_the_cuts() {
    let dir = crate::common::test_scratch();
    for negatives in [false, true] {
        for (old_block, new_block) in [(5, 7), (7, 5), (3, 11)] {
            let (old, new) = write_pair(dir.path(), negatives, old_block, new_block);
            for verbose in [false, true] {
                let reference = diff_text(&old, &new, 1, 0, verbose);
                assert!(reference.contains("created: "), "{reference}");
                for jobs in [1, 3] {
                    for unit_bytes in [1, 200, 0] {
                        assert_eq!(
                            diff_text(&old, &new, jobs, unit_bytes, verbose),
                            reference,
                            "negatives {negatives}, blocks {old_block}/{new_block}, verbose \
                             {verbose}, jobs {jobs}, unit {unit_bytes}"
                        );
                    }
                }
            }
        }
    }
}

/// A unit whose text passes the output chunk size hands it over in several
/// chunks: the text is still whole and in order. Every node moves, so each
/// prints a line, and the default target keeps all of them in one unit.
#[test]
fn a_unit_output_larger_than_a_chunk_arrives_whole() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("big-old.osm.pbf");
    let new = dir.path().join("big-new.osm.pbf");
    let count = 120_000;
    write_multi_block_test_pbf(&old, &generate_nodes(count, 1), &[], &[], 8_000);
    let moved: Vec<TestNode> = generate_nodes(count, 1)
        .into_iter()
        .map(|mut n| {
            n.lat = n.lat.saturating_add(7);
            n
        })
        .collect();
    write_multi_block_test_pbf(&new, &moved, &[], &[], 6_000);
    // Verbose: each moved node prints its line and its coordinate change.
    let reference = diff_text(&old, &new, 1, 0, true);
    assert!(
        reference.len() > 2 << 20,
        "the fixture must pass the 1 MiB chunk size well: {} bytes",
        reference.len()
    );
    for (jobs, unit_bytes) in [(3, 0), (3, 100_000)] {
        assert_eq!(
            diff_text(&old, &new, jobs, unit_bytes, true),
            reference,
            "jobs {jobs}, unit {unit_bytes}"
        );
    }
}

/// A panic in a unit worker surfaces as that unit's error, not an unwind,
/// and leaves nothing on disk; the count-only driver runs the same workers.
#[test]
fn fault_injection_diff_unit_panic_surfaces_as_an_error() {
    let dir = crate::common::test_scratch();
    let (old, new) = write_pair(dir.path(), false, 5, 7);
    let before = snapshot_dir(dir.path());

    let host = crate::common::test_host();
    let faults = host.invocation.faults();
    // A one-byte target makes a unit of nearly every blob, so unit 1 exists.
    faults.diff_unit_target_bytes.store(1, Ordering::Relaxed);
    faults.diff_unit_panic.store(1, Ordering::Relaxed);
    let options = DiffOptions {
        suppress_common: false,
        verbose: false,
        type_filter: None,
        parallelism: pbfhogg::Parallelism::from_jobs(3),
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        diff(&old, &new, &mut Vec::new(), &options, false, &host)
    }));
    let err = match result {
        Ok(Err(e)) => e.to_string(),
        Ok(Ok(_)) => panic!("diff with an armed unit panic must not return Ok"),
        Err(_) => panic!("diff must return a unit panic as an error, not unwind"),
    };
    assert!(
        err.contains("test-hooks: diff unit 1 panicking"),
        "the diff failed, but not at the armed unit: {err}"
    );

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
        other => panic!("diff_stats with an armed unit panic must return Err: {other:?}"),
    };
    assert!(err.to_string().contains("diff unit worker"), "{err}");
    assert_eq!(snapshot_dir(dir.path()), before);
}
