//! The input contract, enforced on every subcommand by one table.
//!
//! What a command does with the PBF it is given is the same everywhere, so
//! one table says it for all of them:
//!
//! - A header-only file (a valid PBF with no data blobs) counts as indexed:
//!   no blob lacks indexdata. Every command accepts it without `--force`, and
//!   `inspect --indexed` answers yes and exits 0.
//! - A zero-byte file, and any file whose first blob is not an OSMHeader,
//!   gets the one missing-header error: exit 2, the file named, and no word
//!   about indexdata. `inspect --indexed` is no exception; it exits 2 like
//!   every other command, because the file is unreadable, not unindexed.
//! - An output may not be any file the command reads. The refusal happens
//!   before any work and covers the secondary inputs (`-I`, `-i`, `-e`,
//!   `--polygon`, an `extract --config` file and the polygon files it names),
//!   not only the positional ones.
//!
//! ## Complete by construction
//!
//! The list of subcommands is read from the binary's own `--help`. One that
//! is in neither [`ROWS`] nor [`NO_PBF_INPUT`] fails
//! `every_subcommand_is_in_the_table_or_reads_no_pbf`, as does a table entry
//! the binary no longer has.

#![allow(clippy::unwrap_used, clippy::unwrap_in_result)]

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::invoker::{CliInvoker, CliOutput, subcommands};
use tempfile::TempDir;

/// The one spelling of the missing-header error.
const MISSING_HEADER: &str = "does not start with an OsmHeader blob";

macro_rules! ensure {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            return Err(format!($($arg)*));
        }
    };
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A one-node OSC file.
const OSC: &[u8] = b"<osmChange version=\"0.6\"><create>\
    <node id=\"1000\" version=\"1\" lat=\"0.001\" lon=\"0.002\">\
    <tag k=\"amenity\" v=\"cafe\"/></node></create></osmChange>";

/// A valid PBF holding only a header that declares a history file.
fn write_history_header_only(path: &Path) {
    use pbfhogg::writer::{Compression, PbfWriter};
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    let header = pbfhogg::block_builder::HeaderBuilder::new()
        .sorted()
        .historical()
        .build()
        .unwrap();
    writer.write_header(&header).unwrap();
    writer.flush().unwrap();
}

/// Nodes 1..=5, two street ways over nodes 1..=3 and one relation over both
/// ways: every element kind, a tag `tags-filter highway` matches and enough
/// for the geocode builder to have a street to index.
fn policy_elements(
    first_node: i64,
) -> (
    Vec<crate::common::TestNode>,
    Vec<crate::common::TestWay>,
    Vec<crate::common::TestRelation>,
) {
    // Real metadata: time-filter refuses an input stripped of it.
    let meta = Some(crate::common::TestMeta {
        version: 1,
        timestamp: 1_600_000_000,
        changeset: 1,
        uid: 1,
        user: "u",
        visible: true,
    });
    let mut nodes = crate::common::generate_nodes(5, first_node);
    for node in &mut nodes {
        node.meta = meta;
        // `export` has nothing to write for an untagged node.
        node.tags = vec![("amenity", "cafe")];
    }
    let mut ways = crate::common::generate_ways(2, 1, 3, 1);
    for way in &mut ways {
        way.tags = vec![("highway", "residential"), ("name", "Main Street")];
        way.meta = meta;
    }
    let mut relations = crate::common::generate_relations(1, 1, 2, 1);
    for relation in &mut relations {
        relation.meta = meta;
    }
    (nodes, ways, relations)
}

/// The two inputs [`POLICY`] and [`FIRST_BLOB_ONLY`] read.
///
/// `no-index.osm.pbf` has indexdata on no data blob. `first-blob-only.osm.pbf`
/// has it on its first data blob (nodes 1..=5) and on none after (nodes 6..=10,
/// the ways, the relation): the one shape `has_indexdata`, which reads the
/// first data blob alone, calls indexed.
fn write_policy_fixtures(dir: &Path) {
    let (nodes, ways, relations) = policy_elements(1);
    crate::common::write_test_pbf_non_indexed(
        &dir.join("no-index.osm.pbf"),
        &nodes,
        &ways,
        &relations,
    );

    let head = dir.join("head-indexed.osm.pbf");
    crate::common::write_test_pbf_sorted(&head, &policy_elements(1).0, &[], &[]);
    let tail = dir.join("tail-unindexed.osm.pbf");
    let (nodes, ways, relations) = policy_elements(6);
    crate::common::write_test_pbf_non_indexed(&tail, &nodes, &ways, &relations);
    let mut bytes = std::fs::read(&head).unwrap();
    let tail_bytes = std::fs::read(&tail).unwrap();
    let blobs = crate::common::adversarial::locate_blobs(&tail_bytes);
    assert!(blobs.len() >= 2, "fixture must hold a header and data");
    bytes.extend_from_slice(&tail_bytes[blobs[1].frame_start..]);
    std::fs::write(dir.join("first-blob-only.osm.pbf"), bytes).unwrap();

    // The reverse order: the first data blob unindexed (nodes 1..=5), every
    // later one indexed. The first-blob gates call it unindexed, so a
    // command with one refuses it as unindexed and runs on it under
    // `--force`, where the partial refusal is what stops it.
    let head = dir.join("head-unindexed.osm.pbf");
    crate::common::write_test_pbf_non_indexed(&head, &policy_elements(1).0, &[], &[]);
    let tail = dir.join("tail-indexed.osm.pbf");
    let (nodes, ways, relations) = policy_elements(6);
    crate::common::write_test_pbf_sorted(&tail, &nodes, &ways, &relations);
    let mut bytes = std::fs::read(&head).unwrap();
    let tail_bytes = std::fs::read(&tail).unwrap();
    let blobs = crate::common::adversarial::locate_blobs(&tail_bytes);
    assert!(blobs.len() >= 2, "fixture must hold a header and data");
    bytes.extend_from_slice(&tail_bytes[blobs[1].frame_start..]);
    std::fs::write(dir.join("first-blob-unindexed.osm.pbf"), bytes).unwrap();
}

/// The inputs every row draws from.
struct Fx {
    dir: TempDir,
}

impl Fx {
    fn new() -> Self {
        let dir = crate::common::test_scratch();
        let p = dir.path();
        crate::common::write_base_pbf(&p.join("good.osm.pbf"));
        crate::common::write_changed_pbf(&p.join("changed.osm.pbf"));
        // A valid PBF: a header and no data blob.
        crate::common::write_test_pbf_sorted(&p.join("header-only.osm.pbf"), &[], &[], &[]);
        std::fs::write(p.join("zero.osm.pbf"), b"").unwrap();
        // The data blobs of a good file with its header cut off.
        let bytes = std::fs::read(p.join("good.osm.pbf")).unwrap();
        let blobs = crate::common::adversarial::locate_blobs(&bytes);
        assert!(blobs.len() >= 2, "fixture must hold a header and data");
        std::fs::write(p.join("headerless.osm.pbf"), &bytes[blobs[1].frame_start..]).unwrap();
        // The same file with its header frame repeated: a second OSMHeader.
        let mut twice = bytes[..blobs[0].blob_end].to_vec();
        twice.extend_from_slice(&bytes);
        std::fs::write(p.join("two-headers.osm.pbf"), twice).unwrap();
        // The same file cut halfway through its last blob's payload: that
        // blob's header declares a datasize running past EOF.
        let last = blobs.last().unwrap();
        let cut = last.blob_start + (last.blob_end - last.blob_start) / 2;
        std::fs::write(p.join("payload-past-eof.osm.pbf"), &bytes[..cut]).unwrap();
        // The same two-header shape without indexdata on any data blob, for
        // getid's walker on the forced unindexed route.
        crate::common::write_test_pbf_non_indexed(
            &p.join("unindexed.osm.pbf"),
            &crate::common::generate_nodes(5, 1),
            &[],
            &[],
        );
        let unindexed = std::fs::read(p.join("unindexed.osm.pbf")).unwrap();
        let blobs = crate::common::adversarial::locate_blobs(&unindexed);
        assert!(blobs.len() >= 2, "fixture must hold a header and data");
        let mut twice = unindexed[..blobs[0].blob_end].to_vec();
        twice.extend_from_slice(&unindexed);
        std::fs::write(p.join("two-headers-unindexed.osm.pbf"), twice).unwrap();
        // A header-only file whose header declares a history file, which
        // time-filter reads through its other path.
        write_history_header_only(&p.join("header-only-history.osm.pbf"));
        write_policy_fixtures(p);
        // Both partially indexed files under the history header, for the
        // same.
        for name in ["first-blob-only", "first-blob-unindexed"] {
            let mut history = std::fs::read(p.join("header-only-history.osm.pbf")).unwrap();
            let partial = std::fs::read(p.join(format!("{name}.osm.pbf"))).unwrap();
            let blobs = crate::common::adversarial::locate_blobs(&partial);
            history.extend_from_slice(&partial[blobs[1].frame_start..]);
            std::fs::write(p.join(format!("{name}-history.osm.pbf")), history).unwrap();
        }
        std::fs::write(p.join("a.osc"), OSC).unwrap();
        std::fs::write(p.join("b.osc"), OSC).unwrap();
        std::fs::write(p.join("good.osc"), OSC).unwrap();
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

struct Ctx<'a> {
    fx: &'a Fx,
    out: &'a Path,
}

struct Invocation {
    args: Vec<OsString>,
}

impl Invocation {
    fn new(words: &[&str]) -> Self {
        Self {
            args: words.iter().map(OsString::from).collect(),
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
}

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Prop {
    HeaderOnly,
    ZeroByte,
    Headerless,
    PayloadPastEof,
}

struct Row {
    command: &'static str,
    label: &'static str,
    /// The invocation with `input` as the PBF the command reads first.
    build: fn(&Ctx, &Path) -> Invocation,
    /// Exit codes that mean success (`diff` exits 1 on differences).
    ok_codes: &'static [i32],
    /// Properties this row is known to violate, with the reason.
    gaps: &'static [(Prop, &'static str)],
}

/// Subcommands that read no PBF, with the reason.
const NO_PBF_INPUT: &[(&str, &str)] = &[("merge-changes", "reads OSC files only")];

const fn row(
    command: &'static str,
    label: &'static str,
    build: fn(&Ctx, &Path) -> Invocation,
) -> Row {
    Row {
        command,
        label,
        build,
        ok_codes: &[0],
        gaps: &[],
    }
}

const ROWS: &[Row] = &[
    row("cat", "cat", cat),
    row("cat", "cat --type node", cat_type),
    row("cat", "cat --type way", cat_type_way),
    row("cat", "cat --type relation", cat_type_relation),
    row("cat", "cat --clean", cat_clean),
    row("cat", "cat --dedupe", cat_dedupe),
    row("sort", "sort", sort),
    row("repack", "repack", repack),
    row("degrade", "degrade", degrade),
    row("degrade", "degrade --strip-indexdata", degrade_strip),
    row("renumber", "renumber", renumber),
    row("tags-filter", "tags-filter", tags_filter),
    row("tags-filter", "tags-filter -R", tags_filter_omit),
    row("export", "export", export),
    Row {
        ok_codes: &[0, 1],
        ..row("diff", "diff old", diff_old)
    },
    Row {
        ok_codes: &[0, 1],
        ..row("diff", "diff new", diff_new)
    },
    Row {
        ok_codes: &[0, 1],
        ..row("diff", "diff --format osc old", diff_osc_old)
    },
    Row {
        ok_codes: &[0, 1],
        ..row("diff", "diff --format osc new", diff_osc_new)
    },
    row("getid", "getid", getid),
    row("getid", "getid --add-referenced", getid_add_referenced),
    row("getid", "getid --invert", getid_invert),
    row("getparents", "getparents", getparents),
    row("getparents", "getparents --add-self", getparents_add_self),
    row("extract", "extract (complete)", extract_complete),
    row("extract", "extract --simple", extract_simple),
    row("extract", "extract --smart", extract_smart),
    row("extract", "extract --config", extract_config),
    row("add-locations-to-ways", "altw", altw),
    row("add-locations-to-ways", "altw external", altw_external),
    row("add-locations-to-ways", "altw auto", altw_auto),
    row("time-filter", "time-filter", time_filter),
    row("time-filter", "time-filter (history)", time_filter_history),
    row("apply-changes", "apply-changes", apply_changes),
    row("bench-merge", "bench-merge", bench_merge),
    // An index with nothing queryable in it is refused on purpose (exit 2);
    // what this row pins is that the header-only file is read, not held
    // against indexdata first.
    Row {
        ok_codes: &[2],
        ..row("build-geocode-index", "build-geocode-index", geocode)
    },
    row("digest", "digest", digest),
    row("inspect", "inspect", inspect_full),
    row("inspect", "inspect --indexed", inspect_indexed),
    row("inspect", "inspect --nodes", inspect_nodes),
    row("inspect", "inspect --get", inspect_get),
    // Exits 1 when the element is not in the file, which a header-only file
    // never holds.
    Row {
        ok_codes: &[0, 1],
        ..row("inspect", "inspect --show", inspect_show)
    },
    row("inspect", "inspect --blocks", inspect_blocks),
    row("inspect", "inspect --extended", inspect_extended),
    row("inspect", "inspect --locations", inspect_locations),
    row("inspect", "inspect --id-ranges", inspect_id_ranges),
    row("inspect", "inspect --anomalies", inspect_anomalies),
    row("inspect", "inspect tags", inspect_tags),
    row("inspect", "inspect tags --type", inspect_tags_typed),
    Row {
        ok_codes: &[0, 1],
        ..row("check", "check", check)
    },
    Row {
        ok_codes: &[0, 1],
        ..row("check", "check --ids", check_ids)
    },
    Row {
        ok_codes: &[0, 1],
        ..row("check", "check --refs", check_refs)
    },
    Row {
        ok_codes: &[0, 1],
        ..row("check", "check --index", check_index)
    },
    row("bench-read", "bench-read", bench_read),
    row(
        "bench-read",
        "bench-read --mode blobreader",
        bench_read_blobreader,
    ),
    row("bench-write", "bench-write", bench_write),
];

fn out_pbf(c: &Ctx) -> PathBuf {
    c.out.join("out.osm.pbf")
}

fn cat(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["cat"])
        .arg(input)
        .arg("-o")
        .arg(out_pbf(c))
}

fn cat_type(c: &Ctx, input: &Path) -> Invocation {
    cat(c, input).words(&["--type", "node"])
}

fn cat_type_way(c: &Ctx, input: &Path) -> Invocation {
    cat(c, input).words(&["--type", "way"])
}

fn cat_type_relation(c: &Ctx, input: &Path) -> Invocation {
    cat(c, input).words(&["--type", "relation"])
}

fn cat_clean(c: &Ctx, input: &Path) -> Invocation {
    cat(c, input).words(&["--clean", "version"])
}

fn cat_type_way_clean(c: &Ctx, input: &Path) -> Invocation {
    cat_type_way(c, input).words(&["--clean", "version"])
}

fn cat_type_all_clean(c: &Ctx, input: &Path) -> Invocation {
    cat(c, input).words(&["--type", "node,way,relation", "--clean", "version"])
}

fn cat_dedupe(c: &Ctx, input: &Path) -> Invocation {
    cat(c, input).words(&["--dedupe"])
}

fn sort(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["sort"])
        .arg(input)
        .arg("-o")
        .arg(out_pbf(c))
}

fn repack(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["repack"])
        .arg(input)
        .arg("-o")
        .arg(out_pbf(c))
}

fn degrade(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["degrade"])
        .arg(input)
        .words(&["--strip-locations"])
        .arg("-o")
        .arg(out_pbf(c))
}

fn degrade_strip(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["degrade"])
        .arg(input)
        .words(&["--strip-indexdata"])
        .arg("-o")
        .arg(out_pbf(c))
}

fn renumber(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["renumber"])
        .arg(input)
        .arg("-o")
        .arg(out_pbf(c))
}

fn tags_filter(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["tags-filter"])
        .arg(input)
        .arg("-o")
        .arg(out_pbf(c))
        .arg("highway")
}

fn tags_filter_omit(c: &Ctx, input: &Path) -> Invocation {
    tags_filter(c, input).words(&["-R"])
}

fn export(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["export"])
        .arg(input)
        .words(&["--type", "node"])
        .arg("-o")
        .arg(c.out.join("out.geojsonseq"))
}

fn diff_old(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["diff"])
        .arg(input)
        .arg(c.fx.path("changed.osm.pbf"))
        .words(&["-j", "1"])
        .arg("-o")
        .arg(c.out.join("diff.txt"))
}

fn diff_old_parallel(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["diff"])
        .arg(input)
        .arg(c.fx.path("changed.osm.pbf"))
        .words(&["-j", "2"])
        .arg("-o")
        .arg(c.out.join("diff.txt"))
}

fn diff_new(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["diff"])
        .arg(c.fx.path("changed.osm.pbf"))
        .arg(input)
        .words(&["-j", "1"])
        .arg("-o")
        .arg(c.out.join("diff.txt"))
}

fn diff_osc_old(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["diff"])
        .arg(input)
        .arg(c.fx.path("changed.osm.pbf"))
        .words(&["--format", "osc"])
        .arg("-o")
        .arg(c.out.join("diff.osc"))
}

fn diff_osc_new(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["diff"])
        .arg(c.fx.path("changed.osm.pbf"))
        .arg(input)
        .words(&["--format", "osc"])
        .arg("-o")
        .arg(c.out.join("diff.osc"))
}

fn getid(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["getid"])
        .arg(input)
        .arg("-o")
        .arg(out_pbf(c))
        .arg("n1")
}

fn getid_add_referenced(c: &Ctx, input: &Path) -> Invocation {
    getid(c, input).words(&["--add-referenced"])
}

fn getid_invert(c: &Ctx, input: &Path) -> Invocation {
    getid(c, input).words(&["--invert"])
}

fn getid_add_referenced_way(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["getid"])
        .arg(input)
        .arg("-o")
        .arg(out_pbf(c))
        .words(&["w1", "--add-referenced"])
}

/// `input` as the `-I` ID source of a getid run over the good file.
fn getid_id_osm_file(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["getid"])
        .arg(c.fx.path("good.osm.pbf"))
        .arg("-o")
        .arg(out_pbf(c))
        .arg("-I")
        .arg(input)
}

/// `input` as the `-I` ID source of a getparents run over the good file.
fn getparents_id_osm_file(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["getparents"])
        .arg(c.fx.path("good.osm.pbf"))
        .arg("-o")
        .arg(out_pbf(c))
        .arg("-I")
        .arg(input)
}

fn getparents(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["getparents"])
        .arg(input)
        .arg("-o")
        .arg(out_pbf(c))
        .arg("n1")
}

fn getparents_add_self(c: &Ctx, input: &Path) -> Invocation {
    getparents(c, input).words(&["--add-self"])
}

/// getparents on its full-scan arm, which a small file reaches only through
/// the hidden threshold instrument.
fn getparents_full_scan(c: &Ctx, input: &Path) -> Invocation {
    getparents(c, input).words(&["--full-scan-min-blobs", "1"])
}

fn extract_complete(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["extract"])
        .arg(input)
        .arg("--bbox=-1,-1,1,1")
        .arg("-o")
        .arg(out_pbf(c))
}

fn extract_simple(c: &Ctx, input: &Path) -> Invocation {
    extract_complete(c, input).words(&["--simple"])
}

fn extract_smart(c: &Ctx, input: &Path) -> Invocation {
    extract_complete(c, input).words(&["--smart"])
}

fn extract_config(c: &Ctx, input: &Path) -> Invocation {
    let config = c.fx.path("extract-config.json");
    std::fs::write(
        &config,
        format!(
            r#"{{"directory":"{}","extracts":[
                {{"output":"a.osm.pbf","bbox":[-1.0,-1.0,1.0,1.0]}}]}}"#,
            c.out.display()
        ),
    )
    .unwrap();
    Invocation::new(&["extract"])
        .arg(input)
        .arg("--config")
        .arg(&config)
        .arg("--simple")
}

fn altw(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["add-locations-to-ways"])
        .arg(input)
        .arg("-o")
        .arg(out_pbf(c))
}

fn altw_external(c: &Ctx, input: &Path) -> Invocation {
    altw(c, input).words(&["--index-type", "external"])
}

fn altw_auto(c: &Ctx, input: &Path) -> Invocation {
    altw(c, input).words(&["--index-type", "auto"])
}

fn time_filter(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["time-filter"])
        .arg(input)
        .arg("-o")
        .arg(out_pbf(c))
        .arg("2030-01-01T00:00:00Z")
}

/// The header-only and both partially indexed inputs become their
/// history-header twins, so the history path is the one that reads them; any
/// other file stays as given.
fn time_filter_history(c: &Ctx, input: &Path) -> Invocation {
    let twin = ["header-only", "first-blob-only", "first-blob-unindexed"]
        .into_iter()
        .find(|name| input == c.fx.path(&format!("{name}.osm.pbf")))
        .map(|name| c.fx.path(&format!("{name}-history.osm.pbf")));
    time_filter(c, twin.as_deref().unwrap_or(input))
}

fn apply_changes(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["apply-changes"])
        .arg(input)
        .arg(c.fx.path("good.osc"))
        .arg("-o")
        .arg(out_pbf(c))
}

fn bench_merge(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["bench-merge"])
        .arg(input)
        .arg(c.fx.path("good.osc"))
        .arg("-o")
        .arg(out_pbf(c))
}

fn geocode(c: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["build-geocode-index"])
        .arg(input)
        .arg("--output-dir")
        .arg(c.out.join("index"))
}

fn digest(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["digest"]).arg(input)
}

fn inspect_full(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["inspect"]).arg(input)
}

fn inspect_indexed(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["inspect"]).arg(input).arg("--indexed")
}

fn inspect_nodes(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["inspect"]).arg(input).arg("--nodes")
}

fn inspect_get(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["inspect"])
        .arg(input)
        .words(&["--get", "file.format"])
}

fn inspect_show(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["inspect"])
        .arg(input)
        .words(&["--show", "n1"])
}

fn inspect_blocks(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["inspect"]).arg(input).arg("--blocks")
}

fn inspect_extended(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["inspect"]).arg(input).arg("--extended")
}

fn inspect_locations(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["inspect"]).arg(input).arg("--locations")
}

fn inspect_id_ranges(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["inspect"]).arg(input).arg("--id-ranges")
}

fn inspect_anomalies(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["inspect"]).arg(input).arg("--anomalies")
}

fn inspect_tags(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["inspect", "tags"]).arg(input)
}

fn inspect_tags_typed(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["inspect", "tags"])
        .arg(input)
        .words(&["--type", "way"])
}

fn check(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["check"]).arg(input)
}

fn check_ids(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["check"]).arg(input).arg("--ids")
}

fn check_refs(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["check"]).arg(input).arg("--refs")
}

fn check_index(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["check"]).arg(input).arg("--index")
}

fn bench_read(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["bench-read"])
        .arg(input)
        .words(&["--mode", "sequential"])
}

fn bench_read_blobreader(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["bench-read"])
        .arg(input)
        .words(&["--mode", "blobreader"])
}

fn bench_write(_: &Ctx, input: &Path) -> Invocation {
    Invocation::new(&["bench-write"]).arg(input)
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

fn run(inv: &Invocation) -> CliOutput {
    CliInvoker::new().args(&inv.args).run()
}

fn exited_ok(row: &Row, out: &CliOutput) -> bool {
    out.status
        .code()
        .is_some_and(|code| row.ok_codes.contains(&code))
}

fn built(fx: &Fx, row: &Row, dir: &Path, input: &Path) -> Invocation {
    (row.build)(&Ctx { fx, out: dir }, input)
}

/// A header-only file is a valid PBF: every command accepts it without
/// `--force`, and none says a word about indexdata.
fn header_only_is_accepted(fx: &Fx, row: &Row) -> Result<(), String> {
    let dir = crate::common::test_scratch();
    let inv = built(fx, row, dir.path(), &fx.path("header-only.osm.pbf"));
    let out = run(&inv);
    let stderr = out.stderr_str();
    ensure!(
        exited_ok(row, &out),
        "exit {:?}, wanted one of {:?}; stderr:\n{stderr}",
        out.status.code(),
        row.ok_codes
    );
    ensure!(
        !stderr.contains("Generate an indexed PBF"),
        "the empty file was held against indexdata; stderr:\n{stderr}"
    );
    Ok(())
}

/// Not a PBF at all: exit 2, the one missing-header error, the file named,
/// and nothing about indexdata.
fn missing_header_is_one_error(fx: &Fx, row: &Row, file: &str) -> Result<(), String> {
    let dir = crate::common::test_scratch();
    let input = fx.path(file);
    let inv = built(fx, row, dir.path(), &input);
    let out = run(&inv);
    let stderr = out.stderr_str();
    ensure!(
        out.status.code() == Some(2),
        "exit {:?}, wanted 2; stderr:\n{stderr}",
        out.status.code()
    );
    ensure!(
        stderr.contains(MISSING_HEADER),
        "wanted {MISSING_HEADER:?}; stderr:\n{stderr}"
    );
    ensure!(
        stderr.contains(file),
        "the error must name {file}; stderr:\n{stderr}"
    );
    ensure!(
        !stderr.contains("Generate an indexed PBF"),
        "an indexdata refusal came before the missing-header error; stderr:\n{stderr}"
    );
    Ok(())
}

fn zero_byte_is_missing_header(fx: &Fx, row: &Row) -> Result<(), String> {
    missing_header_is_one_error(fx, row, "zero.osm.pbf")
}

fn headerless_is_missing_header(fx: &Fx, row: &Row) -> Result<(), String> {
    missing_header_is_one_error(fx, row, "headerless.osm.pbf")
}

/// See [`STOPS_BEFORE_LAST_PAYLOAD`]: a row exempt as stopping early must
/// still succeed, so a stale exemption fails.
fn payload_past_eof_is_located(fx: &Fx, row: &Row) -> Result<(), String> {
    let file = "payload-past-eof.osm.pbf";
    let dir = crate::common::test_scratch();
    let out = run(&built(fx, row, dir.path(), &fx.path(file)));
    let stderr = out.stderr_str();
    if STOPS_BEFORE_LAST_PAYLOAD
        .iter()
        .any(|(label, _)| *label == row.label)
    {
        ensure!(
            exited_ok(row, &out),
            "exempt as stopping early, but exit {:?}: drop the exemption; stderr:\n{stderr}",
            out.status.code()
        );
        return Ok(());
    }
    ensure!(
        out.status.code() == Some(2),
        "exit {:?}, wanted 2; stderr:\n{stderr}",
        out.status.code()
    );
    ensure!(
        stderr.contains(file) && stderr.contains("at byte offset"),
        "the error must name {file} and a byte offset; stderr:\n{stderr}"
    );
    Ok(())
}

type Check = fn(&Fx, &Row) -> Result<(), String>;

/// Run one property over every row, collect every violation, and honour the
/// known-gap markers in both directions.
fn run_property(prop: Prop, check: Check) {
    let fx = Fx::new();
    let mut failures = Vec::new();
    for row in ROWS {
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
// Output aliasing each secondary input
// ---------------------------------------------------------------------------

/// One input flag besides the positional PBF. `build` lays out its files in
/// `dir` and returns the invocation, whose output is the protected file
/// (with `--overwrite`, so only the alias check can refuse it), and that file.
struct Alias {
    command: &'static str,
    flag: &'static str,
    build: fn(&Fx, &Path) -> (Vec<OsString>, PathBuf),
}

const ALIASES: &[Alias] = &[
    Alias {
        command: "getid",
        flag: "-i",
        build: alias_getid_ids,
    },
    Alias {
        command: "getid",
        flag: "-I",
        build: alias_getid_pbf,
    },
    Alias {
        command: "getparents",
        flag: "-i",
        build: alias_getparents_ids,
    },
    Alias {
        command: "getparents",
        flag: "-I",
        build: alias_getparents_pbf,
    },
    Alias {
        command: "tags-filter",
        flag: "-e",
        build: alias_tags_filter_expressions,
    },
    Alias {
        command: "export",
        flag: "-e",
        build: alias_export_expressions,
    },
    Alias {
        command: "extract",
        flag: "--polygon",
        build: alias_extract_polygon,
    },
    Alias {
        command: "extract",
        flag: "--config",
        build: alias_extract_config_file,
    },
    Alias {
        command: "extract",
        flag: "--config (polygon_file)",
        build: alias_extract_config_polygon,
    },
    Alias {
        command: "merge-changes",
        flag: "first OSC input",
        build: alias_merge_changes_first,
    },
    Alias {
        command: "merge-changes",
        flag: "second OSC input",
        build: alias_merge_changes_second,
    },
];

/// Two OSC inputs, the output being the one at `protected`.
fn merge_changes_aliasing(dir: &Path, protected: &str) -> (Vec<OsString>, PathBuf) {
    for name in ["a.osc", "b.osc"] {
        std::fs::write(dir.join(name), OSC).unwrap();
    }
    let mut args = os(&["merge-changes"]);
    args.push(dir.join("a.osc").into());
    args.push(dir.join("b.osc").into());
    args.extend(os(&["-O", "-o"]));
    args.push(dir.join(protected).into());
    (args, dir.join(protected))
}

fn alias_merge_changes_first(_: &Fx, dir: &Path) -> (Vec<OsString>, PathBuf) {
    merge_changes_aliasing(dir, "a.osc")
}

fn alias_merge_changes_second(_: &Fx, dir: &Path) -> (Vec<OsString>, PathBuf) {
    merge_changes_aliasing(dir, "b.osc")
}

const POLYGON: &str = r#"{"type":"Polygon","coordinates":[[[-1,-1],[1,-1],[1,1],[-1,1],[-1,-1]]]}"#;

fn os(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

fn alias_getid_ids(fx: &Fx, dir: &Path) -> (Vec<OsString>, PathBuf) {
    let ids = dir.join("ids.txt");
    std::fs::write(&ids, "n1\n").unwrap();
    let mut args = os(&["getid"]);
    args.push(fx.path("good.osm.pbf").into());
    args.extend(os(&["-i"]));
    args.push(ids.clone().into());
    args.extend(os(&["-O", "-o"]));
    args.push(ids.clone().into());
    (args, ids)
}

fn alias_getid_pbf(fx: &Fx, dir: &Path) -> (Vec<OsString>, PathBuf) {
    let ids = dir.join("ids.osm.pbf");
    std::fs::copy(fx.path("good.osm.pbf"), &ids).unwrap();
    let mut args = os(&["getid"]);
    args.push(fx.path("good.osm.pbf").into());
    args.extend(os(&["-I"]));
    args.push(ids.clone().into());
    args.extend(os(&["-O", "-o"]));
    args.push(ids.clone().into());
    (args, ids)
}

fn alias_getparents_ids(fx: &Fx, dir: &Path) -> (Vec<OsString>, PathBuf) {
    let (mut args, ids) = alias_getid_ids(fx, dir);
    args[0] = "getparents".into();
    (args, ids)
}

fn alias_getparents_pbf(fx: &Fx, dir: &Path) -> (Vec<OsString>, PathBuf) {
    let (mut args, ids) = alias_getid_pbf(fx, dir);
    args[0] = "getparents".into();
    (args, ids)
}

fn alias_tags_filter_expressions(fx: &Fx, dir: &Path) -> (Vec<OsString>, PathBuf) {
    let expressions = dir.join("expressions.txt");
    std::fs::write(&expressions, "highway\n").unwrap();
    let mut args = os(&["tags-filter"]);
    args.push(fx.path("good.osm.pbf").into());
    args.extend(os(&["-e"]));
    args.push(expressions.clone().into());
    args.extend(os(&["-O", "-o"]));
    args.push(expressions.clone().into());
    (args, expressions)
}

fn alias_export_expressions(fx: &Fx, dir: &Path) -> (Vec<OsString>, PathBuf) {
    let (mut args, expressions) = alias_tags_filter_expressions(fx, dir);
    args[0] = "export".into();
    (args, expressions)
}

fn alias_extract_polygon(fx: &Fx, dir: &Path) -> (Vec<OsString>, PathBuf) {
    let polygon = dir.join("area.geojson");
    std::fs::write(&polygon, POLYGON).unwrap();
    let mut args = os(&["extract"]);
    args.push(fx.path("good.osm.pbf").into());
    args.extend(os(&["--simple", "--polygon"]));
    args.push(polygon.clone().into());
    args.extend(os(&["-O", "-o"]));
    args.push(polygon.clone().into());
    (args, polygon)
}

/// A config whose only output is the config file itself.
fn alias_extract_config_file(fx: &Fx, dir: &Path) -> (Vec<OsString>, PathBuf) {
    let config = dir.join("config.json");
    std::fs::write(
        &config,
        r#"{"extracts":[{"output":"config.json","bbox":[-1.0,-1.0,1.0,1.0]}]}"#,
    )
    .unwrap();
    let mut args = os(&["extract"]);
    args.push(fx.path("good.osm.pbf").into());
    args.extend(os(&["--simple", "-O", "--config"]));
    args.push(config.clone().into());
    (args, config)
}

/// A config whose only output is the polygon file it reads.
fn alias_extract_config_polygon(fx: &Fx, dir: &Path) -> (Vec<OsString>, PathBuf) {
    let polygon = dir.join("area.geojson");
    std::fs::write(&polygon, POLYGON).unwrap();
    let config = dir.join("config.json");
    std::fs::write(
        &config,
        r#"{"extracts":[{"output":"area.geojson","polygon_file":"area.geojson"}]}"#,
    )
    .unwrap();
    let mut args = os(&["extract"]);
    args.push(fx.path("good.osm.pbf").into());
    args.extend(os(&["--simple", "-O", "--config"]));
    args.push(config.into());
    (args, polygon)
}

// ---------------------------------------------------------------------------
// What a command does with unindexed input
// ---------------------------------------------------------------------------
//
// The rules are stated in reference/indexdata-trust.md; the outcome is per
// command, and these tables pin it. Each row is one command line and the
// outcome today: refused (and what lifts the refusal), fallen back from
// (silently or with a note), or verified. A change to any of them is a
// decision, and fails here until the row says so.

/// Present in every refusal of a missing index (a few say `base PBF`).
const NO_INDEXDATA: &str = "no blob-level indexdata";
/// The indexing recipe every missing-indexdata refusal prints.
const RECIPE: &str = "Generate an indexed PBF first";
/// The note most fallbacks print.
const BLOBS_NOTE: &str = "Input contains blobs without indexdata";

/// What lifts a refusal.
#[derive(Clone, Copy)]
enum Escape {
    /// `--force`; the forced run exits 0 and, when given, says this.
    Force(Option<&'static str>),
    /// A command-specific flag in place of `--force`; the run with it exits 0
    /// and, when given, says this.
    Flag(&'static str, Option<&'static str>),
    /// Nothing: `--force` is refused by the parser, with this text, before
    /// the input is opened.
    Rejected(&'static str),
}

/// What a fallback tells the operator.
#[derive(Clone, Copy)]
enum Says {
    /// No `Note:` and no `Warning:`.
    Nothing,
    /// This text.
    Text(&'static str),
}

#[derive(Clone, Copy)]
enum Policy {
    /// Exit 2 with `reason`, the indexing recipe, and advice naming exactly
    /// the switch `escape` offers.
    Refuses {
        reason: &'static str,
        escape: Escape,
    },
    /// The command runs anyway, on the slow path, saying `says`.
    FallsBack(Says),
    /// The command reads the index (or its absence) and reports on it.
    Verifies(&'static str),
}

struct PolicyRow {
    label: &'static str,
    build: fn(&Ctx, &Path) -> Invocation,
    /// Exit codes that mean the command ran (`diff` exits 1 on differences,
    /// `inspect --indexed` exits 1 for no).
    exit: &'static [i32],
    policy: Policy,
}

const fn refuses(
    label: &'static str,
    build: fn(&Ctx, &Path) -> Invocation,
    escape: Escape,
) -> PolicyRow {
    PolicyRow {
        label,
        build,
        exit: &[0],
        policy: Policy::Refuses {
            reason: NO_INDEXDATA,
            escape,
        },
    }
}

const fn falls_back(
    label: &'static str,
    build: fn(&Ctx, &Path) -> Invocation,
    says: Says,
) -> PolicyRow {
    PolicyRow {
        label,
        build,
        exit: &[0],
        policy: Policy::FallsBack(says),
    }
}

const FORCE_NOTED: Escape = Escape::Force(Some(BLOBS_NOTE));

/// Every command line the unindexed input meets, with today's outcome.
const POLICY: &[PolicyRow] = &[
    // Refuse unless `--force`.
    refuses(
        "sort",
        sort,
        Escape::Force(Some("sorting without indexdata")),
    ),
    refuses("repack", repack, FORCE_NOTED),
    refuses("tags-filter", tags_filter, FORCE_NOTED),
    refuses("tags-filter -R", tags_filter_omit, FORCE_NOTED),
    refuses("getid", getid, FORCE_NOTED),
    refuses("getid --invert", getid_invert, FORCE_NOTED),
    refuses("getid --add-referenced", getid_add_referenced, FORCE_NOTED),
    refuses("apply-changes", apply_changes, FORCE_NOTED),
    refuses("altw", altw, FORCE_NOTED),
    refuses("extract (complete)", extract_complete, FORCE_NOTED),
    refuses("extract --smart", extract_smart, FORCE_NOTED),
    refuses("cat --type node", cat_type, FORCE_NOTED),
    refuses("cat --type way", cat_type_way, FORCE_NOTED),
    refuses("cat --type way --clean", cat_type_way_clean, FORCE_NOTED),
    refuses("cat --dedupe", cat_dedupe, FORCE_NOTED),
    // degrade's decode path (a transformation that re-encodes elements).
    refuses("degrade --strip-locations", degrade, FORCE_NOTED),
    refuses("inspect --nodes", inspect_nodes, FORCE_NOTED),
    refuses("inspect tags", inspect_tags, FORCE_NOTED),
    // Refuse with no escape.
    PolicyRow {
        label: "renumber",
        build: renumber,
        exit: &[0],
        policy: Policy::Refuses {
            reason: "renumber requires indexdata on every OsmData blob",
            escape: Escape::Rejected("unexpected argument '--force'"),
        },
    },
    // The parser refuses `--force` here; the entry's "accepts --force and
    // then refuses" is not what the binary does.
    PolicyRow {
        label: "altw --index-type external",
        build: altw_external,
        exit: &[0],
        policy: Policy::Refuses {
            reason: "external join requires indexdata",
            escape: Escape::Rejected("--force has no effect with --index-type external"),
        },
    },
    // Refuse unless `--allow-unindexed`.
    PolicyRow {
        label: "build-geocode-index",
        build: geocode,
        exit: &[0],
        policy: Policy::Refuses {
            reason: NO_INDEXDATA,
            escape: Escape::Flag("--allow-unindexed", Some(BLOBS_NOTE)),
        },
    },
    // The legacy spelling grants the same and also overwrite, and says so.
    PolicyRow {
        label: "build-geocode-index --force",
        build: geocode_legacy_force,
        exit: &[0],
        policy: Policy::FallsBack(Says::Text("--force also grants --overwrite")),
    },
    // No gate: these do not ask for an index.
    falls_back(
        "tags-filter --invert-match",
        tags_filter_invert,
        Says::Text(BLOBS_NOTE),
    ),
    falls_back(
        "tags-filter --distrust-metadata",
        tags_filter_distrust,
        Says::Nothing,
    ),
    falls_back("extract --simple", extract_simple, Says::Text(BLOBS_NOTE)),
    falls_back("cat", cat, Says::Nothing),
    // The clean pass decodes every blob exactly once whatever its indexdata,
    // so it has no slower path to gate; selecting every kind is that pass.
    falls_back("cat --clean", cat_clean, Says::Nothing),
    falls_back(
        "cat --type node,way,relation --clean",
        cat_type_all_clean,
        Says::Nothing,
    ),
    falls_back("degrade --strip-indexdata", degrade_strip, Says::Nothing),
    falls_back("time-filter", time_filter, Says::Nothing),
    falls_back("getparents", getparents, Says::Text(BLOBS_NOTE)),
    falls_back("export", export, Says::Nothing),
    PolicyRow {
        label: "diff",
        build: diff_old,
        exit: &[0, 1],
        policy: Policy::FallsBack(Says::Nothing),
    },
    PolicyRow {
        label: "diff --format osc",
        build: diff_osc_old,
        exit: &[0, 1],
        policy: Policy::FallsBack(Says::Text("Using sequential comparison: missing indexdata")),
    },
    falls_back(
        "check",
        check,
        Says::Text("Warning: input has no blob-level indexdata"),
    ),
    falls_back(
        "check --ids",
        check_ids,
        Says::Text("Warning: input has no blob-level indexdata"),
    ),
    falls_back("check --refs", check_refs, Says::Text(BLOBS_NOTE)),
    // Verify rather than trust.
    PolicyRow {
        label: "digest",
        build: digest,
        exit: &[0],
        policy: Policy::Verifies("xxh128 v1"),
    },
    PolicyRow {
        label: "check --index",
        build: check_index,
        exit: &[0],
        policy: Policy::Verifies("0 with indexdata"),
    },
    PolicyRow {
        label: "inspect --indexed",
        build: inspect_indexed,
        exit: &[1],
        policy: Policy::Verifies("Indexed: no"),
    },
    PolicyRow {
        label: "inspect",
        build: inspect_full,
        exit: &[0],
        policy: Policy::Verifies("Indexed:  no"),
    },
];

fn tags_filter_invert(c: &Ctx, input: &Path) -> Invocation {
    tags_filter(c, input).words(&["--invert-match"])
}

fn tags_filter_distrust(c: &Ctx, input: &Path) -> Invocation {
    tags_filter(c, input).words(&["--distrust-metadata"])
}

fn geocode_legacy_force(c: &Ctx, input: &Path) -> Invocation {
    geocode(c, input).words(&["--force"])
}

/// A run in a fresh output directory, with `extra` words last.
fn run_policy_row(fx: &Fx, row: &PolicyRow, input: &Path, extra: &[&str]) -> (CliOutput, String) {
    let dir = crate::common::test_scratch();
    let inv = (row.build)(
        &Ctx {
            fx,
            out: dir.path(),
        },
        input,
    )
    .words(extra);
    let out = run(&inv);
    let text = format!("{}{}", out.stdout_str(), out.stderr_str());
    (out, text)
}

fn ran_with(row: &PolicyRow, out: &CliOutput, text: &str, extra: &str) -> Result<(), String> {
    ensure!(
        out.status.code().is_some_and(|c| row.exit.contains(&c)),
        "{extra}exit {:?}, wanted one of {:?}; output:\n{text}",
        out.status.code(),
        row.exit
    );
    Ok(())
}

fn says_what_it_says(says: Says, text: &str) -> Result<(), String> {
    match says {
        Says::Text(wanted) => ensure!(text.contains(wanted), "wanted {wanted:?}; output:\n{text}"),
        Says::Nothing => ensure!(
            !text.contains("Note:") && !text.contains("Warning:"),
            "wanted silence; output:\n{text}"
        ),
    }
    Ok(())
}

fn policy_holds(fx: &Fx, row: &PolicyRow) -> Result<(), String> {
    let input = fx.path("no-index.osm.pbf");
    let (out, text) = run_policy_row(fx, row, &input, &[]);
    match row.policy {
        Policy::FallsBack(says) => {
            ran_with(row, &out, &text, "")?;
            says_what_it_says(says, &text)
        }
        Policy::Verifies(wanted) => {
            ran_with(row, &out, &text, "")?;
            ensure!(text.contains(wanted), "wanted {wanted:?}; output:\n{text}");
            Ok(())
        }
        Policy::Refuses { reason, escape } => {
            ensure!(
                out.status.code() == Some(2),
                "exit {:?}, wanted the refusal (2); output:\n{text}",
                out.status.code()
            );
            ensure!(text.contains(reason), "wanted {reason:?}; output:\n{text}");
            ensure!(text.contains(RECIPE), "wanted the recipe; output:\n{text}");
            let advice = match escape {
                Escape::Force(_) => Some("Or pass --force to proceed anyway."),
                Escape::Flag(flag, _) => {
                    // Checked below against the flag itself.
                    ensure!(
                        text.contains(&format!("Or pass {flag} to proceed anyway.")),
                        "wanted advice naming {flag}; output:\n{text}"
                    );
                    None
                }
                Escape::Rejected(_) => {
                    ensure!(
                        !text.contains("Or pass"),
                        "advised a switch that does not exist; output:\n{text}"
                    );
                    None
                }
            };
            if let Some(advice) = advice {
                ensure!(text.contains(advice), "wanted {advice:?}; output:\n{text}");
            }
            match escape {
                Escape::Force(note) => lifted_by(fx, row, &input, "--force", note),
                Escape::Flag(flag, note) => lifted_by(fx, row, &input, flag, note),
                Escape::Rejected(parser_text) => {
                    let (out, text) = run_policy_row(fx, row, &input, &["--force"]);
                    ensure!(
                        out.status.code() == Some(2) && text.contains(parser_text),
                        "`--force` must be refused with {parser_text:?}; exit {:?}, output:\n{text}",
                        out.status.code()
                    );
                    Ok(())
                }
            }
        }
    }
}

fn lifted_by(
    fx: &Fx,
    row: &PolicyRow,
    input: &Path,
    flag: &str,
    note: Option<&str>,
) -> Result<(), String> {
    let (out, text) = run_policy_row(fx, row, input, &[flag]);
    ran_with(row, &out, &text, &format!("with {flag}: "))?;
    if let Some(note) = note {
        ensure!(
            text.contains(note),
            "with {flag}: wanted {note:?}; output:\n{text}"
        );
    }
    Ok(())
}

/// What commands do with a file whose first data blob is indexed and later
/// ones are not.
#[derive(Clone, Copy)]
enum Mixed {
    /// Runs; no word about missing indexdata.
    Accepts,
    /// Runs and reports this.
    Reports(&'static str),
    /// Exit 1 (a finding) with this text.
    Violates(&'static str),
    /// Exit 2 with this text.
    Fails(&'static str),
    /// Exit 2 with the refusal a schedule-building header walk gives a
    /// partially indexed file: the typed missing-indexdata error, so the
    /// CLI's indexing advice follows it.
    RefusesPartial,
}

/// How the schedule walks' partial-index refusal reads.
const PARTIAL_INDEX: &str = "the file is partially indexed";

/// The CLI's advice after a missing-indexdata error.
const INDEXING_ADVICE: &str = "Generate an indexed PBF first";

/// One row: label, invocation builder, expected outcome.
type MixedRow = (&'static str, fn(&Ctx, &Path) -> Invocation, Mixed);

/// Every gate above looks at the first data blob only, so this file passes
/// all of them. Rows pin what happens after that: every command refuses a
/// partially indexed input once its reads have seen both kinds of data blob
/// and points at `pbfhogg cat`, except `cat` itself (the remedy), `check
/// --index` (which reports it as a violation) and `inspect` (which
/// describes it). A few commands refuse any unindexed blob with their own
/// reason first.
const FIRST_BLOB_ONLY: &[MixedRow] = &[
    ("sort", sort, Mixed::RefusesPartial),
    ("repack", repack, Mixed::RefusesPartial),
    // tags-filter refuses on every path, -R's reader included, and with
    // --invert-match and --distrust-metadata, which consult no indexdata:
    // the refusal is the command's input contract.
    ("tags-filter", tags_filter, Mixed::RefusesPartial),
    ("tags-filter -R", tags_filter_omit, Mixed::RefusesPartial),
    (
        "tags-filter --invert-match",
        tags_filter_invert,
        Mixed::RefusesPartial,
    ),
    (
        "tags-filter --distrust-metadata",
        tags_filter_distrust,
        Mixed::RefusesPartial,
    ),
    // getid refuses on every path, whichever arm the blob count picks. The
    // way-ID row runs `--add-referenced`'s classify walk, the other its
    // second pass alone, and the `-I` rows read this file as the ID source
    // (getparents shares that reader).
    ("getid", getid, Mixed::RefusesPartial),
    ("getid --invert", getid_invert, Mixed::RefusesPartial),
    (
        "getid --add-referenced",
        getid_add_referenced,
        Mixed::RefusesPartial,
    ),
    (
        "getid --add-referenced w1",
        getid_add_referenced_way,
        Mixed::RefusesPartial,
    ),
    ("getid -I", getid_id_osm_file, Mixed::RefusesPartial),
    (
        "getparents -I",
        getparents_id_osm_file,
        Mixed::RefusesPartial,
    ),
    // getparents refuses on both arms.
    ("getparents", getparents, Mixed::RefusesPartial),
    (
        "getparents --add-self",
        getparents_add_self,
        Mixed::RefusesPartial,
    ),
    (
        "getparents --full-scan-min-blobs 1",
        getparents_full_scan,
        Mixed::RefusesPartial,
    ),
    ("apply-changes", apply_changes, Mixed::RefusesPartial),
    (
        "extract (complete)",
        extract_complete,
        Mixed::RefusesPartial,
    ),
    ("extract --simple", extract_simple, Mixed::RefusesPartial),
    ("extract --smart", extract_smart, Mixed::RefusesPartial),
    // time-filter reads no indexdata on either path, and refuses on both.
    ("time-filter", time_filter, Mixed::RefusesPartial),
    (
        "time-filter (history)",
        time_filter_history,
        Mixed::RefusesPartial,
    ),
    // export reads no indexdata either, and refuses.
    ("export", export, Mixed::RefusesPartial),
    (
        "degrade --strip-indexdata",
        degrade_strip,
        Mixed::RefusesPartial,
    ),
    // cat is the remedy and accepts in every mode: plain cat indexes each
    // blob without indexdata, --type decodes one and filters its actual
    // elements, --dedupe's first pass scans its IDs, and --clean schedules
    // and decodes every blob whatever its indexdata.
    ("cat", cat, Mixed::Accepts),
    ("cat --type node", cat_type, Mixed::Accepts),
    ("cat --dedupe", cat_dedupe, Mixed::Accepts),
    ("cat --clean", cat_clean, Mixed::Accepts),
    // The unindexed tail is decoded although a kind is unselected, and the
    // schedule walk says so.
    (
        "cat --type way --clean",
        cat_type_way_clean,
        Mixed::Reports(BLOBS_NOTE),
    ),
    ("degrade --strip-locations", degrade, Mixed::RefusesPartial),
    ("inspect --nodes", inspect_nodes, Mixed::RefusesPartial),
    ("inspect tags", inspect_tags, Mixed::RefusesPartial),
    ("altw", altw, Mixed::RefusesPartial),
    // Refused at the Pass 1.5 schedule walk, after Pass 1 has run.
    ("build-geocode-index", geocode, Mixed::RefusesPartial),
    // Refused in its one decoding pass rather than at a header walk, at the
    // same blob.
    ("digest", digest, Mixed::RefusesPartial),
    // These refuse any unindexed data blob at their own walk, with their own
    // reason; behind an indexed first blob the partial refusal comes first.
    ("renumber", renumber, Mixed::RefusesPartial),
    (
        "altw --index-type external",
        altw_external,
        Mixed::RefusesPartial,
    ),
    // diff chooses its arm from the first blob, so a partial index is neither
    // the sequential fallback it takes for a wholly unindexed file nor
    // accepted: both block-pair paths refuse it with the typed missing-indexdata
    // error, located at the first unindexed blob, and the CLI adds its
    // indexing advice.
    (
        "diff -j 1",
        diff_old,
        Mixed::Fails("Generate an indexed PBF first"),
    ),
    (
        "diff -j 2",
        diff_old_parallel,
        Mixed::Fails("Generate an indexed PBF first"),
    ),
    (
        "diff --format osc",
        diff_osc_old,
        Mixed::Fails("Generate an indexed PBF first"),
    ),
    (
        "inspect --indexed",
        inspect_indexed,
        Mixed::Reports("Indexed: yes"),
    ),
    // inspect describes the file as it is: its index-only scan falls back to
    // a full decode at the unindexed blob.
    ("inspect", inspect_full, Mixed::Accepts),
    // Some blobs indexed and some not is a violation, unlike a wholly
    // unindexed file.
    (
        "check --index",
        check_index,
        Mixed::Violates("partially indexed"),
    ),
];

/// One row: label, invocation builder, the words added to it, outcome.
type ForcedMixedRow = (
    &'static str,
    fn(&Ctx, &Path) -> Invocation,
    &'static [&'static str],
    Mixed,
);

/// The reverse order: the first data blob unindexed, the rest indexed. A
/// command with a first-blob gate refuses this file as unindexed (the
/// `POLICY` table pins that refusal); each row passes the switch that lifts
/// the gate, if any, and must then refuse the file as partially indexed, as
/// in the other order, or accept it where the command is exempt.
const FIRST_BLOB_UNINDEXED: &[ForcedMixedRow] = &[
    ("sort --force", sort, &["--force"], Mixed::RefusesPartial),
    (
        "repack --force",
        repack,
        &["--force"],
        Mixed::RefusesPartial,
    ),
    (
        "apply-changes --force",
        apply_changes,
        &["--force"],
        Mixed::RefusesPartial,
    ),
    (
        "tags-filter --force",
        tags_filter,
        &["--force"],
        Mixed::RefusesPartial,
    ),
    (
        "tags-filter -R --force",
        tags_filter_omit,
        &["--force"],
        Mixed::RefusesPartial,
    ),
    (
        "tags-filter --invert-match",
        tags_filter_invert,
        &[],
        Mixed::RefusesPartial,
    ),
    (
        "tags-filter --distrust-metadata",
        tags_filter_distrust,
        &[],
        Mixed::RefusesPartial,
    ),
    ("getid --force", getid, &["--force"], Mixed::RefusesPartial),
    (
        "getid --invert --force",
        getid_invert,
        &["--force"],
        Mixed::RefusesPartial,
    ),
    (
        "getid --add-referenced --force",
        getid_add_referenced,
        &["--force"],
        Mixed::RefusesPartial,
    ),
    (
        "getid --add-referenced w1 --force",
        getid_add_referenced_way,
        &["--force"],
        Mixed::RefusesPartial,
    ),
    ("getid -I", getid_id_osm_file, &[], Mixed::RefusesPartial),
    (
        "getparents -I",
        getparents_id_osm_file,
        &[],
        Mixed::RefusesPartial,
    ),
    ("getparents", getparents, &[], Mixed::RefusesPartial),
    (
        "getparents --full-scan-min-blobs 1",
        getparents_full_scan,
        &[],
        Mixed::RefusesPartial,
    ),
    ("time-filter", time_filter, &[], Mixed::RefusesPartial),
    (
        "time-filter (history)",
        time_filter_history,
        &[],
        Mixed::RefusesPartial,
    ),
    ("export", export, &[], Mixed::RefusesPartial),
    ("digest", digest, &[], Mixed::RefusesPartial),
    ("cat", cat, &[], Mixed::Accepts),
    (
        "cat --type node --force",
        cat_type,
        &["--force"],
        Mixed::Accepts,
    ),
    (
        "cat --dedupe --force",
        cat_dedupe,
        &["--force"],
        Mixed::Accepts,
    ),
    ("cat --clean", cat_clean, &[], Mixed::Accepts),
    ("inspect", inspect_full, &[], Mixed::Accepts),
    (
        "check --index",
        check_index,
        &[],
        Mixed::Violates("partially indexed"),
    ),
];

fn first_blob_only_holds(
    fx: &Fx,
    label: &str,
    build: fn(&Ctx, &Path) -> Invocation,
    mixed: Mixed,
) -> Result<(), String> {
    mixed_holds(fx, "first-blob-only.osm.pbf", label, build, &[], mixed)
}

fn mixed_holds(
    fx: &Fx,
    fixture: &str,
    label: &str,
    build: fn(&Ctx, &Path) -> Invocation,
    extra: &[&str],
    mixed: Mixed,
) -> Result<(), String> {
    let dir = crate::common::test_scratch();
    let inv = build(
        &Ctx {
            fx,
            out: dir.path(),
        },
        &fx.path(fixture),
    )
    .words(extra);
    let out = run(&inv);
    let text = format!("{}{}", out.stdout_str(), out.stderr_str());
    let code = out.status.code();
    match mixed {
        Mixed::Accepts => {
            // `diff` and friends exit 1 on findings; none of these rows do.
            ensure!(code == Some(0), "[{label}] exit {code:?}; output:\n{text}");
            ensure!(
                !text.contains(NO_INDEXDATA),
                "[{label}] refused a file the first-blob probe calls indexed; output:\n{text}"
            );
        }
        Mixed::Reports(wanted) => {
            ensure!(code == Some(0), "[{label}] exit {code:?}; output:\n{text}");
            ensure!(
                text.contains(wanted),
                "[{label}] wanted {wanted:?}; output:\n{text}"
            );
        }
        Mixed::Violates(wanted) => {
            ensure!(code == Some(1), "[{label}] exit {code:?}; output:\n{text}");
            ensure!(
                text.contains(wanted),
                "[{label}] wanted {wanted:?}; output:\n{text}"
            );
        }
        Mixed::Fails(wanted) => {
            ensure!(code == Some(2), "[{label}] exit {code:?}; output:\n{text}");
            ensure!(
                text.contains(wanted),
                "[{label}] wanted {wanted:?}; output:\n{text}"
            );
        }
        Mixed::RefusesPartial => {
            ensure!(code == Some(2), "[{label}] exit {code:?}; output:\n{text}");
            for wanted in [PARTIAL_INDEX, INDEXING_ADVICE] {
                ensure!(
                    text.contains(wanted),
                    "[{label}] wanted {wanted:?}; output:\n{text}"
                );
            }
            // No escape switch lifts this refusal, so none is advised.
            for unwanted in ["--force", "--allow-unindexed"] {
                ensure!(
                    !text.contains(unwanted),
                    "[{label}] advises {unwanted:?}; output:\n{text}"
                );
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn every_subcommand_is_in_the_table_or_reads_no_pbf() {
    let commands = subcommands();
    assert!(
        commands.len() >= 20,
        "the clap tree lists only {commands:?}"
    );
    let mut problems = Vec::new();
    for command in &commands {
        let in_table = ROWS.iter().any(|row| row.command == command);
        let no_pbf = NO_PBF_INPUT.iter().any(|(name, _)| name == command);
        match (in_table, no_pbf) {
            (false, false) => problems.push(format!(
                "`{command}` is in neither ROWS nor NO_PBF_INPUT: add a row for it, or say why it reads no PBF"
            )),
            (true, true) => problems.push(format!("`{command}` is in both ROWS and NO_PBF_INPUT")),
            _ => {}
        }
    }
    for name in ROWS
        .iter()
        .map(|row| row.command)
        .chain(NO_PBF_INPUT.iter().map(|(name, _)| *name))
    {
        if !commands.iter().any(|c| c == name) {
            problems.push(format!(
                "`{name}` is in the table but the binary has no such subcommand"
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn a_header_only_file_is_accepted_by_every_command_without_force() {
    run_property(Prop::HeaderOnly, header_only_is_accepted);
}

#[test]
fn a_zero_byte_file_is_the_missing_header_error_in_every_command() {
    run_property(Prop::ZeroByte, zero_byte_is_missing_header);
}

#[test]
fn a_file_without_a_leading_header_is_the_missing_header_error_in_every_command() {
    run_property(Prop::Headerless, headerless_is_missing_header);
}

/// The frame decoder owns the one-header policy, so the commands that walk
/// frames themselves carry no guard of their own (getid's walker, altw's
/// schedule build, apply-changes' header read). This is the proof they still
/// refuse a second OSMHeader: exit 2, the one error, and no output published.
#[test]
fn a_second_header_is_refused_by_the_commands_that_walk_frames_themselves() {
    let fx = Fx::new();
    let mut failures = Vec::new();
    for row in ROWS.iter().filter(|row| {
        matches!(
            row.label,
            "getid" | "apply-changes" | "altw" | "altw external"
        )
    }) {
        let dir = crate::common::test_scratch();
        let out = run(&built(
            &fx,
            row,
            dir.path(),
            &fx.path("two-headers.osm.pbf"),
        ));
        let stderr = out.stderr_str();
        if out.status.code() != Some(2) || !stderr.contains("a second OSMHeader blob") {
            failures.push(format!(
                "[{}] exit {:?}; stderr:\n{stderr}",
                row.label,
                out.status.code()
            ));
        }
        if out_pbf(&Ctx {
            fx: &fx,
            out: dir.path(),
        })
        .exists()
        {
            failures.push(format!("[{}] published an output", row.label));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Rows that legitimately stop reading before the last blob's payload, with
/// where they stop. `reference/truncation-handling.md` allows an early stop;
/// what it forbids is reading a short payload as though it were whole.
const STOPS_BEFORE_LAST_PAYLOAD: &[(&str, &str)] = &[
    ("inspect --indexed", "stops at the first data blob's header"),
    ("inspect --get", "stops after the file header"),
    (
        "inspect --show",
        "stops at the blob holding the element, which precedes the last",
    ),
];

/// A data blob whose declared payload runs past EOF is refused by every
/// command that reads that far: exit 2, the file named, and a byte offset
/// named. The schedule builders get this from the header walk, which refuses
/// the frame before any pread worker sees it, so no builder carries a check
/// of its own. An exempt row must still succeed, so a stale exemption fails.
#[test]
fn a_payload_running_past_eof_is_refused_with_its_location_by_every_command() {
    run_property(Prop::PayloadPastEof, payload_past_eof_is_located);
}

/// getid on unindexed input with `--force` skips the arm estimate and walks
/// the headers (an unindexed input always takes the walker); it refuses a
/// second OSMHeader like the indexed route above. The streaming full-scan
/// arm, which no CLI fixture reaches, takes its refusal from
/// the raw-frame walk (`Frames::next_raw`), which the frame conformance
/// tests cover.
#[test]
fn getid_forced_unindexed_route_refuses_a_second_header() {
    let fx = Fx::new();
    let dir = crate::common::test_scratch();
    let output = dir.path().join("out.osm.pbf");
    let out = CliInvoker::new()
        .arg("getid")
        .arg(fx.path("two-headers-unindexed.osm.pbf"))
        .arg("--force")
        .arg("-o")
        .arg(&output)
        .arg("n1")
        .run();
    let stderr = out.stderr_str();
    assert_eq!(out.status.code(), Some(2), "stderr:\n{stderr}");
    assert!(stderr.contains("a second OSMHeader blob"), "{stderr}");
    assert!(!output.exists(), "published an output");
}

/// getid and getparents pin their input before they read any ID file, so a
/// FIFO input is refused ahead of an `-I` PBF that does not exist, in every
/// mode (getid's include and invert, getparents' walker and full-scan
/// arms); and the `-I` PBF is pinned too, so a FIFO there is refused rather
/// than opened (a blocking open would wait for a writer).
#[cfg(unix)]
#[test]
fn id_commands_pin_their_input_and_id_pbf_before_reading_either() {
    let fx = Fx::new();
    let dir = crate::common::test_scratch();
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    let missing = dir.path().join("missing-ids.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    let modes: [(&str, &[&str]); 4] = [
        ("getid", &[]),
        ("getid", &["--invert"]),
        ("getparents", &[]),
        ("getparents", &["--full-scan-min-blobs", "1"]),
    ];
    for (command, extra) in modes {
        let out = CliInvoker::new()
            .arg(command)
            .arg(&fifo)
            .arg("-o")
            .arg(&output)
            .arg("-I")
            .arg(&missing)
            .args(extra)
            .run();
        let stderr = out.stderr_str();
        assert!(
            !out.status.success(),
            "{command} {extra:?}: stderr:\n{stderr}"
        );
        assert!(
            stderr.contains(&format!("{} is not a regular file", fifo.display())),
            "{command} {extra:?}: the input's refusal must come first:\n{stderr}"
        );
        assert!(!output.exists(), "{command} {extra:?}: published an output");
    }
    for command in ["getid", "getparents"] {
        let out = CliInvoker::new()
            .arg(command)
            .arg(fx.path("good.osm.pbf"))
            .arg("-o")
            .arg(&output)
            .arg("-I")
            .arg(&fifo)
            .run();
        let stderr = out.stderr_str();
        assert!(!out.status.success(), "{command}: stderr:\n{stderr}");
        assert!(
            stderr.contains(&format!("{} is not a regular file", fifo.display())),
            "{command}: {stderr}"
        );
        assert!(!output.exists(), "{command}: published an output");
    }
}

/// inspect pins its input wherever a run reads it more than once: the scan
/// that may fall back from the header walk to a full decode, node
/// statistics, and any run composing the scan with `--indexed` or `--nodes`
/// (the CLI pins those before the first call, even where the scan alone
/// would read once). A FIFO is refused there rather than opened (a blocking
/// open would wait for a writer, and none is attached).
#[cfg(unix)]
#[test]
fn inspect_pins_its_input_wherever_it_reads_it_twice() {
    let dir = crate::common::test_scratch();
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    let modes: [&[&str]; 5] = [
        &[],
        &["--nodes"],
        &["--extended", "--indexed"],
        &["--extended", "--nodes"],
        &["--blocks", "--indexed"],
    ];
    for extra in modes {
        let out = CliInvoker::new()
            .arg("inspect")
            .arg(&fifo)
            .args(extra)
            .run();
        let stderr = out.stderr_str();
        assert!(!out.status.success(), "{extra:?}: stderr:\n{stderr}");
        assert!(
            stderr.contains(&format!("{} is not a regular file", fifo.display())),
            "{extra:?}: {stderr}"
        );
    }
}

/// The `check` selector sets that compose more than one library call on the
/// input, with the pinned opens each run takes: `--ids` opens twice (its
/// probe and its schedule walk), `--index` and `--refs` once each.
const CHECK_COMPOSITIONS: [(&[&str], u64); 5] = [
    (&[], 3),
    (&["--ids", "--refs"], 3),
    (&["--ids", "--index"], 3),
    (&["--index", "--refs"], 2),
    (&["--ids", "--index", "--refs"], 4),
];

/// A check run composing several checks pins its input once, before the
/// first, so a FIFO is refused there rather than opened (a blocking open
/// would wait for a writer, and none is attached). `--ids` alone pins in the
/// library.
#[cfg(unix)]
#[test]
fn check_pins_its_input_when_it_composes_checks() {
    let dir = crate::common::test_scratch();
    let fifo = crate::common::make_fifo(dir.path(), "in.fifo");
    let ids_alone: &[&str] = &["--ids"];
    for extra in CHECK_COMPOSITIONS
        .iter()
        .map(|&(extra, _)| extra)
        .chain([ids_alone])
    {
        let out = CliInvoker::new().arg("check").arg(&fifo).args(extra).run();
        let stderr = out.stderr_str();
        assert!(!out.status.success(), "{extra:?}: stderr:\n{stderr}");
        assert!(
            stderr.contains(&format!("{} is not a regular file", fifo.display())),
            "{extra:?}: {stderr}"
        );
    }
}

/// A check run composing several checks reads every check from the one
/// pinned file, through the CLI's input replacement hook (`signals`
/// `test_hooks`): a file renamed over the input after each pinned open in
/// turn, between the checks as well as inside `--ids`, leaves the report
/// unchanged where opens reach the pinned file (procfs, or after the last
/// open), and otherwise refuses the next open with nothing printed. A CLI
/// that pinned or opened the path per check would report the replacement
/// for the checks after it. One test per selector set, each a few dozen
/// runs of the binary, so each stays inside the per-test watchdog.
#[cfg(all(unix, feature = "test-hooks"))]
mod check_composition {
    use super::{CHECK_COMPOSITIONS, CliInvoker, Path};

    #[test]
    fn bare() {
        reads_every_check_from_the_pinned_input(0);
    }

    #[test]
    fn ids_refs() {
        reads_every_check_from_the_pinned_input(1);
    }

    #[test]
    fn ids_index() {
        reads_every_check_from_the_pinned_input(2);
    }

    #[test]
    fn index_refs() {
        reads_every_check_from_the_pinned_input(3);
    }

    #[test]
    fn ids_index_refs() {
        reads_every_check_from_the_pinned_input(4);
    }

    /// Unique, ordered IDs; the replacement has more nodes, ways and blobs,
    /// so every check's report changes if it reads it.
    fn write(path: &Path, nodes: usize, ways: i64) {
        use crate::common::{TestWay, generate_nodes, write_multi_block_test_pbf};
        let nodes = generate_nodes(nodes, 1);
        let ways: Vec<TestWay> = (0..ways)
            .map(|k| TestWay {
                id: 1_000 + k,
                refs: vec![k + 1, k + 2, k + 3],
                tags: vec![("highway", "primary")],
                meta: None,
            })
            .collect();
        write_multi_block_test_pbf(path, &nodes, &ways, &[], 25);
    }

    /// `fired attempts verified refused attempts_after_fired`, as the CLI's
    /// replacement hook writes them; a missing report fails.
    fn report(path: &Path) -> [u64; 5] {
        let text = std::fs::read_to_string(path).expect("the run writes its replacement report");
        let fields: Vec<u64> = text
            .split_whitespace()
            .map(|f| f.parse().expect("numeric field"))
            .collect();
        fields.try_into().expect("five fields")
    }

    fn reads_every_check_from_the_pinned_input(composition: usize) {
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        let replacement = dir.path().join("replacement.osm.pbf");
        let control = crate::common::test_scratch();
        let run = |extra: &[&str], after: u64, force_path: bool| {
            let report_file = control.path().join("report");
            drop(std::fs::remove_file(&report_file));
            let out = CliInvoker::new()
                .arg("check")
                .arg(&input)
                .args(extra)
                .arg("--json")
                .env("PBFHOGG_TEST_REPLACE_INPUT", &input)
                .env("PBFHOGG_TEST_REPLACE_WITH", &replacement)
                .env("PBFHOGG_TEST_REPLACE_AFTER", after.to_string())
                .env(
                    "PBFHOGG_TEST_FORCE_PIN_PATH_REOPEN",
                    if force_path { "1" } else { "0" },
                )
                .env("PBFHOGG_TEST_REPLACE_REPORT", &report_file)
                .run();
            (out, report(&report_file))
        };

        let (extra, opens) = CHECK_COMPOSITIONS[composition];
        write(&input, 100, 5);
        write(&replacement, 130, 9);
        // Armed past the last open: the replacement never fires, and the run
        // is the reference.
        let (reference, counted) = run(extra, opens + 1, false);
        assert!(
            reference.status.success(),
            "{extra:?}: {}",
            reference.stderr_str()
        );
        assert_eq!(counted, [0, opens, opens, 0, 0], "{extra:?}");
        let expected = reference.stdout_str();
        assert!(
            expected.contains("\"nodes\": 100") || expected.contains("\"node_count\": 100"),
            "{extra:?}: the reference must report the pinned file:\n{expected}"
        );

        for after in 1..=opens {
            for force_path in [true, false] {
                write(&input, 100, 5);
                write(&replacement, 130, 9);
                let host = crate::common::test_host();
                host.invocation
                    .faults()
                    .force_pin_path_reopen
                    .store(force_path, std::sync::atomic::Ordering::Relaxed);
                let procfs = pbfhogg::read::input::PinnedInput::pin(&input, &host)
                    .expect("probe pin")
                    .reopens_through_procfs();
                let (out, [fired, attempts, verified, refused, after_fired]) =
                    run(extra, after, force_path);
                let cell = format!("{extra:?}, replaced after open {after}, procfs {procfs}");
                assert_eq!(fired, 1, "{cell}: the replacement must fire");
                if procfs || after == opens {
                    assert!(out.status.success(), "{cell}: {}", out.stderr_str());
                    assert_eq!(
                        [attempts, verified, refused, after_fired],
                        [opens, opens, 0, opens - after],
                        "{cell}"
                    );
                    assert_eq!(
                        out.stdout_str(),
                        expected,
                        "{cell}: every check must read the pinned file"
                    );
                } else {
                    assert!(!out.status.success(), "{cell}: must refuse");
                    assert!(
                        out.stderr_str().contains("the input was replaced"),
                        "{cell}: {}",
                        out.stderr_str()
                    );
                    assert!(out.stdout_str().is_empty(), "{cell}: nothing printed");
                    assert_eq!(
                        [attempts, verified, refused, after_fired],
                        [after + 1, after, 1, 1],
                        "{cell}"
                    );
                }
            }
        }
    }
}

#[test]
fn inspect_indexed_says_yes_for_a_header_only_file_and_fails_a_zero_byte_one() {
    let fx = Fx::new();
    let out = CliInvoker::new()
        .args(["inspect", "--indexed"])
        .arg(fx.path("header-only.osm.pbf"))
        .run();
    assert_eq!(out.status.code(), Some(0), "stderr:\n{}", out.stderr_str());
    assert!(out.stdout_str().contains("Indexed: yes"));

    let out = CliInvoker::new()
        .args(["inspect", "--indexed"])
        .arg(fx.path("zero.osm.pbf"))
        .run();
    assert_eq!(out.status.code(), Some(2), "stdout:\n{}", out.stdout_str());
    assert!(out.stderr_str().contains(MISSING_HEADER));
    assert!(!out.stdout_str().contains("Indexed"));
}

#[test]
fn an_output_that_is_any_input_the_command_reads_is_refused_before_any_work() {
    let fx = Fx::new();
    let mut failures = Vec::new();
    for alias in ALIASES {
        let dir = crate::common::test_scratch();
        let (args, protected) = (alias.build)(&fx, dir.path());
        let before = std::fs::read(&protected).unwrap();
        let out = CliInvoker::new().args(&args).run();
        let stderr = out.stderr_str();
        let label = format!("{} {}", alias.command, alias.flag);
        if out.status.code() != Some(2) {
            failures.push(format!(
                "[{label}] exit {:?}, wanted 2; stderr:\n{stderr}",
                out.status.code()
            ));
        } else if !stderr.contains("aliases input") {
            failures.push(format!(
                "[{label}] the refusal must say the output aliases an input; stderr:\n{stderr}"
            ));
        }
        if std::fs::read(&protected).ok().as_deref() != Some(&before[..]) {
            failures.push(format!("[{label}] the input file was replaced"));
        }
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".pbfhogg-"))
            .collect();
        if !names.is_empty() {
            failures.push(format!("[{label}] left behind {names:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn every_secondary_input_flag_has_an_alias_case() {
    // The flags that name an input file besides the positional PBF, read from
    // each command's own `--help`; one without a case in ALIASES fails here.
    let expected: &[(&str, &[&str])] = &[
        ("getid", &["-i", "-I"]),
        ("getparents", &["-i", "-I"]),
        ("tags-filter", &["-e"]),
        ("export", &["-e"]),
        ("extract", &["--polygon", "--config"]),
    ];
    // merge-changes reads only OSC files, positional; its two inputs have
    // their own cases above.
    let mut problems = Vec::new();
    for (command, flags) in expected {
        let help = CliInvoker::new()
            .args([command, "--help"])
            .assert_success()
            .stdout_str();
        for flag in *flags {
            if !help.contains(flag) {
                problems.push(format!("`{command} --help` no longer offers {flag}"));
            }
            if !ALIASES
                .iter()
                .any(|a| a.command == *command && a.flag.starts_with(flag))
            {
                problems.push(format!("`{command} {flag}` has no alias case"));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// What each command does with a file that has no indexdata on any data blob:
/// refuse (and what lifts it), fall back (and what it says), or verify. The
/// table is the per-command pin of reference/indexdata-trust.md; the rows
/// follow the code.
#[test]
fn what_each_command_does_with_unindexed_input_is_pinned() {
    let fx = Fx::new();
    let failures: Vec<String> = POLICY
        .iter()
        .filter_map(|row| {
            policy_holds(&fx, row)
                .err()
                .map(|why| format!("[{}] {why}", row.label))
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The probe behind every gate reads the first data blob only, so a file
/// indexed there and nowhere else passes them all; this is what the commands
/// do next.
#[test]
fn what_each_command_does_with_input_indexed_on_its_first_blob_only_is_pinned() {
    let fx = Fx::new();
    let failures: Vec<String> = FIRST_BLOB_ONLY
        .iter()
        .filter_map(|(label, build, mixed)| first_blob_only_holds(&fx, label, *build, *mixed).err())
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn what_each_command_does_with_input_unindexed_on_its_first_blob_only_is_pinned() {
    let fx = Fx::new();
    let failures: Vec<String> = FIRST_BLOB_UNINDEXED
        .iter()
        .filter_map(|(label, build, extra, mixed)| {
            mixed_holds(
                &fx,
                "first-blob-unindexed.osm.pbf",
                label,
                *build,
                extra,
                *mixed,
            )
            .err()
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
