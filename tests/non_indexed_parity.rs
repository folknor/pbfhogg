//! Parity tests: non-indexed input with `--force` must produce output
//! element-equivalent to the indexed twin.
//!
//! Several pbfhogg commands check for `BlobHeader.indexdata` via
//! `require_indexdata` at entry and refuse to proceed on non-indexed
//! PBFs unless the caller sets `force: true`. When `--force` is set,
//! the command routes through a slower fallback that cannot rely on
//! per-blob ID ranges or element-kind hints from indexdata. Before
//! these tests there was no coverage for the fallback path on any of
//! these commands; a silent correctness regression on non-indexed
//! input would only surface downstream in a user pipeline.
//!
//! Structure: for each command, build the same logical input twice
//! (indexed via `write_test_pbf_sorted`, non-indexed via
//! `write_test_pbf_non_indexed`), run the command with `force: true`
//! against both, and `assert_elements_equivalent` the outputs.
//! `assert_elements_equivalent` ignores string-table ordering, blob
//! layout, and metadata absence differences; it only compares the
//! element-level content contract.

use crate::common::{
    TestMember, TestNode, TestRelation, TestWay, assert_elements_equivalent, assert_indexed,
    assert_non_indexed, generate_nodes, generate_ways, write_test_pbf_non_indexed,
    write_test_pbf_sorted,
};
use pbfhogg::MemberId;
use pbfhogg::write::WriteOptions;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;

/// Default compression and backend with `force` set: these tests feed the
/// commands non-indexed input on purpose.
fn forced() -> WriteOptions {
    WriteOptions {
        force: true,
        ..WriteOptions::default()
    }
}

// ---------------------------------------------------------------------------
// Shared fixture: a small sorted PBF suitable for every command under test.
// ---------------------------------------------------------------------------

fn shared_nodes() -> Vec<TestNode> {
    let mut nodes = generate_nodes(10, 1);
    // Decorate half the nodes with a matchable tag so tags-filter has
    // something to select.
    for n in &mut nodes[..5] {
        n.tags = vec![("amenity", "cafe")];
    }
    nodes
}

fn shared_ways() -> Vec<TestWay> {
    let mut ways = generate_ways(4, 1_000, 3, 1);
    for (i, w) in ways.iter_mut().enumerate() {
        w.tags = if i % 2 == 0 {
            vec![("highway", "primary")]
        } else {
            vec![("building", "yes")]
        };
    }
    ways
}

fn shared_relations() -> Vec<TestRelation> {
    vec![TestRelation {
        id: 100,
        members: vec![TestMember {
            id: MemberId::Way(1_000),
            role: "outer",
        }],
        tags: vec![("type", "multipolygon")],
        meta: None,
    }]
}

fn write_both(indexed: &Path, non_indexed: &Path) {
    let nodes = shared_nodes();
    let ways = shared_ways();
    let relations = shared_relations();
    write_test_pbf_sorted(indexed, &nodes, &ways, &relations);
    write_test_pbf_non_indexed(non_indexed, &nodes, &ways, &relations);
    assert_indexed(indexed);
    assert_non_indexed(non_indexed);
}

// Only `extract --smart` needs this fixture, and that test is gated on
// `commands`.
#[cfg(feature = "commands")]
fn write_smart_both(indexed: &Path, non_indexed: &Path) {
    let nodes = vec![
        TestNode {
            id: 1,
            lat: 557_000_000,
            lon: 125_000_000,
            tags: vec![("name", "inside-a")],
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 540_000_000,
            lon: 125_000_000,
            tags: vec![("name", "south")],
            meta: None,
        },
        TestNode {
            id: 3,
            lat: 556_500_000,
            lon: 125_500_000,
            tags: vec![("name", "inside-b")],
            meta: None,
        },
        TestNode {
            id: 4,
            lat: 557_000_000,
            lon: 140_000_000,
            tags: vec![("name", "east")],
            meta: None,
        },
    ];
    let ways = vec![
        TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "primary")],
            meta: None,
        },
        TestWay {
            id: 11,
            refs: vec![2, 4],
            tags: vec![("natural", "coastline")],
            meta: None,
        },
        TestWay {
            id: 12,
            refs: vec![1, 3],
            tags: vec![("boundary", "administrative")],
            meta: None,
        },
    ];
    let relations = vec![
        TestRelation {
            id: 300,
            members: vec![
                TestMember {
                    id: MemberId::Way(10),
                    role: "outer",
                },
                TestMember {
                    id: MemberId::Way(11),
                    role: "inner",
                },
            ],
            tags: vec![("type", "multipolygon")],
            meta: None,
        },
        TestRelation {
            id: 301,
            members: vec![
                TestMember {
                    id: MemberId::Way(12),
                    role: "outer",
                },
                TestMember {
                    id: MemberId::Node(4),
                    role: "admin_centre",
                },
            ],
            tags: vec![("type", "boundary"), ("boundary", "administrative")],
            meta: None,
        },
    ];
    write_test_pbf_sorted(indexed, &nodes, &ways, &relations);
    write_test_pbf_non_indexed(non_indexed, &nodes, &ways, &relations);
    assert_indexed(indexed);
    assert_non_indexed(non_indexed);
}

// ---------------------------------------------------------------------------
// sort
// ---------------------------------------------------------------------------

#[test]
fn sort_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    let out_idx = dir.path().join("out_idx.osm.pbf");
    let out_non = dir.path().join("out_non.osm.pbf");
    write_both(&in_idx, &in_non);

    let opts = forced();
    pbfhogg::commands::sort::sort(
        &in_idx,
        crate::common::out(&out_idx),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("sort indexed");
    pbfhogg::commands::sort::sort(
        &in_non,
        crate::common::out(&out_non),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("sort non-indexed");

    assert_elements_equivalent(&out_idx, &out_non);
}

// ---------------------------------------------------------------------------
// tags-filter (two-pass)
// ---------------------------------------------------------------------------

#[test]
fn tags_filter_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    let out_idx = dir.path().join("out_idx.osm.pbf");
    let out_non = dir.path().join("out_non.osm.pbf");
    write_both(&in_idx, &in_non);

    let expression_strs = vec!["w/highway=primary".to_string()];
    let opts = pbfhogg::tags_filter::TagsFilterOptions {
        distrust_metadata: false,
        expression_strs: &expression_strs,
        omit_referenced: false,
        invert: false,
        remove_tags: false,
        write: forced(),
        jobs: None,
    };
    pbfhogg::tags_filter::tags_filter(
        &in_idx,
        crate::common::out(&out_idx),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("tags_filter indexed");
    pbfhogg::tags_filter::tags_filter(
        &in_non,
        crate::common::out(&out_non),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("tags_filter non-indexed");

    assert_elements_equivalent(&out_idx, &out_non);
}

// ---------------------------------------------------------------------------
// extract --strategy simple
// ---------------------------------------------------------------------------
//
// Pins the non-indexed parity contract for `extract --strategy simple`
// under `force: true`. The earlier regression double-emitted elements
// (3x on nodes, 2x on ways, 1x on relations) because simple's three
// sorted-path write phases fed the same non-indexed blob through each
// phase and the kind-agnostic writer re-emitted matching elements as
// the id sets grew. Fixed 2026-04-23 by kind-scoping the pass-2 writer
// (see `extract_block_pass2`'s `phase_kind` parameter).

#[cfg(feature = "commands")] // uses pbfhogg::extract, gated behind `commands`
#[test]
fn extract_simple_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    let out_idx = dir.path().join("out_idx.osm.pbf");
    let out_non = dir.path().join("out_non.osm.pbf");
    write_both(&in_idx, &in_non);

    // A bbox that clips about half the nodes - generate_nodes walks
    // coords 1000..=10_000 decimicrodegrees on both axes, so a bbox
    // clipping at 5000 catches ids 1..=5.
    let bbox = pbfhogg::commands::extract::parse_bbox("0.0,0.0,0.0006,0.0006").expect("parse bbox");
    let region = pbfhogg::commands::extract::Region::Bbox(bbox);

    let extract = |input: &Path, output: &Path| {
        pbfhogg::commands::extract::extract(
            input,
            crate::common::out(output),
            &region,
            pbfhogg::commands::extract::ExtractStrategy::Simple,
            true,
            &pbfhogg::cat::CleanAttrs::default(),
            &forced(),
            &pbfhogg::HeaderOverrides::default(),
            &crate::common::test_host(),
        )
        .expect("extract");
    };
    extract(&in_idx, &out_idx);
    extract(&in_non, &out_non);

    assert_elements_equivalent(&out_idx, &out_non);
}

// ---------------------------------------------------------------------------
// getid (include mode)
// ---------------------------------------------------------------------------

#[test]
fn getid_include_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    let out_idx = dir.path().join("out_idx.osm.pbf");
    let out_non = dir.path().join("out_non.osm.pbf");
    write_both(&in_idx, &in_non);

    let ids_vec = ["n1".to_string(), "n5".to_string(), "w1000".to_string()]
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let id_set = pbfhogg::getid::parse_ids(&ids_vec).expect("parse ids");
    let opts = pbfhogg::getid::GetidOptions {
        add_referenced: false,
        remove_tags: false,
        report_found: false,
    };
    let write = forced();
    let hdr = pbfhogg::HeaderOverrides::default();

    pbfhogg::getid::getid(
        &in_idx,
        crate::common::out(&out_idx),
        &id_set,
        &opts,
        &write,
        &hdr,
        &crate::common::test_host(),
    )
    .expect("getid indexed");
    pbfhogg::getid::getid(
        &in_non,
        crate::common::out(&out_non),
        &id_set,
        &opts,
        &write,
        &hdr,
        &crate::common::test_host(),
    )
    .expect("getid non-indexed");

    assert_elements_equivalent(&out_idx, &out_non);
}

// ---------------------------------------------------------------------------
// apply-changes (`merge`)
// ---------------------------------------------------------------------------
//
// Pins the non-indexed parity contract for `apply-changes --force`.
// The earlier regression over-emitted one node: the scanner emitted
// non-indexed blobs as `DrainItem::Rewritten` carrying the placeholder
// `kind=Node` and `id_range=None` (→ (0, 0) on the wire), so the
// drain's per-kind upsert cursor never advanced past any id. At
// end-of-stream the trailing-creates loop re-emitted every modify the
// worker had already handled inline. Fixed 2026-04-23 by recovering
// the true `(kind, min_id, max_id)` from the parsed block before
// routing it into the drain (`streaming.rs::infer_kind_and_range`).

fn write_osc_gz(path: &Path, xml: &str) {
    let file = File::create(path).expect("create osc.gz");
    let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    enc.write_all(xml.as_bytes()).expect("write xml");
    enc.finish().expect("finish gz");
}

#[test]
fn apply_changes_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let out_idx = dir.path().join("out_idx.osm.pbf");
    let out_non = dir.path().join("out_non.osm.pbf");
    write_both(&in_idx, &in_non);

    // A minimal OSC: modify node 2, delete node 3, create node 100.
    write_osc_gz(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="2" lat="11.0" lon="21.0" version="2"/>
  </modify>
  <delete>
    <node id="3" lat="0" lon="0" version="2"/>
  </delete>
  <create>
    <node id="100" lat="55.0" lon="12.0" version="1"/>
  </create>
</osmChange>
"#,
    );

    let opts = pbfhogg::apply_changes::MergeOptions {
        write: forced(),
        locations_on_ways: false,
        ignore_missing_nodes: false,
        jobs: None,
        allow_version_regression: false,
        #[cfg(feature = "test-hooks")]
        panic_at_blob_seq: None,
    };
    pbfhogg::apply_changes::merge(
        &in_idx,
        &osc,
        crate::common::out(&out_idx),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("merge indexed");
    pbfhogg::apply_changes::merge(
        &in_non,
        &osc,
        crate::common::out(&out_non),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("merge non-indexed");

    assert_elements_equivalent(&out_idx, &out_non);
}

// ---------------------------------------------------------------------------
// check --refs
// ---------------------------------------------------------------------------
//
// `check_refs` uses `build_classify_schedules_split`, which for
// non-indexed blobs replicates the blob into all three per-kind
// schedules (nodes, ways, relations). A decoded non-indexed blob is
// thus processed three times, once per kind - only the matching-kind
// pass contributes to the per-kind counters, but the triple-scan is
// what extract --simple got wrong. Pin that check_refs counts survive
// the triple-scan shape.

#[cfg(feature = "commands")] // uses pbfhogg::check, gated behind `commands`
#[test]
fn check_refs_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    write_both(&in_idx, &in_non);

    let opts = pbfhogg::check::refs::CheckRefsOptions {
        check: pbfhogg::check::CheckOptions {
            max_errors: 0,
            direct_io: false,
        },
        check_relations: true,
        show_ids: false,
    };
    let idx = pbfhogg::check::refs::check_refs(&in_idx, &opts, &crate::common::test_host())
        .expect("check_refs indexed");
    let non = pbfhogg::check::refs::check_refs(&in_non, &opts, &crate::common::test_host())
        .expect("check_refs non-indexed");

    assert_eq!(idx.node_count, non.node_count, "node_count parity");
    assert_eq!(idx.way_count, non.way_count, "way_count parity");
    assert_eq!(
        idx.relation_count, non.relation_count,
        "relation_count parity"
    );
    assert_eq!(
        idx.missing_node_refs, non.missing_node_refs,
        "missing_node_refs parity"
    );
    assert_eq!(
        idx.missing_way_refs, non.missing_way_refs,
        "missing_way_refs parity"
    );
    assert_eq!(
        idx.missing_node_members, non.missing_node_members,
        "missing_node_members parity"
    );
    assert_eq!(
        idx.missing_relation_members, non.missing_relation_members,
        "missing_relation_members parity"
    );
    assert_eq!(idx.is_valid(), non.is_valid(), "is_valid parity");
}

// ---------------------------------------------------------------------------
// inspect --show-id
// ---------------------------------------------------------------------------
//
// `show_element` gates blob-skip on `blob.index()`; non-indexed blobs
// are decompressed and element-scanned unconditionally. Finding the
// target element must work regardless. These tests pin the outcome
// (found / not-found) and the rendered header line on both twins.

#[test]
fn show_element_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    write_both(&in_idx, &in_non);

    use pbfhogg::ElementKind;
    use pbfhogg::inspect::show_element_to;

    // Render one lookup into a buffer: (found, output text).
    let show = |path: &std::path::Path, ty: ElementKind, id: i64| -> (bool, String) {
        let mut out: Vec<u8> = Vec::new();
        let found = show_element_to(path, ty, id, false, &mut out, &crate::common::test_host())
            .expect("show_element_to");
        (found, String::from_utf8(out).expect("utf8 output"))
    };

    // Hit: node 5 exists in the fixture.
    let (idx_hit, idx_text) = show(&in_idx, ElementKind::Node, 5);
    let (non_hit, non_text) = show(&in_non, ElementKind::Node, 5);
    assert!(idx_hit, "node 5 must be found on indexed fixture");
    assert!(non_hit, "node 5 must be found on non-indexed fixture");
    assert!(idx_text.starts_with("node/5\n"), "got {idx_text:?}");
    assert!(non_text.starts_with("node/5\n"), "got {non_text:?}");

    // Miss: no node at id 999.
    let (idx_miss, idx_miss_text) = show(&in_idx, ElementKind::Node, 999);
    let (non_miss, non_miss_text) = show(&in_non, ElementKind::Node, 999);
    assert!(!idx_miss, "node 999 must NOT be found on indexed fixture");
    assert!(
        !non_miss,
        "node 999 must NOT be found on non-indexed fixture"
    );
    assert!(idx_miss_text.is_empty() && non_miss_text.is_empty());

    // Hit on a way.
    let (idx_way, idx_way_text) = show(&in_idx, ElementKind::Way, 1_000);
    let (non_way, non_way_text) = show(&in_non, ElementKind::Way, 1_000);
    assert!(idx_way, "way 1000 must be found on indexed fixture");
    assert!(non_way, "way 1000 must be found on non-indexed fixture");
    assert!(
        idx_way_text.starts_with("way/1000\n"),
        "got {idx_way_text:?}"
    );
    assert!(
        non_way_text.starts_with("way/1000\n"),
        "got {non_way_text:?}"
    );
}

// ---------------------------------------------------------------------------
// check --ids (verify_ids)
// ---------------------------------------------------------------------------
//
// Pins parity between indexed and non-indexed `check --ids --full`
// reports. The earlier regression fired spurious `TypeOrder`
// violations on non-indexed input because the parallel path's
// `check_type_order` compared max/min offsets across the per-kind
// schedules, and `build_classify_schedules_split` replicates every
// non-indexed blob into all three schedules. Fixed 2026-04-23 by
// gating the offset-based check on `indexed`; non-indexed `--full`
// runs skip it and rely on the sequential element-level check for
// the non-`--full` path.

#[cfg(feature = "commands")] // uses pbfhogg::check, gated behind `commands`
#[test]
fn check_ids_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    write_both(&in_idx, &in_non);

    let opts = pbfhogg::check::verify_ids::VerifyIdsOptions {
        check: pbfhogg::check::CheckOptions {
            max_errors: 100,
            direct_io: false,
        },
        full: true,
        type_filter: None,
    };
    let idx = pbfhogg::check::verify_ids::verify_ids(&in_idx, &opts, &crate::common::test_host())
        .expect("verify_ids indexed");
    let non = pbfhogg::check::verify_ids::verify_ids(&in_non, &opts, &crate::common::test_host())
        .expect("verify_ids non-indexed");

    // `indexed` field deliberately WILL differ - that's its whole point.
    // All other report fields must match.
    assert!(idx.indexed, "indexed fixture must self-report as indexed");
    assert!(
        !non.indexed,
        "non-indexed fixture must self-report as non-indexed"
    );

    assert_eq!(idx.header_sorted, non.header_sorted, "header_sorted parity");
    assert_eq!(idx.full, non.full, "full parity");
    assert_eq!(idx.node_count, non.node_count, "node_count parity");
    assert_eq!(idx.way_count, non.way_count, "way_count parity");
    assert_eq!(
        idx.relation_count, non.relation_count,
        "relation_count parity"
    );
    assert_eq!(
        idx.total_violations, non.total_violations,
        "total_violations parity"
    );
    assert_eq!(idx.passed, non.passed, "passed parity");
}

// ---------------------------------------------------------------------------
// derive_changes
// ---------------------------------------------------------------------------
//
// `derive_changes` requires sorted input, accepts non-indexed. The
// sequential (`jobs=1`) path is taken whenever either input lacks
// indexdata. Compare `DeriveChangesStats` on twins. Output is
// `.osc.gz` - not element-compared here; stats parity is the
// meaningful pin.

fn write_derive_pair(old_idx: &Path, old_non: &Path, new_idx: &Path, new_non: &Path) {
    // Old: the shared baseline.
    write_both(old_idx, old_non);

    // New: same 10 nodes as old, but node 2 has a tag changed (modify),
    // node 3 removed (delete), and a new node 100 added (create).
    // One way unchanged; relation unchanged.
    let mut new_nodes = shared_nodes();
    new_nodes[1].tags = vec![("amenity", "restaurant")];
    new_nodes.remove(2);
    new_nodes.push(TestNode {
        id: 100,
        lat: 555_000_000,
        lon: 120_000_000,
        tags: vec![],
        meta: None,
    });
    let ways = shared_ways();
    let relations = shared_relations();
    write_test_pbf_sorted(new_idx, &new_nodes, &ways, &relations);
    write_test_pbf_non_indexed(new_non, &new_nodes, &ways, &relations);
    assert_indexed(new_idx);
    assert_non_indexed(new_non);
}

#[test]
fn derive_changes_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let old_idx = dir.path().join("old_idx.osm.pbf");
    let old_non = dir.path().join("old_non.osm.pbf");
    let new_idx = dir.path().join("new_idx.osm.pbf");
    let new_non = dir.path().join("new_non.osm.pbf");
    let osc_ii = dir.path().join("ii.osc.gz");
    let osc_nn = dir.path().join("nn.osc.gz");
    write_derive_pair(&old_idx, &old_non, &new_idx, &new_non);

    let stats_ii = pbfhogg::diff::derive::derive_changes(
        &old_idx,
        &new_idx,
        crate::common::out(&osc_ii),
        false,
        false,
        None,
        1,
        &crate::common::test_host(),
        crate::common::scratch_in(dir.path()),
    )
    .expect("derive_changes indexed");
    let stats_nn = pbfhogg::diff::derive::derive_changes(
        &old_non,
        &new_non,
        crate::common::out(&osc_nn),
        false,
        false,
        None,
        1,
        &crate::common::test_host(),
        crate::common::scratch_in(dir.path()),
    )
    .expect("derive_changes non-indexed");

    assert_eq!(stats_ii.creates, stats_nn.creates, "creates parity");
    assert_eq!(stats_ii.modifies, stats_nn.modifies, "modifies parity");
    assert_eq!(stats_ii.deletes, stats_nn.deletes, "deletes parity");

    // Sanity: the change set should be non-trivial.
    assert!(
        stats_ii.creates + stats_ii.modifies + stats_ii.deletes > 0,
        "derive change fixture must produce at least one op"
    );
}

// ---------------------------------------------------------------------------
// renumber (external)
// ---------------------------------------------------------------------------
//
// Unlike the commands with a `force: true` bypass, `renumber_external`
// rejects non-indexed input outright - it schedules blobs from their
// indexdata and has no degraded fallback. Pin
// the error message so the contract is explicit and anyone removing
// the check sees this test break.

#[test]
fn renumber_external_rejects_non_indexed() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    let out_idx = dir.path().join("out_idx.osm.pbf");
    let out_non = dir.path().join("out_non.osm.pbf");
    write_both(&in_idx, &in_non);

    let opts = pbfhogg::commands::renumber::RenumberOptions {
        start_node_id: 1,
        start_way_id: 1,
        start_relation_id: 1,
    };

    // Indexed input proceeds normally.
    let stats_idx = pbfhogg::commands::renumber::renumber_external(
        &in_idx,
        crate::common::out(&out_idx),
        &opts,
        &pbfhogg::write::WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("renumber must accept indexed input");
    assert!(
        stats_idx.nodes_written > 0,
        "sanity: indexed renumber produced nodes"
    );

    // Non-indexed input is rejected with an actionable error message.
    let err = pbfhogg::commands::renumber::renumber_external(
        &in_non,
        crate::common::out(&out_non),
        &opts,
        &pbfhogg::write::WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect_err("renumber must reject non-indexed input");
    let msg = err.to_string();
    assert!(
        msg.contains("indexdata") || msg.contains("indexed"),
        "error message must mention indexdata/indexed; got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// extract_multi (CompleteWays strategy)
// ---------------------------------------------------------------------------
//
// Pins the non-indexed parity contract for `extract_multi` under
// the `CompleteWays` strategy. The earlier regression emitted an
// empty file because `collect_pass1_generic`'s sorted path only
// populated per-kind schedules from blobs with indexdata, so on
// a fully non-indexed input all three schedules stayed empty and
// the pass-2 writer emitted nothing. Fixed 2026-04-23 by
// replicating non-indexed blobs into every per-kind schedule
// (see `smart.rs::collect_pass1_generic`); the classify closures
// already kind-filter at the element level, so mismatched blobs
// safely no-op.

#[cfg(feature = "commands")] // uses pbfhogg::extract, gated behind `commands`
#[test]
fn extract_multi_complete_ways_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    write_both(&in_idx, &in_non);

    let regions = || {
        let bbox_a =
            pbfhogg::commands::extract::parse_bbox("0.0,0.0,0.0006,0.0006").expect("parse bbox a");
        let bbox_b =
            pbfhogg::commands::extract::parse_bbox("0.0,0.0,0.002,0.002").expect("parse bbox b");
        vec![
            pbfhogg::commands::extract::Region::Bbox(bbox_a),
            pbfhogg::commands::extract::Region::Bbox(bbox_b),
        ]
    };
    let outputs = |suffix: &str| {
        [
            dir.path().join(format!("a_{suffix}.osm.pbf")),
            dir.path().join(format!("b_{suffix}.osm.pbf")),
        ]
    };
    let outputs_idx = outputs("idx");
    let outputs_non = outputs("non");

    for (input, paths) in [(&in_idx, &outputs_idx), (&in_non, &outputs_non)] {
        pbfhogg::commands::extract::extract_multi(
            input,
            &regions(),
            pbfhogg::OutputSet::files(paths, pbfhogg::Overwrite::Refuse).expect("stage outputs"),
            pbfhogg::commands::extract::ExtractStrategy::CompleteWays,
            true,
            &pbfhogg::cat::CleanAttrs::default(),
            &forced(),
            &pbfhogg::HeaderOverrides::default(),
            &crate::common::test_host(),
        )
        .expect("extract_multi");
    }

    assert_elements_equivalent(&outputs_idx[0], &outputs_non[0]);
    assert_elements_equivalent(&outputs_idx[1], &outputs_non[1]);
}

// Pins the non-indexed parity contract for `extract --strategy smart`
// under `force: true`, including smart's relation-member expansion.
// Shares a root cause with the CompleteWays case above - the sorted
// pass-1 scheduler dropped non-indexed blobs on the floor - and was
// fixed by the same change in `smart.rs::collect_pass1_generic`.
#[cfg(feature = "commands")] // uses pbfhogg::extract, gated behind `commands`
#[test]
fn extract_smart_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    let out_idx = dir.path().join("out_idx.osm.pbf");
    let out_non = dir.path().join("out_non.osm.pbf");
    write_smart_both(&in_idx, &in_non);

    let bbox = pbfhogg::commands::extract::parse_bbox("12.4,55.6,12.7,55.8").expect("parse bbox");
    let region = pbfhogg::commands::extract::Region::Bbox(bbox);

    pbfhogg::commands::extract::extract(
        &in_idx,
        crate::common::out(&out_idx),
        &region,
        pbfhogg::commands::extract::ExtractStrategy::Smart,
        true,
        &pbfhogg::cat::CleanAttrs::default(),
        &forced(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("extract indexed");
    pbfhogg::commands::extract::extract(
        &in_non,
        crate::common::out(&out_non),
        &region,
        pbfhogg::commands::extract::ExtractStrategy::Smart,
        true,
        &pbfhogg::cat::CleanAttrs::default(),
        &forced(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("extract non-indexed");

    assert_elements_equivalent(&out_idx, &out_non);
}

// ---------------------------------------------------------------------------
// merge_pbf (cat --dedupe)
// ---------------------------------------------------------------------------
//
// `merge_pbf` merges N sorted PBFs into one with exact-duplicate
// dedup. `require_indexdata(.., force: true, ..)` gates the
// non-indexed fast-path but allows a slower full-decode fallback.
// Parity: merging (indexed, indexed) vs (non-indexed, non-indexed)
// must produce element-equivalent output. Use A + A as inputs so
// the dedup path fires on every element.

/// Write two disjoint sorted PBFs for merge_pbf. `path_a` contains
/// nodes 1..=5 only; `path_b` contains nodes 6..=10 plus ways 1000,
/// 1001 and relation 100. No overlap, so merge should produce the
/// union with zero duplicates removed.
fn write_disjoint_pair(path_a: &Path, path_b: &Path) {
    let a_nodes: Vec<TestNode> = (0_i32..5)
        .map(|i| TestNode {
            id: i64::from(i) + 1,
            lat: i * 1000,
            lon: i * 1000,
            tags: vec![],
            meta: None,
        })
        .collect();
    write_test_pbf_sorted(path_a, &a_nodes, &[], &[]);

    let b_nodes: Vec<TestNode> = (0_i32..5)
        .map(|i| TestNode {
            id: i64::from(i) + 6,
            lat: (i + 5) * 1000,
            lon: (i + 5) * 1000,
            tags: vec![],
            meta: None,
        })
        .collect();
    let b_ways = vec![
        TestWay {
            id: 1_000,
            refs: vec![6, 7],
            tags: vec![("highway", "primary")],
            meta: None,
        },
        TestWay {
            id: 1_001,
            refs: vec![8, 9, 10],
            tags: vec![("building", "yes")],
            meta: None,
        },
    ];
    let b_rels = vec![TestRelation {
        id: 100,
        members: vec![TestMember {
            id: MemberId::Way(1_000),
            role: "outer",
        }],
        tags: vec![("type", "multipolygon")],
        meta: None,
    }];
    write_test_pbf_sorted(path_b, &b_nodes, &b_ways, &b_rels);
}

#[test]
fn merge_pbf_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");

    // Build indexed twins of a disjoint pair, then rebuild as
    // non-indexed twins via the non-indexed writer's helper by
    // writing custom fixtures. Reuse `write_disjoint_pair` for
    // indexed; inline a non-indexed variant below.
    let a_idx = dir.path().join("a_idx.osm.pbf");
    let b_idx = dir.path().join("b_idx.osm.pbf");
    let a_non = dir.path().join("a_non.osm.pbf");
    let b_non = dir.path().join("b_non.osm.pbf");
    write_disjoint_pair(&a_idx, &b_idx);

    // Rebuild the same content as non-indexed.
    let a_nodes: Vec<TestNode> = (0_i32..5)
        .map(|i| TestNode {
            id: i64::from(i) + 1,
            lat: i * 1000,
            lon: i * 1000,
            tags: vec![],
            meta: None,
        })
        .collect();
    let b_nodes: Vec<TestNode> = (0_i32..5)
        .map(|i| TestNode {
            id: i64::from(i) + 6,
            lat: (i + 5) * 1000,
            lon: (i + 5) * 1000,
            tags: vec![],
            meta: None,
        })
        .collect();
    let b_ways = vec![
        TestWay {
            id: 1_000,
            refs: vec![6, 7],
            tags: vec![("highway", "primary")],
            meta: None,
        },
        TestWay {
            id: 1_001,
            refs: vec![8, 9, 10],
            tags: vec![("building", "yes")],
            meta: None,
        },
    ];
    let b_rels = vec![TestRelation {
        id: 100,
        members: vec![TestMember {
            id: MemberId::Way(1_000),
            role: "outer",
        }],
        tags: vec![("type", "multipolygon")],
        meta: None,
    }];
    write_test_pbf_non_indexed(&a_non, &a_nodes, &[], &[]);
    write_test_pbf_non_indexed(&b_non, &b_nodes, &b_ways, &b_rels);

    let out_idx = dir.path().join("out_idx.osm.pbf");
    let out_non = dir.path().join("out_non.osm.pbf");

    let opts = pbfhogg::write::WriteOptions {
        force: true,
        ..Default::default()
    };
    let stats_idx = pbfhogg::cat::dedupe::merge_pbf(
        &[&a_idx, &b_idx],
        crate::common::out(&out_idx),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("merge_pbf indexed");
    let stats_non = pbfhogg::cat::dedupe::merge_pbf(
        &[&a_non, &b_non],
        crate::common::out(&out_non),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("merge_pbf non-indexed");

    // The indexed side copies its disjoint blobs through undecoded, so its
    // counts are indexdata claims; the non-indexed side scans every blob and
    // observes them. Parity holds on the total, not on the split.
    let total = |s: &pbfhogg::cat::dedupe::MergePbfStats| {
        s.nodes + s.ways + s.relations + s.passthrough_claimed
    };
    assert_eq!(total(&stats_idx), total(&stats_non), "element total parity");
    assert_eq!(
        stats_non.passthrough_claimed, 0,
        "non-indexed input claims nothing"
    );
    assert_eq!(
        stats_idx.duplicates_removed, stats_non.duplicates_removed,
        "duplicates_removed parity"
    );
    assert_elements_equivalent(&out_idx, &out_non);

    // Sanity: we produced an actual union (no silent passthrough bug).
    assert_eq!(
        stats_non.nodes, 10,
        "union must contain both disjoint node sets"
    );
    assert_eq!(stats_non.ways, 2, "union must contain b's ways");
    assert_eq!(stats_non.relations, 1, "union must contain b's relation");
    assert_eq!(
        stats_idx.duplicates_removed, 0,
        "no overlap -> no duplicates"
    );
}
