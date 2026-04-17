//! CLI integration tests - invoke the `pbfhogg` binary via `std::process::Command`.
//!
//! These tests verify that the CLI dispatches correctly, flags are accepted,
//! and feature-gated flags produce clear error messages when the feature is
//! absent at compile time.

use std::path::Path;
use std::process::Command;

/// Path to the compiled `pbfhogg` binary (set by cargo test).
fn pbfhogg_bin() -> &'static str {
    env!("CARGO_BIN_EXE_pbfhogg")
}

/// Write a minimal valid PBF file using the library API.
fn write_minimal_pbf(path: &Path) {
    write_minimal_pbf_impl(path, false);
}

/// Write a minimal valid PBF with the `Sort.Type_then_ID` header flag set.
/// Required for `renumber` which rejects unsorted input.
fn write_minimal_sorted_pbf(path: &Path) {
    write_minimal_pbf_impl(path, true);
}

fn write_minimal_pbf_impl(path: &Path, sorted: bool) {
    use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
    use pbfhogg::writer::{Compression, PbfWriter};

    let file = std::fs::File::create(path).expect("create file");
    let mut writer = PbfWriter::new(file, Compression::default());
    let mut hb = HeaderBuilder::new();
    if sorted {
        hb = hb.sorted();
    }
    let header = hb.build().expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();
    bb.add_node(1, 100_000_000, 200_000_000, [("name", "test")], None);
    bb.add_node(2, 110_000_000, 210_000_000, std::iter::empty(), None);
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    bb.add_way(10, [("highway", "path")], &[1, 2], None);
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write block");
    }

    writer.flush().expect("flush");
}

// ---------------------------------------------------------------------------
// Basic CLI tests
// ---------------------------------------------------------------------------

#[test]
fn cli_version() {
    let output = Command::new(pbfhogg_bin())
        .arg("--version")
        .output()
        .expect("run pbfhogg --version");
    assert!(output.status.success(), "exit code: {}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("pbfhogg"), "version output: {stdout}");
}

#[test]
fn cli_help() {
    let output = Command::new(pbfhogg_bin())
        .arg("--help")
        .output()
        .expect("run pbfhogg --help");
    assert!(output.status.success(), "exit code: {}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("cat"), "help should list cat command");
    assert!(stdout.contains("sort"), "help should list sort command");
}

#[test]
fn cli_cat_passthrough() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_minimal_pbf(&input);

    let result = Command::new(pbfhogg_bin())
        .args(["cat", input.to_str().expect("path"), "-o", output.to_str().expect("path")])
        .output()
        .expect("run pbfhogg cat");

    assert!(result.status.success(), "cat failed: {}", String::from_utf8_lossy(&result.stderr));
    assert!(output.exists(), "output file should exist");
    assert!(output.metadata().expect("metadata").len() > 0, "output should be non-empty");
}

#[test]
fn cli_sort() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_minimal_pbf(&input);

    let result = Command::new(pbfhogg_bin())
        .args(["sort", input.to_str().expect("path"), "-o", output.to_str().expect("path")])
        .output()
        .expect("run pbfhogg sort");

    assert!(result.status.success(), "sort failed: {}", String::from_utf8_lossy(&result.stderr));
    assert!(output.exists(), "output file should exist");
}

#[test]
fn cli_inspect() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");

    write_minimal_pbf(&input);

    let result = Command::new(pbfhogg_bin())
        .args(["inspect", input.to_str().expect("path")])
        .output()
        .expect("run pbfhogg inspect");

    assert!(result.status.success(), "inspect failed: {}", String::from_utf8_lossy(&result.stderr));
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(stdout.contains("nodes") || stdout.contains("Node"), "inspect should show element info");
}

#[test]
fn cli_check() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");

    write_minimal_pbf(&input);

    let result = Command::new(pbfhogg_bin())
        .args(["check", input.to_str().expect("path")])
        .output()
        .expect("run pbfhogg check");

    assert!(result.status.success(), "check failed: {}", String::from_utf8_lossy(&result.stderr));
}

// ---------------------------------------------------------------------------
// Renumber CLI
// ---------------------------------------------------------------------------

#[test]
fn cli_renumber() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_minimal_sorted_pbf(&input);

    let result = Command::new(pbfhogg_bin())
        .args([
            "renumber",
            input.to_str().expect("path"),
            "-o", output.to_str().expect("path"),
        ])
        .output()
        .expect("run pbfhogg renumber");

    assert!(result.status.success(), "renumber failed: {}", String::from_utf8_lossy(&result.stderr));
    assert!(output.exists(), "output file should exist");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("Renumbered") && stderr.contains("2 nodes") && stderr.contains("1 ways"),
        "expected element count summary, got: {stderr}"
    );
}

#[test]
fn cli_renumber_custom_start_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_minimal_sorted_pbf(&input);

    let result = Command::new(pbfhogg_bin())
        .args([
            "renumber",
            "-s", "1000,5000,9000",
            input.to_str().expect("path"),
            "-o", output.to_str().expect("path"),
        ])
        .output()
        .expect("run pbfhogg renumber -s ...");

    assert!(
        result.status.success(),
        "renumber with custom start_id failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    // Verify the output by reading it back and checking the first node's id
    // is the requested start_node_id.
    use pbfhogg::{BlobDecode, BlobReader, Element};
    let reader = BlobReader::from_path(&output).expect("open output");
    let mut first_node_id: Option<i64> = None;
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode") {
            for element in block.elements() {
                match element {
                    Element::DenseNode(dn) => {
                        first_node_id = Some(dn.id());
                        break;
                    }
                    Element::Node(n) => {
                        first_node_id = Some(n.id());
                        break;
                    }
                    _ => {}
                }
            }
        }
        if first_node_id.is_some() { break; }
    }
    assert_eq!(first_node_id, Some(1000), "first node id should be start_node_id=1000");
}

// ---------------------------------------------------------------------------
// Feature-gated flag tests
// ---------------------------------------------------------------------------

/// When built without `linux-direct-io`, `--direct-io` should be accepted by
/// the CLI parser but the command should fail with a clear feature error.
#[cfg(not(feature = "linux-direct-io"))]
#[test]
fn cli_direct_io_rejected_without_feature() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_minimal_pbf(&input);

    let result = Command::new(pbfhogg_bin())
        .args([
            "sort",
            input.to_str().expect("path"),
            "-o", output.to_str().expect("path"),
            "--direct-io",
        ])
        .output()
        .expect("run pbfhogg sort --direct-io");

    assert!(!result.status.success(), "should fail without linux-direct-io feature");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("direct-io") && stderr.contains("feature"),
        "error should mention missing feature, got: {stderr}"
    );
}

/// When built without `linux-io-uring`, `--io-uring` should fail with a clear error.
#[cfg(not(feature = "linux-io-uring"))]
#[test]
fn cli_io_uring_rejected_without_feature() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_minimal_pbf(&input);

    let result = Command::new(pbfhogg_bin())
        .args([
            "sort",
            input.to_str().expect("path"),
            "-o", output.to_str().expect("path"),
            "--io-uring",
        ])
        .output()
        .expect("run pbfhogg sort --io-uring");

    assert!(!result.status.success(), "should fail without linux-io-uring feature");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("io-uring") && stderr.contains("feature"),
        "error should mention missing feature, got: {stderr}"
    );
}
