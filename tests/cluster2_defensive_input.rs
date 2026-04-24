//! Cluster 2 defensive-input regression tests.
//!
//! These tests exercise the five cluster-2 fixes landed 2026-04-24:
//!
//! 1. `renumber/mod.rs::renumber_external` scans the full schedule for
//!    `max_node_id` instead of trusting "last blob's max_id == global
//!    max". The test here writes a PBF whose node blobs are deliberately
//!    NOT in ID order but whose header claims `Sort.Type_then_ID`, and
//!    verifies renumber does not panic in `IdSet::set_atomic`.
//!
//! 2. `blob_meta/scan_ids.rs::scan_nodes` uses checked arithmetic for
//!    the nanodegree -> decimicrodegree conversion. Not yet covered
//!    here - see TODO below.
//!
//! 3. `renumber/wire_rewrite.rs::count_varints_strict` validates every
//!    varint rather than counting terminator bytes. Not yet covered
//!    here - see TODO below.
//!
//! 4. `apply_changes/rewrite.rs::build_header_bytes` rejects base PBFs
//!    whose header does not advertise `Sort.Type_then_ID`, for both the
//!    `--locations-on-ways` path and the general path. The test here
//!    writes an unsorted-header base via `write_test_pbf` and verifies
//!    merge returns a clean error.
//!
//! 5. `altw/external/stage1.rs::build_node_blob_mapping` rejects
//!    `max_id < min_id` indexdata at the boundary. Not yet covered
//!    here - see TODO below.
//!
//! # TODO: extend fixture coverage
//!
//! The three fixes not yet tested (2, 3, 5) all require byte-level
//! manipulation of produced PBFs (custom `granularity` in a DenseNodes
//! block, truncated varint in a Relation.memids field, custom
//! per-blob indexdata bytes with reversed min/max). The test-shape
//! gap listed in `TODO.md` > Release prep > "Lying-indexdata test
//! fixtures" should land a small helper module (e.g.
//! `tests/common/adversarial.rs`) with:
//!
//! - `mutate_blob_header_indexdata(pbf_bytes, blob_idx, f)` so tests
//!   can inject reversed / overshooting ranges.
//! - `mutate_blob_payload(pbf_bytes, blob_idx, f)` so tests can
//!   truncate a varint or bump granularity.
//!
//! Each fix then gets a one-test-per-fix assertion that the relevant
//! command surfaces a clean error rather than panicking or silently
//! producing wrong output. The two tests in this file show the
//! expected pattern.

mod common;

use std::path::Path;

use common::{write_test_pbf, TestNode, TestRelation, TestWay};
use pbfhogg::apply_changes::{merge, MergeOptions};
use pbfhogg::block_builder::{self, BlockBuilder};
use pbfhogg::renumber::{renumber_external, RenumberOptions};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::HeaderOverrides;

// Arbitrary coordinates in decimicrodegrees; IDs are what matter here.
const LAT: i32 = 550_000_000; // 55.0 degrees
const LON: i32 = 120_000_000; // 12.0 degrees

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

    // Three blocks, IDs in an order that makes the LAST block's max_id
    // smaller than an EARLIER block's max_id:
    //   Block 1: ids 1..=100   (max_id = 100)
    //   Block 2: ids 500..=600 (max_id = 600)  <- global max is here
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

#[test]
fn renumber_survives_lying_sorted_header_out_of_order_blobs() {
    // Before the 2026-04-24 fix: renumber took `max_node_id =
    // pass1_schedule.last().max_id`, which equals 300 for this input.
    // The IdSet was pre-allocated for ids up to 300, and when pass 1
    // tried to record id 500 (Block 2) it panicked inside
    // `IdSet::set_atomic` with "pre_allocate only covers...".
    // After the fix: max_node_id is the scan max across the full
    // schedule (600), and the command runs to completion.
    let scratch = std::env::temp_dir().join(format!(
        "pbfhogg-cluster2-lying-sorted-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&scratch).expect("create scratch");
    let input = scratch.join("input.osm.pbf");
    let output = scratch.join("output.osm.pbf");
    write_lying_sorted_pbf(&input);

    let opts = RenumberOptions {
        start_node_id: 1,
        start_way_id: 1,
        start_relation_id: 1,
    };
    let result = renumber_external(
        &input,
        &output,
        &opts,
        Compression::default(),
        false,
        &HeaderOverrides::default(),
    );

    // We don't require the command to produce "correct" output
    // (the input is intentionally malformed). What we reject is a
    // panic in IdSet::set_atomic.
    match result {
        Ok(_) => {} // acceptable: the fix lets this complete
        Err(e) => {
            let msg = format!("{e}");
            assert!(
                !msg.contains("pre_allocate only covers"),
                "renumber panicked in IdSet::set_atomic despite the \
                 max_id scan fix: {msg}"
            );
        }
    }

    drop(std::fs::remove_dir_all(&scratch));
}

#[test]
fn apply_changes_rejects_unsorted_header() {
    // Before the 2026-04-24 fix: the is_sorted() gate fired only for
    // --locations-on-ways. The general path silently accepted
    // unsorted headers and could drop upsert creates.
    // After the fix: any apply-changes call rejects an unsorted
    // header upfront with a specific error message.
    let scratch = std::env::temp_dir().join(format!(
        "pbfhogg-cluster2-unsorted-apply-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&scratch).expect("create scratch");
    let base = scratch.join("base.osm.pbf");
    let diff = scratch.join("diff.osc.gz");
    let output = scratch.join("output.osm.pbf");

    // write_test_pbf defaults to sorted=false in the header builder.
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

    // Empty OSC (content doesn't matter; we expect to fail before
    // reading it).
    let file = std::fs::File::create(&diff).expect("create");
    let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    use std::io::Write;
    enc.write_all(b"<?xml version='1.0' encoding='UTF-8'?>\n<osmChange version='0.6'/>\n")
        .expect("write xml");
    enc.finish().expect("finish gz");

    let opts = MergeOptions {
        compression: Compression::default(),
        direct_io: false,
        io_uring: false,
        force: false,
        locations_on_ways: false,
        jobs: None,
    };
    let result = merge(&base, &diff, &output, &opts, &HeaderOverrides::default());
    let err = result.err().expect("merge should reject unsorted base header");
    let msg = format!("{err}");
    assert!(
        msg.contains("sorted base PBF"),
        "expected a sortedness error, got: {msg}"
    );

    drop(std::fs::remove_dir_all(&scratch));
}
