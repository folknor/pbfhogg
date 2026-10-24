//! The shared output and scratch contract, enforced on every subcommand by
//! one table.
//!
//! The contract is ADR-0017 (outputs are published by the library), ADR-0018
//! (scratch is one run directory and leftovers are reported) and the "Output
//! files", "Scratch files" and "Names pbfhogg leaves behind" sections of
//! `reference/cli-reference.md`. For every subcommand that writes an output
//! (a file, several files, a directory; PBF or text):
//!
//! - an existing output is refused before any work, with the typed advice;
//! - `--overwrite` and `-O` replace it;
//! - a symlinked output is refused even with `--overwrite`, the link and its
//!   target untouched;
//! - a run that fails mid-way leaves no file at the destination and no
//!   `.pbfhogg-*` entry anywhere near it;
//! - a successful run leaves exactly its outputs in the output's directory;
//! - where the command takes `--tmp-dir`: a bad directory is refused before
//!   any input is read, and the scratch lands there and only there.
//!
//! ## Complete by construction
//!
//! The list of subcommands is read from the binary's own `--help`, not
//! written here. A subcommand the binary has that is in neither [`ROWS`] nor
//! [`NO_OUTPUT`] fails `every_subcommand_is_in_the_table_or_marked_as_writing_no_output`,
//! as does a table entry the binary no longer has. A command in `NO_OUTPUT`
//! must also show no output flag in its `--help`, so the claim "writes no
//! output" is checked against the command's own surface.
//!
//! ## Known gaps
//!
//! A row that violates a property keeps asserting the correct behaviour and
//! names the property in its `gaps` list with the reason. The check still
//! runs and is expected to fail; when it starts passing the test fails and
//! asks for the marker to be removed, so a gap cannot outlive its fix.
//!
//! ## How "before any work" and "mid-run failure" are proven
//!
//! "Before any work" is the input that does not exist: the refusal must be
//! the error reported, not the missing input. A mid-run failure is an input
//! whose framing is intact but whose last blob's compressed payload is
//! damaged (or a malformed OSC), so the failure comes from decoding, after
//! the command opened its output.

// The property checks return `Result<(), String>` to report every violation
// at once; fixture and filesystem setup inside them still unwraps.
#![allow(clippy::unwrap_used, clippy::unwrap_in_result)]

use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::invoker::{CliInvoker, CliOutput, subcommands};
use tempfile::TempDir;

const USER_DATA: &[u8] = b"user data";
/// Appears in the name of every input that does not exist.
const MISSING_MARK: &str = "no-such-input";

macro_rules! ensure {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            return Err(format!($($arg)*));
        }
    };
}

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Input {
    Good,
    /// Does not exist.
    Missing,
    /// Intact framing, damaged payload: fails when decoded, not when opened.
    Corrupt,
}

/// Flip the last bytes of the file: the tail of the last blob's compressed
/// payload, where the zlib checksum sits. Framing stays valid.
fn corrupt_last_blob(source: &Path, dest: &Path) {
    let mut bytes = std::fs::read(source).unwrap();
    let len = bytes.len();
    for byte in &mut bytes[len - 8..] {
        *byte ^= 0xFF;
    }
    std::fs::write(dest, bytes).unwrap();
}

/// The fixtures every row draws from, built once per test into a directory
/// kept apart from every output directory.
struct Fx {
    dir: TempDir,
    counter: Cell<usize>,
}

impl Fx {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        crate::common::write_base_pbf(&p.join("good.osm.pbf"));
        crate::common::write_changed_pbf(&p.join("changed.osm.pbf"));
        let mut truncated = std::fs::read(p.join("good.osm.pbf")).unwrap();
        truncated.truncate(truncated.len() - 5);
        std::fs::write(p.join("truncated.osm.pbf"), truncated).unwrap();
        corrupt_last_blob(&p.join("good.osm.pbf"), &p.join("corrupt.osm.pbf"));
        crate::common::write_synthetic_indexed_input(&p.join("geo.osm.pbf"));
        corrupt_last_blob(&p.join("geo.osm.pbf"), &p.join("geo-corrupt.osm.pbf"));
        // time-filter refuses an input without element metadata.
        let meta = Some(crate::common::TestMeta::default());
        crate::common::write_test_pbf_sorted(
            &p.join("history.osm.pbf"),
            &crate::common::generate_nodes(3, 1)
                .into_iter()
                .map(|n| crate::common::TestNode { meta, ..n })
                .collect::<Vec<_>>(),
            &crate::common::generate_ways(1, 1, 2, 1)
                .into_iter()
                .map(|w| crate::common::TestWay { meta, ..w })
                .collect::<Vec<_>>(),
            &crate::common::generate_relations(1, 1, 1, 1)
                .into_iter()
                .map(|r| crate::common::TestRelation { meta, ..r })
                .collect::<Vec<_>>(),
        );
        corrupt_last_blob(
            &p.join("history.osm.pbf"),
            &p.join("history-corrupt.osm.pbf"),
        );
        std::fs::write(
            p.join("good.osc"),
            b"<osmChange version=\"0.6\"><create>\
              <node id=\"1000\" version=\"1\" lat=\"0.001\" lon=\"0.002\">\
              <tag k=\"amenity\" v=\"cafe\"/></node></create></osmChange>",
        )
        .unwrap();
        std::fs::write(p.join("corrupt.osc"), b"<osmChange><create>").unwrap();
        Self {
            dir,
            counter: Cell::new(0),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn next(&self) -> usize {
        self.counter.set(self.counter.get() + 1);
        self.counter.get()
    }

    fn pbf(&self, input: Input) -> PathBuf {
        match input {
            Input::Good => self.path("good.osm.pbf"),
            Input::Missing => self.path(&format!("{MISSING_MARK}.osm.pbf")),
            Input::Corrupt => self.path("corrupt.osm.pbf"),
        }
    }

    /// A PBF whose elements carry metadata.
    fn history(&self, input: Input) -> PathBuf {
        match input {
            Input::Good => self.path("history.osm.pbf"),
            Input::Missing => self.path(&format!("{MISSING_MARK}.osm.pbf")),
            Input::Corrupt => self.path("history-corrupt.osm.pbf"),
        }
    }

    fn changed_pbf(&self) -> PathBuf {
        self.path("changed.osm.pbf")
    }

    fn geo(&self, input: Input) -> PathBuf {
        match input {
            Input::Good => self.path("geo.osm.pbf"),
            Input::Missing => self.path(&format!("{MISSING_MARK}.osm.pbf")),
            Input::Corrupt => self.path("geo-corrupt.osm.pbf"),
        }
    }

    fn osc(&self, input: Input) -> PathBuf {
        match input {
            Input::Good => self.path("good.osc"),
            Input::Missing => self.path(&format!("{MISSING_MARK}.osc")),
            Input::Corrupt => self.path("corrupt.osc"),
        }
    }
}

/// What a row builds an invocation from: the fixtures and the directory its
/// outputs belong in.
struct Ctx<'a> {
    fx: &'a Fx,
    out: &'a Path,
}

struct Invocation {
    args: Vec<OsString>,
    /// Every output the run is contracted to produce, in publication order.
    outputs: Vec<PathBuf>,
}

impl Invocation {
    fn new(words: &[&str]) -> Self {
        Self {
            args: words.iter().map(OsString::from).collect(),
            outputs: Vec::new(),
        }
    }

    fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    fn words(mut self, words: &[&str]) -> Self {
        self.args.extend(words.iter().map(OsString::from));
        self
    }

    /// `flag path`, registering `path` as an output.
    fn out(mut self, flag: &str, path: PathBuf) -> Self {
        self.args.push(flag.into());
        self.args.push(path.clone().into_os_string());
        self.outputs.push(path);
        self
    }
}

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    File,
    /// A directory output (the geocode index).
    Dir,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Prop {
    Refused,
    Overwrite,
    Symlink,
    Failure,
    Published,
    TmpDir,
    #[cfg_attr(not(feature = "test-hooks"), allow(dead_code))]
    Interrupted,
}

struct Row {
    command: &'static str,
    /// Distinguishes several rows of one command.
    label: &'static str,
    build: fn(&Ctx, Input) -> Invocation,
    kind: Kind,
    /// Takes `--tmp-dir`.
    tmp_dir: bool,
    /// Exit codes that mean success (`diff` exits 1 on differences).
    ok_codes: &'static [i32],
    /// Properties this command is known to violate, with the reason.
    gaps: &'static [(Prop, &'static str)],
}

/// Subcommands that write no output, with the reason.
const NO_OUTPUT: &[(&str, &str)] = &[
    ("digest", "prints the digest on stdout"),
    ("inspect", "prints its report on stdout"),
    (
        "check",
        "prints its findings on stdout, exit code only with --quiet",
    ),
    ("bench-read", "reads and reports counters on stderr"),
    (
        "bench-write",
        "encodes into /dev/null, a byte sink the benchmark discards, not an output",
    ),
];

const ROWS: &[Row] = &[
    Row {
        command: "cat",
        label: "cat",
        build: cat,
        kind: Kind::File,
        tmp_dir: false,
        ok_codes: &[0],
        gaps: &[],
    },
    Row {
        command: "sort",
        label: "sort",
        build: sort,
        kind: Kind::File,
        tmp_dir: false,
        ok_codes: &[0],
        gaps: &[],
    },
    Row {
        command: "repack",
        label: "repack",
        build: repack,
        kind: Kind::File,
        tmp_dir: false,
        ok_codes: &[0],
        gaps: &[],
    },
    Row {
        command: "degrade",
        label: "degrade",
        build: degrade,
        kind: Kind::File,
        tmp_dir: false,
        ok_codes: &[0],
        gaps: &[],
    },
    Row {
        command: "renumber",
        label: "renumber",
        build: renumber,
        kind: Kind::File,
        tmp_dir: false,
        ok_codes: &[0],
        gaps: &[],
    },
    Row {
        command: "tags-filter",
        label: "tags-filter",
        build: tags_filter,
        kind: Kind::File,
        tmp_dir: false,
        ok_codes: &[0],
        gaps: &[],
    },
    Row {
        command: "tags-filter",
        label: "tags-filter --input-kind osc",
        build: tags_filter_osc,
        kind: Kind::File,
        tmp_dir: false,
        ok_codes: &[0],
        gaps: &[],
    },
    Row {
        command: "export",
        label: "export",
        build: export,
        kind: Kind::File,
        tmp_dir: false,
        ok_codes: &[0],
        gaps: &[],
    },
    Row {
        command: "diff",
        label: "diff (text)",
        build: diff_text,
        kind: Kind::File,
        tmp_dir: true,
        ok_codes: &[0, 1],
        gaps: &[],
    },
    Row {
        command: "diff",
        label: "diff --format osc",
        build: diff_osc,
        kind: Kind::File,
        tmp_dir: true,
        ok_codes: &[0, 1],
        gaps: &[],
    },
    Row {
        command: "getid",
        label: "getid",
        build: getid,
        kind: Kind::File,
        tmp_dir: false,
        ok_codes: &[0],
        gaps: &[],
    },
    Row {
        command: "getparents",
        label: "getparents",
        build: getparents,
        kind: Kind::File,
        tmp_dir: false,
        ok_codes: &[0],
        gaps: &[],
    },
    Row {
        command: "extract",
        label: "extract",
        build: extract_single,
        kind: Kind::File,
        tmp_dir: false,
        ok_codes: &[0],
        gaps: &[],
    },
    Row {
        command: "extract",
        label: "extract --config",
        build: extract_config,
        kind: Kind::File,
        tmp_dir: false,
        ok_codes: &[0],
        gaps: &[],
    },
    Row {
        command: "add-locations-to-ways",
        label: "add-locations-to-ways",
        build: add_locations_to_ways,
        kind: Kind::File,
        tmp_dir: true,
        ok_codes: &[0],
        gaps: &[],
    },
    Row {
        command: "time-filter",
        label: "time-filter",
        build: time_filter,
        kind: Kind::File,
        tmp_dir: false,
        ok_codes: &[0],
        gaps: &[],
    },
    Row {
        command: "apply-changes",
        label: "apply-changes",
        build: apply_changes,
        kind: Kind::File,
        tmp_dir: false,
        ok_codes: &[0],
        gaps: &[],
    },
    Row {
        command: "merge-changes",
        label: "merge-changes",
        build: merge_changes,
        kind: Kind::File,
        tmp_dir: false,
        ok_codes: &[0],
        gaps: &[],
    },
    Row {
        command: "bench-merge",
        label: "bench-merge",
        build: bench_merge,
        kind: Kind::File,
        tmp_dir: false,
        ok_codes: &[0],
        gaps: &[],
    },
    Row {
        command: "build-geocode-index",
        label: "build-geocode-index",
        build: build_geocode_index,
        kind: Kind::Dir,
        tmp_dir: true,
        ok_codes: &[0],
        gaps: &[],
    },
];

fn pbf_out(c: &Ctx) -> PathBuf {
    c.out.join("out.osm.pbf")
}

fn cat(c: &Ctx, i: Input) -> Invocation {
    // `-c version` forces the re-encode path; plain `cat` passes blobs through.
    Invocation::new(&["cat"])
        .arg(c.fx.pbf(i))
        .words(&["--clean", "version"])
        .out("-o", pbf_out(c))
}

fn sort(c: &Ctx, i: Input) -> Invocation {
    // Sorted input is copied blob by blob without decoding, by design (the
    // run says so), so a damaged payload is not a failure sort can see. A
    // framing error is: pass 1 indexes every blob.
    let input = if i == Input::Corrupt {
        c.fx.path("truncated.osm.pbf")
    } else {
        c.fx.pbf(i)
    };
    Invocation::new(&["sort"]).arg(input).out("-o", pbf_out(c))
}

fn repack(c: &Ctx, i: Input) -> Invocation {
    Invocation::new(&["repack"])
        .arg(c.fx.pbf(i))
        .out("-o", pbf_out(c))
}

fn degrade(c: &Ctx, i: Input) -> Invocation {
    // A decode-path transformation: header-only flags would pass a damaged
    // payload through untouched.
    Invocation::new(&["degrade"])
        .arg(c.fx.pbf(i))
        .words(&["--drop-ids", "1:1"])
        .out("-o", pbf_out(c))
}

fn renumber(c: &Ctx, i: Input) -> Invocation {
    Invocation::new(&["renumber"])
        .arg(c.fx.pbf(i))
        .out("-o", pbf_out(c))
}

fn tags_filter(c: &Ctx, i: Input) -> Invocation {
    Invocation::new(&["tags-filter"])
        .arg(c.fx.pbf(i))
        .out("-o", pbf_out(c))
        .arg("r/type=multipolygon")
}

fn tags_filter_osc(c: &Ctx, i: Input) -> Invocation {
    Invocation::new(&["tags-filter"])
        .arg(c.fx.osc(i))
        .words(&["--input-kind", "osc"])
        .out("-o", c.out.join("out.osc"))
        .arg("amenity")
}

fn export(c: &Ctx, i: Input) -> Invocation {
    Invocation::new(&["export"])
        .arg(c.fx.pbf(i))
        .words(&["--type", "node"])
        .out("-o", c.out.join("out.geojsonseq"))
}

fn diff_text(c: &Ctx, i: Input) -> Invocation {
    Invocation::new(&["diff"])
        .arg(c.fx.pbf(i))
        .arg(c.fx.changed_pbf())
        .words(&["-j", "2"])
        .out("-o", c.out.join("diff.txt"))
}

fn diff_osc(c: &Ctx, i: Input) -> Invocation {
    Invocation::new(&["diff"])
        .arg(c.fx.pbf(i))
        .arg(c.fx.changed_pbf())
        .words(&["--format", "osc", "-j", "2"])
        .out("-o", c.out.join("delta.osc.gz"))
}

fn getid(c: &Ctx, i: Input) -> Invocation {
    Invocation::new(&["getid"])
        .arg(c.fx.pbf(i))
        .out("-o", pbf_out(c))
        .arg("r1")
}

fn getparents(c: &Ctx, i: Input) -> Invocation {
    Invocation::new(&["getparents"])
        .arg(c.fx.pbf(i))
        .out("-o", pbf_out(c))
        .arg("n1")
}

fn extract_single(c: &Ctx, i: Input) -> Invocation {
    Invocation::new(&["extract"])
        .arg(c.fx.pbf(i))
        .words(&["--simple", "--bbox=-1,-1,1,1"])
        .out("-o", pbf_out(c))
}

/// Two outputs named by a config; the last is the one the seeding scenarios
/// plant, so "the others are not created" is a real claim.
fn extract_config(c: &Ctx, i: Input) -> Invocation {
    let config = c.fx.path(&format!("config-{}.json", c.fx.next()));
    std::fs::write(
        &config,
        format!(
            r#"{{"directory":"{}","extracts":[
                {{"output":"a.osm.pbf","bbox":[-1.0,-1.0,1.0,1.0]}},
                {{"output":"b.osm.pbf","bbox":[-1.0,-1.0,1.0,1.0]}}]}}"#,
            c.out.display()
        ),
    )
    .unwrap();
    let mut inv = Invocation::new(&["extract"])
        .arg(c.fx.pbf(i))
        .arg("--config")
        .arg(&config)
        .arg("--simple");
    inv.outputs = vec![c.out.join("a.osm.pbf"), c.out.join("b.osm.pbf")];
    inv
}

fn add_locations_to_ways(c: &Ctx, i: Input) -> Invocation {
    Invocation::new(&["add-locations-to-ways"])
        .arg(c.fx.pbf(i))
        .out("-o", pbf_out(c))
}

fn time_filter(c: &Ctx, i: Input) -> Invocation {
    Invocation::new(&["time-filter"])
        .arg(c.fx.history(i))
        .out("-o", pbf_out(c))
        .arg("2030-01-01T00:00:00Z")
}

/// The input that varies is the base for "missing", the OSC for "corrupt":
/// the OSC is what the merge parses after its output exists.
fn base_and_osc(c: &Ctx, i: Input) -> (PathBuf, PathBuf) {
    match i {
        Input::Good => (c.fx.pbf(Input::Good), c.fx.osc(Input::Good)),
        Input::Missing => (c.fx.pbf(Input::Missing), c.fx.osc(Input::Good)),
        Input::Corrupt => (c.fx.pbf(Input::Good), c.fx.osc(Input::Corrupt)),
    }
}

fn apply_changes(c: &Ctx, i: Input) -> Invocation {
    let (base, osc) = base_and_osc(c, i);
    Invocation::new(&["apply-changes"])
        .arg(base)
        .arg(osc)
        .out("-o", pbf_out(c))
}

fn merge_changes(c: &Ctx, i: Input) -> Invocation {
    Invocation::new(&["merge-changes"])
        .arg(c.fx.osc(i))
        .out("-o", c.out.join("merged.osc"))
}

fn bench_merge(c: &Ctx, i: Input) -> Invocation {
    let (base, osc) = base_and_osc(c, i);
    Invocation::new(&["bench-merge"])
        .arg(base)
        .arg(osc)
        .out("-o", pbf_out(c))
}

fn build_geocode_index(c: &Ctx, i: Input) -> Invocation {
    Invocation::new(&["build-geocode-index"])
        .arg(c.fx.geo(i))
        .arg("--allow-unindexed")
        .out("--output-dir", c.out.join("index"))
}

// ---------------------------------------------------------------------------
// Observation helpers
// ---------------------------------------------------------------------------

fn run(inv: &Invocation, extra: &[OsString]) -> CliOutput {
    CliInvoker::new().args(&inv.args).args(extra).run()
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn hidden(dir: &Path) -> Vec<String> {
    names(dir)
        .into_iter()
        .filter(|n| n.starts_with(".pbfhogg-"))
        .collect()
}

fn output_names(inv: &Invocation) -> Vec<String> {
    let mut names: Vec<String> = inv
        .outputs
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn seed(path: &Path, kind: Kind) {
    match kind {
        Kind::File => std::fs::write(path, USER_DATA).unwrap(),
        Kind::Dir => std::fs::create_dir(path).unwrap(),
    }
}

/// The planted output is exactly as planted.
fn intact(path: &Path, kind: Kind) -> bool {
    match kind {
        Kind::File => std::fs::read(path).is_ok_and(|bytes| bytes == USER_DATA),
        Kind::Dir => path.is_dir() && names(path).is_empty(),
    }
}

/// A real output stands where the planted one was.
fn replaced(path: &Path, kind: Kind) -> bool {
    match kind {
        Kind::File => {
            std::fs::read(path).is_ok_and(|bytes| !bytes.is_empty() && bytes != USER_DATA)
        }
        Kind::Dir => path.is_dir() && !names(path).is_empty(),
    }
}

fn exited_ok(row: &Row, out: &CliOutput) -> bool {
    out.status
        .code()
        .is_some_and(|code| row.ok_codes.contains(&code))
}

fn tmp_args(tmp: Option<&Path>) -> Vec<OsString> {
    tmp.map_or_else(Vec::new, |dir| {
        vec!["--tmp-dir".into(), dir.as_os_str().to_owned()]
    })
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

type Check = fn(&Fx, &Row) -> Result<(), String>;

fn refused_before_any_work(fx: &Fx, row: &Row) -> Result<(), String> {
    let dir = TempDir::new().unwrap();
    let inv = (row.build)(
        &Ctx {
            fx,
            out: dir.path(),
        },
        Input::Missing,
    );
    let (last, others) = inv.outputs.split_last().unwrap();
    seed(last, row.kind);
    let out = run(&inv, &[]);
    let stderr = out.stderr_str();
    ensure!(
        out.status.code() == Some(2),
        "exit {:?}, wanted 2; stderr:\n{stderr}",
        out.status.code()
    );
    ensure!(
        stderr.contains("already exists") && stderr.contains("--overwrite (-O)"),
        "the refusal must name the output and the flag; stderr:\n{stderr}"
    );
    ensure!(
        !stderr.contains(MISSING_MARK),
        "work was done before the refusal (the missing input was reached); stderr:\n{stderr}"
    );
    ensure!(intact(last, row.kind), "the existing output was touched");
    for other in others {
        ensure!(!other.exists(), "{} was created", other.display());
    }
    ensure!(
        hidden(dir.path()).is_empty(),
        "left behind: {:?}",
        hidden(dir.path())
    );
    Ok(())
}

fn overwrite_replaces(fx: &Fx, row: &Row) -> Result<(), String> {
    for flag in ["--overwrite", "-O"] {
        let dir = TempDir::new().unwrap();
        let inv = (row.build)(
            &Ctx {
                fx,
                out: dir.path(),
            },
            Input::Good,
        );
        for output in &inv.outputs {
            seed(output, row.kind);
        }
        let out = run(&inv, &[flag.into()]);
        ensure!(
            exited_ok(row, &out),
            "{flag}: exit {:?}; stderr:\n{}",
            out.status.code(),
            out.stderr_str()
        );
        for output in &inv.outputs {
            ensure!(
                replaced(output, row.kind),
                "{flag}: {} was not replaced by a real output",
                output.display()
            );
        }
        ensure!(
            names(dir.path()) == output_names(&inv),
            "{flag}: the output directory holds {:?}, wanted {:?}",
            names(dir.path()),
            output_names(&inv)
        );
    }
    Ok(())
}

fn symlink_refused(fx: &Fx, row: &Row) -> Result<(), String> {
    let dir = TempDir::new().unwrap();
    let keep = TempDir::new().unwrap();
    let inv = (row.build)(
        &Ctx {
            fx,
            out: dir.path(),
        },
        Input::Good,
    );
    let (link, others) = inv.outputs.split_last().unwrap();
    let target = keep.path().join("target");
    seed(&target, row.kind);
    if row.kind == Kind::Dir {
        std::fs::write(target.join("marker"), USER_DATA).unwrap();
    }
    std::os::unix::fs::symlink(&target, link).unwrap();
    let out = run(&inv, &["--overwrite".into()]);
    let stderr = out.stderr_str();
    ensure!(
        out.status.code() == Some(2),
        "exit {:?}, wanted 2; stderr:\n{stderr}",
        out.status.code()
    );
    let link_name = link.file_name().unwrap().to_string_lossy().into_owned();
    ensure!(
        stderr.contains(&format!("{link_name} is a symlink to")) && stderr.contains("target"),
        "the error must name the link and its target; stderr:\n{stderr}"
    );
    ensure!(
        std::fs::symlink_metadata(link).is_ok_and(|m| m.file_type().is_symlink()),
        "the link is no longer a symlink"
    );
    let target_intact = match row.kind {
        Kind::File => intact(&target, Kind::File),
        Kind::Dir => names(&target) == ["marker"],
    };
    ensure!(target_intact, "the link's target was touched");
    for other in others {
        ensure!(!other.exists(), "{} was created", other.display());
    }
    ensure!(
        hidden(dir.path()).is_empty(),
        "left behind: {:?}",
        hidden(dir.path())
    );
    Ok(())
}

fn failure_leaves_nothing(fx: &Fx, row: &Row) -> Result<(), String> {
    // Fresh destination: nothing at all may be left, under any name.
    let mut scratch_homes: Vec<Option<TempDir>> = vec![None];
    if row.tmp_dir {
        scratch_homes.push(Some(TempDir::new().unwrap()));
    }
    for tmp in &scratch_homes {
        let tmp_path = tmp.as_ref().map(TempDir::path);
        let dir = TempDir::new().unwrap();
        let inv = (row.build)(
            &Ctx {
                fx,
                out: dir.path(),
            },
            Input::Corrupt,
        );
        let out = run(&inv, &tmp_args(tmp_path));
        ensure!(
            !out.status.success(),
            "the corrupt input did not fail the run (tmp-dir {tmp_path:?}); stderr:\n{}",
            out.stderr_str()
        );
        for output in &inv.outputs {
            ensure!(
                std::fs::symlink_metadata(output).is_err(),
                "a failed run left {} behind (tmp-dir {tmp_path:?})",
                output.display()
            );
        }
        ensure!(
            names(dir.path()).is_empty(),
            "a failed run left {:?} beside the destination (tmp-dir {tmp_path:?})",
            names(dir.path())
        );
        if let Some(tmp_path) = tmp_path {
            ensure!(
                names(tmp_path).is_empty(),
                "a failed run left {:?} in --tmp-dir",
                names(tmp_path)
            );
        }
    }

    // Replacing: the failed run must leave what was there as it was.
    let dir = TempDir::new().unwrap();
    let inv = (row.build)(
        &Ctx {
            fx,
            out: dir.path(),
        },
        Input::Corrupt,
    );
    for output in &inv.outputs {
        seed(output, row.kind);
    }
    let out = run(&inv, &["--overwrite".into()]);
    ensure!(
        !out.status.success(),
        "the corrupt input did not fail the replacing run; stderr:\n{}",
        out.stderr_str()
    );
    for output in &inv.outputs {
        ensure!(
            intact(output, row.kind),
            "a failed replacing run changed {}",
            output.display()
        );
    }
    ensure!(
        hidden(dir.path()).is_empty(),
        "a failed replacing run left {:?}",
        hidden(dir.path())
    );
    Ok(())
}

fn success_leaves_only_outputs(fx: &Fx, row: &Row) -> Result<(), String> {
    let dir = TempDir::new().unwrap();
    let inv = (row.build)(
        &Ctx {
            fx,
            out: dir.path(),
        },
        Input::Good,
    );
    let out = run(&inv, &[]);
    ensure!(
        exited_ok(row, &out),
        "exit {:?}; stderr:\n{}",
        out.status.code(),
        out.stderr_str()
    );
    for output in &inv.outputs {
        ensure!(
            replaced(output, row.kind),
            "{} is missing or empty",
            output.display()
        );
    }
    ensure!(
        names(dir.path()) == output_names(&inv),
        "the output directory holds {:?}, wanted exactly {:?}",
        names(dir.path()),
        output_names(&inv)
    );
    ensure!(
        hidden(fx.dir.path()).is_empty(),
        "scratch landed beside the inputs: {:?}",
        hidden(fx.dir.path())
    );
    Ok(())
}

/// Directory entries created while a command ran, seen through inotify.
/// Scratch is removed by the time the command exits, so where it landed can
/// only be observed while it exists.
struct Watcher {
    fd: libc::c_int,
    dirs: Vec<(libc::c_int, PathBuf)>,
}

impl Watcher {
    /// `None` when the host cannot watch.
    fn new(dirs: &[&Path]) -> Option<Self> {
        // SAFETY: plain syscall, no pointers.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        let mut watcher = Self {
            fd,
            dirs: Vec::new(),
        };
        for dir in dirs {
            let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
            // SAFETY: `path` is a valid NUL-terminated string for the call.
            let wd = unsafe {
                libc::inotify_add_watch(fd, path.as_ptr(), libc::IN_CREATE | libc::IN_MOVED_TO)
            };
            if wd < 0 {
                return None;
            }
            watcher.dirs.push((wd, dir.to_path_buf()));
        }
        Some(watcher)
    }

    /// Every (directory, name) created since the watch began.
    fn created(&self) -> Vec<(PathBuf, String)> {
        let mut found = Vec::new();
        let mut buf = vec![0_u8; 1 << 16];
        loop {
            // SAFETY: `buf` is valid for `buf.len()` bytes.
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
            let Ok(n) = usize::try_from(n) else { break };
            if n == 0 {
                break;
            }
            let mut at = 0;
            while at + 16 <= n {
                let wd = i32::from_ne_bytes(buf[at..at + 4].try_into().unwrap());
                let len = u32::from_ne_bytes(buf[at + 12..at + 16].try_into().unwrap()) as usize;
                let raw = &buf[at + 16..at + 16 + len];
                let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
                let name = String::from_utf8_lossy(&raw[..end]).into_owned();
                if let Some((_, dir)) = self.dirs.iter().find(|(w, _)| *w == wd) {
                    found.push((dir.clone(), name));
                }
                at += 16 + len;
            }
        }
        found
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // SAFETY: the descriptor is ours and closed once.
        unsafe { libc::close(self.fd) };
    }
}

fn run_dirs_of(events: &[(PathBuf, String)], dir: &Path) -> usize {
    events
        .iter()
        .filter(|(d, name)| d == dir && name.starts_with(".pbfhogg-run-"))
        .count()
}

fn bad_tmp_dir_is_refused_first(fx: &Fx, row: &Row) -> Result<(), String> {
    // A bad --tmp-dir is refused before any input is read: the input here
    // does not exist, and must not be the error reported.
    let base = TempDir::new().unwrap();
    let a_file = base.path().join("a-file");
    std::fs::write(&a_file, b"x").unwrap();
    let locked = base.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o500)).unwrap();
    let mut bad = vec![
        ("missing", base.path().join("no-such-dir"), "--tmp-dir"),
        ("a file", a_file, "not a directory"),
    ];
    if !is_root() {
        bad.push(("unwritable", locked.clone(), "--tmp-dir"));
    }
    let mut result = Ok(());
    for (what, tmp, wording) in &bad {
        let dir = TempDir::new().unwrap();
        let inv = (row.build)(
            &Ctx {
                fx,
                out: dir.path(),
            },
            Input::Missing,
        );
        let out = run(&inv, &tmp_args(Some(tmp)));
        let stderr = out.stderr_str();
        let verdict = (|| {
            ensure!(
                !out.status.success(),
                "{what} --tmp-dir was accepted; stderr:\n{stderr}"
            );
            ensure!(
                stderr.contains(wording) && stderr.contains(&tmp.display().to_string()),
                "{what} --tmp-dir: the error must name the flag and the path; stderr:\n{stderr}"
            );
            ensure!(
                !stderr.contains(MISSING_MARK),
                "{what} --tmp-dir: an input was read first; stderr:\n{stderr}"
            );
            ensure!(
                names(dir.path()).is_empty(),
                "{what} --tmp-dir: left {:?} beside the output",
                names(dir.path())
            );
            Ok(())
        })();
        if result.is_ok() {
            result = verdict;
        }
    }
    let locked_left = names(&locked);
    std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    result?;
    ensure!(
        locked_left.is_empty(),
        "the unwritable --tmp-dir got {locked_left:?}"
    );
    ensure!(
        !base.path().join("no-such-dir").exists(),
        "--tmp-dir was created"
    );
    Ok(())
}

fn tmp_dir_contract(fx: &Fx, row: &Row) -> Result<(), String> {
    bad_tmp_dir_is_refused_first(fx, row)?;
    scratch_lands_where_it_was_told(fx, row)
}

/// Scratch lands in --tmp-dir and only there; without it, beside the output.
fn scratch_lands_where_it_was_told(fx: &Fx, row: &Row) -> Result<(), String> {
    for with_tmp in [true, false] {
        let out_dir = TempDir::new().unwrap();
        let tmp = TempDir::new().unwrap();
        let inv = (row.build)(
            &Ctx {
                fx,
                out: out_dir.path(),
            },
            Input::Good,
        );
        let Some(watcher) = Watcher::new(&[out_dir.path(), tmp.path()]) else {
            crate::common::skip("inotify is unavailable on this host");
            return Ok(());
        };
        let out = run(&inv, &tmp_args(with_tmp.then(|| tmp.path())));
        ensure!(
            exited_ok(row, &out),
            "exit {:?}; stderr:\n{}",
            out.status.code(),
            out.stderr_str()
        );
        let events = watcher.created();
        let (in_out, in_tmp) = (
            run_dirs_of(&events, out_dir.path()),
            run_dirs_of(&events, tmp.path()),
        );
        if with_tmp {
            ensure!(
                in_tmp == 1,
                "expected one run directory in --tmp-dir, saw {in_tmp}: {events:?}"
            );
            ensure!(
                in_out == 0,
                "scratch landed beside the output despite --tmp-dir: {events:?}"
            );
        } else {
            ensure!(
                in_out == 1,
                "expected one run directory beside the output, saw {in_out}: {events:?}"
            );
            ensure!(
                in_tmp == 0,
                "scratch landed in an unrelated directory: {events:?}"
            );
        }
        ensure!(
            names(tmp.path()).is_empty(),
            "--tmp-dir still holds {:?}",
            names(tmp.path())
        );
        ensure!(
            names(out_dir.path()) == output_names(&inv),
            "the output directory holds {:?}, wanted exactly {:?}",
            names(out_dir.path()),
            output_names(&inv)
        );
    }
    Ok(())
}

/// Run one property over every row it applies to, collect every violation,
/// and honour the known-gap markers in both directions.
fn run_property(prop: Prop, check: Check, applies: fn(&Row) -> bool) {
    let fx = Fx::new();
    let mut failures = Vec::new();
    for row in ROWS.iter().filter(|row| applies(row)) {
        let verdict = check(&fx, row);
        let gap = row.gaps.iter().find(|(p, _)| *p == prop);
        match (verdict, gap) {
            (Ok(()), None) => {}
            (Err(why), None) => failures.push(format!("[{}] {why}", row.label)),
            (Err(why), Some((_, reason))) => {
                eprintln!("known gap [{}] {prop:?}: {reason}\n  {why}", row.label);
            }
            (Ok(()), Some((_, reason))) => failures.push(format!(
                "[{}] is marked as a known {prop:?} gap ({reason}) but passes: remove the marker",
                row.label
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{prop:?} violations:\n\n{}",
        failures.join("\n\n")
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn every_subcommand_is_in_the_table_or_marked_as_writing_no_output() {
    let commands = subcommands();
    assert!(
        commands.len() >= 20,
        "the clap tree lists only {commands:?}"
    );
    let mut problems = Vec::new();
    for command in &commands {
        let in_table = ROWS.iter().any(|row| row.command == command);
        let no_output = NO_OUTPUT.iter().any(|(name, _)| name == command);
        match (in_table, no_output) {
            (false, false) => problems.push(format!(
                "`{command}` is in neither ROWS nor NO_OUTPUT: add a row for it, or say why it writes no output"
            )),
            (true, true) => problems.push(format!("`{command}` is in both ROWS and NO_OUTPUT")),
            _ => {}
        }
    }
    for name in ROWS
        .iter()
        .map(|row| row.command)
        .chain(NO_OUTPUT.iter().map(|(name, _)| *name))
    {
        if !commands.iter().any(|c| c == name) {
            problems.push(format!(
                "`{name}` is in the table but the binary has no such subcommand"
            ));
        }
    }
    // The table and the command's own surface must agree.
    for row in ROWS {
        let help = CliInvoker::new()
            .args([row.command, "--help"])
            .assert_success()
            .stdout_str();
        if !help.contains("--overwrite") {
            problems.push(format!(
                "[{}] is in ROWS but `{} --help` offers no --overwrite",
                row.label, row.command
            ));
        }
        if row.tmp_dir != help.contains("--tmp-dir") {
            problems.push(format!(
                "[{}] tmp_dir is {} but `{} --help` says otherwise",
                row.label, row.tmp_dir, row.command
            ));
        }
    }
    for (name, reason) in NO_OUTPUT {
        let help = CliInvoker::new()
            .args([name, "--help"])
            .assert_success()
            .stdout_str();
        for flag in ["--overwrite", "--output-dir", "--output <"] {
            if help.contains(flag) {
                problems.push(format!(
                    "`{name}` is marked as writing no output ({reason}) but offers {flag}"
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn an_existing_output_is_refused_before_any_work() {
    run_property(Prop::Refused, refused_before_any_work, |_| true);
}

#[test]
fn overwrite_and_short_o_replace_an_existing_output() {
    run_property(Prop::Overwrite, overwrite_replaces, |_| true);
}

#[test]
fn a_symlinked_output_is_refused_even_with_overwrite() {
    run_property(Prop::Symlink, symlink_refused, |_| true);
}

#[test]
fn a_failed_run_leaves_nothing_at_the_destination_or_beside_it() {
    run_property(Prop::Failure, failure_leaves_nothing, |_| true);
}

#[test]
fn a_successful_run_leaves_only_its_outputs() {
    run_property(Prop::Published, success_leaves_only_outputs, |_| true);
}

#[test]
fn tmp_dir_is_checked_first_and_holds_the_scratch() {
    run_property(Prop::TmpDir, tmp_dir_contract, |row| row.tmp_dir);
}

// ---------------------------------------------------------------------------
// Interruption: SIGINT and SIGTERM mid-run
// ---------------------------------------------------------------------------
//
// The contract (`signals` in the CLI, ADR-0019): the first SIGINT or SIGTERM
// cancels the run, every guard drops (staging and run scratch removed),
// nothing is published and the process exits 128 + signo; a second signal
// exits at once and leaves what it leaves for the next run to report.
//
// "Mid-run" is made deterministic by the CLI's `test-hooks` pause: with
// `PBFHOGG_TEST_CANCEL_PAUSE=<k>:<file>` the thread reaching the k-th
// cancellation checkpoint writes `<file>` and parks until the token trips.
// The test waits for the file, looks at what exists, sends the signal, and
// checks what is left. A counting run (`PBFHOGG_TEST_CANCEL_COUNT`) first
// learns how many checkpoints a run of the row passes, and the sweep pauses
// at ordinal positions spread over that range. Ordinals are positions, not
// sites: which checkpoint is the k-th varies with scheduling, so the claim is
// that every pause, wherever it lands, unwinds cleanly - and the sweep must
// have observed populated staging (and, for scratch-writing rows, a scratch
// payload) at some pause, so cleanup was exercised against real state rather
// than inferred.

#[cfg(feature = "test-hooks")]
mod interrupted {
    use super::*;
    use std::time::{Duration, Instant};

    const READY_TIMEOUT: Duration = Duration::from_secs(10);

    pub(super) fn signal_name(signal: libc::c_int) -> &'static str {
        if signal == libc::SIGTERM {
            "SIGTERM"
        } else {
            "SIGINT"
        }
    }

    pub(super) fn exit_code(signal: libc::c_int) -> i32 {
        128 + signal
    }

    /// Every regular file under `dir`, at any depth.
    fn files_under(dir: &Path, out: &mut Vec<(PathBuf, u64)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => files_under(&path, out),
                Ok(t) if t.is_file() => {
                    let len = entry.metadata().map_or(0, |m| m.len());
                    out.push((path, len));
                }
                _ => {}
            }
        }
    }

    /// A `.pbfhogg-tmp-*` staging entry with content: a non-empty file, or a
    /// directory output's staging directory holding a non-empty file.
    fn staging_populated(dir: &Path) -> bool {
        names(dir)
            .into_iter()
            .filter(|n| n.starts_with(".pbfhogg-tmp-"))
            .any(|n| {
                let mut files = Vec::new();
                let path = dir.join(&n);
                if path.is_file() {
                    return path.metadata().is_ok_and(|m| m.len() > 0);
                }
                files_under(&path, &mut files);
                files.iter().any(|(_, len)| *len > 0)
            })
    }

    /// A run directory holding scratch payload: a non-empty file other than
    /// its `LOCK` and `READY` bookkeeping.
    fn scratch_populated(dir: &Path) -> bool {
        names(dir)
            .into_iter()
            .filter(|n| n.starts_with(".pbfhogg-run-"))
            .any(|n| {
                let mut files = Vec::new();
                files_under(&dir.join(n), &mut files);
                files.iter().any(|(path, len)| {
                    let name = path.file_name().map(OsStr::to_os_string);
                    *len > 0
                        && name.as_deref() != Some(OsStr::new("LOCK"))
                        && name.as_deref() != Some(OsStr::new("READY"))
                })
            })
    }

    /// Extra arguments that put a row on the path whose scratch is visible:
    /// add-locations-to-ways picks the sparse store for a small input, and
    /// that store is unlinked as soon as it is mapped, so only the external
    /// join leaves scratch files a pause can observe.
    pub(super) fn visible_scratch_args(row: &Row) -> Vec<OsString> {
        if row.command == "add-locations-to-ways" {
            vec!["--index-type".into(), "external".into()]
        } else {
            Vec::new()
        }
    }

    /// The row's invocation in a fresh output directory, with `--tmp-dir`
    /// for rows that take it (so scratch and staging are told apart).
    struct Run {
        out: TempDir,
        tmp: Option<TempDir>,
        inv: Invocation,
        extra: Vec<OsString>,
    }

    impl Run {
        fn new(fx: &Fx, row: &Row) -> Self {
            let out = TempDir::new().unwrap();
            let tmp = row.tmp_dir.then(|| TempDir::new().unwrap());
            let inv = (row.build)(
                &Ctx {
                    fx,
                    out: out.path(),
                },
                Input::Good,
            );
            Self {
                out,
                tmp,
                inv,
                extra: visible_scratch_args(row),
            }
        }

        fn invoker(&self, extra: &[OsString]) -> CliInvoker {
            CliInvoker::new()
                .args(&self.inv.args)
                .args(&self.extra)
                .args(extra)
                .args(tmp_args(self.tmp.as_ref().map(TempDir::path)))
        }
    }

    /// How many cancellation checkpoints one uninterrupted run of `row`
    /// passes.
    pub(super) fn checkpoint_count(fx: &Fx, row: &Row) -> Result<u64, String> {
        let run = Run::new(fx, row);
        let control = TempDir::new().unwrap();
        let count_file = control.path().join("count");
        let out = run
            .invoker(&[])
            .env("PBFHOGG_TEST_CANCEL_COUNT", &count_file)
            .run();
        ensure!(
            exited_ok(row, &out),
            "the counting run failed: exit {:?}; stderr:\n{}",
            out.status.code(),
            out.stderr_str()
        );
        std::fs::read_to_string(&count_file)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&k| k > 0)
            .ok_or_else(|| "the counting run passed no cancellation checkpoint".to_owned())
    }

    /// What a pause saw before the signal.
    #[derive(Default)]
    pub(super) struct Seen {
        pub(super) staging: bool,
        pub(super) scratch: bool,
        /// The run finished before reaching the pause (scheduling made it
        /// pass fewer checkpoints than the counting run).
        pub(super) completed: bool,
    }

    /// Start `row`, park it at checkpoint `k`, signal it, and check that it
    /// exits `128 + signal` leaving nothing behind (and, with `replace`, the
    /// planted destination intact).
    pub(super) fn interrupt_at(
        fx: &Fx,
        row: &Row,
        k: u64,
        signal: libc::c_int,
        replace: bool,
    ) -> Result<Seen, String> {
        let run = Run::new(fx, row);
        if replace {
            for output in &run.inv.outputs {
                seed(output, row.kind);
            }
        }
        let control = TempDir::new().unwrap();
        let ready = control.path().join("ready");
        let mut pause = OsString::from(format!("{k}:"));
        pause.push(ready.as_os_str());
        let extra: Vec<OsString> = if replace {
            vec!["--overwrite".into()]
        } else {
            Vec::new()
        };
        let mut child = run
            .invoker(&extra)
            .env("PBFHOGG_TEST_CANCEL_PAUSE", &pause)
            .spawn();

        let start = Instant::now();
        let mut seen = Seen::default();
        loop {
            if ready.exists() {
                break;
            }
            if child.has_exited() {
                seen.completed = true;
                break;
            }
            ensure!(
                start.elapsed() < READY_TIMEOUT,
                "checkpoint {k} was never reached"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        if seen.completed {
            let out = child.wait();
            ensure!(
                exited_ok(row, &out),
                "a run that finished before checkpoint {k} failed: exit {:?}; stderr:\n{}",
                out.status.code(),
                out.stderr_str()
            );
            return Ok(seen);
        }

        seen.staging = staging_populated(run.out.path());
        seen.scratch = run
            .tmp
            .as_ref()
            .is_some_and(|tmp| scratch_populated(tmp.path()));
        child.signal(signal);
        let out = child.wait();
        let stderr = out.stderr_str();
        let name = signal_name(signal);
        ensure!(
            out.status.code() == Some(exit_code(signal)),
            "{name} at checkpoint {k}: exit {:?}, wanted {}; stderr:\n{stderr}",
            out.status.code(),
            exit_code(signal)
        );
        ensure!(
            stderr.contains(&format!("interrupted by {name}")),
            "{name} at checkpoint {k}: the interruption was not reported; stderr:\n{stderr}"
        );
        if replace {
            for output in &run.inv.outputs {
                ensure!(
                    intact(output, row.kind),
                    "{name} at checkpoint {k} changed the existing {}",
                    output.display()
                );
            }
            ensure!(
                names(run.out.path()) == output_names(&run.inv),
                "{name} at checkpoint {k} left {:?} beside the existing outputs",
                names(run.out.path())
            );
        } else {
            ensure!(
                names(run.out.path()).is_empty(),
                "{name} at checkpoint {k} left {:?} in the output directory",
                names(run.out.path())
            );
        }
        if let Some(tmp) = &run.tmp {
            ensure!(
                names(tmp.path()).is_empty(),
                "{name} at checkpoint {k} left {:?} in --tmp-dir",
                names(tmp.path())
            );
        }
        Ok(seen)
    }

    /// How many ordinal pause points one row's sweep visits at most.
    const PAUSES_PER_ROW: u64 = 16;

    /// Ordinal pause points spread over `1..=count`: every one for a short
    /// run, else an even spread that always includes the first and the last
    /// (just before publication).
    pub(super) fn sample(count: u64) -> Vec<u64> {
        if count <= PAUSES_PER_ROW {
            return (1..=count).collect();
        }
        let mut ks: Vec<u64> = (0..PAUSES_PER_ROW)
            .map(|i| 1 + i * (count - 1) / (PAUSES_PER_ROW - 1))
            .collect();
        ks.dedup();
        ks
    }
}

/// Every output-writing command, SIGINT or SIGTERM at checkpoints across the
/// run (and once while replacing an existing output): the interrupted exit
/// status, nothing at the destination, no `.pbfhogg-*` entry anywhere.
#[cfg(feature = "test-hooks")]
fn interrupted_run_leaves_nothing(fx: &Fx, row: &Row) -> Result<(), String> {
    use interrupted::{checkpoint_count, interrupt_at, sample};
    let count = checkpoint_count(fx, row)?;
    let ks = sample(count);
    let mut staging_seen = false;
    let mut scratch_seen = false;
    let mut interrupted = 0_usize;
    for (i, &k) in ks.iter().enumerate() {
        let signal = if i % 2 == 0 {
            libc::SIGINT
        } else {
            libc::SIGTERM
        };
        let seen = interrupt_at(fx, row, k, signal, false)?;
        if !seen.completed {
            interrupted += 1;
        }
        staging_seen |= seen.staging;
        scratch_seen |= seen.scratch;
    }
    // Replacing: an interrupted run leaves the previous output as it was.
    let mid = ks[ks.len() / 2];
    interrupt_at(fx, row, mid, libc::SIGTERM, true)?;
    ensure!(
        interrupted > 0,
        "no pause landed before the run completed ({count} checkpoints counted)"
    );
    ensure!(
        staging_seen,
        "no pause observed a populated staging entry ({count} checkpoints, pauses at {ks:?})"
    );
    ensure!(
        !row.tmp_dir || scratch_seen,
        "no pause observed scratch payload in the run directory ({count} checkpoints, pauses \
         at {ks:?})"
    );
    Ok(())
}

#[cfg(feature = "test-hooks")]
#[test]
fn an_interrupted_run_publishes_nothing_and_leaves_nothing() {
    run_property(Prop::Interrupted, interrupted_run_leaves_nothing, |_| true);
}

/// A second signal while the first is still being acted on exits at once,
/// with 128 + the second signal's number, and what it leaves is reported by
/// the next run's leftover scan. The paused thread is told to stay parked
/// after seeing the cancellation, so the run cannot finish its cleanup and
/// only the second signal ends it.
#[cfg(feature = "test-hooks")]
#[test]
fn a_second_signal_exits_at_once_and_the_next_run_reports_the_leftovers() {
    use std::time::{Duration, Instant};
    let fx = Fx::new();
    let row = ROWS
        .iter()
        .find(|row| row.command == "add-locations-to-ways")
        .expect("the altw row");
    let count = interrupted::checkpoint_count(&fx, row).unwrap();
    for (first, second) in [
        (libc::SIGINT, libc::SIGINT),
        (libc::SIGTERM, libc::SIGTERM),
        (libc::SIGINT, libc::SIGTERM),
    ] {
        let out_dir = TempDir::new().unwrap();
        let tmp = TempDir::new().unwrap();
        let control = TempDir::new().unwrap();
        let inv = (row.build)(
            &Ctx {
                fx: &fx,
                out: out_dir.path(),
            },
            Input::Good,
        );
        let ready = control.path().join("ready");
        let ack = control.path().join("ready.ack");
        let mut pause = OsString::from(format!("{}:", count.div_ceil(2)));
        pause.push(ready.as_os_str());
        pause.push(":stuck");
        let mut child = CliInvoker::new()
            .args(&inv.args)
            .args(interrupted::visible_scratch_args(row))
            .args(tmp_args(Some(tmp.path())))
            .env("PBFHOGG_TEST_CANCEL_PAUSE", &pause)
            .spawn();
        let wait_for = |path: &Path, child: &mut crate::invoker::RunningCli| {
            let start = Instant::now();
            while !path.exists() {
                assert!(
                    !child.has_exited(),
                    "the run ended before {}",
                    path.display()
                );
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "{} never appeared",
                    path.display()
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        wait_for(&ready, &mut child);
        child.signal(first);
        // The first signal is taken (the paused thread saw the token trip)
        // before the second is sent: two pending standard signals coalesce.
        wait_for(&ack, &mut child);
        let sent = Instant::now();
        child.signal(second);
        let out = child.wait();
        assert_eq!(
            out.status.code(),
            Some(interrupted::exit_code(second)),
            "{} then {}: stderr:\n{}",
            interrupted::signal_name(first),
            interrupted::signal_name(second),
            out.stderr_str()
        );
        assert!(
            sent.elapsed() < Duration::from_secs(5),
            "the second signal did not end the run at once"
        );
        // The stuck run never cleaned up: its staging entry is still there,
        // and the next run in the same directories reports it.
        let left = hidden(out_dir.path());
        assert!(
            left.iter().any(|n| n.starts_with(".pbfhogg-tmp-")),
            "expected the abandoned staging entry, found {left:?}"
        );
        let next = CliInvoker::new()
            .args(&inv.args)
            .args(interrupted::visible_scratch_args(row))
            .args(tmp_args(Some(tmp.path())))
            .run();
        assert!(
            exited_ok(row, &next),
            "the next run failed: {}",
            next.stderr_str()
        );
        let stderr = next.stderr_str();
        assert!(
            stderr.contains("Warning:") && stderr.contains("did not"),
            "the next run did not report the leftovers; stderr:\n{stderr}"
        );
    }
}

/// The commands that write no output honour the same signals: the
/// interrupted exit status and nothing left in the current directory.
#[cfg(feature = "test-hooks")]
#[test]
fn a_command_without_an_output_is_interrupted_too() {
    use std::time::{Duration, Instant};
    let fx = Fx::new();
    let good = fx.pbf(Input::Good);
    let commands: [&[&str]; 5] = [
        &["inspect"],
        &["digest"],
        &["check", "--refs"],
        &["bench-read", "--mode", "pipelined"],
        &["bench-write"],
    ];
    for (i, words) in commands.iter().enumerate() {
        let signal = if i % 2 == 0 {
            libc::SIGINT
        } else {
            libc::SIGTERM
        };
        let cwd = TempDir::new().unwrap();
        let control = TempDir::new().unwrap();
        let ready = control.path().join("ready");
        let mut pause = OsString::from("2:");
        pause.push(ready.as_os_str());
        let mut child = CliInvoker::new()
            .args(*words)
            .arg(&good)
            .current_dir(cwd.path())
            .env("PBFHOGG_TEST_CANCEL_PAUSE", &pause)
            .spawn();
        let start = Instant::now();
        while !ready.exists() {
            assert!(!child.has_exited(), "{words:?} ended before checkpoint 2");
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "{words:?} never paused"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        child.signal(signal);
        let out = child.wait();
        assert_eq!(
            out.status.code(),
            Some(interrupted::exit_code(signal)),
            "{words:?}: stderr:\n{}",
            out.stderr_str()
        );
        assert!(
            names(cwd.path()).is_empty(),
            "{words:?} left {:?}",
            names(cwd.path())
        );
    }
}
