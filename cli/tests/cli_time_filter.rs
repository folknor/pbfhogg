//! CLI-driven integration tests for `pbfhogg time-filter`.
//!
//! Replaces the removed library-API `time_filter.rs` test. Fixture PBFs
//! are built with the stable-allowlist writer helpers; the
//! `time-filter` command runs via the compiled `pbfhogg` binary
//! through `CliInvoker`; output is verified by reading the
//! resulting PBF with the stable-allowlist reader helpers, with
//! the stats summary inspected through stderr (the CLI emits it
//! via `eprintln!`). No imports from `pbfhogg::time_filter::*` -
//! a rewrite of `src/commands/time_filter/` cannot break these
//! tests by type changes alone.

#![allow(clippy::unwrap_used)]

use std::path::Path;

use crate::common::read_header;
use crate::invoker::CliInvoker;
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("snapshot_input.osm.pbf");
    let output = dir.path().join("snapshot_output.osm.pbf");

    write_snapshot_input(&input);
    let stderr = run_time_filter(&input, &output, 250);

    // The CLI emits the TimeFilterStats summary on stderr via
    // `write_summary`. Pin the substrings for the five counters
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
    let dir = crate::common::test_scratch();
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

pub(crate) fn write_history_input(path: &Path) {
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

// ---------------------------------------------------------------------------
// Metadata-stripped input
// ---------------------------------------------------------------------------

/// Write a sorted snapshot PBF whose elements carry NO metadata at all -
/// what `osmium --drop-metadata` and several pbfhogg write paths
/// produce.
fn write_metadata_stripped_input(path: &Path) {
    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new()
        .sorted()
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();
    bb.add_node(1, 10, 10, [("name", "a")], None);
    bb.add_node(2, 20, 20, [("name", "b")], None);
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }
    writer.flush().expect("flush");
}

/// `time-filter` must refuse an input whose elements carry no
/// timestamps rather than silently filtering nothing.
///
/// Every timestamp accessor used to end in `unwrap_or(0)`. Zero is at or
/// below every plausible cutoff, so `snapshot_gate` kept every element
/// and counted it in `versions_before_cutoff` - and the command printed
/// "N versions scanned, N <= cutoff, N elements written" having applied
/// no filtering whatsoever. Success plus a full copy of the input is the
/// worst possible failure mode here, because nothing downstream can tell
/// it apart from a cutoff that genuinely kept everything.
#[test]
fn time_filter_refuses_metadata_stripped_input() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("stripped.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    write_metadata_stripped_input(&input);

    let out = CliInvoker::new()
        .arg("time-filter")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("250")
        .assert_failure();
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("carries no timestamp"),
        "error must name the missing timestamp; stderr:\n{stderr}"
    );
    // Naming the element is what makes the error actionable.
    assert!(
        stderr.contains("node 1"),
        "error must name the offending element; stderr:\n{stderr}"
    );
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

pub(crate) fn write_snapshot_input(path: &Path) {
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

// ---------------------------------------------------------------------------
// The sorted claim, checked while reading
// ---------------------------------------------------------------------------

/// A file declaring `Sort.Type_then_ID` (and `HistoricalInformation` when
/// `history`) whose node blobs hold `blocks` in order, each node
/// `(id, version, timestamp)`, with or without indexdata.
fn write_node_blocks(path: &Path, history: bool, indexed: bool, blocks: &[&[(i64, i32, i64)]]) {
    let file = std::fs::File::create(path).expect("create file");
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    let mut header = block_builder::HeaderBuilder::new().sorted();
    if history {
        header = header.historical();
    }
    writer
        .write_header(&header.build().expect("build header"))
        .expect("write header");
    let mut bb = BlockBuilder::new();
    for block in blocks {
        for &(id, version, timestamp) in *block {
            bb.add_node(
                id,
                10,
                10,
                [],
                Some(&snapshot_meta(version, timestamp, true)),
            );
        }
        let bytes = bb.take().expect("take").expect("block");
        if indexed {
            writer.write_primitive_block(bytes).expect("write block");
        } else {
            writer
                .write_primitive_block_no_indexdata(bytes)
                .expect("write block");
        }
    }
    writer.flush().expect("flush");
}

fn assert_time_filter_refuses(input: &Path, needle: &str) -> String {
    let output = input.with_extension("out.pbf");
    let out = CliInvoker::new()
        .arg("time-filter")
        .arg(input)
        .arg("-o")
        .arg(&output)
        .arg("1000")
        .assert_failure();
    let stderr = out.stderr_str();
    assert!(
        stderr.contains(needle),
        "wanted {needle:?}; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(&input.display().to_string()),
        "the refusal names the input; stderr:\n{stderr}"
    );
    assert!(!output.exists(), "a refused run publishes nothing");
    stderr
}

/// The history path groups versions by contiguous ID, believing the sorted
/// header: node 2 coming back after node 1 used to be written twice. It is
/// refused as unsorted, with the repair named, on indexed and unindexed
/// input alike (neither path reads indexdata).
#[test]
fn history_path_refuses_an_object_out_of_order() {
    let dir = crate::common::test_scratch();
    for indexed in [true, false] {
        let input = dir
            .path()
            .join(format!("history-unsorted-{indexed}.osm.pbf"));
        write_node_blocks(
            &input,
            true,
            indexed,
            &[&[(2, 1, 100)], &[(1, 1, 100), (2, 2, 200)]],
        );
        let stderr = assert_time_filter_refuses(&input, "node 1 follows node 2");
        assert!(stderr.contains("sort --distrust-metadata"), "{stderr}");
    }
}

/// Within one object the history path keeps the last version at or before
/// the cutoff in input order, so a version descent used to pick the older
/// version silently, and a repeated version the later of two records.
#[test]
fn history_path_refuses_versions_out_of_order() {
    let dir = crate::common::test_scratch();
    // Version 0 claims no version, so it cannot be ordered against the 1.
    for (name, versions) in [
        ("descending", [2, 1]),
        ("repeated", [2, 2]),
        ("unversioned", [0, 1]),
    ] {
        let input = dir.path().join(format!("history-{name}.osm.pbf"));
        let block = [(7, versions[0], 100), (7, versions[1], 200)];
        write_node_blocks(&input, true, true, &[&block]);
        assert_time_filter_refuses(&input, "versions must be strictly ascending");
    }
}

/// The snapshot path filters each block on its own and carries the sorted
/// claim into the output: a descent across blocks or inside one is refused
/// as unsorted, and an object twice in a non-history file (both occurrences
/// used to be written) is refused as such, whatever the gate keeps.
#[test]
fn snapshot_path_refuses_disorder_and_duplicates() {
    let dir = crate::common::test_scratch();
    type Blocks<'a> = &'a [&'a [(i64, i32, i64)]];
    let cases: [(&str, Blocks<'_>, &str); 4] = [
        (
            "across",
            &[&[(2, 1, 100)], &[(1, 1, 100)]],
            "node 1 follows node 2",
        ),
        (
            "inside",
            &[&[(2, 1, 100), (1, 1, 100)]],
            "node 1 follows node 2",
        ),
        // Both versions are past the cutoff, so the gate keeps neither.
        (
            "dropped",
            &[&[(3, 1, 5_000)], &[(1, 1, 5_000)]],
            "node 1 follows node 3",
        ),
        (
            "duplicate",
            &[&[(1, 1, 100)], &[(1, 2, 200)]],
            "appears more than once",
        ),
    ];
    for (name, blocks, needle) in cases {
        let input = dir.path().join(format!("snapshot-{name}.osm.pbf"));
        write_node_blocks(&input, false, true, blocks);
        assert_time_filter_refuses(&input, needle);
    }
}

/// The passing counterpart: a sorted history input without any indexdata,
/// several versions per object, filters as the indexed one does.
#[test]
fn unindexed_sorted_history_input_still_filters() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("unindexed-history.osm.pbf");
    write_node_blocks(
        &input,
        true,
        false,
        &[&[(1, 1, 100), (1, 2, 200)], &[(1, 3, 2_000), (2, 1, 100)]],
    );
    let output = dir.path().join("out.osm.pbf");
    run_time_filter(&input, &output, 1000);
    assert_eq!(
        read_nodes_with_metadata(&output),
        vec![
            NodeSnapshot {
                id: 1,
                version: 2,
                timestamp: 200
            },
            NodeSnapshot {
                id: 2,
                version: 1,
                timestamp: 100
            },
        ]
    );
}
