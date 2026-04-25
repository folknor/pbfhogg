//! CLI-driven integration tests for `pbfhogg time-filter`.
//!
//! Replaces the library-API `tests/time_filter.rs`. Fixture PBFs
//! are built with the stable-allowlist writer helpers; the
//! `time-filter` command runs via the compiled `pbfhogg` binary
//! through `CliInvoker`; output is verified by reading the
//! resulting PBF with the stable-allowlist reader helpers, with
//! the stats summary inspected through stderr (the CLI emits it
//! via `eprintln!`). No imports from `pbfhogg::time_filter::*` -
//! a rewrite of `src/commands/time_filter/` cannot break these
//! tests by type changes alone.

#![allow(clippy::unwrap_used)]

mod common;

use std::path::Path;

use common::cli::CliInvoker;
use common::read_header;
use pbfhogg::block_builder::{self, BlockBuilder, MemberData, Metadata};
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

/// Invoke `pbfhogg time-filter <input> -o <output> <cutoff>` and
/// assert success. Returns the captured stderr (which carries the
/// stats summary line) so the caller can pin counter values.
fn run_time_filter(input: &Path, output: &Path, cutoff: i64) -> String {
    let out = CliInvoker::new()
        .arg("time-filter")
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg(cutoff.to_string())
        .assert_success();
    out.stderr_str()
}

#[test]
fn snapshot_keeps_latest_version_at_cutoff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("history.osm.pbf");
    let output = dir.path().join("snapshot.osm.pbf");

    write_history_input(&input);
    run_time_filter(&input, &output, 250);

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
    run_time_filter(&input, &output, 350);

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
    run_time_filter(&input, &output, 123);

    let header = read_header(&output);
    assert_eq!(header.osmosis_replication_timestamp(), Some(123));
    assert!(header.is_sorted(), "sorted flag should be preserved");
}

#[test]
fn snapshot_path_filters_non_history_input() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("snapshot_input.osm.pbf");
    let output = dir.path().join("snapshot_output.osm.pbf");

    write_snapshot_input(&input);
    let stderr = run_time_filter(&input, &output, 250);

    // The CLI emits the TimeFilterStats summary on stderr via
    // `print_summary`. Pin the substrings for the five counters
    // the library test pinned by direct field access.
    assert!(
        stderr.contains("8 versions scanned"),
        "stats line missing 'versions_seen=8'; stderr =\n{stderr}",
    );
    assert!(
        stderr.contains("6 <= cutoff"),
        "stats line missing 'versions_before_cutoff=6'; stderr =\n{stderr}",
    );
    assert!(
        stderr.contains("4 elements written"),
        "stats line missing 'elements_written=4'; stderr =\n{stderr}",
    );
    assert!(
        stderr.contains("2 deleted at cutoff"),
        "stats line missing 'dropped_deleted=2'; stderr =\n{stderr}",
    );
    assert!(
        stderr.contains("2 without version <= cutoff"),
        "stats line missing 'dropped_no_snapshot_version=2'; stderr =\n{stderr}",
    );

    let nodes = read_nodes_with_metadata(&output);
    assert_eq!(
        nodes,
        vec![
            NodeSnapshot {
                id: 1,
                version: 1,
                timestamp: 100,
            },
            NodeSnapshot {
                id: 4,
                version: 2,
                timestamp: 250,
            },
        ]
    );

    let (ways, relations) = read_ways_and_relations(&output);
    assert_eq!(
        ways,
        vec![WaySnapshot {
            id: 10,
            version: 3,
            refs: vec![1, 4],
        }]
    );
    assert_eq!(
        relations,
        vec![RelationSnapshot {
            id: 100,
            version: 2,
            member_count: 1,
        }]
    );

    let header = read_header(&output);
    assert_eq!(header.osmosis_replication_timestamp(), Some(250));
    assert!(header.is_sorted(), "sorted flag should be preserved");
    assert!(
        !header.has_historical_information(),
        "snapshot-path output must stay non-historical"
    );
}

#[test]
fn snapshot_ways_and_relations() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("history.osm.pbf");
    let output = dir.path().join("snapshot.osm.pbf");

    write_history_with_ways_and_relations(&input);
    run_time_filter(&input, &output, 250);

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

// ---------------------------------------------------------------------------
// Fixture builders (allowlist-only)
// ---------------------------------------------------------------------------

fn write_history_input(path: &Path) {
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

    bb.add_node(1, 10, 10, std::iter::empty::<(&str, &str)>(), Some(&m1));
    bb.add_node(1, 11, 11, std::iter::empty::<(&str, &str)>(), Some(&m2));
    bb.add_node(1, 12, 12, std::iter::empty::<(&str, &str)>(), Some(&m3));
    bb.add_node(3, 33, 33, std::iter::empty::<(&str, &str)>(), Some(&m4));

    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }
    writer.flush().expect("flush");
}

fn snapshot_meta(version: i32, timestamp: i64, visible: bool) -> Metadata<'static> {
    Metadata {
        version,
        timestamp,
        changeset: 10_000 + i64::from(version),
        uid: 7,
        user: "snapshot",
        visible,
    }
}

fn write_snapshot_input(path: &Path) {
    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new()
        .sorted()
        .replication_timestamp(9_999)
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    bb.add_node(
        1,
        10,
        10,
        [("name", "keep")],
        Some(&snapshot_meta(1, 100, true)),
    );
    bb.add_node(
        2,
        20,
        20,
        [("name", "too-new")],
        Some(&snapshot_meta(1, 260, true)),
    );
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    bb.add_node(
        3,
        30,
        30,
        [("name", "deleted")],
        Some(&snapshot_meta(1, 200, false)),
    );
    bb.add_node(
        4,
        40,
        40,
        [("name", "edge")],
        Some(&snapshot_meta(2, 250, true)),
    );
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    bb.add_way(
        10,
        [("highway", "service")],
        &[1, 4],
        Some(&snapshot_meta(3, 200, true)),
    );
    bb.add_way(
        11,
        [("highway", "residential")],
        &[2, 4],
        Some(&snapshot_meta(1, 300, true)),
    );
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    bb.add_relation(
        100,
        [("type", "multipolygon")],
        &[MemberData {
            id: MemberId::Way(10),
            role: "outer",
        }],
        Some(&snapshot_meta(2, 225, true)),
    );
    bb.add_relation(
        101,
        [("type", "site")],
        &[MemberData {
            id: MemberId::Node(3),
            role: "label",
        }],
        Some(&snapshot_meta(1, 240, false)),
    );
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    writer.flush().expect("flush");
}

fn write_history_with_ways_and_relations(path: &Path) {
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
    bb.add_way(10, std::iter::empty::<(&str, &str)>(), &[1, 2], Some(&w1));
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }
    bb.add_way(
        10,
        std::iter::empty::<(&str, &str)>(),
        &[1, 2, 3],
        Some(&w2),
    );
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }
    bb.add_way(10, std::iter::empty::<(&str, &str)>(), &[], Some(&w3));
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
        [("type", "multipolygon")],
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

// ---------------------------------------------------------------------------
// Allowlist readers
// ---------------------------------------------------------------------------

fn read_nodes_with_metadata(path: &Path) -> Vec<NodeSnapshot> {
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

fn read_ways_and_relations(path: &Path) -> (Vec<WaySnapshot>, Vec<RelationSnapshot>) {
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
