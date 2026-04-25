//! CLI-driven defensive-input regression tests.
//!
//! Replaces `tests/cluster2_defensive_input.rs`. The two tests in this
//! file pin specific defensive-input contracts that span more than one
//! command (renumber + apply-changes), so they live here rather than
//! folded into `cli_apply_changes_invariants.rs` (apply-changes-only)
//! or any single command's `cli_*.rs`.
//!
//! See `notes/testing.md` for the broader test reorg context. Cluster
//! 2 (the 2026-04-24 fix sweep) landed five hard-error promotions; two
//! of them have direct regression tests here. The other three need
//! byte-level fixture manipulation primitives (`mutate_blob_*`)
//! tracked under T02 in `notes/testing.md`.
//!
//! No imports from `pbfhogg::renumber::*` or `pbfhogg::apply_changes::*` -
//! a rewrite of either command cannot break these tests by type
//! changes alone.

#![allow(clippy::unwrap_used)]

mod common;

use std::io::Write;
use std::path::Path;

use common::cli::CliInvoker;
use common::{write_test_pbf, TestNode, TestRelation, TestWay};
use pbfhogg::block_builder::{self, BlockBuilder};
use pbfhogg::writer::{Compression, PbfWriter};
use tempfile::TempDir;

const LAT: i32 = 550_000_000;
const LON: i32 = 120_000_000;

/// Write a PBF with three node blocks in an order that is intentionally
/// NOT monotonic by max_id, while the header claims `Sort.Type_then_ID`.
/// Simulates a producer with a lying header or a malformed file that
/// escaped the sort gate.
fn write_lying_sorted_pbf(path: &Path) {
    let file = std::fs::File::create(path).expect("create");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new()
        .sorted()
        .build()
        .expect("header");
    writer.write_header(&header).expect("write header");

    // Three blocks in deliberately non-monotonic order:
    //   Block 1: ids 1..=100   (max_id = 100)
    //   Block 2: ids 500..=600 (max_id = 600)  <- global max
    //   Block 3: ids 200..=300 (max_id = 300)  <- last block's max
    for (start, end) in [(1_i64, 100_i64), (500, 600), (200, 300)] {
        let mut bb = BlockBuilder::new();
        for id in start..=end {
            bb.add_node(id, LAT, LON, [], None);
        }
        let bytes = bb.take().expect("take").expect("non-empty");
        writer.write_primitive_block(bytes).expect("write block");
    }
    writer.flush().expect("flush");
}

/// Cluster-2 fix #1: `renumber_external` scans the full schedule for
/// `max_node_id` instead of trusting "last blob's max_id == global
/// max". Before the fix the lying-sorted fixture caused a panic
/// inside `IdSet::set_atomic` ("pre_allocate only covers..."). After
/// the fix the command runs without that panic - either to
/// completion, or to a clean error that does NOT mention
/// `pre_allocate`.
#[test]
fn renumber_survives_lying_sorted_header_out_of_order_blobs() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_lying_sorted_pbf(&input);

    let out = CliInvoker::new()
        .arg("renumber")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .run();

    // We don't require the command to produce "correct" output (the
    // input is intentionally malformed). What we reject is a panic
    // mentioning the `IdSet::set_atomic` failure mode.
    let stderr = out.stderr_str();
    assert!(
        !stderr.contains("pre_allocate only covers"),
        "renumber panicked in IdSet::set_atomic despite the \
         max_id scan fix; stderr:\n{stderr}",
    );
}

/// Cluster-2 fix #4: `apply_changes::build_header_bytes` rejects base
/// PBFs whose header does not advertise `Sort.Type_then_ID`, on both
/// the `--locations-on-ways` path and the general path. Before the
/// fix the general path silently accepted unsorted headers and could
/// drop upsert creates.
#[test]
fn apply_changes_rejects_unsorted_header() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let diff = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    // common::write_test_pbf builds an unsorted header (no
    // `.sorted()`).
    let nodes = vec![TestNode {
        id: 1,
        lat: LAT,
        lon: LON,
        tags: vec![],
        meta: None,
    }];
    let ways: Vec<TestWay> = vec![];
    let relations: Vec<TestRelation> = vec![];
    write_test_pbf(&base, &nodes, &ways, &relations);

    // Empty OSC; we expect to fail before the merge reads it.
    let file = std::fs::File::create(&diff).expect("create");
    let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    enc.write_all(b"<?xml version='1.0' encoding='UTF-8'?>\n<osmChange version='0.6'/>\n")
        .expect("write xml");
    enc.finish().expect("finish gz");

    let out = CliInvoker::new()
        .arg("apply-changes")
        .arg(&base)
        .arg(&diff)
        .arg("-o")
        .arg(&output)
        .run();

    assert!(
        !out.status.success(),
        "apply-changes must reject an unsorted base header; stdout:\n{}\nstderr:\n{}",
        out.stdout_str(),
        out.stderr_str(),
    );
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("sorted base PBF"),
        "expected a sortedness error message; stderr:\n{stderr}",
    );
}
