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
//!   3 - a requested platform I/O feature is unavailable at setup
//!  64 - the command line was refused (clap, or the CLI's own option
//!       validation), printed by clap
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

use crate::common::{TestNode, write_test_pbf_sorted};
use crate::invoker::{CliInvoker, EXIT_FAILED, EXIT_USAGE};

const EXIT_SUCCESS: i32 = 0;
const EXIT_ANSWER_NEGATIVE: i32 = 1;

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

fn code(out: &crate::invoker::CliOutput) -> i32 {
    out.status.code().expect("child exited with a code")
}

/// The three bands, on one command. `diff` is the case the split exists
/// for: identical files, differing files, and a missing input must land
/// on three different codes.
#[test]
fn diff_separates_identical_differing_and_failure() {
    let dir = crate::common::test_scratch();
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

/// A closed stderr (`pbfhogg sort ... 2>&1 | head`) stops the diagnostics and
/// nothing else. `eprintln!` used to panic on the broken pipe and abort the
/// command with 101 halfway through; `sort` writes progress lines to stderr
/// before and while it writes its output, so it is the case that used to die.
#[test]
fn a_closed_stderr_does_not_kill_the_run() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let output = dir.path().join("sorted.osm.pbf");
    let nodes = crate::common::generate_nodes(50, 1);
    crate::common::write_indexed_pbf(&input, &nodes, &[], &[]);

    let out = CliInvoker::new()
        .stderr_closed()
        .args([
            "sort",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ])
        .run();
    assert_eq!(
        code(&out),
        EXIT_SUCCESS,
        "a closed stderr must not fail sort"
    );
    let read = crate::common::read_all_elements_with_coords(&output);
    assert_eq!(read.nodes.len(), nodes.len(), "the output must be complete");
}

/// The command's own answer survives a closed stderr: differences found is
/// still 1.
///
/// Deliberately without `-q`: quiet mode writes nothing to stderr, so it
/// would pass whether or not a closed stderr is survivable. Without it the
/// summary goes to the closed stderr after the diff itself went to stdout.
#[test]
fn a_closed_stderr_keeps_the_differences_status() {
    let dir = crate::common::test_scratch();
    let a = dir.path().join("a.osm.pbf");
    let c = dir.path().join("c.osm.pbf");
    write_two_nodes(&a, 110_000_000);
    write_two_nodes(&c, 999_000_000);

    let out = CliInvoker::new()
        .stderr_closed()
        .args(["diff", a.to_str().unwrap(), c.to_str().unwrap()])
        .run();
    assert_eq!(code(&out), EXIT_ANSWER_NEGATIVE);
}

/// A failure is still a failure when nobody can read its message.
#[test]
fn a_closed_stderr_keeps_the_failure_status() {
    let dir = crate::common::test_scratch();
    let missing = dir.path().join("does-not-exist.osm.pbf");
    let output = dir.path().join("out.osm.pbf");

    let out = CliInvoker::new()
        .stderr_closed()
        .args([
            "sort",
            missing.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ])
        .run();
    assert_eq!(code(&out), EXIT_FAILED);
}

/// `inspect --indexed` answers a yes/no question. "No" is 1; a broken
/// invocation is 2.
#[test]
fn inspect_indexed_answer_and_failure_differ() {
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    assert!(
        absent.stderr.is_empty(),
        "a negative answer must not emit diagnostics; stderr:\n{}",
        absent.stderr_str(),
    );

    let malformed = CliInvoker::new()
        .args(["inspect", input.to_str().unwrap(), "--show", "zzz"])
        .run();
    assert_eq!(
        code(&malformed),
        EXIT_USAGE,
        "an unparseable element spec is a usage error; stderr:\n{}",
        malformed.stderr_str(),
    );
}

/// The usage-error status is its own: every way the command line is refused
/// exits 64 in clap's rendering (`runtime_failures_stay_2` is the other
/// side). brokkr reads the usage status from `SUB --hel --help` and the
/// flag-support probes from `SUB --FLAG --help`, so those shapes are pinned
/// too.
#[test]
fn usage_errors_exit_64() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_two_nodes(&input, 110_000_000);
    let input = input.to_str().unwrap();
    let fresh = dir.path().join("fresh.osm.pbf");
    let fresh = fresh.to_str().unwrap();

    // (argv, why) - refused as usage errors.
    let usage: &[(&[&str], &str)] = &[
        (&[], "no subcommand: clap prints help on stderr"),
        (&["sort", "--no-such-flag"], "unknown flag"),
        (&["sort", "--hel", "--help"], "brokkr's control probe"),
        (
            &["inspect", "--overwrite", "--help"],
            "a flag the subcommand lacks",
        ),
        (&["sort", input], "a missing required argument"),
        (&["inspect", input, "--show", "zzz"], "a malformed value"),
        (
            &["export", input, "-O"],
            "a flag rule clap expresses (-O needs -o)",
        ),
        (
            &[
                "degrade",
                input,
                "-o",
                fresh,
                "--unsort-intra",
                "--block-cap",
                "1",
            ],
            "a flag rule the CLI checks before dispatch",
        ),
        (
            &["cat", input, input, "-o", fresh, "--reindex"],
            "a rule a dispatch arm checks",
        ),
        (
            &["inspect", "tags", input, "=primary"],
            "an invalid expression, checked in the arm",
        ),
    ];
    for (args, why) in usage {
        let out = CliInvoker::new().args(*args).run();
        let stderr = out.stderr_str();
        assert_eq!(
            code(&out),
            EXIT_USAGE,
            "{why}: {args:?} must exit {EXIT_USAGE}; stderr:\n{stderr}"
        );
        assert!(
            !stderr.contains("Error: "),
            "{why}: a usage error is printed by clap, not as Error:; stderr:\n{stderr}"
        );
        assert!(
            out.stdout.is_empty(),
            "{why}: a usage error prints nothing on stdout"
        );
    }
    assert!(
        !std::path::Path::new(fresh).exists(),
        "a usage error must not create an output"
    );

    // Help asked for, and a flag the subcommand has, exit 0.
    for args in [
        &["--help"][..],
        &["--version"],
        &["sort", "--help"],
        &["sort", "--overwrite", "--help"],
        &["sort", "--staging-dir=.", "--help"],
    ] {
        let out = CliInvoker::new().args(args).run();
        assert_eq!(code(&out), EXIT_SUCCESS, "{args:?}: {}", out.stderr_str());
    }
}

/// What the command refuses or fails at, once the command line was accepted,
/// keeps the runtime status and is printed as `Error:`, a value the library
/// itself refuses included.
#[test]
fn runtime_failures_stay_2() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_two_nodes(&input, 110_000_000);
    let input = input.to_str().unwrap();
    let existing = dir.path().join("existing.osm.pbf");
    std::fs::write(&existing, b"occupied").unwrap();
    let existing = existing.to_str().unwrap();
    let missing = dir.path().join("missing.osm.pbf");
    let missing = missing.to_str().unwrap();
    let fresh = dir.path().join("fresh.osm.pbf");
    let fresh = fresh.to_str().unwrap();

    // (argv, why) - refused or failed by the command: the runtime status.
    let runtime: &[(&[&str], &str)] = &[
        (&["sort", missing, "-o", fresh], "a missing input"),
        (&["sort", input, "-o", existing], "an existing output"),
        (
            &["sort", input, "-o", input, "-O"],
            "an output aliasing its input",
        ),
        (
            &["renumber", input, "-o", fresh, "--start-id", "0"],
            "a value the library refuses",
        ),
    ];
    for (args, why) in runtime {
        let out = CliInvoker::new().args(*args).run();
        let stderr = out.stderr_str();
        assert_eq!(
            code(&out),
            EXIT_FAILED,
            "{why}: {args:?} must exit {EXIT_FAILED}; stderr:\n{stderr}"
        );
        assert!(
            stderr.contains("Error: "),
            "{why}: a runtime failure is printed as Error:; stderr:\n{stderr}"
        );
    }
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
