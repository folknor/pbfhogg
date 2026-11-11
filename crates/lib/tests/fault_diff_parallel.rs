//! The block-pair diff's work units under the fault plan on the run's token
//! (see `pbfhogg::test_hooks`): a panicking unit worker (`diff_unit_panic`),
//! and the answer at every unit size (`diff_unit_target_bytes`), element
//! cut (`diff_unit_lone_elements`) and text chunk and overflow allowance
//! (`diff_text_chunk_bytes`, `diff_text_overflow_bytes`), so small fixtures
//! run every cut and every hand-over path. One test in `tier3` runs the
//! production thresholds at a size they act on.

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

/// The fault plan's planner and text settings for one run; zero keeps the
/// production value.
#[derive(Clone, Copy, Default)]
struct Knobs {
    unit_bytes: usize,
    lone_elements: u64,
    chunk_bytes: usize,
    overflow_bytes: usize,
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

/// One text diff under `knobs`: its text followed by its counts, and the
/// last value of each counter it emitted.
fn diff_run(
    old: &Path,
    new: &Path,
    jobs: usize,
    knobs: Knobs,
    verbose: bool,
    suppress_common: bool,
) -> (String, std::collections::BTreeMap<String, i64>) {
    let counters = std::sync::Arc::new(Counters::default());
    let mut host = crate::common::test_host();
    host.invocation = pbfhogg::host::Invocation::with_sink(
        std::sync::Arc::clone(&counters) as std::sync::Arc<dyn pbfhogg::debug::InstrumentSink>
    );
    let faults = host.invocation.faults();
    faults
        .diff_unit_target_bytes
        .store(knobs.unit_bytes, Ordering::Relaxed);
    faults
        .diff_unit_lone_elements
        .store(knobs.lone_elements, Ordering::Relaxed);
    faults
        .diff_text_chunk_bytes
        .store(knobs.chunk_bytes, Ordering::Relaxed);
    faults
        .diff_text_overflow_bytes
        .store(knobs.overflow_bytes, Ordering::Relaxed);
    let options = DiffOptions {
        suppress_common,
        verbose,
        type_filter: None,
        parallelism: pbfhogg::Parallelism::from_jobs(jobs),
    };
    let mut out = Vec::new();
    let stats = diff(old, new, &mut out, &options, false, &host).unwrap_or_else(|e| {
        panic!(
            "jobs {jobs}, unit {}, lone {}, chunk {}, overflow {}: {}",
            knobs.unit_bytes,
            knobs.lone_elements,
            knobs.chunk_bytes,
            knobs.overflow_bytes,
            e.report()
        )
    });
    let counters = counters.0.lock().unwrap().iter().cloned().collect();
    (
        format!("{}{stats:?}", String::from_utf8_lossy(&out)),
        counters,
    )
}

/// The node units a run emitted (`diffpar_node_units`).
fn node_units(counters: &std::collections::BTreeMap<String, i64>) -> i64 {
    counters["diffpar_node_units"]
}

/// Old nodes `1..=count` cut every `old_block`, and the same nodes each
/// moved north cut every `new_block`: every node is modified, so a verbose
/// diff prints a line and a coordinate change for each.
fn write_moved(
    dir: &Path,
    name: &str,
    count: usize,
    old_block: usize,
    new_block: usize,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let old = dir.join(format!("{name}-old.osm.pbf"));
    let new = dir.join(format!("{name}-new.osm.pbf"));
    write_multi_block_test_pbf(&old, &generate_nodes(count, 1), &[], &[], old_block);
    let moved: Vec<TestNode> = generate_nodes(count, 1)
        .into_iter()
        .map(|mut n| {
            n.lat = n.lat.saturating_add(7);
            n
        })
        .collect();
    write_multi_block_test_pbf(&new, &moved, &[], &[], new_block);
    (old, new)
}

/// Text handed over in many small chunks, with units holding pieces past a
/// full channel against an allowance that is spent at once, partly, or
/// never: the text is the same, whole and in order, in every case. The
/// verbose text (about half a megabyte) is far past the partial allowance,
/// so the three allowances stay distinct, and even the largest chunk makes
/// over a hundred per run against a channel of a few.
#[test]
fn held_text_chunks_keep_the_answer() {
    let dir = crate::common::test_scratch();
    let (old, new) = write_moved(dir.path(), "held", 8_000, 400, 300);
    let (reference, _) = diff_run(&old, &new, 1, Knobs::default(), true, false);
    assert!(
        reference.len() > 256 << 10,
        "the text must pass the partial allowance well: {} bytes",
        reference.len()
    );
    for chunk_bytes in [7, 977, 4096] {
        for overflow_bytes in [1, 64 << 10, usize::MAX / 2] {
            for (jobs, unit_bytes) in [(1, 0), (3, 0), (4, 2_000)] {
                let knobs = Knobs {
                    unit_bytes,
                    chunk_bytes,
                    overflow_bytes,
                    ..Knobs::default()
                };
                let (text, counters) = diff_run(&old, &new, jobs, knobs, true, false);
                let label = format!(
                    "chunk {chunk_bytes}, overflow {overflow_bytes}, jobs {jobs}, unit {unit_bytes}"
                );
                assert_eq!(text, reference, "{label}");
                // What is held never exceeds the text there is.
                let peak = counters["diff_text_overflow_peak_bytes"];
                assert!(
                    usize::try_from(peak).unwrap() <= reference.len(),
                    "{label}: peak {peak}"
                );
            }
        }
    }
}

/// A long stretch only the new file holds (created nodes past the old
/// file's last ID, as in a later planet snapshot) is cut by its claimed
/// element count, with the byte target far away: several node units, and
/// the same text at every worker count and every byte target.
#[test]
fn a_single_sided_stretch_cuts_by_elements_with_the_same_answer() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("lone-old.osm.pbf");
    let new = dir.path().join("lone-new.osm.pbf");
    write_multi_block_test_pbf(&old, &generate_nodes(1_000, 1), &[], &[], 300);
    // 6,000 created nodes in 400-node blobs: six element cuts of 1,000.
    write_multi_block_test_pbf(&new, &generate_nodes(7_000, 1), &[], &[], 400);
    let lone = Knobs {
        lone_elements: 1_000,
        ..Knobs::default()
    };
    let (reference, _) = diff_run(&old, &new, 1, Knobs::default(), false, false);
    assert!(
        reference.contains("created: 6000"),
        "{}",
        &reference[reference.len().saturating_sub(200)..]
    );
    for (jobs, unit_bytes) in [(3, 0), (3, 1), (8, 0)] {
        let knobs = Knobs { unit_bytes, ..lone };
        let (text, counters) = diff_run(&old, &new, jobs, knobs, false, true);
        let (full, _) = diff_run(&old, &new, jobs, knobs, false, false);
        assert_eq!(full, reference, "jobs {jobs}, unit {unit_bytes}");
        if unit_bytes == 0 {
            // Under `-c` only the created stretch counts toward the cut,
            // and the default byte target is never reached.
            let units = node_units(&counters);
            assert!(units >= 4, "jobs {jobs}: {units} node units under -c");
        }
        assert!(text.contains("created: 6000"), "jobs {jobs}");
    }
    let (_, counters) = diff_run(&old, &new, 3, Knobs::default(), false, true);
    assert_eq!(node_units(&counters), 1, "the production element cut");
}

/// Equal elements in different framing, printed without `-c`: every element
/// is a common line, so the planner cuts by every frame's elements, not only
/// single-sided ones. The text is the same at every worker count and byte
/// target, and holds one common line per element. The cut itself makes
/// several units when common lines print and one under `-c`, which prints
/// none; the frames' unequal sizes carry frames across every cut.
#[test]
fn common_lines_cut_by_elements_with_the_same_answer() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("common-old.osm.pbf");
    let new = dir.path().join("common-new.osm.pbf");
    let count = 3_000;
    write_multi_block_test_pbf(&old, &generate_nodes(count, 1), &[], &[], 400);
    write_multi_block_test_pbf(&new, &generate_nodes(count, 1), &[], &[], 300);
    let lone = Knobs {
        lone_elements: 1_000,
        ..Knobs::default()
    };
    let (reference, _) = diff_run(&old, &new, 1, Knobs::default(), false, false);
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
        let (text, _) = diff_run(&old, &new, jobs, Knobs { unit_bytes, ..lone }, false, false);
        assert_eq!(text, reference, "jobs {jobs}, unit {unit_bytes}");
    }
    // 6,000 claimed elements against a cut of 1,000.
    let (_, plain) = diff_run(&old, &new, 3, lone, false, false);
    let plain = node_units(&plain);
    assert!(plain >= 4, "printing common lines: {plain} node units");
    let (_, suppressed) = diff_run(&old, &new, 3, lone, false, true);
    assert_eq!(node_units(&suppressed), 1, "under -c");
}

/// The production configuration, at a size it acts on: tier 3, run by the
/// full profile and the gate.
mod tier3 {
    use super::*;

    /// A unit whose text passes the default output chunk size hands it over
    /// in several chunks, whole and in order, and the default element cut
    /// splits the stretch: 240,000 claimed elements printed one line each
    /// make at least two node units at the default thresholds.
    #[test]
    fn a_unit_output_larger_than_a_chunk_arrives_whole() {
        let dir = crate::common::test_scratch();
        let (old, new) = write_moved(dir.path(), "big", 120_000, 8_000, 6_000);
        let (reference, _) = diff_run(&old, &new, 1, Knobs::default(), true, false);
        assert!(
            reference.len() > 2 << 20,
            "the fixture must pass the 1 MiB chunk size well: {} bytes",
            reference.len()
        );
        for (jobs, unit_bytes) in [(3, 0), (3, 100_000)] {
            let knobs = Knobs {
                unit_bytes,
                ..Knobs::default()
            };
            let (text, counters) = diff_run(&old, &new, jobs, knobs, true, false);
            assert_eq!(text, reference, "jobs {jobs}, unit {unit_bytes}");
            if unit_bytes == 0 {
                let units = node_units(&counters);
                assert!(units >= 2, "the default element cut: {units} node units");
            }
        }
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
