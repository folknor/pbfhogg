//! CLI-driven integration tests for `pbfhogg repack`.
//!
//! Fixture PBFs are written with the stable-allowlist writer helpers; the
//! repack command runs via the compiled `pbfhogg` binary through
//! `CliInvoker`; output is verified by reading the resulting PBF with the
//! stable-allowlist reader helpers. No imports from
//! `pbfhogg::commands::repack` or any other internal module - a rewrite of
//! `src/commands/repack/` cannot break these tests by type changes alone.

#![allow(clippy::unwrap_used)]

mod common;

use std::path::Path;

use common::cli::CliInvoker;
use common::{
    TestNode, TestRelation, TestWay, generate_nodes, generate_relations, generate_ways,
    read_header, read_normalized, write_multi_block_test_pbf,
};
use pbfhogg::{BlobDecode, BlobReader, Element};

/// Invoke `pbfhogg repack --elements-per-blob N -o <output> <input>`.
fn run_repack(input: &Path, output: &Path, elements_per_blob: usize) -> common::cli::CliOutput {
    CliInvoker::new()
        .arg("repack")
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--elements-per-blob")
        .arg(elements_per_blob.to_string())
        .assert_success()
}

/// Count elements of each kind, per blob, in the output PBF.
///
/// Returns a vec of `(node_count, way_count, relation_count)` tuples in
/// blob order. Used to verify the per-blob element cap is respected and
/// that each blob is single-kind (the PBF interop convention).
fn elements_per_blob(path: &Path) -> Vec<(usize, usize, usize)> {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut per_blob = Vec::new();
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            let mut nodes = 0;
            let mut ways = 0;
            let mut rels = 0;
            for element in block.elements() {
                match element {
                    Element::Node(_) | Element::DenseNode(_) => nodes += 1,
                    Element::Way(_) => ways += 1,
                    Element::Relation(_) => rels += 1,
                    _ => {}
                }
            }
            per_blob.push((nodes, ways, rels));
        }
    }
    per_blob
}

/// Build a sorted, multi-blob fixture: 60 nodes, 12 ways, 3 relations,
/// packed at 20 elements/blob. Yields 3 node blobs + 1 way blob + 1
/// relation blob = 5 OsmData blobs in the input.
///
/// Sizes chosen so input blob size (20) is an exact multiple of the
/// shrink-test cap (10), making per-kind output blob count match the
/// `ceil(elements / cap)` prediction without per-worker fragmentation
/// fudge factors. v1's per-worker cap means a non-multiple input blob
/// size would produce extra partial output blobs at each input-blob
/// boundary.
fn write_repack_fixture(path: &Path) -> (Vec<TestNode>, Vec<TestWay>, Vec<TestRelation>) {
    let mut nodes = generate_nodes(60, 1);
    nodes[0].tags = vec![("place", "city"), ("name", "Origo")];
    nodes[7].tags = vec![("amenity", "cafe")];
    nodes[42].tags = vec![("highway", "bus_stop")];
    let ways = generate_ways(12, 1, 3, 1);
    let rels = generate_relations(3, 1, 2, 1);
    write_multi_block_test_pbf(path, &nodes, &ways, &rels, 20);
    (nodes, ways, rels)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Round-trip preserves element count, IDs, and tag multiset across a
/// shrink (cap=10 vs input 25/blob).
#[test]
fn repack_round_trip_preserves_elements_on_shrink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    let (nodes, ways, rels) = write_repack_fixture(&input);
    run_repack(&input, &output, 10);

    let original = read_normalized(&input);
    let repacked = read_normalized(&output);

    assert_eq!(repacked.nodes.len(), nodes.len());
    assert_eq!(repacked.ways.len(), ways.len());
    assert_eq!(repacked.relations.len(), rels.len());
    assert_eq!(original.nodes, repacked.nodes);
    assert_eq!(original.ways, repacked.ways);
    assert_eq!(original.relations, repacked.relations);
}

/// Every output blob must hold no more than the configured cap, and each
/// blob must be single-kind (PBF interop convention).
#[test]
fn repack_respects_element_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_repack_fixture(&input);
    run_repack(&input, &output, 10);

    let per_blob = elements_per_blob(&output);
    assert!(!per_blob.is_empty(), "output has no OsmData blobs");
    for (i, (n, w, r)) in per_blob.iter().enumerate() {
        let total = n + w + r;
        assert!(total <= 10, "blob {i}: {total} elements exceeds cap 10");
        let kinds_present = u8::from(*n > 0) + u8::from(*w > 0) + u8::from(*r > 0);
        assert_eq!(kinds_present, 1, "blob {i} is mixed-kind: ({n}, {w}, {r})");
    }
}

/// Output blob count tracks the cap-prediction for each kind on shrinks.
/// 60 nodes / cap 10 = 6 node blobs. 12 ways / cap 10 = 2 blobs (1 full +
/// 1 partial of 2). 3 relations / cap 10 = 1 partial. v2.1's central-
/// builder-per-kind makes the prediction `ceil(elements / cap)`
/// regardless of the input blob layout (no per-input-blob fragmentation).
#[test]
fn repack_blob_count_matches_prediction() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_repack_fixture(&input);
    run_repack(&input, &output, 10);

    let per_blob = elements_per_blob(&output);
    let node_blobs = per_blob.iter().filter(|(n, _, _)| *n > 0).count();
    let way_blobs = per_blob.iter().filter(|(_, w, _)| *w > 0).count();
    let rel_blobs = per_blob.iter().filter(|(_, _, r)| *r > 0).count();

    assert_eq!(node_blobs, 6, "expected 6 node blobs (60 nodes / cap 10)");
    assert_eq!(
        way_blobs, 2,
        "expected 2 way blobs (12 ways: 10 + 2 partial)"
    );
    assert_eq!(
        rel_blobs, 1,
        "expected 1 relation blob (3 relations partial)"
    );
}

/// Sort.Type_then_ID propagates from input to output. The input fixture
/// is built via `write_multi_block_test_pbf` which sets the sorted flag.
#[test]
fn repack_propagates_sorted_flag() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_repack_fixture(&input);
    assert!(read_header(&input).is_sorted(), "fixture must be sorted");
    run_repack(&input, &output, 10);
    assert!(
        read_header(&output).is_sorted(),
        "output dropped Sort.Type_then_ID"
    );
}

/// `--elements-per-blob 0` is rejected up front.
#[test]
fn repack_rejects_zero_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_repack_fixture(&input);
    CliInvoker::new()
        .arg("repack")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--elements-per-blob")
        .arg("0")
        .assert_failure()
        .assert_stderr_contains("must be > 0");
}

/// When the cap exceeds every kind's total element count, the central
/// builder never flushes mid-stream and each kind emits exactly one
/// output blob (genuine identity, not a per-input-blob silent
/// passthrough). The CLI emits the "never fired" warning so users running
/// `--elements-per-blob 64000` against a small input know the cap had no
/// effect.
#[test]
fn repack_grow_collapses_to_one_blob_per_kind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_repack_fixture(&input);
    // Cap >> per-kind totals (60 nodes, 12 ways, 3 rels): no mid-stream
    // flush, one output blob per kind.
    let out = run_repack(&input, &output, 8000);
    out.assert_stderr_contains("--elements-per-blob 8000 never fired");

    let original = read_normalized(&input);
    let repacked = read_normalized(&output);
    assert_eq!(original.nodes, repacked.nodes);
    assert_eq!(original.ways, repacked.ways);
    assert_eq!(original.relations, repacked.relations);

    let per_blob = elements_per_blob(&output);
    let node_blobs = per_blob.iter().filter(|(n, _, _)| *n > 0).count();
    let way_blobs = per_blob.iter().filter(|(_, w, _)| *w > 0).count();
    let rel_blobs = per_blob.iter().filter(|(_, _, r)| *r > 0).count();
    assert_eq!(
        node_blobs, 1,
        "expected 60 nodes to collapse to 1 blob (cap 8000)"
    );
    assert_eq!(
        way_blobs, 1,
        "expected 12 ways to collapse to 1 blob (cap 8000)"
    );
    assert_eq!(
        rel_blobs, 1,
        "expected 3 relations to collapse to 1 blob (cap 8000)"
    );
}

/// Round-trip preserves element multiset on a grow that also fires the
/// cap mid-stream: cap=30 vs input 20/blob means the central node
/// builder spans across input-blob boundaries (input blob 1's 20 nodes
/// plus 10 of input blob 2 = first output blob; remaining 10 of blob 2
/// plus all 20 of blob 3 = second output blob).
#[test]
fn repack_round_trip_preserves_elements_on_grow() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    let (nodes, ways, rels) = write_repack_fixture(&input);
    run_repack(&input, &output, 30);

    let original = read_normalized(&input);
    let repacked = read_normalized(&output);

    assert_eq!(repacked.nodes.len(), nodes.len());
    assert_eq!(repacked.ways.len(), ways.len());
    assert_eq!(repacked.relations.len(), rels.len());
    assert_eq!(original.nodes, repacked.nodes);
    assert_eq!(original.ways, repacked.ways);
    assert_eq!(original.relations, repacked.relations);
}

/// Grow output blob count matches `ceil(elements / cap)`. cap=30: 60
/// nodes -> 2 blobs (cross-input-blob coalesce), 12 ways -> 1 partial,
/// 3 relations -> 1 partial.
#[test]
fn repack_grow_blob_count_matches_prediction() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_repack_fixture(&input);
    run_repack(&input, &output, 30);

    let per_blob = elements_per_blob(&output);
    let node_blobs = per_blob.iter().filter(|(n, _, _)| *n > 0).count();
    let way_blobs = per_blob.iter().filter(|(_, w, _)| *w > 0).count();
    let rel_blobs = per_blob.iter().filter(|(_, _, r)| *r > 0).count();
    assert_eq!(node_blobs, 2, "expected 2 node blobs (60 / cap 30)");
    assert_eq!(way_blobs, 1, "expected 1 way blob (12 < cap 30)");
    assert_eq!(rel_blobs, 1, "expected 1 relation blob (3 < cap 30)");
}

/// On a grow that fires the cap mid-stream (cap=30 vs input 20/blob,
/// 60 total nodes) the no-op-cap warning must NOT appear: at least one
/// kind's central builder flushes mid-stream, which suppresses the
/// global warning.
#[test]
fn repack_grow_no_warning_when_cap_fires() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_repack_fixture(&input);
    let out = run_repack(&input, &output, 30);
    let stderr = out.stderr_str();
    assert!(
        !stderr.contains("never fired"),
        "no-op-cap warning should not fire when the cap fires mid-stream; stderr was:\n{stderr}"
    );
}

/// On a shrink (cap=10 vs input 20/blob) the cap fires on every input
/// blob, so the no-op-cap warning must NOT appear. Regression sentinel
/// against false positives on the cap-fires path.
#[test]
fn repack_no_warning_when_cap_fires() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_repack_fixture(&input);
    let out = run_repack(&input, &output, 10);
    let stderr = out.stderr_str();
    assert!(
        !stderr.contains("never fired"),
        "no-op-cap warning should not fire on a real shrink; stderr was:\n{stderr}"
    );
}
