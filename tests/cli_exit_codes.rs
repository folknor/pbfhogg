//! CLI-driven tests for the exit-status contract and for the argument
//! combinations `inspect` used to accept and silently discard.
//!
//! Exit statuses are a user-visible interface: scripts branch on them
//! and `brokkr` shells out to this binary. The contract is
//!
//!   0 - success
//!   1 - the command ran and its answer is negative (differences found,
//!       no indexdata, integrity violations, element not found)
//!   2 - the command failed
//!
//! Before this split every one of those outcomes was `1`, so a caller
//! could not tell "the files differ" from "the file does not exist".
//! `1` deliberately kept its meaning, so existing callers that branch
//! on the semantic answer are unaffected.
//!
//! No imports from the command modules - a rewrite of `src/commands/`
//! cannot break these tests by type changes alone.

#![allow(clippy::unwrap_used)]

use std::path::Path;

use crate::common::cli::CliInvoker;
use crate::common::{TestNode, write_test_pbf_sorted};
use tempfile::TempDir;

const EXIT_SUCCESS: i32 = 0;
const EXIT_ANSWER_NEGATIVE: i32 = 1;
const EXIT_FAILED: i32 = 2;

fn write_two_nodes(path: &Path, second_lat: i32) {
    write_test_pbf_sorted(
        path,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: second_lat,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[],
        &[],
    );
}

fn code(out: &crate::common::cli::CliOutput) -> i32 {
    out.status.code().expect("child exited with a code")
}

/// The three bands, on one command. `diff` is the case the split exists
/// for: identical files, differing files, and a missing input must land
/// on three different codes.
#[test]
fn diff_separates_identical_differing_and_failure() {
    let dir = TempDir::new().unwrap();
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    let c = dir.path().join("c.osm.pbf");
    write_two_nodes(&a, 110_000_000);
    write_two_nodes(&b, 110_000_000);
    write_two_nodes(&c, 999_000_000);

    let same = CliInvoker::new()
        .args(["diff", "-q", a.to_str().unwrap(), b.to_str().unwrap()])
        .run();
    assert_eq!(code(&same), EXIT_SUCCESS, "identical files must exit 0");

    let differ = CliInvoker::new()
        .args(["diff", "-q", a.to_str().unwrap(), c.to_str().unwrap()])
        .run();
    assert_eq!(
        code(&differ),
        EXIT_ANSWER_NEGATIVE,
        "differences found is the semantic answer and stays 1; stderr:\n{}",
        differ.stderr_str(),
    );

    let missing = dir.path().join("does-not-exist.osm.pbf");
    let failed = CliInvoker::new()
        .args(["diff", "-q", a.to_str().unwrap(), missing.to_str().unwrap()])
        .run();
    assert_eq!(
        code(&failed),
        EXIT_FAILED,
        "a command that could not run must not share a code with 'files differ'; stderr:\n{}",
        failed.stderr_str(),
    );
}

/// `inspect --indexed` answers a yes/no question. "No" is 1; a broken
/// invocation is 2.
#[test]
fn inspect_indexed_answer_and_failure_differ() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("in.osm.pbf");
    write_two_nodes(&input, 110_000_000);

    let indexed = CliInvoker::new()
        .args(["inspect", "--indexed", input.to_str().unwrap()])
        .run();
    assert_eq!(code(&indexed), EXIT_SUCCESS);

    let missing = dir.path().join("nope.osm.pbf");
    let failed = CliInvoker::new()
        .args(["inspect", "--indexed", missing.to_str().unwrap()])
        .run();
    assert_eq!(
        code(&failed),
        EXIT_FAILED,
        "a missing input is a failure, not a 'not indexed' answer; stderr:\n{}",
        failed.stderr_str(),
    );
}

/// `inspect --show` on an element that is not in the file is the
/// negative answer, not a failure.
#[test]
fn inspect_show_missing_element_is_the_negative_answer() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("in.osm.pbf");
    write_two_nodes(&input, 110_000_000);

    let found = CliInvoker::new()
        .args(["inspect", input.to_str().unwrap(), "--show", "n1"])
        .run();
    assert_eq!(code(&found), EXIT_SUCCESS);

    let absent = CliInvoker::new()
        .args(["inspect", input.to_str().unwrap(), "--show", "n404"])
        .run();
    assert_eq!(
        code(&absent),
        EXIT_ANSWER_NEGATIVE,
        "element not found is the answer, not an error; stderr:\n{}",
        absent.stderr_str(),
    );

    let malformed = CliInvoker::new()
        .args(["inspect", input.to_str().unwrap(), "--show", "zzz"])
        .run();
    assert_eq!(
        code(&malformed),
        EXIT_FAILED,
        "an unparseable element spec is a failure; stderr:\n{}",
        malformed.stderr_str(),
    );
}

// ---------------------------------------------------------------------------
// inspect flag combinations that used to be silently discarded
// ---------------------------------------------------------------------------

/// `--show` returned early before any other flag was examined, so
/// `inspect --show n1 --json --blocks --extended` ran `show_element` and
/// threw the rest away without a word. Each of these must now be
/// rejected, and the message must name the offending flag so the user
/// can act on it.
#[test]
fn inspect_show_rejects_every_other_mode_flag() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("in.osm.pbf");
    write_two_nodes(&input, 110_000_000);

    for extra in [
        vec!["--json"],
        vec!["--blocks"],
        vec!["--extended"],
        vec!["--id-ranges"],
        vec!["--locations"],
        vec!["--anomalies"],
        vec!["--indexed"],
        vec!["--nodes"],
        vec!["--get", "header.bbox"],
        vec!["-j", "2"],
        vec!["--force"],
    ] {
        let out = CliInvoker::new()
            .args(["inspect", input.to_str().unwrap(), "--show", "n1"])
            .args(&extra)
            .run();
        assert!(
            !out.status.success(),
            "inspect --show {extra:?} must be rejected, not silently ignored; \
             stdout:\n{}\nstderr:\n{}",
            out.stdout_str(),
            out.stderr_str(),
        );
        let stderr = out.stderr_str();
        assert!(
            stderr.contains(extra[0]),
            "the rejection must name {:?}; stderr:\n{stderr}",
            extra[0],
        );
    }
}

/// `--force` and `-j/--jobs` are wired only into the `--nodes` scan.
/// Passing them to any other inspect mode did nothing and said nothing.
#[test]
fn inspect_force_and_jobs_require_nodes() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("in.osm.pbf");
    write_two_nodes(&input, 110_000_000);

    for extra in [vec!["--force"], vec!["-j", "2"]] {
        let out = CliInvoker::new()
            .args(["inspect", input.to_str().unwrap(), "--blocks"])
            .args(&extra)
            .run();
        assert!(
            !out.status.success(),
            "inspect --blocks {extra:?} must be rejected: the flag reaches only \
             the --nodes scan; stdout:\n{}\nstderr:\n{}",
            out.stdout_str(),
            out.stderr_str(),
        );
    }

    // ... and both stay valid where they actually do something.
    CliInvoker::new()
        .args([
            "inspect",
            input.to_str().unwrap(),
            "--nodes",
            "--force",
            "-j",
            "2",
        ])
        .assert_success();
}
