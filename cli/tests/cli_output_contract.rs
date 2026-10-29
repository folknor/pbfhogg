//! The shared output and scratch contract, enforced on every subcommand by
//! one table.
//!
//! The contract is ADR-0017 (outputs are published by the library), ADR-0018
//! (scratch is one run directory and leftovers are reported) and the "Output
//! files", "Scratch files" and "Names pbfhogg leaves behind" sections of
//! `docs/cli/commands.md`. For every subcommand that writes an output
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
        let dir = crate::common::test_scratch();
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
    Quiet,
    #[cfg_attr(not(feature = "test-hooks"), allow(dead_code))]
    Interrupted,
    #[cfg_attr(not(feature = "test-hooks"), allow(dead_code))]
    StagingDir,
    #[cfg_attr(not(feature = "test-hooks"), allow(dead_code))]
    SharedDir,
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
        command: "add-locations-to-ways",
        label: SPARSE_ALTW_LABEL,
        build: add_locations_to_ways_sparse,
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

/// The label of the add-locations-to-ways row pinned to the sparse store.
/// That store is unlinked as soon as it is mapped, so a paused run never
/// shows scratch payload; the interruption sweeps still require it to leave
/// no destination and no staging entry.
const SPARSE_ALTW_LABEL: &str = "add-locations-to-ways --index-type sparse";

fn add_locations_to_ways_sparse(c: &Ctx, i: Input) -> Invocation {
    add_locations_to_ways(c, i).words(&["--index-type", "sparse"])
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
    let dir = crate::common::test_scratch();
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
        let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
    let keep = crate::common::test_scratch();
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
        scratch_homes.push(Some(crate::common::test_scratch()));
    }
    for tmp in &scratch_homes {
        let tmp_path = tmp.as_ref().map(TempDir::path);
        let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let base = crate::common::test_scratch();
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
        let dir = crate::common::test_scratch();
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
        let out_dir = crate::common::test_scratch();
        let tmp = crate::common::test_scratch();
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

/// What an output holds, for comparing two runs: a file output's bytes, or
/// the relative paths of every file under a directory output (the geocode
/// index makes no promise that two builds are byte-identical, so its files
/// are compared by name only).
fn contents(path: &Path, kind: Kind) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                out.push((rel, Vec::new()));
            }
        }
    }
    let mut out = Vec::new();
    match kind {
        Kind::File => out.push((PathBuf::new(), std::fs::read(path).unwrap())),
        Kind::Dir => walk(path, path, &mut out),
    }
    out.sort();
    out
}

/// A bench command's key=value line, which `--quiet` leaves alone: brokkr
/// reads it, and the decision was to keep those blocks as they are.
fn is_bench_key_value(line: &str) -> bool {
    line.split_once('=').is_some_and(|(key, value)| {
        !key.is_empty()
            && !value.is_empty()
            && key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
    })
}

/// `-q, --quiet`: a successful run prints only the warnings a run without it
/// prints (and a bench command's key=value block), exits as it would without
/// it, and writes
/// exactly the outputs a run without it writes (see [`contents`]). `-q` goes
/// after the row's own words, and before the subcommand on a second run:
/// it is global. A text diff is not a row here: a quiet text diff takes no
/// `-o` (`cli_quiet` covers it).
fn quiet_run_prints_only_warnings(fx: &Fx, row: &Row) -> Result<(), String> {
    let plain_dir = crate::common::test_scratch();
    let plain = (row.build)(
        &Ctx {
            fx,
            out: plain_dir.path(),
        },
        Input::Good,
    );
    let reference = run(&plain, &[]);
    ensure!(
        exited_ok(row, &reference),
        "the run without -q failed: exit {:?}; stderr:\n{}",
        reference.status.code(),
        reference.stderr_str()
    );
    ensure!(
        !reference.stderr.is_empty(),
        "the run without -q printed nothing, so -q has nothing to prove here"
    );
    for leading in [false, true] {
        let dir = crate::common::test_scratch();
        let inv = (row.build)(
            &Ctx {
                fx,
                out: dir.path(),
            },
            Input::Good,
        );
        let out = if leading {
            CliInvoker::new().arg("-q").args(&inv.args).run()
        } else {
            run(&inv, &["-q".into()])
        };
        let place = if leading { "before" } else { "after" };
        quiet_streams(row, &out, &reference)
            .map_err(|why| format!("-q {place} the subcommand: {why}"))?;
        ensure!(
            names(dir.path()) == output_names(&inv),
            "-q {place} the subcommand: the output directory holds {:?}, wanted {:?}",
            names(dir.path()),
            output_names(&inv)
        );
        for (quiet_output, plain_output) in inv.outputs.iter().zip(&plain.outputs) {
            ensure!(
                contents(quiet_output, row.kind) == contents(plain_output, row.kind),
                "-q {place} the subcommand changed what {} holds",
                quiet_output.display()
            );
        }
    }
    Ok(())
}

/// A quiet run's exit status and streams: the status of the run without
/// `-q`, nothing on stdout, and on stderr exactly the warnings the run
/// without `-q` printed plus a bench command's key=value block, which must
/// still be there.
fn quiet_streams(row: &Row, out: &CliOutput, reference: &CliOutput) -> Result<(), String> {
    let stderr = out.stderr_str();
    ensure!(
        out.status.code() == reference.status.code(),
        "exit {:?}, without -q {:?}; stderr:\n{stderr}",
        out.status.code(),
        reference.status.code()
    );
    let bench = row.command.starts_with("bench-");
    let reference_stderr = reference.stderr_str();
    let warnings: Vec<&str> = reference_stderr
        .lines()
        .filter(|line| line.starts_with("Warning: "))
        .collect();
    let kept: Vec<&str> = stderr
        .lines()
        .filter(|line| !(bench && is_bench_key_value(line)))
        .collect();
    ensure!(
        kept == warnings,
        "printed {kept:?}, wanted only the warnings {warnings:?}"
    );
    ensure!(
        !bench || stderr.lines().any(|line| line.starts_with("elapsed_ms=")),
        "dropped the bench key=value block; stderr:\n{stderr}"
    );
    ensure!(
        out.stdout.is_empty(),
        "wrote to stdout:\n{}",
        out.stdout_str()
    );
    Ok(())
}

/// Run one property over every row it applies to, collect every violation,
/// and honour the known-gap markers in both directions.
fn run_property(prop: Prop, check: Check, applies: fn(&Row) -> bool) {
    run_property_on(prop, check, ROWS.iter().filter(|row| applies(row)));
}

/// [`run_property`] over an explicit selection of rows.
fn run_property_on<'a>(prop: Prop, check: Check, rows: impl Iterator<Item = &'a Row>) {
    let fx = Fx::new();
    let mut failures = Vec::new();
    for row in rows {
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
        for flag in ["--overwrite", "--staging-dir"] {
            if !help.contains(flag) {
                problems.push(format!(
                    "[{}] is in ROWS but `{} --help` offers no {flag}",
                    row.label, row.command
                ));
            }
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
        for flag in ["--overwrite", "--output-dir", "--output <", "--staging-dir"] {
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

#[test]
fn a_quiet_run_prints_only_warnings_and_writes_the_same_outputs() {
    run_property(Prop::Quiet, quiet_run_prints_only_warnings, |row| {
        row.label != "diff (text)"
    });
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
// learns how many checkpoints a run of the row passes, and the tier-1 sweep
// pauses at ordinal positions spread over that range (`mod tier3` pauses at
// every one of them). Ordinals are positions, not
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
        if row.command == "add-locations-to-ways" && row.label != SPARSE_ALTW_LABEL {
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
            let out = crate::common::test_scratch();
            let tmp = row.tmp_dir.then(crate::common::test_scratch);
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
        let control = crate::common::test_scratch();
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
        let control = crate::common::test_scratch();
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
        !row.tmp_dir || row.label == SPARSE_ALTW_LABEL || scratch_seen,
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

/// What one entry under a snapshotted directory is: a directory, a file and
/// its bytes, or a symlink and its target.
#[cfg(feature = "test-hooks")]
#[derive(Debug, PartialEq, Eq)]
enum Snapshot {
    Dir,
    File(Vec<u8>),
    Link(PathBuf),
}

/// Every entry under `dir` at any depth, by path relative to `dir`, sorted.
/// Symlinks are recorded, never followed.
#[cfg(feature = "test-hooks")]
fn tree_snapshot(dir: &Path) -> Vec<(PathBuf, Snapshot)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Snapshot)>) {
        for name in names(dir) {
            let path = dir.join(&name);
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.file_type().is_symlink() {
                out.push((rel, Snapshot::Link(std::fs::read_link(&path).unwrap())));
            } else if meta.is_dir() {
                out.push((rel, Snapshot::Dir));
                walk(root, &path, out);
            } else {
                out.push((rel, Snapshot::File(std::fs::read(&path).unwrap())));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Every name pbfhogg leaves behind anywhere under `dir`.
#[cfg(feature = "test-hooks")]
fn hidden_anywhere(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for name in names(dir) {
        let path = dir.join(&name);
        if name.starts_with(".pbfhogg-") {
            found.push(path);
        } else if path.is_dir() && !path.is_symlink() {
            found.extend(hidden_anywhere(&path));
        }
    }
    found
}

/// The `--staging-dir` guarantee (`docs/cli/commands.md`, "Names
/// pbfhogg leaves behind"): a run killed outright, with no chance to clean
/// up, leaves its staging only in `--staging-dir` and its scratch only in
/// `--tmp-dir`, and nothing beside its outputs, for every output form a
/// caller can pass. The next run in the same directories succeeds and
/// reports the staging entry it finds there.
#[cfg(feature = "test-hooks")]
fn killed_run_leaves_only_what_its_directories_hold(fx: &Fx, row: &Row) -> Result<(), String> {
    killed_run_in(fx, row, false)
}

/// The same guarantee for the layout a caller is advised to use, one private
/// directory passed as both `--staging-dir` and `--tmp-dir`, on every row
/// that takes both: the killed run's staging entries and its run directory
/// end up side by side in it and nowhere else, the next run reports both
/// and changes neither, removing the directory whole loses no output, and a
/// replacing run through a fresh one leaves it empty. Rows without
/// `--tmp-dir` make no run scratch, so `--staging-dir` alone is their whole
/// layout and the property above covers them.
#[cfg(feature = "test-hooks")]
fn killed_run_sharing_one_directory_leaves_everything_there(
    fx: &Fx,
    row: &Row,
) -> Result<(), String> {
    killed_run_in(fx, row, true)
}

/// A run killed outright with `--staging-dir` (and `--tmp-dir` where the
/// row takes it; with `shared`, the same directory for both).
#[cfg(feature = "test-hooks")]
#[allow(clippy::too_many_lines)]
fn killed_run_in(fx: &Fx, row: &Row, shared: bool) -> Result<(), String> {
    use std::time::{Duration, Instant};
    let count = interrupted::checkpoint_count(fx, row)?;
    let (out_dir, staging, separate_tmp, control) = (
        crate::common::test_scratch(),
        crate::common::test_scratch(),
        crate::common::test_scratch(),
        crate::common::test_scratch(),
    );
    let tmp = if shared {
        staging.path()
    } else {
        separate_tmp.path()
    };
    let inv = (row.build)(
        &Ctx {
            fx,
            out: out_dir.path(),
        },
        Input::Good,
    );
    let mut extra: Vec<OsString> = vec!["--staging-dir".into(), staging.path().into()];
    extra.extend(interrupted::visible_scratch_args(row));
    extra.extend(tmp_args(row.tmp_dir.then_some(tmp)));
    let ready = control.path().join("ready");
    let mut pause = OsString::from(format!("{}:", count.div_ceil(2)));
    pause.push(ready.as_os_str());
    pause.push(":stuck");
    let mut child = CliInvoker::new()
        .args(&inv.args)
        .args(&extra)
        .env("PBFHOGG_TEST_CANCEL_PAUSE", &pause)
        .spawn();
    let start = Instant::now();
    while !ready.exists() {
        ensure!(!child.has_exited(), "the run ended before its pause");
        ensure!(
            start.elapsed() < Duration::from_secs(10),
            "the pause was never reached"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    child.signal(libc::SIGKILL);
    drop(child.wait());

    let beside = hidden_anywhere(out_dir.path());
    ensure!(
        beside.is_empty(),
        "a killed run left {beside:?} beside its outputs"
    );
    let left = names(staging.path());
    let staged: Vec<&String> = left
        .iter()
        .filter(|n| n.starts_with(".pbfhogg-tmp-"))
        .collect();
    let run_dirs: Vec<&String> = left
        .iter()
        .filter(|n| n.starts_with(".pbfhogg-run-"))
        .collect();
    ensure!(
        !staged.is_empty() && staged.len() + if shared { run_dirs.len() } else { 0 } == left.len(),
        "--staging-dir holds {left:?}, wanted the killed run's staging entries{}",
        if shared { " and its run directory" } else { "" }
    );
    if shared {
        // A run that takes --tmp-dir makes its run directory at startup.
        ensure!(
            run_dirs.len() == 1,
            "the shared directory holds {left:?}, wanted one run directory beside the staging"
        );
    } else {
        let scratch = names(tmp);
        ensure!(
            scratch.iter().all(|n| n.starts_with(".pbfhogg-run-")),
            "--tmp-dir holds {scratch:?}"
        );
    }

    let before = tree_snapshot(staging.path());
    let next = CliInvoker::new().args(&inv.args).args(&extra).run();
    let stderr = next.stderr_str();
    ensure!(exited_ok(row, &next), "the next run failed: {stderr}");
    for entry in staged.iter().take(1).chain(&run_dirs) {
        // Reported, and once: a directory named by both flags is scanned
        // once.
        let reported = stderr
            .matches(&staging.path().join(entry).display().to_string())
            .count();
        ensure!(
            reported == 1,
            "the next run reported {entry} in --staging-dir {reported} times, wanted once; \
             stderr:\n{stderr}"
        );
    }
    ensure!(
        hidden_anywhere(out_dir.path()).is_empty(),
        "the next run left {:?} beside its outputs",
        hidden_anywhere(out_dir.path())
    );
    // Every entry at every depth, with every file's bytes: the next run
    // reports leftovers and touches none of them, so a truncated staging
    // file or a file removed inside a kept run directory fails here.
    let after = tree_snapshot(staging.path());
    ensure!(
        after == before,
        "the next run changed --staging-dir: {:?}, before {:?}",
        after.iter().map(|(path, _)| path).collect::<Vec<_>>(),
        before.iter().map(|(path, _)| path).collect::<Vec<_>>()
    );
    if !shared {
        return Ok(());
    }

    // The caller's cleanup: the directory goes whole, the outputs stay.
    std::fs::remove_dir_all(staging.path()).unwrap();
    for output in &inv.outputs {
        ensure!(
            replaced(output, row.kind),
            "removing the shared directory took {} with it",
            output.display()
        );
    }
    ensure!(
        names(out_dir.path()) == output_names(&inv),
        "after removing the shared directory the output directory holds {:?}, wanted {:?}",
        names(out_dir.path()),
        output_names(&inv)
    );
    // A fresh private directory for the next run, which replaces the
    // outputs and leaves the directory empty.
    std::fs::create_dir(staging.path()).unwrap();
    let again = CliInvoker::new()
        .args(&inv.args)
        .args(&extra)
        .arg("--overwrite")
        .run();
    ensure!(
        exited_ok(row, &again),
        "a replacing run through a fresh shared directory failed: {}",
        again.stderr_str()
    );
    ensure!(
        names(staging.path()).is_empty(),
        "a successful run left {:?} in the shared directory",
        names(staging.path())
    );
    ensure!(
        names(out_dir.path()) == output_names(&inv),
        "a successful run through the shared directory left {:?}, wanted {:?}",
        names(out_dir.path()),
        output_names(&inv)
    );
    Ok(())
}

#[cfg(feature = "test-hooks")]
#[test]
fn a_killed_run_leaves_disposable_entries_only_in_the_directories_it_was_given() {
    run_property(
        Prop::StagingDir,
        killed_run_leaves_only_what_its_directories_hold,
        |_| true,
    );
}

#[cfg(feature = "test-hooks")]
#[test]
fn one_directory_can_serve_as_both_staging_and_scratch_and_be_removed_whole() {
    run_property(
        Prop::SharedDir,
        killed_run_sharing_one_directory_leaves_everything_there,
        |row| row.tmp_dir,
    );
}

// ---------------------------------------------------------------------------
// Killed inside publication
// ---------------------------------------------------------------------------
//
// Publication is not cancellable (ADR-0019), so no checkpoint lies inside it
// and the checkpoint pauses above all land before it. These tests stop a run
// after each namespace step publication announces (the CLI's
// `PBFHOGG_TEST_PUBLISH_*` hooks, `cli/src/signals.rs`) and SIGKILL it there.
// A counting run records the scenario's steps first, under the same faults,
// so every step the scenario reaches is visited and a new step is visited
// without editing the test. The expected state at each kill comes from
// replaying the recorded steps up to it through a per-output model; the
// library test `every_publication_announces_its_namespace_steps_in_order`
// pins the traces themselves. One output form per shape stands for all
// commands: publication is one routine behind every shape, and the
// per-command `--staging-dir` wiring is the properties above.

#[cfg(feature = "test-hooks")]
mod in_publication {
    use super::*;
    use std::time::{Duration, Instant};

    /// Which outputs exist before the run (planted, and replaced with
    /// `--overwrite`).
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) enum Seeded {
        None,
        All,
        /// Only the last output, so a group mixes a replaced member with a
        /// fresh one.
        Last,
    }

    pub(super) struct Scenario {
        /// The row label of the command that stands for the output shape.
        pub(super) row: &'static str,
        pub(super) seeded: Seeded,
        /// `PBFHOGG_TEST_PUBLISH_FAULTS`.
        pub(super) faults: &'static str,
        /// The faults make the run fail.
        pub(super) fails: bool,
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Dest {
        Old,
        New,
        Absent,
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Backup {
        None,
        Empty,
        Old,
    }

    /// Every output's expected destination and backup after the steps in
    /// `trace`, from the outputs that were planted. In these scenarios a
    /// backup at a step is empty, holds the whole previous output, or is
    /// gone. A removal only deletes inside its container, so one killed
    /// mid-removal, between two steps, holds part of the previous output;
    /// so does one whose removal failed partway, which no scenario here
    /// injects.
    fn model(trace: &[String], seeded: &[bool]) -> Result<Vec<(Dest, Backup)>, String> {
        let mut state: Vec<(Dest, Backup)> = seeded
            .iter()
            .map(|&here| (if here { Dest::Old } else { Dest::Absent }, Backup::None))
            .collect();
        for step in trace {
            let (name, member) = match step.split_once(':') {
                Some((name, k)) => (name, Some(k.parse::<usize>().map_err(|e| e.to_string())?)),
                None => (step.as_str(), None),
            };
            let Some(k) = member else { continue };
            let entry = state
                .get_mut(k)
                .ok_or_else(|| format!("step {step} names no output"))?;
            match name {
                "backup-reserved" => entry.1 = Backup::Empty,
                "moved-aside" => *entry = (Dest::Absent, Backup::Old),
                "installed" | "linked" => entry.0 = Dest::New,
                "uninstalled" => entry.0 = Dest::Absent,
                "restored" => *entry = (Dest::Old, Backup::Empty),
                "backup-removed" => entry.1 = Backup::None,
                "backup-synced" | "staging-unlinked" | "staging-removed" => {}
                _ => return Err(format!("unknown publication step {step}")),
            }
        }
        Ok(state)
    }

    /// Every entry under a directory, relative, with a file's bytes (`None`
    /// for a directory).
    type Snapshot = Vec<(PathBuf, Option<Vec<u8>>)>;

    fn snapshot(dir: &Path) -> Snapshot {
        fn walk(root: &Path, dir: &Path, out: &mut Snapshot) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                if path.is_dir() && !path.is_symlink() {
                    out.push((rel, None));
                    walk(root, &path, out);
                } else {
                    out.push((rel, Some(std::fs::read(&path).unwrap())));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    fn row(label: &str) -> &'static Row {
        ROWS.iter().find(|row| row.label == label).unwrap()
    }

    /// The header file name that makes a directory one the index builder
    /// agrees to replace (it refuses any other non-empty directory).
    const INDEX_HEADER: &str = "geocode_header.bin";

    /// Plant a previous output with bytes of its own: a file holding
    /// [`USER_DATA`], or an index directory whose header file holds it, so
    /// a backup can be checked byte for byte.
    fn plant(path: &Path, kind: Kind) {
        match kind {
            Kind::File => std::fs::write(path, USER_DATA).unwrap(),
            Kind::Dir => {
                std::fs::create_dir(path).unwrap();
                std::fs::write(path.join(INDEX_HEADER), USER_DATA).unwrap();
            }
        }
    }

    /// `path` holds exactly what [`plant`] put there.
    fn planted(path: &Path, kind: Kind) -> bool {
        match kind {
            Kind::File => intact(path, kind),
            Kind::Dir => {
                path.is_dir()
                    && names(path) == [INDEX_HEADER]
                    && std::fs::read(path.join(INDEX_HEADER)).is_ok_and(|b| b == USER_DATA)
            }
        }
    }

    /// One scenario's directories, planted outputs and command line.
    struct Setup {
        out: TempDir,
        staging: TempDir,
        tmp: TempDir,
        inv: Invocation,
        seeded: Vec<bool>,
        args: Vec<OsString>,
    }

    impl Setup {
        fn new(fx: &Fx, row: &Row, seeded: Seeded) -> Self {
            let (out, staging, tmp) = (
                crate::common::test_scratch(),
                crate::common::test_scratch(),
                crate::common::test_scratch(),
            );
            let inv = (row.build)(
                &Ctx {
                    fx,
                    out: out.path(),
                },
                Input::Good,
            );
            let last = inv.outputs.len() - 1;
            let seeded: Vec<bool> = (0..inv.outputs.len())
                .map(|i| match seeded {
                    Seeded::None => false,
                    Seeded::All => true,
                    Seeded::Last => i == last,
                })
                .collect();
            for (output, &here) in inv.outputs.iter().zip(&seeded) {
                if here {
                    plant(output, row.kind);
                }
            }
            let mut args = inv.args.clone();
            args.extend(["--staging-dir".into(), staging.path().into()]);
            args.extend(tmp_args(row.tmp_dir.then_some(tmp.path())));
            if seeded.contains(&true) {
                args.push("--overwrite".into());
            }
            Self {
                out,
                staging,
                tmp,
                inv,
                seeded,
                args,
            }
        }
    }

    /// The steps one run of the scenario announces.
    fn steps_of(fx: &Fx, scenario: &Scenario) -> Result<Vec<String>, String> {
        let row = row(scenario.row);
        let setup = Setup::new(fx, row, scenario.seeded);
        let control = crate::common::test_scratch();
        let trace = control.path().join("steps");
        let out = CliInvoker::new()
            .args(&setup.args)
            .env("PBFHOGG_TEST_PUBLISH_FAULTS", scenario.faults)
            .env("PBFHOGG_TEST_PUBLISH_STEPS", &trace)
            .run();
        // 101 is the hooks' own failure (a bad spelling, a trace that could
        // not be written) or a panic, never the injected failure a failing
        // scenario wants.
        ensure!(
            out.status.code() != Some(101),
            "the counting run's test hooks failed or it panicked; stderr:\n{}",
            out.stderr_str()
        );
        ensure!(
            exited_ok(row, &out) != scenario.fails,
            "the counting run exited {:?}, wanted {}; stderr:\n{}",
            out.status.code(),
            if scenario.fails {
                "a failure"
            } else {
                "success"
            },
            out.stderr_str()
        );
        let steps: Vec<String> = std::fs::read_to_string(&trace)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        ensure!(!steps.is_empty(), "the counting run announced no step");
        let mut unique = steps.clone();
        unique.sort();
        unique.dedup();
        ensure!(
            unique.len() == steps.len(),
            "a step repeats in {steps:?}, so a pause at it would be ambiguous"
        );
        Ok(steps)
    }

    /// Kill the scenario after step `at` of `steps` and check what it left,
    /// the caller's cleanup, and the next run.
    #[allow(clippy::too_many_lines)]
    fn killed_at(
        fx: &Fx,
        scenario: &Scenario,
        steps: &[String],
        at: usize,
        reference: &[Vec<(PathBuf, Vec<u8>)>],
    ) -> Result<(), String> {
        let row = row(scenario.row);
        let setup = Setup::new(fx, row, scenario.seeded);
        let step = &steps[at];
        let control = crate::common::test_scratch();
        let ready = control.path().join("ready");
        let mut child = CliInvoker::new()
            .args(&setup.args)
            .env("PBFHOGG_TEST_PUBLISH_FAULTS", scenario.faults)
            .env("PBFHOGG_TEST_PUBLISH_PAUSE", step)
            .env("PBFHOGG_TEST_PUBLISH_READY", &ready)
            .spawn();
        let start = Instant::now();
        while !ready.exists() {
            ensure!(!child.has_exited(), "{step}: the run ended before the step");
            ensure!(
                start.elapsed() < Duration::from_secs(10),
                "{step}: the step was never reached"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        child.signal(libc::SIGKILL);
        drop(child.wait());

        // What the kill left: in the output directory only the outputs and
        // backups, each as the steps so far say.
        let expected = model(&steps[..=at], &setup.seeded)?;
        let out_dir = setup.out.path();
        let mut backups = Vec::new();
        for found in hidden_anywhere(out_dir) {
            let name = found.file_name().unwrap().to_string_lossy().into_owned();
            ensure!(
                found.parent() == Some(out_dir) && name.starts_with(".pbfhogg-backup-"),
                "{step}: left {} beside its outputs",
                found.display()
            );
            backups.push(name);
        }
        let names_left = names(out_dir);
        let outputs = output_names(&setup.inv);
        ensure!(
            names_left
                .iter()
                .all(|n| outputs.contains(n) || backups.contains(n)),
            "{step}: the output directory holds {names_left:?}"
        );
        // A whole new output: a file byte for byte as the reference run
        // wrote it; an index with the reference's files that also opens,
        // since two builds need not be byte-identical and the reader's
        // structural validation is what catches a short or empty file.
        let complete = |i: usize, output: &Path| {
            std::fs::symlink_metadata(output).is_ok()
                && contents(output, row.kind) == reference[i]
                && (row.kind == Kind::File
                    || pbfhogg::geocode_index::reader::Reader::open(output).is_ok())
        };
        let mut accounted = 0;
        for (i, ((output, &(dest, backup)), seeded)) in setup
            .inv
            .outputs
            .iter()
            .zip(&expected)
            .zip(&setup.seeded)
            .enumerate()
        {
            let shown = output.display();
            let state_ok = match dest {
                Dest::Old => planted(output, row.kind),
                Dest::New => complete(i, output),
                Dest::Absent => std::fs::symlink_metadata(output).is_err(),
            };
            ensure!(
                state_ok,
                "{step}: {shown} is not {dest:?} (planted: {seeded})"
            );
            let name = output.file_name().unwrap().to_string_lossy().into_owned();
            let suffix = format!(".{name}");
            let mine: Vec<&String> = backups.iter().filter(|b| b.ends_with(&suffix)).collect();
            match backup {
                Backup::None => ensure!(mine.is_empty(), "{step}: {shown} has backups {mine:?}"),
                Backup::Empty | Backup::Old => {
                    ensure!(
                        mine.len() == 1,
                        "{step}: {shown} has backups {mine:?}, wanted one"
                    );
                    accounted += 1;
                    let container = out_dir.join(mine[0]);
                    let inside = names(&container);
                    if backup == Backup::Empty {
                        ensure!(
                            inside.is_empty(),
                            "{step}: {shown}'s backup holds {inside:?}, wanted nothing"
                        );
                    } else {
                        ensure!(
                            inside == [name.clone()] && planted(&container.join(&name), row.kind),
                            "{step}: {shown}'s backup does not hold the whole previous output: \
                             {inside:?}"
                        );
                    }
                }
            }
        }
        ensure!(
            accounted == backups.len(),
            "{step}: backups {backups:?}, expected {accounted}"
        );
        // Disposable entries only where the caller put them.
        let staged = names(setup.staging.path());
        ensure!(
            staged.iter().all(|n| n.starts_with(".pbfhogg-tmp-")),
            "{step}: --staging-dir holds {staged:?}"
        );
        let scratch = names(setup.tmp.path());
        ensure!(
            scratch.iter().all(|n| n.starts_with(".pbfhogg-run-")),
            "{step}: --tmp-dir holds {scratch:?}"
        );

        // The caller's cleanup: both directories go whole and nothing in
        // the output directory changes (a link-installed output's staging
        // name is a second link, so removing it loses nothing).
        let before = snapshot(out_dir);
        let kept: Vec<(String, Snapshot)> = backups
            .iter()
            .map(|b| (b.clone(), snapshot(&out_dir.join(b))))
            .collect();
        for dir in [setup.staging.path(), setup.tmp.path()] {
            std::fs::remove_dir_all(dir).unwrap();
            std::fs::create_dir(dir).unwrap();
        }
        ensure!(
            snapshot(out_dir) == before,
            "{step}: removing --staging-dir and --tmp-dir changed the output directory"
        );

        // The next run replaces the outputs and leaves every backup as the
        // kill left it: a backup is never a run's to remove.
        let mut again = setup.inv.args.clone();
        again.extend(["--staging-dir".into(), setup.staging.path().into()]);
        again.extend(tmp_args(row.tmp_dir.then_some(setup.tmp.path())));
        again.push("--overwrite".into());
        let next = CliInvoker::new().args(&again).run();
        ensure!(
            exited_ok(row, &next),
            "{step}: the next run failed: {}",
            next.stderr_str()
        );
        for (i, output) in setup.inv.outputs.iter().enumerate() {
            ensure!(
                complete(i, output),
                "{step}: the next run did not write the whole {}",
                output.display()
            );
        }
        for (name, contents) in &kept {
            ensure!(
                snapshot(&out_dir.join(name)) == *contents,
                "{step}: the next run changed the backup {name}"
            );
        }
        let mut wanted = outputs.clone();
        wanted.extend(backups.iter().cloned());
        wanted.sort();
        ensure!(
            names(out_dir) == wanted,
            "{step}: after the next run the output directory holds {:?}, wanted {wanted:?}",
            names(out_dir)
        );
        ensure!(
            names(setup.staging.path()).is_empty(),
            "{step}: the next run left {:?} in --staging-dir",
            names(setup.staging.path())
        );
        Ok(())
    }

    /// Every step of one scenario, each killed in a run of its own.
    pub(super) fn killed_at_every_step(scenario: &Scenario) {
        let fx = Fx::new();
        let label = format!("[{} {}]", scenario.row, scenario.faults);
        let steps = steps_of(&fx, scenario).unwrap_or_else(|why| panic!("{label} {why}"));
        // What a whole new output holds (see `contents`), from a run nobody
        // interferes with.
        let row = row(scenario.row);
        let clean = Setup::new(&fx, row, Seeded::None);
        let out = CliInvoker::new().args(&clean.args).run();
        assert!(
            exited_ok(row, &out),
            "{label} the reference run failed: {}",
            out.stderr_str()
        );
        let reference: Vec<Vec<(PathBuf, Vec<u8>)>> = clean
            .inv
            .outputs
            .iter()
            .map(|output| contents(output, row.kind))
            .collect();
        let failures: Vec<String> = (0..steps.len())
            .filter_map(|at| killed_at(&fx, scenario, &steps, at, &reference).err())
            .collect();
        assert!(
            failures.is_empty(),
            "{label} steps {steps:?}:\n{}",
            failures.join("\n")
        );
    }
}

/// One test per scenario: `name`, the row standing for the output shape,
/// which outputs are planted, the faults, and whether they fail the run.
#[cfg(feature = "test-hooks")]
macro_rules! killed_in_publication {
    ($($name:ident: $row:expr, $seeded:ident, $faults:expr, $fails:expr;)*) => {$(
        #[test]
        fn $name() {
            in_publication::killed_at_every_step(&in_publication::Scenario {
                row: $row,
                seeded: in_publication::Seeded::$seeded,
                faults: $faults,
                fails: $fails,
            });
        }
    )*};
}

#[cfg(feature = "test-hooks")]
mod killed_in_publication {
    use super::in_publication;

    killed_in_publication! {
        a_file_published_exclusively: "cat", None, "", false;
        a_file_published_by_link: "cat", None, "force-link-fallback", false;
        a_file_rolled_back_while_linked: "cat", None, "force-link-fallback,after-link", true;
        a_replaced_file: "cat", All, "", false;
        a_replaced_file_kept_after_a_late_failure: "cat", All, "after-installed:0", true;
        a_group_published_exclusively: "extract --config", None, "", false;
        a_replaced_group: "extract --config", All, "", false;
        a_group_replacing_one_member: "extract --config", Last, "", false;
        a_replaced_group_rolled_back: "extract --config", All, "after-installed:1", true;
        a_replaced_group_whose_rollback_fails: "extract --config", All, "after-installed:1,restore", true;
        a_directory_published_exclusively: "build-geocode-index", None, "", false;
        a_directory_rolled_back_out_of_place: "build-geocode-index", None, "after-installed:0", true;
        a_replaced_directory: "build-geocode-index", All, "", false;
        a_replaced_directory_that_cannot_be_moved_aside: "build-geocode-index", All, "move-aside", true;
        a_replaced_directory_failing_after_its_backup: "build-geocode-index", All, "after-backup", true;
        a_replaced_directory_whose_install_is_refused: "build-geocode-index", All, "cross-mount:0", true;
        a_replaced_directory_rolled_back: "build-geocode-index", All, "after-installed:0", true;
        a_replaced_directory_taken_out_but_not_restored: "build-geocode-index", All, "after-installed:0,restore-rename", true;
        a_replaced_directory_left_unsynced: "build-geocode-index", All, "dir-sync", true;
        a_replaced_directory_whose_backup_is_not_removed: "build-geocode-index", All, "backup-removal", false;
    }
}

/// `--staging-dir` is checked before any work, and its refusals name the
/// flag in their advice: one that holds the output is an output refusal
/// (exit 2), one with no output to stage a usage error (exit 64).
#[test]
fn a_staging_dir_holding_the_output_or_without_an_output_is_refused() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    crate::common::write_base_pbf(&input);
    let output = dir.path().join("out.osm.pbf");
    let refused = CliInvoker::new()
        .args(["sort", input.to_str().unwrap(), "-o"])
        .arg(&output)
        .arg("--staging-dir")
        .arg(dir.path())
        .run();
    let stderr = refused.stderr_str();
    assert_eq!(refused.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("is or contains the directory"), "{stderr}");
    assert!(stderr.contains("--staging-dir"), "{stderr}");
    assert!(!output.exists());
    assert!(hidden(dir.path()).is_empty(), "{:?}", hidden(dir.path()));

    // A text diff to stdout has no output to stage.
    let usage = CliInvoker::new()
        .args(["diff", input.to_str().unwrap(), input.to_str().unwrap()])
        .arg("--staging-dir")
        .arg(dir.path())
        .run();
    assert_eq!(
        usage.status.code(),
        Some(crate::invoker::EXIT_USAGE),
        "{}",
        usage.stderr_str()
    );
    assert!(
        usage.stderr_str().contains("--output"),
        "{}",
        usage.stderr_str()
    );
}

/// A `--staging-dir` that is a bind mount of the output's own device is a
/// different mount, across which publication's rename fails. It is refused
/// before any work on every kernel, including one that reports no `statx`
/// mount id, where comparing devices alone used to let it through to fail
/// at the final rename. The bind mount lives in a private user and mount
/// namespace of the child; a host without user namespaces skips. The same
/// setup staging in the bound directory's source publishes, so the refusal
/// is the bind mount's and not the namespace's.
#[cfg(target_os = "linux")]
#[test]
fn a_staging_dir_across_a_bind_mount_of_the_same_device_is_refused_before_any_work() {
    use crate::invoker::{NAMESPACE_BROKEN, NAMESPACE_DENIED};
    use std::os::unix::fs::MetadataExt;
    let root = crate::common::test_scratch();
    let out_dir = root.path().join("out");
    let source = root.path().join("source");
    let bound = root.path().join("bound");
    for dir in [&out_dir, &source, &bound] {
        std::fs::create_dir(dir).unwrap();
    }
    // One device: only the mount differs, so the device check alone passes.
    let dev = |path: &Path| std::fs::metadata(path).unwrap().dev();
    assert_eq!(dev(&source), dev(&out_dir));
    let input = root.path().join("in.osm.pbf");
    crate::common::write_base_pbf(&input);
    let missing = root.path().join(format!("{MISSING_MARK}.osm.pbf"));
    let output = out_dir.join("out.osm.pbf");
    let sort = |input: &Path, staging: &Path| {
        CliInvoker::new()
            .arg("sort")
            .arg(input)
            .arg("-o")
            .arg(&output)
            .arg("--staging-dir")
            .arg(staging)
            .bind_mounted(&source, &bound)
            .run()
    };

    let refused = sort(&missing, &bound);
    let stderr = refused.stderr_str();
    match refused.status.code() {
        Some(code) if (NAMESPACE_DENIED + 1..=NAMESPACE_DENIED + 6).contains(&code) => {
            crate::common::skip(format!(
                "the host refuses an unprivileged user and mount namespace (step {})",
                code - NAMESPACE_DENIED
            ));
            return;
        }
        Some(code) if (NAMESPACE_BROKEN + 1..=NAMESPACE_BROKEN + 6).contains(&code) => {
            panic!("namespace setup failed at step {}", code - NAMESPACE_BROKEN);
        }
        code => assert_eq!(code, Some(2), "{stderr}"),
    }
    assert!(
        stderr.contains("crossing a filesystem or mount boundary"),
        "{stderr}"
    );
    assert!(stderr.contains(&bound.display().to_string()), "{stderr}");
    assert!(stderr.contains("--staging-dir"), "{stderr}");
    assert!(
        !stderr.contains(MISSING_MARK),
        "the input was opened first: {stderr}"
    );
    assert!(!output.exists());
    assert!(names(&out_dir).is_empty(), "{:?}", names(&out_dir));
    assert!(names(&source).is_empty(), "{:?}", names(&source));

    let published = sort(&input, &source);
    assert!(published.status.success(), "{}", published.stderr_str());
    let plain = root.path().join("plain.osm.pbf");
    CliInvoker::new()
        .arg("sort")
        .arg(&input)
        .arg("-o")
        .arg(&plain)
        .assert_success();
    assert_eq!(
        std::fs::read(&output).unwrap(),
        std::fs::read(&plain).unwrap()
    );
    assert_eq!(names(&out_dir), ["out.osm.pbf"]);
    assert!(names(&source).is_empty(), "{:?}", names(&source));
}

/// Every row, a pause at every ordinal of its counting run rather than a
/// sample: a checkpoint between two tier-1 samples whose exit path leaks (a
/// receiver kept alive past a scope, a guard moved out of reach) is caught
/// here. Each ordinal gets one signal, alternating SIGINT and SIGTERM; both
/// go through the same cancellation, and the replace path and the
/// observed-staging and observed-scratch requirements stay with the tier-1
/// sweep.
///
/// What this does not cover. "Every ordinal" is not "every site": which
/// checkpoint is the k-th varies with scheduling, and a paused run may pass
/// fewer checkpoints than the counting run did, so a pause can find the run
/// already finished. Such a run had no k-th checkpoint to interrupt, but the
/// k-th checkpoint of runs that do have one goes untested; those ordinals
/// are reported, not failed, because independent runs need not agree on
/// their counts. And the fixture's ordinals are not real data's: a phase
/// with one checkpoint here may pass thousands on a planet, which no
/// in-tree fixture reproduces.
#[cfg(feature = "test-hooks")]
fn every_ordinal_unwinds(fx: &Fx, row: &Row) -> Result<(), String> {
    use interrupted::{checkpoint_count, interrupt_at};
    let count = checkpoint_count(fx, row)?;
    let mut failures = Vec::new();
    let mut completed = Vec::new();
    // No early return: a known gap must not hide the ordinals after the
    // first failing one, so the row's verdict is taken over all of them.
    for k in 1..=count {
        let signal = if k % 2 == 1 {
            libc::SIGINT
        } else {
            libc::SIGTERM
        };
        match interrupt_at(fx, row, k, signal, false) {
            Ok(seen) if seen.completed => completed.push(k),
            Ok(_) => {}
            Err(why) => failures.push(why),
        }
    }
    let finished = if completed.is_empty() {
        String::new()
    } else {
        format!("; the run finished before the pause at {completed:?}")
    };
    ensure!(
        failures.is_empty(),
        "{} of {count} ordinals failed{finished}:\n{}",
        failures.len(),
        failures.join("\n")
    );
    ensure!(
        completed.len() < usize::try_from(count).unwrap_or(usize::MAX),
        "no pause landed before the run completed ({count} checkpoints counted)"
    );
    if !completed.is_empty() {
        eprintln!("[{}] {count} checkpoints counted{finished}", row.label);
    }
    Ok(())
}

/// The exhaustive sweep, split so each test stays well inside the check
/// watchdog. Rows are dealt to shards by their index in [`ROWS`], so a new
/// row is covered without touching this module.
#[cfg(feature = "test-hooks")]
mod tier3 {
    use super::*;

    fn shard(n: usize, of: usize) {
        run_property_on(
            Prop::Interrupted,
            every_ordinal_unwinds,
            ROWS.iter()
                .enumerate()
                .filter(|(i, _)| i % of == n)
                .map(|(_, row)| row),
        );
    }

    /// One test per listed shard; the shard count is the length of the
    /// list, so the two cannot drift apart.
    macro_rules! shards {
        ($($name:ident = $n:literal),* $(,)?) => {
            const SHARDS: usize = [$($n),*].len();
            $(
                #[test]
                fn $name() {
                    shard($n, SHARDS);
                }
            )*
        };
    }

    shards! {
        an_interrupt_at_every_checkpoint_unwinds_shard_0 = 0,
        an_interrupt_at_every_checkpoint_unwinds_shard_1 = 1,
        an_interrupt_at_every_checkpoint_unwinds_shard_2 = 2,
        an_interrupt_at_every_checkpoint_unwinds_shard_3 = 3,
    }
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
        let out_dir = crate::common::test_scratch();
        let tmp = crate::common::test_scratch();
        let control = crate::common::test_scratch();
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
        let cwd = crate::common::test_scratch();
        let control = crate::common::test_scratch();
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

// ---------------------------------------------------------------------------
// OSC framing: one name rule for every producer
// ---------------------------------------------------------------------------

/// One OSC producer: how to invoke it given the fixtures and an output name.
struct OscProducer {
    label: &'static str,
    /// Words after the subcommand name's inputs, built per output path.
    build: fn(&Path, &Path, &Path) -> Vec<OsString>,
}

fn osc_args(words: &[&str], paths: &[&Path], tail: &[&str]) -> Vec<OsString> {
    let mut args: Vec<OsString> = words.iter().map(OsString::from).collect();
    args.extend(paths.iter().map(|p| p.as_os_str().to_owned()));
    args.extend(tail.iter().map(OsString::from));
    args
}

/// Every command that writes OSC. `diff --format osc` is the only route to
/// the derive-changes writer (it has no subcommand of its own) and it needs
/// `-o`, so there is no stdout form; `diff` has a sequential and a sharded
/// driver, selected by `-j`, and both are listed. All follow one rule: `.osc`
/// plain, `.osc.gz` gzip, any other name refused before any work.
const OSC_PRODUCERS: &[OscProducer] = &[
    OscProducer {
        label: "merge-changes",
        build: |osc, _, out| osc_args(&["merge-changes"], &[osc, Path::new("-o"), out], &[]),
    },
    OscProducer {
        label: "tags-filter --input-kind osc",
        build: |osc, _, out| {
            osc_args(
                &["tags-filter"],
                &[osc],
                &[
                    "--input-kind",
                    "osc",
                    "-o",
                    &out.to_string_lossy(),
                    "amenity",
                ],
            )
        },
    },
    OscProducer {
        label: "diff --format osc -j 1",
        build: |_, pbfs, out| diff_osc_args(pbfs, out, "1"),
    },
    OscProducer {
        label: "diff --format osc -j 2",
        build: |_, pbfs, out| diff_osc_args(pbfs, out, "2"),
    },
];

/// `pbfs` is the directory holding `old.osm.pbf` and `new.osm.pbf`.
fn diff_osc_args(pbfs: &Path, out: &Path, jobs: &str) -> Vec<OsString> {
    osc_args(
        &["diff"],
        &[&pbfs.join("old.osm.pbf"), &pbfs.join("new.osm.pbf")],
        &["--format", "osc", "-j", jobs, "-o", &out.to_string_lossy()],
    )
}

/// Writes the fixtures every producer reads: `in.osc`, `old.osm.pbf`,
/// `new.osm.pbf`.
fn write_osc_fixtures(dir: &Path) -> PathBuf {
    let osc = dir.join("in.osc");
    std::fs::write(
        &osc,
        b"<osmChange version=\"0.6\"><create>\
          <node id=\"1000\" version=\"1\" lat=\"0.001\" lon=\"0.002\">\
          <tag k=\"amenity\" v=\"cafe\"/></node></create></osmChange>",
    )
    .unwrap();
    crate::common::write_test_pbf_sorted(
        &dir.join("old.osm.pbf"),
        &crate::common::generate_nodes(4, 1),
        &[],
        &[],
    );
    crate::common::write_test_pbf_sorted(
        &dir.join("new.osm.pbf"),
        &crate::common::generate_nodes(5, 1),
        &[],
        &[],
    );
    osc
}

/// Names that are neither `.osc` nor `.osc.gz`. `x.OSC` pins the provisional
/// case-sensitive match (HYG-114 leaves that open).
const REFUSED_OSC_NAMES: &[&str] = &["x", "x.gz", "x.xml", "x.osc.gzip", "x.OSC", "x.osc.bz2"];

#[test]
fn every_osc_producer_refuses_a_name_that_is_neither_osc_nor_osc_gz() {
    let dir = crate::common::test_scratch();
    let osc = write_osc_fixtures(dir.path());
    for producer in OSC_PRODUCERS {
        for name in REFUSED_OSC_NAMES {
            let out_dir = crate::common::test_scratch();
            let dest = out_dir.path().join(name);
            let args = (producer.build)(&osc, dir.path(), &dest);
            let result = CliInvoker::new().args(&args).run();
            let label = format!("{} -> {name}", producer.label);
            assert_eq!(
                result.status.code(),
                Some(crate::invoker::EXIT_USAGE),
                "{label}: {}",
                result.stderr_str()
            );
            let stderr = result.stderr_str();
            assert!(
                stderr.contains(".osc.gz") && stderr.contains("must end in .osc"),
                "{label}: refusal does not name both suffixes: {stderr}"
            );
            assert_eq!(
                std::fs::read_dir(out_dir.path()).unwrap().count(),
                0,
                "{label}: refused run left files behind"
            );
        }
    }
}

#[test]
fn osc_gzip_framing_follows_the_output_name_for_every_producer() {
    let dir = crate::common::test_scratch();
    let osc = write_osc_fixtures(dir.path());

    let mut failures = Vec::new();
    for producer in OSC_PRODUCERS {
        for (name, expect_gzip) in [("x.osc", false), ("x.osc.gz", true)] {
            let out_dir = crate::common::test_scratch();
            let dest = out_dir.path().join(name);
            let args = (producer.build)(&osc, dir.path(), &dest);
            let result = CliInvoker::new().args(&args).run();
            // A diff that found changes is a completed run that exits 1.
            let completed = result.status.success()
                || (producer.label.starts_with("diff") && result.status.code() == Some(1));
            if !completed {
                failures.push(format!(
                    "{} -> {name}: exited {:?}: {}",
                    producer.label,
                    result.status.code(),
                    result.stderr_str()
                ));
                continue;
            }
            let bytes = std::fs::read(&dest).unwrap();
            let is_gzip = bytes.starts_with(&[0x1f, 0x8b]);
            if is_gzip != expect_gzip {
                failures.push(format!(
                    "{} -> {name}: gzip magic present = {is_gzip}, expected {expect_gzip}",
                    producer.label
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
