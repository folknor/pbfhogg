mod common;

use common::read_header;
use pbfhogg::block_builder::{self, BlockBuilder, MemberData, Metadata};
use pbfhogg::time_filter::time_filter;
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element, MemberId};

#[derive(Debug, Clone, PartialEq, Eq)]
struct NodeSnapshot {
    id: i64,
    version: i32,
    timestamp: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WaySnapshot {
    id: i64,
    version: i32,
    refs: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelationSnapshot {
    id: i64,
    version: i32,
    member_count: usize,
}

#[test]
fn snapshot_keeps_latest_version_at_cutoff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("history.osm.pbf");
    let output = dir.path().join("snapshot.osm.pbf");

    write_history_input(&input);
    time_filter(&input, &output, 250, Compression::default(), false, &pbfhogg::HeaderOverrides::default()).expect("time-filter");

    let nodes = read_nodes_with_metadata(&output);
    assert_eq!(
        nodes,
        vec![
            NodeSnapshot {
                id: 1,
                version: 2,
                timestamp: 200
            },
            NodeSnapshot {
                id: 3,
                version: 1,
                timestamp: 50
            },
        ]
    );
}

#[test]
fn snapshot_omits_objects_deleted_at_cutoff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("history.osm.pbf");
    let output = dir.path().join("snapshot.osm.pbf");

    write_history_input(&input);
    time_filter(&input, &output, 350, Compression::default(), false, &pbfhogg::HeaderOverrides::default()).expect("time-filter");

    let nodes = read_nodes_with_metadata(&output);
    assert_eq!(
        nodes,
        vec![NodeSnapshot {
            id: 3,
            version: 1,
            timestamp: 50
        }]
    );
}

#[test]
fn output_header_replication_timestamp_is_cutoff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("history.osm.pbf");
    let output = dir.path().join("snapshot.osm.pbf");

    write_history_input(&input);
    time_filter(&input, &output, 123, Compression::default(), false, &pbfhogg::HeaderOverrides::default()).expect("time-filter");

    let header = read_header(&output);
    assert_eq!(header.osmosis_replication_timestamp(), Some(123));
    assert!(header.is_sorted(), "sorted flag should be preserved");
}

fn write_history_input(path: &std::path::Path) {
    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new()
        .sorted()
        .historical()
        .replication_timestamp(9_999)
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();
    let m1 = Metadata {
        version: 1,
        timestamp: 100,
        changeset: 10,
        uid: 1,
        user: "u",
        visible: true,
    };
    let m2 = Metadata {
        version: 2,
        timestamp: 200,
        changeset: 11,
        uid: 1,
        user: "u",
        visible: true,
    };
    let m3 = Metadata {
        version: 3,
        timestamp: 300,
        changeset: 12,
        uid: 1,
        user: "u",
        visible: false,
    };
    let m4 = Metadata {
        version: 1,
        timestamp: 50,
        changeset: 20,
        uid: 2,
        user: "v",
        visible: true,
    };

    bb.add_node(1, 10, 10, &[], Some(&m1));
    bb.add_node(1, 11, 11, &[], Some(&m2));
    bb.add_node(1, 12, 12, &[], Some(&m3));
    bb.add_node(3, 33, 33, &[], Some(&m4));

    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }
    writer.flush().expect("flush");
}

fn read_nodes_with_metadata(path: &std::path::Path) -> Vec<NodeSnapshot> {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut out = Vec::new();
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode") {
            for element in block.elements() {
                match element {
                    Element::DenseNode(dn) => {
                        let info = dn.info().expect("dense info");
                        out.push(NodeSnapshot {
                            id: dn.id(),
                            version: info.version(),
                            timestamp: info.milli_timestamp() / 1000,
                        });
                    }
                    Element::Node(n) => {
                        let info = n.info();
                        out.push(NodeSnapshot {
                            id: n.id(),
                            version: info.version().unwrap_or(0),
                            timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
                        });
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

#[test]
fn snapshot_ways_and_relations() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("history.osm.pbf");
    let output = dir.path().join("snapshot.osm.pbf");

    write_history_with_ways_and_relations(&input);
    time_filter(&input, &output, 250, Compression::default(), false, &pbfhogg::HeaderOverrides::default()).expect("time-filter");

    let (ways, relations) = read_ways_and_relations(&output);
    assert_eq!(
        ways,
        vec![WaySnapshot {
            id: 10,
            version: 2,
            refs: vec![1, 2, 3],
        }]
    );
    assert_eq!(
        relations,
        vec![RelationSnapshot {
            id: 100,
            version: 1,
            member_count: 1,
        }]
    );
}

fn write_history_with_ways_and_relations(path: &std::path::Path) {
    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new()
        .sorted()
        .historical()
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    // Way 10: v1 at t=100, v2 at t=200, v3 (deleted) at t=300
    let w1 = Metadata {
        version: 1,
        timestamp: 100,
        changeset: 10,
        uid: 1,
        user: "u",
        visible: true,
    };
    let w2 = Metadata {
        version: 2,
        timestamp: 200,
        changeset: 11,
        uid: 1,
        user: "u",
        visible: true,
    };
    let w3 = Metadata {
        version: 3,
        timestamp: 300,
        changeset: 12,
        uid: 1,
        user: "u",
        visible: false,
    };
    bb.add_way(10, &[], &[1, 2], Some(&w1));
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }
    bb.add_way(10, &[], &[1, 2, 3], Some(&w2));
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }
    bb.add_way(10, &[], &[], Some(&w3));
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    // Relation 100: v1 at t=150
    let r1 = Metadata {
        version: 1,
        timestamp: 150,
        changeset: 30,
        uid: 2,
        user: "v",
        visible: true,
    };
    bb.add_relation(
        100,
        &[("type", "multipolygon")],
        &[MemberData {
            id: MemberId::Way(10),
            role: "outer",
        }],
        Some(&r1),
    );
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }
    writer.flush().expect("flush");
}

fn read_ways_and_relations(
    path: &std::path::Path,
) -> (Vec<WaySnapshot>, Vec<RelationSnapshot>) {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut ways = Vec::new();
    let mut relations = Vec::new();
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode") {
            for element in block.elements() {
                match element {
                    Element::Way(w) => {
                        ways.push(WaySnapshot {
                            id: w.id(),
                            version: w.info().version().unwrap_or(0),
                            refs: w.refs().collect(),
                        });
                    }
                    Element::Relation(r) => {
                        relations.push(RelationSnapshot {
                            id: r.id(),
                            version: r.info().version().unwrap_or(0),
                            member_count: r.members().count(),
                        });
                    }
                    _ => {}
                }
            }
        }
    }
    (ways, relations)
}
