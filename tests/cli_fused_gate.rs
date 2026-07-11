//! Env-path equivalence tests for command-transform fusion.

#![allow(clippy::unwrap_used)]

mod common;

use common::adversarial::mutate_blob_header_indexdata;
use common::cli::CliInvoker;
use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
use pbfhogg::writer::{Compression, PbfWriter};

fn write_mixed(path: &std::path::Path) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(file, Compression::Zlib(6));
    writer
        .write_header(&HeaderBuilder::new().sorted().build().unwrap())
        .unwrap();
    let mut block = BlockBuilder::new();
    block.add_node(1, 500_000_000, 100_000_000, [("name", "one")], None);
    block.add_node(2, 500_000_001, 100_000_001, [("name", "two")], None);
    writer
        .write_primitive_block(block.take().unwrap().unwrap())
        .unwrap();
    block.add_way(10, [("highway", "primary")], &[1, 2], None);
    writer
        .write_primitive_block(block.take().unwrap().unwrap())
        .unwrap();
    writer.flush().unwrap();
}

fn assert_bytes_equal(left: &std::path::Path, right: &std::path::Path) {
    assert_eq!(std::fs::read(left).unwrap(), std::fs::read(right).unwrap());
}

#[test]
fn fused_gate_getid_add_referenced_matches_default() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.pbf");
    let default = dir.path().join("default.pbf");
    let fused = dir.path().join("fused.pbf");
    write_mixed(&input);
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
        .env("PBFHOGG_FUSE_TRANSFORM", "1")
        .args([
            "getid",
            input.to_str().unwrap(),
            "-o",
            fused.to_str().unwrap(),
            "--add-referenced",
            "w10",
        ])
        .assert_success();
    assert_bytes_equal(&default, &fused);
}

#[test]
fn fused_gate_getparents_full_scan_matches_default() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.pbf");
    let default = dir.path().join("default.pbf");
    let fused = dir.path().join("fused.pbf");
    write_mixed(&input);
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
        .env("PBFHOGG_FUSE_TRANSFORM", "1")
        .args([
            "getparents",
            input.to_str().unwrap(),
            "-o",
            fused.to_str().unwrap(),
            "--full-scan-min-blobs",
            "0",
            "n1",
        ])
        .assert_success();
    assert_bytes_equal(&default, &fused);
}

#[test]
fn fused_gate_tags_filter_single_pass_matches_default() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.pbf");
    let default = dir.path().join("default.pbf");
    let fused = dir.path().join("fused.pbf");
    write_mixed(&input);
    CliInvoker::new()
        .args([
            "tags-filter",
            input.to_str().unwrap(),
            "-o",
            default.to_str().unwrap(),
            "-R",
            "--force",
            "w/highway=primary",
        ])
        .assert_success();
    CliInvoker::new()
        .env("PBFHOGG_FUSE_TRANSFORM", "1")
        .args([
            "tags-filter",
            input.to_str().unwrap(),
            "-o",
            fused.to_str().unwrap(),
            "-R",
            "--force",
            "w/highway=primary",
        ])
        .assert_success();
    assert_bytes_equal(&default, &fused);
}

#[test]
fn fused_gate_altw_decode_all_matches_default() {
    let dir = tempfile::tempdir().unwrap();
    let indexed = dir.path().join("indexed.pbf");
    let input = dir.path().join("raw.pbf");
    let default = dir.path().join("default.pbf");
    let fused = dir.path().join("fused.pbf");
    write_mixed(&indexed);
    std::fs::write(
        &input,
        mutate_blob_header_indexdata(&std::fs::read(&indexed).unwrap(), 1, Vec::clear),
    )
    .unwrap();
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
        .env("PBFHOGG_FUSE_TRANSFORM", "1")
        .args([
            "add-locations-to-ways",
            input.to_str().unwrap(),
            "-o",
            fused.to_str().unwrap(),
            "--index-type",
            "sparse",
            "--force",
        ])
        .assert_success();
    assert_bytes_equal(&default, &fused);
}

#[test]
fn fused_gate_combination_getid_matches_default() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.pbf");
    let default = dir.path().join("default.pbf");
    let fused = dir.path().join("fused.pbf");
    write_mixed(&input);
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
        .env("PBFHOGG_FUSE_TRANSFORM", "1")
        .args([
            "getid",
            input.to_str().unwrap(),
            "-o",
            fused.to_str().unwrap(),
            "--add-referenced",
            "w10",
        ])
        .assert_success();
    assert_bytes_equal(&default, &fused);
}

#[test]
fn fused_gate_combination_getparents_matches_default() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.pbf");
    let default = dir.path().join("default.pbf");
    let fused = dir.path().join("fused.pbf");
    write_mixed(&input);
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
        .env("PBFHOGG_FUSE_TRANSFORM", "1")
        .args([
            "getparents",
            input.to_str().unwrap(),
            "-o",
            fused.to_str().unwrap(),
            "--full-scan-min-blobs",
            "0",
            "n1",
        ])
        .assert_success();
    assert_bytes_equal(&default, &fused);
}

#[test]
fn fused_gate_rejects_invalid_value() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.pbf");
    let output = dir.path().join("output.pbf");
    write_mixed(&input);
    CliInvoker::new()
        .env("PBFHOGG_FUSE_TRANSFORM", "2")
        .args([
            "getid",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--add-referenced",
            "w10",
        ])
        .assert_failure()
        .assert_stderr_contains("PBFHOGG_FUSE_TRANSFORM");
}
