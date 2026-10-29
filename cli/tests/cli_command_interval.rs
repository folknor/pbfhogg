//! The CLI's command interval: one untagged `CLI_COMMAND_START` /
//! `CLI_COMMAND_END` pair on the sidecar FIFO's process stream per
//! dispatched command, bracketing every line the command's library work
//! emits and preceding the end-of-run counters (`cli/src/host.rs`,
//! `CommandInterval`). A regular file stands in for the FIFO, as in the
//! other marker tests.

#![allow(clippy::unwrap_used)]

use crate::invoker::CliInvoker;
use std::path::Path;

const START: &str = "CLI_COMMAND_START";
const END: &str = "CLI_COMMAND_END";

/// Run `args` with the marker file attached; the exit status and the lines
/// written, each split into its timestamp and body.
fn run_with_markers(dir: &Path, label: &str, args: &[&str]) -> (i32, Vec<(u64, String)>) {
    let markers = dir.join(format!("{label}.markers"));
    std::fs::write(&markers, []).unwrap();
    let out = CliInvoker::new()
        .env("BROKKR_MARKER_FIFO", &markers)
        .args(args)
        .run();
    let code = out.status.code().unwrap();
    let lines = std::fs::read_to_string(&markers)
        .unwrap()
        .lines()
        .map(|line| {
            let (ts, body) = line.split_once(' ').unwrap();
            (ts.parse().unwrap(), body.to_owned())
        })
        .collect();
    (code, lines)
}

fn positions(lines: &[(u64, String)], name: &str) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, (_, body))| body == name)
        .map(|(i, _)| i)
        .collect()
}

/// Exactly one pair, START the first line written, every marker of the
/// command's library work inside it, and only counters after END.
#[track_caller]
fn assert_one_interval(lines: &[(u64, String)], label: &str) {
    let starts = positions(lines, START);
    let ends = positions(lines, END);
    assert_eq!(starts, [0], "{label}: one START, first\n{lines:?}");
    assert_eq!(ends.len(), 1, "{label}: one END\n{lines:?}");
    let end = ends[0];
    assert!(lines[end].0 >= lines[0].0, "{label}: END before START");
    for (_, body) in &lines[end + 1..] {
        assert!(
            body.starts_with('@'),
            "{label}: only end-of-run counters follow END, got {body}\n{lines:?}"
        );
    }
}

#[test]
fn a_dispatched_command_marks_one_interval_around_its_work() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    crate::common::write_base_pbf(&input);
    let output = dir.path().join("out.osm.pbf");
    let args = [
        "cat",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ];
    let (code, lines) = run_with_markers(dir.path(), "cat", &args);
    assert_eq!(code, 0, "{lines:?}");
    assert_one_interval(&lines, "cat");
    // The library's own phases and its publication lie inside.
    let end = positions(&lines, END)[0];
    let publish = positions(&lines, "PUBLISH_END");
    assert!(
        !publish.is_empty() && publish.iter().all(|&i| 0 < i && i < end),
        "{lines:?}"
    );
    assert!(
        lines[end + 1..]
            .iter()
            .any(|(_, b)| b.starts_with("@writer_")),
        "the writer counters follow END\n{lines:?}"
    );
}

/// A refusal raised inside the dispatch (an existing output without
/// `--overwrite`) still closes its interval.
#[test]
fn a_refusal_inside_the_dispatch_closes_its_interval() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    crate::common::write_base_pbf(&input);
    let output = dir.path().join("out.osm.pbf");
    std::fs::write(&output, b"kept").unwrap();
    let args = [
        "cat",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ];
    let (code, lines) = run_with_markers(dir.path(), "refused", &args);
    assert_eq!(code, 2, "{lines:?}");
    assert_one_interval(&lines, "refused");
    assert_eq!(std::fs::read(&output).unwrap(), b"kept");
}

/// A command line refused before dispatch, help and the version mark no
/// interval.
#[test]
fn no_interval_before_dispatch() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    crate::common::write_base_pbf(&input);
    let cases: [(&str, &[&str]); 3] = [
        ("usage", &["cat", "--no-such-flag"]),
        ("help", &["cat", "--help"]),
        ("version", &["--version"]),
    ];
    for (label, args) in cases {
        let (_, lines) = run_with_markers(dir.path(), label, args);
        assert!(
            lines.iter().all(|(_, b)| b != START && b != END),
            "{label}: {lines:?}"
        );
    }
}
