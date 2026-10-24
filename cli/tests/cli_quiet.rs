//! `-q, --quiet`, the one global flag that changes what a run prints
//! (`reference/cli-reference.md`, "Quiet"): the exit code is the answer.
//!
//! - Notes, progress and summaries go; warnings and the `Error:` line stay,
//!   since they explain the exit code.
//! - What a command writes to `-o` is untouched, and so is a report on stdout,
//!   except `diff`'s text and `check`'s report, which a quiet run drops.
//! - The bench commands' key=value blocks stay as they are.
//! - It is accepted before or after the subcommand.
//!
//! Every output-writing command's quiet run is pinned by
//! `cli_output_contract::a_quiet_run_prints_only_warnings_and_writes_the_same_outputs`
//! and the refusals beside `-q` by `cli_flag_contract`'s table; this file
//! covers the kinds of line, the commands without an output, and the verdict
//! commands.

#![allow(clippy::unwrap_used)]

use std::ffi::OsString;
use std::path::PathBuf;

use crate::common::{TestWay, generate_nodes, write_test_pbf_sorted};
use crate::invoker::{CliInvoker, CliOutput};
use tempfile::TempDir;

/// The note a forced run on unindexed input prints.
const FORCED_NOTE: &str = "Input contains blobs without indexdata";

struct Fx {
    dir: TempDir,
}

impl Fx {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        crate::common::write_base_pbf(&p.join("base.osm.pbf"));
        crate::common::write_changed_pbf(&p.join("changed.osm.pbf"));
        crate::common::write_synthetic_indexed_input(&p.join("geo.osm.pbf"));
        CliInvoker::new()
            .arg("degrade")
            .arg(p.join("base.osm.pbf"))
            .arg("-o")
            .arg(p.join("unindexed.osm.pbf"))
            .arg("--strip-indexdata")
            .assert_success();
        // A way referencing node 99, which the file does not hold.
        write_test_pbf_sorted(
            &p.join("dangling.osm.pbf"),
            &generate_nodes(2, 1),
            &[TestWay {
                id: 1,
                refs: vec![1, 2, 99],
                tags: vec![],
                meta: None,
            }],
            &[],
        );
        Self { dir }
    }

    /// `words` with each `@name` replaced by the fixture or output path
    /// `name` under the fixture directory.
    fn args(&self, words: &[&str]) -> Vec<OsString> {
        words
            .iter()
            .map(|word| match word.strip_prefix('@') {
                Some(name) => self.dir.path().join(name).into_os_string(),
                None => OsString::from(word),
            })
            .collect()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

/// Run `args` with the current directory in a scratch directory of its own:
/// a text diff without `-o` puts its scratch there.
fn run(args: &[OsString]) -> CliOutput {
    let cwd = TempDir::new().unwrap();
    CliInvoker::new().current_dir(cwd.path()).args(args).run()
}

/// `args` with `-q` before the subcommand (`leading`) or after everything.
fn quiet(args: &[OsString], leading: bool) -> Vec<OsString> {
    let mut out = args.to_vec();
    if leading {
        out.insert(0, "-q".into());
    } else {
        out.push("-q".into());
    }
    out
}

fn place(leading: bool) -> &'static str {
    if leading {
        "-q before the subcommand"
    } else {
        "-q after it"
    }
}

/// A forced run on unindexed input prints a note and a summary; quiet, it
/// prints nothing and writes the same output.
#[test]
fn a_quiet_run_drops_notes_and_summaries() {
    let fx = Fx::new();
    // getid rather than repack: repack on a fixture this small warns that
    // its element cap never fired, and a quiet run rightly keeps warnings.
    let plain = run(&fx.args(&[
        "getid",
        "@unindexed.osm.pbf",
        "n1",
        "--force",
        "-o",
        "@plain.osm.pbf",
    ]));
    assert!(plain.status.success(), "{}", plain.stderr_str());
    let stderr = plain.stderr_str();
    assert!(
        stderr.contains(&format!("Note: {FORCED_NOTE}")),
        "the run without -q must print the forced-run note; stderr:\n{stderr}"
    );
    assert!(
        stderr.lines().any(|line| !line.starts_with("Note: ")),
        "the run without -q must print a summary; stderr:\n{stderr}"
    );
    for leading in [true, false] {
        let name = format!("@quiet-{leading}.osm.pbf");
        let args = fx.args(&[
            "getid",
            "@unindexed.osm.pbf",
            "n1",
            "--force",
            "-o",
            name.as_str(),
        ]);
        let out = run(&quiet(&args, leading));
        let label = place(leading);
        assert_eq!(out.status.code(), Some(0), "{label}: {}", out.stderr_str());
        assert!(
            out.stderr.is_empty(),
            "{label}: printed:\n{}",
            out.stderr_str()
        );
        assert!(out.stdout.is_empty(), "{label}: {}", out.stdout_str());
        assert_eq!(
            std::fs::read(fx.path(&format!("quiet-{leading}.osm.pbf"))).unwrap(),
            std::fs::read(fx.path("plain.osm.pbf")).unwrap(),
            "{label}: the output differs from the run without -q"
        );
    }
}

/// A warning explains the run, so a quiet run keeps it, and keeps nothing
/// else.
#[test]
fn a_quiet_run_keeps_warnings() {
    let fx = Fx::new();
    let args = fx.args(&[
        "build-geocode-index",
        "@geo.osm.pbf",
        "--force",
        "-o",
        "@index",
    ]);
    let out = run(&quiet(&args, false));
    let stderr = out.stderr_str();
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(
        stderr.contains("Warning: build-geocode-index --force also grants --overwrite"),
        "the --force warning must survive -q; stderr:\n{stderr}"
    );
    for line in stderr.lines() {
        assert!(
            line.starts_with("Warning: "),
            "a quiet run printed a line that is not a warning: {line:?}; stderr:\n{stderr}"
        );
    }
    assert!(fx.path("index").is_dir(), "the index was not written");
}

/// A failure is still reported, whichever side of the subcommand `-q` is
/// on, and so is a usage error.
#[test]
fn a_quiet_run_keeps_the_error_line() {
    let fx = Fx::new();
    for leading in [true, false] {
        let args = fx.args(&["sort", "@no-such-input.osm.pbf", "-o", "@sorted.osm.pbf"]);
        let out = run(&quiet(&args, leading));
        let stderr = out.stderr_str();
        let label = place(leading);
        assert_eq!(out.status.code(), Some(2), "{label}: {stderr}");
        assert!(
            stderr.starts_with("Error: ") && stderr.contains("no-such-input"),
            "{label}: the error must be printed, naming the input; stderr:\n{stderr}"
        );
    }
    let out = run(&quiet(&fx.args(&["sort"]), true));
    assert_eq!(out.status.code(), Some(2));
    assert!(
        !out.stderr.is_empty(),
        "a usage error must be printed under -q"
    );
}

/// `diff`, `check` and `inspect --indexed` answer by exit code alone: a
/// quiet run prints nothing on either stream and exits as the run without
/// `-q` does.
#[test]
fn quiet_verdict_commands_answer_by_exit_code_alone() {
    let fx = Fx::new();
    let cases: [(&[&str], i32); 7] = [
        (&["diff", "@base.osm.pbf", "@base.osm.pbf"], 0),
        (&["diff", "@base.osm.pbf", "@changed.osm.pbf"], 1),
        (&["diff", "-j", "1", "@base.osm.pbf", "@changed.osm.pbf"], 1),
        (&["check", "@base.osm.pbf"], 0),
        (&["check", "--refs", "@dangling.osm.pbf"], 1),
        (&["inspect", "--indexed", "@base.osm.pbf"], 0),
        (&["inspect", "--indexed", "@unindexed.osm.pbf"], 1),
    ];
    for (words, code) in cases {
        let args = fx.args(words);
        let plain = run(&args);
        assert_eq!(
            plain.status.code(),
            Some(code),
            "{words:?} without -q; stderr:\n{}",
            plain.stderr_str()
        );
        assert!(
            !plain.stdout.is_empty(),
            "{words:?} without -q must print a report, or -q proves nothing"
        );
        for leading in [true, false] {
            let out = run(&quiet(&args, leading));
            let label = format!("{words:?}, {}", place(leading));
            assert_eq!(
                out.status.code(),
                Some(code),
                "{label}; stderr:\n{}",
                out.stderr_str()
            );
            assert!(
                out.stdout.is_empty(),
                "{label} wrote to stdout:\n{}",
                out.stdout_str()
            );
            assert!(
                out.stderr.is_empty(),
                "{label} wrote to stderr:\n{}",
                out.stderr_str()
            );
        }
    }
}

/// An OSC diff answers like a text diff: exit 1 when the inputs differ, 0
/// when they do not, quiet or not, and it writes its `-o` either way.
#[test]
fn osc_diff_exits_like_a_text_diff() {
    let fx = Fx::new();
    for (new, code) in [("@base.osm.pbf", 0), ("@changed.osm.pbf", 1)] {
        for q in [false, true] {
            let name = format!("@out-{code}-{q}.osc");
            let mut words = vec![
                "diff",
                "--format",
                "osc",
                "@base.osm.pbf",
                new,
                "-o",
                name.as_str(),
            ];
            if q {
                words.push("-q");
            }
            let out = run(&fx.args(&words));
            assert_eq!(
                out.status.code(),
                Some(code),
                "{words:?}; stderr:\n{}",
                out.stderr_str()
            );
            assert!(
                fx.path(&name[1..]).exists(),
                "{words:?} published no output"
            );
            if q {
                assert!(
                    out.stderr.is_empty(),
                    "{words:?} wrote to stderr:\n{}",
                    out.stderr_str()
                );
            }
        }
    }
}

/// A quiet `inspect --indexed` prints no verdict, so `--json`, which only
/// shapes it, is refused beside `-q`.
#[test]
fn quiet_inspect_indexed_refuses_json() {
    let fx = Fx::new();
    for leading in [true, false] {
        let args = fx.args(&["inspect", "--indexed", "--json", "@base.osm.pbf"]);
        let out = run(&quiet(&args, leading));
        assert_eq!(
            out.status.code(),
            Some(2),
            "{}: {}",
            place(leading),
            out.stderr_str()
        );
        assert!(
            out.stderr_str()
                .contains("'--quiet' cannot be used with '--json'"),
            "{}: {}",
            place(leading),
            out.stderr_str()
        );
    }
}

/// A report on stdout is the command's output, not a summary: `-q` leaves it
/// byte-identical and silences only stderr (`inspect tags`' count of
/// distinct values included).
#[test]
fn a_quiet_run_leaves_a_stdout_report_alone() {
    let fx = Fx::new();
    let commands: [&[&str]; 3] = [
        &["inspect", "@base.osm.pbf"],
        &["inspect", "tags", "@base.osm.pbf"],
        &["digest", "@base.osm.pbf"],
    ];
    for words in commands {
        let args = fx.args(words);
        let plain = run(&args);
        assert!(plain.status.success(), "{words:?}: {}", plain.stderr_str());
        assert!(!plain.stdout.is_empty(), "{words:?} printed no report");
        if words[1] == "tags" {
            assert!(
                plain.stderr_str().contains("distinct tag values"),
                "{words:?} without -q must print its count; stderr:\n{}",
                plain.stderr_str()
            );
        }
        for leading in [true, false] {
            let out = run(&quiet(&args, leading));
            let label = format!("{words:?}, {}", place(leading));
            assert!(out.status.success(), "{label}: {}", out.stderr_str());
            assert_eq!(
                out.stdout_str(),
                plain.stdout_str(),
                "{label}: the stdout report changed"
            );
            assert!(
                out.stderr.is_empty(),
                "{label} wrote to stderr:\n{}",
                out.stderr_str()
            );
        }
    }
}

/// The bench commands' key=value blocks are left as they are: brokkr reads
/// them. Nothing else reaches stderr.
#[test]
fn a_quiet_bench_run_keeps_its_key_value_block() {
    let fx = Fx::new();
    let commands: [&[&str]; 2] = [
        &["bench-read", "--mode", "pipelined", "@base.osm.pbf"],
        &["bench-write", "@base.osm.pbf"],
    ];
    for words in commands {
        let out = run(&quiet(&fx.args(words), false));
        let stderr = out.stderr_str();
        assert!(out.status.success(), "{words:?}: {stderr}");
        for key in ["elapsed_ms=", "nodes=300", "ways=40", "relations=5"] {
            assert!(
                stderr.lines().any(|line| line.starts_with(key)),
                "{words:?}: -q dropped {key}; stderr:\n{stderr}"
            );
        }
        for line in stderr.lines() {
            assert!(
                line.split_once('=').is_some_and(|(key, _)| key
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'_')),
                "{words:?}: a quiet bench run printed {line:?}"
            );
        }
    }
}

/// `getid -v` with `-q` is refused (see `cli_flag_contract`); without `-q`
/// the verbose lists still print, and a quiet `getid` prints nothing.
#[test]
fn quiet_getid_prints_nothing_and_verbose_getid_still_lists() {
    let fx = Fx::new();
    let verbose = run(&fx.args(&["getid", "@base.osm.pbf", "-o", "@v.osm.pbf", "-v", "n1"]));
    let stderr = verbose.stderr_str();
    assert!(verbose.status.success(), "{stderr}");
    assert!(
        stderr.contains("Requested IDs:") && stderr.contains("Found all requested objects."),
        "getid -v must list the IDs; stderr:\n{stderr}"
    );
    let out = run(&quiet(
        &fx.args(&["getid", "@base.osm.pbf", "-o", "@q.osm.pbf", "n1"]),
        false,
    ));
    assert!(out.status.success(), "{}", out.stderr_str());
    assert!(
        out.stderr.is_empty(),
        "a quiet getid printed:\n{}",
        out.stderr_str()
    );
}
