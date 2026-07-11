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
    TestNode, TestRelation, TestWay, assert_indexed, assert_non_indexed, assert_sorted_file,
    generate_nodes, generate_relations, generate_ways, read_header, read_normalized,
    write_multi_block_test_pbf,
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

/// Build a sorted fixture whose input blobs are deliberately *smaller*
/// than the `--block-cap` the unsort tests use (4 elements/blob vs cap
/// 10). This is the regime that distinguishes the two unsort modes and
/// reproduces the real-world bug: when input blobs are smaller than the
/// cap, the buggy per-input-blob boundary flush confines the swap to one
/// output blob (intra-blob inversion) instead of producing the documented
/// cross-blob overlap. Each kind has well over `cap + 1` elements so the
/// swap fires for all three kinds.
fn write_unsort_fixture(path: &Path) -> (Vec<TestNode>, Vec<TestWay>, Vec<TestRelation>) {
    let nodes = generate_nodes(60, 1);
    let ways = generate_ways(24, 1, 3, 1);
    let rels = generate_relations(24, 1, 2, 1);
    write_multi_block_test_pbf(path, &nodes, &ways, &rels, 4);
    (nodes, ways, rels)
}

/// The `--block-cap` the unsort tests pass. Larger than the unsort
/// fixture's 4-element input blobs so the two modes diverge.
const UNSORT_CAP: &str = "10";

/// Build a sorted fixture whose input blobs are deliberately *larger* than
/// the `--block-cap` the large-blob test uses (20 elements/blob vs cap 5).
/// This is the regime that exposed finding 1: one input blob carrying more
/// than `cap` same-kind elements. Keying the swap to the `cap` boundary
/// (the old shared logic) made `--unsort-intra` fill and flush an output
/// block here, producing the cross-blob overlap shape instead of an
/// intra-blob inversion. The fix keys `--unsort-intra`'s swap to the first
/// two elements, so it stays inside the first output block regardless of
/// input blob size. Each kind has well over `cap` elements.
fn write_large_blob_unsort_fixture(
    path: &Path,
) -> (Vec<TestNode>, Vec<TestWay>, Vec<TestRelation>) {
    let nodes = generate_nodes(60, 1);
    let ways = generate_ways(60, 1, 3, 1);
    let rels = generate_relations(60, 1, 2, 1);
    write_multi_block_test_pbf(path, &nodes, &ways, &rels, 20);
    (nodes, ways, rels)
}

/// The `--block-cap` the large-blob test passes. Smaller than the
/// large-blob fixture's 20-element input blobs so a single input blob
/// spans more than one output block.
const LARGE_BLOB_CAP: &str = "5";

/// Per-blob `(kind, ordered element ids)` from the output file. Element
/// ids are returned in stream order so callers can detect intra-blob
/// inversions (a descending step within one blob).
fn blob_elements(path: &Path) -> Vec<(BlobKindLabel, Vec<i64>)> {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut out = Vec::new();
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            let mut ids = Vec::new();
            let mut nodes = 0;
            let mut ways = 0;
            for element in block.elements() {
                match element {
                    Element::Node(n) => {
                        nodes += 1;
                        ids.push(n.id());
                    }
                    Element::DenseNode(dn) => {
                        nodes += 1;
                        ids.push(dn.id());
                    }
                    Element::Way(w) => {
                        ways += 1;
                        ids.push(w.id());
                    }
                    Element::Relation(r) => {
                        ids.push(r.id());
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
            out.push((kind, ids));
        }
    }
    out
}

/// Count adjacent same-kind blob pairs with overlapping ID ranges
/// (`max_id` of one blob >= `min_id` of the next same-kind blob). The CLI
/// promises exactly one such overlap per eligible kind under `--unsort`,
/// so tests assert on the count, not just presence.
fn count_adjacent_overlaps(
    blobs: &[(BlobKindLabel, i64, i64, usize)],
    kind: BlobKindLabel,
) -> usize {
    let same: Vec<_> = blobs.iter().filter(|(k, ..)| *k == kind).collect();
    same.windows(2)
        .filter(|w| {
            let (_, _, a_max, _) = w[0];
            let (_, b_min, _, _) = w[1];
            a_max >= b_min
        })
        .count()
}

/// Count strictly-descending steps (internal ID inversions) across all
/// blobs of `kind`. Each unsort swap contributes exactly one, so
/// `--unsort-intra` tests assert this equals one per eligible kind and
/// `--unsort` tests assert it equals zero.
fn count_intra_blob_inversions(blobs: &[(BlobKindLabel, Vec<i64>)], kind: BlobKindLabel) -> usize {
    blobs
        .iter()
        .filter(|(k, _)| *k == kind)
        .map(|(_, ids)| ids.windows(2).filter(|w| w[0] > w[1]).count())
        .sum()
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

/// `--unsort` clears `Sort.Type_then_ID` and produces genuine cross-blob
/// overlap: at least one adjacent same-kind blob pair whose indexdata ID
/// ranges overlap, per kind that has more than `block_cap + 1` elements.
///
/// The fixture packs input at 4 elements/blob and the run uses
/// `--block-cap 10`, so input blobs are smaller than the cap. This is the
/// regime that regressed before the fix (the per-input-blob boundary
/// flush confined the swap to one output blob and `detect_overlaps`
/// returned zero). The central builder must now pack continuously across
/// input blobs so the swap straddles a real output-blob boundary. The two
/// straddling blobs stay internally ID-monotone - the disorder lives at
/// the inter-blob seam, not inside a blob.
#[test]
fn degrade_unsort_creates_adjacent_overlap_per_kind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort")
        .arg("--block-cap")
        .arg(UNSORT_CAP)
        .assert_success();

    assert_unsort_cross_blob_shape(&output, &input);
}

/// Shared assertions for the `--unsort` cross-blob shape: header sortedness
/// cleared, exactly one adjacent cross-blob overlap per kind, zero
/// intra-blob inversions (each blob internally ID-monotone), element
/// multiset preserved.
fn assert_unsort_cross_blob_shape(output: &Path, input: &Path) {
    assert!(
        !read_header(output).is_sorted(),
        "--unsort output must not declare Sort.Type_then_ID"
    );

    let summary = blob_index_summary(output);
    let elements = blob_elements(output);
    for kind in [
        BlobKindLabel::Node,
        BlobKindLabel::Way,
        BlobKindLabel::Relation,
    ] {
        let same_count = summary.iter().filter(|(k, ..)| *k == kind).count();
        assert!(
            same_count >= 2,
            "kind {kind:?}: need at least 2 blobs to verify overlap, got {same_count}"
        );
        // The CLI promises exactly one adjacent cross-blob overlap per
        // eligible kind - the minimum perturbation that fires sort's
        // detect_overlaps. Count it, don't just check presence.
        assert_eq!(
            count_adjacent_overlaps(&summary, kind),
            1,
            "kind {kind:?}: expected exactly one adjacent cross-blob overlap, \
             blobs were {:?}",
            summary
                .iter()
                .filter(|(k, ..)| *k == kind)
                .collect::<Vec<_>>()
        );
        // The overlap is expressed cross-blob; each blob stays internally
        // ID-monotone (this is what separates --unsort from --unsort-intra).
        assert_eq!(
            count_intra_blob_inversions(&elements, kind),
            0,
            "kind {kind:?}: --unsort blobs must be internally ID-monotone, \
             blobs were {:?}",
            elements
                .iter()
                .filter(|(k, _)| *k == kind)
                .collect::<Vec<_>>()
        );
    }

    // Element multiset preserved (just reordered).
    let original = read_normalized(input);
    let degraded = read_normalized(output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

/// `--unsort-intra` clears `Sort.Type_then_ID` and produces the intra-blob
/// adversarial shape: exactly one same-kind blob per kind has an internal
/// ID-order inversion, but no adjacent same-kind blob pair overlaps.
///
/// This is the shape that slips past a blob-range overlap check: `sort`
/// decides whether to rewrite by comparing adjacent same-kind blobs'
/// `(min_id, max_id)` ranges, and here every blob's range stays disjoint
/// from its neighbours even though one blob is internally unsorted. So the
/// stream is genuinely out of order while a range-only check sees nothing
/// to fix and the header no longer claims sortedness - a monotonicity
/// blind spot for any consumer that trusts declared sortedness plus
/// non-overlapping ranges.
#[test]
fn degrade_unsort_intra_creates_intra_blob_inversion() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort-intra")
        .arg("--block-cap")
        .arg(UNSORT_CAP)
        .assert_success();

    assert_unsort_intra_shape(&output, &input);
}

/// `--unsort-intra` stays intra-blob even when a single input blob carries
/// more than `--block-cap` same-kind elements (finding 1's regime). The
/// fixture packs 20 elements/blob and the run caps output blocks at 5, so
/// each input blob spans four output blocks. The old shared swap keyed to
/// the cap boundary would have filled and flushed a block here, producing
/// the cross-blob overlap shape; the fix keys the swap to the first two
/// elements so it lands at the start of the first output block.
#[test]
fn degrade_unsort_intra_large_input_blobs_stay_intra_blob() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_large_blob_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort-intra")
        .arg("--block-cap")
        .arg(LARGE_BLOB_CAP)
        .assert_success();

    assert_unsort_intra_shape(&output, &input);
}

/// Shared assertions for the `--unsort-intra` shape: header sortedness
/// cleared, exactly one intra-blob inversion per kind, zero cross-blob
/// overlaps, element multiset preserved.
fn assert_unsort_intra_shape(output: &Path, input: &Path) {
    assert!(
        !read_header(output).is_sorted(),
        "--unsort-intra output must not declare Sort.Type_then_ID"
    );

    let summary = blob_index_summary(output);
    let elements = blob_elements(output);
    for kind in [
        BlobKindLabel::Node,
        BlobKindLabel::Way,
        BlobKindLabel::Relation,
    ] {
        assert_eq!(
            count_intra_blob_inversions(&elements, kind),
            1,
            "kind {kind:?}: expected exactly one intra-blob inversion, \
             blobs were {:?}",
            elements
                .iter()
                .filter(|(k, _)| *k == kind)
                .collect::<Vec<_>>()
        );
        // No cross-blob overlap: this is exactly the shape a blob-range
        // overlap check cannot see.
        assert_eq!(
            count_adjacent_overlaps(&summary, kind),
            0,
            "kind {kind:?}: --unsort-intra must not produce cross-blob overlap, \
             blobs were {:?}",
            summary
                .iter()
                .filter(|(k, ..)| *k == kind)
                .collect::<Vec<_>>()
        );
    }

    // Element multiset preserved (just reordered).
    let original = read_normalized(input);
    let degraded = read_normalized(output);
    assert_eq!(original.nodes, degraded.nodes);
    assert_eq!(original.ways, degraded.ways);
    assert_eq!(original.relations, degraded.relations);
}

/// `--unsort` and `--unsort-intra` are mutually exclusive.
#[test]
fn degrade_unsort_and_unsort_intra_are_mutually_exclusive() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort")
        .arg("--unsort-intra")
        .assert_failure()
        .assert_stderr_contains("unsort-intra");
}

/// `--unsort` output piped through `pbfhogg sort` recovers the original
/// element set with `Sort.Type_then_ID` re-declared. Closes the loop on
/// the design's primary use case: the cross-blob overlap must actually
/// reach `sort`'s overlap-rewrite path.
#[test]
fn degrade_unsort_then_sort_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let unsorted = dir.path().join("unsorted.osm.pbf");
    let resorted = dir.path().join("resorted.osm.pbf");

    write_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&unsorted)
        .arg("--unsort")
        .arg("--block-cap")
        .arg(UNSORT_CAP)
        .assert_success();

    let sort_out = CliInvoker::new()
        .arg("sort")
        .arg(&unsorted)
        .arg("-o")
        .arg(&resorted)
        .arg("--force")
        .assert_success();

    // Prove sort actually hit the overlap-rewrite path rather than passing
    // the file through untouched. Sort prints this line only when
    // detect_overlaps flags at least one blob run for decode + re-encode;
    // the cross-blob overlap --unsort injects is what makes it fire. (A
    // full passthrough would print nothing here.)
    sort_out.assert_stderr_contains("blobs in overlap runs");

    // Prove the output is genuinely sorted in file order, not merely
    // element-equivalent. read_normalized re-sorts every section before
    // comparison, so it would accept a stream that sort left disordered;
    // assert_sorted_file walks the file in blob order and checks the
    // header flag plus per-type monotonicity, catching a passthrough that
    // never repaired the overlap.
    assert_sorted_file(&resorted);

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

/// `--unsort --strip-locations` composes: the cross-blob overlap shape is
/// preserved *and* `LocationsOnWays` is cleared. Confirms the swap logic
/// still fires when the ways go through the coordinate-dropping re-encode.
#[test]
fn degrade_unsort_and_strip_locations_compose() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort")
        .arg("--strip-locations")
        .arg("--block-cap")
        .arg(UNSORT_CAP)
        .assert_success();

    assert!(
        !read_header(&output).has_locations_on_ways(),
        "--strip-locations output must not declare LocationsOnWays"
    );
    assert_unsort_cross_blob_shape(&output, &input);
}

/// `--unsort-intra --strip-locations` composes: the intra-blob inversion
/// shape is preserved *and* `LocationsOnWays` is cleared.
#[test]
fn degrade_unsort_intra_and_strip_locations_compose() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort-intra")
        .arg("--strip-locations")
        .arg("--block-cap")
        .arg(UNSORT_CAP)
        .assert_success();

    assert!(
        !read_header(&output).has_locations_on_ways(),
        "--strip-locations output must not declare LocationsOnWays"
    );
    assert_unsort_intra_shape(&output, &input);
}

/// `--unsort-intra --strip-indexdata` composes: output is intra-blob
/// unsorted *and* unindexed.
#[test]
fn degrade_unsort_intra_and_strip_indexdata_compose() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort-intra")
        .arg("--strip-indexdata")
        .arg("--block-cap")
        .arg(UNSORT_CAP)
        .assert_success();

    assert_non_indexed(&output);
    assert_unsort_intra_shape(&output, &input);
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

/// `--unsort-intra --block-cap 1` is rejected: an intra-blob inversion
/// needs two same-kind elements in one output block, which a cap of 1
/// cannot hold. Rejecting up front avoids a silent no-op that would still
/// clear Sort.Type_then_ID.
#[test]
fn degrade_unsort_intra_rejects_block_cap_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort-intra")
        .arg("--block-cap")
        .arg("1")
        .assert_failure()
        .assert_stderr_contains("block-cap >= 2");
}

/// `--unsort --block-cap 1` is supported (not a silent no-op): each output
/// blob holds one element, and swapping the first two adjacent
/// single-element blobs produces exactly one descending cross-blob step -
/// the same overlap shape sort's detect_overlaps fires on.
#[test]
fn degrade_unsort_accepts_block_cap_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    write_unsort_fixture(&input);

    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--unsort")
        .arg("--block-cap")
        .arg("1")
        .assert_success();

    assert_unsort_cross_blob_shape(&output, &input);
}
