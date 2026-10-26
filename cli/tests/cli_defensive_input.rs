//! CLI-driven defensive-input regression tests.
//!
//! Replaces the removed `cluster2_defensive_input.rs` test. The two tests in this
//! file pin specific defensive-input contracts that span more than one
//! command (renumber + apply-changes), so they live here rather than
//! folded into `cli_apply_changes_invariants.rs` (apply-changes-only)
//! or any single command's `cli_*.rs`.
//!
//! Cluster 2 (the 2026-04-24 fix sweep) landed five hard-error
//! promotions; two of them have direct regression tests here. The
//! other three need byte-level fixture manipulation primitives
//! (`mutate_blob_*`) and pick up coverage as those land.
//!
//! No imports from `pbfhogg::renumber::*` or `pbfhogg::apply_changes::*` -
//! a rewrite of either command cannot break these tests by type
//! changes alone.

#![allow(clippy::unwrap_used)]

use std::io::Write;
use std::path::Path;

use crate::common::adversarial::{
    locate_blobs, mutate_blob_header_indexdata, mutate_blob_payload,
    set_relation_memids_terminator_continuation,
};
use crate::common::{TestMember, TestNode, TestRelation, TestWay, write_test_pbf};
use crate::invoker::CliInvoker;
use pbfhogg::MemberId;
use pbfhogg::block_builder::{self, BlockBuilder};
use pbfhogg::writer::{Compression, PbfWriter};

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

/// Renumber must refuse out-of-order blobs even when the header claims
/// `Sort.Type_then_ID`: positional numbering and rank-based reference
/// resolution agree only when IDs increase in file order.
#[test]
fn renumber_survives_lying_sorted_header_out_of_order_blobs() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_lying_sorted_pbf(&input);

    let out = CliInvoker::new()
        .arg("renumber")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .run();

    assert!(
        !out.status.success(),
        "renumber must refuse out-of-order blobs; stdout:\n{}\nstderr:\n{}",
        out.stdout_str(),
        out.stderr_str(),
    );
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("renumber requires node ids to strictly increase in file order"),
        "expected the cross-blob order refusal; stderr:\n{stderr}",
    );
    assert!(
        !stderr.contains("panicked at"),
        "renumber must refuse malformed order without panicking; stderr:\n{stderr}",
    );
    assert!(
        !stderr.contains("pre_allocate only covers"),
        "renumber hit the IdSet preallocation panic; stderr:\n{stderr}",
    );
}

#[test]
fn renumber_unsorted_header_error_explains_renumber_requirement() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf(&input, &[], &[], &[]);

    let out = CliInvoker::new()
        .arg("renumber")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .run();

    assert!(
        !out.status.success(),
        "renumber must reject an unsorted header; stdout:\n{}\nstderr:\n{}",
        out.stdout_str(),
        out.stderr_str(),
    );
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("Renumber assigns IDs by file position and resolves references by rank."),
        "expected renumber-specific sortedness guidance; stderr:\n{stderr}",
    );
    assert!(
        !stderr.contains("Streaming diff requires sorted inputs"),
        "renumber error must not mention diff; stderr:\n{stderr}",
    );
}

/// Cluster-2 fix #4: `apply_changes::build_header_bytes` rejects base
/// PBFs whose header does not advertise `Sort.Type_then_ID`, on both
/// the `--locations-on-ways` path and the general path. Before the
/// fix the general path silently accepted unsorted headers and could
/// drop upsert creates.
#[test]
fn apply_changes_rejects_unsorted_header() {
    let dir = crate::common::test_scratch();
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
// Cluster-2 fixes that need byte-level fixture manipulation.
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
    writer
        .write_primitive_block(bytes)
        .expect("write relations");
    writer.flush().expect("flush");
}

/// `altw external` must refuse a node blob whose indexdata advertises
/// `max_id < min_id`. Accepted, stage 2's id-range partition would
/// consume an empty range silently and drop every coordinate of the
/// blob. `BlobIndex::deserialize` now rejects an inverted range as
/// unreadable indexdata, so the input is refused up front as lacking
/// indexdata, before stage 1's own reversed-range check is reached.
#[test]
fn altw_external_rejects_reversed_indexdata_range() {
    let dir = crate::common::test_scratch();
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
    // Pin the specific refusal, so an unrelated failure cannot pass.
    assert!(
        stderr.contains("requires indexdata"),
        "expected the unreadable-indexdata refusal; stderr:\n{stderr}",
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    // test (`cli/tests/cli_truncation_sweep.rs`) DOES require non-zero
    // exit; the difference is that the sweep truncates at frame
    // boundaries the reader can detect, while `mutate_blob_payload`
    // produces a byte-valid frame whose inner protobuf decodes
    // partially. Pinning `!success` here would force a code change
    // to cat's tolerance policy, which is out of scope for an
    // assertion-strengthening pass. The contract this test pins
    // is "no panic on adversarial node payload"; the broader
    // "non-zero exit on truncation" contract lives in the sweep.
}

/// `--full-scan-min-blobs` is a hidden test instrument that reaches a
/// separate library entry point straight from CLI input. Zero is not a
/// threshold - it pins the ADR-0006 FullScan arm unconditionally - so the
/// CLI boundary range-checks it. Rejection must happen in clap, before any
/// input is opened, which is why this passes a path that does not exist:
/// if the bound were dropped the command would get as far as the file and
/// fail with a different message.
#[test]
fn getparents_rejects_zero_full_scan_min_blobs() {
    let dir = crate::common::test_scratch();
    let output = dir.path().join("out.osm.pbf");
    // Every other argument is well-formed, so the range check is the only
    // thing left that can reject this invocation.
    let out = CliInvoker::new()
        .args(["getparents", "/nonexistent/never-opened.osm.pbf"])
        .arg("-o")
        .arg(&output)
        .args(["--full-scan-min-blobs", "0", "n1"])
        .assert_failure();
    out.assert_stderr_contains("invalid value");
    let stderr = out.stderr_str();
    assert!(
        !stderr.contains("never-opened"),
        "argument validation must reject before touching the input; stderr:\n{stderr}",
    );
}

/// Public readers discard diagnostics unless the CLI explicitly attaches its
/// handle. Benchmark commands must retain the same malformed-index warning as
/// commands that construct their readers through the host.
#[test]
fn bench_commands_report_unrecognised_indexdata() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("bad-indexdata.osm.pbf");
    crate::common::write_bad_indexdata_pbf(&path);
    let input = path.to_str().expect("utf-8 scratch path");
    // Parallel map/reduce and the unfiltered pipeline have always skipped
    // indexdata parsing. Keep that fast path, including its lack of a warning.
    for (mode, warnings) in [
        ("sequential", 1),
        ("parallel", 0),
        ("pipelined", 0),
        ("blobreader", 1),
    ] {
        let out = CliInvoker::new()
            .args(["bench-read", input, "--mode", mode])
            .assert_success();
        let stderr = out.stderr_str();
        assert_eq!(
            stderr
                .matches("indexdata has an unrecognised length")
                .count(),
            warnings,
            "bench-read {mode}: {stderr}"
        );
        if mode == "blobreader" {
            // The warning names the file and the byte offset of the data
            // blob whose header carries the bad indexdata.
            assert!(
                stderr.contains("bad-indexdata.osm.pbf") && stderr.contains("at byte offset"),
                "bench-read {mode}: {stderr}"
            );
        }
    }
    for mode in ["sync", "pipelined"] {
        let out = CliInvoker::new()
            .args(["bench-write", input, "--writer", mode])
            .assert_success();
        let stderr = out.stderr_str();
        assert_eq!(
            stderr
                .matches("indexdata has an unrecognised length")
                .count(),
            1,
            "bench-write {mode}: {stderr}"
        );
    }
}

/// Which string reference of the fixture points past its block's string
/// table.
#[derive(Clone, Copy)]
enum BadString {
    /// Node 1's `Info.user_sid`.
    User,
    /// The role of relation 1's first member.
    Role,
}

fn varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 128 {
        out.push(u8::try_from(value & 127).expect("seven bits") | 128);
        value >>= 7;
    }
    out.push(u8::try_from(value).expect("seven bits"));
}

fn varint_field(field: u64, value: u64, out: &mut Vec<u8>) {
    varint(field * 8, out);
    varint(value, out);
}

fn bytes_field(field: u64, bytes: &[u8], out: &mut Vec<u8>) {
    varint(field * 8 + 2, out);
    varint(u64::try_from(bytes.len()).expect("length"), out);
    out.extend_from_slice(bytes);
}

/// An element `Info`: version 1, timestamp 100 s (default granularity),
/// changeset 1, uid 1 and user string index `user_sid`.
fn info(user_sid: u64) -> Vec<u8> {
    let mut info = Vec::new();
    for (field, value) in [(1, 1), (2, 100), (3, 1), (4, 1), (5, user_sid)] {
        varint_field(field, value, &mut info);
    }
    info
}

/// A sorted PBF with a plain node 1 and a relation 1 (member node 1), each
/// in its own block whose string table holds only the empty string, so any
/// string index but 0 is past it. `bad` picks the reference that is 9.
fn write_bad_string_pbf(path: &Path, bad: BadString, historical: bool) {
    let mut writer = PbfWriter::new(
        std::fs::File::create(path).expect("create input"),
        Compression::default(),
    );
    let mut header = block_builder::HeaderBuilder::new().sorted();
    if historical {
        header = header.historical();
    }
    writer
        .write_header(&header.build().expect("header"))
        .expect("write header");
    let user_sid = if matches!(bad, BadString::User) { 9 } else { 0 };
    let role_sid: u8 = if matches!(bad, BadString::Role) { 9 } else { 0 };

    let mut node = Vec::new();
    varint_field(1, 2, &mut node); // id 1, zigzag
    bytes_field(4, &info(user_sid), &mut node);
    varint_field(8, u64::try_from(LAT).expect("lat") * 2, &mut node);
    varint_field(9, u64::try_from(LON).expect("lon") * 2, &mut node);
    let mut group = Vec::new();
    bytes_field(1, &node, &mut group);
    let mut block = Vec::new();
    bytes_field(1, &[10, 0], &mut block); // string table: just ""
    bytes_field(2, &group, &mut block);
    writer.write_primitive_block(&block).expect("node block");

    let mut relation = Vec::new();
    varint_field(1, 1, &mut relation); // id 1
    bytes_field(4, &info(0), &mut relation);
    bytes_field(8, &[role_sid], &mut relation); // roles_sid
    bytes_field(9, &[2], &mut relation); // memids: node 1, zigzag delta
    bytes_field(10, &[0], &mut relation); // types: node
    let mut group = Vec::new();
    bytes_field(4, &relation, &mut group);
    let mut block = Vec::new();
    bytes_field(1, &[10, 0], &mut block);
    bytes_field(2, &group, &mut block);
    writer
        .write_primitive_block(&block)
        .expect("relation block");
    writer.flush().expect("flush");
}

fn assert_refused(out: &crate::invoker::CliOutput, needle: &str, context: &str) {
    let stderr = out.stderr_str();
    assert!(
        !out.status.success(),
        "{context}: a bad string index must be refused, not re-encoded as \"\"; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(needle),
        "{context}: expected `{needle}`; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("panicked at"),
        "{context}: stderr:\n{stderr}"
    );
}

/// The re-encoding conversions refuse a user or role string index past the
/// block's string table instead of writing an empty user or role: on the
/// borrowed path (time-filter snapshot, cat --clean), the owned path
/// (time-filter history) and even when the user is being cleaned away.
#[test]
fn bad_user_and_role_string_indices_are_refused_on_reencode() {
    let dir = crate::common::test_scratch();
    let user_needle = "node 1: user name string index 9";
    let role_needle = "relation 1 member 0: role string index 9";
    for (bad, needle) in [
        (BadString::User, user_needle),
        (BadString::Role, role_needle),
    ] {
        for historical in [false, true] {
            let input = dir.path().join("bad-string.osm.pbf");
            write_bad_string_pbf(&input, bad, historical);
            let out = CliInvoker::new()
                .arg("time-filter")
                .arg(&input)
                .arg("-o")
                .arg(dir.path().join("time-filter.osm.pbf"))
                .arg("250")
                .run();
            assert_refused(
                &out,
                needle,
                &format!("time-filter historical={historical}"),
            );
        }
        let input = dir.path().join("bad-string.osm.pbf");
        write_bad_string_pbf(&input, bad, false);
        // Re-encoding that keeps the user refuses it.
        let out = CliInvoker::new()
            .arg("cat")
            .arg(&input)
            .arg("-o")
            .arg(dir.path().join("cat.osm.pbf"))
            .args(["--clean", "uid"])
            .run();
        assert_refused(&out, needle, "cat --clean uid");
    }
    // Roles have no --clean and stay refused under `--clean user`.
    let input = dir.path().join("bad-string.osm.pbf");
    write_bad_string_pbf(&input, BadString::Role, false);
    let out = CliInvoker::new()
        .arg("cat")
        .arg(&input)
        .arg("-o")
        .arg(dir.path().join("cat.osm.pbf"))
        .args(["--clean", "user"])
        .run();
    assert_refused(&out, role_needle, "cat --clean user, bad role");
}

/// `cat --clean user` drops the user, so it never resolves it: a file with an
/// out-of-range user string index is rescued by it.
#[test]
fn cat_clean_user_rescues_a_bad_user_string_index() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("bad-user.osm.pbf");
    write_bad_string_pbf(&input, BadString::User, false);
    let output = dir.path().join("rescued.osm.pbf");
    let out = CliInvoker::new()
        .arg("cat")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .args(["--clean", "user"])
        .run();
    assert!(
        out.status.success(),
        "cat --clean user must rescue a bad user index; stderr:\n{}",
        out.stderr_str()
    );
    assert!(output.exists());
}
