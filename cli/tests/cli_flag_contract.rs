//! The flag contract, enforced by tables (`reference/cli-reference.md`,
//! "Flag rules", "`-j/--jobs`" and "`--force`"):
//!
//! - A flag the command line itself makes ineffective is refused, as a usage
//!   error (exit 2), and so is a malformed flag value. Both happen before any
//!   input is opened: every row here names inputs that do not exist, so a
//!   refusal that waited for the input would report the missing input
//!   instead.
//! - `-j/--jobs` means the same on every command that has it: `0` automatic,
//!   `1` sequential, `N` a cap. Which values each command accepts is one
//!   table, read against the commands' own `--help`.
//! - `--force` is offered only by commands that refuse unindexed input, and
//!   each of them refuses through the shared indexdata gate and proceeds with
//!   `--force`. The commands are read from their own `--help`.

#![allow(clippy::unwrap_used, clippy::unwrap_in_result)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::invoker::{CliInvoker, CliOutput, subcommand_paths};
use tempfile::TempDir;

/// In the name of every input that does not exist.
const MISSING: &str = "no-such-input";

/// The shared indexdata gate's advice, which no other refusal prints.
const GATE: &str = "Generate an indexed PBF first";

macro_rules! ensure {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            return Err(format!($($arg)*));
        }
    };
}

fn run(args: &[OsString]) -> CliOutput {
    CliInvoker::new().args(args).run()
}

/// The words of an invocation, with `IN`, `IN2`, `OSC` and `OUT` replaced by
/// paths under `dir` that do not exist (`OUT`'s directory does).
fn argv(dir: &Path, words: &[&str]) -> Vec<OsString> {
    words
        .iter()
        .map(|word| match *word {
            "IN" => dir.join(format!("{MISSING}.osm.pbf")).into(),
            "IN2" => dir.join(format!("{MISSING}-2.osm.pbf")).into(),
            "OSC" => dir.join(format!("{MISSING}.osc.gz")).into(),
            "OUT" => dir.join("out.osm.pbf").into(),
            // An OSC output name must end `.osc` or `.osc.gz`.
            "OSCOUT" => dir.join("out.osc").into(),
            "DIR" => dir.as_os_str().to_owned(),
            other => OsString::from(other),
        })
        .collect()
}

/// A refusal decided by the arguments alone: exit 2, the expected words, and
/// not a word about the (missing) input.
fn refused_before_input(out: &CliOutput, expect: &str) -> Result<(), String> {
    let stderr = out.stderr_str();
    ensure!(
        out.status.code() == Some(2),
        "exit {:?}, wanted 2; stderr:\n{stderr}",
        out.status.code()
    );
    ensure!(
        stderr.contains(expect),
        "wanted {expect:?} in stderr:\n{stderr}"
    );
    ensure!(
        !stderr.contains(MISSING),
        "the refusal came after the input was opened; stderr:\n{stderr}"
    );
    Ok(())
}

/// Arguments accepted: the run got as far as the input, which does not exist.
fn reached_input(out: &CliOutput) -> Result<(), String> {
    let stderr = out.stderr_str();
    ensure!(
        stderr.contains(MISSING),
        "the arguments were refused, or the input never opened; exit {:?}, stderr:\n{stderr}",
        out.status.code()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Refusals with no input present
// ---------------------------------------------------------------------------

/// Invocation words, and a phrase the refusal must contain.
const REFUSALS: &[(&[&str], &str)] = &[
    // A flag with no effect, by clap.
    (&["export", "IN", "-O"], "--output"),
    (&["diff", "IN", "IN2", "-O"], "--output"),
    (&["cat", "IN", "-o", "OUT", "--force"], "--type"),
    // --clean without --type decodes every blob either way, so it has no
    // slower fallback for --force to grant.
    (
        &["cat", "IN", "-o", "OUT", "--clean", "version", "--force"],
        "--type",
    ),
    // Without the linux-io-uring feature the switch is refused for that
    // instead; both refusals name it.
    (&["cat", "IN", "-o", "OUT", "--io-uring"], "--io-uring"),
    (
        &["cat", "IN", "-o", "OUT", "--dedupe", "--type", "node"],
        "cannot be used with",
    ),
    (
        &["diff", "IN", "IN2", "-v", "--tmp-dir", "DIR"],
        "cannot be used with",
    ),
    (
        &["diff", "IN", "IN2", "-v", "-j", "2"],
        "cannot be used with",
    ),
    (&["diff", "IN", "IN2", "-q", "-s"], "cannot be used with"),
    (&["diff", "IN", "IN2", "-q", "-c"], "cannot be used with"),
    (&["diff", "IN", "IN2", "-q", "-v"], "cannot be used with"),
    (
        &["tags-filter", "IN", "-o", "OUT", "-R", "--remove-tags", "x"],
        "cannot be used with",
    ),
    (
        &["tags-filter", "IN", "-o", "OUT", "-R", "-j", "2", "x"],
        "cannot be used with",
    ),
    (
        &["tags-filter", "IN", "-o", "OUT", "-R", "-j", "0", "x"],
        "cannot be used with",
    ),
    (
        &[
            "tags-filter",
            "IN",
            "-o",
            "OUT",
            "--invert-match",
            "--force",
            "x",
        ],
        "cannot be used with",
    ),
    (
        &[
            "tags-filter",
            "IN",
            "-o",
            "OUT",
            "--distrust-metadata",
            "--force",
            "x",
        ],
        "cannot be used with",
    ),
    (
        &["getid", "IN", "-o", "OUT", "--remove-tags", "n1"],
        "--add-referenced",
    ),
    (
        &[
            "extract", "IN", "-o", "OUT", "-b", "0,0,1,1", "--simple", "--force",
        ],
        "cannot be used with",
    ),
    (
        &[
            "add-locations-to-ways",
            "IN",
            "-o",
            "OUT",
            "--inject-prepass",
            "--force",
        ],
        "cannot be used with",
    ),
    (
        &[
            "apply-changes",
            "IN",
            "OSC",
            "-o",
            "OUT",
            "--locations-on-ways",
            "--force",
        ],
        "cannot be used with",
    ),
    (
        &["degrade", "IN", "-o", "OUT", "--strip-indexdata", "--force"],
        "--unsort",
    ),
    (
        &[
            "degrade",
            "IN",
            "-o",
            "OUT",
            "--corrupt-indexdata",
            "--unsort",
        ],
        "cannot be used with",
    ),
    (&["inspect", "IN", "-j", "2"], "--nodes"),
    (&["inspect", "IN", "--force"], "--nodes"),
    (
        &["inspect", "IN", "--nodes", "--json"],
        "cannot be used with",
    ),
    (
        &["check", "IN", "--quiet", "--show-ids"],
        "cannot be used with",
    ),
    // `-q` is global: its conflicts hold whichever side of the subcommand
    // it is given on.
    (&["check", "IN", "-q", "--json"], "cannot be used with"),
    (
        &["check", "IN", "-q", "--max-errors", "5"],
        "cannot be used with",
    ),
    (&["-q", "check", "IN", "--json"], "cannot be used with"),
    (
        &["diff", "IN", "IN2", "-q", "-o", "OUT"],
        "cannot be used with",
    ),
    (&["-q", "diff", "IN", "IN2", "-c"], "cannot be used with"),
    (&["-q", "diff", "IN", "IN2", "-s"], "cannot be used with"),
    // A quiet text diff only counts, which writes no scratch.
    (
        &["diff", "IN", "IN2", "-q", "--tmp-dir", "DIR"],
        "'--quiet' cannot be used with '--tmp-dir'",
    ),
    (
        &["-q", "diff", "IN", "IN2", "-j", "4", "--tmp-dir", "DIR"],
        "'--quiet' cannot be used with '--tmp-dir'",
    ),
    (
        &["-q", "diff", "IN", "IN2", "-o", "OUT", "--tmp-dir", "DIR"],
        "'--quiet' cannot be used with '--output'",
    ),
    (
        &["getid", "IN", "-o", "OUT", "-v", "-q", "n1"],
        "cannot be used with",
    ),
    (
        &["-q", "getid", "IN", "-o", "OUT", "-v", "n1"],
        "cannot be used with",
    ),
    // Nothing to work on, by clap.
    (&["getid", "IN", "-o", "OUT"], "required arguments"),
    (&["getparents", "IN", "-o", "OUT"], "required arguments"),
    (&["tags-filter", "IN", "-o", "OUT"], "required arguments"),
    (&["degrade", "IN", "-o", "OUT"], "required arguments"),
    // A flag with no effect, by the command's validation step.
    (
        &["diff", "IN", "IN2", "-j", "1", "--tmp-dir", "DIR"],
        "--tmp-dir",
    ),
    (
        &["diff", "IN", "IN2", "--format", "osc"],
        "--output is required",
    ),
    (
        &["diff", "IN", "IN2", "--format", "osc", "-o", "OUT", "-c"],
        "--format osc",
    ),
    (
        &["diff", "IN", "IN2", "--increment-version"],
        "--format osc",
    ),
    (
        &[
            "add-locations-to-ways",
            "IN",
            "-o",
            "OUT",
            "--index-type",
            "external",
            "--force",
        ],
        "--index-type external",
    ),
    (
        &[
            "add-locations-to-ways",
            "IN",
            "-o",
            "OUT",
            "--index-type",
            "dense",
        ],
        "sparse",
    ),
    (
        &[
            "degrade",
            "IN",
            "-o",
            "OUT",
            "--unsort-intra",
            "--block-cap",
            "1",
        ],
        "--block-cap",
    ),
    (&["check", "IN", "--refs", "--full"], "--ids"),
    (&["check", "IN", "--ids", "--show-ids"], "--refs"),
    (
        &[
            "tags-filter",
            "OSC",
            "--input-kind",
            "osc",
            "-o",
            "OUT",
            "--compression",
            "zstd",
            "x",
        ],
        "OSC mode",
    ),
    (
        &[
            "tags-filter",
            "OSC",
            "--input-kind",
            "osc",
            "-o",
            "OUT",
            "--force",
            "x",
        ],
        "OSC mode",
    ),
    (
        &[
            "tags-filter",
            "OSC",
            "--input-kind",
            "osc",
            "-o",
            "OUT",
            "-j",
            "0",
            "x",
        ],
        "OSC mode",
    ),
    (
        &[
            "tags-filter",
            "OSC",
            "--input-kind",
            "osc",
            "-o",
            "OUT",
            "--invert-match",
            "x",
        ],
        "OSC mode",
    ),
    (
        &[
            "tags-filter",
            "OSC",
            "--input-kind",
            "osc",
            "-o",
            "OUT",
            "--generator",
            "g",
            "x",
        ],
        "OSC mode",
    ),
    // A malformed value.
    (
        &["renumber", "IN", "-o", "OUT", "--start-id", "1,2"],
        "--start-id",
    ),
    (
        &["time-filter", "IN", "-o", "OUT", "yesterday"],
        "UNIX seconds",
    ),
    (&["extract", "IN", "-o", "OUT", "--bbox", "1,2,3"], "bbox"),
    (&["export", "IN", "--bbox", "1,2,3"], "bbox"),
    (&["degrade", "IN", "-o", "OUT", "--drop-ids", "5"], "N:SEED"),
    (
        &["sort", "IN", "-o", "OUT", "--output-header", "nonsense"],
        "key=value",
    ),
    (
        &["sort", "IN", "-o", "OUT", "--compression", "lz4"],
        "compression",
    ),
    (
        &["merge-changes", "OSC", "-o", "OUT", "-j", "many"],
        "worker count",
    ),
    (
        &["apply-changes", "IN", "OSC", "-o", "OUT", "-j", "1"],
        "at least 2 workers",
    ),
    (&["tags-filter", "IN", "-o", "OUT", "=primary"], "empty"),
    (&["getid", "IN", "-o", "OUT", "x12"], "x12"),
    (&["export", "IN", "=primary"], "empty"),
    (&["inspect", "tags", "IN", "=primary"], "empty"),
    (&["inspect", "IN", "--get", "data.nonsense"], "unknown key"),
    (
        &["inspect", "IN", "--get", "header.bbox", "--blocks"],
        "cannot be used with",
    ),
    // A retired short flag names the long flag to spell.
    (&["cat", "IN", "-o", "OUT", "-c", "version"], "--clean"),
    (&["extract", "IN", "-c", "config.json"], "--config"),
    (
        &["extract", "IN", "-o", "OUT", "-b", "0,0,1,1", "-s"],
        "--simple",
    ),
    (&["renumber", "IN", "-o", "OUT", "-s", "5"], "--start-id"),
    (&["getparents", "IN", "-o", "OUT", "-s", "n1"], "--add-self"),
    (&["inspect", "IN", "-e"], "--extended"),
    (&["inspect", "tags", "IN", "-s", "name"], "--sort"),
    (
        &["tags-filter", "IN", "-o", "OUT", "-t", "x"],
        "--remove-tags",
    ),
    (
        &["tags-filter", "IN", "-o", "OUT", "-i", "x"],
        "--invert-match",
    ),
    (
        &["getid", "IN", "-o", "OUT", "-r", "-t", "n1"],
        "--remove-tags",
    ),
];

#[test]
fn every_refusal_happens_before_any_input_is_opened() {
    let dir = crate::common::test_scratch();
    let mut failures = Vec::new();
    for (words, expect) in REFUSALS {
        let out = run(&argv(dir.path(), words));
        if let Err(why) = refused_before_input(&out, expect) {
            failures.push(format!("[{}] {why}", words.join(" ")));
        }
    }
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "a refused run left {leftovers:?} behind"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The direct-I/O switches of a build that lacks them are refused while the
/// arguments are parsed, not when a reader or writer first opens.
#[test]
fn a_platform_switch_the_build_lacks_is_refused_before_any_input() {
    let dir = crate::common::test_scratch();
    for (switch, compiled) in [
        (
            "--direct-io",
            pbfhogg::write::OutputBackend::Direct.is_compiled(),
        ),
        (
            "--io-uring",
            pbfhogg::write::OutputBackend::Uring.is_compiled(),
        ),
    ] {
        let out = run(&argv(dir.path(), &["sort", "IN", "-o", "OUT", switch]));
        if compiled {
            reached_input(&out).unwrap_or_else(|why| panic!("{switch}: {why}"));
        } else {
            refused_before_input(&out, "feature").unwrap_or_else(|why| panic!("{switch}: {why}"));
        }
    }
}

// ---------------------------------------------------------------------------
// -j/--jobs
// ---------------------------------------------------------------------------

/// A command line for one `-j` row; `J` is replaced by the value.
struct JobsRow {
    label: &'static str,
    words: &'static [&'static str],
    /// Accepted for `-j 0`, `-j 1`, `-j 2`.
    accepts: [bool; 3],
}

const JOBS: &[JobsRow] = &[
    JobsRow {
        label: "tags-filter",
        words: &["tags-filter", "IN", "-o", "OUT", "x", "-j", "J"],
        accepts: [true, true, true],
    },
    JobsRow {
        label: "tags-filter -R (single pass, no pool)",
        words: &["tags-filter", "IN", "-o", "OUT", "x", "-R", "-j", "J"],
        accepts: [false, false, false],
    },
    JobsRow {
        label: "tags-filter OSC (single-threaded parse)",
        words: &[
            "tags-filter",
            "OSC",
            "--input-kind",
            "osc",
            "-o",
            "OSCOUT",
            "x",
            "-j",
            "J",
        ],
        accepts: [false, false, false],
    },
    JobsRow {
        label: "inspect --nodes",
        words: &["inspect", "IN", "--nodes", "-j", "J"],
        accepts: [true, true, true],
    },
    JobsRow {
        label: "inspect (no pool outside --nodes)",
        words: &["inspect", "IN", "-j", "J"],
        accepts: [false, false, false],
    },
    JobsRow {
        label: "inspect tags",
        words: &["inspect", "tags", "IN", "-j", "J"],
        accepts: [true, true, true],
    },
    JobsRow {
        label: "diff",
        words: &["diff", "IN", "IN2", "-j", "J"],
        accepts: [true, true, true],
    },
    JobsRow {
        label: "diff --format osc",
        words: &[
            "diff", "IN", "IN2", "--format", "osc", "-o", "OSCOUT", "-j", "J",
        ],
        accepts: [true, true, true],
    },
    JobsRow {
        label: "diff -v (sequential only)",
        words: &["diff", "IN", "IN2", "-v", "-j", "J"],
        accepts: [false, false, false],
    },
    JobsRow {
        label: "apply-changes (one worker is not validated)",
        words: &["apply-changes", "IN", "OSC", "-o", "OUT", "-j", "J"],
        accepts: [true, false, true],
    },
    JobsRow {
        label: "merge-changes",
        words: &["merge-changes", "OSC", "-o", "OSCOUT", "-j", "J"],
        accepts: [true, true, true],
    },
    JobsRow {
        label: "sort (sizes itself, has no -j)",
        words: &["sort", "IN", "-o", "OUT", "-j", "J"],
        accepts: [false, false, false],
    },
];

#[test]
fn jobs_is_accepted_or_refused_per_command_and_value() {
    let dir = crate::common::test_scratch();
    let mut failures = Vec::new();
    for row in JOBS {
        for (value, accepts) in ["0", "1", "2"].into_iter().zip(row.accepts) {
            let words: Vec<&str> = row
                .words
                .iter()
                .map(|w| if *w == "J" { value } else { w })
                .collect();
            let out = run(&argv(dir.path(), &words));
            // Accepted: the run went past argument handling and failed on the
            // missing input, which it names.
            let verdict = if accepts {
                reached_input(&out)
            } else {
                refused_before_input(&out, "")
            };
            if let Err(why) = verdict {
                failures.push(format!("[{} -j {value}] {why}", row.label));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Every command offering `-j` in its `--help` has a row, and the help text
/// is the one shared text.
#[test]
fn every_command_with_jobs_is_in_the_table_and_shares_one_help_text() {
    let mut problems = Vec::new();
    let mut texts = Vec::new();
    for path in subcommand_paths() {
        let help = help(&path);
        let Some(line) = help
            .lines()
            .position(|l| l.trim_start().starts_with("-j, --jobs"))
        else {
            continue;
        };
        let name = path.join(" ");
        if !JOBS
            .iter()
            .any(|row| row.words.join(" ").starts_with(&name))
        {
            problems.push(format!("`{name}` offers -j but has no row in JOBS"));
        }
        // The flag line and its wrapped continuation (indented past the flag
        // column), whitespace normalised: clap aligns and wraps per command,
        // and prints the default on a line of its own in long-form help.
        let lines = help.lines().skip(line);
        let mut words: Vec<&str> = Vec::new();
        for (i, l) in lines.enumerate() {
            let continuation = l.starts_with("        ") && !l.trim_start().starts_with('-');
            if i > 0 && !continuation {
                break;
            }
            words.extend(l.split_whitespace());
        }
        let text = words
            .join(" ")
            .replace(" [default: 0]", "")
            .replace("[default: 0]", "");
        texts.push((name, text));
    }
    if let Some((first_name, first)) = texts.first() {
        for (name, text) in &texts {
            if text != first {
                problems.push(format!(
                    "`{name}` -j help differs from `{first_name}`:\n  {text}\n  {first}"
                ));
            }
        }
    }
    assert!(texts.len() >= 5, "found -j on only {texts:?}");
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

// ---------------------------------------------------------------------------
// --force and the shared indexdata gate
// ---------------------------------------------------------------------------

struct Fx {
    dir: TempDir,
}

impl Fx {
    fn new() -> Self {
        let dir = crate::common::test_scratch();
        let p = dir.path();
        crate::common::write_base_pbf(&p.join("indexed.osm.pbf"));
        strip_indexdata(&p.join("indexed.osm.pbf"), &p.join("unindexed.osm.pbf"));
        crate::common::write_synthetic_indexed_input(&p.join("geo-indexed.osm.pbf"));
        strip_indexdata(&p.join("geo-indexed.osm.pbf"), &p.join("geo.osm.pbf"));
        std::fs::write(
            p.join("good.osc"),
            b"<osmChange version=\"0.6\"><create>\
              <node id=\"1000\" version=\"1\" lat=\"0.001\" lon=\"0.002\">\
              <tag k=\"amenity\" v=\"cafe\"/></node></create></osmChange>",
        )
        .unwrap();
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

fn strip_indexdata(from: &Path, to: &Path) {
    CliInvoker::new()
        .arg("degrade")
        .arg(from)
        .arg("-o")
        .arg(to)
        .arg("--strip-indexdata")
        .assert_success();
}

/// A command offering `--force`, run on an unindexed input. `U` is the
/// unindexed PBF, `G` the unindexed geocode input, `C` the OSC, `O` a fresh
/// output path.
struct ForceRow {
    command: &'static str,
    words: &'static [&'static str],
}

const FORCE: &[ForceRow] = &[
    ForceRow {
        command: "cat",
        words: &["cat", "U", "--type", "node", "-o", "O"],
    },
    ForceRow {
        command: "sort",
        words: &["sort", "U", "-o", "O"],
    },
    ForceRow {
        command: "repack",
        words: &["repack", "U", "-o", "O"],
    },
    ForceRow {
        command: "degrade",
        words: &["degrade", "U", "--strip-locations", "-o", "O"],
    },
    ForceRow {
        command: "tags-filter",
        words: &["tags-filter", "U", "highway", "-o", "O"],
    },
    ForceRow {
        command: "getid",
        words: &["getid", "U", "n1", "-o", "O"],
    },
    ForceRow {
        command: "getid",
        words: &["getid", "--invert", "U", "n1", "-o", "O"],
    },
    ForceRow {
        command: "extract",
        words: &["extract", "U", "--bbox=-1,-1,1,1", "-o", "O"],
    },
    ForceRow {
        command: "add-locations-to-ways",
        words: &[
            "add-locations-to-ways",
            "U",
            "--ignore-missing-nodes",
            "-o",
            "O",
        ],
    },
    ForceRow {
        command: "apply-changes",
        words: &["apply-changes", "U", "C", "-o", "O"],
    },
    ForceRow {
        command: "inspect",
        words: &["inspect", "U", "--nodes"],
    },
    ForceRow {
        command: "inspect tags",
        words: &["inspect", "tags", "U"],
    },
    ForceRow {
        command: "build-geocode-index",
        words: &["build-geocode-index", "G", "-o", "O"],
    },
];

fn force_argv(fx: &Fx, row: &ForceRow, n: usize, force: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = row
        .words
        .iter()
        .map(|word| match *word {
            "U" => fx.path("unindexed.osm.pbf").into(),
            "G" => fx.path("geo.osm.pbf").into(),
            "C" => fx.path("good.osc").into(),
            "O" => fx.path(&format!("out-{n}-{force}")).into(),
            other => OsString::from(other),
        })
        .collect();
    if force {
        args.push("--force".into());
    }
    args
}

/// The advice a refusal ends with, per switch.
const FORCE_ADVICE: &str = "Or pass --force";
const ALLOW_ADVICE: &str = "Or pass --allow-unindexed";

/// The modes whose `--force` is refused (they need indexdata whatever
/// `--force` says) must not advise it when they refuse unindexed input.
#[test]
fn the_gate_advises_only_a_switch_the_mode_accepts() {
    let fx = Fx::new();
    let u = fx.path("unindexed.osm.pbf");
    let g = fx.path("geo.osm.pbf");
    let osc = fx.path("good.osc");
    let out = |n: &str| fx.path(n);
    let cases: Vec<(&str, Vec<OsString>, Option<&str>)> = vec![
        (
            "altw external",
            vec![
                "add-locations-to-ways".into(),
                u.clone().into(),
                "--index-type".into(),
                "external".into(),
                "-o".into(),
                out("o1").into(),
            ],
            None,
        ),
        (
            "altw --inject-prepass",
            vec![
                "add-locations-to-ways".into(),
                u.clone().into(),
                "--inject-prepass".into(),
                "-o".into(),
                out("o2").into(),
            ],
            None,
        ),
        (
            "apply-changes --locations-on-ways",
            vec![
                "apply-changes".into(),
                u.clone().into(),
                osc.clone().into(),
                "--locations-on-ways".into(),
                "-o".into(),
                out("o3").into(),
            ],
            None,
        ),
        (
            "sort",
            vec![
                "sort".into(),
                u.clone().into(),
                "-o".into(),
                out("o4").into(),
            ],
            Some(FORCE_ADVICE),
        ),
        (
            "build-geocode-index",
            vec![
                "build-geocode-index".into(),
                g.into(),
                "-o".into(),
                out("o5").into(),
            ],
            Some(ALLOW_ADVICE),
        ),
    ];
    let mut failures = Vec::new();
    for (label, args, advice) in cases {
        let refused = run(&args);
        let stderr = refused.stderr_str();
        if refused.status.code() != Some(2) || !stderr.contains(GATE) {
            failures.push(format!(
                "[{label}] exit {:?}, wanted the gate's refusal; stderr:\n{stderr}",
                refused.status.code()
            ));
            continue;
        }
        for (name, text) in [
            ("--force", FORCE_ADVICE),
            ("--allow-unindexed", ALLOW_ADVICE),
        ] {
            let wanted = advice == Some(text);
            if stderr.contains(text) != wanted {
                failures.push(format!(
                    "[{label}] advice for {name}: wanted {wanted}; stderr:\n{stderr}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// `merge-changes` names an input OSC it cannot open.
#[test]
fn merge_changes_names_a_missing_input() {
    let dir = crate::common::test_scratch();
    let out = run(&argv(dir.path(), &["merge-changes", "OSC", "-o", "OSCOUT"]));
    reached_input(&out).unwrap_or_else(|why| panic!("{why}"));
}

/// Every command with `--force` refuses unindexed input through the shared
/// gate, and `--force` takes it down the fallback path.
#[test]
fn every_command_with_force_refuses_unindexed_input_through_the_shared_gate() {
    let fx = Fx::new();
    let mut failures = Vec::new();
    for (n, row) in FORCE.iter().enumerate() {
        let label = row.words.join(" ");
        let refused = run(&force_argv(&fx, row, n, false));
        let stderr = refused.stderr_str();
        if refused.status.code() != Some(2) || !stderr.contains(GATE) {
            failures.push(format!(
                "[{label}] without --force: exit {:?}, wanted 2 with the gate's advice; stderr:\n{stderr}",
                refused.status.code()
            ));
        }
        let forced = run(&force_argv(&fx, row, n, true));
        if !forced.status.success() {
            failures.push(format!(
                "[{label}] with --force: exit {:?}; stderr:\n{}",
                forced.status.code(),
                forced.stderr_str()
            ));
        }
        if row.command == "build-geocode-index" {
            let stderr = forced.stderr_str();
            if !(stderr.contains("--overwrite") && stderr.contains("--allow-unindexed")) {
                failures.push(format!(
                    "[{label}] --force must warn that it grants overwrite, naming --overwrite \
                     and --allow-unindexed; stderr:\n{stderr}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The recipe a refusal prints: the first line of its stderr that starts with
/// `pbfhogg`, as the arguments after the program name.
fn printed_recipe(stderr: &str) -> Option<Vec<String>> {
    stderr
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("pbfhogg "))
        .map(|line| line.split_whitespace().skip(1).map(str::to_owned).collect())
}

/// The indexing recipe the gate prints works as written: run from an empty
/// directory it writes an indexed file, which the refused command then
/// accepts without `--force`. Also pins that the removed library advice, a
/// bare `cat --clean`, is the usage error it was always going to be.
#[test]
fn the_printed_indexing_recipe_makes_the_refused_command_succeed() {
    let fx = Fx::new();
    let mut failures = Vec::new();
    for (n, row) in FORCE.iter().enumerate() {
        let label = row.words.join(" ");
        let refused = run(&force_argv(&fx, row, n, false));
        let stderr = refused.stderr_str();
        let Some(recipe) = printed_recipe(&stderr) else {
            failures.push(format!("[{label}] no recipe printed; stderr:\n{stderr}"));
            continue;
        };
        let cwd = crate::common::test_scratch();
        let ran = CliInvoker::new()
            .current_dir(cwd.path())
            .args(&recipe)
            .run();
        if !ran.status.success() {
            failures.push(format!(
                "[{label}] the recipe {recipe:?} failed: exit {:?}; stderr:\n{}",
                ran.status.code(),
                ran.stderr_str()
            ));
            continue;
        }
        let indexed = cwd.path().join("indexed.osm.pbf");
        let again: Vec<OsString> = row
            .words
            .iter()
            .map(|word| match *word {
                "U" | "G" => indexed.clone().into(),
                "C" => fx.path("good.osc").into(),
                "O" => fx.path(&format!("recipe-out-{n}")).into(),
                other => OsString::from(other),
            })
            .collect();
        let accepted = run(&again);
        if !accepted.status.success() {
            failures.push(format!(
                "[{label}] the command still fails on the recipe's output: exit {:?}; stderr:\n{}",
                accepted.status.code(),
                accepted.stderr_str()
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));

    let out = CliInvoker::new()
        .arg("cat")
        .arg(fx.path("indexed.osm.pbf"))
        .arg("--clean")
        .arg("-o")
        .arg(fx.path("clean-out"))
        .run();
    assert_eq!(out.status.code(), Some(2), "{}", out.stderr_str());
}

/// The sort recipe an unsorted-input refusal prints works as written.
#[test]
fn the_printed_sort_recipe_makes_the_refused_diff_succeed() {
    let fx = Fx::new();
    let unsorted = fx.path("unsorted.osm.pbf");
    CliInvoker::new()
        .arg("degrade")
        .arg(fx.path("indexed.osm.pbf"))
        .arg("--unsort")
        .arg("-o")
        .arg(&unsorted)
        .assert_success();
    let refused = CliInvoker::new()
        .arg("diff")
        .arg(&unsorted)
        .arg(fx.path("indexed.osm.pbf"))
        .run();
    let stderr = refused.stderr_str();
    assert_eq!(refused.status.code(), Some(2), "{stderr}");
    let recipe = printed_recipe(&stderr).unwrap_or_else(|| panic!("no recipe printed:\n{stderr}"));
    let cwd = crate::common::test_scratch();
    CliInvoker::new()
        .current_dir(cwd.path())
        .args(&recipe)
        .assert_success();
    let accepted = CliInvoker::new()
        .arg("diff")
        .arg(cwd.path().join("sorted.osm.pbf"))
        .arg(fx.path("indexed.osm.pbf"))
        .run();
    assert!(
        accepted.status.code().is_some_and(|c| c <= 1),
        "{}",
        accepted.stderr_str()
    );
}

/// The table covers exactly the commands whose `--help` offers `--force`:
/// a command that gains the flag without refusing anything, or refuses
/// without the flag being tested, fails here.
#[test]
fn the_force_table_is_every_command_offering_force() {
    let mut offering: Vec<String> = subcommand_paths()
        .into_iter()
        .filter(|path| {
            help(path)
                .lines()
                .any(|l| l.trim_start().starts_with("--force"))
        })
        .map(|path| path.join(" "))
        .collect();
    offering.sort();
    let mut table: Vec<String> = FORCE.iter().map(|row| row.command.to_owned()).collect();
    table.sort();
    table.dedup();
    assert_eq!(offering, table);
}

// ---------------------------------------------------------------------------
// Per-command help text (the command list itself is the clap tree's)
// ---------------------------------------------------------------------------

fn help(path: &[String]) -> String {
    CliInvoker::new()
        .args(path)
        .arg("--help")
        .assert_success()
        .stdout_str()
}
