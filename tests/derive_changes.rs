//! derive-changes correctness tests.

use std::path::Path;

use pbfhogg::block_builder::{self, BlockBuilder, MemberData};
use pbfhogg::MemberId;
use pbfhogg::derive_changes::derive_changes;
use pbfhogg::merge::merge;
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
    id: MemberId,
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
                id: m.id,
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
    nodes: Vec<(i64, i32, i32, Vec<(String, String)>)>,
    ways: Vec<(i64, Vec<i64>, Vec<(String, String)>)>,
    relations: Vec<(i64, Vec<(i64, String, String)>, Vec<(String, String)>)>,
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
                        contents
                            .nodes
                            .push((dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), tags));
                    }
                    Element::Node(n) => {
                        let tags: Vec<(String, String)> = n
                            .tags()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect();
                        contents
                            .nodes
                            .push((n.id(), n.decimicro_lat(), n.decimicro_lon(), tags));
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
                        let members: Vec<(i64, String, String)> = r
                            .members()
                            .map(|m| {
                                let type_str = match m.id.member_type() {
                                    pbfhogg::MemberType::Node => "node",
                                    pbfhogg::MemberType::Way => "way",
                                    pbfhogg::MemberType::Relation => "relation",
                                };
                                (
                                    m.id.id(),
                                    type_str.to_string(),
                                    m.role().unwrap_or("").to_string(),
                                )
                            })
                            .collect();
                        contents.relations.push((r.id(), members, tags));
                    }
                }
            }
        }
    }

    contents
}

fn node_ids(c: &PbfContents) -> Vec<i64> {
    c.nodes.iter().map(|(id, _, _, _)| *id).collect()
}

fn way_ids(c: &PbfContents) -> Vec<i64> {
    c.ways.iter().map(|(id, _, _)| *id).collect()
}

fn relation_ids(c: &PbfContents) -> Vec<i64> {
    c.relations.iter().map(|(id, _, _)| *id).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn identical_files_no_changes() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    let nodes = [
        TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")] },
        TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
    ];
    let ways = [
        TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] },
    ];

    write_test_pbf(&old, &nodes, &ways, &[]);
    write_test_pbf(&new, &nodes, &ways, &[]);

    let stats = derive_changes(&old, &new, &osc).expect("derive");
    assert_eq!(stats.creates, 0);
    assert_eq!(stats.modifies, 0);
    assert_eq!(stats.deletes, 0);
}

#[test]
fn create_only() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf(
        &old,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
        &[],
        &[],
    );
    write_test_pbf(
        &new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "new")] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] }],
        &[],
    );

    let stats = derive_changes(&old, &new, &osc).expect("derive");
    assert_eq!(stats.creates, 2); // node 2 + way 10
    assert_eq!(stats.modifies, 0);
    assert_eq!(stats.deletes, 0);
}

#[test]
fn delete_only() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf(
        &old,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![] }],
        &[],
    );
    write_test_pbf(
        &new,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
        &[],
        &[],
    );

    let stats = derive_changes(&old, &new, &osc).expect("derive");
    assert_eq!(stats.creates, 0);
    assert_eq!(stats.modifies, 0);
    assert_eq!(stats.deletes, 2); // node 2 + way 10
}

#[test]
fn modify_node_coords() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf(
        &old,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
        &[],
        &[],
    );
    write_test_pbf(
        &new,
        &[TestNode { id: 1, lat: 150_000_000, lon: 250_000_000, tags: vec![] }],
        &[],
        &[],
    );

    let stats = derive_changes(&old, &new, &osc).expect("derive");
    assert_eq!(stats.creates, 0);
    assert_eq!(stats.modifies, 1);
    assert_eq!(stats.deletes, 0);
}

#[test]
fn modify_node_tags() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf(
        &old,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "old")] }],
        &[],
        &[],
    );
    write_test_pbf(
        &new,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "new")] }],
        &[],
        &[],
    );

    let stats = derive_changes(&old, &new, &osc).expect("derive");
    assert_eq!(stats.modifies, 1);
}

#[test]
fn modify_way_refs() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf(
        &old,
        &[],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] }],
        &[],
    );
    write_test_pbf(
        &new,
        &[],
        &[TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")] }],
        &[],
    );

    let stats = derive_changes(&old, &new, &osc).expect("derive");
    assert_eq!(stats.modifies, 1);
}

#[test]
fn modify_relation_members() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf(
        &old,
        &[],
        &[],
        &[TestRelation {
            id: 100,
            members: vec![TestMember { id: MemberId::Node(1), role: "stop" }],
            tags: vec![("type", "route")],
        }],
    );
    write_test_pbf(
        &new,
        &[],
        &[],
        &[TestRelation {
            id: 100,
            members: vec![
                TestMember { id: MemberId::Node(1), role: "stop" },
                TestMember { id: MemberId::Way(2), role: "outer" },
            ],
            tags: vec![("type", "route")],
        }],
    );

    let stats = derive_changes(&old, &new, &osc).expect("derive");
    assert_eq!(stats.modifies, 1);
}

#[test]
fn mixed_create_modify_delete() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");

    write_test_pbf(
        &old,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "one")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")] }],
        &[],
    );
    write_test_pbf(
        &new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "ONE")] }, // modified tag
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] }, // unchanged
            // node 3 deleted
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![] }, // created
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] }], // modified refs
        &[],
    );

    let stats = derive_changes(&old, &new, &osc).expect("derive");
    assert_eq!(stats.creates, 1);  // node 4
    assert_eq!(stats.modifies, 2); // node 1 + way 10
    assert_eq!(stats.deletes, 1);  // node 3
}

/// Full roundtrip: old → derive_changes → osc → merge(old, osc) → result ≈ new
#[test]
fn roundtrip_with_merge() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    let osc = dir.path().join("changes.osc.gz");
    let result = dir.path().join("result.osm.pbf");

    write_test_pbf(
        &old,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "one")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![("to_delete", "yes")] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] },
        ],
        &[],
    );
    write_test_pbf(
        &new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "ONE")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 5, lat: 140_000_000, lon: 240_000_000, tags: vec![("new", "yes")] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2, 5], tags: vec![("highway", "secondary")] },
        ],
        &[],
    );

    // Derive changes
    let stats = derive_changes(&old, &new, &osc).expect("derive");
    assert_eq!(stats.creates, 1);  // node 5
    assert_eq!(stats.modifies, 2); // node 1 (tags) + way 10 (refs + tags)
    assert_eq!(stats.deletes, 1);  // node 3

    // Apply changes back to old → should produce equivalent of new
    merge(&old, &osc, &result).expect("merge");

    let result_contents = read_all_elements(&result);
    let new_contents = read_all_elements(&new);

    // Compare node IDs and data
    assert_eq!(node_ids(&result_contents), node_ids(&new_contents));
    for (r, n) in result_contents.nodes.iter().zip(new_contents.nodes.iter()) {
        assert_eq!(r.0, n.0, "node ID mismatch");
        assert_eq!(r.1, n.1, "node lat mismatch for id={}", r.0);
        assert_eq!(r.2, n.2, "node lon mismatch for id={}", r.0);
        assert_eq!(r.3, n.3, "node tags mismatch for id={}", r.0);
    }

    // Compare way IDs and data
    assert_eq!(way_ids(&result_contents), way_ids(&new_contents));
    for (r, n) in result_contents.ways.iter().zip(new_contents.ways.iter()) {
        assert_eq!(r.0, n.0, "way ID mismatch");
        assert_eq!(r.1, n.1, "way refs mismatch for id={}", r.0);
        assert_eq!(r.2, n.2, "way tags mismatch for id={}", r.0);
    }

    assert_eq!(relation_ids(&result_contents), relation_ids(&new_contents));
}
