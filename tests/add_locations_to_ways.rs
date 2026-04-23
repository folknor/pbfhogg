//! Integration tests for the add-locations-to-ways command.

mod common;

use std::path::Path;

use common::{
    TestMember, TestNode, TestRelation as CommonTestRelation, TestWay, assert_elements_equivalent,
    generate_nodes, generate_ways, write_multi_block_test_pbf,
};
use pbfhogg::altw::add_locations_to_ways;
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
    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new()
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    for n in nodes {
        if !bb.can_add_node()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(bytes).expect("write block");
        }
        bb.add_node(n.id, n.lat, n.lon, n.tags.iter().copied(), None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(bytes).expect("write block");
    }

    for w in ways {
        if !bb.can_add_way()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(bytes).expect("write block");
        }
        bb.add_way(w.id, w.tags.iter().copied(), &w.refs, None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(bytes).expect("write block");
    }

    for r in relations {
        if !bb.can_add_relation()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(bytes).expect("write block");
        }
        let members: Vec<MemberData<'_>> = r
            .members
            .iter()
            .map(|(id, role)| MemberData { id: *id, role })
            .collect();
        bb.add_relation(r.id, r.tags.iter().copied(), &members, None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(bytes).expect("write block");
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
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 551_000_000,
            lon: 121_000_000,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 3,
            lat: 552_000_000,
            lon: 122_000_000,
            tags: vec![("amenity", "cafe")],
            meta: None,
        },
    ]
}

fn test_ways() -> Vec<TestWay> {
    vec![TestWay {
        id: 10,
        refs: vec![1, 2, 3],
        tags: vec![("highway", "primary")],
        meta: None,
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

    let stats = add_locations_to_ways(
        &input,
        &output,
        true,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::default(),
    )
    .expect("add locations");
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
    add_locations_to_ways(
        &input,
        &output,
        true,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::default(),
    )
    .expect("add locations");

    let reader = BlobReader::from_path(&output).expect("open output");
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmHeader(header) = blob.decode().expect("decode") {
            let features: Vec<&str> = header
                .optional_features()
                .iter()
                .map(String::as_str)
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

    let stats = add_locations_to_ways(
        &input,
        &output,
        false,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::default(),
    )
    .expect("add locations");

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

    let stats = add_locations_to_ways(
        &input,
        &output,
        true,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::default(),
    )
    .expect("add locations");

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
        meta: None,
    }];
    let ways = vec![TestWay {
        id: 10,
        refs: vec![1, 99],
        tags: vec![("highway", "primary")],
        meta: None,
    }];

    write_test_pbf(&input, &nodes, &ways, &[]);

    let stats = add_locations_to_ways(
        &input,
        &output,
        true,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::default(),
    )
    .expect("add locations");
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

    let stats = add_locations_to_ways(
        &input,
        &output,
        true,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::default(),
    )
    .expect("add locations");
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

// ---------------------------------------------------------------------------
// Passthrough tests (indexdata PBFs)
// ---------------------------------------------------------------------------

/// Write a PBF with indexdata embedded in BlobHeaders.
///
/// Uses the pipelined writer which automatically embeds indexdata via
/// `scan_block_ids` during blob framing.
fn write_test_pbf_indexed(
    path: &Path,
    nodes: &[TestNode],
    ways: &[TestWay],
    relations: &[TestRelation],
) {
    let header = block_builder::HeaderBuilder::new()
        .sorted()
        .build()
        .expect("build header");
    let mut writer =
        PbfWriter::to_path(path, Compression::default(), &header).expect("create writer");

    let mut bb = BlockBuilder::new();

    for n in nodes {
        if !bb.can_add_node()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(bytes).expect("write block");
        }
        bb.add_node(n.id, n.lat, n.lon, n.tags.iter().copied(), None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(bytes).expect("write block");
    }

    for w in ways {
        if !bb.can_add_way()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(bytes).expect("write block");
        }
        bb.add_way(w.id, w.tags.iter().copied(), &w.refs, None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(bytes).expect("write block");
    }

    for r in relations {
        if !bb.can_add_relation()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(bytes).expect("write block");
        }
        let members: Vec<MemberData<'_>> = r
            .members
            .iter()
            .map(|(id, role)| MemberData { id: *id, role })
            .collect();
        bb.add_relation(r.id, r.tags.iter().copied(), &members, None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(bytes).expect("write block");
    }

    writer.flush().expect("flush");
}

#[test]
fn passthrough_basic_with_indexdata() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_indexed(&input, &test_nodes(), &test_ways(), &[]);

    let stats = add_locations_to_ways(
        &input,
        &output,
        true,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::default(),
    )
    .expect("add locations");
    assert_eq!(stats.ways_written, 1);
    assert_eq!(stats.missing_locations, 0);
    assert!(stats.blobs_passthrough > 0, "expected passthrough blobs");

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
fn passthrough_relations_preserved() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let relations = vec![TestRelation {
        id: 100,
        members: vec![(MemberId::Way(10), "outer")],
        tags: vec![("type", "multipolygon")],
    }];

    write_test_pbf_indexed(&input, &test_nodes(), &test_ways(), &relations);

    let stats = add_locations_to_ways(
        &input,
        &output,
        true,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::default(),
    )
    .expect("add locations");
    assert_eq!(stats.relations_written, 1);
    assert!(
        stats.blobs_passthrough >= 2,
        "expected node + relation passthrough"
    );

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

#[test]
fn passthrough_drop_untagged_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_indexed(&input, &test_nodes(), &test_ways(), &[]);

    let stats = add_locations_to_ways(
        &input,
        &output,
        false,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::default(),
    )
    .expect("add locations");

    // Node 2 has no tags → dropped
    assert_eq!(stats.nodes_read, 3);
    assert_eq!(stats.nodes_written, 2);
    assert_eq!(stats.nodes_dropped, 1);
    // Relation blobs passthrough, node blobs decoded
    assert!(stats.blobs_decoded > 0, "expected decoded node blobs");
}

#[test]
fn passthrough_keep_untagged_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_indexed(&input, &test_nodes(), &test_ways(), &[]);

    let stats = add_locations_to_ways(
        &input,
        &output,
        true,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::default(),
    )
    .expect("add locations");

    assert_eq!(stats.nodes_read, 3);
    assert_eq!(stats.nodes_written, 3);
    assert_eq!(stats.nodes_dropped, 0);
    assert!(stats.blobs_passthrough > 0, "expected passthrough blobs");
}

#[test]
fn drop_untagged_keeps_relation_member_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let nodes = vec![
        TestNode {
            id: 1,
            lat: 550_000_000,
            lon: 120_000_000,
            tags: vec![("name", "tagged")],
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 551_000_000,
            lon: 121_000_000,
            tags: vec![],
            meta: None,
        },
    ];
    let ways = vec![TestWay {
        id: 10,
        refs: vec![1],
        tags: vec![("highway", "service")],
        meta: None,
    }];
    let relations = vec![TestRelation {
        id: 100,
        members: vec![(MemberId::Node(2), "label")],
        tags: vec![("type", "site")],
    }];

    write_test_pbf(&input, &nodes, &ways, &relations);
    let stats = add_locations_to_ways(
        &input,
        &output,
        false,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::default(),
    )
    .expect("add locations");

    assert_eq!(stats.nodes_read, 2);
    assert_eq!(stats.nodes_written, 2);
    assert_eq!(stats.nodes_dropped, 0);

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
    assert!(node_ids.contains(&1));
    assert!(
        node_ids.contains(&2),
        "untagged relation-member node was dropped"
    );
}

#[test]
fn passthrough_drop_untagged_keeps_relation_member_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    let nodes = vec![
        TestNode {
            id: 1,
            lat: 550_000_000,
            lon: 120_000_000,
            tags: vec![("name", "tagged")],
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 551_000_000,
            lon: 121_000_000,
            tags: vec![],
            meta: None,
        },
    ];
    let ways = vec![TestWay {
        id: 10,
        refs: vec![1],
        tags: vec![("highway", "service")],
        meta: None,
    }];
    let relations = vec![TestRelation {
        id: 100,
        members: vec![(MemberId::Node(2), "label")],
        tags: vec![("type", "site")],
    }];

    write_test_pbf_indexed(&input, &nodes, &ways, &relations);
    let stats = add_locations_to_ways(
        &input,
        &output,
        false,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::default(),
    )
    .expect("add locations");

    assert_eq!(stats.nodes_read, 2);
    assert_eq!(stats.nodes_written, 2);
    assert_eq!(stats.nodes_dropped, 0);

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
    assert!(node_ids.contains(&1));
    assert!(
        node_ids.contains(&2),
        "untagged relation-member node was dropped"
    );
}

// ---------------------------------------------------------------------------
// O_DIRECT variant
// ---------------------------------------------------------------------------

#[cfg(feature = "linux-direct-io")]
#[test]
fn basic_locations_added_direct_io() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);

    let result = add_locations_to_ways(
        &input,
        &output,
        true,
        Compression::default(),
        true,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::default(),
    );

    match result {
        Ok(stats) => {
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
        Err(e) if common::is_einval(&*e) => {
            eprintln!("O_DIRECT not supported on this filesystem, skipping test");
        }
        Err(e) => panic!("unexpected error: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Sparse index variants
// ---------------------------------------------------------------------------

#[test]
fn basic_locations_added_sparse() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(&input, &test_nodes(), &test_ways(), &[]);

    let stats = add_locations_to_ways(
        &input,
        &output,
        true,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::Sparse,
    )
    .expect("add locations");
    assert_eq!(stats.ways_written, 1);
    assert_eq!(stats.missing_locations, 0);

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

#[allow(clippy::cast_possible_wrap)]
#[test]
fn backend_parity_dense_sparse_external_auto() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let out_dense = dir.path().join("out_dense.osm.pbf");
    let out_sparse = dir.path().join("out_sparse.osm.pbf");
    let out_external = dir.path().join("out_external.osm.pbf");
    let out_auto = dir.path().join("out_auto.osm.pbf");

    let mut nodes = generate_nodes(18, 1);
    for idx in [0_usize, 3, 6, 9, 12, 15] {
        nodes[idx].tags = vec![("name", "kept")];
    }

    let mut ways = generate_ways(5, 1_000, 3, 1);
    for (i, way) in ways.iter_mut().enumerate() {
        let start = 1 + i as i64 * 3;
        way.refs = vec![start, start + 1, start + 2];
        way.tags = if i % 2 == 0 {
            vec![("highway", "primary")]
        } else {
            vec![("highway", "service")]
        };
    }

    let relations = vec![CommonTestRelation {
        id: 300,
        members: vec![
            TestMember {
                id: MemberId::Way(1_000),
                role: "outer",
            },
            TestMember {
                id: MemberId::Node(18),
                role: "label",
            },
        ],
        tags: vec![("type", "site")],
        meta: None,
    }];

    write_multi_block_test_pbf(&input, &nodes, &ways, &relations, 5);

    add_locations_to_ways(
        &input,
        &out_dense,
        false,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::Dense,
    )
    .expect("dense backend");
    add_locations_to_ways(
        &input,
        &out_sparse,
        false,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::Sparse,
    )
    .expect("sparse backend");
    add_locations_to_ways(
        &input,
        &out_external,
        false,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::External,
    )
    .expect("external backend");
    add_locations_to_ways(
        &input,
        &out_auto,
        false,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::Auto,
    )
    .expect("auto backend");

    assert_elements_equivalent(&out_dense, &out_sparse);
    assert_elements_equivalent(&out_dense, &out_external);
    assert_elements_equivalent(&out_external, &out_auto);
}

#[test]
fn passthrough_basic_with_indexdata_sparse() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_indexed(&input, &test_nodes(), &test_ways(), &[]);

    let stats = add_locations_to_ways(
        &input,
        &output,
        true,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::Sparse,
    )
    .expect("add locations");
    assert_eq!(stats.ways_written, 1);
    assert_eq!(stats.missing_locations, 0);
    assert!(stats.blobs_passthrough > 0, "expected passthrough blobs");

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
fn missing_node_refs_get_zero_coordinates_sparse() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    // Way references node 999 which doesn't exist
    let ways = vec![TestWay {
        id: 10,
        refs: vec![1, 999, 3],
        tags: vec![("highway", "primary")],
        meta: None,
    }];
    write_test_pbf(&input, &test_nodes(), &ways, &[]);

    let stats = add_locations_to_ways(
        &input,
        &output,
        true,
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
        pbfhogg::altw::IndexType::Sparse,
    )
    .expect("add locations");
    assert_eq!(stats.missing_locations, 1);

    let reader = BlobReader::from_path(&output).expect("open output");
    let mut found_way = false;
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode") {
            for element in block.elements() {
                if let Element::Way(w) = element {
                    let locs: Vec<(i32, i32)> = w
                        .node_locations()
                        .map(|loc| (loc.decimicro_lat(), loc.decimicro_lon()))
                        .collect();
                    assert_eq!(
                        locs,
                        vec![
                            (550_000_000, 120_000_000),
                            (0, 0),
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
