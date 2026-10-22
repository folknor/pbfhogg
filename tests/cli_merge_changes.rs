//! CLI-driven integration tests for `pbfhogg merge-changes`.
//!
//! Replaces the removed library-API `merge_changes.rs` test. OSC inputs
//! are written via `flate2::write::GzEncoder` (or plain file
//! writes for `.osc`); the `merge-changes` command runs via the
//! compiled `pbfhogg` binary through `CliInvoker`; output is
//! verified by reading the resulting OSC text and substring
//! matching, with stats inspected through stderr (the CLI emits
//! them via `MergeChangesStats::write_summary`). No imports
//! from `pbfhogg::merge_changes::*` - a rewrite of
//! `src/commands/merge_changes/` cannot break these tests by
//! type changes alone.

#![allow(clippy::unwrap_used)]

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use crate::common::cli::CliInvoker;
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
/// `MergeChangesStats::write_summary` line) so the caller can pin
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

// ---------------------------------------------------------------------------
// One parser, one leniency
// ---------------------------------------------------------------------------

/// Run `merge-changes` expecting failure; returns stderr.
fn merge_changes_failure(inputs: &[&Path], output: &Path, extra: &[&str]) -> String {
    let mut cli = CliInvoker::new().arg("merge-changes");
    for input in inputs {
        cli = cli.arg(*input);
    }
    cli = cli.arg("-o").arg(output);
    for a in extra {
        cli = cli.arg(*a);
    }
    cli.assert_failure().stderr_str()
}

/// `merge-changes` must not throw away metadata just because `version`
/// is absent.
///
/// `parse_metadata` used to open with
/// `let version = parse_i32_attr_optional(e, b"version")?;` and return
/// `None` for any element without one - dropping timestamp, changeset,
/// uid, user and visible along with it. `apply-changes`, reading the
/// very same file through the other parser, kept all of them. That is
/// the finding: one input, two answers.
///
/// Asserted on the emitted OSC bytes, not by re-parsing through our own
/// parser: the parser is the thing under test, so round-tripping would
/// let a matched pair of bugs cancel out. The output must carry the
/// attributes the input carried and must NOT invent `version="0"` for
/// an element whose producer never claimed a version.
#[test]
fn metadata_survives_an_absent_version() {
    let dir = TempDir::new().expect("tempdir");
    let in1 = dir.path().join("001.osc.gz");
    let out = dir.path().join("out.osc");

    write_osc_gz(
        &in1,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="1" lat="1.0" lon="2.0" timestamp="2025-06-15T12:00:00Z" uid="99" user="bob"/>
  </modify>
</osmChange>"#,
    );

    run_merge_changes(&[&in1], &out, false);
    let xml = read_osc_plain(&out);

    assert!(
        xml.contains(r#"timestamp="2025-06-15T12:00:00Z""#),
        "timestamp must survive an absent version; output was:\n{xml}"
    );
    assert!(
        xml.contains(r#"uid="99""#) && xml.contains(r#"user="bob""#),
        "uid and user must survive an absent version; output was:\n{xml}"
    );
    assert!(
        !xml.contains("version=\"0\""),
        "an absent version must not be emitted as version=\"0\"; output was:\n{xml}"
    );
    // The `version` attribute must be absent entirely on the ELEMENT.
    // Checked on the element's own line, so the XML declaration's
    // `version="1.0"` and the osmChange root's `version="0.6"` - both
    // legitimately present - cannot satisfy this by accident.
    let node_line = xml
        .lines()
        .find(|l| l.contains("<node "))
        .unwrap_or_else(|| panic!("no node element in output:\n{xml}"));
    assert!(
        !node_line.contains("version="),
        "an absent version must not be emitted at all; node line was:\n{node_line}"
    );
}

/// A created or modified `<node>` without usable coordinates is a hard
/// error in `merge-changes` too.
///
/// This half of the parser used to do
/// `parse_f64_attr_optional(..).unwrap_or(0.0)` and a saturating
/// `as i32`, so a coordinate-less node was silently re-emitted at 0/0
/// (a real place, in the Gulf of Guinea) and `lat="1e30"` was clamped
/// to `i32::MAX`. The overlay parser was fixed for this; this one was
/// not, because it was a second copy.
#[test]
fn merge_changes_refuses_a_node_without_usable_coords() {
    let dir = TempDir::new().expect("tempdir");
    let out = dir.path().join("out.osc.gz");

    for (name, attrs) in [
        ("missing", r#"id="1" version="1""#),
        ("unparsable", r#"id="1" lat="abc" lon="2.0" version="1""#),
        (
            "out_of_range",
            r#"id="1" lat="100.0" lon="2.0" version="1""#,
        ),
    ] {
        let input = dir.path().join(format!("{name}.osc.gz"));
        write_osc_gz(
            &input,
            &format!(
                "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <modify>\n    \
                 <node {attrs}/>\n  </modify>\n</osmChange>"
            ),
        );
        let stderr = merge_changes_failure(&[&input], &out, &[]);
        assert!(
            stderr.contains("OSC node 1") && stderr.contains("lat"),
            "{name}: error must name the node and the attribute; stderr:\n{stderr}"
        );
    }
}

/// A `<delete>` node carries no coordinates by design and must keep
/// working - the counterpart that stops the test above from being
/// satisfied by refusing every node.
#[test]
fn merge_changes_delete_needs_no_coords() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("001.osc.gz");
    let out = dir.path().join("out.osc");
    write_osc_gz(
        &input,
        r#"<?xml version="1.0"?>
<osmChange version="0.6">
  <delete>
    <node id="9" version="3"/>
  </delete>
</osmChange>"#,
    );
    run_merge_changes(&[&input], &out, false);
    let xml = read_osc_plain(&out);
    assert!(xml.contains("<delete>") && xml.contains(r#"id="9""#));
}

/// A truncated OSC - complete gzip member, no closing `</osmChange>` -
/// must be refused rather than merged as though it were whole.
#[test]
fn merge_changes_refuses_a_truncated_input() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("cut.osc.gz");
    let out = dir.path().join("out.osc.gz");
    write_osc_gz(
        &input,
        "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <create>\n    \
         <node id=\"1\" lat=\"1.0\" lon=\"2.0\" version=\"1\"/>\n  </create>\n",
    );
    let stderr = merge_changes_failure(&[&input], &out, &[]);
    assert!(
        stderr.contains("closing </osmChange>"),
        "error must name the missing closing tag; stderr:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// Version precedence
// ---------------------------------------------------------------------------

/// `--simplify` keeps the LAST change per object by input position and
/// never consulted `version`, so listing the inputs out of sequence
/// order silently regressed objects to older versions and reported
/// success. That is data corruption, and it is now refused.
#[test]
fn simplify_refuses_a_version_regression() {
    let dir = TempDir::new().expect("tempdir");
    let newer = dir.path().join("002.osc.gz");
    let older = dir.path().join("001.osc.gz");
    let out = dir.path().join("out.osc.gz");

    write_osc_gz(
        &newer,
        r#"<?xml version="1.0"?>
<osmChange version="0.6">
  <modify><node id="1" lat="9.0" lon="9.0" version="9"/></modify>
</osmChange>"#,
    );
    write_osc_gz(
        &older,
        r#"<?xml version="1.0"?>
<osmChange version="0.6">
  <modify><node id="1" lat="8.0" lon="8.0" version="8"/></modify>
</osmChange>"#,
    );

    // Inputs listed newest-first: the older change would win on
    // position and downgrade node 1 from version 9 to version 8.
    let stderr = merge_changes_failure(&[&newer, &older], &out, &["--simplify"]);
    assert!(
        stderr.contains("does not advance past version 9"),
        "error must name the regression; stderr:\n{stderr}"
    );

    // The same inputs in sequence order are fine.
    let stderr = run_merge_changes(&[&older, &newer], &out, true);
    assert!(stderr.contains("-> 1 output changes (simplified)"));
    let xml = read_osc_gz(&out);
    assert!(
        xml.contains(r#"version="9""#),
        "in-order simplify must keep the newer version; output was:\n{xml}"
    );

    // And the escape hatch really is an escape hatch.
    let out2 = dir.path().join("out2.osc.gz");
    let mut cli = CliInvoker::new().arg("merge-changes");
    for input in [&newer, &older] {
        cli = cli.arg(input);
    }
    cli.arg("-o")
        .arg(&out2)
        .arg("--simplify")
        .arg("--allow-version-regression")
        .assert_success();
    let xml = read_osc_gz(&out2);
    assert!(
        xml.contains(r#"version="8""#),
        "--allow-version-regression must restore the positional rule; output was:\n{xml}"
    );
}

/// Without `--simplify` no version is ever compared, so
/// `--allow-version-regression` used to be accepted and do nothing. It is
/// a usage error instead, refused before any input is read.
#[test]
fn allow_version_regression_requires_simplify() {
    let dir = TempDir::new().expect("tempdir");
    let out = dir.path().join("out.osc.gz");
    let result = CliInvoker::new()
        .arg("merge-changes")
        .arg(dir.path().join("missing.osc.gz"))
        .arg("-o")
        .arg(&out)
        .arg("--allow-version-regression")
        .run();
    assert_eq!(
        result.status.code(),
        Some(2),
        "expected a usage error; stderr:\n{}",
        result.stderr_str()
    );
    assert!(
        result.stderr_str().contains("--simplify"),
        "the usage error must name the missing flag; stderr:\n{}",
        result.stderr_str()
    );
    assert!(!out.exists(), "a usage error must not create the output");
}

/// The overlay parser's version gate had a hole at `<delete>`: a delete
/// dropped the live index entry, taking the stored version with it, so
/// the next stale change found nothing to be compared against and
/// resurrected an older version. `--simplify` keeps deletes in its
/// dedupe map as ordinary changes, so the same shape is gated here too -
/// this test pins that, because "one of the two places the gate was
/// added has a hole" is exactly the kind of asymmetry only a test
/// notices.
///
/// Both shapes: several actions inside one OSC, and one per file.
#[test]
fn simplify_gates_a_stale_change_after_a_delete() {
    let dir = TempDir::new().expect("tempdir");
    let out = dir.path().join("out.osc.gz");

    let one_file = dir.path().join("one.osc.gz");
    write_osc_gz(
        &one_file,
        r#"<?xml version="1.0"?>
<osmChange version="0.6">
  <delete><node id="1" version="9"/></delete>
  <modify><node id="1" lat="8.0" lon="8.0" version="8"/></modify>
</osmChange>"#,
    );
    let stderr = merge_changes_failure(&[&one_file], &out, &["--simplify"]);
    assert!(
        stderr.contains("does not advance past version 9"),
        "a stale modify after a delete in one OSC must be refused; stderr:\n{stderr}"
    );

    let del = dir.path().join("002.osc.gz");
    let stale = dir.path().join("001.osc.gz");
    write_osc_gz(
        &del,
        r#"<?xml version="1.0"?>
<osmChange version="0.6">
  <delete><node id="1" version="9"/></delete>
</osmChange>"#,
    );
    write_osc_gz(
        &stale,
        r#"<?xml version="1.0"?>
<osmChange version="0.6">
  <modify><node id="1" lat="8.0" lon="8.0" version="8"/></modify>
</osmChange>"#,
    );
    let stderr = merge_changes_failure(&[&del, &stale], &out, &["--simplify"]);
    assert!(
        stderr.contains("does not advance past version 9"),
        "a stale modify in a later file must be refused; stderr:\n{stderr}"
    );

    // The counterpart: an undelete that genuinely advances is accepted.
    let undelete = dir.path().join("003.osc.gz");
    write_osc_gz(
        &undelete,
        r#"<?xml version="1.0"?>
<osmChange version="0.6">
  <delete><node id="1" version="9"/></delete>
  <create><node id="1" lat="1.0" lon="1.0" version="10"/></create>
</osmChange>"#,
    );
    let stderr = run_merge_changes(&[&undelete], &out, true);
    assert!(stderr.contains("-> 1 output changes (simplified)"));
    let xml = read_osc_gz(&out);
    assert!(
        xml.contains(r#"version="10""#),
        "the advancing undelete must win; output was:\n{xml}"
    );
}

/// The floor is the highest version ever claimed, not the previous
/// change's version. Comparing against the previous change let an
/// unversioned change in between reset it, so a stale change after it
/// sailed through. Both shapes: the floor set by a delete (where
/// apply-changes already refused, so the two commands disagreed), and
/// the floor set by a live modify.
#[test]
fn simplify_floor_survives_an_unversioned_change() {
    let dir = TempDir::new().expect("tempdir");
    let out = dir.path().join("out.osc.gz");

    for (case, first) in [
        ("delete", r#"<delete><node id="1" version="9"/></delete>"#),
        (
            "modify",
            r#"<modify><node id="1" lat="9.0" lon="9.0" version="9"/></modify>"#,
        ),
    ] {
        let a = dir.path().join(format!("{case}-001.osc.gz"));
        let b = dir.path().join(format!("{case}-002.osc.gz"));
        let c = dir.path().join(format!("{case}-003.osc.gz"));
        write_osc_gz(
            &a,
            &format!(
                "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  {first}\n</osmChange>"
            ),
        );
        write_osc_gz(
            &b,
            r#"<?xml version="1.0"?>
<osmChange version="0.6">
  <modify><node id="1" lat="5.0" lon="5.0"/></modify>
</osmChange>"#,
        );
        write_osc_gz(
            &c,
            r#"<?xml version="1.0"?>
<osmChange version="0.6">
  <modify><node id="1" lat="8.0" lon="8.0" version="8"/></modify>
</osmChange>"#,
        );
        let stderr = merge_changes_failure(&[&a, &b, &c], &out, &["--simplify"]);
        assert!(
            stderr.contains("does not advance past version 9"),
            "{case}: the unversioned change must not reset the floor; stderr:\n{stderr}"
        );

        // The counterpart: a claim above the floor is still admitted
        // after the unversioned change, and wins.
        let d = dir.path().join(format!("{case}-004.osc.gz"));
        write_osc_gz(
            &d,
            r#"<?xml version="1.0"?>
<osmChange version="0.6">
  <modify><node id="1" lat="7.0" lon="7.0" version="10"/></modify>
</osmChange>"#,
        );
        let stderr = run_merge_changes(&[&a, &b, &d], &out, true);
        assert!(
            stderr.contains("-> 1 output changes (simplified)"),
            "{case}: stats line unexpected; stderr:\n{stderr}"
        );
        let xml = read_osc_gz(&out);
        assert!(
            xml.contains(r#"version="10""#),
            "{case}: the advancing change must win; output was:\n{xml}"
        );
    }
}

/// The counterpart: elements that claim no version at all are still
/// deduped positionally, because there is nothing to compare.
#[test]
fn simplify_still_dedupes_unversioned_elements() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("001.osc.gz");
    let out = dir.path().join("out.osc");
    write_osc_gz(
        &input,
        r#"<?xml version="1.0"?>
<osmChange version="0.6">
  <create><node id="1" lat="1.0" lon="1.0"/></create>
  <modify><node id="1" lat="2.0" lon="2.0"/></modify>
</osmChange>"#,
    );
    let stderr = run_merge_changes(&[&input], &out, true);
    assert!(
        stderr.contains("2 input changes -> 1 output changes"),
        "stats line unexpected; stderr:\n{stderr}"
    );
    let xml = read_osc_plain(&out);
    assert!(
        xml.contains(r#"lat="2""#) && !xml.contains(r#"lat="1""#),
        "the later unversioned change should win; output was:\n{xml}"
    );
}
