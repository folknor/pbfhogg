//! An error that reaches the operator names the file it is about.
//!
//! A bare "No such file or directory" or "File too large" is true and
//! useless on a command with several inputs and outputs. Each case here
//! forces one failure through the CLI and asserts the path the operator
//! gave appears on stderr: an open failure per input kind, a malformed
//! input, and a write failure per writer family (forced with
//! `RLIMIT_FSIZE`, so it works on any filesystem).

#![allow(clippy::unwrap_used)]

use std::io::Write;
use std::path::Path;

use crate::common::{generate_nodes, write_indexed_pbf};
use crate::invoker::{CliInvoker, CliOutput};

/// A sorted, indexed PBF of `count` nodes at scattered coordinates, so
/// that it compresses poorly and a few thousand nodes outgrow a write cap
/// even when copied through compressed.
fn write_base(path: &Path, count: usize) {
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move |range: u64| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        i32::try_from((state >> 33) % range).unwrap()
    };
    let nodes: Vec<_> = generate_nodes(count, 1)
        .into_iter()
        .map(|mut node| {
            node.lat = next(1_800_000_000) - 900_000_000;
            node.lon = next(2_000_000_000) - 1_000_000_000;
            node
        })
        .collect();
    write_indexed_pbf(path, &nodes, &[], &[]);
}

fn write_osc_gz(path: &Path, xml: &str) {
    let file = std::fs::File::create(path).unwrap();
    let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    enc.write_all(xml.as_bytes()).unwrap();
    enc.finish().unwrap();
}

/// An OSC creating `count` nodes from id 1.
fn creates(count: usize) -> String {
    let mut xml = String::from("<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n<create>\n");
    for id in 1..=count {
        xml.push_str(&format!(
            "<node id=\"{id}\" version=\"1\" lat=\"{}.{:04}\" lon=\"{}.{:04}\"/>\n",
            id % 80,
            id % 9973,
            id % 170,
            (id * 7) % 9973
        ));
    }
    xml.push_str("</create>\n</osmChange>\n");
    xml
}

/// The error report on stderr: from its `Error: ` prefix on. Progress lines
/// before it (`Parsing OSC diff: <path>`) name inputs too, and must not
/// satisfy an assertion about the error.
fn error_text(out: &CliOutput) -> String {
    let stderr = out.stderr_str();
    match stderr.find("Error: ") {
        Some(at) => stderr[at..].to_owned(),
        None => panic!("no error report on stderr:\n{stderr}"),
    }
}

/// The run failed and its error report names `path`.
fn assert_names(out: &CliOutput, path: &Path, what: &str) {
    assert!(
        !out.status.success(),
        "{what}: the run must fail; stderr:\n{}",
        out.stderr_str()
    );
    let shown = path.display().to_string();
    assert!(
        error_text(out).contains(&shown),
        "{what}: the error must name {shown}; stderr:\n{}",
        out.stderr_str()
    );
}

#[test]
fn a_missing_input_of_every_kind_is_named() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    write_base(&base, 10);
    let out_pbf = dir.path().join("out.osm.pbf");
    let missing = |name: &str| dir.path().join(format!("missing-{name}"));

    let pbf = missing("input.osm.pbf");
    let out = CliInvoker::new()
        .args([
            "cat".as_ref(),
            pbf.as_os_str(),
            "-o".as_ref(),
            out_pbf.as_os_str(),
        ])
        .run();
    assert_names(&out, &pbf, "PBF");

    let osc = missing("changes.osc.gz");
    let out = CliInvoker::new()
        .arg("apply-changes")
        .arg(&base)
        .arg(&osc)
        .arg("-o")
        .arg(&out_pbf)
        .run();
    assert_names(&out, &osc, "OSC");

    let ids = missing("ids.txt");
    let out = CliInvoker::new()
        .arg("getid")
        .arg(&base)
        .arg("--id-file")
        .arg(&ids)
        .arg("-o")
        .arg(&out_pbf)
        .run();
    assert_names(&out, &ids, "ID file");

    let polygon = missing("region.geojson");
    let out = CliInvoker::new()
        .arg("extract")
        .arg(&base)
        .arg("--polygon")
        .arg(&polygon)
        .arg("-o")
        .arg(&out_pbf)
        .run();
    assert_names(&out, &polygon, "GeoJSON");

    let config = missing("extracts.json");
    let out = CliInvoker::new()
        .arg("extract")
        .arg(&base)
        .arg("--config")
        .arg(&config)
        .run();
    assert_names(&out, &config, "extract config");

    let expressions = missing("expressions.txt");
    let out = CliInvoker::new()
        .arg("tags-filter")
        .arg(&base)
        .arg("-e")
        .arg(&expressions)
        .arg("-o")
        .arg(&out_pbf)
        .run();
    assert_names(&out, &expressions, "expressions file");

    assert!(!out_pbf.exists(), "no failed run may publish its output");
}

/// A truncated OSC (no closing tag) is refused naming the file, through
/// both the overlay parser (apply-changes) and the owned one
/// (merge-changes).
#[test]
fn a_malformed_osc_is_named() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    write_base(&base, 10);
    let osc = dir.path().join("truncated.osc.gz");
    write_osc_gz(
        &osc,
        "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n<create>\n\
         <node id=\"1\" version=\"1\" lat=\"1.0\" lon=\"1.0\"/>\n</create>\n",
    );

    let out = CliInvoker::new()
        .arg("apply-changes")
        .arg(&base)
        .arg(&osc)
        .arg("-o")
        .arg(dir.path().join("out.osm.pbf"))
        .run();
    assert_names(&out, &osc, "apply-changes");

    let out = CliInvoker::new()
        .arg("merge-changes")
        .arg(&osc)
        .arg("-o")
        .arg(dir.path().join("out.osc.gz"))
        .run();
    assert_names(&out, &osc, "merge-changes");
}

/// Writes in these tests are capped well above any output header and well
/// below any output body.
#[cfg(target_os = "linux")]
const WRITE_CAP: u64 = 4096;

/// A write failure names the output the operator gave (not a staging file),
/// fails the run, and publishes nothing.
#[cfg(target_os = "linux")]
fn assert_write_failure_named(out: &CliOutput, output: &Path, what: &str) {
    assert_names(out, output, what);
    assert!(
        error_text(out).contains("File too large"),
        "{what}: the cause must be the write that hit the cap; stderr:\n{}",
        out.stderr_str()
    );
    assert!(
        !output.exists(),
        "{what}: a failed write must publish nothing"
    );
}

/// The framing writer family: repack re-encodes every block through the
/// buffered writer, uncompressed so the output is large.
#[cfg(target_os = "linux")]
#[test]
fn a_failed_pbf_write_names_the_output() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    write_base(&base, 20_000);
    let output = dir.path().join("repacked.osm.pbf");
    let out = CliInvoker::new()
        .arg("repack")
        .arg(&base)
        .arg("-o")
        .arg(&output)
        .args(["--compression", "none"])
        .file_size_limit(WRITE_CAP)
        .run();
    assert_write_failure_named(&out, &output, "repack");
}

/// The passthrough path: apply-changes with an empty change set copies
/// every base blob into the output, with `copy_file_range` when built with
/// `linux-direct-io`. That failure cannot say which end failed, so it names
/// the input and the byte range and the writer names the output.
#[cfg(target_os = "linux")]
#[test]
fn a_failed_splice_names_the_output_and_the_input_range() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    write_base(&base, 20_000);
    let osc = dir.path().join("empty.osc.gz");
    write_osc_gz(
        &osc,
        "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\"></osmChange>\n",
    );
    let output = dir.path().join("applied.osm.pbf");
    let out = CliInvoker::new()
        .arg("apply-changes")
        .arg(&base)
        .arg(&osc)
        .arg("-o")
        .arg(&output)
        .file_size_limit(WRITE_CAP)
        .run();
    assert_write_failure_named(&out, &output, "apply-changes");
    // Without the feature the passthrough is a userspace copy, whose write
    // failure is the output's alone.
    #[cfg(feature = "linux-direct-io")]
    assert!(
        error_text(&out).contains(&format!("of {}", base.display())),
        "the splice failure must name the input it copied from; stderr:\n{}",
        out.stderr_str()
    );
}

/// The parallel positional writer: apply-changes rewrites every base blob
/// when the change set touches a node in each (blobs hold 8000 nodes).
#[cfg(target_os = "linux")]
#[test]
fn a_failed_parallel_write_names_the_output() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    write_base(&base, 20_000);
    let osc = dir.path().join("touch-every-blob.osc.gz");
    write_osc_gz(
        &osc,
        "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n<modify>\n\
         <node id=\"1\" version=\"2\" lat=\"1.0\" lon=\"1.0\"/>\n\
         <node id=\"8001\" version=\"2\" lat=\"2.0\" lon=\"2.0\"/>\n\
         <node id=\"16001\" version=\"2\" lat=\"3.0\" lon=\"3.0\"/>\n\
         </modify>\n</osmChange>\n",
    );
    let output = dir.path().join("rewritten.osm.pbf");
    let out = CliInvoker::new()
        .arg("apply-changes")
        .arg(&base)
        .arg(&osc)
        .arg("-o")
        .arg(&output)
        .file_size_limit(WRITE_CAP)
        .run();
    assert_write_failure_named(&out, &output, "apply-changes rewrite");
}

/// The text output writer: merge-changes streams one input straight into a
/// gzipped OSC, emitting while it parses. The failure is the output's and
/// must not be blamed on the input it was parsing.
#[cfg(target_os = "linux")]
#[test]
fn a_failed_osc_write_names_the_output_and_not_the_input() {
    let dir = crate::common::test_scratch();
    let osc = dir.path().join("changes.osc.gz");
    write_osc_gz(&osc, &creates(20_000));
    let output = dir.path().join("merged.osc.gz");
    let out = CliInvoker::new()
        .arg("merge-changes")
        .arg(&osc)
        .arg("-o")
        .arg(&output)
        .file_size_limit(WRITE_CAP)
        .run();
    assert_write_failure_named(&out, &output, "merge-changes");
    let blamed = format!("(in {}", osc.display());
    assert!(
        !error_text(&out).contains(&blamed),
        "an output failure must not be located in the input; stderr:\n{}",
        out.stderr_str()
    );
}
