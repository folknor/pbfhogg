//! Every command's `--direct-io` reaches O_DIRECT, witnessed on the binary.
//!
//! `crates/cli/src/lib.rs` wires the flag into each command's options by
//! hand, and the library fault cells pin each command's direct opens only
//! from options the test builds itself, so a CLI arm that dropped the flag
//! would pass them. Each cell here runs one route with `--direct-io` and
//! asserts a direct open, on the run's marker stream, in every direction it
//! declares; not an exact count, which is the library cells' attribution.
//!
//! The table is complete by construction: `cells_cover_every_direct_io_path`
//! fails when a subcommand path carrying the flag has no cell, when a cell
//! names a path without it, and when the directions a path's cells assert
//! differ from what its row in the reach table (`crates/cli/src/io_reach.rs`)
//! claims. That is path-and-direction coverage only: a route that forwards
//! the flag on its own branch (`diff --format osc`, `extract --config`,
//! `inspect --show`) has a cell only where one is declared here, and
//! weakening or deleting one such variant cell passes the check while
//! another cell of its path still asserts the direction.
//!
//! Cells avoid inputs that open O_DIRECT on their own account: IDs are
//! positional (an `-I` PBF is a second direct reader) and no cell passes
//! `--io-uring` (its writer opens O_DIRECT). The per-command backend cells
//! elsewhere (`cli_cat`, `cli_sort`, ...) check content under the flag; these
//! check only that it arrives.

#![allow(clippy::unwrap_used)]
#![cfg_attr(
    not(feature = "linux-direct-io"),
    allow(dead_code, reason = "only the coverage test runs without the feature")
)]

use std::ffi::OsString;
use std::path::Path;

use crate::common::{TestMeta, TestNode, TestRelation, TestWay};

#[derive(Clone, Copy)]
struct Dirs {
    reads: bool,
    writes: bool,
}

const R: Dirs = Dirs {
    reads: true,
    writes: false,
};
const RW: Dirs = Dirs {
    reads: true,
    writes: true,
};

struct Cell {
    name: &'static str,
    /// The subcommand path; the cell's argv starts with it.
    path: &'static [&'static str],
    /// The directions the cell asserts a direct open in.
    dirs: Dirs,
    /// The arguments after the path, over the fixture directory.
    args: fn(&Path) -> Vec<OsString>,
    /// The exit code a completed run gives.
    code: i32,
}

fn os(dir: &Path, words: &[&str]) -> Vec<OsString> {
    words
        .iter()
        .map(|w| match w.strip_prefix('@') {
            Some(name) => dir.join(name).into_os_string(),
            None => OsString::from(w),
        })
        .collect()
}

/// Declares the cells once: the `CELLS` table the coverage test reads, and
/// one test per cell, so each route has its own watchdog budget and rerun.
macro_rules! cells {
    ($($name:ident: $path:expr, $dirs:expr, $code:expr, [$($word:expr),* $(,)?];)*) => {
        pub(super) const CELLS: &[Cell] = &[$(Cell {
            name: stringify!($name),
            path: $path,
            dirs: $dirs,
            args: |dir| os(dir, &[$($word),*]),
            code: $code,
        }),*];

        $(#[cfg(feature = "linux-direct-io")]
        #[test]
        fn $name() {
            super::run_cell(stringify!($name));
        })*
    };
}

/// The cells, and one test each (`backends::<cell>`, tier 1). `@name` is a
/// file in the fixture directory.
mod backends {
    use super::{Cell, R, RW, os};
    cells! {
        inspect_show: &["inspect"], R, 0, ["@base.osm.pbf", "--show", "n1"];
        inspect_unindexed: &["inspect"], R, 0, ["@unindexed.osm.pbf"];
        inspect_nodes: &["inspect"], R, 0, ["@base.osm.pbf", "--nodes"];
        inspect_tags_typed: &["inspect", "tags"], R, 0, ["@base.osm.pbf", "--type", "way"];
        check: &["check"], R, 0, ["@base.osm.pbf"];
        cat: &["cat"], RW, 0, ["@base.osm.pbf", "-o", "@out.osm.pbf"];
        cat_dedupe: &["cat"], RW, 0, ["@base.osm.pbf", "--dedupe", "-o", "@out.osm.pbf"];
        cat_reindex: &["cat"], RW, 0, ["@base.osm.pbf", "--reindex", "-o", "@out.osm.pbf"];
        sort: &["sort"], RW, 0, ["@base.osm.pbf", "-o", "@out.osm.pbf"];
        repack: &["repack"], RW, 0, ["@base.osm.pbf", "-o", "@out.osm.pbf"];
        degrade: &["degrade"], RW, 0, ["@base.osm.pbf", "--strip-locations", "-o", "@out.osm.pbf"];
        renumber: &["renumber"], RW, 0, ["@base.osm.pbf", "-o", "@out.osm.pbf"];
        extract: &["extract"], RW, 0, ["@base.osm.pbf", "--bbox=-180,-90,180,90", "-o", "@out.osm.pbf"];
        extract_config: &["extract"], R, 0, ["@base.osm.pbf", "--config", "@extract.json", "--simple"];
        tags_filter: &["tags-filter"], RW, 0, ["@base.osm.pbf", "-o", "@out.osm.pbf", "highway"];
        tags_filter_omit_referenced: &["tags-filter"], RW, 0, ["@base.osm.pbf", "-o", "@out.osm.pbf", "highway", "-R"];
        getid: &["getid"], RW, 0, ["@base.osm.pbf", "-o", "@out.osm.pbf", "n1"];
        getid_invert: &["getid"], RW, 0, ["@base.osm.pbf", "-o", "@out.osm.pbf", "n1", "--invert"];
        getparents: &["getparents"], RW, 0, ["@base.osm.pbf", "-o", "@out.osm.pbf", "n1"];
        time_filter: &["time-filter"], RW, 0, ["@base.osm.pbf", "-o", "@out.osm.pbf", "2030-01-01T00:00:00Z"];
        add_locations_to_ways: &["add-locations-to-ways"], RW, 0, ["@base.osm.pbf", "-o", "@out.osm.pbf"];
        apply_changes: &["apply-changes"], RW, 0, ["@base.osm.pbf", "@change.osc", "-o", "@out.osm.pbf"];
        // The pair differs, so a completed comparison exits 1.
        diff: &["diff"], R, 1, ["@base.osm.pbf", "@changed.osm.pbf", "-o", "@diff.txt"];
        diff_osc: &["diff"], R, 1, ["@base.osm.pbf", "@changed.osm.pbf", "--format", "osc", "-o", "@diff.osc"];
        digest: &["digest"], R, 0, ["@base.osm.pbf"];
        build_geocode_index: &["build-geocode-index"], R, 0, ["@base.osm.pbf", "--output-dir", "@index"];
    }
}

use backends::CELLS;

/// Nodes 1..=5 with metadata and a tag, two named streets over them and a
/// relation over the streets: every kind, a `highway` match, a street for the
/// geocode builder and timestamps for time-filter. `extra` adds one node, for
/// the second side of a diff.
fn elements(
    extra: bool,
) -> (
    Vec<TestNode<'static>>,
    Vec<TestWay<'static>>,
    Vec<TestRelation<'static>>,
) {
    let meta = Some(TestMeta {
        version: 1,
        timestamp: 1_600_000_000,
        changeset: 1,
        uid: 1,
        user: "u",
        visible: true,
    });
    let mut nodes = crate::common::generate_nodes(if extra { 6 } else { 5 }, 1);
    for node in &mut nodes {
        node.meta = meta;
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

fn write_fixtures(dir: &Path) {
    let (n, w, r) = elements(false);
    crate::common::write_test_pbf_sorted(&dir.join("base.osm.pbf"), &n, &w, &r);
    crate::common::write_test_pbf_non_indexed(&dir.join("unindexed.osm.pbf"), &n, &w, &r);
    let (n, w, r) = elements(true);
    crate::common::write_test_pbf_sorted(&dir.join("changed.osm.pbf"), &n, &w, &r);
    std::fs::write(
        dir.join("change.osc"),
        "<osmChange version=\"0.6\"><create>\
         <node id=\"1000\" version=\"1\" lat=\"0.001\" lon=\"0.002\">\
         <tag k=\"amenity\" v=\"cafe\"/></node></create></osmChange>",
    )
    .unwrap();
    // A relative directory resolves against the config file's own.
    std::fs::write(
        dir.join("extract.json"),
        r#"{"directory":"extracts","extracts":[{"output":"a.osm.pbf","bbox":[-180.0,-90.0,180.0,90.0]}]}"#,
    )
    .unwrap();
    std::fs::create_dir(dir.join("extracts")).unwrap();
}

#[cfg(feature = "linux-direct-io")]
fn run_cell(name: &str) {
    let cell = CELLS.iter().find(|c| c.name == name).unwrap();
    let dir = crate::common::test_scratch();
    write_fixtures(dir.path());
    let markers = dir.path().join("run.markers");
    std::fs::write(&markers, []).unwrap();
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let out = crate::invoker::CliInvoker::new(&invocation_scratch)
        .args(cell.path)
        .args((cell.args)(dir.path()))
        .arg("--direct-io")
        .marker_fifo(&markers)
        .run();
    if out.is_o_direct_unsupported() {
        crate::common::skip("O_DIRECT not supported on this filesystem");
        return;
    }
    assert_eq!(
        out.status.code(),
        Some(cell.code),
        "{name}: {:?} --direct-io; stderr:\n{}",
        cell.path,
        out.stderr_str()
    );
    if cell.dirs.reads {
        crate::invoker::assert_direct_opened(&markers, "read");
    }
    if cell.dirs.writes {
        crate::invoker::assert_direct_opened(&markers, "write");
    }
}

/// Every path carrying `--direct-io` has a cell, every cell's path carries
/// it, and per path the cells assert exactly the directions its reach row
/// claims.
#[test]
fn cells_cover_every_direct_io_path() {
    fn walk(cmd: &clap::Command, path: &[String], out: &mut Vec<Vec<String>>) {
        if cmd.get_arguments().any(|a| a.get_id() == "direct_io") {
            out.push(path.to_vec());
        }
        for sub in cmd.get_subcommands() {
            let mut next = path.to_vec();
            next.push(sub.get_name().to_owned());
            walk(sub, &next, out);
        }
    }
    let mut flagged = Vec::new();
    walk(&pbfhogg_cli::cli_command(), &[], &mut flagged);
    assert!(
        flagged.iter().any(|p| p == &["inspect", "tags"]),
        "the clap walk found only {flagged:?}"
    );
    let mut problems = Vec::new();
    for path in &flagged {
        let path: Vec<&str> = path.iter().map(String::as_str).collect();
        let cells: Vec<&Cell> = CELLS.iter().filter(|c| c.path == path.as_slice()).collect();
        if cells.is_empty() {
            problems.push(format!("{path:?} takes --direct-io but has no cell"));
            continue;
        }
        let claim = pbfhogg_cli::direct_claim(&path).unwrap();
        let reads = cells.iter().any(|c| c.dirs.reads);
        let writes = cells.iter().any(|c| c.dirs.writes);
        if (reads, writes) != (claim.reads, claim.writes) {
            problems.push(format!(
                "{path:?}: cells assert reads={reads} writes={writes}, the reach row claims {claim:?}"
            ));
        }
    }
    for cell in CELLS {
        if !flagged.iter().any(|p| p.as_slice() == cell.path) {
            problems.push(format!(
                "cell {} names {:?}, which takes no --direct-io",
                cell.name, cell.path
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
