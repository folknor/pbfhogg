//! Invariant property tests for `apply_changes::merge`.
//!
//! Captures the current main's behavior on edge cases that the pending
//! descriptor-first streaming rewrite (see `notes/apply-changes-opportunities.md`)
//! must preserve byte-for-byte. These tests MUST pass on current main
//! before the rewrite starts, and must still pass after.
//!
//! Each test pins down a specific correctness invariant from the plan doc's
//! "Correctness invariants" section:
//!
//! - `cursor_rule_*`: Rewrite slots advance `UpsertCursors` past their
//!   blob's ID range; Passthrough/FalsePositive slots do NOT. An upsert
//!   in a false-positive blob's range must appear as a gap create on the
//!   next same-type blob (or as a trailing create if no next blob).
//! - `empty_base_pbf_*`: `last_type == None` forever; trailing creates
//!   must flush all three kinds via the existing `types_to_flush` match.
//! - `locations_on_ways_*`: Missing node refs in OSC ways fall back to
//!   (0, 0) via `element_writes::locations.push((0, 0))`; the merged
//!   `loc_map` resolves in-base + in-diff node IDs correctly.
//!
//! `--direct-io` and `--force` parity tests are harder to exercise in a
//! unit test context (they need a real PBF on disk with indexdata and
//! multi-MB frames to meaningfully differ); those are covered by the
//! cross-validation run against Denmark + Europe byte-equal.

mod common;

use std::fs::File;
use std::io::Write;
use std::path::Path;

use common::{
    read_all_elements_with_coords as read_all_elements,
    write_test_pbf_sorted, TestNode, TestWay,
};
use flate2::write::GzEncoder;
use pbfhogg::apply_changes::{merge, MergeOptions};
use pbfhogg::writer::Compression;
use tempfile::TempDir;

fn write_osc(path: &Path, xml: &str) {
    let file = File::create(path).expect("create osc file");
    let mut enc = GzEncoder::new(file, flate2::Compression::fast());
    enc.write_all(xml.as_bytes()).expect("write xml");
    enc.finish().expect("finish gz");
}

fn run_merge(base: &Path, osc: &Path, output: &Path) {
    merge(
        base,
        osc,
        output,
        &MergeOptions {
            compression: Compression::default(),
            direct_io: false,
            io_uring: false,
            force: true,
            locations_on_ways: false,
        },
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("merge");
}

// ---------------------------------------------------------------------------
// Cursor rule: Passthrough / FalsePositive slots must NOT advance the cursor.
// ---------------------------------------------------------------------------

#[test]
fn cursor_rule_false_positive_blob_emits_create_after() {
    // Base: node blob contains IDs {1, 2, 10} - a gap at 3..=9 means the
    // indexdata range is [1, 10]. Then a way blob forces a type transition.
    //
    // OSC: create node id=5. This lands in the node blob's indexdata
    // range [1, 10] so the fast-path does NOT fire. After decompress +
    // `block_overlaps_diff`, no element matches id=5 → FalsePositive.
    //
    // **The current correctness contract**: the FalsePositive does not
    // advance the cursor and the blob is passed through verbatim
    // (intentionally - rewriting just to interleave a pure create would
    // be wasted work, see `classify.rs::block_overlaps_diff` doc comment).
    // The pending create flows through the type-transition path
    // `flush_remaining_upserts(Node, Way, ...)` and lands AFTER the base
    // node blob's bytes in the output, in blob-tail order rather than
    // OSM-sorted interleaved order.
    //
    // The invariant pinned here is **id=5 must appear in the output**.
    // If a future change mistakenly applies the rewrite-path's cursor
    // advance to FalsePositive slots, the cursor would walk past id=5
    // and the create would be silently dropped.
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 100_000_000, tags: vec![] },
            TestNode { id: 2, lat: 200_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 10, lat: 300_000_000, lon: 300_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 100, refs: vec![1, 2, 10], tags: vec![] },
        ],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="5" lat="40.0" lon="40.0" version="1"/>
  </create>
</osmChange>"#,
    );

    run_merge(&base, &osc, &output);
    let c = read_all_elements(&output);

    // Pin: id=5 is present in the output. The order is blob-tail
    // [1, 2, 10, 5] (current behavior), not OSM-sorted [1, 2, 5, 10].
    let node_ids: Vec<i64> = c.nodes.iter().map(|n| n.0).collect();
    assert!(
        node_ids.contains(&5),
        "FalsePositive cursor rule: id=5 must be present in output. \
         Got nodes {node_ids:?}. If id=5 is missing, the cursor advanced \
         through a FalsePositive blob and dropped the create."
    );
    assert_eq!(
        node_ids,
        vec![1, 2, 10, 5],
        "FalsePositive blob is passed through verbatim, then id=5 emitted \
         on the type transition Node → Way (blob-tail order, not OSM-sorted \
         interleave). If this assertion changes shape (e.g. to [1,2,5,10] \
         OSM-sorted), the rewrite has changed the false-positive policy - \
         intentional or regression? Re-read classify.rs::block_overlaps_diff \
         doc comment."
    );

    // Base way is untouched.
    assert_eq!(
        c.ways.iter().map(|w| w.0).collect::<Vec<_>>(),
        vec![100],
    );
}

#[test]
fn cursor_rule_false_positive_blob_emits_create_at_tail() {
    // Same shape as above but with no subsequent blob of any type - the
    // create must flow through the trailing-creates path (last_type ==
    // Some(Node), flush remaining node upserts at EOF). Output has the
    // create appended after the passthrough blob's elements in blob-tail
    // order: [1, 10, 5], not [1, 5, 10].
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 100_000_000, tags: vec![] },
            TestNode { id: 10, lat: 300_000_000, lon: 300_000_000, tags: vec![] },
        ],
        &[],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="5" lat="40.0" lon="40.0" version="1"/>
  </create>
</osmChange>"#,
    );

    run_merge(&base, &osc, &output);
    let c = read_all_elements(&output);

    let node_ids: Vec<i64> = c.nodes.iter().map(|n| n.0).collect();
    assert!(
        node_ids.contains(&5),
        "FalsePositive cursor rule at tail: id=5 must be present (got {node_ids:?})"
    );
    assert_eq!(
        node_ids,
        vec![1, 10, 5],
        "Trailing-create after FalsePositive blob: blob-tail order [1, 10, 5]"
    );
}

// ---------------------------------------------------------------------------
// Empty base PBF: last_type stays None; trailing creates flush all three.
// ---------------------------------------------------------------------------

#[test]
fn empty_base_pbf_flushes_all_three_kinds() {
    // Base has no OsmData blobs at all (header-only sorted PBF). last_type
    // never gets set. At channel close the existing `types_to_flush` match's
    // `None` arm must flush all three upsert kinds in order Node → Way →
    // Relation.
    //
    // Under the streaming rewrite, the drain actor's close path must honor
    // the same arm. This test pins that invariant.
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(&base, &[], &[], &[]);

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="1" lat="10.0" lon="10.0" version="1"/>
    <node id="2" lat="20.0" lon="20.0" version="1"/>
    <way id="100" version="1">
      <nd ref="1"/>
      <nd ref="2"/>
    </way>
    <relation id="1000" version="1">
      <member type="way" ref="100" role="outer"/>
    </relation>
  </create>
</osmChange>"#,
    );

    run_merge(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(
        c.nodes.iter().map(|n| n.0).collect::<Vec<_>>(),
        vec![1, 2],
        "empty-base trailing creates: nodes 1, 2"
    );
    assert_eq!(
        c.ways.iter().map(|w| w.0).collect::<Vec<_>>(),
        vec![100],
        "empty-base trailing creates: way 100"
    );
    assert_eq!(
        c.relations.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![1000],
        "empty-base trailing creates: relation 1000"
    );
}

#[test]
fn empty_base_pbf_noop_on_empty_diff() {
    // Degenerate edge: empty base + empty diff = empty output. The scanner
    // emits no blobs, workers produce nothing, drain's trailing-creates
    // have no upserts to flush. The output should still be a valid PBF
    // with just the header.
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(&base, &[], &[], &[]);
    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
</osmChange>"#,
    );

    run_merge(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert!(c.nodes.is_empty());
    assert!(c.ways.is_empty());
    assert!(c.relations.is_empty());
}

// ---------------------------------------------------------------------------
// --locations-on-ways: coord resolution via merged loc_map.
//
// NOT exercised here. The merge() entry point requires the base PBF to
// carry the LocationsOnWays header flag (`merge --locations-on-ways
// requires the base PBF to have LocationsOnWays`), which means the
// fixture has to be ALTW-enriched. The existing test fixture helpers
// (`write_test_pbf_sorted` etc.) build vanilla PBFs without the flag,
// and bootstrapping ALTW output in unit-test code is non-trivial.
//
// The two contracts that matter under --locations-on-ways are:
//   - Missing node refs in OSC ways fall back to (0, 0) (see
//     `element_writes.rs`, search `locations.push((0, 0))`).
//   - Merged loc_map resolves both in-base node IDs and in-diff
//     node IDs (post-prefill-fusion: workers extract coords during
//     node-blob decompress, drain merges per-worker maps before
//     dispatching way blobs).
//
// Both contracts are exercised end-to-end by the Denmark byte-equal
// cross-validation: brokkr verify merge --dataset denmark on a real
// ALTW-enriched base PBF + OSC. If those byte-equality checks pass,
// the contracts hold.
//
// If we ever build an ALTW-enriched test fixture helper, port these
// invariants here. Until then: rely on Denmark byte-equal.

// ---------------------------------------------------------------------------
// Trailing creates interleave correctly across kinds.
// ---------------------------------------------------------------------------

#[test]
fn trailing_creates_after_node_blob_flush_way_and_relation() {
    // Base has only node blob. OSC creates a node, a way, and a relation.
    // Expected output order: base nodes, created node (as gap/trailing on
    // nodes), then way, then relation.
    //
    // This exercises the `types_to_flush = [Node, Way, Relation]` expansion
    // that fires on `None | Some(Node)` at close.
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[TestNode { id: 1, lat: 100_000_000, lon: 100_000_000, tags: vec![] }],
        &[],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="2" lat="20.0" lon="20.0" version="1"/>
    <way id="100" version="1">
      <nd ref="1"/>
      <nd ref="2"/>
    </way>
    <relation id="1000" version="1">
      <member type="way" ref="100" role="outer"/>
    </relation>
  </create>
</osmChange>"#,
    );

    run_merge(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(c.nodes.iter().map(|n| n.0).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(c.ways.iter().map(|w| w.0).collect::<Vec<_>>(), vec![100]);
    assert_eq!(c.relations.iter().map(|r| r.0).collect::<Vec<_>>(), vec![1000]);
}

// ---------------------------------------------------------------------------
// Permissive missing-element semantics (see DEVIATIONS.md):
// - <modify> on absent ID silently inserts
// - <delete> on absent ID is a silent no-op
// - <create> on existing ID silently overwrites
// These tests pin the behaviour so any future tightening must be deliberate.
// ---------------------------------------------------------------------------

#[test]
fn modify_on_missing_id_silently_inserts() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[TestNode { id: 1, lat: 100_000_000, lon: 100_000_000, tags: vec![] }],
        &[],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="42" lat="55.0" lon="12.0" version="3"/>
  </modify>
</osmChange>"#,
    );

    run_merge(&base, &osc, &output);
    let c = read_all_elements(&output);

    let node_ids: Vec<i64> = c.nodes.iter().map(|n| n.0).collect();
    assert_eq!(
        node_ids,
        vec![1, 42],
        "modify on absent ID is treated as an insert (DEVIATIONS.md: \
         apply-changes permissive missing-element semantics). If this \
         changes, update DEVIATIONS.md."
    );
}

#[test]
fn delete_on_missing_id_is_noop() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[TestNode { id: 1, lat: 100_000_000, lon: 100_000_000, tags: vec![] }],
        &[],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <delete>
    <node id="42" version="1"/>
  </delete>
</osmChange>"#,
    );

    run_merge(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(
        c.nodes.iter().map(|n| n.0).collect::<Vec<_>>(),
        vec![1],
        "delete on absent ID is a silent no-op (DEVIATIONS.md: \
         apply-changes permissive missing-element semantics). Base \
         node 1 untouched; nothing else emitted."
    );
    assert!(c.ways.is_empty());
    assert!(c.relations.is_empty());
}

#[test]
fn create_on_existing_id_overwrites_base() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[TestNode { id: 42, lat: 100_000_000, lon: 100_000_000, tags: vec![] }],
        &[],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="42" lat="20.0" lon="20.0" version="2"/>
  </create>
</osmChange>"#,
    );

    run_merge(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(
        c.nodes.len(),
        1,
        "create on existing ID must not duplicate: exactly one node \
         with id=42 in output (DEVIATIONS.md: apply-changes permissive \
         missing-element semantics)."
    );
    let n = &c.nodes[0];
    assert_eq!(n.0, 42);
    // OSC record wins: lat=20.0deg = 200_000_000 decimicrodegrees.
    assert_eq!(
        (n.1, n.2),
        (200_000_000, 200_000_000),
        "create on existing ID replaces base record (OSC wins)."
    );
}

#[test]
fn trailing_creates_after_way_blob_flush_relation_only() {
    // Base has node + way blobs. OSC creates a relation only.
    // last_type ends as Some(Way) → types_to_flush = [Way, Relation].
    // The Relation trailing create must fire.
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[TestNode { id: 1, lat: 100_000_000, lon: 100_000_000, tags: vec![] }],
        &[TestWay { id: 10, refs: vec![1], tags: vec![] }],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <relation id="500" version="1">
      <member type="way" ref="10" role="outer"/>
    </relation>
  </create>
</osmChange>"#,
    );

    run_merge(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(c.nodes.iter().map(|n| n.0).collect::<Vec<_>>(), vec![1]);
    assert_eq!(c.ways.iter().map(|w| w.0).collect::<Vec<_>>(), vec![10]);
    assert_eq!(c.relations.iter().map(|r| r.0).collect::<Vec<_>>(), vec![500]);
}
