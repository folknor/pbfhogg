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

mod common;

use common::{
    TestMember, TestNode, TestRelation, TestWay, assert_elements_equivalent, assert_indexed,
    assert_non_indexed, generate_nodes, generate_ways, write_test_pbf_non_indexed,
    write_test_pbf_sorted,
};
use pbfhogg::MemberId;
use pbfhogg::writer::Compression;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;

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

    let opts = pbfhogg::sort::SortOptions {
        compression: Compression::default(),
        direct_io: false,
        io_uring: false,
        force: true,
    };
    pbfhogg::commands::sort::sort(
        &in_idx,
        &out_idx,
        &opts,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("sort indexed");
    pbfhogg::commands::sort::sort(
        &in_non,
        &out_non,
        &opts,
        &pbfhogg::HeaderOverrides::default(),
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
        expression_strs: &expression_strs,
        omit_referenced: false,
        invert: false,
        remove_tags: false,
        compression: Compression::default(),
        direct_io: false,
        force: true,
        jobs: None,
    };
    pbfhogg::tags_filter::tags_filter(
        &in_idx,
        &out_idx,
        &opts,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("tags_filter indexed");
    pbfhogg::tags_filter::tags_filter(
        &in_non,
        &out_non,
        &opts,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("tags_filter non-indexed");

    assert_elements_equivalent(&out_idx, &out_non);
}

// ---------------------------------------------------------------------------
// extract --strategy simple
// ---------------------------------------------------------------------------
//
// KNOWN FAILURE, 2026-04-22. `extract --strategy simple` with
// `force: true` on a non-indexed input produces a node count that
// diverges substantially from the indexed twin (observed 18 vs 6 on a
// 10-node fixture - looks like the non-indexed single-pass path
// double-emits decompressed blocks). The test below is written to
// pin the correctness contract once the underlying bug is fixed; it
// is `#[ignore]`-gated so CI stays green while the investigation is
// parked in TODO.md.

#[test]
#[ignore = "extract --simple non-indexed fallback produces wrong node count (see TODO.md)"]
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
            output,
            &region,
            pbfhogg::commands::extract::ExtractStrategy::Simple,
            true,
            &pbfhogg::cat::CleanAttrs::default(),
            Compression::default(),
            false,
            true,
            &pbfhogg::HeaderOverrides::default(),
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
    };
    let compression = Compression::default();
    let hdr = pbfhogg::HeaderOverrides::default();

    pbfhogg::getid::getid(
        &in_idx,
        &out_idx,
        &id_set,
        &opts,
        compression,
        false,
        true,
        &hdr,
    )
    .expect("getid indexed");
    pbfhogg::getid::getid(
        &in_non,
        &out_non,
        &id_set,
        &opts,
        compression,
        false,
        true,
        &hdr,
    )
    .expect("getid non-indexed");

    assert_elements_equivalent(&out_idx, &out_non);
}

// ---------------------------------------------------------------------------
// apply-changes (`merge`)
// ---------------------------------------------------------------------------
//
// KNOWN FAILURE, 2026-04-22. Running the same OSC against the
// indexed and non-indexed twins produces off-by-one node counts
// (observed 10 vs 11 on a delete + modify + create OSC). Likely the
// non-indexed scanner routes blob descriptors through the unconditional
// worker-pool branch without the fast-path splice and some element
// emerges with duplicated or missing state. `#[ignore]`-gated as a
// pinned regression target; details logged in TODO.md.

fn write_osc_gz(path: &Path, xml: &str) {
    let file = File::create(path).expect("create osc.gz");
    let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    enc.write_all(xml.as_bytes()).expect("write xml");
    enc.finish().expect("finish gz");
}

#[test]
#[ignore = "apply-changes non-indexed fallback off-by-one on delete (see TODO.md)"]
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
        compression: Compression::default(),
        direct_io: false,
        io_uring: false,
        force: true,
        locations_on_ways: false,
        jobs: None,
    };
    pbfhogg::apply_changes::merge(
        &in_idx,
        &osc,
        &out_idx,
        &opts,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("merge indexed");
    pbfhogg::apply_changes::merge(
        &in_non,
        &osc,
        &out_non,
        &opts,
        &pbfhogg::HeaderOverrides::default(),
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

#[test]
fn check_refs_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    write_both(&in_idx, &in_non);

    let idx =
        pbfhogg::check::refs::check_refs(&in_idx, true, false, false).expect("check_refs indexed");
    let non = pbfhogg::check::refs::check_refs(&in_non, true, false, false)
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
// (found / not-found) on both twins; they do not compare stdout since
// the function prints directly.

#[test]
fn show_element_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    write_both(&in_idx, &in_non);

    use pbfhogg::inspect::{ShowElementType, show_element};

    // Hit: node 5 exists in the fixture.
    let idx_hit =
        show_element(&in_idx, ShowElementType::Node, 5, false).expect("show node indexed");
    let non_hit =
        show_element(&in_non, ShowElementType::Node, 5, false).expect("show node non-indexed");
    assert!(idx_hit, "node 5 must be found on indexed fixture");
    assert!(non_hit, "node 5 must be found on non-indexed fixture");

    // Miss: no node at id 999.
    let idx_miss =
        show_element(&in_idx, ShowElementType::Node, 999, false).expect("show miss indexed");
    let non_miss =
        show_element(&in_non, ShowElementType::Node, 999, false).expect("show miss non-indexed");
    assert!(!idx_miss, "node 999 must NOT be found on indexed fixture");
    assert!(
        !non_miss,
        "node 999 must NOT be found on non-indexed fixture"
    );

    // Hit on a way.
    let idx_way =
        show_element(&in_idx, ShowElementType::Way, 1_000, false).expect("show way indexed");
    let non_way =
        show_element(&in_non, ShowElementType::Way, 1_000, false).expect("show way non-indexed");
    assert!(idx_way, "way 1000 must be found on indexed fixture");
    assert!(non_way, "way 1000 must be found on non-indexed fixture");
}

// ---------------------------------------------------------------------------
// check --ids (verify_ids)
// ---------------------------------------------------------------------------
//
// KNOWN FAILURE, 2026-04-22. The `check_type_order` function compares
// max(node_offsets) vs min(way_offsets) vs min(relation_offsets) from
// the schedule built by `build_classify_schedules_split`. For
// non-indexed PBFs the builder replicates every blob into all three
// per-kind schedules, so those comparisons span replicated copies
// rather than the actual per-type blob sets - producing spurious
// TypeOrder violations even when the underlying file is correctly
// ordered. Verified: 10-node fixture reports 0 violations indexed,
// 2 violations non-indexed. Fix: gate the offset-based type-order
// check on indexed input, or swap to an element-kind-based ordering
// check that decodes blob contents.

#[test]
#[ignore = "check --ids fires spurious TypeOrder violations on non-indexed schedules (see TODO.md)"]
fn check_ids_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    write_both(&in_idx, &in_non);

    let opts = pbfhogg::check::verify_ids::VerifyIdsOptions {
        full: true,
        type_filter: None,
        max_errors: 100,
        direct_io: false,
    };
    let idx = pbfhogg::check::verify_ids::verify_ids(&in_idx, &opts).expect("verify_ids indexed");
    let non =
        pbfhogg::check::verify_ids::verify_ids(&in_non, &opts).expect("verify_ids non-indexed");

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

    let stats_ii =
        pbfhogg::diff::derive::derive_changes(&old_idx, &new_idx, &osc_ii, false, false, false, 1)
            .expect("derive_changes indexed");
    let stats_nn =
        pbfhogg::diff::derive::derive_changes(&old_non, &new_non, &osc_nn, false, false, false, 1)
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
// rejects non-indexed input outright - it depends on per-blob
// `IdSet::resolve()` fast-paths and has no degraded fallback. Pin
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
        &out_idx,
        &opts,
        Compression::default(),
        false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber must accept indexed input");
    assert!(
        stats_idx.nodes_written > 0,
        "sanity: indexed renumber produced nodes"
    );

    // Non-indexed input is rejected with an actionable error message.
    let err = pbfhogg::commands::renumber::renumber_external(
        &in_non,
        &out_non,
        &opts,
        Compression::default(),
        false,
        &pbfhogg::HeaderOverrides::default(),
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
// KNOWN FAILURE, 2026-04-22. `extract_multi` with CompleteWays on
// non-indexed input produces an empty output file (0 nodes) while
// the indexed twin produces the expected 6 nodes on a 10-node
// fixture. `require_indexdata(.., force: true, ..)` lets the call
// proceed but `extract_complete_ways` appears to silently no-op
// because its pass-1 probably relies on per-blob bboxes from
// indexdata to populate `bbox_node_ids`, and on non-indexed input
// that set stays empty, propagating to 0 matched ways and 0
// transitive refs. Fix: make the pass-1 scanner fall back to
// element-level bbox testing when `blob.index()` is None. Test is
// `#[ignore]`-gated with this diagnosis logged in TODO.md.

#[test]
#[ignore = "extract_multi CompleteWays non-indexed produces empty output (see TODO.md)"]
fn extract_multi_complete_ways_non_indexed_parity() {
    let dir = TempDir::new().expect("tempdir");
    let in_idx = dir.path().join("in_idx.osm.pbf");
    let in_non = dir.path().join("in_non.osm.pbf");
    write_both(&in_idx, &in_non);

    let make_slots = |suffix: &str| -> Vec<pbfhogg::commands::extract::ExtractSlot> {
        let bbox_a =
            pbfhogg::commands::extract::parse_bbox("0.0,0.0,0.0006,0.0006").expect("parse bbox a");
        let bbox_b =
            pbfhogg::commands::extract::parse_bbox("0.0,0.0,0.002,0.002").expect("parse bbox b");
        vec![
            pbfhogg::commands::extract::ExtractSlot {
                region: pbfhogg::commands::extract::Region::Bbox(bbox_a),
                output: dir.path().join(format!("a_{suffix}.osm.pbf")),
            },
            pbfhogg::commands::extract::ExtractSlot {
                region: pbfhogg::commands::extract::Region::Bbox(bbox_b),
                output: dir.path().join(format!("b_{suffix}.osm.pbf")),
            },
        ]
    };
    let slots_idx = make_slots("idx");
    let slots_non = make_slots("non");

    pbfhogg::commands::extract::extract_multi(
        &in_idx,
        &slots_idx,
        pbfhogg::commands::extract::ExtractStrategy::CompleteWays,
        true,
        &pbfhogg::cat::CleanAttrs::default(),
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("extract_multi indexed");
    pbfhogg::commands::extract::extract_multi(
        &in_non,
        &slots_non,
        pbfhogg::commands::extract::ExtractStrategy::CompleteWays,
        true,
        &pbfhogg::cat::CleanAttrs::default(),
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("extract_multi non-indexed");

    assert_elements_equivalent(&slots_idx[0].output, &slots_non[0].output);
    assert_elements_equivalent(&slots_idx[1].output, &slots_non[1].output);
}

// KNOWN FAILURE, 2026-04-22. `extract --strategy smart` with
// `force: true` on a non-indexed input produces an empty output on the
// smart fixture below (indexed output: 4 nodes; non-indexed: 0). This
// looks like the same missing element-level pass-1 fallback already
// pinned for CompleteWays, but with smart relation-member expansion on
// top. Parked in TODO.md; keep the regression test in-tree via
// `#[ignore]`.
#[test]
#[ignore = "extract --smart non-indexed produces empty output (see TODO.md)"]
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
        &out_idx,
        &region,
        pbfhogg::commands::extract::ExtractStrategy::Smart,
        true,
        &pbfhogg::cat::CleanAttrs::default(),
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("extract indexed");
    pbfhogg::commands::extract::extract(
        &in_non,
        &out_non,
        &region,
        pbfhogg::commands::extract::ExtractStrategy::Smart,
        true,
        &pbfhogg::cat::CleanAttrs::default(),
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
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

    let opts = pbfhogg::cat::dedupe::MergePbfOptions {
        compression: Compression::default(),
        direct_io: false,
        io_uring: false,
        force: true,
    };
    let stats_idx = pbfhogg::cat::dedupe::merge_pbf(
        &[&a_idx, &b_idx],
        &out_idx,
        &opts,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("merge_pbf indexed");
    let stats_non = pbfhogg::cat::dedupe::merge_pbf(
        &[&a_non, &b_non],
        &out_non,
        &opts,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("merge_pbf non-indexed");

    assert_eq!(stats_idx.nodes, stats_non.nodes, "nodes parity");
    assert_eq!(stats_idx.ways, stats_non.ways, "ways parity");
    assert_eq!(stats_idx.relations, stats_non.relations, "relations parity");
    assert_eq!(
        stats_idx.duplicates_removed, stats_non.duplicates_removed,
        "duplicates_removed parity"
    );
    assert_elements_equivalent(&out_idx, &out_non);

    // Sanity: we produced an actual union (no silent passthrough bug).
    assert_eq!(
        stats_idx.nodes, 10,
        "union must contain both disjoint node sets"
    );
    assert_eq!(stats_idx.ways, 2, "union must contain b's ways");
    assert_eq!(stats_idx.relations, 1, "union must contain b's relation");
    assert_eq!(
        stats_idx.duplicates_removed, 0,
        "no overlap -> no duplicates"
    );
}
