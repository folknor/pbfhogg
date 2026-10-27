//! `pbfhogg check --locations`: a LocationsOnWays file's inline way-node
//! coordinates against its node records.
//!
//! The library tests (`tests/check_locations.rs`) pin the counts, categories
//! and partition invariance. These pin the command surface: exit 1 is "ran,
//! and the file is wrong", exit 2 "could not check it", the report and its
//! JSON, the selector rules, and two whole-workflow cases - a file whose
//! metadata lies about a blob's kind, and an `apply-changes
//! --locations-on-ways` output after a node move.

#![allow(clippy::unwrap_used)]

use std::path::Path;

use crate::invoker::{CliInvoker, CliOutput};
use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
use pbfhogg::writer::{Compression, PbfWriter};

const EXIT_ANSWER_NEGATIVE: i32 = 1;
const EXIT_ERROR: i32 = 2;

type Way = (i64, Vec<i64>, Vec<(i32, i32)>);

/// A sorted, indexed file: one node blob, then one blob per way group.
fn write_low(
    path: &Path,
    locations_header: bool,
    nodes: &[(i64, i32, i32)],
    way_blobs: &[Vec<Way>],
) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    let mut header = HeaderBuilder::new().sorted();
    if locations_header {
        header = header.optional_feature("LocationsOnWays");
    }
    writer.write_header(&header.build().unwrap()).unwrap();
    let mut bb = BlockBuilder::new();
    for &(id, lat, lon) in nodes {
        bb.add_node(id, lat, lon, std::iter::empty(), None);
    }
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();
    for blob in way_blobs {
        for (id, refs, coords) in blob {
            bb.add_way_with_locations(*id, [("highway", "path")], refs, coords, None);
        }
        writer
            .write_primitive_block(bb.take().unwrap().unwrap())
            .unwrap();
    }
    writer.flush().unwrap();
}

/// [`write_low`], with each way block carrying one more group holding only
/// an empty `DenseNodes` message. The writer's index scan counts no element
/// in it, so each way blob is indexed as ways.
fn write_low_with_empty_dense_in_way_blob(
    path: &Path,
    nodes: &[(i64, i32, i32)],
    way_blobs: &[Vec<Way>],
) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    let header = HeaderBuilder::new()
        .sorted()
        .optional_feature("LocationsOnWays");
    writer.write_header(&header.build().unwrap()).unwrap();
    let mut bb = BlockBuilder::new();
    for &(id, lat, lon) in nodes {
        bb.add_node(id, lat, lon, std::iter::empty(), None);
    }
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();
    for blob in way_blobs {
        for (id, refs, coords) in blob {
            bb.add_way_with_locations(*id, [("highway", "path")], refs, coords, None);
        }
        let mut block = bb.take().unwrap().unwrap().to_vec();
        // PrimitiveBlock field 2 (a group) holding PrimitiveGroup field 2
        // (DenseNodes) of length zero.
        block.extend_from_slice(&[0x12, 0x02, 0x12, 0x00]);
        writer.write_primitive_block(&block).unwrap();
    }
    writer.flush().unwrap();
}

const NODES: [(i64, i32, i32); 3] = [
    (1, 100_000_000, 200_000_000),
    (2, 300_000_000, 400_000_000),
    (3, 500_000_000, 600_000_000),
];

fn honest_ways() -> Vec<Vec<Way>> {
    vec![
        vec![(
            10,
            vec![1, 2],
            vec![(100_000_000, 200_000_000), (300_000_000, 400_000_000)],
        )],
        vec![(
            20,
            vec![2, 3],
            vec![(300_000_000, 400_000_000), (500_000_000, 600_000_000)],
        )],
    ]
}

fn check(path: &Path, extra: &[&str]) -> CliOutput {
    CliInvoker::new()
        .arg("check")
        .arg("--locations")
        .args(extra)
        .arg(path)
        .run()
}

#[test]
fn an_honest_file_passes_and_exits_zero() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("honest.osm.pbf");
    write_low(&path, true, &NODES, &honest_ways());
    let out = check(&path, &[]);
    assert!(
        out.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        out.stdout_str(),
        out.stderr_str()
    );
    let stdout = out.stdout_str();
    assert!(stdout.contains("4 verified, 0 mismatched"), "{stdout}");
    assert!(
        stdout.contains("Inline locations for honest.osm.pbf: OK"),
        "{stdout}"
    );
}

#[test]
fn a_stale_coordinate_exits_one_and_names_the_way() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("stale.osm.pbf");
    let mut ways = honest_ways();
    ways[1][0].2[1] = (500_000_001, 600_000_000);
    write_low(&path, true, &NODES, &ways);
    let out = check(&path, &[]);
    assert_eq!(out.status.code(), Some(EXIT_ANSWER_NEGATIVE));
    let stdout = out.stdout_str();
    assert!(
        stdout.contains(
            "w20 ref 1 (n3): inline 50.0000001,60.0000000, node record 50.0000000,60.0000000"
        ),
        "{stdout}"
    );
    assert!(stdout.contains("FAILED (1 violations)"), "{stdout}");
}

#[test]
fn json_carries_a_locations_section() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("honest.osm.pbf");
    write_low(&path, true, &NODES, &honest_ways());
    let out = check(&path, &["--json"]);
    assert!(out.status.success(), "{}", out.stderr_str());
    let value: serde_json::Value = serde_json::from_str(&out.stdout_str()).unwrap();
    let section = &value["locations"];
    assert_eq!(section["passed"], true);
    assert_eq!(section["refs_verified"], 4);
    assert_eq!(section["nodes_absent"], serde_json::Value::Null);
    // --locations alone suppresses the default --ids + --refs pair.
    assert!(
        value.get("ids").is_none() && value.get("refs").is_none(),
        "{value}"
    );
}

#[test]
fn a_file_without_the_claim_exits_two() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("plain.osm.pbf");
    write_low(&path, false, &NODES, &honest_ways());
    let out = check(&path, &[]);
    assert_eq!(out.status.code(), Some(EXIT_ERROR), "{}", out.stderr_str());
    assert!(
        out.stderr_str().contains("LocationsOnWays"),
        "{}",
        out.stderr_str()
    );
}

#[test]
fn agreement_requires_locations() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("honest.osm.pbf");
    write_low(&path, true, &NODES, &honest_ways());
    let out = CliInvoker::new()
        .arg("check")
        .arg("--agreement")
        .arg(&path)
        .run();
    assert_eq!(out.status.code(), Some(EXIT_ERROR));
    assert!(
        out.stderr_str()
            .contains("--agreement requires --locations"),
        "{}",
        out.stderr_str()
    );
}

/// An ID-check flag under `--locations` names the selector it needs, not
/// the one the user did not pass.
#[test]
fn an_id_check_flag_under_locations_names_its_selector() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("honest.osm.pbf");
    write_low(&path, true, &NODES, &honest_ways());
    let out = check(&path, &["--full"]);
    assert_eq!(out.status.code(), Some(EXIT_ERROR));
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("--full applies to the ID check: add --ids"),
        "{stderr}"
    );
    assert!(!stderr.contains("omit --refs"), "{stderr}");
}

/// A build without `linux-direct-io` refuses the flag before any check runs.
#[cfg(feature = "linux-direct-io")]
#[test]
fn direct_io_is_reported_as_having_no_effect() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("honest.osm.pbf");
    write_low(&path, true, &NODES, &honest_ways());
    let out = check(&path, &["--direct-io"]);
    assert!(out.status.success(), "{}", out.stderr_str());
    assert!(
        out.stderr_str()
            .contains("--direct-io has no effect on --locations"),
        "{}",
        out.stderr_str()
    );
}

/// Without node 2 the file is still right as far as anyone can tell; with
/// `--agreement`, its two inline copies must match.
#[test]
fn agreement_fails_disagreeing_copies_of_an_absent_node() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("absent.osm.pbf");
    let mut ways = honest_ways();
    ways[1][0].2[0] = (300_000_000, 400_000_001);
    write_low(&path, true, &[NODES[0], NODES[2]], &ways);

    let out = check(&path, &[]);
    assert!(out.status.success(), "{}", out.stdout_str());
    assert!(
        out.stdout_str()
            .contains("Refs without a node record (not verifiable): 2"),
        "{}",
        out.stdout_str()
    );

    let out = check(&path, &["--agreement"]);
    assert_eq!(out.status.code(), Some(EXIT_ANSWER_NEGATIVE));
    let stdout = out.stdout_str();
    assert!(stdout.contains("n2: not in the file"), "{stdout}");
    assert!(
        stdout.contains("Nodes not in the file: 1, 1 with disagreeing"),
        "{stdout}"
    );
}

/// A node blob relabelled as a way blob would hide its nodes from a check
/// that routes on the label; the check fails the file (exit 1, a stopped
/// report naming the blob and the claim it contradicts) instead of passing,
/// and points at `check --index` and the repair.
#[test]
fn a_relabelled_blob_fails_the_check_not_passes() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let mut ways = honest_ways();
    ways[1][0].2[1] = (500_000_001, 600_000_000);
    write_low(&input, true, &NODES, &ways);
    let corrupt = dir.path().join("kind.osm.pbf");
    CliInvoker::new()
        .arg("degrade")
        .arg(&input)
        .arg("-o")
        .arg(&corrupt)
        .arg("--corrupt-indexdata-kind")
        .assert_success();
    let out = check(&corrupt, &[]);
    assert_eq!(
        out.status.code(),
        Some(EXIT_ANSWER_NEGATIVE),
        "{}",
        out.stderr_str()
    );
    let stdout = out.stdout_str();
    assert!(
        stdout.contains("Location check stopped: blob 1 at byte offset")
            && stdout.contains("claims way elements but holds node elements"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("Refs"),
        "a stopped check reports no counts: {stdout}"
    );
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("check --index") && stderr.contains("cat --reindex"),
        "{stderr}"
    );

    let out = check(&corrupt, &["--json"]);
    assert_eq!(out.status.code(), Some(EXIT_ANSWER_NEGATIVE));
    let json: serde_json::Value = serde_json::from_str(&out.stdout_str()).unwrap();
    let locations = &json["locations"];
    assert_eq!(locations["complete"], false, "{json}");
    assert_eq!(locations["kind_contradiction"]["claimed"], "way", "{json}");
    assert_eq!(locations["kind_contradiction"]["found"], "node", "{json}");
}

/// A way blob carrying an empty dense-node message holds no node: the
/// location check judges kinds as the decoder does, so the honest file
/// passes. Its scanner once counted the empty message as a node and failed
/// a way claim it could not contradict.
#[test]
fn an_empty_dense_message_in_a_way_blob_is_no_node() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("empty-dense.osm.pbf");
    write_low_with_empty_dense_in_way_blob(&path, &NODES, &honest_ways());
    let out = check(&path, &[]);
    assert!(
        out.status.success(),
        "{}{}",
        out.stdout_str(),
        out.stderr_str()
    );
}

/// The workflow the check exists for: an OSC that moves node 3 and touches
/// no way must leave way 20, in a way blob the OSC does not overlap, carrying
/// node 3's new position. `apply-changes --locations-on-ways` once copied
/// such ways' coordinates unchanged, which this oracle reports as a
/// mismatch on way 20.
#[test]
fn apply_changes_output_passes_after_a_node_move() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("move.osc.gz");
    let output = dir.path().join("out.osm.pbf");
    write_low(&base, true, &NODES, &honest_ways());
    assert!(check(&base, &[]).status.success());
    crate::cli_apply_changes::write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="3" lat="51.0" lon="61.0" version="2"/>
  </modify>
</osmChange>"#,
    );
    CliInvoker::new()
        .arg("apply-changes")
        .arg(&base)
        .arg(&osc)
        .arg("-o")
        .arg(&output)
        .arg("--locations-on-ways")
        .assert_success();

    // Read the output independently of the check: node 3 and way 20's
    // inline copy of it must both be at the new position, so a passing check
    // cannot be explained by an output that kept the base unchanged.
    let moved = (510_000_000, 610_000_000);
    let mut node3 = None;
    let mut way20 = None;
    for blob in pbfhogg::BlobReader::from_path(&output).unwrap() {
        if let pbfhogg::BlobDecode::OsmData(block) = blob.unwrap().decode().unwrap() {
            for element in block.elements() {
                match element {
                    pbfhogg::Element::DenseNode(n) if n.id() == 3 => {
                        node3 = Some((n.decimicro_lat(), n.decimicro_lon()));
                    }
                    pbfhogg::Element::Node(n) if n.id() == 3 => {
                        node3 = Some((n.decimicro_lat(), n.decimicro_lon()));
                    }
                    pbfhogg::Element::Way(w) if w.id() == 20 => {
                        way20 = Some(
                            w.node_locations()
                                .map(|l| (l.decimicro_lat(), l.decimicro_lon()))
                                .collect::<Vec<_>>(),
                        );
                    }
                    _ => {}
                }
            }
        }
    }
    assert_eq!(node3, Some(moved), "node 3 must carry the OSC position");
    assert_eq!(
        way20,
        Some(vec![(300_000_000, 400_000_000), moved]),
        "way 20 must carry node 3's new position inline"
    );

    let out = check(&output, &[]);
    assert!(out.status.success(), "{}", out.stdout_str());
    assert!(
        out.stdout_str().contains("4 verified, 0 mismatched"),
        "{}",
        out.stdout_str()
    );
}
