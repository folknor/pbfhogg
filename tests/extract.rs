//! Extract (bbox) correctness tests.

mod common;

use common::{
    node_ids_id_only as node_ids, read_all_elements_id_only as read_all_elements,
    way_ids_id_only as way_ids, relation_ids_id_only as relation_ids,
    write_test_pbf, TestMember, TestNode, TestRelation, TestWay,
};
use pbfhogg::extract::{extract, parse_bbox, parse_geojson, PolygonRings, Region};
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
    let stats = extract(&input, &output, &region, true, Compression::default(), false).expect("extract");
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
    let stats = extract(&input, &output, &region, true, Compression::default(), false).expect("extract");
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
    let stats = extract(&input, &output, &region, true, Compression::default(), false).expect("extract");
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
    let stats = extract(&input, &output, &region, false, Compression::default(), false).expect("extract");
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
    let stats = extract(&input, &output, &region, false, Compression::default(), false).expect("extract");
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
    let stats = extract(&input, &output, &region, true, Compression::default(), false).expect("extract");
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
    let stats = extract(&input, &output, &region, false, Compression::default(), false).expect("extract");
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
    extract(&input, &output, &region, false, Compression::default(), false).expect("extract");
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
    let stats = extract(&input, &output, &region, true, Compression::default(), false).expect("extract");
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
    let stats = extract(&input, &output, &region, false, Compression::default(), false).expect("extract");
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

    let stats = extract(&input, &output, &region, true, Compression::default(), false).expect("extract");
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
    let stats = extract(&input, &output, &region, true, Compression::default(), false).expect("extract");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 3]);
    assert_eq!(stats.nodes_in_bbox, 2);
}
