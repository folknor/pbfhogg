//! Extract (bbox) correctness tests.

use std::path::Path;

use pbfhogg::block_builder::{self, BlockBuilder, MemberData};
use pbfhogg::extract::{extract, parse_bbox};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element, MemberId};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers (same pattern as tests/tags_filter.rs)
// ---------------------------------------------------------------------------

struct TestNode {
    id: i64,
    lat: i32,
    lon: i32,
    tags: Vec<(&'static str, &'static str)>,
}

struct TestWay {
    id: i64,
    refs: Vec<i64>,
    tags: Vec<(&'static str, &'static str)>,
}

struct TestRelation {
    id: i64,
    members: Vec<TestMember>,
    tags: Vec<(&'static str, &'static str)>,
}

struct TestMember {
    id: MemberId,
    role: &'static str,
}

fn write_test_pbf(path: &Path, nodes: &[TestNode], ways: &[TestWay], relations: &[TestRelation]) {
    let mut writer = PbfWriter::to_path(path, Compression::default()).expect("create writer");
    let header = block_builder::build_header(None, None, None, None).expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    for n in nodes {
        if !bb.can_add_node()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(&bytes).expect("write block");
        }
        bb.add_node(n.id, n.lat, n.lon, &n.tags, None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(&bytes).expect("write block");
    }

    for w in ways {
        if !bb.can_add_way()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(&bytes).expect("write block");
        }
        bb.add_way(w.id, &w.tags, &w.refs, None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(&bytes).expect("write block");
    }

    for r in relations {
        if !bb.can_add_relation()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(&bytes).expect("write block");
        }
        let members: Vec<MemberData<'_>> = r
            .members
            .iter()
            .map(|m| MemberData {
                id: m.id,
                role: m.role,
            })
            .collect();
        bb.add_relation(r.id, &r.tags, &members, None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(&bytes).expect("write block");
    }

    writer.flush().expect("flush");
}

#[derive(Debug)]
#[allow(clippy::type_complexity)]
struct PbfContents {
    nodes: Vec<(i64, Vec<(String, String)>)>,
    ways: Vec<(i64, Vec<i64>, Vec<(String, String)>)>,
    relations: Vec<(i64, Vec<(String, String)>)>,
}

fn read_all_elements(path: &Path) -> PbfContents {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut contents = PbfContents {
        nodes: Vec::new(),
        ways: Vec::new(),
        relations: Vec::new(),
    };

    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            for element in block.elements() {
                match element {
                    Element::DenseNode(dn) => {
                        let tags: Vec<(String, String)> = dn
                            .tags()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect();
                        contents.nodes.push((dn.id(), tags));
                    }
                    Element::Node(n) => {
                        let tags: Vec<(String, String)> = n
                            .tags()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect();
                        contents.nodes.push((n.id(), tags));
                    }
                    Element::Way(w) => {
                        let tags: Vec<(String, String)> =
                            w.tags().map(|(k, v)| (k.to_string(), v.to_string())).collect();
                        let refs: Vec<i64> = w.refs().collect();
                        contents.ways.push((w.id(), refs, tags));
                    }
                    Element::Relation(r) => {
                        let tags: Vec<(String, String)> =
                            r.tags().map(|(k, v)| (k.to_string(), v.to_string())).collect();
                        contents.relations.push((r.id(), tags));
                    }
                }
            }
        }
    }

    contents
}

fn node_ids(c: &PbfContents) -> Vec<i64> {
    c.nodes.iter().map(|(id, _)| *id).collect()
}

fn way_ids(c: &PbfContents) -> Vec<i64> {
    c.ways.iter().map(|(id, _, _)| *id).collect()
}

fn relation_ids(c: &PbfContents) -> Vec<i64> {
    c.relations.iter().map(|(id, _)| *id).collect()
}

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
    let stats = extract(&input, &output, &bbox, true).expect("extract");
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
    let stats = extract(&input, &output, &bbox, true).expect("extract");
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
    let stats = extract(&input, &output, &bbox, true).expect("extract");
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
    let stats = extract(&input, &output, &bbox, false).expect("extract");
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
    let stats = extract(&input, &output, &bbox, false).expect("extract");
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
    let stats = extract(&input, &output, &bbox, true).expect("extract");
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
    let stats = extract(&input, &output, &bbox, false).expect("extract");
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
    extract(&input, &output, &bbox, false).expect("extract");
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
