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
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_lying_sorted_pbf(&input);

    let out = CliInvoker::new(&invocation_scratch)
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
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf(&input, &[], &[], &[]);

    let out = CliInvoker::new(&invocation_scratch)
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
    let invocation_scratch = pbfhogg_test_scratch::scratch();
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

    let out = CliInvoker::new(&invocation_scratch)
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
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_three_kind_fixture(&input);

    // Node blob is the first OSMData blob (index 1; index 0 is OSMHeader).
    // The indexdata layout (see `crates/lib/src/blob_meta/mod.rs`) is:
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

    let out = CliInvoker::new(&invocation_scratch)
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
    let invocation_scratch = pbfhogg_test_scratch::scratch();
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

    let out = CliInvoker::new(&invocation_scratch)
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
    // `crates/lib/src/commands/renumber/wire_rewrite.rs:556-563`. The truncated
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
    let invocation_scratch = pbfhogg_test_scratch::scratch();
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

    let out = CliInvoker::new(&invocation_scratch)
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
    // test (`crates/cli/tests/cli_truncation_sweep.rs`) DOES require non-zero
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
#[cfg(pbfhogg_test_hooks)]
#[test]
fn getparents_rejects_zero_full_scan_min_blobs() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let output = dir.path().join("out.osm.pbf");
    // Every other argument is well-formed, so the range check is the only
    // thing left that can reject this invocation.
    let out = CliInvoker::new(&invocation_scratch)
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

/// The threshold instrument is in the command tree exactly when the build
/// has test-hooks: a production build cannot be steered onto a dispatch arm.
#[test]
fn full_scan_min_blobs_exists_only_in_test_hooks_builds() {
    let command = pbfhogg_cli::cli_command();
    let getparents = command
        .get_subcommands()
        .find(|sub| sub.get_name() == "getparents")
        .expect("getparents subcommand");
    let present = getparents
        .get_arguments()
        .any(|arg| arg.get_long() == Some("full-scan-min-blobs"));
    assert_eq!(present, cfg!(pbfhogg_test_hooks));
}

/// Public readers discard diagnostics unless the CLI explicitly attaches its
/// handle. Benchmark commands must retain the same malformed-index warning as
/// commands that construct their readers through the host.
#[test]
fn bench_commands_report_unrecognised_indexdata() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let path = dir.path().join("bad-indexdata.osm.pbf");
    crate::common::write_bad_indexdata_pbf(&path);
    let input = path.as_os_str();
    // Parallel map/reduce and the unfiltered pipeline have always skipped
    // indexdata parsing. Keep that fast path, including its lack of a warning.
    for (mode, warnings) in [
        ("sequential", 1),
        ("parallel", 0),
        ("pipelined", 0),
        ("blobreader", 1),
    ] {
        let out = CliInvoker::new(&invocation_scratch)
            .args([
                std::ffi::OsStr::new("bench-read"),
                input,
                std::ffi::OsStr::new("--mode"),
                std::ffi::OsStr::new(mode),
            ])
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
        let out = CliInvoker::new(&invocation_scratch)
            .args([
                std::ffi::OsStr::new("bench-write"),
                input,
                std::ffi::OsStr::new("--writer"),
                std::ffi::OsStr::new(mode),
            ])
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
    let invocation_scratch = pbfhogg_test_scratch::scratch();
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
            let out = CliInvoker::new(&invocation_scratch)
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
        let out = CliInvoker::new(&invocation_scratch)
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
    let out = CliInvoker::new(&invocation_scratch)
        .arg("cat")
        .arg(&input)
        .arg("-o")
        .arg(dir.path().join("cat.osm.pbf"))
        .args(["--clean", "user"])
        .run();
    assert_refused(&out, role_needle, "cat --clean user, bad role");
}

/// The bench-write instrument refuses a bad role under both writers instead
/// of encoding an empty one: a revision whose role decoding fails does less
/// work and must never complete as a faster run.
#[test]
fn bench_write_refuses_a_bad_role_string_index() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let input = dir.path().join("bad-role.osm.pbf");
    write_bad_string_pbf(&input, BadString::Role, false);
    for mode in ["sync", "pipelined"] {
        let out = CliInvoker::new(&invocation_scratch)
            .arg("bench-write")
            .arg(&input)
            .args(["--writer", mode])
            .run();
        assert_refused(
            &out,
            "relation 1 member 0: role string index 9",
            &format!("bench-write --writer {mode}"),
        );
    }
}

/// `cat --clean user` drops the user, so it never resolves it: a file with an
/// out-of-range user string index is rescued by it.
#[test]
fn cat_clean_user_rescues_a_bad_user_string_index() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let input = dir.path().join("bad-user.osm.pbf");
    write_bad_string_pbf(&input, BadString::User, false);
    let output = dir.path().join("rescued.osm.pbf");
    let out = CliInvoker::new(&invocation_scratch)
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

// ---------------------------------------------------------------------------
// apply-changes' raw rewrite of a touched base blob
//
// A base element the diff leaves alone, in a blob the diff touches, is
// copied with its string indices unresolved into an output block whose
// string table starts as a copy of its source block's. The diff's own
// elements in that output block add their strings behind the copy, so a
// source index at or past the source table either names one of them (a
// valid file with the wrong string) or lies past everything (a tag key
// there used to panic the run). Either is refused.
// ---------------------------------------------------------------------------

/// Which string index of [`write_raw_rewrite_pbf`]'s second element of a
/// block is bad. The first element of each block is the one the diff
/// modifies; the second is copied raw.
#[derive(Clone, Copy, Debug)]
enum RawSid {
    DenseKey,
    DenseVal,
    DenseUser,
    NodeKey,
    NodeVal,
    NodeUser,
    WayKey,
    WayVal,
    WayUser,
    RelationKey,
    RelationRole,
    RelationUser,
}

impl RawSid {
    const ALL: [RawSid; 12] = [
        RawSid::DenseKey,
        RawSid::DenseVal,
        RawSid::DenseUser,
        RawSid::NodeKey,
        RawSid::NodeVal,
        RawSid::NodeUser,
        RawSid::WayKey,
        RawSid::WayVal,
        RawSid::WayUser,
        RawSid::RelationKey,
        RawSid::RelationRole,
        RawSid::RelationUser,
    ];

    /// The refusal's element and field, ahead of the index.
    fn needle(self) -> &'static str {
        match self {
            RawSid::DenseKey => "node 2: tag key string index",
            RawSid::DenseVal => "node 2: tag value string index",
            RawSid::DenseUser => "node 2: user name string index",
            RawSid::NodeKey => "node 4: tag key string index",
            RawSid::NodeVal => "node 4: tag value string index",
            RawSid::NodeUser => "node 4: user name string index",
            RawSid::WayKey => "way 11: tag key string index",
            RawSid::WayVal => "way 11: tag value string index",
            RawSid::WayUser => "way 11: user name string index",
            RawSid::RelationKey => "relation 21: tag key string index",
            RawSid::RelationRole => "relation 21 member 0: role string index",
            RawSid::RelationUser => "relation 21: user name string index",
        }
    }
}

/// Every block's string table: four entries, so index 4 is the first slot
/// the diff's strings take in a rewritten block.
const RAW_TABLE: [&str; 4] = ["", "k", "v", "u"];

fn raw_string_table() -> Vec<u8> {
    let mut table = Vec::new();
    for s in RAW_TABLE {
        bytes_field(1, s.as_bytes(), &mut table);
    }
    table
}

fn packed(values: &[u64]) -> Vec<u8> {
    let mut out = Vec::new();
    for &v in values {
        varint(v, &mut out);
    }
    out
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)).cast_unsigned()
}

fn raw_block(group: &[u8]) -> Vec<u8> {
    let mut block = Vec::new();
    bytes_field(1, &raw_string_table(), &mut block);
    bytes_field(2, group, &mut block);
    block
}

/// Dense nodes 1 and 2, each tagged `k=v` with user `u`, except where
/// `bad` puts `sid` into node 2.
fn raw_dense_block(bad: Option<(RawSid, u64)>) -> Vec<u8> {
    let pick = |which: RawSid, good: u64| match bad {
        Some((b, sid)) if std::mem::discriminant(&b) == std::mem::discriminant(&which) => sid,
        _ => good,
    };
    let (key, val, user) = (
        pick(RawSid::DenseKey, 1),
        pick(RawSid::DenseVal, 2),
        pick(RawSid::DenseUser, 3),
    );
    let user = i64::try_from(user).expect("dense user index fits i64");
    let mut info = Vec::new();
    bytes_field(1, &packed(&[1, 1]), &mut info);
    bytes_field(2, &packed(&[zigzag(100), 0]), &mut info);
    bytes_field(3, &packed(&[zigzag(1), 0]), &mut info);
    bytes_field(4, &packed(&[zigzag(1), 0]), &mut info);
    bytes_field(5, &packed(&[zigzag(3), zigzag(user - 3)]), &mut info);
    let lat = i64::from(LAT);
    let lon = i64::from(LON);
    let mut dense = Vec::new();
    bytes_field(1, &packed(&[zigzag(1), zigzag(1)]), &mut dense);
    bytes_field(5, &info, &mut dense);
    bytes_field(8, &packed(&[zigzag(lat), 0]), &mut dense);
    bytes_field(9, &packed(&[zigzag(lon), 0]), &mut dense);
    bytes_field(10, &packed(&[1, 2, 0, key, val, 0]), &mut dense);
    let mut group = Vec::new();
    bytes_field(2, &dense, &mut group);
    raw_block(&group)
}

/// Plain `Node` messages 3 and 4, as [`raw_dense_block`].
fn raw_node_block(bad: Option<(RawSid, u64)>) -> Vec<u8> {
    let pick = |which: RawSid, good: u64| match bad {
        Some((b, sid)) if std::mem::discriminant(&b) == std::mem::discriminant(&which) => sid,
        _ => good,
    };
    let mut group = Vec::new();
    for (id, key, val, user) in [
        (3, 1, 2, 3),
        (
            4,
            pick(RawSid::NodeKey, 1),
            pick(RawSid::NodeVal, 2),
            pick(RawSid::NodeUser, 3),
        ),
    ] {
        let mut node = Vec::new();
        varint_field(1, zigzag(id), &mut node);
        bytes_field(2, &packed(&[key]), &mut node);
        bytes_field(3, &packed(&[val]), &mut node);
        bytes_field(4, &info(user), &mut node);
        varint_field(8, zigzag(i64::from(LAT)), &mut node);
        varint_field(9, zigzag(i64::from(LON)), &mut node);
        bytes_field(1, &node, &mut group);
    }
    raw_block(&group)
}

/// Ways 10 and 11 (refs 1, 2), as [`raw_dense_block`].
fn raw_way_block(bad: Option<(RawSid, u64)>) -> Vec<u8> {
    let pick = |which: RawSid, good: u64| match bad {
        Some((b, sid)) if std::mem::discriminant(&b) == std::mem::discriminant(&which) => sid,
        _ => good,
    };
    let mut group = Vec::new();
    for (id, key, val, user) in [
        (10, 1, 2, 3),
        (
            11,
            pick(RawSid::WayKey, 1),
            pick(RawSid::WayVal, 2),
            pick(RawSid::WayUser, 3),
        ),
    ] {
        let mut way = Vec::new();
        varint_field(1, id, &mut way);
        bytes_field(2, &packed(&[key]), &mut way);
        bytes_field(3, &packed(&[val]), &mut way);
        bytes_field(4, &info(user), &mut way);
        bytes_field(8, &packed(&[zigzag(1), zigzag(1)]), &mut way);
        bytes_field(3, &way, &mut group);
    }
    raw_block(&group)
}

/// Relations 20 and 21 (one node 1 member, role `v`), as
/// [`raw_dense_block`].
fn raw_relation_block(bad: Option<(RawSid, u64)>) -> Vec<u8> {
    let pick = |which: RawSid, good: u64| match bad {
        Some((b, sid)) if std::mem::discriminant(&b) == std::mem::discriminant(&which) => sid,
        _ => good,
    };
    let mut group = Vec::new();
    for (id, key, role, user) in [
        (20, 1, 2, 3),
        (
            21,
            pick(RawSid::RelationKey, 1),
            pick(RawSid::RelationRole, 2),
            pick(RawSid::RelationUser, 3),
        ),
    ] {
        let mut relation = Vec::new();
        varint_field(1, id, &mut relation);
        bytes_field(2, &packed(&[key]), &mut relation);
        bytes_field(3, &packed(&[2]), &mut relation);
        bytes_field(4, &info(user), &mut relation);
        bytes_field(8, &packed(&[role]), &mut relation);
        bytes_field(9, &packed(&[zigzag(1)]), &mut relation);
        bytes_field(10, &packed(&[0]), &mut relation);
        bytes_field(4, &relation, &mut group);
    }
    raw_block(&group)
}

/// A sorted, indexed base of four blocks (dense nodes, plain nodes, ways,
/// relations), each blob indexed for its valid form, with `bad`'s block's
/// payload then swapped for the malformed one: the writer's tag scan
/// refuses an out-of-range key, and an indexed blob is what apply-changes
/// rewrites without scanning.
fn write_raw_rewrite_pbf(path: &Path, bad: Option<(RawSid, u64)>) {
    {
        let mut writer = PbfWriter::new(
            std::fs::File::create(path).expect("create input"),
            Compression::default(),
        );
        let header = block_builder::HeaderBuilder::new()
            .sorted()
            .build()
            .expect("header");
        writer.write_header(&header).expect("write header");
        for block in [
            raw_dense_block(None),
            raw_node_block(None),
            raw_way_block(None),
            raw_relation_block(None),
        ] {
            writer.write_primitive_block(&block).expect("block");
        }
        writer.flush().expect("flush");
    }
    let Some((which, _)) = bad else {
        return;
    };
    let (frame, block) = match which {
        RawSid::DenseKey | RawSid::DenseVal | RawSid::DenseUser => (1, raw_dense_block(bad)),
        RawSid::NodeKey | RawSid::NodeVal | RawSid::NodeUser => (2, raw_node_block(bad)),
        RawSid::WayKey | RawSid::WayVal | RawSid::WayUser => (3, raw_way_block(bad)),
        RawSid::RelationKey | RawSid::RelationRole | RawSid::RelationUser => {
            (4, raw_relation_block(bad))
        }
    };
    let pbf = std::fs::read(path).expect("read input");
    let pbf = mutate_blob_payload(&pbf, frame, |payload| *payload = block);
    std::fs::write(path, pbf).expect("write input");
}

/// A diff modifying the first element of every block, with strings none
/// of the base's tables hold.
fn write_raw_rewrite_osc(path: &Path) {
    let meta = r#"version="2" timestamp="2020-01-01T00:00:00Z" changeset="2" uid="2" user="osc""#;
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="1" lat="55.0" lon="12.0" {meta}><tag k="name" v="new"/></node>
    <node id="3" lat="55.0" lon="12.0" {meta}><tag k="name" v="new"/></node>
    <way id="10" {meta}><nd ref="1"/><nd ref="2"/><tag k="name" v="new"/></way>
    <relation id="20" {meta}><member type="node" ref="1" role="new"/><tag k="name" v="new"/></relation>
  </modify>
</osmChange>"#
    );
    let file = std::fs::File::create(path).expect("create osc");
    let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    enc.write_all(xml.as_bytes()).expect("write xml");
    enc.finish().expect("finish gz");
}

fn run_raw_rewrite(dir: &Path, base: &Path) -> crate::invoker::CliOutput {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let osc = dir.join("diff.osc.gz");
    write_raw_rewrite_osc(&osc);
    CliInvoker::new(&invocation_scratch)
        .arg("apply-changes")
        .arg(base)
        .arg(&osc)
        .arg("-o")
        .arg(dir.join("output.osm.pbf"))
        .arg("--overwrite")
        .run()
}

/// The valid form of the fixture rewrites every blob and succeeds, so the
/// refusals below are about the bad index and nothing else.
#[test]
fn apply_changes_rewrites_the_valid_raw_rewrite_fixture() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    write_raw_rewrite_pbf(&base, None);
    let out = run_raw_rewrite(dir.path(), &base);
    assert!(
        out.status.success(),
        "the valid fixture must apply; stderr:\n{}",
        out.stderr_str()
    );
    let c = crate::common::read_all_elements_with_coords(&dir.path().join("output.osm.pbf"));
    let kv = vec![("k".to_string(), "v".to_string())];
    let new = vec![("name".to_string(), "new".to_string())];
    let node_tags: Vec<_> = c.nodes.iter().map(|n| (n.0, n.3.clone())).collect();
    assert_eq!(
        node_tags,
        vec![
            (1, new.clone()),
            (2, kv.clone()),
            (3, new.clone()),
            (4, kv.clone())
        ]
    );
    let way_tags: Vec<_> = c.ways.iter().map(|w| (w.0, w.2.clone())).collect();
    assert_eq!(way_tags, vec![(10, new.clone()), (11, kv.clone())]);
    let relations: Vec<_> = c
        .relations
        .iter()
        .map(|r| (r.0, r.1[0].2.clone(), r.2.clone()))
        .collect();
    assert_eq!(
        relations,
        vec![(20, "new".to_string(), new), (21, "v".to_string(), kv)]
    );
}

/// A string index of a copied base element at or past its source block's
/// table is refused, naming the element, field and index: one inside the
/// slots the diff's strings take behind the copied table (which used to
/// name a diff string silently), one past those too (a tag key there used
/// to panic), and, for a plain node's key, one at or past 2^31 (which the
/// copy used to carry as a negative dense index).
#[test]
fn apply_changes_refuses_a_bad_string_index_in_a_rewritten_blob() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    for which in RawSid::ALL {
        for sid in [4, 40] {
            write_raw_rewrite_pbf(&base, Some((which, sid)));
            let out = run_raw_rewrite(dir.path(), &base);
            assert_refused(
                &out,
                &format!("{} {sid}", which.needle()),
                &format!("{which:?} index {sid}"),
            );
            assert_eq!(out.status.code(), Some(2), "{which:?} index {sid}");
            assert!(
                !dir.path().join("output.osm.pbf").exists(),
                "{which:?} index {sid}: a refused run publishes nothing"
            );
        }
    }
    write_raw_rewrite_pbf(&base, Some((RawSid::NodeKey, 1 << 31)));
    let out = run_raw_rewrite(dir.path(), &base);
    assert_refused(
        &out,
        &format!("{} {}", RawSid::NodeKey.needle(), 1u64 << 31),
        "NodeKey index 2^31",
    );
}
