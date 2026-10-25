//! CLI-driven invariant tests for `pbfhogg apply-changes`.
//!
//! Sibling to `cli_apply_changes.rs`; replaces the removed
//! library-API `apply_changes_invariants.rs` test. Pins correctness
//! invariants the descriptor-first streaming rewrite had to preserve
//! byte-for-byte, and any later rewrite must still preserve. Each
//! test pins one invariant:
//!
//! - `cursor_rule_*`: Rewrite slots advance UpsertCursors past their blob's ID range; Passthrough/FalsePositive slots do NOT.
//! - `empty_base_pbf_*`: `last_type == None` forever; trailing creates must flush all three kinds.
//! - `trailing_creates_after_*`: Type-transition flush correctness.
//! - `modify_on_missing_id_*`, `delete_on_missing_id_*`, `create_on_existing_id_*`: Permissive missing-element semantics (reference/osmium-parity.md).
//! - `merge_jobs_parity_*`: Output is independent of worker count between `-j 2` and `-j 4`.
//!
//! No imports from `pbfhogg::apply_changes::*` or `pbfhogg::altw::*`.
//! The parity tests bootstrap their LocationsOnWays base via
//! `pbfhogg add-locations-to-ways` (CLI), and the merge runs are
//! `pbfhogg apply-changes` (CLI). A rewrite of either command cannot
//! break these tests by type changes alone.

#![allow(clippy::unwrap_used)]
#![allow(clippy::too_many_lines)]

use std::fs::File;
use std::io::Write;
use std::path::Path;

use crate::common::{
    TestNode, TestWay, assert_elements_equivalent, generate_nodes, generate_ways,
    read_all_elements_with_coords as read_all_elements, write_multi_block_test_pbf,
    write_test_pbf_sorted,
};
use crate::invoker::{CliInvoker, CliOutput};
use flate2::write::GzEncoder;

fn write_osc(path: &Path, xml: &str) {
    let file = File::create(path).expect("create osc file");
    let mut enc = GzEncoder::new(file, flate2::Compression::fast());
    enc.write_all(xml.as_bytes()).expect("write xml");
    enc.finish().expect("finish gz");
}

fn run_apply_changes(
    base: &Path,
    osc: &Path,
    output: &Path,
    jobs: Option<usize>,
    locations_on_ways: bool,
) -> CliOutput {
    let mut cli = CliInvoker::new()
        .arg("apply-changes")
        .arg(base)
        .arg(osc)
        .arg("-o")
        .arg(output);
    // `--locations-on-ways` needs an indexed base whatever `--force` says,
    // so it refuses the flag.
    cli = if locations_on_ways {
        cli.arg("--locations-on-ways")
    } else {
        cli.arg("--force")
    };
    if let Some(j) = jobs {
        cli = cli.arg("-j").arg(j.to_string());
    }
    cli.assert_success()
}

fn run_apply_simple(base: &Path, osc: &Path, output: &Path) -> CliOutput {
    run_apply_changes(base, osc, output, None, false)
}

/// Bootstrap a LocationsOnWays-enriched base PBF via the
/// `add-locations-to-ways` CLI.
fn bootstrap_low_base(input: &Path, output: &Path) {
    CliInvoker::new()
        .arg("add-locations-to-ways")
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--keep-untagged-nodes")
        .arg("--force")
        .assert_success();
}

#[allow(clippy::cast_possible_wrap)]
fn write_merge_jobs_fixture(base: &Path, osc: &Path) {
    let mut nodes = generate_nodes(24, 1);
    for (i, node) in nodes.iter_mut().enumerate() {
        if i % 4 == 0 {
            node.tags = vec![("name", "base")];
        }
    }

    let mut ways = generate_ways(10, 1_000, 3, 1);
    for (i, way) in ways.iter_mut().enumerate() {
        let start = 1 + i as i64 * 2;
        way.refs = vec![start, start + 1, start + 2];
        way.tags = if i % 2 == 0 {
            vec![("highway", "residential")]
        } else {
            vec![("highway", "service")]
        };
    }

    write_multi_block_test_pbf(base, &nodes, &ways, &[], 4);

    write_osc(
        osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="30" lat="0.3000000" lon="0.6000000" version="1">
      <tag k="created" v="yes"/>
    </node>
    <way id="2000" version="1">
      <nd ref="5"/>
      <nd ref="30"/>
      <nd ref="6"/>
      <tag k="highway" v="primary"/>
    </way>
  </create>
  <modify>
    <node id="5" lat="0.5555555" lon="0.4444444" version="2">
      <tag k="name" v="modified"/>
    </node>
    <way id="1003" version="2">
      <nd ref="7"/>
      <nd ref="5"/>
      <nd ref="30"/>
      <tag k="highway" v="secondary"/>
      <tag k="surface" v="gravel"/>
    </way>
  </modify>
  <delete>
    <node id="23" version="1"/>
    <way id="1007" version="1"/>
  </delete>
</osmChange>"#,
    );
}

/// Pull just the "Wrote/Base/Diff/Deleted/Blobs" lines out of a
/// stats summary stderr block. The Blobs line includes total-blob
/// count which is jobs-independent (both runs decode the same
/// number of blobs); the byte-rewrite-ratio and blob-size lines
/// are deliberately *not* compared - they vary with worker
/// scheduling on multi-blob inputs.
fn stats_block(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|l| {
            l.starts_with("Merge complete:")
                || l.starts_with("  Base:")
                || l.starts_with("  Diff:")
                || l.starts_with("  Deleted:")
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Cursor rule: Passthrough / FalsePositive slots must NOT advance the cursor -
// but a blob may only take the passthrough slot when no upsert lands inside its
// ID span. A pure create in a blob's interior matches no base element, so the
// precise `block_overlaps_diff` check says "false positive" while the create is
// still owed inside that range; passing the blob through verbatim then emits
// the create after larger IDs, under a header that stamps Sort.Type_then_ID.
//
// These two tests previously asserted the misordered output as the expected
// answer ("blob-tail order, not OSM-sorted"). They now assert the ordering the
// header claims.
// ---------------------------------------------------------------------------

#[test]
fn interior_create_in_false_positive_blob_stays_sorted() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 100_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 200_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 10,
                lat: 300_000_000,
                lon: 300_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 100,
            refs: vec![1, 2, 10],
            tags: vec![],
            meta: None,
        }],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="5" lat="40.0" lon="40.0" version="1"/>
  </create>
</osmChange>"#,
    );

    run_apply_simple(&base, &osc, &output);
    let c = read_all_elements(&output);

    let node_ids: Vec<i64> = c.nodes.iter().map(|n| n.0).collect();
    assert!(
        node_ids.contains(&5),
        "FalsePositive cursor rule: id=5 must be present. Got {node_ids:?}",
    );
    assert_eq!(
        node_ids,
        vec![1, 2, 5, 10],
        "id=5 falls inside the blob's [1, 10] span, so it must be interleaved \
         at its sorted position rather than trailing the blob"
    );

    assert_eq!(c.ways.iter().map(|w| w.0).collect::<Vec<_>>(), vec![100]);
}

#[test]
fn interior_create_in_final_false_positive_blob_stays_sorted() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 100_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 10,
                lat: 300_000_000,
                lon: 300_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="5" lat="40.0" lon="40.0" version="1"/>
  </create>
</osmChange>"#,
    );

    run_apply_simple(&base, &osc, &output);
    let c = read_all_elements(&output);

    let node_ids: Vec<i64> = c.nodes.iter().map(|n| n.0).collect();
    assert!(
        node_ids.contains(&5),
        "id=5 must be present (got {node_ids:?})"
    );
    assert_eq!(
        node_ids,
        vec![1, 5, 10],
        "id=5 falls inside the only blob's [1, 10] span, so it must be \
         interleaved at its sorted position even though nothing follows"
    );
}

// ---------------------------------------------------------------------------
// Output sortedness: `build_header_bytes` stamps `Sort.Type_then_ID`
// unconditionally, so the drain must refuse to write blobs that contradict it.
// ---------------------------------------------------------------------------

/// The base header's `is_sorted()` gate is a claim, not proof - `degrade
/// --unsort` exists to produce files where the two disagree. When the base is
/// unsorted underneath a sorted stamp, apply-changes used to copy the
/// disorder into the output and re-stamp it as sorted, handing every
/// downstream consumer a file that lies about its ordering.
#[test]
fn unsorted_base_under_a_sorted_stamp_is_refused() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    // Two node blobs whose ID spans go backwards: 10..20 then 5..6. The
    // header still stamps Sort.Type_then_ID (write_multi_block_test_pbf
    // always does), which is exactly the shape being guarded against.
    let nodes: Vec<TestNode> = [10, 20, 5, 6]
        .into_iter()
        .map(|id| TestNode {
            id,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        })
        .collect();
    write_multi_block_test_pbf(&base, &nodes, &[], &[], 2);

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="99" lat="40.0" lon="40.0" version="1"/>
  </create>
</osmChange>"#,
    );

    let out = CliInvoker::new()
        .arg("apply-changes")
        .arg(&base)
        .arg(&osc)
        .arg("-o")
        .arg(&output)
        .arg("--force")
        .run();

    assert_eq!(
        out.status.code(),
        Some(2),
        "an unsorted base must fail rather than yield a falsely-stamped file; stderr:\n{}",
        out.stderr_str(),
    );
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("refusing to write an unsorted PBF"),
        "error should say what it refused to write; got:\n{stderr}"
    );
}

/// The same fixture shape, but honestly ordered, must still succeed - without
/// this the guard above would be satisfied by a build that refused every
/// multi-blob input.
#[test]
fn sorted_multi_blob_base_still_succeeds() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    let nodes: Vec<TestNode> = [5, 6, 10, 20]
        .into_iter()
        .map(|id| TestNode {
            id,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        })
        .collect();
    write_multi_block_test_pbf(&base, &nodes, &[], &[], 2);

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="99" lat="40.0" lon="40.0" version="1"/>
  </create>
</osmChange>"#,
    );

    run_apply_simple(&base, &osc, &output);
    let c = read_all_elements(&output);
    let ids: Vec<i64> = c.nodes.iter().map(|n| n.0).collect();
    assert_eq!(ids, vec![5, 6, 10, 20, 99]);
}

// ---------------------------------------------------------------------------
// Forced runs over an unindexed base write a wholly indexed file. A blob the
// diff leaves alone used to be copied without indexdata beside the indexed
// rewrites, a partially indexed output every schedule walk refuses. Blocks no
// single index describes are hand-encoded here, since `BlockBuilder` never
// writes them.
// ---------------------------------------------------------------------------

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(u8::try_from(value & 0x7F).unwrap() | 0x80);
        value >>= 7;
    }
    out.push(u8::try_from(value).unwrap());
}

fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)).cast_unsigned()
}

fn put_len(out: &mut Vec<u8>, field: u64, body: &[u8]) {
    put_varint(out, field << 3 | 2);
    put_varint(out, u64::try_from(body.len()).unwrap());
    out.extend_from_slice(body);
}

fn put_uint(out: &mut Vec<u8>, field: u64, value: u64) {
    put_varint(out, field << 3);
    put_varint(out, value);
}

/// Packed sint64, delta-coded as `DenseNodes` and way refs are.
fn packed_deltas(values: &[i64]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev = 0;
    for &v in values {
        put_varint(&mut out, zigzag(v - prev));
        prev = v;
    }
    out
}

/// `PrimitiveGroup` field numbers.
const G_NODE: u64 = 1;
const G_DENSE: u64 = 2;
const G_WAY: u64 = 3;

fn dense(ids: &[i64]) -> (u64, Vec<u8>) {
    let coords: Vec<i64> = ids.iter().map(|id| id * 1_000).collect();
    let mut body = Vec::new();
    if !ids.is_empty() {
        put_len(&mut body, 1, &packed_deltas(ids));
        put_len(&mut body, 8, &packed_deltas(&coords));
        put_len(&mut body, 9, &packed_deltas(&coords));
    }
    (G_DENSE, body)
}

fn plain_node(id: i64) -> (u64, Vec<u8>) {
    let mut body = Vec::new();
    put_uint(&mut body, 1, zigzag(id));
    put_uint(&mut body, 8, zigzag(id * 1_000));
    put_uint(&mut body, 9, zigzag(id * 1_000));
    (G_NODE, body)
}

fn way(id: i64, refs: &[i64]) -> (u64, Vec<u8>) {
    let mut body = Vec::new();
    put_uint(&mut body, 1, id.cast_unsigned());
    put_len(&mut body, 8, &packed_deltas(refs));
    (G_WAY, body)
}

/// One `PrimitiveGroup` holding these element fields, in this order.
fn group(fields: &[(u64, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (field, body) in fields {
        put_len(&mut out, *field, body);
    }
    out
}

/// A `PrimitiveBlock` with a one-entry string table and these groups.
fn block(groups: &[Vec<u8>]) -> Vec<u8> {
    let mut table = Vec::new();
    put_len(&mut table, 1, b"");
    let mut out = Vec::new();
    put_len(&mut out, 1, &table);
    for g in groups {
        put_len(&mut out, 2, g);
    }
    out
}

/// A sorted PBF whose data blobs are `blocks`, none carrying indexdata.
fn write_unindexed_blocks(path: &Path, blocks: &[Vec<u8>]) {
    use pbfhogg::writer::{Compression, PbfWriter};
    let file = File::create(path).unwrap();
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    let header = pbfhogg::block_builder::HeaderBuilder::new()
        .sorted()
        .build()
        .unwrap();
    writer.write_header(&header).unwrap();
    for b in blocks {
        writer.write_primitive_block_no_indexdata(b).unwrap();
    }
    writer.flush().unwrap();
}

/// Every shape a forced run meets: an indexable block the diff rewrites, one
/// it leaves alone (reframed), a node block whose one group carries both
/// `DenseNodes` and a plain node (one kind, no single index: re-encoded, with
/// a create interleaved), an empty block (omitted), and a way block whose
/// group opens with an empty `DenseNodes` (a way blob that `block_type()`
/// would call nodes).
#[test]
fn forced_run_over_an_unindexed_base_writes_a_wholly_indexed_file() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_unindexed_blocks(
        &base,
        &[
            block(&[group(&[dense(&[1, 2, 3, 4])])]),
            block(&[group(&[dense(&[5, 7]), plain_node(9)])]),
            block(&[]),
            block(&[group(&[way(100, &[1, 2])])]),
            block(&[group(&[dense(&[]), way(101, &[3, 4])])]),
            block(&[group(&[way(102, &[5, 7])])]),
        ],
    );
    crate::common::assert_non_indexed(&base);

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="2" lat="2.0" lon="2.0" version="2"/>
    <way id="102" version="2"><nd ref="7"/><nd ref="9"/></way>
  </modify>
  <create>
    <node id="8" lat="8.0" lon="8.0" version="1"/>
    <node id="10" lat="10.0" lon="10.0" version="1"/>
    <way id="103" version="1"><nd ref="1"/><nd ref="9"/></way>
  </create>
</osmChange>"#,
    );

    run_apply_simple(&base, &osc, &output);

    crate::common::assert_every_data_blob_indexed(&output);
    let check = CliInvoker::new()
        .arg("check")
        .arg(&output)
        .arg("--index")
        .run();
    assert_eq!(
        check.status.code(),
        Some(0),
        "check --index must accept the forced output; stdout:\n{}\nstderr:\n{}",
        check.stdout_str(),
        check.stderr_str()
    );
    crate::common::assert_sorted_file(&output);

    let c = read_all_elements(&output);
    assert_eq!(
        c.nodes.iter().map(|n| n.0).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 7, 8, 9, 10]
    );
    assert_eq!(
        c.ways.iter().map(|w| w.0).collect::<Vec<_>>(),
        vec![100, 101, 102, 103]
    );
}

/// A blob whose elements span two kinds cannot be placed in type order by a
/// drain that places each blob by one kind, so a forced run refuses it,
/// even after earlier blobs have been written, and publishes nothing.
#[test]
fn forced_run_refuses_a_blob_that_mixes_kinds_and_publishes_nothing() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_unindexed_blocks(
        &base,
        &[
            block(&[group(&[dense(&[1, 2])])]),
            block(&[group(&[dense(&[3])]), group(&[way(100, &[1, 2])])]),
            block(&[group(&[way(101, &[2, 3])])]),
        ],
    );
    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="99" lat="9.0" lon="9.0" version="1"/>
  </create>
</osmChange>"#,
    );

    let refused = |extra: &[&str]| {
        let out = CliInvoker::new()
            .arg("apply-changes")
            .arg(&base)
            .arg(&osc)
            .arg("-o")
            .arg(&output)
            .arg("--force")
            .args(extra.iter().copied())
            .run();
        let stderr = out.stderr_str();
        assert_eq!(out.status.code(), Some(2), "stderr:\n{stderr}");
        assert!(
            stderr.contains("mixes node and way elements"),
            "stderr:\n{stderr}"
        );
        assert!(
            stderr.contains("each blob holds one kind of element"),
            "stderr:\n{stderr}"
        );
    };

    refused(&[]);
    assert!(!output.exists(), "a refused run published an output");

    std::fs::write(&output, b"keep").unwrap();
    refused(&["--overwrite"]);
    assert_eq!(std::fs::read(&output).unwrap(), b"keep");
}

// ---------------------------------------------------------------------------
// jobs parity: output is independent of worker count.
// ---------------------------------------------------------------------------

#[test]
fn merge_jobs_parity_on_multiblob_input() {
    let dir = crate::common::test_scratch();
    let base_raw = dir.path().join("base_raw.osm.pbf");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let out_seq = dir.path().join("out_seq.osm.pbf");
    let out_par = dir.path().join("out_par.osm.pbf");

    write_merge_jobs_fixture(&base_raw, &osc);
    bootstrap_low_base(&base_raw, &base);

    // Parity baseline is jobs=2; jobs=1 is rejected up front.
    let seq = run_apply_changes(&base, &osc, &out_seq, Some(2), true);
    let par = run_apply_changes(&base, &osc, &out_par, Some(4), true);

    // Stats parity: every counter that's part of the standard summary
    // block must match between worker counts.
    assert_eq!(
        stats_block(&seq.stderr_str()),
        stats_block(&par.stderr_str()),
        "stats summary diverges between -j 2 and -j 4",
    );

    assert_elements_equivalent(&out_seq, &out_par);
}

#[test]
fn merge_jobs_parity_without_locations_on_ways() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let out_seq = dir.path().join("out_seq.osm.pbf");
    let out_par = dir.path().join("out_par.osm.pbf");

    write_merge_jobs_fixture(&base, &osc);

    let seq = run_apply_changes(&base, &osc, &out_seq, Some(2), false);
    let par = run_apply_changes(&base, &osc, &out_par, Some(4), false);

    assert_eq!(
        stats_block(&seq.stderr_str()),
        stats_block(&par.stderr_str()),
        "stats summary diverges between -j 2 and -j 4",
    );

    assert_elements_equivalent(&out_seq, &out_par);
}

// ---------------------------------------------------------------------------
// Empty base PBF: last_type stays None; trailing creates flush all three.
// ---------------------------------------------------------------------------

#[test]
fn empty_base_pbf_flushes_all_three_kinds() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(&base, &[], &[], &[]);

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="1" lat="10.0" lon="10.0" version="1"/>
    <node id="2" lat="20.0" lon="20.0" version="1"/>
    <way id="100" version="1">
      <nd ref="1"/>
      <nd ref="2"/>
    </way>
    <relation id="1000" version="1">
      <member type="way" ref="100" role="outer"/>
    </relation>
  </create>
</osmChange>"#,
    );

    run_apply_simple(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(c.nodes.iter().map(|n| n.0).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(c.ways.iter().map(|w| w.0).collect::<Vec<_>>(), vec![100]);
    assert_eq!(
        c.relations.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![1000]
    );
}

#[test]
fn empty_base_pbf_noop_on_empty_diff() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(&base, &[], &[], &[]);
    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
</osmChange>"#,
    );

    run_apply_simple(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert!(c.nodes.is_empty());
    assert!(c.ways.is_empty());
    assert!(c.relations.is_empty());
}

// ---------------------------------------------------------------------------
// Trailing creates interleave correctly across kinds.
// ---------------------------------------------------------------------------

#[test]
fn trailing_creates_after_node_blob_flush_way_and_relation() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 100_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="2" lat="20.0" lon="20.0" version="1"/>
    <way id="100" version="1">
      <nd ref="1"/>
      <nd ref="2"/>
    </way>
    <relation id="1000" version="1">
      <member type="way" ref="100" role="outer"/>
    </relation>
  </create>
</osmChange>"#,
    );

    run_apply_simple(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(c.nodes.iter().map(|n| n.0).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(c.ways.iter().map(|w| w.0).collect::<Vec<_>>(), vec![100]);
    assert_eq!(
        c.relations.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![1000]
    );
}

#[test]
fn trailing_creates_after_way_blob_flush_relation_only() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 100_000_000,
            tags: vec![],
            meta: None,
        }],
        &[TestWay {
            id: 10,
            refs: vec![1],
            tags: vec![],
            meta: None,
        }],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <relation id="500" version="1">
      <member type="way" ref="10" role="outer"/>
    </relation>
  </create>
</osmChange>"#,
    );

    run_apply_simple(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(c.nodes.iter().map(|n| n.0).collect::<Vec<_>>(), vec![1]);
    assert_eq!(c.ways.iter().map(|w| w.0).collect::<Vec<_>>(), vec![10]);
    assert_eq!(
        c.relations.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![500]
    );
}

// ---------------------------------------------------------------------------
// Permissive missing-element semantics (reference/osmium-parity.md):
// modify on absent ID silently inserts; delete on absent ID is a no-op;
// create on existing ID silently overwrites.
// ---------------------------------------------------------------------------

#[test]
fn modify_on_missing_id_silently_inserts() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 100_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="42" lat="55.0" lon="12.0" version="3"/>
  </modify>
</osmChange>"#,
    );

    run_apply_simple(&base, &osc, &output);
    let c = read_all_elements(&output);
    let node_ids: Vec<i64> = c.nodes.iter().map(|n| n.0).collect();
    assert_eq!(
        node_ids,
        vec![1, 42],
        "modify on absent ID is treated as an insert (reference/osmium-parity.md)",
    );
}

#[test]
fn delete_on_missing_id_is_noop() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 100_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <delete>
    <node id="42" version="1"/>
  </delete>
</osmChange>"#,
    );

    run_apply_simple(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(c.nodes.iter().map(|n| n.0).collect::<Vec<_>>(), vec![1]);
    assert!(c.ways.is_empty());
    assert!(c.relations.is_empty());
}

#[test]
fn create_on_existing_id_overwrites_base() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[TestNode {
            id: 42,
            lat: 100_000_000,
            lon: 100_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );

    write_osc(
        &osc,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="42" lat="20.0" lon="20.0" version="2"/>
  </create>
</osmChange>"#,
    );

    run_apply_simple(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(c.nodes.len(), 1, "create on existing ID must not duplicate");
    let n = &c.nodes[0];
    assert_eq!(n.0, 42);
    // OSC record wins: lat=20.0deg = 200_000_000 decimicrodegrees.
    assert_eq!((n.1, n.2), (200_000_000, 200_000_000));
}
