//! CLI-driven integration tests for `pbfhogg degrade`.
//!
//! Fixtures are built with the stable-allowlist writer helpers; the
//! degrade command runs via the compiled `pbfhogg` binary through
//! `CliInvoker`; output is verified by reading the resulting PBF with
//! the stable-allowlist reader helpers and (where useful) by piping the
//! output through `pbfhogg sort` to confirm it round-trips back to the
//! original element set. No imports from `pbfhogg::commands::degrade` -
//! a rewrite of `src/commands/degrade/` cannot break these tests by
//! type changes alone.

#![allow(clippy::unwrap_used)]

mod common;

use std::path::Path;

use common::cli::CliInvoker;
use common::{
    assert_indexed, assert_non_indexed, generate_nodes, generate_relations, generate_ways,
    read_header, read_normalized, write_multi_block_test_pbf, TestNode, TestRelation, TestWay,
};
use pbfhogg::{BlobDecode, BlobReader, Element};

/// Build a sorted, multi-blob fixture: 60 nodes, 12 ways, 6 relations,
/// packed at 20 elements/blob. Yields 3 node blobs + 1 way blob + 1
/// relation blob = 5 OsmData blobs in the input.
fn write_degrade_fixture(path: &Path) -> (Vec<TestNode>, Vec<TestWay>, Vec<TestRelation>) {
    let mut nodes = generate_nodes(60, 1);
    nodes[0].tags = vec![("place", "city"), ("name", "Origo")];
    nodes[7].tags = vec![("amenity", "cafe")];
    nodes[42].tags = vec![("highway", "bus_stop")];
    let ways = generate_ways(12, 1, 3, 1);
    let rels = generate_relations(6, 1, 2, 1);
    write_multi_block_test_pbf(path, &nodes, &ways, &rels, 20);
    (nodes, ways, rels)
}

/// Per-blob `(kind, min_id, max_id, count)` from the output file. Used
/// to assert overlap structure after `--unsort`.
fn blob_index_summary(path: &Path) -> Vec<(BlobKindLabel, i64, i64, usize)> {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut out = Vec::new();
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            let mut min_id = i64::MAX;
            let mut max_id = i64::MIN;
            let mut nodes = 0;
            let mut ways = 0;
            let mut rels = 0;
            for element in block.elements() {
                match element {
                    Element::Node(n) => {
                        nodes += 1;
                        min_id = min_id.min(n.id());
                        max_id = max_id.max(n.id());
                    }
                    Element::DenseNode(dn) => {
                        nodes += 1;
                        min_id = min_id.min(dn.id());
                        max_id = max_id.max(dn.id());
                    }
                    Element::Way(w) => {
                        ways += 1;
                        min_id = min_id.min(w.id());
                        max_id = max_id.max(w.id());
                    }
                    Element::Relation(r) => {
                        rels += 1;
                        min_id = min_id.min(r.id());
                        max_id = max_id.max(r.id());
                    }
                    _ => {}
                }
            }
            let kind = if nodes > 0 {
                BlobKindLabel::Node
            } else if ways > 0 {
                BlobKindLabel::Way
            } else {
                BlobKindLabel::Relation
            };
            let count = nodes + ways + rels;
            out.push((kind, min_id, max_id, count));
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlobKindLabel {
    Node,
    Way,
    Relation,
}

// ---------------------------------------------------------------------------
// --strip-indexdata
// ---------------------------------------------------------------------------

/// `--strip-indexdata` clears the BlobHeader.indexdata field on every
/// OsmData blob. Element semantics, sortedness, and `LocationsOnWays`
/// (when set) all pass through unchanged because the blob payload is
/// not touched.
#[test]
fn degrade_strip_indexdata_drops_indexdata() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_degrade_fixture(&input);
    assert_indexed(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--strip-indexdata")
        .assert_success();

    assert_non_indexed(&output);

    // Sortedness preserved (the blob payload is unchanged; only the
    // BlobHeader.indexdata is cleared).
    assert!(
        read_header(&output).is_sorted(),
        "--strip-indexdata should not clear Sort.Type_then_ID"
    );

    // Element semantics preserved.
    let original = read_normalized(&input);
    let degraded = read_normalized(&output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

// ---------------------------------------------------------------------------
// --strip-locations
// ---------------------------------------------------------------------------

/// `--strip-locations` clears the `LocationsOnWays` header feature.
/// (The fixture doesn't set LOW, so this test focuses on the sentinel
/// that the flag does not silently re-introduce LOW.) Element data
/// round-trips through the BlockBuilder.
#[test]
fn degrade_strip_locations_clears_low_and_preserves_elements() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_degrade_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--strip-locations")
        .assert_success();

    assert!(
        !read_header(&output).has_locations_on_ways(),
        "--strip-locations output must not declare LocationsOnWays"
    );

    let original = read_normalized(&input);
    let degraded = read_normalized(&output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

// ---------------------------------------------------------------------------
// --unsort
// ---------------------------------------------------------------------------

/// `--unsort` clears `Sort.Type_then_ID` and creates exactly one adjacent
/// same-kind blob pair with overlapping ID ranges per kind that has more
/// than `block_cap + 1` elements. The fixture's 60 nodes / 12 ways /
/// 6 relations all clear that bar at `--block-cap 5`.
#[test]
fn degrade_unsort_creates_adjacent_overlap_per_kind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_degrade_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort")
        .arg("--block-cap")
        .arg("5")
        .assert_success();

    assert!(
        !read_header(&output).is_sorted(),
        "--unsort output must not declare Sort.Type_then_ID"
    );

    // For each kind, find at least one adjacent same-kind blob pair
    // whose ID ranges overlap.
    let blobs = blob_index_summary(&output);
    for kind in [
        BlobKindLabel::Node,
        BlobKindLabel::Way,
        BlobKindLabel::Relation,
    ] {
        let same: Vec<_> = blobs.iter().filter(|(k, ..)| *k == kind).collect();
        assert!(
            same.len() >= 2,
            "kind {kind:?}: need at least 2 blobs to verify overlap, got {}",
            same.len()
        );
        let mut overlap_found = false;
        for window in same.windows(2) {
            let (_, _, a_max, _) = window[0];
            let (_, b_min, _, _) = window[1];
            if a_max >= b_min {
                overlap_found = true;
                break;
            }
        }
        assert!(
            overlap_found,
            "kind {kind:?}: expected at least one adjacent overlap, blobs were {same:?}"
        );
    }

    // Element multiset preserved (just reordered).
    let original = read_normalized(&input);
    let degraded = read_normalized(&output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

/// `--unsort` output piped through `pbfhogg sort` recovers the original
/// element set with `Sort.Type_then_ID` re-declared. Closes the loop on
/// the design's primary use case.
#[test]
fn degrade_unsort_then_sort_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let unsorted = dir.path().join("unsorted.osm.pbf");
    let resorted = dir.path().join("resorted.osm.pbf");

    write_degrade_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&unsorted)
        .arg("--unsort")
        .arg("--block-cap")
        .arg("5")
        .assert_success();

    CliInvoker::new()
        .arg("sort")
        .arg(&unsorted)
        .arg("-o")
        .arg(&resorted)
        .arg("--force")
        .assert_success();

    assert!(
        read_header(&resorted).is_sorted(),
        "sort output must declare Sort.Type_then_ID"
    );

    let original = read_normalized(&input);
    let recovered = read_normalized(&resorted);
    assert_eq!(original.nodes, recovered.nodes);
    assert_eq!(original.ways, recovered.ways);
    assert_eq!(original.relations, recovered.relations);
}

// ---------------------------------------------------------------------------
// Composition
// ---------------------------------------------------------------------------

/// `--unsort --strip-indexdata` composes: output is unsorted *and*
/// unindexed.
#[test]
fn degrade_unsort_and_strip_indexdata_compose() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_degrade_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort")
        .arg("--strip-indexdata")
        .arg("--block-cap")
        .arg("5")
        .assert_success();

    assert_non_indexed(&output);
    assert!(!read_header(&output).is_sorted());

    let original = read_normalized(&input);
    let degraded = read_normalized(&output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Running `degrade` with no transformation flags is rejected.
#[test]
fn degrade_requires_at_least_one_flag() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_degrade_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .assert_failure()
        .assert_stderr_contains("at least one transformation flag");
}

/// `--block-cap 0` is rejected up front.
#[test]
fn degrade_rejects_zero_block_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_degrade_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort")
        .arg("--block-cap")
        .arg("0")
        .assert_failure()
        .assert_stderr_contains("must be > 0");
}
