//! Derive's block-pair work units under the fault plan on the run's token
//! (see `pbfhogg::test_hooks`): a panicking unit worker
//! (`derive_unit_panic`), and the output at every unit size
//! (`diff_unit_target_bytes`).

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::atomic::Ordering;

use pbfhogg::diff::derive::derive_changes;

use crate::common::{
    TestNode, generate_nodes, generate_ways, snapshot_dir, write_multi_block_test_pbf,
};

/// Old and new node files with misaligned blobs: every 9th node created,
/// every 11th deleted, every 5th moved.
fn write_pair(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let old = dir.join("old.osm.pbf");
    let new = dir.join("new.osm.pbf");
    let old_nodes: Vec<TestNode> = generate_nodes(120, 1)
        .into_iter()
        .enumerate()
        .filter(|(i, _)| i % 9 != 4)
        .map(|(_, n)| n)
        .collect();
    let new_nodes: Vec<TestNode> = generate_nodes(120, 1)
        .into_iter()
        .enumerate()
        .filter(|(i, _)| i % 11 != 7)
        .map(|(i, mut n)| {
            if i % 5 == 0 {
                n.lat = n.lat.saturating_add(1_000);
            }
            n
        })
        .collect();
    // Ways too, so every action section holds two kinds in order.
    let old_ways = generate_ways(30, 1_000, 3, 1);
    let new_ways: Vec<_> = generate_ways(30, 1_000, 3, 1)
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
    write_multi_block_test_pbf(&old, &old_nodes, &old_ways, &[], 5);
    write_multi_block_test_pbf(&new, &new_nodes, &new_ways, &[], 7);
    (old, new)
}

/// One plain-XML derive at `jobs` and `unit_bytes` (0 keeps the default),
/// written to a fresh output each call.
fn derive_xml(
    owner: &pbfhogg_test_scratch::TestScratch,
    old: &Path,
    new: &Path,
    jobs: usize,
    unit_bytes: usize,
) -> String {
    let cell = owner.cell();
    let dir = cell.as_path();
    let host = crate::common::test_host();
    host.invocation
        .faults()
        .diff_unit_target_bytes
        .store(unit_bytes, Ordering::Relaxed);
    let osc = dir.join("out.osc");
    derive_changes(
        old,
        new,
        crate::common::out(&osc),
        false,
        false,
        None,
        pbfhogg::Parallelism::from_jobs(jobs),
        &host,
        crate::common::scratch_in(dir),
    )
    .unwrap_or_else(|e| panic!("jobs {jobs}, unit {unit_bytes}: {}", e.report()));
    std::fs::read_to_string(&osc).unwrap()
}

/// The OSC is the same however the units are cut and however many workers
/// merge them: each action's elements stay in input order.
#[test]
fn the_osc_does_not_depend_on_the_cuts() {
    let dir = crate::common::test_scratch();
    let (old, new) = write_pair(dir.path());
    let reference = derive_xml(&dir, &old, &new, 1, 0);
    assert!(reference.contains("<delete>") && reference.contains("<create>"));
    for jobs in [1, 3] {
        for unit_bytes in [1, 200, 0] {
            assert_eq!(
                derive_xml(&dir, &old, &new, jobs, unit_bytes),
                reference,
                "jobs {jobs}, unit {unit_bytes}"
            );
        }
    }
}

/// One `.osc.gz` derive at `jobs`, one-byte units (a unit of nearly every
/// blob), `chunk` bytes per member and units spilling after `spill_after`
/// queued members, decompressed. Asserts the run left no scratch behind.
fn derive_gz(
    owner: &pbfhogg_test_scratch::TestScratch,
    old: &Path,
    new: &Path,
    jobs: usize,
    chunk: usize,
    spill_after: usize,
) -> String {
    use std::io::Read;
    let cell = owner.cell();
    let dir = cell.as_path();
    let host = crate::common::test_host();
    let faults = host.invocation.faults();
    faults.diff_unit_target_bytes.store(1, Ordering::Relaxed);
    faults.derive_chunk_bytes.store(chunk, Ordering::Relaxed);
    faults
        .derive_spill_after
        .store(spill_after, Ordering::Relaxed);
    let scratch = dir.join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    let osc = dir.join("out.osc.gz");
    derive_changes(
        old,
        new,
        crate::common::out(&osc),
        false,
        false,
        None,
        pbfhogg::Parallelism::from_jobs(jobs),
        &host,
        crate::common::scratch_in(&scratch),
    )
    .unwrap_or_else(|e| {
        panic!(
            "jobs {jobs}, chunk {chunk}, spill {spill_after}: {}",
            e.report()
        )
    });
    assert!(snapshot_dir(&scratch).is_empty(), "spill files left behind");
    let mut xml = String::new();
    flate2::read::MultiGzDecoder::new(std::fs::File::open(&osc).unwrap())
        .read_to_string(&mut xml)
        .unwrap();
    xml
}

/// A gzip OSC decompresses to the plain one whether each unit's members
/// reach the consumer through its channel or through spill files, and
/// whatever the member size: a unit that spills after some queued members,
/// and its last partial members after the spill began, keep each action's
/// elements in input order.
#[test]
fn the_gzip_osc_does_not_depend_on_members_or_spills() {
    let dir = crate::common::test_scratch();
    let (old, new) = write_pair(dir.path());
    let reference = derive_xml(&dir, &old, &new, 1, 0);
    for jobs in [1, 3] {
        for chunk in [64, 0] {
            for spill_after in [usize::MAX, 0, 2] {
                assert_eq!(
                    derive_gz(&dir, &old, &new, jobs, chunk, spill_after),
                    reference,
                    "jobs {jobs}, chunk {chunk}, spill after {spill_after}"
                );
            }
        }
    }
}

/// A panic in a derive unit worker surfaces as an `Err`, publishes nothing
/// and leaves the scratch directory clean.
#[test]
fn fault_injection_derive_unit_panic_surfaces_and_sweeps_scratch() {
    let dir = crate::common::test_scratch();
    let (old, new) = write_pair(dir.path());

    // An explicit scratch parent, checked empty after a clean run first so
    // that the faulted run's leftovers cannot be a previous run's.
    let scratch = dir.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    let osc = dir.path().join("changes.osc.gz");
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

    let host = crate::common::test_host();
    let faults = host.invocation.faults();
    // A one-byte target makes a unit of nearly every blob, so unit 1 exists.
    faults.diff_unit_target_bytes.store(1, Ordering::Relaxed);
    faults.derive_unit_panic.store(1, Ordering::Relaxed);

    // A fresh output name: the clean run published `osc`, and staging over
    // it would be refused before any unit ran.
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

    let err = match result {
        Ok(Err(e)) => e.to_string(),
        Ok(Ok(_)) => panic!("derive_changes with an armed unit panic must not return Ok"),
        Err(_) => panic!("derive_changes must return a unit panic as an error, not unwind"),
    };
    assert!(
        err.contains("test-hooks: derive unit 1 panicking"),
        "the run failed, but not at the armed unit: {err}"
    );
    assert!(!faulted_osc.exists(), "a failed derive published an output");
    assert!(
        snapshot_dir(&scratch).is_empty(),
        "scratch override leaked files"
    );
    assert_eq!(snapshot_dir(dir.path()), before);
}
