//! CLI-driven integration tests for `pbfhogg extract`.
//!
//! Replaces the removed library-API `extract.rs` test. Fixture PBFs are
//! built with the stable-allowlist writers; `extract` runs through
//! `CliInvoker`; output is verified by reading the resulting PBF
//! with the stable-allowlist readers. No imports from
//! `pbfhogg::extract::*` or `pbfhogg::cat::CleanAttrs` - a rewrite
//! of `crates/lib/src/commands/extract/` cannot break these tests by type
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

#![allow(clippy::unwrap_used)]

use std::path::Path;

use crate::common::{
    NormalizedMeta, TestMember, TestMeta, TestNode, TestRelation, TestWay,
    node_ids_id_only as node_ids, read_all_elements_id_only as read_all_elements,
    read_all_elements_with_coords, read_header, read_in_file_order, read_normalized,
    relation_ids_id_only as relation_ids, way_ids_id_only as way_ids, write_multi_block_test_pbf,
    write_test_pbf, write_test_pbf_sorted,
};
use crate::invoker::{CliInvoker, CliOutput};
use pbfhogg::MemberId;
use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData};
use pbfhogg::writer::{Compression, PbfWriter};

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

#[derive(Clone, Copy, Debug)]
enum Strategy {
    Simple,
    CompleteWays,
    Smart,
}

enum RegionArg<'a> {
    Bbox(&'a str),
    Polygon(&'a Path),
}

fn test_nodes() -> Vec<TestNode<'static>> {
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

fn test_ways() -> Vec<TestWay<'static>> {
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

fn test_relations() -> Vec<TestRelation<'static>> {
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

/// The strategy flag, plus `--force` for the strategies that refuse an
/// unindexed input (`--simple` never refuses, so it refuses `--force`).
fn strategy_args<'a>(cli: CliInvoker<'a>, strategy: Strategy) -> CliInvoker<'a> {
    match strategy {
        Strategy::Simple => cli.arg("--simple"),
        Strategy::CompleteWays => cli.arg("--force"),
        Strategy::Smart => cli.arg("--smart").arg("--force"),
    }
}

/// Invoke `pbfhogg extract <input> -o <output> [--bbox|--polygon]
/// [--simple|--smart] --set-bounds [--force]` and assert success.
fn run_extract(
    input: &Path,
    output: &Path,
    region: &RegionArg<'_>,
    strategy: Strategy,
) -> CliOutput {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let mut cli = CliInvoker::new(&invocation_scratch)
        .arg("extract")
        .arg(input)
        .arg("-o")
        .arg(output);
    match region {
        RegionArg::Bbox(s) => cli = cli.arg("--bbox").arg(*s),
        RegionArg::Polygon(p) => cli = cli.arg("--polygon").arg(*p),
    }
    strategy_args(cli, strategy)
        .arg("--set-bounds")
        .assert_success()
}

/// Invoke `pbfhogg extract <input> --config <config>
/// [--simple|--smart] --set-bounds [--force]` and assert success.
fn run_extract_multi(input: &Path, config: &Path, strategy: Strategy) -> CliOutput {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let cli = CliInvoker::new(&invocation_scratch)
        .arg("extract")
        .arg(input)
        .arg("--config")
        .arg(config);
    strategy_args(cli, strategy)
        .arg("--set-bounds")
        .assert_success()
}

fn assert_extract_cleans_version(strategy: Strategy, multi: bool) {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    // The sorted, indexed blob is entirely inside the bbox, making it
    // eligible for simple extract's raw passthrough without cleaning.
    write_test_pbf_sorted(
        &input,
        &[TestNode {
            id: 1,
            lat: 557_000_000,
            lon: 125_000_000,
            tags: vec![("name", "inside")],
            meta: Some(TestMeta {
                version: 7,
                timestamp: 0,
                changeset: 0,
                uid: 0,
                user: "",
                visible: true,
            }),
        }],
        &[],
        &[],
    );

    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let config = single_region_config(dir.path());
    let output = dir.path().join("m.osm.pbf");
    let cli = CliInvoker::new(&invocation_scratch)
        .arg("extract")
        .arg(&input);
    let cli = if multi {
        cli.arg("--config").arg(&config)
    } else {
        cli.arg("--bbox").arg(BBOX_STR).arg("-o").arg(&output)
    };
    strategy_args(cli, strategy)
        .args(["--clean", "version"])
        .assert_success();

    let mut expected = read_normalized(&input).nodes;
    // A versionless dense slot treats these zeros as padding. Plain Info
    // preserves the explicit claims and omits the default visibility.
    expected[0].meta = Some(NormalizedMeta {
        version: None,
        milli_timestamp: Some(0),
        changeset: Some(0),
        uid: Some(0),
        user: Some(String::new()),
        visible: None,
    });
    let actual = read_normalized(&output);
    assert_eq!(actual.nodes, expected);
    assert!(actual.ways.is_empty());
    assert!(actual.relations.is_empty());
}

#[test]
fn simple_extract_cleans_version() {
    assert_extract_cleans_version(Strategy::Simple, false);
}

#[test]
fn complete_ways_extract_cleans_version() {
    assert_extract_cleans_version(Strategy::CompleteWays, false);
}

#[test]
fn smart_extract_cleans_version() {
    assert_extract_cleans_version(Strategy::Smart, false);
}

#[test]
fn simple_multi_extract_cleans_version() {
    assert_extract_cleans_version(Strategy::Simple, true);
}

fn write_geojson(path: &Path, content: &str) {
    std::fs::write(path, content).expect("write geojson");
}

// ---------------------------------------------------------------------------
// Bbox tests
// ---------------------------------------------------------------------------

#[test]
fn simple_filters_nodes_by_bbox() {
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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

/// The unsorted simple sweep skips a block by the kinds its elements are,
/// not by its first-field label: an empty dense message in front of ways
/// labels the block as nodes, and skipping it in the way sweep lost the way.
#[test]
fn simple_unsorted_reads_ways_behind_an_empty_dense_message() {
    use crate::common::groups::{Field, block, write_unindexed_blocks};
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_unindexed_blocks(
        &input,
        &[
            block(&[&[Field::Dense(&[1])]]),
            block(&[&[Field::Dense(&[]), Field::Ways(&[10])]]),
        ],
    );
    run_extract(
        &input,
        &output,
        &RegionArg::Bbox("0,0,0.5,0.5"),
        Strategy::Simple,
    );
    let c = read_all_elements(&output);
    assert_eq!(node_ids(&c), vec![1]);
    assert_eq!(way_ids(&c), vec![10]);
}

/// The unsorted simple write pass skips a node blob whose indexdata bbox
/// misses the region, as the sorted route does: a repeated node ID selected
/// from an inside blob is not written again from the outside blob, and the
/// way and relation sets still drive their own blobs.
#[test]
fn simple_unsorted_skips_a_bbox_missed_node_blob_in_the_write_pass() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let mut nodes = inside_nodes(1, 1);
    nodes.push(TestNode {
        id: 1,
        lat: 0,
        lon: 0,
        tags: vec![],
        meta: None,
    });
    let ways = [TestWay {
        id: 10,
        refs: vec![1],
        tags: vec![],
        meta: None,
    }];
    crate::common::write_test_pbf_impl(&input, &nodes, &ways, &[], false, Some(1));
    run_extract(
        &input,
        &output,
        &RegionArg::Bbox(BBOX_STR),
        Strategy::Simple,
    );
    let c = read_all_elements(&output);
    assert_eq!(node_ids(&c), vec![1]);
    assert_eq!(way_ids(&c), vec![10]);
}

#[test]
fn simple_does_not_add_extra_nodes() {
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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

/// A node on a polygon vertex is inside, as in osmium, under every strategy
/// and through `--config`: a boundary exported from OSM has every vertex on a
/// node. Node 2 sits on the triangle's apex, node 3 on its horizontal base
/// (inside: the region lies above it), node 4 one column above the apex,
/// outside.
#[test]
fn polygon_vertex_and_edge_nodes_follow_osmium() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let geojson = dir.path().join("region.geojson");
    let node = |id, lat, lon| TestNode {
        id,
        lat,
        lon,
        tags: vec![],
        meta: None,
    };
    let nodes = vec![
        node(1, 557_000_000, 125_000_000),
        node(2, 559_000_000, 125_500_000),
        node(3, 555_000_000, 126_000_000),
        node(4, 559_000_001, 125_500_000),
    ];
    write_test_pbf_sorted(&input, &nodes, &[], &[]);
    write_geojson(&geojson, TRIANGLE_GEOJSON);
    for (name, strategy) in [
        ("simple", Strategy::Simple),
        ("complete", Strategy::CompleteWays),
        ("smart", Strategy::Smart),
    ] {
        let out = dir.path().join(format!("{name}.osm.pbf"));
        run_extract(&input, &out, &RegionArg::Polygon(&geojson), strategy);
        assert_eq!(node_ids(&read_all_elements(&out)), vec![1, 2, 3], "{name}");
    }
    let config = dir.path().join("config.json");
    std::fs::write(
        &config,
        format!(
            r#"{{"directory":"{}","extracts":[{{"output":"m.osm.pbf","polygon_file":"region.geojson"}}]}}"#,
            dir.path().display(),
        ),
    )
    .expect("write config");
    run_extract_multi(&input, &config, Strategy::Simple);
    assert_eq!(
        node_ids(&read_all_elements(&dir.path().join("m.osm.pbf"))),
        vec![1, 2, 3]
    );
}

#[test]
fn polygon_rejects_degenerate_exterior_without_publishing_output() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let geojson = dir.path().join("region.geojson");

    write_test_pbf(&input, &test_nodes(), &[], &[]);
    write_geojson(
        &geojson,
        r#"{"type":"Polygon","coordinates":[[[12.0,55.0],[13.0,56.0]]] }"#,
    );

    let result = CliInvoker::new(&invocation_scratch)
        .arg("extract")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--polygon")
        .arg(&geojson)
        .arg("--simple")
        .arg("--set-bounds")
        .assert_failed();
    result.assert_stderr_contains("at least 3");
    assert!(
        !output.exists(),
        "invalid polygon input must not produce an empty output"
    );
}

// ---------------------------------------------------------------------------
// Smart strategy tests
// ---------------------------------------------------------------------------

#[test]
fn smart_includes_multipolygon_way_members() {
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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

    // The smart-contribution branch of `ExtractStats::write_summary`
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
    let dir = crate::common::test_scratch();
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

/// Smart judges a relation by its first `type` tag, as osmium does.
#[test]
fn smart_completion_follows_the_first_type_tag() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let relations = vec![TestRelation {
        id: 310,
        members: vec![
            TestMember {
                id: MemberId::Way(10),
                role: "outer",
            },
            TestMember {
                id: MemberId::Way(11),
                role: "outer",
            },
        ],
        tags: vec![("type", "route"), ("type", "multipolygon")],
        meta: None,
    }];
    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &relations);
    run_extract(&input, &output, &RegionArg::Bbox(BBOX_STR), Strategy::Smart);
    let c = read_all_elements(&output);
    assert_eq!(way_ids(&c), vec![10, 12]);
    assert_eq!(relation_ids(&c), vec![310]);
}

fn member_relation(id: i64) -> TestMember {
    TestMember {
        id: MemberId::Relation(id),
        role: "",
    }
}

/// Relations around the matched route 100 (member node 1, in the region):
/// a parent, a grandparent, a parent stored before its child, a cycle
/// through a self-member, a multipolygon parent that also lists way 11, a
/// parent of the unmatched route 101, and a parent of an absent relation.
fn parent_relations() -> Vec<TestRelation<'static>> {
    let rel = |id: i64, members: Vec<TestMember>, kind: &'static str| TestRelation {
        id,
        members,
        tags: vec![("type", kind)],
        meta: None,
    };
    let mut relations = test_relations();
    relations.extend([
        rel(50, vec![member_relation(401)], "superroute"),
        rel(400, vec![member_relation(100)], "route_master"),
        rel(401, vec![member_relation(400)], "superroute"),
        rel(402, vec![member_relation(101)], "route_master"),
        rel(
            403,
            vec![member_relation(403), member_relation(401)],
            "network",
        ),
        rel(
            404,
            vec![
                member_relation(100),
                TestMember {
                    id: MemberId::Way(11),
                    role: "outer",
                },
            ],
            "multipolygon",
        ),
        rel(405, vec![member_relation(999)], "collection"),
    ]);
    relations.sort_by_key(|r| r.id);
    relations
}

/// complete-ways and smart include every relation that has an included
/// relation as a member, recursively and regardless of file order, as
/// osmium's do; a parent pulls in nothing else, and smart does not complete
/// a parent. Simple has no relation-to-relation step, also as osmium.
#[test]
fn complete_and_smart_include_parent_relations_recursively() {
    let dir = crate::common::test_scratch();
    for (layout, sorted) in [("sorted", true), ("unsorted", false)] {
        let input = dir.path().join(format!("{layout}.osm.pbf"));
        if sorted {
            write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &parent_relations());
        } else {
            write_test_pbf(&input, &test_nodes(), &test_ways(), &parent_relations());
        }
        for (name, strategy) in [
            ("complete", Strategy::CompleteWays),
            ("smart", Strategy::Smart),
        ] {
            let out = dir.path().join(format!("{layout}-{name}.osm.pbf"));
            run_extract(&input, &out, &RegionArg::Bbox(BBOX_STR), strategy);
            let c = read_all_elements(&out);
            assert_eq!(
                relation_ids(&c),
                vec![50, 100, 400, 401, 403, 404],
                "{layout} {name}"
            );
            assert_eq!(way_ids(&c), vec![10, 12], "{layout} {name}");
        }
        let out = dir.path().join(format!("{layout}-simple.osm.pbf"));
        run_extract(&input, &out, &RegionArg::Bbox(BBOX_STR), Strategy::Simple);
        assert_eq!(
            relation_ids(&read_all_elements(&out)),
            vec![100],
            "{layout}"
        );
    }
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
    let header = HeaderBuilder::new().build().expect("build header");
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
    let dir = crate::common::test_scratch();
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
    let header = HeaderBuilder::new().build().expect("build header");
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
        &[MemberData {
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
        let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
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

    let result = CliInvoker::new(&invocation_scratch)
        .arg("extract")
        .arg(&input)
        .arg("--config")
        .arg(&config)
        .arg("--simple")
        .arg("--set-bounds")
        .assert_failed();
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
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
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

    let result = CliInvoker::new(&invocation_scratch)
        .arg("extract")
        .arg(&input)
        .arg("--config")
        .arg(&config)
        .arg("--simple")
        .arg("--set-bounds")
        .assert_failed();
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
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
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

    let result = CliInvoker::new(&invocation_scratch)
        .arg("extract")
        .arg(&input)
        .arg("--config")
        .arg(&config)
        .arg("--simple")
        .arg("--set-bounds")
        .arg("--overwrite")
        .assert_failed();
    // Refused before any extraction, even with overwrite permission: a
    // directory is never replaced by a file output.
    result.assert_stderr_contains("is not a regular file");
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

/// A single-pass multi-extract holds a writer per region beside each
/// region's staging descriptor, so a config with more regions than the
/// open-file limit leaves room for is refused up front, naming the counts
/// and the remedy, with nothing published and no staging file left behind.
/// At a limit of 64, 40 regions' staging descriptors fit, but their 40
/// writers on top do not: without the admission the run fails with EMFILE
/// partway through the writers instead.
#[cfg(target_os = "linux")]
#[test]
fn multi_extract_refuses_more_regions_than_the_descriptor_limit_holds() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    const REGIONS: usize = 40;
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let config = dir.path().join("config.json");
    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &test_relations());

    let extracts: Vec<String> = (0..REGIONS)
        .map(|i| format!(r#"{{ "output": "r{i}.osm.pbf", "bbox": [12.4, 55.6, 12.7, 55.8] }}"#))
        .collect();
    let config_json = format!(
        r#"{{ "directory": "{}", "extracts": [{}] }}"#,
        dir.path().display(),
        extracts.join(", ")
    );
    std::fs::write(&config, config_json).expect("write config");

    let result = CliInvoker::new(&invocation_scratch)
        .arg("extract")
        .arg(&input)
        .arg("--config")
        .arg(&config)
        .arg("--simple")
        .open_file_limit(64)
        .assert_failed();
    result.assert_stderr_contains("need 49 descriptors");
    result.assert_stderr_contains("extract fewer regions per call");
    for i in 0..REGIONS {
        assert!(
            !dir.path().join(format!("r{i}.osm.pbf")).exists(),
            "region {i} was published"
        );
    }
    let strays: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .filter(|n| n.to_string_lossy().starts_with(".pbfhogg-tmp-"))
        .collect();
    assert!(strays.is_empty(), "staging file leaked: {strays:?}");
}

#[test]
fn multi_extract_two_bbox_regions() {
    let dir = crate::common::test_scratch();
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

#[test]
fn multi_extract_diagnostics_use_final_output_paths() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let config = dir.path().join("config.json");
    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &test_relations());

    let simple_output = dir.path().join("simple.osm.pbf");
    std::fs::write(
        &config,
        format!(
            r#"{{"directory":"{}","extracts":[{{"output":"simple.osm.pbf","bbox":[12.4,55.6,12.7,55.8]}}]}}"#,
            dir.path().display()
        ),
    )
    .expect("write config");
    let simple = run_extract_multi(&input, &config, Strategy::Simple);
    let simple_stderr = simple.stderr_str();
    assert!(
        simple_stderr.contains(simple_output.to_string_lossy().as_ref()),
        "the summary should report the final simple output path; stderr:\n{simple_stderr}"
    );
    assert_eq!(
        simple_stderr.matches("simple.osm.pbf").count(),
        1,
        "single-pass multi-extract should report each output once; stderr:\n{simple_stderr}"
    );
    assert!(
        !simple_stderr.contains(".pbfhogg-tmp-"),
        "single-pass diagnostics must not expose scratch paths; stderr:\n{simple_stderr}"
    );

    // Every strategy takes the single pass on sorted input, so the
    // sequential control is an unsorted one.
    let unsorted = dir.path().join("unsorted.osm.pbf");
    write_test_pbf(&unsorted, &test_nodes(), &test_ways(), &test_relations());
    let sequential_output = dir.path().join("sequential.osm.pbf");
    std::fs::write(
        &config,
        format!(
            r#"{{"directory":"{}","extracts":[{{"output":"sequential.osm.pbf","bbox":[12.4,55.6,12.7,55.8]}}]}}"#,
            dir.path().display()
        ),
    )
    .expect("write config");
    let sequential = run_extract_multi(&unsorted, &config, Strategy::CompleteWays);
    let sequential_stderr = sequential.stderr_str();
    assert!(
        sequential_stderr.contains(&format!("Extracting to {}", sequential_output.display())),
        "sequential diagnostics should report the final output path; stderr:\n{sequential_stderr}"
    );
    assert!(
        !sequential_stderr.contains(".pbfhogg-tmp-"),
        "sequential diagnostics must not expose scratch paths; stderr:\n{sequential_stderr}"
    );
}

#[test]
fn multi_extract_config_paths_resolve_from_config_and_directory_override() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let config_dir = dir.path().join("config");
    let configured_dir = config_dir.join("configured");
    let override_dir = dir.path().join("override");
    let external_dir = dir.path().join("external");
    std::fs::create_dir(&config_dir).expect("create config directory");
    std::fs::create_dir(&configured_dir).expect("create configured output directory");
    std::fs::create_dir(configured_dir.join("nested")).expect("create configured nested directory");
    std::fs::create_dir(&override_dir).expect("create override directory");
    std::fs::create_dir(override_dir.join("nested")).expect("create override nested directory");
    std::fs::create_dir(&external_dir).expect("create external output directory");

    let input = dir.path().join("input.osm.pbf");
    let config = config_dir.join("extract.json");
    let relative_output = override_dir.join("nested/relative.osm.pbf");
    let absolute_output = external_dir.join("absolute.osm.pbf");
    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &test_relations());

    std::fs::write(
        &config,
        format!(
            r#"{{"directory":"configured","extracts":[
                {{"output":"nested/relative.osm.pbf","bbox":[12.4,55.6,12.7,55.8]}},
                {{"output":"{}","bbox":[13.5,55.5,14.5,55.9]}}
            ]}}"#,
            absolute_output.display()
        ),
    )
    .expect("write config");

    let result = CliInvoker::new(&invocation_scratch)
        .arg("extract")
        .arg(&input)
        .arg("--config")
        .arg(&config)
        .arg("--directory")
        .arg(&override_dir)
        .arg("--simple")
        .arg("--set-bounds")
        .assert_success();

    assert!(relative_output.exists());
    assert!(override_dir.join("absolute.osm.pbf").exists());
    assert!(!configured_dir.join("nested/relative.osm.pbf").exists());
    assert!(!absolute_output.exists());
    assert!(
        result
            .stderr_str()
            .contains(relative_output.to_string_lossy().as_ref()),
        "summary should report the rebased relative output; stderr:\n{}",
        result.stderr_str()
    );

    let config_relative_dir = config_dir.join("relative-dir.json");
    let resolved_dir = config_dir.join("relative-outputs");
    std::fs::create_dir(&resolved_dir).expect("create config-relative output directory");
    std::fs::write(
        &config_relative_dir,
        r#"{"directory":"relative-outputs","extracts":[{"output":"anchored.osm.pbf","bbox":[12.4,55.6,12.7,55.8]}]}"#,
    )
    .expect("write relative directory config");
    run_extract_multi(&input, &config_relative_dir, Strategy::Simple);
    assert!(resolved_dir.join("anchored.osm.pbf").exists());
}

/// Under `-d`, a relative `--config` still anchors the configured output
/// directory, so an absolute output below it keeps its subdirectory (the
/// configured directory itself need not exist), and an output that would
/// leave the override is refused before anything is written.
#[test]
fn multi_extract_directory_override_keeps_subpaths_and_refuses_escapes() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let root = dir.path().canonicalize().expect("absolute scratch");
    std::fs::create_dir(root.join("cfg")).expect("create config directory");
    let override_dir = root.join("new");
    std::fs::create_dir_all(override_dir.join("part")).expect("create override");
    let input = root.join("input.osm.pbf");
    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &test_relations());

    let nested = root.join("cfg/out/part/a.osm.pbf");
    std::fs::write(
        root.join("cfg/extract.json"),
        format!(
            r#"{{"directory":"out","extracts":[{{"output":"{}","bbox":[12.4,55.6,12.7,55.8]}}]}}"#,
            nested.display()
        ),
    )
    .expect("write config");
    CliInvoker::new(&invocation_scratch)
        .current_dir(&root)
        .arg("extract")
        .arg(&input)
        .arg("--config")
        .arg("cfg/extract.json")
        .arg("--directory")
        .arg(&override_dir)
        .arg("--simple")
        .assert_success();
    assert!(override_dir.join("part/a.osm.pbf").exists());
    assert!(!override_dir.join("a.osm.pbf").exists());

    std::fs::write(
        root.join("cfg/escape.json"),
        r#"{"extracts":[{"output":"../escaped.osm.pbf","bbox":[12.4,55.6,12.7,55.8]}]}"#,
    )
    .expect("write escaping config");
    let result = CliInvoker::new(&invocation_scratch)
        .current_dir(&root)
        .arg("extract")
        .arg(&input)
        .arg("--config")
        .arg("cfg/escape.json")
        .arg("--directory")
        .arg(&override_dir)
        .arg("--simple")
        .assert_failed();
    assert!(
        result
            .stderr_str()
            .contains("would leave the output directory"),
        "stderr:\n{}",
        result.stderr_str()
    );
    assert!(!root.join("escaped.osm.pbf").exists());
}

/// `-b` and `--config` quantize a 17-digit edge to the same column.
/// "12.639999999999999" names 126399999.99999999 decimicrodegrees, which
/// rounds to column 126400000, so node 1 on that column is inside under
/// both inputs. Each edge is quantized from its own token, so this no
/// longer depends on how either parser rounds the token to an f64;
/// `a_half_step_edge_quantizes_from_its_digits_in_cli_and_config` is the
/// case where the f64 route would pick a different column.
#[test]
fn config_bbox_floats_parse_identically_to_cli_bbox() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let out_cli = dir.path().join("cli.osm.pbf");
    let config = dir.path().join("config.json");

    // Node 1 sits on the edge column. Node 2 is safely interior.
    let nodes = vec![
        TestNode {
            id: 1,
            lat: 557_000_000,
            lon: 126_400_000,
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
        "the edge column must be inside"
    );
    assert_eq!(
        node_ids(&cfg_elements),
        node_ids(&cli_elements),
        "--config and -b must agree on the byte-identical bbox"
    );
}

/// A node exactly on a typed decimal edge is inside, and the column next to
/// it is outside - at a boundary where f64 arithmetic cannot be trusted.
///
/// `556_000_000 as f64 * 1e-7` is strictly below `55.6`, so a membership
/// test on that product excludes a node sitting on the 55.6 edge, while
/// osmium, which quantizes the typed decimal, includes it. Every edge is
/// quantized to the decimicrodegree its text names and compared
/// inclusively. A fixture at a round boundary such as 55.0 cannot see the
/// difference.
///
/// This has to hold at every layer that decides membership -
/// element-by-element classification, the columnar bbox classifier used by
/// complete-ways, `smart` and multi-extract, and the raw-blob passthrough
/// proof, which copies a whole blob through untouched.
#[test]
fn bbox_edge_is_the_typed_decimal_column_inclusive() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");

    // Node 1 sits exactly on 55.6. Node 2 is one column below it, outside.
    // Node 3 is interior, so no output is trivially empty.
    let nodes = vec![
        TestNode {
            id: 1,
            lat: 556_000_000,
            lon: 126_500_000,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 555_999_999,
            lon: 126_500_000,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 3,
            lat: 557_000_000,
            lon: 126_500_000,
            tags: vec![],
            meta: None,
        },
    ];
    write_test_pbf_sorted(&input, &nodes, &[], &[]);

    for (name, strategy) in [
        ("simple", Strategy::Simple),
        ("smart", Strategy::Smart),
        ("complete", Strategy::CompleteWays),
    ] {
        let out = dir.path().join(format!("{name}.osm.pbf"));
        run_extract(
            &input,
            &out,
            &RegionArg::Bbox("12.4,55.6,12.7,55.8"),
            strategy,
        );
        assert_eq!(
            node_ids(&read_all_elements(&out)),
            vec![1, 3],
            "{name} must include the node on the 55.6 edge and nothing below it"
        );
    }

    // Without node 2 the blob's bbox touches the edge exactly, so the simple
    // strategy nominates it for raw passthrough; the proof must agree that
    // the edge node is inside.
    let edge_only = dir.path().join("edge-only.osm.pbf");
    let edge_nodes: Vec<TestNode> = nodes.into_iter().filter(|n| n.id != 2).collect();
    write_test_pbf_sorted(&edge_only, &edge_nodes, &[], &[]);
    let out = dir.path().join("edge-only-out.osm.pbf");
    let result = run_extract(
        &edge_only,
        &out,
        &RegionArg::Bbox("12.4,55.6,12.7,55.8"),
        Strategy::Simple,
    );
    assert_eq!(node_ids(&read_all_elements(&out)), vec![1, 3]);
    let stderr = result.stderr_str();
    assert!(
        !stderr.contains("filtered instead of copied raw"),
        "an edge-touching blob must verify for passthrough: {stderr}"
    );

    let config = dir.path().join("config.json");
    std::fs::write(
        &config,
        format!(
            r#"{{
                "directory": "{}",
                "extracts": [
                    {{ "output": "m.osm.pbf", "bbox": [12.4, 55.6, 12.7, 55.8] }}
                ]
            }}"#,
            dir.path().display(),
        ),
    )
    .expect("write config");
    run_extract_multi(&input, &config, Strategy::Simple);
    assert_eq!(
        node_ids(&read_all_elements(&dir.path().join("m.osm.pbf"))),
        vec![1, 3],
        "multi-extract must include the node on the 55.6 edge"
    );
}

/// An edge typed with more than seven decimals is quantized from its own
/// digits, half away from zero, identically from `-b` text and from a
/// `--config` number: `1.00000115` names column 10000012. Its nearest f64
/// times 1e7 is 10000011.499999998, so any route through the f64 would
/// pick 10000011 and admit node 1 below the edge.
#[test]
fn a_half_step_edge_quantizes_from_its_digits_in_cli_and_config() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let nodes = vec![
        TestNode {
            id: 1,
            lat: 10_000_000,
            lon: 10_000_011,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 10_000_000,
            lon: 10_000_012,
            tags: vec![],
            meta: None,
        },
    ];
    write_test_pbf_sorted(&input, &nodes, &[], &[]);
    let out = dir.path().join("cli.osm.pbf");
    run_extract(
        &input,
        &out,
        &RegionArg::Bbox("1.00000115,0.5,2,2"),
        Strategy::Simple,
    );
    assert_eq!(node_ids(&read_all_elements(&out)), vec![2]);

    let config = dir.path().join("config.json");
    std::fs::write(
        &config,
        format!(
            r#"{{"directory":"{}","extracts":[{{"output":"cfg.osm.pbf","bbox":[1.00000115,0.5,2,2]}}]}}"#,
            dir.path().display(),
        ),
    )
    .expect("write config");
    run_extract_multi(&input, &config, Strategy::Simple);
    assert_eq!(
        node_ids(&read_all_elements(&dir.path().join("cfg.osm.pbf"))),
        vec![2]
    );
}

#[test]
fn multi_extract_grid_matches_linear_bbox() {
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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

// ---------------------------------------------------------------------------
// Raw passthrough must not trust indexdata
// ---------------------------------------------------------------------------

/// `nodes` inside `BBOX_STR`, ids starting at `start`.
fn inside_nodes(count: i32, start: i64) -> Vec<TestNode<'static>> {
    (0..count)
        .map(|i| TestNode {
            id: start + i64::from(i),
            lat: 556_500_000 + i * 50_000,
            lon: 125_000_000,
            tags: vec![],
            meta: None,
        })
        .collect()
}

/// The `BlobHeader.indexdata` bytes of frame `frame` (0 is the OSMHeader).
fn indexdata_of(pbf: &[u8], frame: usize) -> Vec<u8> {
    let mut captured = Vec::new();
    let _ = crate::common::adversarial::mutate_blob_header_indexdata(pbf, frame, |ix| {
        captured.clone_from(ix);
    });
    captured
}

fn single_region_config(dir: &Path) -> std::path::PathBuf {
    let config = dir.join("config.json");
    std::fs::write(
        &config,
        format!(
            r#"{{"directory":"{}","extracts":[{{"output":"m.osm.pbf","bbox":[12.4,55.6,12.7,55.8]}}]}}"#,
            dir.display()
        ),
    )
    .expect("write config");
    config
}

/// A raw-passthrough blob used to add its indexdata `count` to "N in bbox":
/// an unverified claim reported as an observation. `--corrupt-indexdata`
/// inflates every count by 1 000 000 while leaving the bbox honest, so both
/// node blobs still pass through - the output carries their headers
/// verbatim - and the summary must still say 20.
#[test]
fn passthrough_counts_observed_nodes_not_the_indexdata_count() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_multi_block_test_pbf(&honest, &inside_nodes(20, 1), &[], &[], 10);
    CliInvoker::new(&invocation_scratch)
        .arg("degrade")
        .arg(&honest)
        .arg("-o")
        .arg(&input)
        .arg("--corrupt-indexdata")
        .assert_success();

    let out = run_extract(
        &input,
        &output,
        &RegionArg::Bbox(BBOX_STR),
        Strategy::Simple,
    );
    assert_eq!(
        node_ids(&read_all_elements(&output)),
        (1..=20).collect::<Vec<_>>()
    );
    assert!(
        out.stderr_str()
            .contains("Extract (simple): 20 nodes (20 in bbox, 0 from ways)"),
        "the summary must count decoded nodes, not the inflated claim; stderr:\n{}",
        out.stderr_str()
    );
    // The blobs really were copied raw, so this exercised the passthrough
    // count rather than the decode path.
    let input_bytes = std::fs::read(&input).expect("read input");
    let output_bytes = std::fs::read(&output).expect("read output");
    for frame in [1, 2] {
        assert_eq!(
            indexdata_of(&output_bytes, frame),
            indexdata_of(&input_bytes, frame),
            "node blob {frame} should have been passed through verbatim"
        );
    }

    let config = single_region_config(dir.path());
    let out = run_extract_multi(&input, &config, Strategy::Simple);
    assert_eq!(
        node_ids(&read_all_elements(&dir.path().join("m.osm.pbf"))),
        (1..=20).collect::<Vec<_>>()
    );
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("node blobs: 2 passthrough, 0 decoded"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("Extract (simple): 20 nodes (20 in bbox, 0 from ways)"),
        "multi-extract must count decoded nodes too; stderr:\n{stderr}"
    );
}

/// A region's raw copy is decided per region, as its single extract
/// decides it. Each node blob's payload gains a 37-nanodegree `lat_offset`,
/// finer than the decimicrodegree a rebuilt block keeps, with its indexdata
/// bbox claim left honest. The wide region holds both blobs and the narrow
/// one only the first, so the second blob is a candidate for the wide region
/// alone: it must still be copied raw there (its framing kept) and rebuilt
/// for the narrow one. Run on the fused route, then on the classify-then-
/// write route (blob 2's claimed ID range widened to overlap blob 1's, an
/// inexact claim that route neither relies on nor checks), and with a
/// polygon region beside the bboxes.
#[test]
fn multi_extract_copies_a_blob_raw_to_each_region_containing_it() {
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    write_multi_block_test_pbf(&honest, &inside_nodes(20, 1), &[], &[], 10);
    let pbf = std::fs::read(&honest).expect("read fixture");
    // The offset moves each blob's northmost node 37 nanodegrees north, so
    // its claimed max latitude (bytes 30..34) grows one decimicrodegree to
    // stay the outward-rounded bound.
    let offset = |pbf: &[u8], blob: usize| {
        let pbf = crate::common::adversarial::mutate_blob_payload(pbf, blob, |p| {
            p.extend_from_slice(&[0x98, 0x01]);
            crate::common::adversarial::write_varint(p, 37);
        });
        crate::common::adversarial::mutate_blob_header_indexdata(&pbf, blob, |ix| {
            let max_lat = i32::from_le_bytes(ix[30..34].try_into().unwrap()) + 1;
            ix[30..34].copy_from_slice(&max_lat.to_le_bytes());
        })
    };
    let framed = offset(&offset(&pbf, 1), 2);
    let fused = dir.path().join("fused.osm.pbf");
    std::fs::write(&fused, &framed).expect("write fused input");
    let overlapping = dir.path().join("overlapping.osm.pbf");
    std::fs::write(
        &overlapping,
        crate::common::adversarial::forge_indexdata_range(&framed, 2, 1, 20, None),
    )
    .expect("write overlapping input");
    let wide = "12.4,55.6,12.7,55.8";
    let narrow = "12.4,55.6,12.7,55.7";
    for (tag, input) in [("fused", &fused), ("two-phase", &overlapping)] {
        let outputs = assert_multi_matches_singles(
            dir.path(),
            input,
            tag,
            &[wide, narrow],
            Strategy::Simple,
            &[],
        );
        let input_bytes = std::fs::read(input).expect("read input");
        let wide_bytes = std::fs::read(&outputs[0]).expect("read wide output");
        for frame in [1, 2] {
            assert_eq!(
                indexdata_of(&wide_bytes, frame),
                indexdata_of(&input_bytes, frame),
                "{tag}: node blob {frame} is copied raw to the wide region"
            );
        }

        // A polygon region beside the wide bbox takes the element-wise
        // classifier, which must verify the bbox region's candidates too.
        let invocation_scratch = pbfhogg_test_scratch::scratch();
        let polygon = dir.path().join(format!("{tag}-triangle.geojson"));
        std::fs::write(&polygon, TRIANGLE_GEOJSON).expect("write polygon");
        let multi_wide = dir.path().join(format!("{tag}-mixed-wide.osm.pbf"));
        let multi_poly = dir.path().join(format!("{tag}-mixed-poly.osm.pbf"));
        let config = dir.path().join(format!("{tag}-mixed.json"));
        std::fs::write(
            &config,
            format!(
                r#"{{"extracts":[{{"output":"{}","bbox":[{wide}]}},{{"output":"{}","polygon_file":"{}"}}]}}"#,
                multi_wide.display(),
                multi_poly.display(),
                polygon.display(),
            ),
        )
        .expect("write config");
        CliInvoker::new(&invocation_scratch)
            .arg("extract")
            .arg(input)
            .arg("--config")
            .arg(&config)
            .args(["--simple", "--set-bounds"])
            .assert_success();
        assert_eq!(
            read_in_file_order(&multi_wide),
            read_in_file_order(&outputs[0]),
            "{tag}: the wide region beside a polygon equals its single extract"
        );
        let mixed_bytes = std::fs::read(&multi_wide).expect("read mixed wide output");
        assert_eq!(
            indexdata_of(&mixed_bytes, 2),
            indexdata_of(&input_bytes, 2),
            "{tag}: node blob 2 is copied raw beside a polygon region"
        );
    }
}

/// Sorted simple extract decodes each blob once when the kind's ID ranges
/// are disjoint, deciding each ID inside its blob; that rests on the ranges,
/// so a range that is false for its blob is refused and nothing is
/// published, both a range wider than the blob (still disjoint) and a count
/// that disagrees.
#[test]
fn simple_extract_refuses_an_inexact_id_range_it_relies_on() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    write_multi_block_test_pbf(&honest, &inside_nodes(20, 1), &[], &[], 10);
    let pbf = std::fs::read(&honest).expect("read fixture");
    for (name, min, max, count) in [("wide", 11, 25, None), ("count", 11, 20, Some(9))] {
        let input = dir.path().join(format!("{name}.osm.pbf"));
        let output = dir.path().join(format!("{name}-out.osm.pbf"));
        let forged = crate::common::adversarial::forge_indexdata_range(&pbf, 2, min, max, count);
        std::fs::write(&input, forged).expect("write forged fixture");
        let out = CliInvoker::new(&invocation_scratch)
            .arg("extract")
            .arg(&input)
            .arg("-o")
            .arg(&output)
            .arg("--bbox")
            .arg(BBOX_STR)
            .arg("--simple")
            .assert_failed();
        out.assert_stderr_contains("this run places and counts the blob by its claim");
        assert!(!output.exists(), "{name}: nothing is published");
    }
}

/// Multi-extract's fused kind pass relies on the same disjoint ID ranges and
/// refuses the same inexact claims, publishing no region.
#[test]
fn multi_extract_refuses_an_inexact_id_range_it_relies_on() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    write_multi_block_test_pbf(&honest, &inside_nodes(20, 1), &[], &[], 10);
    let pbf = std::fs::read(&honest).expect("read fixture");
    let config = single_region_config(dir.path());
    for (name, min, max, count) in [("wide", 11, 25, None), ("count", 11, 20, Some(9))] {
        let input = dir.path().join(format!("{name}.osm.pbf"));
        let forged = crate::common::adversarial::forge_indexdata_range(&pbf, 2, min, max, count);
        std::fs::write(&input, forged).expect("write forged fixture");
        let out = CliInvoker::new(&invocation_scratch)
            .arg("extract")
            .arg(&input)
            .arg("--config")
            .arg(&config)
            .arg("--simple")
            .assert_failed();
        out.assert_stderr_contains("this run places and counts the blob by its claim");
        assert!(
            !dir.path().join("m.osm.pbf").exists(),
            "{name}: nothing is published"
        );
    }
}

/// A blob whose indexdata bbox claims containment while its nodes are
/// outside used to be copied raw on the claim alone, putting out-of-region
/// nodes in the extract. The classify phase now verifies every candidate
/// against its decoded payload and filters the ones that fail.
#[test]
fn a_lying_indexdata_bbox_does_not_smuggle_nodes_through() {
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let mut nodes = inside_nodes(10, 1);
    nodes.extend((0_i64..10).map(|i| TestNode {
        id: 100 + i,
        lat: 400_000_000,
        lon: 500_000_000,
        tags: vec![],
        meta: None,
    }));
    write_multi_block_test_pbf(&honest, &nodes, &[], &[], 10);
    // Give the outside blob (frame 2) the inside blob's (frame 1) index,
    // bbox included.
    let pbf = std::fs::read(&honest).expect("read fixture");
    let inside_index = indexdata_of(&pbf, 1);
    let lying = crate::common::adversarial::mutate_blob_header_indexdata(&pbf, 2, |ix| {
        ix.clone_from(&inside_index);
    });
    std::fs::write(&input, lying).expect("write lying fixture");

    let want: Vec<i64> = (1..=10).collect();
    let out = run_extract(
        &input,
        &output,
        &RegionArg::Bbox(BBOX_STR),
        Strategy::Simple,
    );
    assert_eq!(
        node_ids(&read_all_elements(&output)),
        want,
        "the outside nodes must not ride in on the lying bbox"
    );
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("Extract (simple): 10 nodes (10 in bbox, 0 from ways)"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("filtered instead of copied raw"),
        "the failed verification must be reported; stderr:\n{stderr}"
    );

    let config = single_region_config(dir.path());
    let out = run_extract_multi(&input, &config, Strategy::Simple);
    assert_eq!(
        node_ids(&read_all_elements(&dir.path().join("m.osm.pbf"))),
        want,
        "multi-extract must not trust the lying bbox either"
    );
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("Extract (simple): 10 nodes (10 in bbox, 0 from ways)"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("filtered instead of copied raw"),
        "stderr:\n{stderr}"
    );
}

/// A passthrough candidate whose block the decoder refuses must not reach
/// the output through a raw copy. The first node blob's string table gets
/// an invalid UTF-8 entry: its nodes, its dense-only shape and its honest
/// indexdata bbox all qualify it for the copy, and only the block validator
/// refuses it. Single-region and multi-extract both fail with the
/// decoder's error and publish nothing.
#[test]
fn a_candidate_the_decoder_refuses_is_not_copied_raw() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    let input = dir.path().join("input.osm.pbf");
    write_multi_block_test_pbf(&honest, &inside_nodes(20, 1), &[], &[], 10);
    let pbf = std::fs::read(&honest).expect("read fixture");
    let broken = crate::common::adversarial::mutate_blob_payload(&pbf, 1, |payload| {
        // The string table, holding its one empty entry, leads the block.
        assert_eq!(payload[..4], [10, 2, 10, 0], "fixture string table");
        payload.splice(..4, [10, 3, 10, 1, 0xFF]);
    });
    std::fs::write(&input, broken).expect("write broken fixture");
    let refusal = "invalid UTF-8 at string table index 0";

    let output = dir.path().join("single.osm.pbf");
    let out = CliInvoker::new(&invocation_scratch)
        .arg("extract")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .args(["--bbox", BBOX_STR, "--simple"])
        .assert_failed();
    out.assert_stderr_contains(refusal);
    assert!(!output.exists(), "nothing is published");

    let config = multi_config(dir.path(), "multi-refused");
    let out = CliInvoker::new(&invocation_scratch)
        .arg("extract")
        .arg(&input)
        .arg("--config")
        .arg(&config)
        .args(["--simple", "--set-bounds"])
        .assert_failed();
    out.assert_stderr_contains(refusal);
    assert!(
        !dir.path().join("multi-refused.osm.pbf").exists(),
        "nothing is published"
    );
}

/// A sorted input whose one data blob holds two plain (non-dense) Node
/// messages, (id, lat, lon) = (1, 10, 20) and (2, 30, 40) decimicrodegrees,
/// default granularity, no indexdata.
fn write_plain_node_pbf(input: &Path) {
    let mut writer = PbfWriter::new(
        std::fs::File::create(input).expect("create input"),
        Compression::default(),
    );
    writer
        .write_header(&HeaderBuilder::new().sorted().build().expect("header"))
        .expect("write header");
    // Empty string-table entry, then the two plain Node messages.
    let block = [
        10, 2, 10, 0, 18, 16, 10, 6, 8, 2, 64, 20, 72, 40, 10, 6, 8, 4, 64, 60, 72, 80,
    ];
    writer
        .write_primitive_block(&block)
        .expect("write plain nodes");
    writer.flush().expect("flush");
}

/// complete-ways and smart classify a sorted bbox input's nodes from dense
/// columns, which leave plain Node messages out; those must still be
/// classified, or an in-region plain node and everything it would have
/// matched is silently missing.
#[test]
fn complete_and_smart_classify_plain_nodes_on_sorted_input() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let input = dir.path().join("plain.osm.pbf");
    write_plain_node_pbf(&input);
    for (name, strategy) in [
        ("complete", Strategy::CompleteWays),
        ("smart", Strategy::Smart),
    ] {
        for (case, bbox, expected) in [
            ("all", "--bbox=-1,-1,1,1", vec![1, 2]),
            ("partial", "--bbox=0,0,0.0000025,0.0000025", vec![1]),
        ] {
            let out = dir.path().join(format!("{name}-{case}.osm.pbf"));
            let cli = CliInvoker::new(&invocation_scratch)
                .arg("extract")
                .arg(&input)
                .arg("-o")
                .arg(&out)
                .arg(bbox);
            strategy_args(cli, strategy).assert_success();
            assert_eq!(
                node_ids(&read_all_elements(&out)),
                expected,
                "{name} {case} lost plain nodes"
            );
        }
    }
}

#[test]
fn multi_bbox_classifies_plain_nodes_with_and_without_passthrough_candidates() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let input = dir.path().join("plain.osm.pbf");
    write_plain_node_pbf(&input);
    let single = dir.path().join("single.osm.pbf");
    CliInvoker::new(&invocation_scratch)
        .args(["extract", "--simple", "--bbox=-1,-1,1,1"])
        .arg(&input)
        .arg("-o")
        .arg(&single)
        .assert_success();
    assert_eq!(node_ids(&read_all_elements(&single)), vec![1, 2]);
    for (name, bbox, expected) in [
        ("contained", "[-1,-1,1,1]", vec![1, 2]),
        ("partial", "[0,0,0.0000025,0.0000025]", vec![1]),
    ] {
        let config = dir.path().join(format!("{name}.json"));
        let a = format!("{name}-a.osm.pbf");
        let b = format!("{name}-b.osm.pbf");
        let json = format!(
            r#"{{"directory":"{}","extracts":[{{"output":"{a}","bbox":{bbox}}},{{"output":"{b}","bbox":[-1,-1,1,1]}}]}}"#,
            dir.path()
                .canonicalize()
                .expect("absolute directory")
                .display(),
        );
        std::fs::write(&config, json).expect("config");
        run_extract_multi(&input, &config, Strategy::Simple);
        assert_eq!(node_ids(&read_all_elements(&dir.path().join(a))), expected);
        assert_eq!(
            node_ids(&read_all_elements(&dir.path().join(b))),
            vec![1, 2]
        );
    }
}

// ---------------------------------------------------------------------------
// Kind claims
// ---------------------------------------------------------------------------

/// Indexdata kind bytes (byte 1 of the indexdata).
const KIND_NODE: u8 = 0;
const KIND_WAY: u8 = 1;
const KIND_RELATION: u8 = 2;

/// Frames of the fixture's blobs: the header, then one blob per kind.
const FRAME_WAYS: usize = 2;
const FRAME_RELATIONS: usize = 3;

/// One forged claim: the frame, its honest kind byte, the claimed kind byte
/// and the words the refusal uses for the claim and for what the blob holds.
struct ForgedClaim {
    frame: usize,
    honest: u8,
    claim: u8,
    claimed: &'static str,
    found: &'static str,
}

/// A way blob claiming nodes (the node phase believes it), a way blob
/// claiming relations, and a relation blob claiming ways (the way phase
/// believes it).
const FORGED_CLAIMS: [ForgedClaim; 3] = [
    ForgedClaim {
        frame: FRAME_WAYS,
        honest: KIND_WAY,
        claim: KIND_NODE,
        claimed: "node",
        found: "way",
    },
    ForgedClaim {
        frame: FRAME_WAYS,
        honest: KIND_WAY,
        claim: KIND_RELATION,
        claimed: "relation",
        found: "way",
    },
    ForgedClaim {
        frame: FRAME_RELATIONS,
        honest: KIND_RELATION,
        claim: KIND_WAY,
        claimed: "way",
        found: "relation",
    },
];

fn write_kind_fixture(path: &Path, sorted: bool) {
    if sorted {
        write_test_pbf_sorted(path, &test_nodes(), &test_ways(), &test_relations());
    } else {
        write_test_pbf(path, &test_nodes(), &test_ways(), &test_relations());
    }
}

/// The fixture with one blob's indexdata kind byte forged: a lie no
/// producer writes.
fn forge_kind_claim(dir: &Path, sorted: bool, forged: &ForgedClaim) -> std::path::PathBuf {
    let honest = dir.join(format!("honest-{sorted}.osm.pbf"));
    write_kind_fixture(&honest, sorted);
    let bytes = std::fs::read(&honest).expect("read the fixture");
    let lying =
        crate::common::adversarial::mutate_blob_header_indexdata(&bytes, forged.frame, |index| {
            assert_eq!(index[1], forged.honest, "frame {} kind", forged.frame);
            index[1] = forged.claim;
        });
    let path = dir.join(format!(
        "frame{}-claims-{}-{sorted}.osm.pbf",
        forged.frame, forged.claim
    ));
    std::fs::write(&path, lying).expect("write the forged file");
    path
}

fn refusal_text(claimed: &str, found: &str) -> String {
    format!("blob indexdata claims {claimed} elements but the blob holds {found} elements")
}

fn multi_config(dir: &Path, name: &str) -> std::path::PathBuf {
    let config = dir.join(format!("{name}.json"));
    std::fs::write(
        &config,
        format!(
            r#"{{"directory":"{}","extracts":[{{"output":"{name}.osm.pbf","bbox":[12.4,55.6,12.7,55.8]}}]}}"#,
            dir.display()
        ),
    )
    .expect("write config");
    config
}

/// A blob claiming one kind but holding another used to land in a phase
/// that ignores its real contents (a node-claimed blob holding ways was read
/// only by node-phase code, so its ways were dropped) and the extract was
/// published with exit 0. Every strategy, sorted and unsorted, refuses it
/// and publishes nothing.
#[test]
fn extract_refuses_a_blob_whose_claimed_kind_is_false() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    for sorted in [true, false] {
        for forged in &FORGED_CLAIMS {
            let input = forge_kind_claim(dir.path(), sorted, forged);
            for (name, strategy) in [
                ("simple", Strategy::Simple),
                ("complete", Strategy::CompleteWays),
                ("smart", Strategy::Smart),
            ] {
                let output = dir.path().join(format!(
                    "out-{name}-{sorted}-{}-{}.osm.pbf",
                    forged.frame, forged.claim
                ));
                let cli = CliInvoker::new(&invocation_scratch)
                    .arg("extract")
                    .arg(&input)
                    .arg("-o")
                    .arg(&output)
                    .arg("--bbox")
                    .arg(BBOX_STR);
                let out = strategy_args(cli, strategy)
                    .arg("--set-bounds")
                    .assert_failed();
                out.assert_stderr_contains(&refusal_text(forged.claimed, forged.found));
                assert!(
                    !output.exists(),
                    "{name}, sorted {sorted}, frame {} claims {}: nothing is published",
                    forged.frame,
                    forged.claim
                );
            }
        }
    }
}

/// A way blob given a node blob's indexdata, bbox included, is a raw
/// passthrough candidate: the node classify phase reads it raw, finds no
/// node, and used to drop its ways. Single-region and multi-extract both
/// refuse it.
#[test]
fn extract_refuses_a_passthrough_candidate_holding_ways() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    let input = dir.path().join("input.osm.pbf");
    let ways = [TestWay {
        id: 10,
        refs: vec![1, 2],
        tags: vec![],
        meta: None,
    }];
    write_test_pbf_sorted(&honest, &inside_nodes(4, 1), &ways, &[]);
    let pbf = std::fs::read(&honest).expect("read fixture");
    let node_index = indexdata_of(&pbf, 1);
    let lying = crate::common::adversarial::mutate_blob_header_indexdata(&pbf, FRAME_WAYS, |ix| {
        ix.clone_from(&node_index);
    });
    std::fs::write(&input, lying).expect("write lying fixture");

    let output = dir.path().join("single.osm.pbf");
    let out = CliInvoker::new(&invocation_scratch)
        .arg("extract")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .args(["--bbox", BBOX_STR, "--simple"])
        .assert_failed();
    out.assert_stderr_contains(&refusal_text("node", "way"));
    assert!(!output.exists(), "nothing is published");

    let config = multi_config(dir.path(), "multi-candidate");
    let out = CliInvoker::new(&invocation_scratch)
        .arg("extract")
        .arg(&input)
        .arg("--config")
        .arg(&config)
        .args(["--simple", "--set-bounds"])
        .assert_failed();
    out.assert_stderr_contains(&refusal_text("node", "way"));
    assert!(
        !dir.path().join("multi-candidate.osm.pbf").exists(),
        "nothing is published"
    );
}

/// Multi-extract classifies each kind from the blobs claiming it, so every
/// forged claim is refused before any region output is published.
#[test]
fn multi_extract_refuses_a_blob_whose_claimed_kind_is_false() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    for forged in &FORGED_CLAIMS {
        let input = forge_kind_claim(dir.path(), true, forged);
        let name = format!("multi-{}-{}", forged.frame, forged.claim);
        let config = multi_config(dir.path(), &name);
        let out = CliInvoker::new(&invocation_scratch)
            .arg("extract")
            .arg(&input)
            .arg("--config")
            .arg(&config)
            .args(["--simple", "--set-bounds"])
            .assert_failed();
        out.assert_stderr_contains(&refusal_text(forged.claimed, forged.found));
        assert!(
            !dir.path().join(format!("{name}.osm.pbf")).exists(),
            "frame {} claims {}: nothing is published",
            forged.frame,
            forged.claim
        );
    }
}

/// The passing counterpart: honest claims extract under every strategy,
/// sorted and unsorted, and under multi-extract.
#[test]
fn extract_accepts_honest_kind_claims() {
    let dir = crate::common::test_scratch();
    for sorted in [true, false] {
        let input = dir.path().join(format!("honest-{sorted}.osm.pbf"));
        write_kind_fixture(&input, sorted);
        for (name, strategy) in [
            ("simple", Strategy::Simple),
            ("complete", Strategy::CompleteWays),
            ("smart", Strategy::Smart),
        ] {
            let output = dir.path().join(format!("out-{name}-{sorted}.osm.pbf"));
            run_extract(&input, &output, &RegionArg::Bbox(BBOX_STR), strategy);
            let elements = read_all_elements(&output);
            let nodes = node_ids(&elements);
            assert!(
                nodes.contains(&1) && nodes.contains(&3),
                "{name}, sorted {sorted}: {nodes:?}"
            );
            assert!(
                way_ids(&elements).contains(&12),
                "{name}, sorted {sorted}: the way inside the bbox is extracted"
            );
        }
    }

    let input = dir.path().join("honest-true.osm.pbf");
    let config = multi_config(dir.path(), "multi-honest");
    run_extract_multi(&input, &config, Strategy::Simple);
    let elements = read_all_elements(&dir.path().join("multi-honest.osm.pbf"));
    assert_eq!(node_ids(&elements), vec![1, 3]);
    assert!(way_ids(&elements).contains(&12));
}

// ---------------------------------------------------------------------------
// Negative IDs
// ---------------------------------------------------------------------------

/// A graph whose negative elements reach every closure route: the outside
/// node of a matched way (complete-ways), the member way and its outside node
/// of a matched multipolygon (smart pass 1 extras and pass 2), and a parent
/// relation of an included relation. Every negative has a magnitude twin
/// that an absolute-value match would confuse it with: `n2` is inside while
/// `n-2` is not, so `w10`, which refers only to negatives outside, must stay
/// out. Canonical order per kind, so the same lists serve the sorted file.
fn negative_graph() -> (
    Vec<TestNode<'static>>,
    Vec<TestWay<'static>>,
    Vec<TestRelation<'static>>,
) {
    let node = |id, lat, lon| TestNode {
        id,
        lat,
        lon,
        tags: vec![],
        meta: None,
    };
    let way = |id, refs: &[i64]| TestWay {
        id,
        refs: refs.to_vec(),
        tags: vec![("highway", "path")],
        meta: None,
    };
    let nodes = vec![
        node(-1, 557_000_000, 125_000_000),
        node(-2, 540_000_000, 125_000_000),
        node(-3, 557_000_000, 140_000_000),
        node(2, 556_500_000, 125_500_000),
    ];
    let ways = vec![way(-10, &[-1, -2]), way(-11, &[-2, -3]), way(10, &[-3, -2])];
    let relations = vec![
        TestRelation {
            id: -300,
            members: vec![
                TestMember {
                    id: MemberId::Way(-10),
                    role: "outer",
                },
                TestMember {
                    id: MemberId::Way(-11),
                    role: "inner",
                },
            ],
            tags: vec![("type", "multipolygon")],
            meta: None,
        },
        TestRelation {
            id: -400,
            members: vec![TestMember {
                id: MemberId::Relation(-300),
                role: "",
            }],
            tags: vec![("type", "route")],
            meta: None,
        },
        TestRelation {
            id: 400,
            members: vec![TestMember {
                id: MemberId::Way(10),
                role: "",
            }],
            tags: vec![("type", "route")],
            meta: None,
        },
    ];
    (nodes, ways, relations)
}

fn sorted_ids(mut ids: Vec<i64>) -> Vec<i64> {
    ids.sort_unstable();
    ids
}

#[test]
fn negative_ids_are_selected_exactly_by_every_strategy() {
    let dir = crate::common::test_scratch();
    let (nodes, ways, relations) = negative_graph();
    for sorted in [false, true] {
        let input = dir.path().join(format!("neg-{sorted}.osm.pbf"));
        if sorted {
            write_test_pbf_sorted(&input, &nodes, &ways, &relations);
        } else {
            write_test_pbf(&input, &nodes, &ways, &relations);
        }
        for (name, strategy, want_nodes, want_ways, want_relations) in [
            (
                "simple",
                Strategy::Simple,
                vec![-1, 2],
                vec![-10],
                vec![-300],
            ),
            (
                "complete",
                Strategy::CompleteWays,
                vec![-2, -1, 2],
                vec![-10],
                vec![-400, -300],
            ),
            (
                "smart",
                Strategy::Smart,
                vec![-3, -2, -1, 2],
                vec![-11, -10],
                vec![-400, -300],
            ),
        ] {
            let output = dir.path().join(format!("out-{name}-{sorted}.osm.pbf"));
            run_extract(&input, &output, &RegionArg::Bbox(BBOX_STR), strategy);
            let c = read_all_elements(&output);
            let at = format!("{name}, sorted {sorted}");
            assert_eq!(sorted_ids(node_ids(&c)), want_nodes, "{at}: nodes");
            assert_eq!(sorted_ids(way_ids(&c)), want_ways, "{at}: ways");
            assert_eq!(
                sorted_ids(relation_ids(&c)),
                want_relations,
                "{at}: relations"
            );
        }
    }
}

/// The single-pass multi-extract keys negatives into region membership, so
/// each region of one `--config` run writes what its own single extract
/// writes, and the twins stay apart between regions.
#[test]
fn multi_extract_selects_negative_ids_as_its_single_extracts_do() {
    let dir = crate::common::test_scratch();
    let (nodes, ways, relations) = negative_graph();
    let input = dir.path().join("neg.osm.pbf");
    write_test_pbf_sorted(&input, &nodes, &ways, &relations);
    let mut simple_b = None;
    for strategy in [Strategy::Simple, Strategy::CompleteWays, Strategy::Smart] {
        let outputs = assert_multi_matches_singles(
            dir.path(),
            &input,
            "neg",
            &[BBOX_STR, SOUTH],
            strategy,
            &[],
        );
        if matches!(strategy, Strategy::Simple) {
            simple_b = Some(outputs[1].clone());
        }
    }
    let b = read_all_elements(&simple_b.unwrap());
    assert_eq!(node_ids(&b), vec![-2]);
    assert_eq!(sorted_ids(way_ids(&b)), vec![-11, -10, 10]);
    assert_eq!(sorted_ids(relation_ids(&b)), vec![-300, 400]);
}

// ---------------------------------------------------------------------------
// Multi-extract parity with single extracts
// ---------------------------------------------------------------------------

/// Node 2's region (south of `BBOX_STR`).
const SOUTH: &str = "12.4,53.9,12.6,54.1";
/// Node 4's region (east of `BBOX_STR`).
const EAST: &str = "13.5,55.5,14.5,55.9";
/// Overlaps `BBOX_STR` on node 1 and node 3 (on its south edge) and holds
/// node 4.
const WIDE: &str = "12.45,55.65,14.5,55.9";

/// The `Extract (...)` summary line from an extract's stderr: a single
/// extract's (`region` None), or region `region`'s (1-based) of a
/// multi-extract.
fn extract_summary(stderr: &str, region: Option<usize>) -> String {
    let tag = region.map(|r| format!("[{r}] "));
    let line = stderr
        .lines()
        .find(|l| {
            l.contains("Extract (")
                && match &tag {
                    Some(tag) => l.contains(tag.as_str()),
                    None => true,
                }
        })
        .unwrap_or_else(|| panic!("no summary for {region:?}; stderr:\n{stderr}"));
    line[line.find("Extract (").unwrap()..].to_string()
}

/// Run one `--config` multi-extract of `bboxes` and a single extract per
/// bbox, under `strategy` and `extra` flags, and assert that the multi run
/// took the single pass and that every region's output (elements, metadata
/// and file order) and summary equal its single extract's. Returns the
/// multi-extract outputs, in region order.
fn assert_multi_matches_singles(
    dir: &Path,
    input: &Path,
    tag: &str,
    bboxes: &[&str],
    strategy: Strategy,
    extra: &[&str],
) -> Vec<std::path::PathBuf> {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let at = format!("{tag} {strategy:?} {} regions", bboxes.len());
    let stem = format!("{tag}-{strategy:?}-{}", bboxes.len());
    let outputs: Vec<_> = (0..bboxes.len())
        .map(|i| dir.join(format!("{stem}-{i}.osm.pbf")))
        .collect();
    let extracts = outputs
        .iter()
        .zip(bboxes)
        .map(|(o, b)| format!(r#"{{"output":"{}","bbox":[{b}]}}"#, o.display()))
        .collect::<Vec<_>>()
        .join(",");
    let config = dir.join(format!("{stem}.json"));
    std::fs::write(&config, format!(r#"{{"extracts":[{extracts}]}}"#)).unwrap();
    let cli = CliInvoker::new(&invocation_scratch)
        .arg("extract")
        .arg(input)
        .arg("--config")
        .arg(&config);
    let multi = strategy_args(cli, strategy)
        .args(extra)
        .arg("--set-bounds")
        .assert_success();
    let multi_stderr = multi.stderr_str();
    assert!(
        multi_stderr.contains(&format!(
            "[multi-extract] single-pass: {} regions",
            bboxes.len()
        )),
        "{at}: the single pass must be taken; stderr:\n{multi_stderr}"
    );
    for (i, (output, bbox)) in outputs.iter().zip(bboxes).enumerate() {
        let single = dir.join(format!("{stem}-{i}-single.osm.pbf"));
        let cli = CliInvoker::new(&invocation_scratch)
            .arg("extract")
            .arg(input)
            .arg("-o")
            .arg(&single)
            .arg("--bbox")
            .arg(*bbox);
        let out = strategy_args(cli, strategy)
            .args(extra)
            .arg("--set-bounds")
            .assert_success();
        assert_eq!(
            read_in_file_order(output),
            read_in_file_order(&single),
            "{at}: region {i} output"
        );
        assert_eq!(
            extract_summary(&multi_stderr, Some(i + 1)),
            extract_summary(&out.stderr_str(), None),
            "{at}: region {i} summary"
        );
    }
    outputs
}

/// Two and three overlapping regions over a graph with parent chains, a
/// self-member, a parent stored before its child and a multipolygon whose
/// way is matched in two other regions: each region equals its single
/// extract under every strategy.
#[test]
fn multi_extract_overlapping_regions_match_their_single_extracts() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("overlap.osm.pbf");
    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &parent_relations());
    for strategy in [Strategy::Simple, Strategy::CompleteWays, Strategy::Smart] {
        for bboxes in [&[BBOX_STR, WIDE][..], &[BBOX_STR, WIDE, SOUTH][..]] {
            assert_multi_matches_singles(dir.path(), &input, "overlap", bboxes, strategy, &[]);
        }
    }
}

/// A node at a coordinate inside `BBOX_STR` (`in_a`) or inside `SOUTH`.
fn node_at(id: i64, in_a: bool) -> TestNode<'static> {
    TestNode {
        id,
        lat: if in_a { 557_000_000 } else { 540_000_000 },
        lon: 125_000_000,
        tags: vec![],
        meta: None,
    }
}

/// Ways and a relation over nodes 1 to 3: way 10 joins 1 and 3, way 11
/// is node 2 alone, relation 20 holds node 1 and way 11.
fn admission_ways_and_relation() -> (Vec<TestWay<'static>>, Vec<TestRelation<'static>>) {
    let ways = vec![
        TestWay {
            id: 10,
            refs: vec![1, 3],
            tags: vec![],
            meta: None,
        },
        TestWay {
            id: 11,
            refs: vec![2],
            tags: vec![],
            meta: None,
        },
    ];
    let relations = vec![TestRelation {
        id: 20,
        members: vec![
            TestMember {
                id: MemberId::Node(1),
                role: "",
            },
            TestMember {
                id: MemberId::Way(11),
                role: "",
            },
        ],
        tags: vec![("type", "route")],
        meta: None,
    }];
    (ways, relations)
}

/// A node ID repeated in two blobs whose bboxes each meet one region only
/// (history-shaped input, one node per blob): each region reads only the
/// blobs its bbox meets, as its single extract does, so node 1 is not
/// written to `SOUTH` from the blob at `BBOX_STR`'s coordinate, nor node 2
/// to `BBOX_STR` from the one at `SOUTH`'s.
#[test]
fn multi_extract_reads_a_repeated_id_only_from_each_regions_blobs() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("repeat.osm.pbf");
    let nodes = [
        node_at(1, true),
        node_at(1, false),
        node_at(2, false),
        node_at(2, true),
        node_at(3, true),
    ];
    let (ways, relations) = admission_ways_and_relation();
    crate::common::write_test_pbf_impl(&input, &nodes, &ways, &relations, true, Some(1));
    for strategy in [Strategy::Simple, Strategy::CompleteWays, Strategy::Smart] {
        let outputs = assert_multi_matches_singles(
            dir.path(),
            &input,
            "repeat",
            &[BBOX_STR, SOUTH],
            strategy,
            &[],
        );
        if matches!(strategy, Strategy::Simple) {
            let a = read_all_elements(&outputs[0]);
            let south = read_all_elements(&outputs[1]);
            // A: 1 and 2 from the blobs at its coordinate, and 3. SOUTH: 1
            // and 2 from the blobs at its coordinate.
            assert_eq!(node_ids(&a), vec![1, 2, 3]);
            assert_eq!(node_ids(&south), vec![1, 2]);
        }
    }
}

/// A node blob whose bbox claim understates its node (claimed at `SOUTH`'s
/// coordinate, holding node 1 at `BBOX_STR`'s) is skipped unread by the
/// single `BBOX_STR` extract, so the multi run must not classify node 1
/// for `BBOX_STR` either - nor match way 10 or relation 20 through it. On
/// disjoint ID ranges (simple runs fused) and, with node 3 repeated, on
/// overlapping ones (simple classifies then writes).
#[test]
fn multi_extract_believes_an_understated_node_bbox_per_region() {
    let dir = crate::common::test_scratch();
    let (ways, relations) = admission_ways_and_relation();
    for (tag, repeat) in [("disjoint", false), ("overlap", true)] {
        let honest = dir.path().join(format!("{tag}-honest.osm.pbf"));
        let input = dir.path().join(format!("{tag}.osm.pbf"));
        let mut nodes = vec![node_at(1, true), node_at(2, false), node_at(3, true)];
        if repeat {
            nodes.push(node_at(3, true));
        }
        crate::common::write_test_pbf_impl(&honest, &nodes, &ways, &relations, true, Some(1));
        // Frame 1 holds node 1, frame 2 node 2: give frame 1 frame 2's bbox
        // (indexdata bytes 26 to 42), keeping its own kind and ID range.
        let pbf = std::fs::read(&honest).unwrap();
        let south_bbox = indexdata_of(&pbf, 2)[26..42].to_vec();
        let understated = crate::common::adversarial::mutate_blob_header_indexdata(&pbf, 1, |ix| {
            ix[26..42].copy_from_slice(&south_bbox);
        });
        std::fs::write(&input, understated).unwrap();
        for strategy in [Strategy::Simple, Strategy::CompleteWays, Strategy::Smart] {
            let outputs = assert_multi_matches_singles(
                dir.path(),
                &input,
                tag,
                &[BBOX_STR, SOUTH],
                strategy,
                &[],
            );
            if matches!(strategy, Strategy::Simple) {
                let a = read_all_elements(&outputs[0]);
                assert_eq!(node_ids(&a), if repeat { vec![3, 3] } else { vec![3] });
            }
        }
    }
}

/// The same parity on an input without indexdata (`--force`): its blobs
/// join every classify schedule and the write pass reads each once.
#[test]
fn extract_multi_complete_ways_non_indexed_parity() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("non-indexed.osm.pbf");
    crate::common::write_multi_block_non_indexed_pbf(
        &input,
        &test_nodes(),
        &test_ways(),
        &parent_relations(),
        2,
    );
    for strategy in [Strategy::CompleteWays, Strategy::Smart] {
        assert_multi_matches_singles(
            dir.path(),
            &input,
            "non-indexed",
            &[BBOX_STR, WIDE, SOUTH],
            strategy,
            &[],
        );
    }
}

fn versioned(version: i32) -> Option<TestMeta> {
    Some(TestMeta {
        version,
        timestamp: 1_000 * i64::from(version),
        changeset: i64::from(version),
        uid: 7,
        user: "u",
        visible: true,
    })
}

/// Way 20 in two versions: version 1 refers to node 1 (in `BBOX_STR`) and
/// outside node 5, version 2 only to outside node 8. Multipolygon 600 lists
/// way 20 and node 4 (in `EAST`).
fn version_split_pbf(path: &Path) {
    let node = |id, lat, lon| TestNode {
        id,
        lat,
        lon,
        tags: vec![],
        meta: versioned(1),
    };
    let nodes = vec![
        node(1, 557_000_000, 125_000_000),
        node(4, 557_000_000, 140_000_000),
        node(5, 500_000_000, 100_000_000),
        node(8, 510_000_000, 100_000_000),
    ];
    let way = |version, refs: &[i64]| TestWay {
        id: 20,
        refs: refs.to_vec(),
        tags: vec![("highway", "path")],
        meta: versioned(version),
    };
    let ways = vec![way(1, &[1, 5]), way(2, &[8])];
    let relations = vec![TestRelation {
        id: 600,
        members: vec![
            TestMember {
                id: MemberId::Way(20),
                role: "outer",
            },
            TestMember {
                id: MemberId::Node(4),
                role: "",
            },
        ],
        tags: vec![("type", "multipolygon")],
        meta: versioned(3),
    }];
    write_test_pbf_sorted(path, &nodes, &ways, &relations);
}

/// A way matched in one region by one version and reached in another only
/// through a relation: the matching region completes only the version that
/// matched (node 8 stays out), the relation-only region gets both versions
/// and every version's refs. Cleaned and uncleaned.
#[test]
fn multi_extract_completes_ways_per_version_and_region() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("versions.osm.pbf");
    version_split_pbf(&input);
    for clean in [&[][..], &["--clean", "version"][..]] {
        let tag = if clean.is_empty() {
            "versions"
        } else {
            "versions-clean"
        };
        for strategy in [Strategy::CompleteWays, Strategy::Smart] {
            let outputs = assert_multi_matches_singles(
                dir.path(),
                &input,
                tag,
                &[BBOX_STR, EAST],
                strategy,
                clean,
            );
            let a = read_all_elements(&outputs[0]);
            let b = read_all_elements(&outputs[1]);
            assert_eq!(way_ids(&a), vec![20, 20], "{tag} {strategy:?}");
            assert!(!node_ids(&a).contains(&8), "{tag} {strategy:?}");
            assert_eq!(relation_ids(&b), vec![600], "{tag} {strategy:?}");
            if matches!(strategy, Strategy::Smart) {
                assert_eq!(sorted_ids(node_ids(&a)), vec![1, 4, 5], "{tag}");
                assert_eq!(way_ids(&b), vec![20, 20], "{tag}");
                assert_eq!(sorted_ids(node_ids(&b)), vec![1, 4, 5, 8], "{tag}");
            } else {
                assert_eq!(sorted_ids(node_ids(&a)), vec![1, 5], "{tag}");
                assert!(way_ids(&b).is_empty(), "{tag}");
            }
        }
    }
}

/// Smart completes a relation by each record's own first `type` tag:
/// version 1 of relation 700 is a multipolygon matched in `BBOX_STR`,
/// version 2 a route matched in both regions, so `EAST` completes nothing
/// of it (way 10, a version 2 member matched only in `BBOX_STR`, stays out).
#[test]
fn multi_extract_smart_type_is_per_record() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("types.osm.pbf");
    let relations = vec![
        TestRelation {
            id: 700,
            members: vec![
                TestMember {
                    id: MemberId::Way(12),
                    role: "outer",
                },
                TestMember {
                    id: MemberId::Node(2),
                    role: "",
                },
            ],
            tags: vec![("type", "multipolygon")],
            meta: versioned(1),
        },
        TestRelation {
            id: 700,
            members: vec![
                TestMember {
                    id: MemberId::Node(4),
                    role: "",
                },
                TestMember {
                    id: MemberId::Way(10),
                    role: "",
                },
            ],
            tags: vec![("type", "route")],
            meta: versioned(2),
        },
    ];
    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &relations);
    for strategy in [Strategy::CompleteWays, Strategy::Smart] {
        let outputs = assert_multi_matches_singles(
            dir.path(),
            &input,
            "types",
            &[BBOX_STR, EAST],
            strategy,
            &[],
        );
        let b = read_all_elements(&outputs[1]);
        assert_eq!(relation_ids(&b), vec![700, 700], "{strategy:?}");
        assert_eq!(way_ids(&b), vec![11], "{strategy:?}");
    }
}

/// The parent closure per region: multipolygon 800 is matched in
/// `BBOX_STR` and is a parent (through 801, a cycle with 800) of route 101
/// matched in `EAST`, so both carry both regions, and 800, already in the
/// first region, still gains the second. Bits a parent gains never
/// complete it: `EAST` does not get 800's member node 1. An unseeded cycle
/// and a self-member add nothing.
#[test]
fn multi_extract_parent_closure_crosses_regions_and_cycles() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("parents.osm.pbf");
    let rel = |id: i64, members: Vec<TestMember>, kind: &'static str| TestRelation {
        id,
        members,
        tags: vec![("type", kind)],
        meta: None,
    };
    let mut relations = test_relations();
    relations.extend([
        rel(
            800,
            vec![
                member_relation(801),
                TestMember {
                    id: MemberId::Node(1),
                    role: "",
                },
            ],
            "multipolygon",
        ),
        rel(
            801,
            vec![member_relation(800), member_relation(101)],
            "collection",
        ),
        rel(900, vec![member_relation(901)], "collection"),
        rel(901, vec![member_relation(900)], "collection"),
        rel(902, vec![member_relation(902)], "collection"),
    ]);
    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &relations);
    for strategy in [Strategy::CompleteWays, Strategy::Smart] {
        let outputs = assert_multi_matches_singles(
            dir.path(),
            &input,
            "parents",
            &[BBOX_STR, EAST],
            strategy,
            &[],
        );
        let a = read_all_elements(&outputs[0]);
        let b = read_all_elements(&outputs[1]);
        assert_eq!(relation_ids(&a), vec![100, 800, 801], "{strategy:?}");
        assert_eq!(relation_ids(&b), vec![101, 800, 801], "{strategy:?}");
        assert_eq!(sorted_ids(node_ids(&b)), vec![2, 4], "{strategy:?}");
    }
}

/// A one-node input whose block framing puts the node at a coordinate that
/// fits `i64` nanodegrees but not an `i32` decimicrodegree:
/// lat_offset 429_496_729_600 with raw latitude and longitude 0 is
/// 429.4967296 degrees, 4_294_967_296 decimicrodegrees, which `as i32`
/// narrows to 0. `offset` 0 is the in-range control. `plain` selects a
/// plain Node message instead of a dense group. The writer gives the blob
/// indexdata without a bbox (no box holds the coordinate), so every route
/// decodes it.
fn write_unrepresentable_node_pbf(input: &Path, sorted: bool, plain: bool, offset: u64) {
    fn varint(mut v: u64, out: &mut Vec<u8>) {
        while v >= 0x80 {
            out.push(u8::try_from(v & 0x7f).expect("seven bits") | 0x80);
            v >>= 7;
        }
        out.push(u8::try_from(v).expect("below 0x80"));
    }
    // id 1 (zigzag 2), lat 0, lon 0.
    let group: Vec<u8> = if plain {
        vec![10, 6, 8, 2, 64, 0, 72, 0]
    } else {
        vec![18, 9, 10, 1, 2, 66, 1, 0, 74, 1, 0]
    };
    let mut block = vec![10, 2, 10, 0, 18];
    varint(u64::try_from(group.len()).expect("small"), &mut block);
    block.extend_from_slice(&group);
    // Field 19, lat_offset.
    varint(19 << 3, &mut block);
    varint(offset, &mut block);
    let header = if sorted {
        HeaderBuilder::new().sorted()
    } else {
        HeaderBuilder::new()
    };
    let mut writer = PbfWriter::new(
        std::fs::File::create(input).expect("create input"),
        Compression::default(),
    );
    writer
        .write_header(&header.build().expect("header"))
        .expect("write header");
    writer.write_primitive_block(&block).expect("write block");
    writer.flush().expect("flush");
}

/// The checks are not only at classification. Node 1 at 10,10 sits in its
/// own blob, whose stored bbox claim stays 10,10 after its payload gains a
/// lat_offset that puts the node past `i32`: the claim is disjoint from the
/// region, so classification skips the blob, but way 10 is selected through
/// node 2 at 0,0, and complete-ways and smart then write node 1 as a way
/// dependency. That write refuses rather than emitting the wrapped
/// coordinate. Mutating node 2's blob instead leaves its claim contained in
/// the region, so multi-extract nominates it for raw copy; verification
/// must refuse it, not copy it. Offset 0 is the control.
#[test]
fn extract_writers_and_raw_copy_refuse_an_unrepresentable_coordinate() {
    const REFUSAL: &str = "node coordinate does not fit in an i32 decimicrodegree";
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let node = |id, lat, lon| TestNode {
        id,
        lat,
        lon,
        tags: vec![],
        meta: None,
    };
    crate::common::write_multi_block_test_pbf(
        &base,
        &[node(1, 100_000_000, 100_000_000), node(2, 0, 0)],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "residential")],
            meta: None,
        }],
        &[],
        1,
    );
    let pbf = std::fs::read(&base).expect("read base");
    let mutated = |blob: usize, offset: i64, name: &str| {
        let path = dir.path().join(name);
        std::fs::write(
            &path,
            crate::common::adversarial::mutate_blob_payload(&pbf, blob, |p| {
                p.extend_from_slice(&[0x98, 0x01]);
                crate::common::adversarial::write_varint(p, offset.cast_unsigned());
            }),
        )
        .expect("write mutated input");
        path
    };
    for (offset, refused) in [(429_496_729_600_i64, true), (0, false)] {
        let input = mutated(1, offset, &format!("dependency-{offset}.osm.pbf"));
        for (name, strategy) in [
            ("complete", Strategy::CompleteWays),
            ("smart", Strategy::Smart),
        ] {
            let out = dir.path().join(format!("{name}-{offset}.osm.pbf"));
            let cli = CliInvoker::new(&invocation_scratch)
                .arg("extract")
                .arg(&input)
                .arg("-o")
                .arg(&out)
                .arg("--bbox=-1,-1,1,1");
            let cli = strategy_args(cli, strategy);
            if refused {
                cli.assert_refused(REFUSAL);
                assert!(!out.exists(), "{name}: nothing is published");
            } else {
                cli.assert_success();
                assert_eq!(node_ids(&read_all_elements(&out)), vec![1, 2], "{name}");
            }
        }

        let input = mutated(2, offset, &format!("candidate-{offset}.osm.pbf"));
        let config = dir.path().join(format!("multi-{offset}.json"));
        let (a, b) = (format!("a-{offset}.osm.pbf"), format!("b-{offset}.osm.pbf"));
        std::fs::write(
            &config,
            format!(
                r#"{{"directory":"{}","extracts":[{{"output":"{a}","bbox":[-1,-1,1,1]}},{{"output":"{b}","bbox":[-2,-2,2,2]}}]}}"#,
                dir.path()
                    .canonicalize()
                    .expect("absolute directory")
                    .display(),
            ),
        )
        .expect("config");
        let cli = CliInvoker::new(&invocation_scratch)
            .arg("extract")
            .arg(&input)
            .arg("--config")
            .arg(&config)
            .arg("--simple");
        if refused {
            cli.assert_refused(REFUSAL);
            assert!(!dir.path().join(&a).exists(), "multi: nothing is published");
        } else {
            cli.assert_success();
            assert_eq!(node_ids(&read_all_elements(&dir.path().join(&a))), vec![2]);
        }
    }
}

/// Every extract route refuses a node whose coordinate does not fit an
/// `i32` decimicrodegree, as sorted simple's node scanner always did, rather
/// than wrapping it to `(0, 0)` and selecting and writing it there:
/// sorted and unsorted input, dense and plain nodes, simple, complete-ways
/// and smart, bbox and polygon, and the shared multi-extract route. The
/// same input with offset 0 extracts the node, so the refusal is the
/// coordinate's.
#[test]
fn every_extract_route_refuses_an_unrepresentable_coordinate() {
    const REFUSAL: &str = "node coordinate does not fit in an i32 decimicrodegree";
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let polygon = dir.path().join("null-island.geojson");
    std::fs::write(
        &polygon,
        r#"{"type":"Polygon","coordinates":[[[-1,-1],[1,-1],[1,1],[-1,1],[-1,-1]]]}"#,
    )
    .expect("polygon");
    for sorted in [true, false] {
        for plain in [false, true] {
            for (offset, refused) in [(429_496_729_600_u64, true), (0, false)] {
                let case = format!("sorted {sorted} plain {plain} offset {offset}");
                let input = dir
                    .path()
                    .join(format!("in-{sorted}-{plain}-{offset}.osm.pbf"));
                write_unrepresentable_node_pbf(&input, sorted, plain, offset);
                for (name, strategy) in [
                    ("simple", Strategy::Simple),
                    ("complete", Strategy::CompleteWays),
                    ("smart", Strategy::Smart),
                ] {
                    for region in [RegionArg::Bbox("-1,-1,1,1"), RegionArg::Polygon(&polygon)] {
                        let out = dir.path().join(format!("out-{name}.osm.pbf"));
                        if out.exists() {
                            std::fs::remove_file(&out).expect("remove previous output");
                        }
                        let cli = CliInvoker::new(&invocation_scratch)
                            .arg("extract")
                            .arg(&input)
                            .arg("-o")
                            .arg(&out);
                        let cli = match region {
                            RegionArg::Bbox(b) => cli.arg(format!("--bbox={b}")),
                            RegionArg::Polygon(p) => cli.arg("--polygon").arg(p),
                        };
                        let cli = strategy_args(cli, strategy);
                        if refused {
                            cli.assert_refused(REFUSAL);
                            assert!(!out.exists(), "{name}, {case}: nothing is published");
                        } else {
                            cli.assert_success();
                            assert_eq!(
                                node_ids(&read_all_elements(&out)),
                                vec![1],
                                "{name}, {case}"
                            );
                        }
                    }
                }
                // All-bbox regions take the columnar classifier; one polygon
                // moves every region onto the element classifier.
                for second in [
                    r#""bbox":[-2,-2,2,2]"#,
                    r#""polygon":{"type":"Polygon","coordinates":[[[-2,-2],[2,-2],[2,2],[-2,2],[-2,-2]]]}"#,
                ] {
                    let config = dir.path().join("multi.json");
                    std::fs::write(
                        &config,
                        format!(
                            r#"{{"directory":"{}","extracts":[{{"output":"a.osm.pbf","bbox":[-1,-1,1,1]}},{{"output":"b.osm.pbf",{second}}}]}}"#,
                            dir.path().canonicalize().expect("absolute directory").display(),
                        ),
                    )
                    .expect("config");
                    for name in ["a.osm.pbf", "b.osm.pbf"] {
                        let path = dir.path().join(name);
                        if path.exists() {
                            std::fs::remove_file(&path).expect("remove previous output");
                        }
                    }
                    let cli = CliInvoker::new(&invocation_scratch)
                        .arg("extract")
                        .arg(&input)
                        .arg("--config")
                        .arg(&config)
                        .arg("--simple");
                    if refused {
                        cli.assert_refused(REFUSAL);
                        assert!(!dir.path().join("a.osm.pbf").exists(), "multi, {case}");
                    } else {
                        cli.assert_success();
                        assert_eq!(
                            node_ids(&read_all_elements(&dir.path().join("b.osm.pbf"))),
                            vec![1],
                            "multi {second}, {case}"
                        );
                    }
                }
            }
        }
    }
}
