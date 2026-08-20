//! CLI-driven integration tests for `pbfhogg extract`.
//!
//! Replaces the library-API `tests/extract.rs`. Fixture PBFs are
//! built with the stable-allowlist writers; `extract` runs through
//! `CliInvoker`; output is verified by reading the resulting PBF
//! with the stable-allowlist readers. No imports from
//! `pbfhogg::extract::*` or `pbfhogg::cat::CleanAttrs` - a rewrite
//! of `src/commands/extract/` cannot break these tests by type
//! changes alone.
//!
//! Region types map onto CLI flags:
//!
//! - `Region::Bbox(...)` -> `--bbox <minlon,minlat,maxlon,maxlat>`
//! - `Region::Polygon(...)` -> `--polygon <geojson_path>` (the
//!   polygon, including any holes, is written to a temp geojson file)
//! - Multi-extract slots -> `--config <json_path>` with the same
//!   schema the original `parse_extract_config` accepted.
//!
//! Per-test `ExtractStats` assertions are mostly redundant with
//! element-set assertions and dropped here. The two tests that
//! exist precisely to pin a counter (`smart_includes_boundary_node_members`
//! pinning `nodes_from_relations > 0`, and the tags-preserved test)
//! retain stderr substring assertions.

#![cfg(feature = "commands")]
#![allow(clippy::unwrap_used)]

mod common;

use std::path::Path;

use common::cli::{CliInvoker, CliOutput};
use common::{
    TestMember, TestNode, TestRelation, TestWay, node_ids_id_only as node_ids,
    read_all_elements_id_only as read_all_elements, read_all_elements_with_coords, read_header,
    relation_ids_id_only as relation_ids, way_ids_id_only as way_ids, write_multi_block_test_pbf,
    write_test_pbf, write_test_pbf_sorted,
};
use pbfhogg::MemberId;
use pbfhogg::block_builder::{self, BlockBuilder};
use pbfhogg::writer::{Compression, PbfWriter};
use tempfile::TempDir;

const BBOX_STR: &str = "12.4,55.6,12.7,55.8";

/// Triangle covering nodes 1 and 3 but excluding 2 and 4.
const TRIANGLE_GEOJSON: &str = r#"{
    "type": "Polygon",
    "coordinates": [
        [[12.3, 55.5], [12.8, 55.5], [12.55, 55.9], [12.3, 55.5]]
    ]
}"#;

/// Square exterior with a square hole around node 1.
const SQUARE_WITH_HOLE_GEOJSON: &str = r#"{
    "type": "Polygon",
    "coordinates": [
        [[12.0, 55.0], [13.0, 55.0], [13.0, 56.0], [12.0, 56.0], [12.0, 55.0]],
        [[12.45, 55.68], [12.55, 55.68], [12.55, 55.72], [12.45, 55.72], [12.45, 55.68]]
    ]
}"#;

#[derive(Clone, Copy)]
enum Strategy {
    Simple,
    CompleteWays,
    Smart,
}

enum RegionArg<'a> {
    Bbox(&'a str),
    Polygon(&'a Path),
}

fn test_nodes() -> Vec<TestNode> {
    vec![
        TestNode {
            id: 1,
            lat: 557_000_000,
            lon: 125_000_000,
            tags: vec![("name", "inside1")],
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 540_000_000,
            lon: 125_000_000,
            tags: vec![("name", "outside_south")],
            meta: None,
        },
        TestNode {
            id: 3,
            lat: 556_500_000,
            lon: 125_500_000,
            tags: vec![("name", "inside2")],
            meta: None,
        },
        TestNode {
            id: 4,
            lat: 557_000_000,
            lon: 140_000_000,
            tags: vec![("name", "outside_east")],
            meta: None,
        },
    ]
}

fn test_ways() -> Vec<TestWay> {
    vec![
        TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "primary")],
            meta: None,
        },
        TestWay {
            id: 11,
            refs: vec![2, 4],
            tags: vec![("highway", "secondary")],
            meta: None,
        },
        TestWay {
            id: 12,
            refs: vec![1, 3],
            tags: vec![("highway", "tertiary")],
            meta: None,
        },
    ]
}

fn test_relations() -> Vec<TestRelation> {
    vec![
        TestRelation {
            id: 100,
            members: vec![TestMember {
                id: MemberId::Node(1),
                role: "stop",
            }],
            tags: vec![("type", "route")],
            meta: None,
        },
        TestRelation {
            id: 101,
            members: vec![TestMember {
                id: MemberId::Node(4),
                role: "stop",
            }],
            tags: vec![("type", "route")],
            meta: None,
        },
    ]
}

/// Invoke `pbfhogg extract <input> -o <output> [--bbox|--polygon]
/// [--simple|--smart] --set-bounds --force` and assert success.
fn run_extract(
    input: &Path,
    output: &Path,
    region: &RegionArg<'_>,
    strategy: Strategy,
) -> CliOutput {
    let mut cli = CliInvoker::new()
        .arg("extract")
        .arg(input)
        .arg("-o")
        .arg(output);
    match region {
        RegionArg::Bbox(s) => cli = cli.arg("--bbox").arg(*s),
        RegionArg::Polygon(p) => cli = cli.arg("--polygon").arg(*p),
    }
    match strategy {
        Strategy::Simple => cli = cli.arg("--simple"),
        Strategy::CompleteWays => {}
        Strategy::Smart => cli = cli.arg("--smart"),
    }
    cli.arg("--set-bounds").arg("--force").assert_success()
}

/// Invoke `pbfhogg extract <input> --config <config>
/// [--simple|--smart] --set-bounds --force` and assert success.
fn run_extract_multi(input: &Path, config: &Path, strategy: Strategy) -> CliOutput {
    let mut cli = CliInvoker::new()
        .arg("extract")
        .arg(input)
        .arg("--config")
        .arg(config);
    match strategy {
        Strategy::Simple => cli = cli.arg("--simple"),
        Strategy::CompleteWays => {}
        Strategy::Smart => cli = cli.arg("--smart"),
    }
    cli.arg("--set-bounds").arg("--force").assert_success()
}

fn write_geojson(path: &Path, content: &str) {
    std::fs::write(path, content).expect("write geojson");
}

// ---------------------------------------------------------------------------
// Bbox tests
// ---------------------------------------------------------------------------

#[test]
fn simple_filters_nodes_by_bbox() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf(&input, &test_nodes(), &[], &[]);

    run_extract(
        &input,
        &output,
        &RegionArg::Bbox(BBOX_STR),
        Strategy::Simple,
    );
    let c = read_all_elements(&output);
    assert_eq!(node_ids(&c), vec![1, 3]);
}

#[test]
fn simple_includes_ways_with_nodes_in_bbox() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);

    run_extract(
        &input,
        &output,
        &RegionArg::Bbox(BBOX_STR),
        Strategy::Simple,
    );
    let c = read_all_elements(&output);
    // Ways 10 and 12 have at least one node in bbox; way 11 does not.
    assert_eq!(way_ids(&c), vec![10, 12]);
}

#[test]
fn simple_does_not_add_extra_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);

    run_extract(
        &input,
        &output,
        &RegionArg::Bbox(BBOX_STR),
        Strategy::Simple,
    );
    let c = read_all_elements(&output);
    // Simple mode: only nodes actually in bbox, not way dependencies.
    assert_eq!(node_ids(&c), vec![1, 3]);
}

#[test]
fn complete_ways_includes_all_way_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);

    run_extract(
        &input,
        &output,
        &RegionArg::Bbox(BBOX_STR),
        Strategy::CompleteWays,
    );
    let c = read_all_elements(&output);
    // Way 10 [1,2]: node 1 in bbox -> way matches -> node 2 pulled in
    // Way 12 [1,3]: both in bbox -> no extra deps
    // Way 11 [2,4]: no nodes in bbox -> excluded
    assert_eq!(node_ids(&c), vec![1, 2, 3]);
    assert_eq!(way_ids(&c), vec![10, 12]);
}

#[test]
fn complete_ways_includes_relations() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf(&input, &test_nodes(), &test_ways(), &test_relations());

    run_extract(
        &input,
        &output,
        &RegionArg::Bbox(BBOX_STR),
        Strategy::CompleteWays,
    );
    let c = read_all_elements(&output);
    // Relation 100 has member node 1 (in bbox) -> included
    // Relation 101 has member node 4 (outside bbox) -> excluded
    assert_eq!(relation_ids(&c), vec![100]);
}

#[test]
fn simple_includes_relations_with_matched_ways() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let relations = vec![TestRelation {
        id: 200,
        members: vec![TestMember {
            id: MemberId::Way(10),
            role: "outer",
        }],
        tags: vec![("type", "multipolygon")],
        meta: None,
    }];
    write_test_pbf(&input, &test_nodes(), &test_ways(), &relations);

    run_extract(
        &input,
        &output,
        &RegionArg::Bbox(BBOX_STR),
        Strategy::Simple,
    );
    let c = read_all_elements(&output);
    // Way 10 matched -> relation 200 should be included.
    assert_eq!(relation_ids(&c), vec![200]);
}

#[test]
fn empty_extract() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf(&input, &test_nodes(), &test_ways(), &test_relations());

    // Bbox far away from all test data.
    run_extract(
        &input,
        &output,
        &RegionArg::Bbox("0.0,0.0,1.0,1.0"),
        Strategy::CompleteWays,
    );
    let c = read_all_elements(&output);
    assert!(c.nodes.is_empty());
    assert!(c.ways.is_empty());
    assert!(c.relations.is_empty());
}

#[test]
fn tags_preserved_in_extract() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);

    run_extract(
        &input,
        &output,
        &RegionArg::Bbox(BBOX_STR),
        Strategy::CompleteWays,
    );
    let c = read_all_elements_with_coords(&output);

    // Check node 1 tags.
    let node1 = c
        .nodes
        .iter()
        .find(|(id, _, _, _)| *id == 1)
        .expect("node 1");
    assert_eq!(node1.3, vec![("name".to_string(), "inside1".to_string())]);

    // Check way 10 tags.
    let way10 = c.ways.iter().find(|(id, _, _)| *id == 10).expect("way 10");
    assert_eq!(
        way10.2,
        vec![("highway".to_string(), "primary".to_string())]
    );
}

// ---------------------------------------------------------------------------
// Polygon tests
// ---------------------------------------------------------------------------

#[test]
fn polygon_simple_filters_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let geojson = dir.path().join("region.geojson");

    write_test_pbf(&input, &test_nodes(), &[], &[]);
    write_geojson(&geojson, TRIANGLE_GEOJSON);

    run_extract(
        &input,
        &output,
        &RegionArg::Polygon(&geojson),
        Strategy::Simple,
    );
    let c = read_all_elements(&output);
    assert_eq!(node_ids(&c), vec![1, 3]);
}

#[test]
fn polygon_complete_ways_includes_all_way_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let geojson = dir.path().join("region.geojson");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);
    write_geojson(&geojson, TRIANGLE_GEOJSON);

    run_extract(
        &input,
        &output,
        &RegionArg::Polygon(&geojson),
        Strategy::CompleteWays,
    );
    let c = read_all_elements(&output);
    assert_eq!(node_ids(&c), vec![1, 2, 3]);
    assert_eq!(way_ids(&c), vec![10, 12]);
}

#[test]
fn polygon_with_hole_excludes_interior() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let geojson = dir.path().join("region.geojson");

    write_test_pbf(&input, &test_nodes(), &[], &[]);
    write_geojson(&geojson, SQUARE_WITH_HOLE_GEOJSON);

    run_extract(
        &input,
        &output,
        &RegionArg::Polygon(&geojson),
        Strategy::Simple,
    );
    let c = read_all_elements(&output);
    // Node 1 is inside the hole -> excluded.
    // Node 3 is inside exterior, outside hole -> included.
    // Nodes 2 and 4 are outside the exterior square.
    assert_eq!(node_ids(&c), vec![3]);
}

#[test]
fn polygon_from_geojson_file() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let geojson = dir.path().join("region.geojson");

    write_test_pbf(&input, &test_nodes(), &[], &[]);
    write_geojson(&geojson, TRIANGLE_GEOJSON);

    run_extract(
        &input,
        &output,
        &RegionArg::Polygon(&geojson),
        Strategy::Simple,
    );
    let c = read_all_elements(&output);
    assert_eq!(node_ids(&c), vec![1, 3]);
}

// ---------------------------------------------------------------------------
// Smart strategy tests
// ---------------------------------------------------------------------------

#[test]
fn smart_includes_multipolygon_way_members() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let relations = vec![TestRelation {
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
    }];
    write_test_pbf(&input, &test_nodes(), &test_ways(), &relations);

    run_extract(&input, &output, &RegionArg::Bbox(BBOX_STR), Strategy::Smart);
    let c = read_all_elements(&output);
    // Way 10 matched (has bbox node 1). Way 11 pulled in via smart deps.
    // Way 12 matched normally (both refs in bbox).
    assert_eq!(way_ids(&c), vec![10, 11, 12]);
    // Nodes 1, 3 in bbox. Node 2 from way deps. Node 4 from way 11 smart deps.
    assert_eq!(node_ids(&c), vec![1, 2, 3, 4]);
    assert_eq!(relation_ids(&c), vec![300]);
}

#[test]
fn smart_includes_boundary_node_members() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let relations = vec![TestRelation {
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
    }];
    write_test_pbf(&input, &test_nodes(), &test_ways(), &relations);

    let out = run_extract(&input, &output, &RegionArg::Bbox(BBOX_STR), Strategy::Smart);
    let c = read_all_elements(&output);
    // Node 4 should be pulled in via smart boundary deps.
    assert!(
        node_ids(&c).contains(&4),
        "node 4 (admin_centre) should be included"
    );
    assert_eq!(relation_ids(&c), vec![301]);

    // The smart-contribution branch of `ExtractStats::print_summary`
    // fires precisely when at least one node or way came from a
    // relation. Confirm the summary used that branch.
    assert!(
        out.stderr_str().contains("from relations"),
        "expected smart summary form mentioning 'from relations'; stderr:\n{}",
        out.stderr_str(),
    );
}

#[test]
fn smart_ignores_non_qualifying_relations() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let relations = vec![TestRelation {
        id: 302,
        members: vec![
            TestMember {
                id: MemberId::Way(10),
                role: "forward",
            },
            TestMember {
                id: MemberId::Way(11),
                role: "backward",
            },
        ],
        tags: vec![("type", "route")],
        meta: None,
    }];
    write_test_pbf(&input, &test_nodes(), &test_ways(), &relations);

    run_extract(&input, &output, &RegionArg::Bbox(BBOX_STR), Strategy::Smart);
    let c = read_all_elements(&output);
    // type=route should NOT trigger smart behavior - way 11 stays excluded.
    assert_eq!(way_ids(&c), vec![10, 12]);
    // Relation 302 is still included (it has way 10 as member, which is matched).
    assert_eq!(relation_ids(&c), vec![302]);
    // Nodes: 1,3 in bbox + 2 from way 10 deps. Node 4 NOT included.
    assert_eq!(node_ids(&c), vec![1, 2, 3]);
}

// ---------------------------------------------------------------------------
// Spatial blob filter
// ---------------------------------------------------------------------------

/// Helper: write a PBF with nodes in separate blobs at distinct
/// locations. Each blob is flushed after adding its nodes.
#[allow(clippy::cast_possible_truncation)]
fn write_multi_blob_pbf(path: &Path) {
    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new()
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    // Blob 1: Copenhagen area (lat ~55.7, lon ~12.5)
    for i in 1..=10_i64 {
        bb.add_node(
            i,
            557_000_000 + i as i32 * 1000,
            125_000_000 + i as i32 * 1000,
            std::iter::empty::<(&str, &str)>(),
            None,
        );
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    // Blob 2: Stockholm area (lat ~59.3, lon ~18.0) - far from Copenhagen
    for i in 11..=20_i64 {
        bb.add_node(
            i,
            593_000_000 + i as i32 * 1000,
            180_000_000 + i as i32 * 1000,
            std::iter::empty::<(&str, &str)>(),
            None,
        );
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    // Blob 3: Berlin area (lat ~52.5, lon ~13.4) - far from Copenhagen
    for i in 21..=30_i64 {
        bb.add_node(
            i,
            525_000_000 + i as i32 * 1000,
            134_000_000 + i as i32 * 1000,
            std::iter::empty::<(&str, &str)>(),
            None,
        );
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    writer.flush().expect("flush");
}

#[test]
fn spatial_filter_skips_distant_blobs() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("multi_blob.osm.pbf");
    let output = dir.path().join("extracted.osm.pbf");

    write_multi_blob_pbf(&input);

    run_extract(
        &input,
        &output,
        &RegionArg::Bbox("12.4,55.6,12.7,55.8"),
        Strategy::Simple,
    );
    let c = read_all_elements(&output);

    // Only Copenhagen nodes (1-10) should be in the output.
    let ids = node_ids(&c);
    assert_eq!(ids, (1..=10).collect::<Vec<_>>());
    assert!(
        ids.iter().all(|&id| id <= 10),
        "no non-Copenhagen nodes should be present"
    );
}

/// Write an unsorted PBF whose blobs are in the worst order for a
/// single-pass classifier: ways first, then the nodes those ways
/// reference, then a relation whose only member is one of those ways.
///
/// The header does not declare `Sort.Type_then_ID`, so extract takes its
/// unsorted fallback. Node 1 is inside `BBOX_STR`; node 2 is far south of
/// it. Way 100 therefore belongs in the extract, and relation 300 with it.
fn write_ways_before_nodes_pbf(path: &Path) {
    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new()
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    // Blob 0: ways, before any node has been seen.
    bb.add_way(100, [("highway", "primary")], &[1, 2], None);
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    // Blob 1: the nodes those ways reference. Node 1 is inside BBOX_STR.
    bb.add_node(
        1,
        557_000_000,
        125_000_000,
        std::iter::empty::<(&str, &str)>(),
        None,
    );
    bb.add_node(
        2,
        400_000_000,
        125_000_000,
        std::iter::empty::<(&str, &str)>(),
        None,
    );
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    // Blob 2: a relation whose member way lives in blob 0.
    bb.add_relation(
        300,
        [("type", "route")],
        &[block_builder::MemberData {
            id: MemberId::Way(100),
            role: "outer",
        }],
        None,
    );
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    writer.flush().expect("flush");
}

/// On an unsorted input, a way stored before its nodes must still be
/// extracted - and so must a relation whose member way is.
///
/// The unsorted fallback used to classify nodes, ways and relations in a
/// single file-order sweep, which is only sound when nodes precede ways
/// precede relations. That is the sorted layout, and this is the branch
/// taken when the file is not sorted. Under the old sweep, way 100 was
/// tested against an empty `bbox_node_ids` and relation 300 against an
/// empty `matched_way_ids`: both were silently absent from the output,
/// with no error, no warning and no count. Every strategy shares the
/// fallback, so all three are exercised.
#[test]
fn unsorted_way_before_its_nodes_is_still_extracted() {
    for strategy in [Strategy::Simple, Strategy::CompleteWays, Strategy::Smart] {
        let dir = TempDir::new().expect("tempdir");
        let input = dir.path().join("ways_before_nodes.osm.pbf");
        let output = dir.path().join("output.osm.pbf");
        write_ways_before_nodes_pbf(&input);

        run_extract(&input, &output, &RegionArg::Bbox(BBOX_STR), strategy);
        let c = read_all_elements(&output);

        assert_eq!(
            way_ids(&c),
            vec![100],
            "a way whose blob precedes its nodes must still be matched"
        );
        assert_eq!(
            relation_ids(&c),
            vec![300],
            "a relation whose member way was matched must still be matched"
        );
        // And the classification is still selective: node 2 is far outside
        // the bbox, so this is not satisfied by an extract that keeps
        // everything. Node 2 may be pulled in as a way member under the
        // complete-ways and smart strategies, so only node 1's presence and
        // the way/relation sets are asserted here.
        assert!(
            node_ids(&c).contains(&1),
            "the in-bbox node must be present"
        );
    }
}

// ---------------------------------------------------------------------------
// Sorted-input fast path
// ---------------------------------------------------------------------------
//
// Library tests asserted that the sorted single-pass extract produces
// identical output to the unsorted two-pass path. The CLI surface is
// the same; the strategy / input-shape choice happens internally.

#[test]
fn simple_sorted_filters_nodes_by_bbox() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf_sorted(&input, &test_nodes(), &[], &[]);

    run_extract(
        &input,
        &output,
        &RegionArg::Bbox(BBOX_STR),
        Strategy::Simple,
    );
    let c = read_all_elements(&output);
    assert_eq!(node_ids(&c), vec![1, 3]);
}

#[test]
fn simple_sorted_includes_ways_with_nodes_in_bbox() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &[]);

    run_extract(
        &input,
        &output,
        &RegionArg::Bbox(BBOX_STR),
        Strategy::Simple,
    );
    let c = read_all_elements(&output);
    assert_eq!(way_ids(&c), vec![10, 12]);
}

#[test]
fn simple_sorted_does_not_add_extra_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &[]);

    run_extract(
        &input,
        &output,
        &RegionArg::Bbox(BBOX_STR),
        Strategy::Simple,
    );
    let c = read_all_elements(&output);
    assert_eq!(node_ids(&c), vec![1, 3]);
}

#[test]
fn simple_sorted_includes_relations() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &test_relations());

    run_extract(
        &input,
        &output,
        &RegionArg::Bbox(BBOX_STR),
        Strategy::Simple,
    );
    let c = read_all_elements(&output);
    assert_eq!(relation_ids(&c), vec![100]);
}

#[test]
fn simple_sorted_includes_relations_with_matched_ways() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let relations = vec![TestRelation {
        id: 200,
        members: vec![TestMember {
            id: MemberId::Way(10),
            role: "outer",
        }],
        tags: vec![("type", "multipolygon")],
        meta: None,
    }];
    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &relations);

    run_extract(
        &input,
        &output,
        &RegionArg::Bbox(BBOX_STR),
        Strategy::Simple,
    );
    let c = read_all_elements(&output);
    assert_eq!(relation_ids(&c), vec![200]);
}

#[test]
fn simple_sorted_empty_extract() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &test_relations());

    run_extract(
        &input,
        &output,
        &RegionArg::Bbox("0.0,0.0,1.0,1.0"),
        Strategy::Simple,
    );
    let c = read_all_elements(&output);
    assert!(c.nodes.is_empty());
    assert!(c.ways.is_empty());
    assert!(c.relations.is_empty());
}

#[test]
fn simple_sorted_output_declares_sorted() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &test_relations());

    run_extract(
        &input,
        &output,
        &RegionArg::Bbox(BBOX_STR),
        Strategy::Simple,
    );
    let header = read_header(&output);
    assert!(
        header.is_sorted(),
        "output should declare Sort.Type_then_ID"
    );
}

#[test]
fn simple_sorted_polygon_filters_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let geojson = dir.path().join("region.geojson");

    write_test_pbf_sorted(&input, &test_nodes(), &[], &[]);
    write_geojson(&geojson, TRIANGLE_GEOJSON);

    run_extract(
        &input,
        &output,
        &RegionArg::Polygon(&geojson),
        Strategy::Simple,
    );
    let c = read_all_elements(&output);
    assert_eq!(node_ids(&c), vec![1, 3]);
}

// ---------------------------------------------------------------------------
// Multi-extract via --config
// ---------------------------------------------------------------------------

/// `parse_extract_config` rejects *lexically* identical output paths, but two
/// slots that spell the same file differently slip past it and open two
/// independent writers on one file: the second `File::create` truncates what
/// the first is producing and the two element streams interleave. `sub/..`
/// normalizes away only under canonicalization, which is what the CLI-side
/// pairwise check does.
#[test]
fn multi_extract_rejects_outputs_that_normalize_to_one_path() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let config = dir.path().join("config.json");
    std::fs::create_dir(dir.path().join("sub")).expect("create subdir");

    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &test_relations());

    let config_json = format!(
        r#"{{
            "directory": "{}",
            "extracts": [
                {{ "output": "dup.osm.pbf", "bbox": [12.4, 55.6, 12.7, 55.8] }},
                {{ "output": "sub/../dup.osm.pbf", "bbox": [13.5, 55.5, 14.5, 55.9] }}
            ]
        }}"#,
        dir.path().display(),
    );
    std::fs::write(&config, config_json).expect("write config");

    let result = CliInvoker::new()
        .arg("extract")
        .arg(&input)
        .arg("--config")
        .arg(&config)
        .arg("--simple")
        .arg("--set-bounds")
        .arg("--force")
        .assert_failure();
    result.assert_stderr_contains("resolve to the same file");
    assert!(
        !dir.path().join("dup.osm.pbf").exists(),
        "no writer may be opened once a collision is detected"
    );
}

/// Distinct output *names* can still be one file. The collision check must
/// use the same symlink and hardlink aware notion of identity as the
/// input/output check, not bare path equality.
#[cfg(unix)]
#[test]
fn multi_extract_rejects_symlinked_output_paths() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let config = dir.path().join("config.json");
    let real = dir.path().join("a.osm.pbf");
    let link = dir.path().join("b.osm.pbf");

    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &test_relations());
    std::fs::write(&real, b"placeholder").expect("seed output");
    std::os::unix::fs::symlink(&real, &link).expect("symlink output");

    let config_json = format!(
        r#"{{
            "directory": "{}",
            "extracts": [
                {{ "output": "a.osm.pbf", "bbox": [12.4, 55.6, 12.7, 55.8] }},
                {{ "output": "b.osm.pbf", "bbox": [13.5, 55.5, 14.5, 55.9] }}
            ]
        }}"#,
        dir.path().display(),
    );
    std::fs::write(&config, config_json).expect("write config");

    let result = CliInvoker::new()
        .arg("extract")
        .arg(&input)
        .arg("--config")
        .arg(&config)
        .arg("--simple")
        .arg("--set-bounds")
        .arg("--force")
        .assert_failure();
    result.assert_stderr_contains("resolve to the same file");
    assert_eq!(
        std::fs::read(&real).expect("read seeded output"),
        b"placeholder",
        "the aliased target must not be truncated"
    );
}

/// Publication is a sequence of renames, not a transaction. A destination
/// that cannot be renamed over - here, one that is a directory - must be
/// caught by the preflight so that *nothing* is published, rather than
/// discovered halfway through the commit loop with the earlier destinations
/// already replaced.
///
/// The assertion that bites is the second one: dropping the preflight still
/// fails the command, because the doomed rename returns an error either way.
/// What changes is that the first destination has already been overwritten
/// by then.
#[test]
fn multi_extract_refuses_when_a_destination_cannot_be_published() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let config = dir.path().join("config.json");
    let out_a = dir.path().join("a.osm.pbf");

    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &test_relations());
    std::fs::write(&out_a, b"placeholder").expect("seed first destination");
    // The second destination is a directory: `rename` over it always fails.
    std::fs::create_dir(dir.path().join("b.osm.pbf")).expect("create blocking dir");

    let config_json = format!(
        r#"{{
            "directory": "{}",
            "extracts": [
                {{ "output": "a.osm.pbf", "bbox": [12.4, 55.6, 12.7, 55.8] }},
                {{ "output": "b.osm.pbf", "bbox": [13.5, 55.5, 14.5, 55.9] }}
            ]
        }}"#,
        dir.path().display(),
    );
    std::fs::write(&config, config_json).expect("write config");

    let result = CliInvoker::new()
        .arg("extract")
        .arg(&input)
        .arg("--config")
        .arg(&config)
        .arg("--simple")
        .arg("--set-bounds")
        .arg("--force")
        .assert_failure();
    result.assert_stderr_contains("existing directory");
    assert_eq!(
        std::fs::read(&out_a).expect("read first destination"),
        b"placeholder",
        "no destination may be published when another one cannot be"
    );
    let strays: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .filter(|n| n.to_string_lossy().starts_with(".pbfhogg-tmp-"))
        .collect();
    assert!(strays.is_empty(), "scratch file leaked: {strays:?}");
}

#[test]
fn multi_extract_two_bbox_regions() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let out_a = dir.path().join("a.osm.pbf");
    let out_b = dir.path().join("b.osm.pbf");
    let config = dir.path().join("config.json");

    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &test_relations());

    let config_json = format!(
        r#"{{
            "directory": "{}",
            "extracts": [
                {{ "output": "a.osm.pbf", "bbox": [12.4, 55.6, 12.7, 55.8] }},
                {{ "output": "b.osm.pbf", "bbox": [13.5, 55.5, 14.5, 55.9] }}
            ]
        }}"#,
        dir.path().display(),
    );
    std::fs::write(&config, config_json).expect("write config");

    run_extract_multi(&input, &config, Strategy::Simple);

    // Extract A: nodes 1, 3 in bbox; ways 10, 12 match; relation 100 matches.
    let ca = read_all_elements(&out_a);
    assert_eq!(node_ids(&ca), vec![1, 3]);
    assert_eq!(way_ids(&ca), vec![10, 12]);
    assert_eq!(relation_ids(&ca), vec![100]);

    // Extract B: node 4 in bbox; way 11 refs [2,4] - only node 4 is in
    // B's bbox so way 11 matches. Relation 101 has member node 4.
    let cb = read_all_elements(&out_b);
    assert_eq!(node_ids(&cb), vec![4]);
    assert_eq!(way_ids(&cb), vec![11]);
    assert_eq!(relation_ids(&cb), vec![101]);
}

/// The config path parses bbox floats with serde_json, the CLI `-b` path
/// with std `FromStr`. serde_json's default float parse is best-effort
/// and can land 1 ULP off the correctly-rounded value on 17-digit inputs
/// like 12.639999999999999; after the floor/ceil decimicrodegree
/// conversion that ULP becomes a whole coordinate step, so a node sitting
/// exactly on the boundary silently changes membership between `--config`
/// and `-b` runs of the byte-identical bbox. Found as a persistent 1-node
/// multi-extract-vs-sequential verify failure at a denmark strip boundary
/// (n5446477279, lon 12.6399999, against boundary 12.4 + 4*0.06); fixed
/// by serde_json's `float_roundtrip` feature, which this test pins.
#[test]
fn config_bbox_floats_parse_identically_to_cli_bbox() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let out_cli = dir.path().join("cli.osm.pbf");
    let config = dir.path().join("config.json");

    // Node 1 sits exactly on the boundary decimicro column: 12.6399999
    // degrees = 126399999. The bbox min_lon 12.639999999999999 floors to
    // 126399999 only when parsed correctly rounded (best-effort parsing
    // yields the f64 of 12.64, which floors to 126400000 and drops the
    // node). Node 2 is safely interior so both outputs are non-empty
    // regardless.
    let nodes = vec![
        TestNode {
            id: 1,
            lat: 557_000_000,
            lon: 126_399_999,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 557_000_000,
            lon: 126_500_000,
            tags: vec![],
            meta: None,
        },
    ];
    write_test_pbf_sorted(&input, &nodes, &[], &[]);

    run_extract(
        &input,
        &out_cli,
        &RegionArg::Bbox("12.639999999999999,55.6,12.7,55.8"),
        Strategy::Simple,
    );

    let config_json = format!(
        r#"{{
            "directory": "{}",
            "extracts": [
                {{ "output": "cfg.osm.pbf", "bbox": [12.639999999999999, 55.6, 12.7, 55.8] }}
            ]
        }}"#,
        dir.path().display(),
    );
    std::fs::write(&config, config_json).expect("write config");
    run_extract_multi(&input, &config, Strategy::Simple);

    let cli_elements = read_all_elements(&out_cli);
    let cfg_elements = read_all_elements(&dir.path().join("cfg.osm.pbf"));
    assert_eq!(
        node_ids(&cli_elements),
        vec![1, 2],
        "CLI -b must include the boundary node"
    );
    assert_eq!(
        node_ids(&cfg_elements),
        node_ids(&cli_elements),
        "--config and -b must agree on the byte-identical bbox"
    );
}

#[test]
fn multi_extract_grid_matches_linear_bbox() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let config = dir.path().join("config.json");
    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &test_relations());

    let extracts = (0..20)
        .map(|i| format!(r#"{{"output":"grid-{i}.osm.pbf","bbox":[12.4,55.6,12.7,55.8]}}"#))
        .collect::<Vec<_>>()
        .join(",");
    std::fs::write(
        &config,
        format!(
            r#"{{"directory":"{}","extracts":[{extracts}]}}"#,
            dir.path().display()
        ),
    )
    .expect("write config");

    run_extract_multi(&input, &config, Strategy::Simple);
    for i in 0..20 {
        let output = dir.path().join(format!("grid-{i}.osm.pbf"));
        let extracted = read_all_elements(&output);
        assert_eq!(node_ids(&extracted), vec![1, 3]);
        assert_eq!(way_ids(&extracted), vec![10, 12]);
        assert_eq!(relation_ids(&extracted), vec![100]);
    }
}

#[test]
fn multi_extract_grid_matches_linear_mixed_regions() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let config = dir.path().join("config.json");
    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &test_relations());

    let extracts = (0..20)
        .map(|i| {
            if i % 2 == 0 {
                format!(r#"{{"output":"mixed-{i}.osm.pbf","bbox":[12.4,55.6,12.7,55.8]}}"#)
            } else {
                format!(r#"{{"output":"mixed-{i}.osm.pbf","polygon":{TRIANGLE_GEOJSON}}}"#)
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    std::fs::write(
        &config,
        format!(
            r#"{{"directory":"{}","extracts":[{extracts}]}}"#,
            dir.path().display()
        ),
    )
    .expect("write config");

    run_extract_multi(&input, &config, Strategy::Simple);
    for i in 0..20 {
        let output = dir.path().join(format!("mixed-{i}.osm.pbf"));
        let extracted = read_all_elements(&output);
        assert_eq!(node_ids(&extracted), vec![1, 3]);
        assert_eq!(way_ids(&extracted), vec![10, 12]);
        assert_eq!(relation_ids(&extracted), vec![100]);
    }
}

#[test]
fn multi_extract_from_config_file() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let out_cph = dir.path().join("copenhagen.osm.pbf");
    let out_east = dir.path().join("east.osm.pbf");
    let config = dir.path().join("config.json");

    write_test_pbf_sorted(&input, &test_nodes(), &[], &[]);

    let config_json = format!(
        r#"{{
            "directory": "{}",
            "extracts": [
                {{ "output": "copenhagen.osm.pbf", "bbox": [12.4, 55.6, 12.7, 55.8] }},
                {{ "output": "east.osm.pbf", "bbox": [13.5, 55.5, 14.5, 55.9] }}
            ]
        }}"#,
        dir.path().display(),
    );
    std::fs::write(&config, config_json).expect("write config");

    run_extract_multi(&input, &config, Strategy::Simple);

    let ca = read_all_elements(&out_cph);
    assert_eq!(node_ids(&ca), vec![1, 3]);

    let cb = read_all_elements(&out_east);
    assert_eq!(node_ids(&cb), vec![4]);
}

// ---------------------------------------------------------------------------
// Multi-blob bbox partitioning (raw passthrough)
// ---------------------------------------------------------------------------

#[test]
fn simple_multi_blob_bbox_partitioning() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    // 10 "inside" nodes (lon 12.5, lat 55.65..) + 10 "outside" nodes
    // far south. block_size=10 -> two separate node blobs, exactly the
    // split the per-blob classify is supposed to handle.
    let mut all_nodes: Vec<TestNode> = (0_i32..10)
        .map(|i| TestNode {
            id: i64::from(i) + 1,
            lat: 556_500_000 + i * 100_000,
            lon: 125_000_000,
            tags: vec![],
            meta: None,
        })
        .collect();
    all_nodes.extend((0_i32..10).map(|i| TestNode {
        id: i64::from(i) + 100,
        lat: 400_000_000,
        lon: 500_000_000,
        tags: vec![],
        meta: None,
    }));

    write_multi_block_test_pbf(&input, &all_nodes, &[], &[], 10);

    run_extract(
        &input,
        &output,
        &RegionArg::Bbox(BBOX_STR),
        Strategy::Simple,
    );
    let c = read_all_elements(&output);
    let got: Vec<i64> = node_ids(&c);
    let want: Vec<i64> = (1..=10).collect();
    assert_eq!(got, want, "wrong node set survived the per-blob classify");
}
