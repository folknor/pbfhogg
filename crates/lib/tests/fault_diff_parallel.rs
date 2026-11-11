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
    let nodes = |n: usize| -> Vec<TestNode<'static>> {
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
/// prints a line and its coordinate change; the element cut makes two units
/// of the default target, each well past one chunk.
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

/// Text handed over in many small chunks, with units holding pieces past a
/// full channel against an allowance that is spent at once, partly, or
/// never: the text is the same, whole and in order, in every case.
#[test]
fn held_text_chunks_keep_the_answer() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("held-old.osm.pbf");
    let new = dir.path().join("held-new.osm.pbf");
    let count = 60_000;
    write_multi_block_test_pbf(&old, &generate_nodes(count, 1), &[], &[], 8_000);
    let moved: Vec<TestNode> = generate_nodes(count, 1)
        .into_iter()
        .map(|mut n| {
            n.lat = n.lat.saturating_add(7);
            n
        })
        .collect();
    write_multi_block_test_pbf(&new, &moved, &[], &[], 6_000);
    let reference = diff_text(&old, &new, 1, 0, true);
    let faults_text = |chunk: usize, overflow: usize, jobs: usize, unit: usize| {
        let host = crate::common::test_host();
        let faults = host.invocation.faults();
        faults.diff_unit_target_bytes.store(unit, Ordering::Relaxed);
        faults.diff_text_chunk_bytes.store(chunk, Ordering::Relaxed);
        faults
            .diff_text_overflow_bytes
            .store(overflow, Ordering::Relaxed);
        let options = DiffOptions {
            suppress_common: false,
            verbose: true,
            type_filter: None,
            parallelism: pbfhogg::Parallelism::from_jobs(jobs),
        };
        let mut out = Vec::new();
        let stats = diff(&old, &new, &mut out, &options, false, &host)
            .unwrap_or_else(|e| panic!("chunk {chunk}, overflow {overflow}: {}", e.report()));
        format!("{}{stats:?}", String::from_utf8_lossy(&out))
    };
    for chunk in [7, 977, 4096] {
        for overflow in [1, 64 << 10, usize::MAX / 2] {
            for (jobs, unit) in [(1, 0), (3, 0), (4, 100_000)] {
                assert_eq!(
                    faults_text(chunk, overflow, jobs, unit),
                    reference,
                    "chunk {chunk}, overflow {overflow}, jobs {jobs}, unit {unit}"
                );
            }
        }
    }
}

/// A long stretch only the new file holds (created nodes past the old
/// file's last ID, as in a later planet snapshot) is cut by its claimed
/// element count as well as by bytes: the text is the same at every worker
/// count and every byte target.
#[test]
fn a_single_sided_stretch_cuts_by_elements_with_the_same_answer() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("lone-old.osm.pbf");
    let new = dir.path().join("lone-new.osm.pbf");
    write_multi_block_test_pbf(&old, &generate_nodes(20_000, 1), &[], &[], 8_000);
    // 400,000 created nodes: several element targets of one-sided blobs.
    write_multi_block_test_pbf(&new, &generate_nodes(420_000, 1), &[], &[], 8_000);
    let reference = diff_text(&old, &new, 1, 0, false);
    assert!(
        reference.contains("created: 400000"),
        "{}",
        &reference[reference.len().saturating_sub(200)..]
    );
    for (jobs, unit_bytes) in [(3, 0), (3, 1), (8, 0)] {
        assert_eq!(
            diff_text(&old, &new, jobs, unit_bytes, false),
            reference,
            "jobs {jobs}, unit {unit_bytes}"
        );
    }
}

/// Equal elements in different framing, printed without `-c`: every element
/// is a common line, so the planner cuts by every frame's elements, not only
/// single-sided ones. The text is the same at every worker count and byte
/// target, and holds one common line per element.
#[test]
fn common_lines_cut_by_elements_with_the_same_answer() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("common-old.osm.pbf");
    let new = dir.path().join("common-new.osm.pbf");
    let count = 300_000;
    write_multi_block_test_pbf(&old, &generate_nodes(count, 1), &[], &[], 8_000);
    write_multi_block_test_pbf(&new, &generate_nodes(count, 1), &[], &[], 6_000);
    let reference = diff_text(&old, &new, 1, 0, false);
    assert!(
        reference.ends_with(&format!(
            "DiffStats {{ common: {count}, created: 0, modified: 0, deleted: 0 }}"
        )),
        "{}",
        &reference[reference.len().saturating_sub(200)..]
    );
    // One line per element, then the counts.
    assert_eq!(reference.lines().count(), count + 1);
    for (jobs, unit_bytes) in [(3, 0), (3, 1), (8, 0)] {
        assert_eq!(
            diff_text(&old, &new, jobs, unit_bytes, false),
            reference,
            "jobs {jobs}, unit {unit_bytes}"
        );
    }
    // The cut itself: 600,000 claimed elements make several units when
    // common lines print, one under `-c`, which prints none.
    let plain = node_units(&old, &new, false);
    assert!(plain >= 4, "printing common lines: {plain} node units");
    assert_eq!(node_units(&old, &new, true), 1, "under -c");
}

/// Records every counter a run emits.
#[derive(Default)]
struct Counters(std::sync::Mutex<Vec<(String, i64)>>);
impl pbfhogg::debug::InstrumentSink for Counters {
    fn marker(&self, _name: &str) {}
    fn counter(&self, name: &str, value: i64) {
        self.0.lock().unwrap().push((name.to_owned(), value));
    }
}

/// The node units a text diff at 3 workers and the default target ran.
fn node_units(old: &Path, new: &Path, suppress_common: bool) -> i64 {
    let counters = std::sync::Arc::new(Counters::default());
    let mut host = crate::common::test_host();
    host.invocation = pbfhogg::host::Invocation::with_sink(
        std::sync::Arc::clone(&counters) as std::sync::Arc<dyn pbfhogg::debug::InstrumentSink>
    );
    let options = DiffOptions {
        suppress_common,
        verbose: false,
        type_filter: None,
        parallelism: pbfhogg::Parallelism::from_jobs(3),
    };
    diff(old, new, &mut std::io::sink(), &options, false, &host).unwrap();
    let counters = counters.0.lock().unwrap();
    counters
        .iter()
        .find(|(name, _)| name == "diffpar_node_units")
        .map(|&(_, v)| v)
        .unwrap()
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
