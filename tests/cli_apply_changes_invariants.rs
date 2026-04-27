//! CLI-driven invariant tests for `pbfhogg apply-changes`.
//!
//! Replaces `tests/apply_changes_invariants.rs` (split out of
//! priority 1; sibling to `cli_apply_changes.rs`). Pins specific
//! correctness invariants that the descriptor-first streaming
//! rewrite documented in `notes/apply-changes-opportunities.md`
//! must preserve byte-for-byte. These tests MUST pass on current
//! main before the rewrite starts and must still pass after.
//!
//! Each test pins down a specific invariant from the plan doc's
//! "Correctness invariants" section:
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

mod common;

use std::fs::File;
use std::io::Write;
use std::path::Path;

use common::cli::{CliInvoker, CliOutput};
use common::{
    assert_elements_equivalent, generate_nodes, generate_ways,
    read_all_elements_with_coords as read_all_elements, write_multi_block_test_pbf,
    write_test_pbf_sorted, TestNode, TestWay,
};
use flate2::write::GzEncoder;
use tempfile::TempDir;

fn write_osc(path: &Path, xml: &str) {
    let file = File::create(path).expect("create osc file");
    let mut enc = GzEncoder::new(file, flate2::Compression::fast());
    enc.write_all(xml.as_bytes()).expect("write xml");
    enc.finish().expect("finish gz");
}

fn run_apply_changes(base: &Path, osc: &Path, output: &Path, jobs: Option<usize>, locations_on_ways: bool) -> CliOutput {
    let mut cli = CliInvoker::new()
        .arg("apply-changes")
        .arg(base)
        .arg(osc)
        .arg("-o")
        .arg(output);
    if locations_on_ways {
        cli = cli.arg("--locations-on-ways");
    }
    if let Some(j) = jobs {
        cli = cli.arg("-j").arg(j.to_string());
    }
    cli.arg("--force").assert_success()
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
// Cursor rule: Passthrough / FalsePositive slots must NOT advance the cursor.
// ---------------------------------------------------------------------------

#[test]
fn cursor_rule_false_positive_blob_emits_create_after() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 100_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 200_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 10, lat: 300_000_000, lon: 300_000_000, tags: vec![], meta: None },
        ],
        &[TestWay { id: 100, refs: vec![1, 2, 10], tags: vec![], meta: None }],
        &[],
    );

    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="5" lat="40.0" lon="40.0" version="1"/>
  </create>
</osmChange>"#);

    run_apply_simple(&base, &osc, &output);
    let c = read_all_elements(&output);

    let node_ids: Vec<i64> = c.nodes.iter().map(|n| n.0).collect();
    assert!(
        node_ids.contains(&5),
        "FalsePositive cursor rule: id=5 must be present. Got {node_ids:?}",
    );
    assert_eq!(
        node_ids,
        vec![1, 2, 10, 5],
        "FalsePositive blob is passed through verbatim, then id=5 emitted \
         on the type transition Node -> Way (blob-tail order, not OSM-sorted)."
    );

    assert_eq!(c.ways.iter().map(|w| w.0).collect::<Vec<_>>(), vec![100]);
}

#[test]
fn cursor_rule_false_positive_blob_emits_create_at_tail() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 100_000_000, tags: vec![], meta: None },
            TestNode { id: 10, lat: 300_000_000, lon: 300_000_000, tags: vec![], meta: None },
        ],
        &[],
        &[],
    );

    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="5" lat="40.0" lon="40.0" version="1"/>
  </create>
</osmChange>"#);

    run_apply_simple(&base, &osc, &output);
    let c = read_all_elements(&output);

    let node_ids: Vec<i64> = c.nodes.iter().map(|n| n.0).collect();
    assert!(node_ids.contains(&5), "id=5 must be present (got {node_ids:?})");
    assert_eq!(
        node_ids,
        vec![1, 10, 5],
        "Trailing-create after FalsePositive blob: blob-tail order [1, 10, 5]"
    );
}

// ---------------------------------------------------------------------------
// jobs parity: output is independent of worker count.
// ---------------------------------------------------------------------------

#[test]
fn merge_jobs_parity_on_multiblob_input() {
    let dir = TempDir::new().expect("tempdir");
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
    let dir = TempDir::new().expect("tempdir");
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
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(&base, &[], &[], &[]);

    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
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
</osmChange>"#);

    run_apply_simple(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(c.nodes.iter().map(|n| n.0).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(c.ways.iter().map(|w| w.0).collect::<Vec<_>>(), vec![100]);
    assert_eq!(c.relations.iter().map(|r| r.0).collect::<Vec<_>>(), vec![1000]);
}

#[test]
fn empty_base_pbf_noop_on_empty_diff() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(&base, &[], &[], &[]);
    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
</osmChange>"#);

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
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[TestNode { id: 1, lat: 100_000_000, lon: 100_000_000, tags: vec![], meta: None }],
        &[],
        &[],
    );

    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
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
</osmChange>"#);

    run_apply_simple(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(c.nodes.iter().map(|n| n.0).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(c.ways.iter().map(|w| w.0).collect::<Vec<_>>(), vec![100]);
    assert_eq!(c.relations.iter().map(|r| r.0).collect::<Vec<_>>(), vec![1000]);
}

#[test]
fn trailing_creates_after_way_blob_flush_relation_only() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[TestNode { id: 1, lat: 100_000_000, lon: 100_000_000, tags: vec![], meta: None }],
        &[TestWay { id: 10, refs: vec![1], tags: vec![], meta: None }],
        &[],
    );

    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <relation id="500" version="1">
      <member type="way" ref="10" role="outer"/>
    </relation>
  </create>
</osmChange>"#);

    run_apply_simple(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(c.nodes.iter().map(|n| n.0).collect::<Vec<_>>(), vec![1]);
    assert_eq!(c.ways.iter().map(|w| w.0).collect::<Vec<_>>(), vec![10]);
    assert_eq!(c.relations.iter().map(|r| r.0).collect::<Vec<_>>(), vec![500]);
}

// ---------------------------------------------------------------------------
// Permissive missing-element semantics (reference/osmium-parity.md):
// modify on absent ID silently inserts; delete on absent ID is a no-op;
// create on existing ID silently overwrites.
// ---------------------------------------------------------------------------

#[test]
fn modify_on_missing_id_silently_inserts() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[TestNode { id: 1, lat: 100_000_000, lon: 100_000_000, tags: vec![], meta: None }],
        &[],
        &[],
    );

    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="42" lat="55.0" lon="12.0" version="3"/>
  </modify>
</osmChange>"#);

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
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[TestNode { id: 1, lat: 100_000_000, lon: 100_000_000, tags: vec![], meta: None }],
        &[],
        &[],
    );

    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <delete>
    <node id="42" version="1"/>
  </delete>
</osmChange>"#);

    run_apply_simple(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(c.nodes.iter().map(|n| n.0).collect::<Vec<_>>(), vec![1]);
    assert!(c.ways.is_empty());
    assert!(c.relations.is_empty());
}

#[test]
fn create_on_existing_id_overwrites_base() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &base,
        &[TestNode { id: 42, lat: 100_000_000, lon: 100_000_000, tags: vec![], meta: None }],
        &[],
        &[],
    );

    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="42" lat="20.0" lon="20.0" version="2"/>
  </create>
</osmChange>"#);

    run_apply_simple(&base, &osc, &output);
    let c = read_all_elements(&output);

    assert_eq!(c.nodes.len(), 1, "create on existing ID must not duplicate");
    let n = &c.nodes[0];
    assert_eq!(n.0, 42);
    // OSC record wins: lat=20.0deg = 200_000_000 decimicrodegrees.
    assert_eq!((n.1, n.2), (200_000_000, 200_000_000));
}
