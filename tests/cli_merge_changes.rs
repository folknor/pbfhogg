//! CLI-driven integration tests for `pbfhogg merge-changes`.
//!
//! Replaces the library-API `tests/merge_changes.rs`. OSC inputs
//! are written via `flate2::write::GzEncoder` (or plain file
//! writes for `.osc`); the `merge-changes` command runs via the
//! compiled `pbfhogg` binary through `CliInvoker`; output is
//! verified by reading the resulting OSC text and substring
//! matching, with stats inspected through stderr (the CLI emits
//! them via `MergeChangesStats::print_summary`). No imports
//! from `pbfhogg::merge_changes::*` - a rewrite of
//! `src/commands/merge_changes/` cannot break these tests by
//! type changes alone.

#![allow(clippy::unwrap_used)]

mod common;

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use common::cli::CliInvoker;
use flate2::Compression;
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use tempfile::TempDir;

fn write_osc_gz(path: &Path, xml: &str) {
    let file = File::create(path).expect("create osc file");
    let mut enc = GzEncoder::new(file, Compression::fast());
    enc.write_all(xml.as_bytes()).expect("write xml");
    enc.finish().expect("finish gz");
}

fn write_osc_plain(path: &Path, xml: &str) {
    let mut file = File::create(path).expect("create osc file");
    file.write_all(xml.as_bytes()).expect("write xml");
}

fn read_osc_gz(path: &Path) -> String {
    let file = File::open(path).expect("open osc file");
    let mut dec = MultiGzDecoder::new(file);
    let mut xml = String::new();
    dec.read_to_string(&mut xml).expect("read osc");
    xml
}

fn read_osc_plain(path: &Path) -> String {
    let mut file = File::open(path).expect("open osc file");
    let mut xml = String::new();
    file.read_to_string(&mut xml).expect("read osc");
    xml
}

/// Invoke `pbfhogg merge-changes <inputs...> -o <output> [--simplify]`
/// and assert success. Returns captured stderr (which carries the
/// `MergeChangesStats::print_summary` line) so the caller can pin
/// counter values.
fn run_merge_changes(inputs: &[&Path], output: &Path, simplify: bool) -> String {
    let mut cli = CliInvoker::new().arg("merge-changes");
    for input in inputs {
        cli = cli.arg(*input);
    }
    cli = cli.arg("-o").arg(output);
    if simplify {
        cli = cli.arg("--simplify");
    }
    cli.assert_success().stderr_str()
}

#[test]
fn merge_changes_keeps_full_stream_by_default() {
    let dir = TempDir::new().expect("tempdir");
    let in1 = dir.path().join("001.osc.gz");
    let in2 = dir.path().join("002.osc.gz");
    let out = dir.path().join("out.osc.gz");

    write_osc_gz(
        &in1,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="1" lat="1.0" lon="2.0" version="1" timestamp="2025-01-15T10:30:00Z" changeset="12345" uid="100" user="testuser">
      <tag k="name" v="first"/>
    </node>
  </create>
</osmChange>"#,
    );

    write_osc_gz(
        &in2,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="1" lat="1.1" lon="2.1" version="2" timestamp="2025-01-16T12:00:00Z" changeset="12346" uid="200" user="otheruser">
      <tag k="name" v="second"/>
    </node>
  </modify>
</osmChange>"#,
    );

    let stderr = run_merge_changes(&[&in1, &in2], &out, false);
    // Non-simplify summary: "Merged {files} files: {changes_out} changes"
    assert!(
        stderr.contains("Merged 2 files: 2 changes"),
        "stats line missing or unexpected; stderr =\n{stderr}",
    );
    assert!(
        !stderr.contains("simplified"),
        "non-simplify run must not emit '(simplified)'",
    );

    let xml = read_osc_gz(&out);
    assert!(xml.contains("<create>"));
    assert!(xml.contains("<modify>"));
    assert!(xml.contains(r#"version="1""#));
    assert!(xml.contains(r#"version="2""#));
    assert!(xml.contains(r#"v="first""#));
    assert!(xml.contains(r#"v="second""#));
    // Metadata attributes are preserved
    assert!(xml.contains(r#"timestamp="2025-01-15T10:30:00Z""#));
    assert!(xml.contains(r#"changeset="12345""#));
    assert!(xml.contains(r#"uid="100""#));
    assert!(xml.contains(r#"user="testuser""#));
    assert!(xml.contains(r#"timestamp="2025-01-16T12:00:00Z""#));
    assert!(xml.contains(r#"user="otheruser""#));
}

#[test]
fn simplify_keeps_only_last_change_per_object() {
    let dir = TempDir::new().expect("tempdir");
    let in1 = dir.path().join("001.osc.gz");
    let in2 = dir.path().join("002.osc.gz");
    let out = dir.path().join("out.osc.gz");

    write_osc_gz(
        &in1,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="1" lat="1.0" lon="2.0" version="1"/>
    <way id="10" version="1">
      <nd ref="1"/>
    </way>
  </create>
</osmChange>"#,
    );

    write_osc_gz(
        &in2,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <delete>
    <node id="1" version="2"/>
  </delete>
  <modify>
    <way id="10" version="2">
      <nd ref="1"/>
      <tag k="highway" v="residential"/>
    </way>
  </modify>
</osmChange>"#,
    );

    let stderr = run_merge_changes(&[&in1, &in2], &out, true);
    // Simplify summary:
    //   "Merged {files} files: {changes_in} input changes -> {changes_out} output changes (simplified)"
    assert!(
        stderr.contains("Merged 2 files: 4 input changes -> 2 output changes (simplified)"),
        "stats line missing or unexpected; stderr =\n{stderr}",
    );

    let xml = read_osc_gz(&out);
    assert!(xml.contains("<delete>"));
    assert!(xml.contains(r#"<node id="1" version="2"/>"#));
    assert!(xml.contains("<modify>"));
    assert!(xml.contains(r#"<way id="10" version="2">"#));

    assert!(!xml.contains(r#"<node id="1" lat="1""#));
    assert!(!xml.contains(r#"<way id="10" version="1""#));
}

#[test]
fn plain_osc_input_supported() {
    let dir = TempDir::new().expect("tempdir");
    let in1 = dir.path().join("001.osc");
    let out = dir.path().join("out.osc");

    write_osc_plain(
        &in1,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="1" lat="1.0" lon="2.0" version="1"/>
  </create>
</osmChange>"#,
    );

    let stderr = run_merge_changes(&[&in1], &out, false);
    assert!(stderr.contains("Merged 1 files: 1 changes"));

    let xml = read_osc_plain(&out);
    assert!(xml.contains(r#"id="1""#));
}

#[test]
fn relation_roundtrip() {
    let dir = TempDir::new().expect("tempdir");
    let in1 = dir.path().join("001.osc.gz");
    let out = dir.path().join("out.osc.gz");

    write_osc_gz(
        &in1,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <relation id="100" version="1">
      <member type="way" ref="10" role="outer"/>
      <member type="way" ref="11" role="inner"/>
      <tag k="type" v="multipolygon"/>
    </relation>
  </create>
</osmChange>"#,
    );

    let stderr = run_merge_changes(&[&in1], &out, false);
    assert!(stderr.contains("Merged 1 files: 1 changes"));

    let xml = read_osc_gz(&out);
    assert!(xml.contains(r#"<relation id="100" version="1">"#));
    assert!(xml.contains(r#"type="way""#));
    assert!(xml.contains(r#"ref="10""#));
    assert!(xml.contains(r#"role="outer""#));
    assert!(xml.contains(r#"role="inner""#));
    assert!(xml.contains(r#"v="multipolygon""#));
}

#[test]
fn simplify_multiple_same_type_different_ids() {
    let dir = TempDir::new().expect("tempdir");
    let in1 = dir.path().join("001.osc.gz");
    let out = dir.path().join("out.osc.gz");

    write_osc_gz(
        &in1,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="1" lat="1.0" lon="2.0" version="1"/>
    <node id="2" lat="3.0" lon="4.0" version="1"/>
  </create>
  <modify>
    <node id="1" lat="1.1" lon="2.1" version="2"/>
  </modify>
</osmChange>"#,
    );

    let stderr = run_merge_changes(&[&in1], &out, true);
    assert!(
        stderr.contains("Merged 1 files: 3 input changes -> 2 output changes (simplified)"),
        "stats line missing or unexpected; stderr =\n{stderr}",
    );

    let xml = read_osc_gz(&out);
    // node 1 simplified to modify (last action)
    assert!(xml.contains(r#"<node id="1" lat="1.1" lon="2.1" version="2""#));
    // node 2 stays as create
    assert!(xml.contains(r#"<node id="2" lat="3" lon="4" version="1""#));
}

#[test]
fn simplify_preserves_metadata() {
    let dir = TempDir::new().expect("tempdir");
    let in1 = dir.path().join("001.osc.gz");
    let out = dir.path().join("out.osc.gz");

    write_osc_gz(
        &in1,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="1" lat="1.0" lon="2.0" version="1" timestamp="2025-01-01T00:00:00Z" uid="42" user="alice"/>
  </create>
  <modify>
    <node id="1" lat="1.1" lon="2.1" version="2" timestamp="2025-06-15T12:00:00Z" uid="99" user="bob"/>
  </modify>
</osmChange>"#,
    );

    let stderr = run_merge_changes(&[&in1], &out, true);
    assert!(
        stderr.contains("-> 1 output changes (simplified)"),
        "stats line missing expected output count; stderr =\n{stderr}",
    );

    let xml = read_osc_gz(&out);
    // Should have the last version's metadata
    assert!(xml.contains(r#"version="2""#));
    assert!(xml.contains(r#"timestamp="2025-06-15T12:00:00Z""#));
    assert!(xml.contains(r#"user="bob""#));
    // Old metadata should be gone
    assert!(!xml.contains(r#"user="alice""#));
}

#[test]
fn empty_osc_produces_empty_output() {
    let dir = TempDir::new().expect("tempdir");
    let in1 = dir.path().join("001.osc.gz");
    let out = dir.path().join("out.osc.gz");

    write_osc_gz(
        &in1,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
</osmChange>"#,
    );

    let stderr = run_merge_changes(&[&in1], &out, false);
    assert!(stderr.contains("Merged 1 files: 0 changes"));

    let xml = read_osc_gz(&out);
    assert!(xml.contains("osmChange"));
    assert!(!xml.contains("<create>"));
    assert!(!xml.contains("<modify>"));
    assert!(!xml.contains("<delete>"));
}

#[test]
fn simplify_create_then_delete_yields_only_delete() {
    let dir = TempDir::new().expect("tempdir");
    let in1 = dir.path().join("001.osc.gz");
    let in2 = dir.path().join("002.osc.gz");
    let out = dir.path().join("out.osc.gz");

    // First OSC: create a node
    write_osc_gz(
        &in1,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="42" lat="1.0" lon="2.0" version="1"/>
  </create>
</osmChange>"#,
    );

    // Second OSC: delete that same node
    write_osc_gz(
        &in2,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <delete>
    <node id="42" version="2"/>
  </delete>
</osmChange>"#,
    );

    let stderr = run_merge_changes(&[&in1, &in2], &out, true);
    // Simplified output: only the delete (last action wins).
    assert!(
        stderr.contains("Merged 2 files: 2 input changes -> 1 output changes (simplified)"),
        "stats line missing or unexpected; stderr =\n{stderr}",
    );

    let xml = read_osc_gz(&out);
    // Should contain the delete
    assert!(
        xml.contains("<delete>"),
        "output should contain a delete section"
    );
    assert!(
        xml.contains(r#"id="42""#),
        "output should contain node id=42"
    );
    assert!(
        xml.contains(r#"version="2""#),
        "output should have version 2 from the delete"
    );
    // Should NOT contain a create
    assert!(
        !xml.contains("<create>"),
        "simplified output should not contain create for a deleted element"
    );
}
