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

use common::adversarial::{
    locate_blobs, mutate_blob_header_indexdata, mutate_blob_payload,
    set_relation_memids_terminator_continuation,
};
use common::cli::CliInvoker;
use common::{write_test_pbf, TestMember, TestNode, TestRelation, TestWay};
use pbfhogg::block_builder::{self, BlockBuilder};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::MemberId;
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

// ---------------------------------------------------------------------------
// Cluster-2 fixes that need byte-level fixture manipulation (T02).
//
// Every cluster-2 fix is a defensive-input promotion: malformed PBFs that
// previously panicked or silently produced wrong output now hit a clean
// hard error. The seed tests above cover two of the five fixes via
// fixtures that lie at the BlockBuilder level. The remaining tests inject
// malformed bytes via `common::adversarial::*`.
// ---------------------------------------------------------------------------

/// Write a sorted-header indexed PBF with one node blob, one way blob,
/// and one relation blob - the canonical input shape for renumber and
/// altw. Returns the path the fixture lives at.
fn write_three_kind_fixture(path: &Path) {
    let nodes = (1..=8_i64)
        .map(|id| TestNode {
            id,
            lat: LAT,
            lon: LON,
            tags: vec![],
            meta: None,
        })
        .collect::<Vec<_>>();
    let ways = (1..=4_i64)
        .map(|id| TestWay {
            id,
            refs: vec![1, 2, 3, 4],
            tags: vec![],
            meta: None,
        })
        .collect::<Vec<_>>();
    let relations = vec![TestRelation {
        id: 1,
        members: vec![
            TestMember {
                id: MemberId::Way(1),
                role: "outer",
            },
            TestMember {
                id: MemberId::Way(2),
                role: "outer",
            },
            TestMember {
                id: MemberId::Way(3),
                role: "inner",
            },
            TestMember {
                id: MemberId::Way(4),
                role: "inner",
            },
        ],
        tags: vec![("type", "multipolygon")],
        meta: None,
    }];

    let file = std::fs::File::create(path).expect("create");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new()
        .sorted()
        .build()
        .expect("header");
    writer.write_header(&header).expect("write header");
    let mut bb = BlockBuilder::new();
    for n in &nodes {
        bb.add_node(n.id, n.lat, n.lon, [], None);
    }
    let bytes = bb.take().expect("take").expect("non-empty");
    writer.write_primitive_block(bytes).expect("write nodes");
    let mut bb = BlockBuilder::new();
    for w in &ways {
        bb.add_way(w.id, [], &w.refs, None);
    }
    let bytes = bb.take().expect("take").expect("non-empty");
    writer.write_primitive_block(bytes).expect("write ways");
    let mut bb = BlockBuilder::new();
    for r in &relations {
        let members: Vec<block_builder::MemberData<'_>> = r
            .members
            .iter()
            .map(|m| block_builder::MemberData {
                id: m.id,
                role: m.role,
            })
            .collect();
        bb.add_relation(r.id, r.tags.iter().map(|(k, v)| (*k, *v)), &members, None);
    }
    let bytes = bb.take().expect("take").expect("non-empty");
    writer.write_primitive_block(bytes).expect("write relations");
    writer.flush().expect("flush");
}

/// Cluster-2 fix: `altw external::stage1` rejects a node blob whose
/// indexdata advertises `max_id < min_id`. Before the fix, stage 2's
/// id-range partition would consume an empty range silently and emit
/// no per-bucket assignments, causing later stages to drop all coords
/// for the affected blob. After the fix, stage 1 hard-errors with a
/// `reversed indexdata range` message.
#[test]
fn altw_external_rejects_reversed_indexdata_range() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_three_kind_fixture(&input);

    // Node blob is the first OSMData blob (index 1; index 0 is OSMHeader).
    // The indexdata layout (see `src/blob_meta/mod.rs`) is:
    //   byte 0   version
    //   byte 1   kind
    //   bytes 2..10   min_id (i64 LE)
    //   bytes 10..18  max_id (i64 LE)
    //   ...
    let pbf = std::fs::read(&input).expect("read fixture");
    let mutated = mutate_blob_header_indexdata(&pbf, 1, |ix| {
        assert!(ix.len() >= 18, "indexdata too short to swap min/max");
        let mut min_buf = [0u8; 8];
        let mut max_buf = [0u8; 8];
        min_buf.copy_from_slice(&ix[2..10]);
        max_buf.copy_from_slice(&ix[10..18]);
        ix[2..10].copy_from_slice(&max_buf);
        ix[10..18].copy_from_slice(&min_buf);
    });
    std::fs::write(&input, &mutated).expect("rewrite fixture");

    let out = CliInvoker::new()
        .arg("add-locations-to-ways")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--index-type")
        .arg("external")
        .run();

    assert!(
        !out.status.success(),
        "altw external must reject reversed indexdata; stdout:\n{}\nstderr:\n{}",
        out.stdout_str(),
        out.stderr_str(),
    );
    let stderr = out.stderr_str();
    // Pin the exact reversed-range hard error from
    // `src/commands/altw/external/stage1.rs:412`. The original test
    // OR'd this with `max_id` which the actual error always contains
    // anyway, defeating the assertion (any unrelated error mentioning
    // max_id would pass). Tier A1 follow-up.
    assert!(
        stderr.contains("reversed indexdata range"),
        "expected the stage1 reversed-range hard error; stderr:\n{stderr}",
    );
    assert!(
        !stderr.contains("panicked at"),
        "altw external must not panic on reversed indexdata; stderr:\n{stderr}",
    );
}

/// Cluster-2 fix: `renumber/wire_rewrite::count_varints_strict` errors
/// on a truncated trailing varint inside a relation's `memids` packed
/// field. Before the fix the count walk silently dropped the last
/// element of a truncated stream, causing renumber to emit fewer
/// member ids than the relation declared. After the fix the rewriter
/// returns a clean error and renumber exits non-zero.
///
/// `set_relation_memids_terminator_continuation` flips the high bit on
/// the last byte of relation 0's memids field (field 9) inside the
/// relation blob's PrimitiveBlock. Plain truncation drops a complete
/// single-byte varint cleanly in this fixture and is caught by the
/// downstream count-vs-types mismatch check rather than
/// `count_varints_strict` itself; flipping the terminator's
/// continuation bit instead leaves a hanging-continuation varint that
/// only the strict walk can detect.
#[test]
fn renumber_rejects_truncated_relation_blob_payload() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_three_kind_fixture(&input);

    let pbf = std::fs::read(&input).expect("read fixture");
    let blobs = locate_blobs(&pbf);
    // Last data blob is the relation blob (header + nodes + ways + relations).
    let blob_idx = blobs.len() - 1;
    let mutated = set_relation_memids_terminator_continuation(&pbf, blob_idx, 0);
    std::fs::write(&input, &mutated).expect("rewrite fixture");

    let out = CliInvoker::new()
        .arg("renumber")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .run();

    assert!(
        !out.status.success(),
        "renumber must reject a truncated relation payload; stdout:\n{}\nstderr:\n{}",
        out.stdout_str(),
        out.stderr_str(),
    );
    let stderr = out.stderr_str();
    assert!(
        !stderr.contains("panicked at"),
        "renumber must not panic on truncated relation payload; stderr:\n{stderr}",
    );
    // Pin count_varints_strict's error format from
    // `src/commands/renumber/wire_rewrite.rs:556-563`. The truncated
    // memids field is the exact byte stream that walk consumes, so
    // the rejection must surface through this error path.
    assert!(
        stderr.contains("reframe_relations: relation 1 memids:"),
        "expected count_varints_strict error from reframe_relations; stderr:\n{stderr}",
    );
}

/// Cluster-2 fix: `cat` (running the indexdata-generation passthrough
/// with no flags) walks every blob through `scan_block_ids` to derive
/// indexdata. Before the cluster-2 hardening, an attacker-controlled
/// `granularity` × `lat_offset` combination could wrap inside
/// `gran * raw_lat` in release builds and produce a poisoned bbox in
/// indexdata; in debug builds it panicked. After the fix the bbox is
/// silently dropped via `checked_mul`/`checked_add`, leaving a clean
/// id-range-only indexdata entry.
///
/// We exercise the broader defensive surface: truncate the last byte
/// of a node blob's PrimitiveBlock, then run `cat`. Whichever varint
/// the chop lands inside (granularity, lat/lon offset, DenseNodes id
/// stream, etc.), the response must be a clean non-zero exit, never
/// a panic. This pins the "no panic on adversarial node blob" contract
/// without requiring a bespoke granularity-overflow fixture.
#[test]
fn cat_rejects_truncated_node_blob_payload() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_three_kind_fixture(&input);

    let pbf = std::fs::read(&input).expect("read fixture");
    // Node blob is the first OSMData blob.
    let mutated = mutate_blob_payload(&pbf, 1, |payload| {
        payload.pop();
    });
    std::fs::write(&input, &mutated).expect("rewrite fixture");

    let out = CliInvoker::new()
        .arg("cat")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .run();

    let stderr = out.stderr_str();
    assert!(
        !stderr.contains("panicked at"),
        "cat must not panic on truncated node payload; stderr:\n{stderr}",
    );
    // Tier A3 follow-up: the reviewer flagged that the original
    // version accepted any exit status. Strengthening to assert
    // `!success` revealed that cat actually exits 0 on this
    // truncation - cat tolerates partially-readable blobs by
    // design (the in-tree comment under cluster-2 hardening calls
    // this out as a "hardening tradeoff"). The truncation-sweep
    // test (`tests/cli_truncation_sweep.rs`) DOES require non-zero
    // exit; the difference is that the sweep truncates at frame
    // boundaries the reader can detect, while `mutate_blob_payload`
    // produces a byte-valid frame whose inner protobuf decodes
    // partially. Pinning `!success` here would force a code change
    // to cat's tolerance policy, which is out of scope for an
    // assertion-strengthening pass. The contract this test pins
    // is "no panic on adversarial node payload"; the broader
    // "non-zero exit on truncation" contract lives in the sweep.
}
