//! Integration tests for the add-locations-to-ways command.

mod common;

use std::path::Path;

use common::{TestNode, TestWay};
use pbfhogg::add_locations_to_ways::add_locations_to_ways;
use pbfhogg::block_builder::{self, BlockBuilder, MemberData};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element, MemberId};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// NOTE: This file uses a local TestRelation and write_test_pbf instead of the
// shared versions in tests/common/mod.rs because the TestRelation here uses
// tuple-based members `Vec<(MemberId, &str)>` rather than the `TestMember`
// struct used everywhere else. This was the original design of this test file
// and changing it would require updating all test call sites.

struct TestRelation {
    id: i64,
    members: Vec<(MemberId, &'static str)>,
    tags: Vec<(&'static str, &'static str)>,
}

fn write_test_pbf(path: &Path, nodes: &[TestNode], ways: &[TestWay], relations: &[TestRelation]) {
    let mut writer = PbfWriter::to_path(path, Compression::default()).expect("create writer");
    let header = block_builder::build_header(None, None, None, None, &[]).expect("build header");
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
            .map(|(id, role)| MemberData { id: *id, role })
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

// ---------------------------------------------------------------------------
// Test data
// ---------------------------------------------------------------------------

fn test_nodes() -> Vec<TestNode> {
    vec![
        TestNode {
            id: 1,
            lat: 550_000_000,
            lon: 120_000_000,
            tags: vec![("name", "tagged_node")],
        },
        TestNode {
            id: 2,
            lat: 551_000_000,
            lon: 121_000_000,
            tags: vec![],
        },
        TestNode {
            id: 3,
            lat: 552_000_000,
            lon: 122_000_000,
            tags: vec![("amenity", "cafe")],
        },
    ]
}

fn test_ways() -> Vec<TestWay> {
    vec![TestWay {
        id: 10,
        refs: vec![1, 2, 3],
        tags: vec![("highway", "primary")],
    }]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn basic_locations_added_to_ways() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);

    let stats = add_locations_to_ways(&input, &output, true).expect("add locations");
    assert_eq!(stats.ways_written, 1);
    assert_eq!(stats.missing_locations, 0);

    // Read output and verify way has locations
    let reader = BlobReader::from_path(&output).expect("open output");
    let mut found_way = false;
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode") {
            for element in block.elements() {
                if let Element::Way(w) = element {
                    assert_eq!(w.id(), 10);
                    let locs: Vec<(i32, i32)> = w
                        .node_locations()
                        .map(|loc| (loc.decimicro_lat(), loc.decimicro_lon()))
                        .collect();
                    assert_eq!(
                        locs,
                        vec![
                            (550_000_000, 120_000_000),
                            (551_000_000, 121_000_000),
                            (552_000_000, 122_000_000),
                        ]
                    );
                    found_way = true;
                }
            }
        }
    }
    assert!(found_way, "way not found in output");
}

#[test]
fn header_has_locations_on_ways_feature() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);
    add_locations_to_ways(&input, &output, true).expect("add locations");

    let reader = BlobReader::from_path(&output).expect("open output");
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmHeader(header) = blob.decode().expect("decode") {
            let features: Vec<&str> = header
                .optional_features()
                .iter()
                .map(|s| s.as_ref())
                .collect();
            assert!(
                features.contains(&"LocationsOnWays"),
                "LocationsOnWays not in optional features: {features:?}"
            );
            return;
        }
    }
    panic!("no header found in output");
}

#[test]
fn drop_untagged_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);

    let stats = add_locations_to_ways(&input, &output, false).expect("add locations");

    // Node 2 has no tags → dropped
    assert_eq!(stats.nodes_read, 3);
    assert_eq!(stats.nodes_written, 2);
    assert_eq!(stats.nodes_dropped, 1);

    // Verify output has only nodes 1 and 3
    let reader = BlobReader::from_path(&output).expect("open output");
    let mut node_ids: Vec<i64> = Vec::new();
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode") {
            for element in block.elements() {
                match element {
                    Element::DenseNode(dn) => node_ids.push(dn.id()),
                    Element::Node(n) => node_ids.push(n.id()),
                    _ => {}
                }
            }
        }
    }
    assert_eq!(node_ids, vec![1, 3]);
}

#[test]
fn keep_untagged_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);

    let stats = add_locations_to_ways(&input, &output, true).expect("add locations");

    assert_eq!(stats.nodes_read, 3);
    assert_eq!(stats.nodes_written, 3);
    assert_eq!(stats.nodes_dropped, 0);
}

#[test]
fn missing_node_refs_get_zero_coordinates() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    // Way references node 99 which doesn't exist
    let nodes = vec![TestNode {
        id: 1,
        lat: 550_000_000,
        lon: 120_000_000,
        tags: vec![],
    }];
    let ways = vec![TestWay {
        id: 10,
        refs: vec![1, 99],
        tags: vec![("highway", "primary")],
    }];

    write_test_pbf(&input, &nodes, &ways, &[]);

    let stats = add_locations_to_ways(&input, &output, true).expect("add locations");
    assert_eq!(stats.missing_locations, 1);

    // Verify the missing ref got (0, 0)
    let reader = BlobReader::from_path(&output).expect("open output");
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode") {
            for element in block.elements() {
                if let Element::Way(w) = element {
                    let locs: Vec<(i32, i32)> = w
                        .node_locations()
                        .map(|loc| (loc.decimicro_lat(), loc.decimicro_lon()))
                        .collect();
                    assert_eq!(locs, vec![(550_000_000, 120_000_000), (0, 0)]);
                    return;
                }
            }
        }
    }
    panic!("way not found in output");
}

#[test]
fn relations_preserved() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let relations = vec![TestRelation {
        id: 100,
        members: vec![(MemberId::Way(10), "outer")],
        tags: vec![("type", "multipolygon")],
    }];

    write_test_pbf(&input, &test_nodes(), &test_ways(), &relations);

    let stats = add_locations_to_ways(&input, &output, true).expect("add locations");
    assert_eq!(stats.relations_written, 1);

    // Verify relation exists in output
    let reader = BlobReader::from_path(&output).expect("open output");
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode") {
            for element in block.elements() {
                if let Element::Relation(r) = element {
                    assert_eq!(r.id(), 100);
                    return;
                }
            }
        }
    }
    panic!("relation not found in output");
}
