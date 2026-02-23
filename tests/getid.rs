//! getid / removeid correctness tests.

use std::path::Path;

use pbfhogg::block_builder::{self, BlockBuilder, MemberData, MemberType};
use pbfhogg::getid::{getid, parse_ids, removeid};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
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
    id: i64,
    member_type: MemberType,
    role: &'static str,
}

fn write_test_pbf(path: &Path, nodes: &[TestNode], ways: &[TestWay], relations: &[TestRelation]) {
    let mut writer = PbfWriter::to_path(path, Compression::default()).expect("create writer");
    let header = block_builder::build_header(None, None, None, None).expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    for n in nodes {
        if !bb.can_add_node() {
            if let Some(bytes) = bb.take().expect("take") {
                writer.write_primitive_block(&bytes).expect("write block");
            }
        }
        bb.add_node(n.id, n.lat, n.lon, &n.tags, None);
    }
    if !bb.is_empty() {
        if let Some(bytes) = bb.take().expect("take") {
            writer.write_primitive_block(&bytes).expect("write block");
        }
    }

    for w in ways {
        if !bb.can_add_way() {
            if let Some(bytes) = bb.take().expect("take") {
                writer.write_primitive_block(&bytes).expect("write block");
            }
        }
        bb.add_way(w.id, &w.tags, &w.refs, None);
    }
    if !bb.is_empty() {
        if let Some(bytes) = bb.take().expect("take") {
            writer.write_primitive_block(&bytes).expect("write block");
        }
    }

    for r in relations {
        if !bb.can_add_relation() {
            if let Some(bytes) = bb.take().expect("take") {
                writer.write_primitive_block(&bytes).expect("write block");
            }
        }
        let members: Vec<MemberData<'_>> = r
            .members
            .iter()
            .map(|m| MemberData {
                member_id: m.id,
                member_type: m.member_type,
                role: m.role,
            })
            .collect();
        bb.add_relation(r.id, &r.tags, &members, None);
    }
    if !bb.is_empty() {
        if let Some(bytes) = bb.take().expect("take") {
            writer.write_primitive_block(&bytes).expect("write block");
        }
    }

    writer.flush().expect("flush");
}

#[derive(Debug)]
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

fn ids(strs: &[&str]) -> Vec<String> {
    strs.iter().map(|s| s.to_string()).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn getid_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "b")] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![("name", "c")] },
        ],
        &[],
        &[],
    );

    let id_set = parse_ids(&ids(&["n1", "n3"])).expect("parse ids");
    let stats = getid(&input, &output, &id_set, false).expect("getid");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 3]);
    assert_eq!(stats.nodes_written, 2);
}

#[test]
fn getid_mixed_types() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] },
            TestWay { id: 11, refs: vec![1], tags: vec![] },
        ],
        &[
            TestRelation { id: 100, members: vec![], tags: vec![("type", "route")] },
        ],
    );

    let id_set = parse_ids(&ids(&["n2", "w10", "r100"])).expect("parse ids");
    let stats = getid(&input, &output, &id_set, false).expect("getid");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![2]);
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(relation_ids(&c), vec![100]);
    assert_eq!(stats.nodes_written, 1);
    assert_eq!(stats.ways_written, 1);
    assert_eq!(stats.relations_written, 1);
}

#[test]
fn getid_add_referenced() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")] },
        ],
        &[],
    );

    // Without --add-referenced: only the way
    let id_set = parse_ids(&ids(&["w10"])).expect("parse ids");
    let stats = getid(&input, &output, &id_set, false).expect("getid");
    let c = read_all_elements(&output);
    assert!(node_ids(&c).is_empty());
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(stats.ways_written, 1);
    assert_eq!(stats.nodes_written, 0);

    // With --add-referenced: way + its referenced nodes
    let output2 = dir.path().join("output2.osm.pbf");
    let stats = getid(&input, &output2, &id_set, true).expect("getid");
    let c = read_all_elements(&output2);
    assert_eq!(node_ids(&c), vec![1, 2, 3]);
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(stats.nodes_written, 3);
    assert_eq!(stats.ways_written, 1);
}

#[test]
fn removeid_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
        ],
        &[],
        &[],
    );

    let id_set = parse_ids(&ids(&["n2"])).expect("parse ids");
    let stats = removeid(&input, &output, &id_set).expect("removeid");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 3]);
    assert_eq!(stats.nodes_written, 2);
}

#[test]
fn removeid_mixed_types() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![] },
            TestWay { id: 11, refs: vec![1], tags: vec![] },
        ],
        &[
            TestRelation { id: 100, members: vec![], tags: vec![] },
            TestRelation { id: 101, members: vec![], tags: vec![] },
        ],
    );

    let id_set = parse_ids(&ids(&["n1", "w11", "r100"])).expect("parse ids");
    let stats = removeid(&input, &output, &id_set).expect("removeid");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![2]);
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(relation_ids(&c), vec![101]);
    assert_eq!(stats.nodes_written, 1);
    assert_eq!(stats.ways_written, 1);
    assert_eq!(stats.relations_written, 1);
}

#[test]
fn getid_empty_result() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
        ],
        &[],
        &[],
    );

    let id_set = parse_ids(&ids(&["n999"])).expect("parse ids");
    let stats = getid(&input, &output, &id_set, false).expect("getid");
    let c = read_all_elements(&output);

    assert!(node_ids(&c).is_empty());
    assert_eq!(stats.nodes_written, 0);
}

#[test]
fn getid_preserves_tags() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "test"), ("amenity", "bench")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[],
        &[],
    );

    let id_set = parse_ids(&ids(&["n1"])).expect("parse ids");
    getid(&input, &output, &id_set, false).expect("getid");
    let c = read_all_elements(&output);

    assert_eq!(c.nodes.len(), 1);
    let (id, ref tags) = c.nodes[0];
    assert_eq!(id, 1);
    assert_eq!(
        tags,
        &[
            ("name".to_string(), "test".to_string()),
            ("amenity".to_string(), "bench".to_string()),
        ]
    );
}

#[test]
fn getid_from_id_file() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let id_file = dir.path().join("ids.txt");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![] },
        ],
        &[],
    );

    std::fs::write(&id_file, "# comment\nn1\nw10\n\nn3\n").expect("write id file");

    let id_set = pbfhogg::getid::parse_ids_from_file(&id_file).expect("parse file");
    let stats = getid(&input, &output, &id_set, false).expect("getid");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 3]);
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(stats.nodes_written, 2);
    assert_eq!(stats.ways_written, 1);
}

#[test]
fn getid_add_referenced_plus_direct_node() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![2, 3], tags: vec![] },
        ],
        &[],
    );

    // Request node 1 directly + way 10 with refs → should get nodes 1,2,3 + way 10
    let id_set = parse_ids(&ids(&["n1", "w10"])).expect("parse ids");
    let stats = getid(&input, &output, &id_set, true).expect("getid");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2, 3]);
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(stats.nodes_written, 3);
    assert_eq!(stats.ways_written, 1);
}
