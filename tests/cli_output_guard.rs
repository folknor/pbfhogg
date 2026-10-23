//! The output contract, end to end through the binary: every output is
//! written to a staging sibling and published only on success; an existing
//! output is refused before any work unless `--overwrite` (`-O`) is given;
//! a symlink is refused always.
//!
//! The regression the staging guards: a command opened the destination with
//! `File::create` before it had produced a single byte, so pointing `-o` at
//! an existing file destroyed that file the moment the run began, and any
//! mid-run failure left a truncated stub under the name the user chose.
//!
//! ## Where each property is proven
//!
//! The publication mechanism - staging, refusal, exclusive publication,
//! group rollback, every injected fault - is proven on the library type in
//! `src/output.rs`. Forced mid-run failures of real commands are in the
//! fault tests (`fault_apply_changes`, `fault_parallel_writer`,
//! `fault_geocode_pass3`) and `renumber_external`, each asserting that no
//! file appears at the destination.
//!
//! Here, through the CLI: `cat` with a corrupt second input fails after its
//! writer opened, and `add-locations-to-ways` refuses a complete output
//! after its writer finished; both must leave a pre-existing destination
//! byte-identical. The refusal tests prove "before any work" by pointing the
//! command at an input that does not exist: the output refusal must be the
//! error reported, not the missing input.

#![allow(clippy::unwrap_used)]

use std::path::Path;

use crate::common::cli::CliInvoker;
use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
use pbfhogg::writer::{Compression, PbfWriter};
use tempfile::TempDir;

fn write_fixture(path: &Path) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    writer
        .write_header(&HeaderBuilder::new().build().unwrap())
        .unwrap();
    let mut builder = BlockBuilder::new();
    builder.add_node(1, 100_000_000, 100_000_000, [("amenity", "cafe")], None);
    if let Some(bytes) = builder.take().unwrap() {
        writer.write_primitive_block(bytes).unwrap();
    }
    writer.flush().unwrap();
}

/// A PBF truncated inside a blob. Framing is broken, so this fails in the
/// header preflight; used only where the question is what the *message*
/// says.
fn write_truncated(path: &Path, source: &Path) {
    let mut bytes = std::fs::read(source).unwrap();
    let keep = bytes.len() - 5;
    bytes.truncate(keep);
    std::fs::write(path, bytes).unwrap();
}

fn scratch_leftovers(dir: &Path) -> Vec<std::ffi::OsString> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .filter(|n| n.to_string_lossy().starts_with(".pbfhogg-tmp-"))
        .collect()
}

/// Run a command that should succeed, and assert its output landed at the
/// name the user gave with no scratch file left beside it.
///
/// This is the test that bites on the realistic wiring mistakes: a call
/// site wrapped in the guard but never committed leaves the destination
/// missing, and a guard that forgets to remove or rename its scratch
/// leaves a stray `.pbfhogg-tmp-*` behind. Both fail here loudly.
fn assert_output_published(dir: &Path, dest: &Path, args: &[&str]) {
    let out = CliInvoker::new().args(args).run();
    assert!(
        out.status.success(),
        "command failed; stderr:\n{}",
        out.stderr_str()
    );
    assert!(
        dest.exists(),
        "output never appeared at the path the user named: {}",
        dest.display()
    );
    let len = std::fs::metadata(dest).unwrap().len();
    assert!(len > 0, "output at {} is empty", dest.display());
    let strays = scratch_leftovers(dir);
    assert!(strays.is_empty(), "scratch file leaked: {strays:?}");
}

#[test]
fn sort_publishes_through_the_guard() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input);
    let dest = dir.path().join("out.osm.pbf");
    assert_output_published(
        dir.path(),
        &dest,
        &[
            "sort",
            input.to_str().unwrap(),
            "--force",
            "-o",
            dest.to_str().unwrap(),
        ],
    );
}

#[test]
fn cat_publishes_through_the_guard() {
    let dir = TempDir::new().unwrap();
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    write_fixture(&a);
    write_fixture(&b);
    let dest = dir.path().join("out.osm.pbf");
    assert_output_published(
        dir.path(),
        &dest,
        &[
            "cat",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--force",
            "-o",
            dest.to_str().unwrap(),
        ],
    );
}

#[test]
fn tags_filter_publishes_through_the_guard() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input);
    let dest = dir.path().join("out.osm.pbf");
    assert_output_published(
        dir.path(),
        &dest,
        &[
            "tags-filter",
            input.to_str().unwrap(),
            "--force",
            "-o",
            dest.to_str().unwrap(),
            "amenity",
        ],
    );
}

#[test]
fn getid_publishes_through_the_guard() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input);
    let dest = dir.path().join("out.osm.pbf");
    assert_output_published(
        dir.path(),
        &dest,
        &[
            "getid",
            input.to_str().unwrap(),
            "--force",
            "-o",
            dest.to_str().unwrap(),
            "n1",
        ],
    );
}

/// `merge-changes` picks gzip from the output name ending in `.gz`. The
/// scratch file therefore has to keep that suffix, or the committed file
/// is plain XML under a `.gz` name - a bug the guard would have introduced
/// rather than fixed.
#[test]
fn merge_changes_output_is_still_gzipped_through_the_guard() {
    let dir = TempDir::new().unwrap();
    let osc = dir.path().join("a.osc");
    std::fs::write(
        &osc,
        b"<?xml version='1.0' encoding='UTF-8'?>\n\
          <osmChange version=\"0.6\">\n\
          <create><node id=\"1\" version=\"1\" lat=\"1.0\" lon=\"2.0\"/></create>\n\
          </osmChange>\n",
    )
    .unwrap();
    let dest = dir.path().join("merged.osc.gz");
    CliInvoker::new()
        .args([
            "merge-changes",
            osc.to_str().unwrap(),
            "-o",
            dest.to_str().unwrap(),
        ])
        .assert_success();
    let bytes = std::fs::read(&dest).unwrap();
    assert!(
        bytes.starts_with(&[0x1f, 0x8b]),
        "merge-changes output is not gzipped: first bytes {:?}",
        &bytes[..bytes.len().min(8)]
    );
}

/// A nonexistent input must be reported as a missing input, by name. The
/// up-front aliasing check canonicalizes every input path, and used to
/// surface a nonexistent one as a bare `No such file or directory` from
/// the check itself - before the command ran, and without naming the file.
#[test]
fn missing_input_is_reported_as_a_missing_input() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("nowhere.osm.pbf");
    let dest = dir.path().join("out.osm.pbf");
    let out = CliInvoker::new()
        .args([
            "sort",
            missing.to_str().unwrap(),
            "-o",
            dest.to_str().unwrap(),
        ])
        .run();
    assert!(!out.status.success());
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("nowhere.osm.pbf"),
        "missing-input error does not name the input; stderr:\n{stderr}"
    );
}

/// The aliasing check itself must still fire - canonicalizing lazily must
/// not have turned it off for inputs that do exist.
#[test]
fn input_output_alias_is_still_rejected() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input);
    let out = CliInvoker::new()
        .args([
            "sort",
            input.to_str().unwrap(),
            "--force",
            "-o",
            input.to_str().unwrap(),
        ])
        .run();
    assert!(!out.status.success());
    assert!(
        out.stderr_str().contains("aliases input"),
        "aliasing check did not fire; stderr:\n{}",
        out.stderr_str()
    );
}

/// A framing error names the input it came from and the blob it sits in.
/// On a multi-input command a byte offset alone cannot say which stream it
/// indexes.
#[test]
fn framing_error_names_the_input_file() {
    let dir = TempDir::new().unwrap();
    let good = dir.path().join("in.osm.pbf");
    let bad = dir.path().join("corrupt.osm.pbf");
    write_fixture(&good);
    write_truncated(&bad, &good);
    let dest = dir.path().join("out.osm.pbf");
    let out = CliInvoker::new()
        .args([
            "cat",
            good.to_str().unwrap(),
            bad.to_str().unwrap(),
            "-c",
            "version",
            "--force",
            "-o",
            dest.to_str().unwrap(),
        ])
        .run();
    assert!(!out.status.success());
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("corrupt.osm.pbf"),
        "error does not name the failing input; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("blob "),
        "error does not name the failing blob; stderr:\n{stderr}"
    );
}

/// The survival property, per-command, on the one multi-input path where a
/// CLI-reachable failure genuinely lands *after* the writer opened: `cat`
/// re-encoding two inputs writes the first one out before it ever reads the
/// second, so a corrupt second input fails mid-stream with the destination
/// already opened under the old shape.
///
/// `--overwrite` is given, so the run gets past the refusal and genuinely
/// fails mid-stream: writing to the destination instead of the staging file
/// would leave the seeded file gone.
#[test]
fn cat_failure_leaves_preexisting_destination_intact() {
    let dir = TempDir::new().unwrap();
    let good = dir.path().join("in.osm.pbf");
    let bad = dir.path().join("corrupt.osm.pbf");
    write_fixture(&good);
    write_truncated(&bad, &good);
    let dest = dir.path().join("precious.osm.pbf");
    std::fs::write(&dest, b"user data").unwrap();
    let out = CliInvoker::new()
        .args([
            "cat",
            good.to_str().unwrap(),
            bad.to_str().unwrap(),
            "-c",
            "version",
            "--force",
            "-o",
            dest.to_str().unwrap(),
            "--overwrite",
        ])
        .run();
    assert!(!out.status.success(), "corrupt input should fail the run");
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        b"user data",
        "a failed run destroyed the pre-existing destination"
    );
    let strays = scratch_leftovers(dir.path());
    assert!(strays.is_empty(), "scratch file leaked: {strays:?}");
}

/// `bench-merge` must keep a failed merge away from the user-named output.
/// A malformed but existing OSC reaches the command after alias validation;
/// the old upfront remove deleted the destination before parsing it.
#[test]
fn bench_merge_failure_leaves_preexisting_destination_intact() {
    let dir = crate::common::test_scratch();
    let base = dir.path().join("base.osm.pbf");
    let changes = dir.path().join("malformed.osc");
    crate::common::write_indexed_pbf(
        &base,
        &[crate::common::TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );
    std::fs::write(&changes, b"<osmChange><create>").unwrap();
    let dest = dir.path().join("precious.osm.pbf");
    std::fs::write(&dest, b"user data").unwrap();

    let out = CliInvoker::new()
        .args([
            "bench-merge",
            base.to_str().unwrap(),
            changes.to_str().unwrap(),
            "-o",
            dest.to_str().unwrap(),
            "--overwrite",
        ])
        .run();
    assert!(!out.status.success(), "malformed OSC should fail the merge");
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        b"user data",
        "a failed benchmark merge destroyed the pre-existing destination"
    );
    let strays = scratch_leftovers(dir.path());
    assert!(strays.is_empty(), "scratch file leaked: {strays:?}");
}

/// A PBF whose single way references a node that is not in the file.
/// `add-locations-to-ways` writes a complete output and only then refuses,
/// which is the post-writer CLI failure this file otherwise has no way to
/// produce.
fn write_dangling_ref_fixture(path: &Path) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    writer
        .write_header(&HeaderBuilder::new().build().unwrap())
        .unwrap();
    let mut builder = BlockBuilder::new();
    builder.add_node(1, 100_000_000, 100_000_000, [("amenity", "cafe")], None);
    if let Some(bytes) = builder.take().unwrap() {
        writer.write_primitive_block(bytes).unwrap();
    }
    // Node 999 is absent, so the run fails its missing-locations policy.
    builder.add_way(10, [("highway", "primary")], &[1, 999], None);
    if let Some(bytes) = builder.take().unwrap() {
        writer.write_primitive_block(bytes).unwrap();
    }
    writer.flush().unwrap();
}

/// The single-input, post-writer case: `add-locations-to-ways` produces a
/// complete PBF and only then refuses it, so the guard is the only thing
/// standing between a failed run and a destroyed destination.
///
/// Unlike a truncated-input fixture, this genuinely fails after the writer
/// closed - point the dispatch site at the destination instead of the
/// scratch sibling and this test reports `precious.osm.pbf` replaced by a
/// PBF full of fabricated `(0, 0)` vertices.
#[test]
fn altw_missing_nodes_leave_preexisting_destination_intact() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("in.osm.pbf");
    write_dangling_ref_fixture(&input);
    let dest = dir.path().join("precious.osm.pbf");
    std::fs::write(&dest, b"user data").unwrap();

    let out = CliInvoker::new()
        .args([
            "add-locations-to-ways",
            input.to_str().unwrap(),
            "--force",
            "-o",
            dest.to_str().unwrap(),
            "--overwrite",
        ])
        .run();
    assert!(
        !out.status.success(),
        "an unresolvable way node ref must fail the run; stderr:\n{}",
        out.stderr_str()
    );
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        b"user data",
        "a failed run destroyed the pre-existing destination"
    );
    let strays = scratch_leftovers(dir.path());
    assert!(strays.is_empty(), "scratch file leaked: {strays:?}");
}

/// Passing counterpart, so the test above is not satisfied by a command
/// that never publishes anything: the same fixture with the policy waived
/// replaces the destination through the guard, leaving no scratch behind.
#[test]
fn altw_publishes_through_the_guard() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("in.osm.pbf");
    write_dangling_ref_fixture(&input);
    let dest = dir.path().join("out.osm.pbf");
    assert_output_published(
        dir.path(),
        &dest,
        &[
            "add-locations-to-ways",
            input.to_str().unwrap(),
            "--ignore-missing-nodes",
            "--force",
            "-o",
            dest.to_str().unwrap(),
        ],
    );
}

/// A usage error keeps clap's own exit code and its own message. The switch
/// from `Cli::parse` to `Cli::try_parse` exists to drop the hotpath guard
/// on the way out, and must be invisible otherwise - so this is a guard
/// against the change, not a demonstration of it.
#[test]
fn usage_error_keeps_claps_exit_code_and_message() {
    let out = CliInvoker::new().args(["sort", "--no-such-flag"]).run();
    assert_eq!(
        out.status.code(),
        Some(2),
        "usage error should exit 2; stderr:\n{}",
        out.stderr_str()
    );
    assert!(
        out.stderr_str().contains("unexpected argument"),
        "usage error lost clap's message; stderr:\n{}",
        out.stderr_str()
    );
}

/// `--help` is also an `Err` from `try_parse`, with exit code 0 and its
/// text on stdout. Getting that wrong would turn every `--help` into a
/// failure, which no other test here would notice.
#[test]
fn help_exits_zero_on_stdout() {
    let out = CliInvoker::new().args(["--help"]).run();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("OpenStreetMap PBF toolkit"),
        "--help text did not reach stdout"
    );
}

// ---------------------------------------------------------------------------
// Existing outputs and symlinks
// ---------------------------------------------------------------------------

/// An existing output is refused, and refused before any work: the input
/// here does not exist, so a command that did any work first would report
/// the missing input instead. The advice names the flag; the file survives.
#[test]
fn an_existing_output_is_refused_before_any_work() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("no-such-input.osm.pbf");
    let dest = dir.path().join("out.osm.pbf");
    std::fs::write(&dest, b"user data").unwrap();
    let out = CliInvoker::new()
        .args([
            "sort",
            missing.to_str().unwrap(),
            "-o",
            dest.to_str().unwrap(),
        ])
        .run();
    assert_eq!(out.status.code(), Some(2), "stderr:\n{}", out.stderr_str());
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("already exists") && stderr.contains("--overwrite (-O)"),
        "the refusal must name the output and the flag; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("no-such-input"),
        "the refusal must come before the input is read; stderr:\n{stderr}"
    );
    assert_eq!(std::fs::read(&dest).unwrap(), b"user data");
    assert!(scratch_leftovers(dir.path()).is_empty());
}

/// `--overwrite` and its short form `-O` replace an existing output.
#[test]
fn overwrite_replaces_an_existing_output() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input);
    for flag in ["--overwrite", "-O"] {
        let dest = dir.path().join(format!("out{flag}.osm.pbf"));
        std::fs::write(&dest, b"stale").unwrap();
        CliInvoker::new()
            .args([
                "sort",
                input.to_str().unwrap(),
                "--force",
                "-o",
                dest.to_str().unwrap(),
                flag,
            ])
            .assert_success();
        let bytes = std::fs::read(&dest).unwrap();
        assert_ne!(bytes, b"stale", "{flag} did not replace the output");
        assert!(
            pbfhogg::ElementReader::from_path(&dest).is_ok(),
            "{flag}: the replaced output is not a PBF"
        );
    }
    assert!(scratch_leftovers(dir.path()).is_empty());
}

/// A symlink is never an output, not even with `--overwrite`: renaming over
/// it would turn the link into a regular file. The error names the link and
/// its target, and both are left alone.
#[cfg(unix)]
#[test]
fn a_symlinked_output_is_refused_even_with_overwrite() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input);
    let target = dir.path().join("target.osm.pbf");
    std::fs::write(&target, b"user data").unwrap();
    let link = dir.path().join("link.osm.pbf");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let out = CliInvoker::new()
        .args([
            "sort",
            input.to_str().unwrap(),
            "--force",
            "-o",
            link.to_str().unwrap(),
            "--overwrite",
        ])
        .run();
    assert_eq!(out.status.code(), Some(2), "stderr:\n{}", out.stderr_str());
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("link.osm.pbf is a symlink to") && stderr.contains("target.osm.pbf"),
        "the error must name the link and its target; stderr:\n{stderr}"
    );
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"user data");
}

/// Every output a config names is checked before any extraction: one
/// existing output among several refuses the whole run and creates none of
/// the others. `--overwrite` on the command line covers them all.
#[test]
fn extract_config_checks_every_output_up_front() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input);
    let config = dir.path().join("config.json");
    std::fs::write(
        &config,
        format!(
            r#"{{
                "directory": "{}",
                "extracts": [
                    {{ "output": "a.osm.pbf", "bbox": [9.0, 9.0, 11.0, 11.0] }},
                    {{ "output": "b.osm.pbf", "bbox": [9.0, 9.0, 11.0, 11.0] }},
                    {{ "output": "c.osm.pbf", "bbox": [9.0, 9.0, 11.0, 11.0] }}
                ]
            }}"#,
            dir.path().display()
        ),
    )
    .unwrap();
    let existing = dir.path().join("c.osm.pbf");
    std::fs::write(&existing, b"user data").unwrap();
    let args = [
        "extract",
        input.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
        "--simple",
        "--force",
    ];
    let out = CliInvoker::new().args(args).run();
    assert_eq!(out.status.code(), Some(2), "stderr:\n{}", out.stderr_str());
    out.assert_stderr_contains("c.osm.pbf already exists");
    assert!(!dir.path().join("a.osm.pbf").exists());
    assert!(!dir.path().join("b.osm.pbf").exists());
    assert_eq!(std::fs::read(&existing).unwrap(), b"user data");
    assert!(scratch_leftovers(dir.path()).is_empty());

    CliInvoker::new()
        .args(args)
        .arg("--overwrite")
        .assert_success();
    for name in ["a.osm.pbf", "b.osm.pbf", "c.osm.pbf"] {
        assert!(
            pbfhogg::ElementReader::from_path(dir.path().join(name)).is_ok(),
            "{name} was not published"
        );
    }
    assert!(scratch_leftovers(dir.path()).is_empty());
}

/// A directory output follows the same rule: an existing index directory is
/// refused before any work without `--overwrite`.
#[test]
fn an_existing_index_directory_is_refused_before_any_work() {
    let dir = TempDir::new().unwrap();
    let index = dir.path().join("index");
    std::fs::create_dir(&index).unwrap();
    let out = CliInvoker::new()
        .args([
            "build-geocode-index",
            dir.path().join("no-such-input.osm.pbf").to_str().unwrap(),
            "--output-dir",
            index.to_str().unwrap(),
        ])
        .run();
    assert_eq!(out.status.code(), Some(2), "stderr:\n{}", out.stderr_str());
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("already exists") && stderr.contains("--overwrite (-O)"),
        "stderr:\n{stderr}"
    );
    assert!(!stderr.contains("no-such-input"), "stderr:\n{stderr}");
    assert!(index.is_dir());
}

/// Non-PBF outputs go through the same rules: `diff -o`, `export -o` and
/// `merge-changes -o` refuse an existing output and replace it under `-O`.
#[test]
fn text_and_osc_outputs_follow_the_same_rules() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("in.osm.pbf");
    // Sorted, because diff requires it.
    crate::common::write_test_pbf_sorted(
        &input,
        &[crate::common::TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 100_000_000,
            tags: vec![("amenity", "cafe")],
            meta: None,
        }],
        &[],
        &[],
    );
    let osc = dir.path().join("a.osc");
    std::fs::write(
        &osc,
        b"<osmChange version=\"0.6\"><create><node id=\"1\" version=\"1\" lat=\"1.0\" lon=\"2.0\"/></create></osmChange>",
    )
    .unwrap();
    let cases: [(&str, Vec<String>); 3] = [
        (
            "diff.txt",
            vec![
                "diff".into(),
                input.display().to_string(),
                input.display().to_string(),
            ],
        ),
        (
            "export.geojsonseq",
            vec![
                "export".into(),
                input.display().to_string(),
                "--type".into(),
                "node".into(),
            ],
        ),
        (
            "merged.osc",
            vec!["merge-changes".into(), osc.display().to_string()],
        ),
    ];
    for (name, args) in cases {
        let dest = dir.path().join(name);
        std::fs::write(&dest, b"user data").unwrap();
        let out = CliInvoker::new().args(&args).arg("-o").arg(&dest).run();
        assert_eq!(
            out.status.code(),
            Some(2),
            "{name}: stderr:\n{}",
            out.stderr_str()
        );
        out.assert_stderr_contains("already exists");
        assert_eq!(std::fs::read(&dest).unwrap(), b"user data", "{name}");

        let out = CliInvoker::new()
            .args(&args)
            .arg("-o")
            .arg(&dest)
            .arg("-O")
            .run();
        assert!(
            out.status.code() == Some(0) || out.status.code() == Some(1),
            "{name}: stderr:\n{}",
            out.stderr_str()
        );
        assert_ne!(std::fs::read(&dest).unwrap(), b"user data", "{name}");
    }
    assert!(scratch_leftovers(dir.path()).is_empty());
}
