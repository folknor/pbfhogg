//! Every user-named `--output` is written through a scratch sibling and
//! renamed into place only on success.
//!
//! The regression this guards: a command opened the destination with
//! `File::create` before it had produced a single byte, so pointing `-o` at
//! an existing file destroyed that file the moment the run began, and any
//! mid-run failure left a truncated stub under the name the user chose.
//!
//! ## What is testable from here, and what is not
//!
//! The survival property itself - "a run that dies after the writer opened
//! leaves the destination byte-identical" - is covered where it can be
//! covered honestly: `src/path_guard.rs` proves it on the primitive, and
//! `tests/cli_export.rs` proves it end-to-end on `export`, whose decode
//! failure genuinely lands after its writer is open.
//!
//! It is covered here for `cat`, the one CLI-reachable case where a failure
//! genuinely lands after the writer opened: re-encoding several inputs
//! writes the first one out before it ever reads the second, so a corrupt
//! second input fails mid-stream with the destination already open under
//! the old shape. `cat_failure_leaves_preexisting_destination_intact` is
//! confirmed to fail when `guarded_output` is made to run against the
//! destination path.
//!
//! It is also covered for one single-input command:
//! `add-locations-to-ways`. Its missing-locations policy can only be
//! evaluated once a complete output exists - the external backend does not
//! learn its resolved count until its last stage - so a way referencing an
//! absent node is a CLI-reachable failure that lands *after* the writer has
//! finished, which is exactly the shape this file could not previously
//! reach. `altw_missing_nodes_leave_preexisting_destination_intact` covers
//! it, and needs no fault injection: a two-node fixture with a dangling ref
//! is enough.
//!
//! It remains uncovered for the *other* single-input commands, deliberately.
//! Every corrupt-input fixture reachable from the CLI makes those fail
//! before their writer opens: a truncated file dies in the header-walk
//! preflight, and a corrupted blob payload dies in `sort`'s pass 1. A
//! survival test built on either passes with the guard removed - it proves
//! nothing, and a test that still passes when the production change is
//! reverted is worse than no test. Closing that remaining gap needs a
//! `test-hooks`-gated fault-injection flag on the CLI, of the same shape as
//! `apply_changes`'s existing `panic_at_blob_seq`.
//!
//! What is tested here is the wiring, against the failure modes the wiring
//! actually has: a guard that never commits (output missing), a guard whose
//! scratch name changes what the writer produces (the `.gz` case), and a
//! guard that leaks its scratch file.

#![cfg(feature = "commands")]
#![allow(clippy::unwrap_used)]

mod common;

use std::path::Path;

use common::cli::CliInvoker;
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
/// This bites on the production change itself: run the closure against the
/// destination instead of the scratch sibling and the seeded file is gone.
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
