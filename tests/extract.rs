//! Extract (bbox) correctness tests.
#![cfg(feature = "commands")]

mod common;

use common::{
    node_ids_id_only as node_ids, read_all_elements_id_only as read_all_elements,
    way_ids_id_only as way_ids, relation_ids_id_only as relation_ids,
    read_header, write_test_pbf, write_test_pbf_sorted, TestMember, TestNode, TestRelation,
    TestWay,
};
use pbfhogg::cat::CleanAttrs;
use pbfhogg::extract::{extract, parse_bbox, parse_geojson, ExtractStrategy, PolygonRings, Region};
use pbfhogg::writer::Compression;
use pbfhogg::MemberId;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Test data
// ---------------------------------------------------------------------------
//
// Node geography (bbox will be 12.4,55.6,12.7,55.8):
//   Node 1: lat=55.70, lon=12.50 → INSIDE    (decimicro: 557_000_000, 125_000_000)
//   Node 2: lat=54.00, lon=12.50 → OUTSIDE    (decimicro: 540_000_000, 125_000_000)
//   Node 3: lat=55.65, lon=12.55 → INSIDE    (decimicro: 556_500_000, 125_500_000)
//   Node 4: lat=55.70, lon=14.00 → OUTSIDE    (decimicro: 557_000_000, 140_000_000)
//
// Ways:
//   Way 10: refs [1, 2] → node 1 in bbox (way matches, but node 2 is outside)
//   Way 11: refs [2, 4] → no nodes in bbox (way does NOT match)
//   Way 12: refs [1, 3] → both nodes in bbox
//
// Relations:
//   Relation 100: member=node 1 → matches (node 1 in bbox)
//   Relation 101: member=node 4  → does NOT match

const BBOX_STR: &str = "12.4,55.6,12.7,55.8";

fn test_nodes() -> Vec<TestNode> {
    vec![
        TestNode { id: 1, lat: 557_000_000, lon: 125_000_000, tags: vec![("name", "inside1")] },
        TestNode { id: 2, lat: 540_000_000, lon: 125_000_000, tags: vec![("name", "outside_south")] },
        TestNode { id: 3, lat: 556_500_000, lon: 125_500_000, tags: vec![("name", "inside2")] },
        TestNode { id: 4, lat: 557_000_000, lon: 140_000_000, tags: vec![("name", "outside_east")] },
    ]
}

fn test_ways() -> Vec<TestWay> {
    vec![
        TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] },
        TestWay { id: 11, refs: vec![2, 4], tags: vec![("highway", "secondary")] },
        TestWay { id: 12, refs: vec![1, 3], tags: vec![("highway", "tertiary")] },
    ]
}

fn test_relations() -> Vec<TestRelation> {
    vec![
        TestRelation {
            id: 100,
            members: vec![TestMember { id: MemberId::Node(1), role: "stop" }],
            tags: vec![("type", "route")],
        },
        TestRelation {
            id: 101,
            members: vec![TestMember { id: MemberId::Node(4), role: "stop" }],
            tags: vec![("type", "route")],
        },
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn simple_filters_nodes_by_bbox() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &[], &[]);

    let bbox = parse_bbox(BBOX_STR).expect("parse bbox");
    let region = Region::Bbox(bbox);
    let stats = extract(&input, &output, &region, ExtractStrategy::Simple, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 3]);
    assert_eq!(stats.nodes_in_bbox, 2);
    assert_eq!(stats.nodes_from_ways, 0);
}

#[test]
fn simple_includes_ways_with_nodes_in_bbox() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);

    let bbox = parse_bbox(BBOX_STR).expect("parse bbox");
    let region = Region::Bbox(bbox);
    let stats = extract(&input, &output, &region, ExtractStrategy::Simple, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    // Ways 10 and 12 have at least one node in bbox; way 11 does not
    assert_eq!(way_ids(&c), vec![10, 12]);
    assert_eq!(stats.ways_written, 2);
}

#[test]
fn simple_does_not_add_extra_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);

    let bbox = parse_bbox(BBOX_STR).expect("parse bbox");
    let region = Region::Bbox(bbox);
    let stats = extract(&input, &output, &region, ExtractStrategy::Simple, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    // Simple mode: only nodes actually in bbox, not way dependencies
    assert_eq!(node_ids(&c), vec![1, 3]);
    assert_eq!(stats.nodes_from_ways, 0);
}

#[test]
fn complete_ways_includes_all_way_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);

    let bbox = parse_bbox(BBOX_STR).expect("parse bbox");
    let region = Region::Bbox(bbox);
    let stats = extract(&input, &output, &region, ExtractStrategy::CompleteWays, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    // Way 10 refs [1, 2]: node 1 in bbox → way matches → node 2 pulled in
    // Way 12 refs [1, 3]: both in bbox → no extra deps
    // Way 11 refs [2, 4]: no nodes in bbox → excluded
    assert_eq!(node_ids(&c), vec![1, 2, 3]);
    assert_eq!(way_ids(&c), vec![10, 12]);
    assert_eq!(stats.nodes_in_bbox, 2);
    assert_eq!(stats.nodes_from_ways, 1);
}

#[test]
fn complete_ways_includes_relations() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &test_relations());

    let bbox = parse_bbox(BBOX_STR).expect("parse bbox");
    let region = Region::Bbox(bbox);
    let stats = extract(&input, &output, &region, ExtractStrategy::CompleteWays, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    // Relation 100 has member node 1 (in bbox) → included
    // Relation 101 has member node 4 (outside bbox) → excluded
    assert_eq!(relation_ids(&c), vec![100]);
    assert_eq!(stats.relations_written, 1);
}

#[test]
fn simple_includes_relations_with_matched_ways() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let relations = vec![TestRelation {
        id: 200,
        members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
        tags: vec![("type", "multipolygon")],
    }];

    write_test_pbf(&input, &test_nodes(), &test_ways(), &relations);

    let bbox = parse_bbox(BBOX_STR).expect("parse bbox");
    let region = Region::Bbox(bbox);
    let stats = extract(&input, &output, &region, ExtractStrategy::Simple, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    // Way 10 matched → relation 200 should be included
    assert_eq!(relation_ids(&c), vec![200]);
    assert_eq!(stats.relations_written, 1);
}

#[test]
fn empty_extract() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &test_relations());

    // Bbox far away from all test data
    let bbox = parse_bbox("0.0,0.0,1.0,1.0").expect("parse bbox");
    let region = Region::Bbox(bbox);
    let stats = extract(&input, &output, &region, ExtractStrategy::CompleteWays, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    assert!(c.nodes.is_empty());
    assert!(c.ways.is_empty());
    assert!(c.relations.is_empty());
    assert_eq!(stats.nodes_in_bbox, 0);
    assert_eq!(stats.ways_written, 0);
}

#[test]
fn tags_preserved_in_extract() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);

    let bbox = parse_bbox(BBOX_STR).expect("parse bbox");
    let region = Region::Bbox(bbox);
    extract(&input, &output, &region, ExtractStrategy::CompleteWays, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    // Check node 1 tags
    let node1 = c.nodes.iter().find(|(id, _)| *id == 1).expect("node 1");
    assert_eq!(node1.1, vec![("name".to_string(), "inside1".to_string())]);

    // Check way 10 tags
    let way10 = c.ways.iter().find(|(id, _, _)| *id == 10).expect("way 10");
    assert_eq!(
        way10.2,
        vec![("highway".to_string(), "primary".to_string())]
    );
}

// ---------------------------------------------------------------------------
// Polygon tests
// ---------------------------------------------------------------------------
//
// Test polygon: a triangle covering nodes 1 and 3 but NOT node 2 or 4.
//
//   Node 1: lat=55.70, lon=12.50 → INSIDE triangle
//   Node 2: lat=54.00, lon=12.50 → OUTSIDE (too far south)
//   Node 3: lat=55.65, lon=12.55 → INSIDE triangle
//   Node 4: lat=55.70, lon=14.00 → OUTSIDE (too far east)
//
// Triangle vertices (lon, lat):
//   (12.3, 55.5), (12.8, 55.5), (12.55, 55.9)

fn test_polygon_region() -> Region {
    Region::Polygon {
        polygons: vec![PolygonRings {
            exterior: vec![
                (12.3, 55.5),
                (12.8, 55.5),
                (12.55, 55.9),
                (12.3, 55.5),
            ],
            holes: vec![],
        }],
        bbox: pbfhogg::extract::Bbox {
            min_lon: 12.3,
            min_lat: 55.5,
            max_lon: 12.8,
            max_lat: 55.9,
        },
    }
}

#[test]
fn polygon_simple_filters_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &[], &[]);

    let region = test_polygon_region();
    let stats = extract(&input, &output, &region, ExtractStrategy::Simple, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    // Nodes 1 and 3 are inside the triangle; nodes 2 and 4 are outside
    assert_eq!(node_ids(&c), vec![1, 3]);
    assert_eq!(stats.nodes_in_bbox, 2);
}

#[test]
fn polygon_complete_ways_includes_all_way_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);

    let region = test_polygon_region();
    let stats = extract(&input, &output, &region, ExtractStrategy::CompleteWays, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    // Way 10 [1,2]: node 1 in polygon → way matches → node 2 pulled in
    // Way 12 [1,3]: both in polygon → no extra deps
    // Way 11 [2,4]: no nodes in polygon → excluded
    assert_eq!(node_ids(&c), vec![1, 2, 3]);
    assert_eq!(way_ids(&c), vec![10, 12]);
    assert_eq!(stats.nodes_in_bbox, 2);
    assert_eq!(stats.nodes_from_ways, 1);
}

#[test]
fn polygon_with_hole_excludes_interior() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    // Use a square polygon with a hole that excludes node 1 but includes node 3
    //
    // Node 1: lat=55.70, lon=12.50 → inside hole → EXCLUDED
    // Node 3: lat=55.65, lon=12.55 → inside exterior, outside hole → INCLUDED
    //
    // Exterior: large square covering both nodes
    // Hole: small square around node 1 only
    let region = Region::Polygon {
        polygons: vec![PolygonRings {
            exterior: vec![
                (12.0, 55.0),
                (13.0, 55.0),
                (13.0, 56.0),
                (12.0, 56.0),
                (12.0, 55.0),
            ],
            holes: vec![vec![
                (12.45, 55.68),
                (12.55, 55.68),
                (12.55, 55.72),
                (12.45, 55.72),
                (12.45, 55.68),
            ]],
        }],
        bbox: pbfhogg::extract::Bbox {
            min_lon: 12.0,
            min_lat: 55.0,
            max_lon: 13.0,
            max_lat: 56.0,
        },
    };

    write_test_pbf(&input, &test_nodes(), &[], &[]);

    let stats = extract(&input, &output, &region, ExtractStrategy::Simple, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    // Node 1 is inside the hole → excluded
    // Node 3 is inside exterior, outside hole → included
    // Node 2 is far south (lat=54) → outside exterior
    // Node 4 is far east (lon=14) → outside exterior
    assert_eq!(node_ids(&c), vec![3]);
    assert_eq!(stats.nodes_in_bbox, 1);
}

#[test]
fn polygon_from_geojson_file() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let geojson_path = dir.path().join("region.geojson");

    // Write a GeoJSON triangle that covers nodes 1 and 3
    std::fs::write(
        &geojson_path,
        r#"{
            "type": "Polygon",
            "coordinates": [
                [[12.3, 55.5], [12.8, 55.5], [12.55, 55.9], [12.3, 55.5]]
            ]
        }"#,
    )
    .expect("write geojson");

    write_test_pbf(&input, &test_nodes(), &[], &[]);

    let region = parse_geojson(&geojson_path).expect("parse geojson");
    let stats = extract(&input, &output, &region, ExtractStrategy::Simple, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 3]);
    assert_eq!(stats.nodes_in_bbox, 2);
}

// ---------------------------------------------------------------------------
// Smart strategy tests
// ---------------------------------------------------------------------------
//
// Way 11 refs [2, 4]: no bbox nodes → NOT matched by simple/complete_ways.
// But if a multipolygon relation includes way 11 as a member AND is itself
// matched (via another member in the bbox), smart should pull in way 11 and
// its nodes (2, 4).

#[test]
fn smart_includes_multipolygon_way_members() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    // Relation 300: type=multipolygon, members = way 10 (matched) + way 11 (not matched)
    let relations = vec![TestRelation {
        id: 300,
        members: vec![
            TestMember { id: MemberId::Way(10), role: "outer" },
            TestMember { id: MemberId::Way(11), role: "inner" },
        ],
        tags: vec![("type", "multipolygon")],
    }];

    write_test_pbf(&input, &test_nodes(), &test_ways(), &relations);

    let bbox = parse_bbox(BBOX_STR).expect("parse bbox");
    let region = Region::Bbox(bbox);
    let stats = extract(&input, &output, &region, ExtractStrategy::Smart, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    // Way 10 matched (has bbox node 1). Way 11 pulled in via smart deps.
    // Way 12 matched normally (both refs in bbox).
    assert_eq!(way_ids(&c), vec![10, 11, 12]);
    // Nodes 1, 3 in bbox. Node 2 from way deps. Node 4 from way 11 smart deps.
    assert_eq!(node_ids(&c), vec![1, 2, 3, 4]);
    assert_eq!(relation_ids(&c), vec![300]);
    assert_eq!(stats.relations_written, 1);
}

#[test]
fn smart_includes_boundary_node_members() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    // Relation 301: type=boundary, members = way 12 (matched) + node 4 (outside)
    let relations = vec![TestRelation {
        id: 301,
        members: vec![
            TestMember { id: MemberId::Way(12), role: "outer" },
            TestMember { id: MemberId::Node(4), role: "admin_centre" },
        ],
        tags: vec![("type", "boundary"), ("boundary", "administrative")],
    }];

    write_test_pbf(&input, &test_nodes(), &test_ways(), &relations);

    let bbox = parse_bbox(BBOX_STR).expect("parse bbox");
    let region = Region::Bbox(bbox);
    let stats = extract(&input, &output, &region, ExtractStrategy::Smart, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    // Node 4 should be pulled in via smart boundary deps.
    assert!(node_ids(&c).contains(&4), "node 4 (admin_centre) should be included");
    assert_eq!(relation_ids(&c), vec![301]);
    assert!(stats.nodes_from_relations > 0);
}

#[test]
fn smart_ignores_non_qualifying_relations() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    // Relation 302: type=route (NOT multipolygon/boundary), members = way 10 + way 11
    let relations = vec![TestRelation {
        id: 302,
        members: vec![
            TestMember { id: MemberId::Way(10), role: "forward" },
            TestMember { id: MemberId::Way(11), role: "backward" },
        ],
        tags: vec![("type", "route")],
    }];

    write_test_pbf(&input, &test_nodes(), &test_ways(), &relations);

    let bbox = parse_bbox(BBOX_STR).expect("parse bbox");
    let region = Region::Bbox(bbox);
    let stats = extract(&input, &output, &region, ExtractStrategy::Smart, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    // type=route should NOT trigger smart behavior — way 11 stays excluded
    assert_eq!(way_ids(&c), vec![10, 12]);
    // Relation 302 is still included (it has way 10 as member, which is matched)
    assert_eq!(relation_ids(&c), vec![302]);
    // Nodes: 1,3 in bbox + 2 from way 10 deps. Node 4 NOT included.
    assert_eq!(node_ids(&c), vec![1, 2, 3]);
    assert_eq!(stats.nodes_from_relations, 0);
}

// ---------------------------------------------------------------------------
// Spatial blob filter tests
// ---------------------------------------------------------------------------
//
// These tests create multiple node blobs at distinct geographic locations
// to exercise the v2 indexdata spatial blob filter. When the extraction bbox
// overlaps only one blob, the pipeline skips decompression of the others.

/// Helper: write a PBF with nodes in separate blobs at distinct locations.
/// Each blob is flushed after adding its nodes to ensure separate blobs.
#[allow(clippy::cast_possible_truncation)]
fn write_multi_blob_pbf(path: &std::path::Path) {
    use pbfhogg::block_builder::{self, BlockBuilder};
    use pbfhogg::writer::{Compression, PbfWriter};

    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new().build().expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    // Blob 1: Copenhagen area (lat ~55.7, lon ~12.5)
    for i in 1..=10_i64 {
        bb.add_node(i, 557_000_000 + i as i32 * 1000, 125_000_000 + i as i32 * 1000, std::iter::empty::<(&str, &str)>(), None);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    // Blob 2: Stockholm area (lat ~59.3, lon ~18.0) — far from Copenhagen
    for i in 11..=20_i64 {
        bb.add_node(i, 593_000_000 + i as i32 * 1000, 180_000_000 + i as i32 * 1000, std::iter::empty::<(&str, &str)>(), None);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    // Blob 3: Berlin area (lat ~52.5, lon ~13.4) — far from Copenhagen
    for i in 21..=30_i64 {
        bb.add_node(i, 525_000_000 + i as i32 * 1000, 134_000_000 + i as i32 * 1000, std::iter::empty::<(&str, &str)>(), None);
    }
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    writer.flush().expect("flush");
}

/// Extract from a multi-blob PBF with a bbox covering only the Copenhagen blob.
/// The spatial blob filter should skip Stockholm and Berlin blobs entirely.
#[test]
fn spatial_filter_skips_distant_blobs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("multi_blob.osm.pbf");
    let output = dir.path().join("extracted.osm.pbf");

    write_multi_blob_pbf(&input);

    // Bbox covering Copenhagen area only
    let bbox = parse_bbox("12.4,55.6,12.7,55.8").expect("parse bbox");
    let region = Region::Bbox(bbox);
    let stats = extract(
        &input,
        &output,
        &region,
        ExtractStrategy::Simple,
        true,
        &CleanAttrs::default(),
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("extract");

    let c = read_all_elements(&output);

    // Only Copenhagen nodes (1-10) should be in the output
    let ids = node_ids(&c);
    assert_eq!(ids, (1..=10).collect::<Vec<_>>());
    assert_eq!(stats.nodes_in_bbox, 10);
    // Stockholm (11-20) and Berlin (21-30) nodes should not appear
    assert!(ids.iter().all(|&id| id <= 10), "no non-Copenhagen nodes should be present");
}

// ---------------------------------------------------------------------------
// Single-pass sorted tests — verify that the sorted fast path produces
// identical results to the two-pass unsorted path.
// ---------------------------------------------------------------------------

#[test]
fn simple_sorted_filters_nodes_by_bbox() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(&input, &test_nodes(), &[], &[]);

    let bbox = parse_bbox(BBOX_STR).expect("parse bbox");
    let region = Region::Bbox(bbox);
    let stats = extract(&input, &output, &region, ExtractStrategy::Simple, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 3]);
    assert_eq!(stats.nodes_in_bbox, 2);
    assert_eq!(stats.nodes_from_ways, 0);
}

#[test]
fn simple_sorted_includes_ways_with_nodes_in_bbox() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &[]);

    let bbox = parse_bbox(BBOX_STR).expect("parse bbox");
    let region = Region::Bbox(bbox);
    let stats = extract(&input, &output, &region, ExtractStrategy::Simple, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    assert_eq!(way_ids(&c), vec![10, 12]);
    assert_eq!(stats.ways_written, 2);
}

#[test]
fn simple_sorted_does_not_add_extra_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &[]);

    let bbox = parse_bbox(BBOX_STR).expect("parse bbox");
    let region = Region::Bbox(bbox);
    let stats = extract(&input, &output, &region, ExtractStrategy::Simple, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 3]);
    assert_eq!(stats.nodes_from_ways, 0);
}

#[test]
fn simple_sorted_includes_relations() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &test_relations());

    let bbox = parse_bbox(BBOX_STR).expect("parse bbox");
    let region = Region::Bbox(bbox);
    let stats = extract(&input, &output, &region, ExtractStrategy::Simple, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    assert_eq!(relation_ids(&c), vec![100]);
    assert_eq!(stats.relations_written, 1);
}

#[test]
fn simple_sorted_includes_relations_with_matched_ways() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let relations = vec![TestRelation {
        id: 200,
        members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
        tags: vec![("type", "multipolygon")],
    }];

    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &relations);

    let bbox = parse_bbox(BBOX_STR).expect("parse bbox");
    let region = Region::Bbox(bbox);
    let stats = extract(&input, &output, &region, ExtractStrategy::Simple, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    assert_eq!(relation_ids(&c), vec![200]);
    assert_eq!(stats.relations_written, 1);
}

#[test]
fn simple_sorted_empty_extract() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &test_relations());

    let bbox = parse_bbox("0.0,0.0,1.0,1.0").expect("parse bbox");
    let region = Region::Bbox(bbox);
    let stats = extract(&input, &output, &region, ExtractStrategy::Simple, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    assert!(c.nodes.is_empty());
    assert!(c.ways.is_empty());
    assert!(c.relations.is_empty());
    assert_eq!(stats.nodes_in_bbox, 0);
    assert_eq!(stats.ways_written, 0);
}

#[test]
fn simple_sorted_output_declares_sorted() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &test_relations());

    let bbox = parse_bbox(BBOX_STR).expect("parse bbox");
    let region = Region::Bbox(bbox);
    extract(&input, &output, &region, ExtractStrategy::Simple, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");

    let header = read_header(&output);
    assert!(header.is_sorted(), "output should declare Sort.Type_then_ID");
}

#[test]
fn simple_sorted_polygon_filters_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(&input, &test_nodes(), &[], &[]);

    let region = test_polygon_region();
    let stats = extract(&input, &output, &region, ExtractStrategy::Simple, true, &CleanAttrs::default(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("extract");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 3]);
    assert_eq!(stats.nodes_in_bbox, 2);
}

// ---------------------------------------------------------------------------
// Multi-extract (--config) integration tests
// ---------------------------------------------------------------------------

use pbfhogg::extract::{extract_multi, parse_extract_config, ExtractSlot};

#[test]
fn multi_extract_two_bbox_regions() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let out_a = dir.path().join("a.osm.pbf");
    let out_b = dir.path().join("b.osm.pbf");

    write_test_pbf_sorted(&input, &test_nodes(), &test_ways(), &test_relations());

    // Extract A: Copenhagen area (nodes 1, 3)
    let bbox_a = parse_bbox("12.4,55.6,12.7,55.8").expect("bbox_a");
    // Extract B: far east area (node 4)
    let bbox_b = parse_bbox("13.5,55.5,14.5,55.9").expect("bbox_b");

    let slots = vec![
        ExtractSlot {
            region: Region::Bbox(bbox_a),
            output: out_a.clone(),
        },
        ExtractSlot {
            region: Region::Bbox(bbox_b),
            output: out_b.clone(),
        },
    ];

    let all_stats = extract_multi(
        &input,
        &slots,
        ExtractStrategy::Simple,
        true,
        &CleanAttrs::default(),
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("extract_multi");

    assert_eq!(all_stats.len(), 2);

    // Extract A: nodes 1, 3 in bbox; ways 10, 12 match; relation 100 matches
    let ca = read_all_elements(&out_a);
    assert_eq!(node_ids(&ca), vec![1, 3]);
    assert_eq!(way_ids(&ca), vec![10, 12]);
    assert_eq!(relation_ids(&ca), vec![100]);
    assert_eq!(all_stats[0].nodes_in_bbox, 2);

    // Extract B: node 4 in bbox; way 11 refs [2,4] — only node 4 is in B's bbox
    // so way 11 matches. Relation 101 has member node 4.
    let cb = read_all_elements(&out_b);
    assert_eq!(node_ids(&cb), vec![4]);
    assert_eq!(way_ids(&cb), vec![11]);
    assert_eq!(relation_ids(&cb), vec![101]);
    assert_eq!(all_stats[1].nodes_in_bbox, 1);
}

#[test]
fn multi_extract_from_config_file() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");

    write_test_pbf_sorted(&input, &test_nodes(), &[], &[]);

    let config_json = format!(
        r#"{{
            "directory": "{}",
            "extracts": [
                {{ "output": "copenhagen.osm.pbf", "bbox": [12.4, 55.6, 12.7, 55.8] }},
                {{ "output": "east.osm.pbf", "bbox": [13.5, 55.5, 14.5, 55.9] }}
            ]
        }}"#,
        dir.path().display()
    );
    let config_path = dir.path().join("config.json");
    std::fs::write(&config_path, &config_json).expect("write config");

    let (_, slots) = parse_extract_config(&config_path).expect("parse config");
    assert_eq!(slots.len(), 2);

    let all_stats = extract_multi(
        &input,
        &slots,
        ExtractStrategy::Simple,
        false,
        &CleanAttrs::default(),
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("extract_multi");

    let ca = read_all_elements(&slots[0].output);
    assert_eq!(node_ids(&ca), vec![1, 3]);
    assert_eq!(all_stats[0].nodes_in_bbox, 2);

    let cb = read_all_elements(&slots[1].output);
    assert_eq!(node_ids(&cb), vec![4]);
    assert_eq!(all_stats[1].nodes_in_bbox, 1);
}
