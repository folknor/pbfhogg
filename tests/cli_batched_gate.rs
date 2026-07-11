//! Env-path equivalence tests for the temporary batched pipeline gate.

#![allow(clippy::unwrap_used)]

mod common;

use common::adversarial::mutate_blob_header_indexdata;
use common::cli::CliInvoker;
use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, Metadata};
use pbfhogg::writer::{Compression, PbfWriter};

fn write_mixed(path: &std::path::Path, historical: bool) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(file, Compression::Zlib(6));
    let mut header = HeaderBuilder::new().sorted();
    if historical {
        header = header.historical();
    }
    let header = header.build().unwrap();
    writer.write_header(&header).unwrap();
    let mut block = BlockBuilder::new();
    let meta = historical.then_some(Metadata {
        version: 1,
        timestamp: 100,
        changeset: 1,
        uid: 1,
        user: "test",
        visible: true,
    });
    block.add_node(
        1,
        500_000_000,
        100_000_000,
        [("name", "one")],
        meta.as_ref(),
    );
    block.add_node(
        2,
        500_000_001,
        100_000_001,
        [("name", "two")],
        meta.as_ref(),
    );
    writer
        .write_primitive_block(block.take().unwrap().unwrap())
        .unwrap();
    block.add_way(10, [("highway", "primary")], &[1, 2], meta.as_ref());
    writer
        .write_primitive_block(block.take().unwrap().unwrap())
        .unwrap();
    writer.flush().unwrap();
}

fn assert_bytes_equal(default: &std::path::Path, batched: &std::path::Path) {
    assert_eq!(
        std::fs::read(default).unwrap(),
        std::fs::read(batched).unwrap()
    );
}

#[test]
fn batched_gate_time_filter_history_matches_default() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("history.osm.pbf");
    let default = dir.path().join("default.osm.pbf");
    let batched = dir.path().join("batched.osm.pbf");
    write_mixed(&input, true);
    CliInvoker::new()
        .args([
            "time-filter",
            input.to_str().unwrap(),
            "-o",
            default.to_str().unwrap(),
            "200",
        ])
        .assert_success();
    CliInvoker::new()
        .env("PBFHOGG_BATCHED_PIPELINE", "1")
        .args([
            "time-filter",
            input.to_str().unwrap(),
            "-o",
            batched.to_str().unwrap(),
            "200",
        ])
        .assert_success();
    assert_bytes_equal(&default, &batched);
}

#[test]
fn batched_gate_getid_add_referenced_matches_default() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.osm.pbf");
    let default = dir.path().join("default.osm.pbf");
    let batched = dir.path().join("batched.osm.pbf");
    write_mixed(&input, false);
    CliInvoker::new()
        .args([
            "getid",
            input.to_str().unwrap(),
            "-o",
            default.to_str().unwrap(),
            "--add-referenced",
            "w10",
        ])
        .assert_success();
    CliInvoker::new()
        .env("PBFHOGG_BATCHED_PIPELINE", "1")
        .args([
            "getid",
            input.to_str().unwrap(),
            "-o",
            batched.to_str().unwrap(),
            "--add-referenced",
            "w10",
        ])
        .assert_success();
    assert_bytes_equal(&default, &batched);
}

#[test]
fn batched_gate_getparents_full_scan_matches_default() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.osm.pbf");
    let default = dir.path().join("default.osm.pbf");
    let batched = dir.path().join("batched.osm.pbf");
    write_mixed(&input, false);
    CliInvoker::new()
        .args([
            "getparents",
            input.to_str().unwrap(),
            "-o",
            default.to_str().unwrap(),
            "--full-scan-min-blobs",
            "0",
            "n1",
        ])
        .assert_success();
    CliInvoker::new()
        .env("PBFHOGG_BATCHED_PIPELINE", "1")
        .args([
            "getparents",
            input.to_str().unwrap(),
            "-o",
            batched.to_str().unwrap(),
            "--full-scan-min-blobs",
            "0",
            "n1",
        ])
        .assert_success();
    assert_bytes_equal(&default, &batched);
}

#[test]
fn batched_gate_altw_decode_all_matches_default() {
    let dir = tempfile::tempdir().unwrap();
    let indexed = dir.path().join("indexed.osm.pbf");
    let input = dir.path().join("raw.osm.pbf");
    let default = dir.path().join("default.osm.pbf");
    let batched = dir.path().join("batched.osm.pbf");
    write_mixed(&indexed, false);
    let bytes = std::fs::read(&indexed).unwrap();
    std::fs::write(&input, mutate_blob_header_indexdata(&bytes, 1, Vec::clear)).unwrap();
    CliInvoker::new()
        .args([
            "add-locations-to-ways",
            input.to_str().unwrap(),
            "-o",
            default.to_str().unwrap(),
            "--index-type",
            "sparse",
            "--force",
        ])
        .assert_success();
    CliInvoker::new()
        .env("PBFHOGG_BATCHED_PIPELINE", "1")
        .args([
            "add-locations-to-ways",
            input.to_str().unwrap(),
            "-o",
            batched.to_str().unwrap(),
            "--index-type",
            "sparse",
            "--force",
        ])
        .assert_success();
    assert_bytes_equal(&default, &batched);
}

#[test]
fn batched_gate_rejects_invalid_value() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_mixed(&input, false);
    CliInvoker::new()
        .env("PBFHOGG_BATCHED_PIPELINE", "2")
        .arg("getparents")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--full-scan-min-blobs")
        .arg("0")
        .arg("n1")
        .assert_failure()
        .assert_stderr_contains("PBFHOGG_BATCHED_PIPELINE");
}
