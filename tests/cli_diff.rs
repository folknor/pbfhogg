//! CLI-driven integration tests for `pbfhogg diff` (text format).
//!
//! Replaces `tests/diff.rs`. The OSC variant of diff (which is the
//! `derive_changes` operation) lives in `cli_derive_changes.rs`.
//! This file covers the default text-format output. Output goes to
//! stdout; the `--summary` flag prints `DiffStats::print_summary` to
//! stderr in pbfhogg format ("`{total} differences: {created}
//! created, {modified} modified, {deleted} deleted ({common} common)`"
//! or "`Files are identical ({common} common elements)`"). All tests
//! pass `--summary` so stats are recoverable.
//!
//! No imports from `pbfhogg::diff::*` - a rewrite of
//! `src/commands/diff/` cannot break these tests by type changes
//! alone.

#![allow(clippy::unwrap_used)]
#![allow(clippy::too_many_lines)]

mod common;

use std::path::Path;

use common::cli::CliInvoker;
use common::{
    assert_indexed, assert_non_indexed, generate_nodes, write_multi_block_test_pbf,
    write_test_pbf, write_test_pbf_non_indexed, write_test_pbf_sorted, TestMember, TestMeta,
    TestNode, TestRelation, TestWay,
};
use pbfhogg::MemberId;
use tempfile::TempDir;

#[derive(Default, Clone, Copy)]
struct DiffOpts {
    suppress_common: bool,
    verbose: bool,
    type_filter: Option<&'static str>,
    jobs: Option<usize>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
struct DiffStats {
    common: u64,
    created: u64,
    modified: u64,
    deleted: u64,
}

impl DiffStats {
    fn has_differences(&self) -> bool {
        self.created > 0 || self.modified > 0 || self.deleted > 0
    }
}

fn run_diff(old: &Path, new: &Path, opts: DiffOpts) -> (String, DiffStats) {
    // The pbfhogg binary's `main` installs a `HotpathGuardBuilder`,
    // and the guard emits its timing banner to stdout at drop time.
    // That banner pollutes the diff text we want to read. Side-step
    // it by routing diff output to a file via `-o` and reading the
    // file - stdout from the binary is then noise we ignore.
    //
    // Note: do NOT pass --summary. That flips the stderr stats line
    // from pbfhogg format to osmium format (`left=N right=N same=N
    // different=N`) and loses the per-counter created/modified/deleted
    // split. The default summary always fires unless --quiet.
    let dir_for_output = old.parent().expect("old has parent");
    // Stable name keyed by (suppress_common, verbose, type_filter,
    // jobs) so two run_diff calls in the same test don't collide.
    let suffix = format!(
        "diff-out-c{}-v{}-t{}-j{}.txt",
        u8::from(opts.suppress_common),
        u8::from(opts.verbose),
        opts.type_filter.unwrap_or("none"),
        opts.jobs.map_or("def".into(), |j| j.to_string()),
    );
    let out_path = dir_for_output.join(suffix);

    let mut cli = CliInvoker::new()
        .arg("diff")
        .arg(old)
        .arg(new)
        .arg("-o")
        .arg(&out_path);
    if opts.suppress_common {
        cli = cli.arg("-c");
    }
    if opts.verbose {
        cli = cli.arg("-v");
    }
    if let Some(t) = opts.type_filter {
        cli = cli.arg("-t").arg(t);
    }
    if let Some(j) = opts.jobs {
        cli = cli.arg("-j").arg(j.to_string());
    }
    let out = cli.run();
    // diff exits 0 when files are identical, 1 when they differ. Any
    // other exit code is an unexpected error.
    let code = out.status.code();
    assert!(
        code == Some(0) || code == Some(1),
        "pbfhogg diff failed unexpectedly (exit {code:?}); stderr:\n{}",
        out.stderr_str(),
    );
    let stats = parse_diff_stats(&out.stderr_str())
        .unwrap_or_else(|| panic!("could not parse diff stats; stderr:\n{}", out.stderr_str()));
    let text = std::fs::read_to_string(&out_path).expect("read diff output file");
    (text, stats)
}

fn run_diff_failing(old: &Path, new: &Path) -> common::cli::CliOutput {
    CliInvoker::new()
        .arg("diff")
        .arg(old)
        .arg(new)
        .arg("--osmium-summary")
        .run()
}

/// Parse the summary line emitted by `DiffStats::print_summary`:
///
///   "Files are identical ({common} common elements)"
///   "{total} differences: {created} created, {modified} modified, {deleted} deleted ({common} common)"
fn parse_diff_stats(stderr: &str) -> Option<DiffStats> {
    if let Some(line) = stderr.lines().find(|l| l.starts_with("Files are identical")) {
        let common = first_u64(line)?;
        return Some(DiffStats { common, ..DiffStats::default() });
    }
    let line = stderr.lines().find(|l| l.contains(" differences: "))?;
    let nums: Vec<u64> = line
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    if nums.len() < 5 { return None; }
    // [total, created, modified, deleted, common]
    Some(DiffStats {
        created: nums[1],
        modified: nums[2],
        deleted: nums[3],
        common: nums[4],
    })
}

fn first_u64(line: &str) -> Option<u64> {
    line.split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
}

// ---------------------------------------------------------------------------
// Basic
// ---------------------------------------------------------------------------

#[test]
fn identical_files_empty_output() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    let nodes = [
        TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
        TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
    ];
    let ways = [TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")], meta: None }];

    write_test_pbf_sorted(&old, &nodes, &ways, &[]);
    write_test_pbf_sorted(&new, &nodes, &ways, &[]);

    let (text, stats) = run_diff(&old, &new, DiffOpts { suppress_common: true, ..DiffOpts::default() });
    assert!(text.is_empty(), "suppress_common output should be empty for identical files");
    assert!(stats.common > 0);
    assert!(!stats.has_differences());
}

#[test]
fn identical_files_shows_common() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    let nodes = [
        TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
        TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
    ];
    let ways = [TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")], meta: None }];

    write_test_pbf_sorted(&old, &nodes, &ways, &[]);
    write_test_pbf_sorted(&new, &nodes, &ways, &[]);

    let (text, stats) = run_diff(&old, &new, DiffOpts::default());
    for line in text.lines() {
        assert!(line.starts_with(' '), "all lines should start with space; got: {line:?}");
    }
    assert_eq!(stats.common, 3);
}

#[test]
fn added_elements() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(&old, &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None }], &[], &[]);
    write_test_pbf_sorted(
        &new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")], meta: None }],
        &[],
    );

    let (text, stats) = run_diff(&old, &new, DiffOpts { suppress_common: true, ..DiffOpts::default() });
    assert!(text.contains("+n2"), "stdout should contain +n2; got:\n{text}");
    assert!(text.contains("+w10"), "stdout should contain +w10; got:\n{text}");
    assert_eq!(stats.created, 2);
}

#[test]
fn deleted_elements() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![], meta: None }],
        &[],
    );
    write_test_pbf_sorted(&new, &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None }], &[], &[]);

    let (text, stats) = run_diff(&old, &new, DiffOpts { suppress_common: true, ..DiffOpts::default() });
    assert!(text.contains("-n2"), "stdout should contain -n2; got:\n{text}");
    assert!(text.contains("-w10"), "stdout should contain -w10; got:\n{text}");
    assert_eq!(stats.deleted, 2);
}

#[test]
fn modified_node_coordinates() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(&old, &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None }], &[], &[]);
    write_test_pbf_sorted(&new, &[TestNode { id: 1, lat: 150_000_000, lon: 250_000_000, tags: vec![], meta: None }], &[], &[]);

    let (text, stats) = run_diff(&old, &new, DiffOpts { suppress_common: true, ..DiffOpts::default() });
    assert!(text.contains("*n1"), "stdout should contain *n1; got:\n{text}");
    assert_eq!(stats.modified, 1);
}

#[test]
fn modified_node_tags() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(&old, &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "old")], meta: None }], &[], &[]);
    write_test_pbf_sorted(&new, &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "new")], meta: None }], &[], &[]);

    let (text, stats) = run_diff(&old, &new, DiffOpts { suppress_common: true, ..DiffOpts::default() });
    assert!(text.contains("*n1"), "stdout: {text}");
    assert_eq!(stats.modified, 1);
}

#[test]
fn modified_way_refs() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(&old, &[], &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")], meta: None }], &[]);
    write_test_pbf_sorted(&new, &[], &[TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")], meta: None }], &[]);

    let (text, stats) = run_diff(&old, &new, DiffOpts { suppress_common: true, ..DiffOpts::default() });
    assert!(text.contains("*w10"), "stdout: {text}");
    assert_eq!(stats.modified, 1);
}

#[test]
fn modified_relation_members() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(&old, &[], &[], &[TestRelation {
        id: 100,
        members: vec![TestMember { id: MemberId::Node(1), role: "stop" }],
        tags: vec![("type", "route")],
        meta: None,
    }]);
    write_test_pbf_sorted(&new, &[], &[], &[TestRelation {
        id: 100,
        members: vec![
            TestMember { id: MemberId::Node(1), role: "stop" },
            TestMember { id: MemberId::Way(2), role: "outer" },
        ],
        tags: vec![("type", "route")],
        meta: None,
    }]);

    let (text, stats) = run_diff(&old, &new, DiffOpts { suppress_common: true, ..DiffOpts::default() });
    assert!(text.contains("*r100"), "stdout: {text}");
    assert_eq!(stats.modified, 1);
}

#[test]
fn suppress_common_hides_unchanged() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "old")], meta: None },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![], meta: None },
        ],
        &[TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")], meta: None }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "new")], meta: None },
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![], meta: None },
        ],
        &[TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")], meta: None }],
        &[],
    );

    let (text, _) = run_diff(&old, &new, DiffOpts { suppress_common: true, ..DiffOpts::default() });
    for line in text.lines() {
        assert!(!line.starts_with(' '), "suppress_common should hide unchanged; got: {line:?}");
    }
}

#[test]
fn verbose_shows_tag_details() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(&old, &[TestNode {
        id: 1, lat: 100_000_000, lon: 200_000_000,
        tags: vec![("name", "old"), ("amenity", "cafe")],
        meta: None,
    }], &[], &[]);
    write_test_pbf_sorted(&new, &[TestNode {
        id: 1, lat: 100_000_000, lon: 200_000_000,
        tags: vec![("name", "new"), ("highway", "primary")],
        meta: None,
    }], &[], &[]);

    let (text, stats) = run_diff(&old, &new, DiffOpts { suppress_common: true, verbose: true, ..DiffOpts::default() });
    assert_eq!(stats.modified, 1);
    assert!(text.contains("~name: old -> new"), "verbose tag change; text:\n{text}");
    assert!(text.contains("-amenity=cafe"), "verbose tag remove; text:\n{text}");
    assert!(text.contains("+highway=primary"), "verbose tag add; text:\n{text}");
}

#[test]
fn verbose_shows_coordinate_details() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(&old, &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None }], &[], &[]);
    write_test_pbf_sorted(&new, &[TestNode { id: 1, lat: 150_000_000, lon: 250_000_000, tags: vec![], meta: None }], &[], &[]);

    let (text, stats) = run_diff(&old, &new, DiffOpts { suppress_common: true, verbose: true, ..DiffOpts::default() });
    assert_eq!(stats.modified, 1);
    assert!(text.contains("coordinates:"), "verbose coords; text:\n{text}");
}

#[test]
fn type_filter_restricts_output() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(&old, &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None }],
        &[TestWay { id: 10, refs: vec![1], tags: vec![], meta: None }], &[]);
    write_test_pbf_sorted(&new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![], meta: None },
            TestWay { id: 20, refs: vec![2], tags: vec![], meta: None },
        ],
        &[],
    );

    let (text, _) = run_diff(&old, &new, DiffOpts { suppress_common: true, type_filter: Some("node"), ..DiffOpts::default() });

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }
        let second_char = trimmed.chars().nth(1);
        assert_ne!(second_char, Some('w'), "type_filter=node should exclude ways; got: {line:?}");
    }
}

#[test]
fn unsorted_input_rejected() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf(&old, &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None }], &[], &[]);
    write_test_pbf_sorted(&new, &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None }], &[], &[]);

    let out = run_diff_failing(&old, &new);
    assert!(!out.status.success(), "should reject unsorted; stderr:\n{}", out.stderr_str());
    let stderr = out.stderr_str();
    assert!(stderr.contains("not sorted"), "stderr:\n{stderr}");
    assert!(stderr.contains("Sort.Type_then_ID"), "stderr:\n{stderr}");
    assert!(stderr.contains("pbfhogg sort"), "stderr:\n{stderr}");
}

#[test]
fn empty_files_no_data_blocks() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(&old, &[], &[], &[]);
    write_test_pbf_sorted(&new, &[], &[], &[]);

    let (text, stats) = run_diff(&old, &new, DiffOpts::default());
    assert!(text.is_empty(), "empty files should produce no output; got:\n{text}");
    assert!(!stats.has_differences());
    assert_eq!(stats.common, 0);
}

#[test]
fn multi_block_boundary_asymmetric() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    #[allow(clippy::cast_possible_truncation)]
    let old_nodes: Vec<TestNode> = (1_i64..=9000)
        .map(|id| TestNode { id, lat: id as i32, lon: id as i32, tags: vec![], meta: None })
        .collect();
    #[allow(clippy::cast_possible_truncation)]
    let new_nodes: Vec<TestNode> = (1_i64..=7500)
        .map(|id| TestNode { id, lat: id as i32, lon: id as i32, tags: vec![], meta: None })
        .collect();

    write_test_pbf_sorted(&old, &old_nodes, &[], &[]);
    write_test_pbf_sorted(&new, &new_nodes, &[], &[]);

    let (text, stats) = run_diff(&old, &new, DiffOpts { suppress_common: true, ..DiffOpts::default() });
    assert_eq!(stats.deleted, 1500, "expected 1500 deleted; got {}", stats.deleted);
    assert_eq!(stats.common, 7500, "expected 7500 common; got {}", stats.common);
    assert_eq!(stats.created, 0);
    assert_eq!(stats.modified, 0);
    for line in text.lines() {
        assert!(line.starts_with('-'), "all lines should be deletions; got: {line:?}");
    }
}

#[test]
fn type_filter_way_skips_phases() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(&old,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")], meta: None }],
        &[TestRelation {
            id: 100,
            members: vec![TestMember { id: MemberId::Node(1), role: "stop" }],
            tags: vec![("type", "route")],
            meta: None,
        }],
    );
    write_test_pbf_sorted(&new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![], meta: None },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")], meta: None },
            TestWay { id: 20, refs: vec![1, 3], tags: vec![("highway", "secondary")], meta: None },
        ],
        &[TestRelation {
            id: 200,
            members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
            tags: vec![("type", "multipolygon")],
            meta: None,
        }],
    );

    let (text, stats) = run_diff(&old, &new, DiffOpts { type_filter: Some("way"), ..DiffOpts::default() });
    for line in text.lines() {
        if line.is_empty() { continue; }
        let type_char = line.chars().nth(1);
        assert_eq!(type_char, Some('w'), "type_filter=way; got: {line:?}");
    }
    assert_eq!(stats.common, 1);
    assert_eq!(stats.created, 1);
    assert_eq!(stats.modified, 0);
    assert_eq!(stats.deleted, 0);
}

#[test]
fn osmium_summary_format() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(&old,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "old")], meta: None },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![], meta: None },
        ],
        &[TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")], meta: None }],
        &[],
    );
    write_test_pbf_sorted(&new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "new")], meta: None },
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![], meta: None },
        ],
        &[TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")], meta: None }],
        &[],
    );

    let (_, stats) = run_diff(&old, &new, DiffOpts::default());
    assert_eq!(stats.common, 2);
    assert_eq!(stats.modified, 1);
    assert_eq!(stats.deleted, 1);
    assert_eq!(stats.created, 1);

    let left = stats.common + stats.modified + stats.deleted;
    let right = stats.common + stats.modified + stats.created;
    let different = stats.created + stats.modified + stats.deleted;
    assert_eq!(left, 4);
    assert_eq!(right, 4);
    assert_eq!(different, 3);
}

// ---------------------------------------------------------------------------
// Fallback path: indexed vs non-indexed inputs
// ---------------------------------------------------------------------------

fn fallback_fixture_old(path: &Path, non_indexed: bool) {
    let nodes = [
        TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
        TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "old")], meta: None },
        TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![], meta: None },
    ];
    let ways = [TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")], meta: None }];
    if non_indexed {
        write_test_pbf_non_indexed(path, &nodes, &ways, &[]);
    } else {
        write_test_pbf_sorted(path, &nodes, &ways, &[]);
    }
}

fn fallback_fixture_new(path: &Path, non_indexed: bool) {
    let nodes = [
        TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
        TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "new")], meta: None },
        TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![], meta: None },
    ];
    let ways = [TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")], meta: None }];
    if non_indexed {
        write_test_pbf_non_indexed(path, &nodes, &ways, &[]);
    } else {
        write_test_pbf_sorted(path, &nodes, &ways, &[]);
    }
}

fn assert_fallback_stats(stats: &DiffStats) {
    assert_eq!(stats.common, 2);
    assert_eq!(stats.modified, 1);
    assert_eq!(stats.deleted, 1);
    assert_eq!(stats.created, 1);
}

#[test]
fn fallback_old_non_indexed() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    fallback_fixture_old(&old, true);
    fallback_fixture_new(&new, false);
    assert_non_indexed(&old);
    assert_indexed(&new);
    let (_, stats) = run_diff(&old, &new, DiffOpts::default());
    assert_fallback_stats(&stats);
}

#[test]
fn fallback_new_non_indexed() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    fallback_fixture_old(&old, false);
    fallback_fixture_new(&new, true);
    assert_indexed(&old);
    assert_non_indexed(&new);
    let (_, stats) = run_diff(&old, &new, DiffOpts::default());
    assert_fallback_stats(&stats);
}

#[test]
fn fallback_both_non_indexed() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    fallback_fixture_old(&old, true);
    fallback_fixture_new(&new, true);
    assert_non_indexed(&old);
    assert_non_indexed(&new);
    let (_, stats) = run_diff(&old, &new, DiffOpts::default());
    assert_fallback_stats(&stats);
}

#[test]
fn fallback_parity_with_indexed_path() {
    let dir = TempDir::new().expect("tempdir");
    let oi = dir.path().join("old_indexed.osm.pbf");
    let on = dir.path().join("old_nonindexed.osm.pbf");
    let ni = dir.path().join("new_indexed.osm.pbf");
    let nn = dir.path().join("new_nonindexed.osm.pbf");

    fallback_fixture_old(&oi, false);
    fallback_fixture_old(&on, true);
    fallback_fixture_new(&ni, false);
    fallback_fixture_new(&nn, true);

    let (text_ii, stats_ii) = run_diff(&oi, &ni, DiffOpts::default());
    let (text_ni, stats_ni) = run_diff(&on, &ni, DiffOpts::default());
    let (text_in, stats_in) = run_diff(&oi, &nn, DiffOpts::default());
    let (text_nn, stats_nn) = run_diff(&on, &nn, DiffOpts::default());

    assert_eq!(text_ii, text_ni, "old non-indexed drifts");
    assert_eq!(text_ii, text_in, "new non-indexed drifts");
    assert_eq!(text_ii, text_nn, "both non-indexed drift");

    for s in [&stats_ii, &stats_ni, &stats_in, &stats_nn] {
        assert_fallback_stats(s);
    }
}

#[test]
fn fallback_verbose_parity() {
    let dir = TempDir::new().expect("tempdir");
    let oi = dir.path().join("old_indexed.osm.pbf");
    let on = dir.path().join("old_nonindexed.osm.pbf");
    let ni = dir.path().join("new_indexed.osm.pbf");
    let nn = dir.path().join("new_nonindexed.osm.pbf");

    fallback_fixture_old(&oi, false);
    fallback_fixture_old(&on, true);
    fallback_fixture_new(&ni, false);
    fallback_fixture_new(&nn, true);

    let opts = DiffOpts { verbose: true, ..DiffOpts::default() };
    let (text_ii, _) = run_diff(&oi, &ni, opts);
    let (text_nn, _) = run_diff(&on, &nn, opts);
    assert_eq!(text_ii, text_nn, "verbose output differs between indexed and fallback paths");
}

// ---------------------------------------------------------------------------
// Metadata version suffixes
// ---------------------------------------------------------------------------

fn meta(v: i32, user: &'static str) -> TestMeta {
    TestMeta { version: v, timestamp: 0, changeset: 0, uid: 0, user, visible: true }
}

#[test]
fn diff_compact_line_shows_version_when_meta_present() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    let nodes = [TestNode {
        id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![],
        meta: Some(meta(1, "")),
    }];
    write_test_pbf_sorted(&old, &nodes, &[], &[]);
    write_test_pbf_sorted(&new, &nodes, &[], &[]);

    let (text, _) = run_diff(&old, &new, DiffOpts::default());
    assert!(text.contains(" n1 v1"), "common line should include v1; text:\n{text}");
}

#[test]
fn diff_modified_line_shows_version_bump_on_payload_change() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(&old, &[TestNode {
        id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "old")],
        meta: Some(meta(1, "alice")),
    }], &[], &[]);
    write_test_pbf_sorted(&new, &[TestNode {
        id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "new")],
        meta: Some(meta(2, "bob")),
    }], &[], &[]);

    let (text, stats) = run_diff(&old, &new, DiffOpts::default());
    assert!(text.contains("*n1 v1 -> v2"), "v-bump line; text:\n{text}");
    assert_eq!(stats.modified, 1);
    assert_eq!(stats.common, 0);
}

#[test]
fn diff_pure_metadata_bump_is_common_not_modified() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(&old, &[TestNode {
        id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "same")],
        meta: Some(meta(1, "alice")),
    }], &[], &[]);
    write_test_pbf_sorted(&new, &[TestNode {
        id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "same")],
        meta: Some(meta(2, "bob")),
    }], &[], &[]);

    let (text, stats) = run_diff(&old, &new, DiffOpts::default());
    assert_eq!(stats.modified, 0, "pure metadata bumps must NOT be modified");
    assert_eq!(stats.common, 1);
    assert!(text.contains(" n1 v1"), "common line should carry old-side version; text:\n{text}");
}

#[test]
fn diff_created_deleted_lines_carry_version() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(&old, &[TestNode {
        id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![],
        meta: Some(meta(3, "alice")),
    }], &[], &[]);
    write_test_pbf_sorted(&new, &[TestNode {
        id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![],
        meta: Some(meta(5, "bob")),
    }], &[], &[]);

    let (text, stats) = run_diff(&old, &new, DiffOpts::default());
    assert!(text.contains("-n1 v3"), "deleted line includes version; text:\n{text}");
    assert!(text.contains("+n2 v5"), "created line includes version; text:\n{text}");
    assert_eq!(stats.deleted, 1);
    assert_eq!(stats.created, 1);
}

#[test]
fn diff_mixed_metadata_sides() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(&old, &[TestNode {
        id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "old")],
        meta: Some(meta(4, "alice")),
    }], &[], &[]);
    write_test_pbf_sorted(&new, &[TestNode {
        id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "new")],
        meta: None,
    }], &[], &[]);

    let (text, stats) = run_diff(&old, &new, DiffOpts::default());
    assert!(text.contains("*n1 v4"), "mixed-metadata modified line; text:\n{text}");
    assert_eq!(stats.modified, 1);
}

#[test]
fn diff_verbose_emits_payload_deltas_not_metadata_deltas() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(&old, &[TestNode {
        id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![],
        meta: Some(meta(1, "alice")),
    }], &[], &[]);
    write_test_pbf_sorted(&new, &[TestNode {
        id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "added")],
        meta: Some(meta(2, "bob")),
    }], &[], &[]);

    let (text, _) = run_diff(&old, &new, DiffOpts { verbose: true, ..DiffOpts::default() });
    assert!(text.contains("*n1 v1 -> v2"), "v-bump line; text:\n{text}");
    assert!(text.contains("+name=added"), "tag-add detail; text:\n{text}");
    for line in text.lines() {
        if line.is_empty() { continue; }
        assert!(
            !line.contains("alice") && !line.contains("bob"),
            "verbose must not leak metadata; line:\n{line}",
        );
        assert!(
            !line.starts_with("  user") && !line.starts_with("  version"),
            "verbose must not emit metadata-keyed detail; line:\n{line}",
        );
    }
}

// ---------------------------------------------------------------------------
// Multi-blob diff_block_pair branches
// ---------------------------------------------------------------------------

#[test]
fn diff_block_pair_covers_all_four_branches() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    let old_nodes = generate_nodes(30, 1);
    write_multi_block_test_pbf(&old, &old_nodes, &[], &[], 10);

    let mut new_nodes = generate_nodes(10, 1);
    let mut blob_b = generate_nodes(10, 11);
    blob_b[4].tags = vec![("touched", "yes")];
    new_nodes.extend(blob_b);
    new_nodes.extend(generate_nodes(10, 31));
    write_multi_block_test_pbf(&new, &new_nodes, &[], &[], 10);

    let (_, stats) = run_diff(&old, &new, DiffOpts::default());
    assert_eq!(stats.common, 19, "byte-equal blob A (10) + overlap-equal blob B (9)");
    assert_eq!(stats.modified, 1, "exactly one element (id 15) modified");
    assert_eq!(stats.deleted, 10, "blob-old-C BlobOldOnly");
    assert_eq!(stats.created, 10, "blob-new-C BlobNewOnly");
}

#[test]
fn diff_block_pair_parallel_matches_sequential_on_multi_blob() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    let old_nodes = generate_nodes(30, 1);
    write_multi_block_test_pbf(&old, &old_nodes, &[], &[], 10);

    let mut new_nodes = generate_nodes(10, 1);
    let mut blob_b = generate_nodes(10, 11);
    blob_b[4].tags = vec![("touched", "yes")];
    new_nodes.extend(blob_b);
    new_nodes.extend(generate_nodes(10, 31));
    write_multi_block_test_pbf(&new, &new_nodes, &[], &[], 10);

    let (text_seq, stats_seq) = run_diff(&old, &new, DiffOpts { jobs: Some(1), ..DiffOpts::default() });
    let (text_par, stats_par) = run_diff(&old, &new, DiffOpts { jobs: Some(4), ..DiffOpts::default() });

    assert_eq!(stats_seq, stats_par, "parallel stats drift");
    assert_eq!(text_seq, text_par, "parallel text diverges from sequential on multi-blob input");
}
