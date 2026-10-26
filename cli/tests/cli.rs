//! CLI integration tests - invoke the `pbfhogg` binary via `std::process::Command`.
//!
//! These tests verify that the CLI dispatches correctly, flags are accepted,
//! and feature-gated flags produce clear error messages when the feature is
//! absent at compile time.

use std::path::Path;
use std::process::Command;

use crate::invoker::pbfhogg_bin;

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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_minimal_pbf(&input);

    let result = Command::new(pbfhogg_bin())
        .args([
            "cat",
            input.to_str().expect("path"),
            "-o",
            output.to_str().expect("path"),
        ])
        .output()
        .expect("run pbfhogg cat");

    assert!(
        result.status.success(),
        "cat failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(output.exists(), "output file should exist");
    assert!(
        output.metadata().expect("metadata").len() > 0,
        "output should be non-empty"
    );
}

#[test]
fn cli_sort() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_minimal_pbf(&input);

    let result = Command::new(pbfhogg_bin())
        .args([
            "sort",
            input.to_str().expect("path"),
            "-o",
            output.to_str().expect("path"),
        ])
        .output()
        .expect("run pbfhogg sort");

    assert!(
        result.status.success(),
        "sort failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(output.exists(), "output file should exist");
}

#[test]
fn cli_inspect() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");

    write_minimal_pbf(&input);

    let result = Command::new(pbfhogg_bin())
        .args(["inspect", input.to_str().expect("path")])
        .output()
        .expect("run pbfhogg inspect");

    assert!(
        result.status.success(),
        "inspect failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        stdout.contains("nodes") || stdout.contains("Node"),
        "inspect should show element info"
    );
}

#[test]
fn cli_check() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");

    write_minimal_pbf(&input);

    let result = Command::new(pbfhogg_bin())
        .args(["check", input.to_str().expect("path")])
        .output()
        .expect("run pbfhogg check");

    assert!(
        result.status.success(),
        "check failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

/// `--refs` reads nothing with O_DIRECT, so `--direct-io` is accepted, has
/// no effect, and says so; it used to be ignored silently. `--refs` alone
/// opens nothing with O_DIRECT, so this runs on any filesystem. A build
/// without `linux-direct-io` refuses the flag before any check runs.
#[cfg(feature = "linux-direct-io")]
#[test]
fn cli_check_refs_reports_direct_io_as_having_no_effect() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    write_minimal_pbf(&input);

    let result = Command::new(pbfhogg_bin())
        .arg("check")
        .arg(&input)
        .args(["--refs", "--direct-io"])
        .output()
        .expect("run pbfhogg check");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "{stderr}");
    assert!(
        stderr.contains("--direct-io has no effect on --refs"),
        "{stderr}"
    );

    // Without the flag there is nothing to say.
    let result = Command::new(pbfhogg_bin())
        .arg("check")
        .arg(&input)
        .arg("--refs")
        .output()
        .expect("run pbfhogg check");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "{stderr}");
    assert!(!stderr.contains("--direct-io"), "{stderr}");
}

// ---------------------------------------------------------------------------
// Renumber CLI
// ---------------------------------------------------------------------------

#[test]
fn cli_renumber() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_minimal_sorted_pbf(&input);

    let result = Command::new(pbfhogg_bin())
        .args([
            "renumber",
            input.to_str().expect("path"),
            "-o",
            output.to_str().expect("path"),
        ])
        .output()
        .expect("run pbfhogg renumber");

    assert!(
        result.status.success(),
        "renumber failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(output.exists(), "output file should exist");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("Renumbered") && stderr.contains("2 nodes") && stderr.contains("1 ways"),
        "expected element count summary, got: {stderr}"
    );
}

#[test]
fn cli_renumber_custom_start_id() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_minimal_sorted_pbf(&input);

    let result = Command::new(pbfhogg_bin())
        .args([
            "renumber",
            "--start-id",
            "1000,5000,9000",
            input.to_str().expect("path"),
            "-o",
            output.to_str().expect("path"),
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
        if first_node_id.is_some() {
            break;
        }
    }
    assert_eq!(
        first_node_id,
        Some(1000),
        "first node id should be start_node_id=1000"
    );
}

// ---------------------------------------------------------------------------
// Feature-gated flag tests
// ---------------------------------------------------------------------------

/// When built without `linux-direct-io`, `--direct-io` is refused while the
/// arguments are parsed, with a clear feature error.
#[cfg(not(feature = "linux-direct-io"))]
#[test]
fn cli_direct_io_rejected_without_feature() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_minimal_pbf(&input);

    let result = Command::new(pbfhogg_bin())
        .args([
            "sort",
            input.to_str().expect("path"),
            "-o",
            output.to_str().expect("path"),
            "--direct-io",
        ])
        .output()
        .expect("run pbfhogg sort --direct-io");

    assert!(
        !result.status.success(),
        "should fail without linux-direct-io feature"
    );
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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_minimal_pbf(&input);

    let result = Command::new(pbfhogg_bin())
        .args([
            "sort",
            input.to_str().expect("path"),
            "-o",
            output.to_str().expect("path"),
            "--io-uring",
        ])
        .output()
        .expect("run pbfhogg sort --io-uring");

    assert!(
        !result.status.success(),
        "should fail without linux-io-uring feature"
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("io-uring") && stderr.contains("feature"),
        "error should mention missing feature, got: {stderr}"
    );
}

#[test]
fn cli_rejects_ambiguous_inspect_and_missing_extract_region() {
    for args in [
        vec!["inspect", "first.pbf", "tags", "second.pbf"],
        vec!["inspect"],
        vec!["extract", "input.pbf", "-o", "output.pbf"],
    ] {
        let output = Command::new(pbfhogg_bin())
            .args(&args)
            .output()
            .expect("run CLI");
        assert_eq!(output.status.code(), Some(2), "args: {args:?}");
    }
}

#[test]
fn cli_tags_filter_ambiguous_content_uses_legacy_fallback() {
    let dir = crate::common::test_scratch();
    for (name, bytes) in [
        ("unknown.pbf", b"not a PBF".as_slice()),
        ("unknown.osc.gz", b"\x1f\x8bnot OSC".as_slice()),
        ("unknown.osc.bz2", b"BZhnot OSC".as_slice()),
    ] {
        let input = dir.path().join(name);
        std::fs::write(&input, bytes).expect("write input");
        let output = Command::new(pbfhogg_bin())
            .arg("tags-filter")
            .arg(&input)
            .arg("name")
            .arg("-o")
            .arg(dir.path().join("out.pbf"))
            .output()
            .expect("run tags-filter");
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("Cannot determine input format"));
    }
}

#[test]
fn cli_tags_filter_unknown_extension_does_not_skip_bom_and_whitespace() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("changes.unknown");
    std::fs::write(
        &input,
        b"\xef\xbb\xbf  \n<osmChange version=\"0.6\"></osmChange>",
    )
    .expect("write XML");
    let output = Command::new(pbfhogg_bin())
        .arg("tags-filter")
        .arg(&input)
        .arg("name")
        .arg("-o")
        .arg(dir.path().join("out.osc"))
        .output()
        .expect("run tags-filter");
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Cannot determine input format"));
}

#[test]
fn cli_diff_quiet_osc_without_output_retains_error() {
    let output = Command::new(pbfhogg_bin())
        .args(["diff", "old.pbf", "new.pbf", "--format", "osc", "-q"])
        .output()
        .expect("run diff");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--output is required with --format osc")
    );
}

#[cfg(unix)]
#[test]
fn cli_check_json_preserves_non_utf8_filename_lossily() {
    use std::os::unix::ffi::OsStringExt;
    let dir = crate::common::test_scratch();
    let name = std::ffi::OsString::from_vec(b"actual-\xff.pbf".to_vec());
    let input = dir.path().join(&name);
    write_minimal_pbf(&input);
    let output = Command::new(pbfhogg_bin())
        .arg("check")
        .arg(&input)
        .args(["--ids", "--json"])
        .output()
        .expect("run check");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert!(json.to_string().contains(name.to_string_lossy().as_ref()));
    assert!(!json.to_string().contains("input.osm.pbf"));
}

#[test]
fn cli_osc_outputs_are_plain_under_osc_and_keep_metadata() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osc");
    std::fs::write(&input, b"<osmChange version=\"0.6\"><create><node id=\"7\" lat=\"1\" lon=\"2\" timestamp=\"yesterday\" changeset=\"no\" uid=\"&#48;\" visible=\"False\"><tag k=\"name\" v=\"test\"/></node></create></osmChange>").expect("XML");
    for command in ["tags-filter", "merge-changes"] {
        let output = dir.path().join(format!("{command}.osc"));
        let mut cli = Command::new(pbfhogg_bin());
        cli.arg(command).arg(&input);
        if command == "tags-filter" {
            cli.args(["--input-kind", "osc", "name"]);
        }
        let result = cli.arg("-o").arg(&output).output().expect("run command");
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let bytes = std::fs::read(&output).expect("output");
        assert!(!bytes.starts_with(b"\x1f\x8b"), "{command}: .osc is plain");
        let xml = String::from_utf8(bytes).expect("XML");
        for attribute in [
            "timestamp=\"yesterday\"",
            "changeset=\"no\"",
            "uid=\"0\"",
            "visible=\"False\"",
        ] {
            assert!(xml.contains(attribute), "{xml}");
        }
    }
}

#[cfg(unix)]
#[test]
fn cli_merge_changes_non_utf8_osc_gz_path_is_gzipped() {
    use std::os::unix::ffi::OsStrExt;
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osc");
    std::fs::write(&input, b"<osmChange version=\"0.6\"></osmChange>").expect("XML");
    for simplify in [false, true] {
        let output = dir
            .path()
            .join(std::ffi::OsStr::from_bytes(b"invalid-\xff.osc.gz"));
        let mut cli = Command::new(pbfhogg_bin());
        // Both passes write the same name; the second replaces the first.
        cli.arg("merge-changes")
            .arg(&input)
            .arg("-o")
            .arg(&output)
            .arg("--overwrite");
        if simplify {
            cli.arg("--simplify");
        }
        let result = cli.output().expect("merge");
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let bytes = std::fs::read(&output).expect("output");
        assert!(bytes.starts_with(b"\x1f\x8b"), "simplify={simplify}");
    }
}

#[cfg(unix)]
#[test]
fn cli_osc_counters_keep_historical_name_and_per_file_values() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osc");
    let base = dir.path().join("base.pbf");
    write_minimal_sorted_pbf(&base);
    std::fs::write(&input, b"<osmChange version=\"0.6\"></osmChange>").expect("XML");
    for command in ["merge-changes", "tags-filter", "apply-changes"] {
        let markers = dir.path().join(format!("{command}.markers"));
        std::fs::write(&markers, []).expect("markers");
        let mut cli = Command::new(pbfhogg_bin());
        cli.env("BROKKR_MARKER_FIFO", &markers).arg(command);
        match command {
            "apply-changes" => {
                cli.arg(&base).arg(&input);
            }
            "tags-filter" => {
                cli.arg(&input).args(["--input-kind", "osc", "name"]);
            }
            _ => {
                cli.arg(&input).arg(&input);
            }
        }
        let result = cli
            .arg("-o")
            .arg(dir.path().join(format!("{command}-out.osc")))
            .output()
            .expect("command");
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let events = std::fs::read_to_string(&markers).expect("markers");
        let values: Vec<_> = events
            .lines()
            .filter_map(|line| {
                line.split_once("@merge_changes_decompress_ns=")
                    .map(|(_, value)| value)
            })
            .collect();
        assert_eq!(
            values.len(),
            if command == "merge-changes" { 2 } else { 1 },
            "{events}"
        );
        // The historical timer includes reader work even for plain XML. Every
        // input emits its own sample, rather than a command-specific total.
        assert!(
            values.iter().all(|value| value.parse::<u64>().is_ok()),
            "{events}"
        );
        assert!(!events.contains("apply_changes_osc_decompress_ns"));
        assert!(!events.contains("tags_filter_osc_decompress_ns"));
    }
}
