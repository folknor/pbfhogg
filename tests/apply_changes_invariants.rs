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
    TestNode, TestWay, assert_elements_equivalent, generate_nodes, generate_ways,
    read_all_elements_with_coords as read_all_elements, write_multi_block_test_pbf,
    write_test_pbf_sorted,
};
use flate2::write::GzEncoder;
use pbfhogg::altw::add_locations_to_ways;
use pbfhogg::apply_changes::{MergeOptions, MergeStats, merge};
use pbfhogg::writer::Compression;
use tempfile::TempDir;

fn write_osc(path: &Path, xml: &str) {
    let file = File::create(path).expect("create osc file");
    let mut enc = GzEncoder::new(file, flate2::Compression::fast());
    enc.write_all(xml.as_bytes()).expect("write xml");
    enc.finish().expect("finish gz");
}

fn run_merge(base: &Path, osc: &Path, output: &Path) {
    let _ = run_merge_with_jobs(base, osc, output, None, false);
}

fn run_merge_with_jobs(
    base: &Path,
    osc: &Path,
    output: &Path,
    jobs: Option<usize>,
    locations_on_ways: bool,
) -> MergeStats {
    merge(
        base,
        osc,
        output,
        &MergeOptions {
            compression: Compression::default(),
            direct_io: false,
            io_uring: false,
            force: true,
            locations_on_ways,
            jobs,
            #[cfg(feature = "test-hooks")]
            panic_at_blob_seq: None,
        },
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("merge")
}

#[allow(clippy::cast_possible_wrap)]
fn write_merge_jobs_fixture(base: &Path, osc: &Path) {
    let mut nodes = generate_nodes(24, 1);
    for (i, node) in nodes.iter_mut().enumerate() {
        if i % 4 == 0 {
            node.tags = vec![("name", "base")];
        }
    }

    let mut ways = generate_ways(10, 1_000, 3, 1);
    for (i, way) in ways.iter_mut().enumerate() {
        let start = 1 + i as i64 * 2;
        way.refs = vec![start, start + 1, start + 2];
        way.tags = if i % 2 == 0 {
            vec![("highway", "residential")]
        } else {
            vec![("highway", "service")]
        };
    }

    write_multi_block_test_pbf(base, &nodes, &ways, &[], 4);

    write_osc(
        osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="30" lat="0.3000000" lon="0.6000000" version="1">
      <tag k="created" v="yes"/>
    </node>
    <way id="2000" version="1">
      <nd ref="5"/>
      <nd ref="30"/>
      <nd ref="6"/>
      <tag k="highway" v="primary"/>
    </way>
  </create>
  <modify>
    <node id="5" lat="0.5555555" lon="0.4444444" version="2">
      <tag k="name" v="modified"/>
    </node>
    <way id="1003" version="2">
      <nd ref="7"/>
      <nd ref="5"/>
      <nd ref="30"/>
      <tag k="highway" v="secondary"/>
      <tag k="surface" v="gravel"/>
    </way>
  </modify>
  <delete>
    <node id="23" version="1"/>
    <way id="1007" version="1"/>
  </delete>
</osmChange>"#,
    );
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
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 100_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 200_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 10,
                lat: 300_000_000,
                lon: 300_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 100,
            refs: vec![1, 2, 10],
            tags: vec![],
            meta: None,
        }],
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
    assert_eq!(c.ways.iter().map(|w| w.0).collect::<Vec<_>>(), vec![100],);
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
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 100_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 10,
                lat: 300_000_000,
                lon: 300_000_000,
                tags: vec![],
                meta: None,
            },
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

#[test]
fn merge_jobs_parity_on_multiblob_input() {
    let dir = TempDir::new().expect("tempdir");
    let base_raw = dir.path().join("base_raw.osm.pbf");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let out_seq = dir.path().join("out_seq.osm.pbf");
    let out_par = dir.path().join("out_par.osm.pbf");

    write_merge_jobs_fixture(&base_raw, &osc);
    add_locations_to_ways(
        &base_raw,
        &base,
        true,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::Dense,
    )
    .expect("bootstrap locations-on-ways base");

    let seq = run_merge_with_jobs(&base, &osc, &out_seq, Some(1), true);
    let par = run_merge_with_jobs(&base, &osc, &out_par, Some(4), true);

    assert_eq!(seq.base_nodes, par.base_nodes);
    assert_eq!(seq.base_ways, par.base_ways);
    assert_eq!(seq.base_relations, par.base_relations);
    assert_eq!(seq.diff_nodes, par.diff_nodes);
    assert_eq!(seq.diff_ways, par.diff_ways);
    assert_eq!(seq.diff_relations, par.diff_relations);
    assert_eq!(seq.deleted, par.deleted);

    assert_elements_equivalent(&out_seq, &out_par);
}

#[test]
fn merge_jobs_parity_without_locations_on_ways() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let out_seq = dir.path().join("out_seq.osm.pbf");
    let out_par = dir.path().join("out_par.osm.pbf");

    write_merge_jobs_fixture(&base, &osc);

    let seq = run_merge_with_jobs(&base, &osc, &out_seq, Some(1), false);
    let par = run_merge_with_jobs(&base, &osc, &out_par, Some(4), false);

    assert_eq!(seq.base_nodes, par.base_nodes);
    assert_eq!(seq.base_ways, par.base_ways);
    assert_eq!(seq.base_relations, par.base_relations);
    assert_eq!(seq.diff_nodes, par.diff_nodes);
    assert_eq!(seq.diff_ways, par.diff_ways);
    assert_eq!(seq.diff_relations, par.diff_relations);
    assert_eq!(seq.deleted, par.deleted);

    assert_elements_equivalent(&out_seq, &out_par);
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
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 100_000_000,
            tags: vec![],
            meta: None,
        }],
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
    assert_eq!(
        c.relations.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![1000]
    );
}

// ---------------------------------------------------------------------------
// Permissive missing-element semantics (see reference/osmium-parity.md):
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
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 100_000_000,
            tags: vec![],
            meta: None,
        }],
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
        "modify on absent ID is treated as an insert (reference/osmium-parity.md: \
         apply-changes permissive missing-element semantics). If this \
         changes, update reference/osmium-parity.md."
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
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 100_000_000,
            tags: vec![],
            meta: None,
        }],
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
        "delete on absent ID is a silent no-op (reference/osmium-parity.md: \
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
        &[TestNode {
            id: 42,
            lat: 100_000_000,
            lon: 100_000_000,
            tags: vec![],
            meta: None,
        }],
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
         with id=42 in output (reference/osmium-parity.md: apply-changes permissive \
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
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 100_000_000,
            tags: vec![],
            meta: None,
        }],
        &[TestWay {
            id: 10,
            refs: vec![1],
            tags: vec![],
            meta: None,
        }],
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
    assert_eq!(
        c.relations.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![500]
    );
}

// ---------------------------------------------------------------------------
// Fault-injection canonical test.
//
// Exercises the worker-panic -> scope-join -> scratch-cleanup recovery
// path. Template for sibling tests across every other parallel pipeline
// (altw external stages 3/4, geocode Pass 3 Stage A, diff/derive
// parallel, etc.). See `MergeOptions::with_panic_at_blob_seq` and the
// `test-hooks` Cargo feature.
// ---------------------------------------------------------------------------

/// A worker panic mid-stream surfaces as either a returned Err or a
/// propagated panic (depending on which pipeline stage errors first),
/// the command never silently succeeds, scratch is clean, and the
/// output file is absent-or-truncated (never a zero-filled short file
/// masquerading as valid output).
#[cfg(feature = "test-hooks")]
#[test]
fn fault_injection_worker_panic_surfaces_error_and_leaves_scratch_clean() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    // Multi-blob base: 40 nodes split across blocks of 5 -> 8 node blobs.
    // Enough blobs that the scanner dispatches several candidates before
    // the injected panic fires.
    let nodes = generate_nodes(40, 1);
    write_multi_block_test_pbf(&base, &nodes, &[], &[], 5);

    // OSC that creates node candidates overlapping every base blob.
    // The scanner will route every node blob to the worker pool.
    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="1" lat="0.0000001" lon="0.0000001" version="2"/>
    <node id="10" lat="0.0000010" lon="0.0000010" version="2"/>
    <node id="20" lat="0.0000020" lon="0.0000020" version="2"/>
    <node id="30" lat="0.0000030" lon="0.0000030" version="2"/>
  </modify>
</osmChange>"#,
    );

    // Snapshot the tempdir before the run. The three input files are
    // already present; the output file is not. After the panic we
    // compare and assert nothing beyond those + the output file
    // changed.
    let before = common::snapshot_dir(dir.path());

    // Arm the hook at blob seq 3 (an arbitrary early-middle blob that
    // corresponds to a Candidate dispatched to the worker pool).
    //
    // `jobs: Some(2)` rather than `Some(1)`: with a single worker, its
    // panic leaves no one draining `candidate_rx`, the scanner blocks
    // forever on a full channel, and the command deadlocks. The
    // surviving worker in the 2-worker case keeps consuming so the
    // scanner can finish, the drain then hits its "channel closed
    // with items" path, and the scope surfaces the panic. The
    // `jobs == 1` deadlock is tracked as a separate invariant gap.
    let opts = MergeOptions {
        compression: Compression::default(),
        direct_io: false,
        io_uring: false,
        force: true,
        locations_on_ways: false,
        jobs: Some(2),
        panic_at_blob_seq: Some(3),
    };

    // A worker panic propagates through thread::scope. Depending on
    // whether the drain's "channel closed" check fires first, merge()
    // may either return Err or panic - both are acceptable: the
    // invariant is "does not silently succeed" + "scratch stays clean"
    // + "output is absent-or-truncated-to-zero".
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        merge(&base, &osc, &output, &opts, &pbfhogg::HeaderOverrides::default())
    }));
    let silently_succeeded = matches!(result, Ok(Ok(_)));
    assert!(
        !silently_succeeded,
        "merge must not silently succeed when panic_at_blob_seq fires"
    );

    // Scratch tracking: the only new path relative to `before` is the
    // output file (possibly created). Remove it from the after-set
    // before comparing.
    let mut after = common::snapshot_dir(dir.path());
    after.remove(std::path::Path::new("output.osm.pbf"));
    common::assert_scratch_unchanged(&before, &after);

    // Output file: if it exists at all, must be short enough to be
    // unmistakably incomplete. A full output would be at least the
    // size of the input base; an abandoned stream is either absent
    // or smaller.
    if output.exists() {
        let out_len = std::fs::metadata(&output).expect("stat output").len();
        let base_len = std::fs::metadata(&base).expect("stat base").len();
        assert!(
            out_len < base_len,
            "output ({out_len} bytes) must be truncated relative to base ({base_len} bytes) \
             when a worker panics mid-stream"
        );
    }
}
