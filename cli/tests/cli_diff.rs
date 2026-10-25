//! CLI-driven integration tests for `pbfhogg diff` (text format).
//!
//! Replaces the removed library-API `diff.rs` test. The OSC variant of diff (which is the
//! `derive_changes` operation) lives in `cli_derive_changes.rs`.
//! This file covers the default text-format output. Output goes to
//! stdout; the `--summary` flag prints `DiffStats::write_summary` to
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

use std::path::Path;

use crate::common::{
    TestMember, TestMeta, TestNode, TestRelation, TestWay, assert_indexed, assert_non_indexed,
    generate_nodes, write_multi_block_test_pbf, write_test_pbf, write_test_pbf_non_indexed,
    write_test_pbf_sorted,
};
use crate::invoker::CliInvoker;
use pbfhogg::MemberId;

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
        .arg(&out_path)
        // Paired runs (indexed and fallback) share this name and read the
        // output straight back, so the second replaces the first.
        .arg("--overwrite");
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

fn run_diff_failing(old: &Path, new: &Path) -> crate::invoker::CliOutput {
    CliInvoker::new()
        .arg("diff")
        .arg(old)
        .arg(new)
        .arg("--osmium-summary")
        .run()
}

/// Parse the summary line emitted by `DiffStats::write_summary`:
///
///   "Files are identical ({common} common elements)"
///   "{total} differences: {created} created, {modified} modified, {deleted} deleted ({common} common)"
fn parse_diff_stats(stderr: &str) -> Option<DiffStats> {
    if let Some(line) = stderr
        .lines()
        .find(|l| l.starts_with("Files are identical"))
    {
        let common = first_u64(line)?;
        return Some(DiffStats {
            common,
            ..DiffStats::default()
        });
    }
    let line = stderr.lines().find(|l| l.contains(" differences: "))?;
    let nums: Vec<u64> = line
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    if nums.len() < 5 {
        return None;
    }
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
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    let nodes = [
        TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 110_000_000,
            lon: 210_000_000,
            tags: vec![],
            meta: None,
        },
    ];
    let ways = [TestWay {
        id: 10,
        refs: vec![1, 2],
        tags: vec![("highway", "primary")],
        meta: None,
    }];

    write_test_pbf_sorted(&old, &nodes, &ways, &[]);
    write_test_pbf_sorted(&new, &nodes, &ways, &[]);

    let (text, stats) = run_diff(
        &old,
        &new,
        DiffOpts {
            suppress_common: true,
            ..DiffOpts::default()
        },
    );
    assert!(
        text.is_empty(),
        "suppress_common output should be empty for identical files"
    );
    assert!(stats.common > 0);
    assert!(!stats.has_differences());
}

#[test]
fn identical_files_shows_common() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    let nodes = [
        TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 110_000_000,
            lon: 210_000_000,
            tags: vec![],
            meta: None,
        },
    ];
    let ways = [TestWay {
        id: 10,
        refs: vec![1, 2],
        tags: vec![("highway", "primary")],
        meta: None,
    }];

    write_test_pbf_sorted(&old, &nodes, &ways, &[]);
    write_test_pbf_sorted(&new, &nodes, &ways, &[]);

    let (text, stats) = run_diff(&old, &new, DiffOpts::default());
    for line in text.lines() {
        assert!(
            line.starts_with(' '),
            "all lines should start with space; got: {line:?}"
        );
    }
    assert_eq!(stats.common, 3);
}

#[test]
fn added_elements() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
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
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "primary")],
            meta: None,
        }],
        &[],
    );

    let (text, stats) = run_diff(
        &old,
        &new,
        DiffOpts {
            suppress_common: true,
            ..DiffOpts::default()
        },
    );
    assert!(
        text.contains("+n2"),
        "stdout should contain +n2; got:\n{text}"
    );
    assert!(
        text.contains("+w10"),
        "stdout should contain +w10; got:\n{text}"
    );
    assert_eq!(stats.created, 2);
}

#[test]
fn deleted_elements() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
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
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![],
            meta: None,
        }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );

    let (text, stats) = run_diff(
        &old,
        &new,
        DiffOpts {
            suppress_common: true,
            ..DiffOpts::default()
        },
    );
    assert!(
        text.contains("-n2"),
        "stdout should contain -n2; got:\n{text}"
    );
    assert!(
        text.contains("-w10"),
        "stdout should contain -w10; got:\n{text}"
    );
    assert_eq!(stats.deleted, 2);
}

#[test]
fn modified_node_coordinates() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode {
            id: 1,
            lat: 150_000_000,
            lon: 250_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );

    let (text, stats) = run_diff(
        &old,
        &new,
        DiffOpts {
            suppress_common: true,
            ..DiffOpts::default()
        },
    );
    assert!(
        text.contains("*n1"),
        "stdout should contain *n1; got:\n{text}"
    );
    assert_eq!(stats.modified, 1);
}

#[test]
fn modified_node_tags() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "old")],
            meta: None,
        }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "new")],
            meta: None,
        }],
        &[],
        &[],
    );

    let (text, stats) = run_diff(
        &old,
        &new,
        DiffOpts {
            suppress_common: true,
            ..DiffOpts::default()
        },
    );
    assert!(text.contains("*n1"), "stdout: {text}");
    assert_eq!(stats.modified, 1);
}

#[test]
fn modified_way_refs() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "primary")],
            meta: None,
        }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[],
        &[TestWay {
            id: 10,
            refs: vec![1, 2, 3],
            tags: vec![("highway", "primary")],
            meta: None,
        }],
        &[],
    );

    let (text, stats) = run_diff(
        &old,
        &new,
        DiffOpts {
            suppress_common: true,
            ..DiffOpts::default()
        },
    );
    assert!(text.contains("*w10"), "stdout: {text}");
    assert_eq!(stats.modified, 1);
}

#[test]
fn modified_relation_members() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[],
        &[],
        &[TestRelation {
            id: 100,
            members: vec![TestMember {
                id: MemberId::Node(1),
                role: "stop",
            }],
            tags: vec![("type", "route")],
            meta: None,
        }],
    );
    write_test_pbf_sorted(
        &new,
        &[],
        &[],
        &[TestRelation {
            id: 100,
            members: vec![
                TestMember {
                    id: MemberId::Node(1),
                    role: "stop",
                },
                TestMember {
                    id: MemberId::Way(2),
                    role: "outer",
                },
            ],
            tags: vec![("type", "route")],
            meta: None,
        }],
    );

    let (text, stats) = run_diff(
        &old,
        &new,
        DiffOpts {
            suppress_common: true,
            ..DiffOpts::default()
        },
    );
    assert!(text.contains("*r100"), "stdout: {text}");
    assert_eq!(stats.modified, 1);
}

#[test]
fn suppress_common_hides_unchanged() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
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
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("name", "old")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2, 3],
            tags: vec![("highway", "primary")],
            meta: None,
        }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
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
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("name", "new")],
                meta: None,
            },
            TestNode {
                id: 4,
                lat: 130_000_000,
                lon: 230_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2, 3],
            tags: vec![("highway", "primary")],
            meta: None,
        }],
        &[],
    );

    let (text, _) = run_diff(
        &old,
        &new,
        DiffOpts {
            suppress_common: true,
            ..DiffOpts::default()
        },
    );
    for line in text.lines() {
        assert!(
            !line.starts_with(' '),
            "suppress_common should hide unchanged; got: {line:?}"
        );
    }
}

#[test]
fn verbose_shows_tag_details() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "old"), ("amenity", "cafe")],
            meta: None,
        }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "new"), ("highway", "primary")],
            meta: None,
        }],
        &[],
        &[],
    );

    let (text, stats) = run_diff(
        &old,
        &new,
        DiffOpts {
            suppress_common: true,
            verbose: true,
            ..DiffOpts::default()
        },
    );
    assert_eq!(stats.modified, 1);
    assert!(
        text.contains("~name: old -> new"),
        "verbose tag change; text:\n{text}"
    );
    assert!(
        text.contains("-amenity=cafe"),
        "verbose tag remove; text:\n{text}"
    );
    assert!(
        text.contains("+highway=primary"),
        "verbose tag add; text:\n{text}"
    );
}

#[test]
fn verbose_shows_coordinate_details() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode {
            id: 1,
            lat: 150_000_000,
            lon: 250_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );

    let (text, stats) = run_diff(
        &old,
        &new,
        DiffOpts {
            suppress_common: true,
            verbose: true,
            ..DiffOpts::default()
        },
    );
    assert_eq!(stats.modified, 1);
    assert!(
        text.contains("coordinates:"),
        "verbose coords; text:\n{text}"
    );
}

#[test]
fn type_filter_restricts_output() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[TestWay {
            id: 10,
            refs: vec![1],
            tags: vec![],
            meta: None,
        }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
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
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[
            TestWay {
                id: 10,
                refs: vec![1, 2],
                tags: vec![],
                meta: None,
            },
            TestWay {
                id: 20,
                refs: vec![2],
                tags: vec![],
                meta: None,
            },
        ],
        &[],
    );

    let (text, _) = run_diff(
        &old,
        &new,
        DiffOpts {
            suppress_common: true,
            type_filter: Some("node"),
            ..DiffOpts::default()
        },
    );

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let second_char = trimmed.chars().nth(1);
        assert_ne!(
            second_char,
            Some('w'),
            "type_filter=node should exclude ways; got: {line:?}"
        );
    }
}

#[test]
fn unsorted_input_rejected() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf(
        &old,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );

    let out = run_diff_failing(&old, &new);
    assert!(
        !out.status.success(),
        "should reject unsorted; stderr:\n{}",
        out.stderr_str()
    );
    let stderr = out.stderr_str();
    assert!(stderr.contains("not sorted"), "stderr:\n{stderr}");
    assert!(stderr.contains("Sort.Type_then_ID"), "stderr:\n{stderr}");
    assert!(stderr.contains("pbfhogg sort"), "stderr:\n{stderr}");
}

#[test]
fn empty_files_no_data_blocks() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(&old, &[], &[], &[]);
    write_test_pbf_sorted(&new, &[], &[], &[]);

    let (text, stats) = run_diff(&old, &new, DiffOpts::default());
    assert!(
        text.is_empty(),
        "empty files should produce no output; got:\n{text}"
    );
    assert!(!stats.has_differences());
    assert_eq!(stats.common, 0);
}

#[test]
fn multi_block_boundary_asymmetric() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    #[allow(clippy::cast_possible_truncation)]
    let old_nodes: Vec<TestNode> = (1_i64..=9000)
        .map(|id| TestNode {
            id,
            lat: id as i32,
            lon: id as i32,
            tags: vec![],
            meta: None,
        })
        .collect();
    #[allow(clippy::cast_possible_truncation)]
    let new_nodes: Vec<TestNode> = (1_i64..=7500)
        .map(|id| TestNode {
            id,
            lat: id as i32,
            lon: id as i32,
            tags: vec![],
            meta: None,
        })
        .collect();

    write_test_pbf_sorted(&old, &old_nodes, &[], &[]);
    write_test_pbf_sorted(&new, &new_nodes, &[], &[]);

    let (text, stats) = run_diff(
        &old,
        &new,
        DiffOpts {
            suppress_common: true,
            ..DiffOpts::default()
        },
    );
    assert_eq!(
        stats.deleted, 1500,
        "expected 1500 deleted; got {}",
        stats.deleted
    );
    assert_eq!(
        stats.common, 7500,
        "expected 7500 common; got {}",
        stats.common
    );
    assert_eq!(stats.created, 0);
    assert_eq!(stats.modified, 0);
    for line in text.lines() {
        assert!(
            line.starts_with('-'),
            "all lines should be deletions; got: {line:?}"
        );
    }
}

#[test]
fn type_filter_way_skips_phases() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
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
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "primary")],
            meta: None,
        }],
        &[TestRelation {
            id: 100,
            members: vec![TestMember {
                id: MemberId::Node(1),
                role: "stop",
            }],
            tags: vec![("type", "route")],
            meta: None,
        }],
    );
    write_test_pbf_sorted(
        &new,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[
            TestWay {
                id: 10,
                refs: vec![1, 2],
                tags: vec![("highway", "primary")],
                meta: None,
            },
            TestWay {
                id: 20,
                refs: vec![1, 3],
                tags: vec![("highway", "secondary")],
                meta: None,
            },
        ],
        &[TestRelation {
            id: 200,
            members: vec![TestMember {
                id: MemberId::Way(10),
                role: "outer",
            }],
            tags: vec![("type", "multipolygon")],
            meta: None,
        }],
    );

    let (text, stats) = run_diff(
        &old,
        &new,
        DiffOpts {
            type_filter: Some("way"),
            ..DiffOpts::default()
        },
    );
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
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
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
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
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("name", "old")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2, 3],
            tags: vec![("highway", "primary")],
            meta: None,
        }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
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
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("name", "new")],
                meta: None,
            },
            TestNode {
                id: 4,
                lat: 130_000_000,
                lon: 230_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2, 3],
            tags: vec![("highway", "primary")],
            meta: None,
        }],
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
        TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 110_000_000,
            lon: 210_000_000,
            tags: vec![("name", "old")],
            meta: None,
        },
        TestNode {
            id: 3,
            lat: 120_000_000,
            lon: 220_000_000,
            tags: vec![],
            meta: None,
        },
    ];
    let ways = [TestWay {
        id: 10,
        refs: vec![1, 2, 3],
        tags: vec![("highway", "primary")],
        meta: None,
    }];
    if non_indexed {
        write_test_pbf_non_indexed(path, &nodes, &ways, &[]);
    } else {
        write_test_pbf_sorted(path, &nodes, &ways, &[]);
    }
}

fn fallback_fixture_new(path: &Path, non_indexed: bool) {
    let nodes = [
        TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        },
        TestNode {
            id: 2,
            lat: 110_000_000,
            lon: 210_000_000,
            tags: vec![("name", "new")],
            meta: None,
        },
        TestNode {
            id: 4,
            lat: 130_000_000,
            lon: 230_000_000,
            tags: vec![],
            meta: None,
        },
    ];
    let ways = [TestWay {
        id: 10,
        refs: vec![1, 2, 3],
        tags: vec![("highway", "primary")],
        meta: None,
    }];
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
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
    let dir = crate::common::test_scratch();
    let oi = dir.path().join("old_indexed.osm.pbf");
    let on = dir.path().join("old_nonindexed.osm.pbf");
    let ni = dir.path().join("new_indexed.osm.pbf");
    let nn = dir.path().join("new_nonindexed.osm.pbf");

    fallback_fixture_old(&oi, false);
    fallback_fixture_old(&on, true);
    fallback_fixture_new(&ni, false);
    fallback_fixture_new(&nn, true);

    let opts = DiffOpts {
        verbose: true,
        ..DiffOpts::default()
    };
    let (text_ii, _) = run_diff(&oi, &ni, opts);
    let (text_nn, _) = run_diff(&on, &nn, opts);
    assert_eq!(
        text_ii, text_nn,
        "verbose output differs between indexed and fallback paths"
    );
}

// ---------------------------------------------------------------------------
// Metadata version suffixes
// ---------------------------------------------------------------------------

fn meta(v: i32, user: &'static str) -> TestMeta {
    TestMeta {
        version: v,
        timestamp: 0,
        changeset: 0,
        uid: 0,
        user,
        visible: true,
    }
}

#[test]
fn diff_compact_line_shows_version_when_meta_present() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    let nodes = [TestNode {
        id: 1,
        lat: 100_000_000,
        lon: 200_000_000,
        tags: vec![],
        meta: Some(meta(1, "")),
    }];
    write_test_pbf_sorted(&old, &nodes, &[], &[]);
    write_test_pbf_sorted(&new, &nodes, &[], &[]);

    let (text, _) = run_diff(&old, &new, DiffOpts::default());
    assert!(
        text.contains(" n1 v1"),
        "common line should include v1; text:\n{text}"
    );
}

#[test]
fn diff_modified_line_shows_version_bump_on_payload_change() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "old")],
            meta: Some(meta(1, "alice")),
        }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "new")],
            meta: Some(meta(2, "bob")),
        }],
        &[],
        &[],
    );

    let (text, stats) = run_diff(&old, &new, DiffOpts::default());
    assert!(text.contains("*n1 v1 -> v2"), "v-bump line; text:\n{text}");
    assert_eq!(stats.modified, 1);
    assert_eq!(stats.common, 0);
}

#[test]
fn diff_pure_metadata_bump_is_common_not_modified() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "same")],
            meta: Some(meta(1, "alice")),
        }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "same")],
            meta: Some(meta(2, "bob")),
        }],
        &[],
        &[],
    );

    let (text, stats) = run_diff(&old, &new, DiffOpts::default());
    assert_eq!(
        stats.modified, 0,
        "pure metadata bumps must NOT be modified"
    );
    assert_eq!(stats.common, 1);
    assert!(
        text.contains(" n1 v1"),
        "common line should carry old-side version; text:\n{text}"
    );
}

#[test]
fn diff_created_deleted_lines_carry_version() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: Some(meta(3, "alice")),
        }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode {
            id: 2,
            lat: 110_000_000,
            lon: 210_000_000,
            tags: vec![],
            meta: Some(meta(5, "bob")),
        }],
        &[],
        &[],
    );

    let (text, stats) = run_diff(&old, &new, DiffOpts::default());
    assert!(
        text.contains("-n1 v3"),
        "deleted line includes version; text:\n{text}"
    );
    assert!(
        text.contains("+n2 v5"),
        "created line includes version; text:\n{text}"
    );
    assert_eq!(stats.deleted, 1);
    assert_eq!(stats.created, 1);
}

#[test]
fn diff_mixed_metadata_sides() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "old")],
            meta: Some(meta(4, "alice")),
        }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "new")],
            meta: None,
        }],
        &[],
        &[],
    );

    let (text, stats) = run_diff(&old, &new, DiffOpts::default());
    assert!(
        text.contains("*n1 v4"),
        "mixed-metadata modified line; text:\n{text}"
    );
    assert_eq!(stats.modified, 1);
}

#[test]
fn diff_verbose_emits_payload_deltas_not_metadata_deltas() {
    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: Some(meta(1, "alice")),
        }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "added")],
            meta: Some(meta(2, "bob")),
        }],
        &[],
        &[],
    );

    let (text, _) = run_diff(
        &old,
        &new,
        DiffOpts {
            verbose: true,
            ..DiffOpts::default()
        },
    );
    assert!(text.contains("*n1 v1 -> v2"), "v-bump line; text:\n{text}");
    assert!(
        text.contains("+name=added"),
        "tag-add detail; text:\n{text}"
    );
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
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
    let dir = crate::common::test_scratch();
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
    assert_eq!(
        stats.common, 19,
        "byte-equal blob A (10) + overlap-equal blob B (9)"
    );
    assert_eq!(stats.modified, 1, "exactly one element (id 15) modified");
    assert_eq!(stats.deleted, 10, "blob-old-C BlobOldOnly");
    assert_eq!(stats.created, 10, "blob-new-C BlobNewOnly");
}

/// Seq-vs-parallel parity on an honestly-sorted multi-blob pair.
///
/// **This fixture proves less than the test name suggests, by construction.**
/// `write_multi_block_test_pbf` can only emit blobs in canonical order, and
/// the two paths agree trivially on such input - which is why this test
/// passed throughout the entire period when `diff` returned exit 0 "Files
/// are identical" at the default `-j` and exit 2 "blobs are not in canonical
/// order" at `-j 1`, on the same pair of files. The parity case with real
/// power needs a fixture laid out in an order this helper cannot produce,
/// and lives in `cli/tests/cli_merge_join_trust.rs`. Keep this one for the
/// ordinary path; do not read it as covering seq-vs-parallel generally.
#[test]
fn diff_block_pair_parallel_matches_sequential_on_multi_blob() {
    let dir = crate::common::test_scratch();
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

    let (text_seq, stats_seq) = run_diff(
        &old,
        &new,
        DiffOpts {
            jobs: Some(1),
            ..DiffOpts::default()
        },
    );
    let (text_par, stats_par) = run_diff(
        &old,
        &new,
        DiffOpts {
            jobs: Some(4),
            ..DiffOpts::default()
        },
    );

    assert_eq!(stats_seq, stats_par, "parallel stats drift");
    assert_eq!(
        text_seq, text_par,
        "parallel text diverges from sequential on multi-blob input"
    );
}

/// `-v/--verbose` details come from the one diff sink every shard writes
/// through, so a sharded verbose diff prints exactly the one-shard text,
/// detail lines included, each under its modification line.
#[test]
fn diff_verbose_sharded_matches_one_shard() {
    let dir = crate::common::test_scratch();
    let (old, new) = write_shard_pair(dir.path());

    let opts = |jobs| DiffOpts {
        verbose: true,
        jobs: Some(jobs),
        ..DiffOpts::default()
    };
    let (text_one, stats_one) = run_diff(&old, &new, opts(1));
    let (text_par, stats_par) = run_diff(&old, &new, opts(8));
    assert!(
        text_one.lines().any(|line| line.starts_with("  ")),
        "the fixture must produce verbose detail lines; text:\n{text_one}"
    );
    assert_eq!(stats_one, stats_par, "verbose stats drift");
    assert_eq!(text_one, text_par, "-j 8 verbose text diverges from -j 1");
}

/// `-j 1` is the scratch-free path: it must run, to stdout, from a current
/// directory the process cannot write, and leave no run directory anywhere.
#[test]
fn diff_single_job_needs_no_writable_current_directory() {
    use std::os::unix::fs::PermissionsExt;

    let dir = crate::common::test_scratch();
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");
    write_test_pbf_sorted(&old, &generate_nodes(4, 1), &[], &[]);
    write_test_pbf_sorted(&new, &generate_nodes(4, 1), &[], &[]);
    let cwd = dir.path().join("readonly");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::set_permissions(&cwd, std::fs::Permissions::from_mode(0o555)).unwrap();

    let out = CliInvoker::new()
        .arg("diff")
        .arg(&old)
        .arg(&new)
        .arg("-j")
        .arg("1")
        .current_dir(&cwd)
        .run();
    std::fs::set_permissions(&cwd, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        out.status.success(),
        "diff -j 1 failed in a read-only current directory; stderr:\n{}",
        out.stderr_str(),
    );
    assert_eq!(
        std::fs::read_dir(&cwd).unwrap().count(),
        0,
        "diff -j 1 left something in the current directory"
    );
}

// ---------------------------------------------------------------------------
// Sequential vs parallel sharded equivalence
// ---------------------------------------------------------------------------
//
// `-j 1` runs the block-pair merge as one shard; `-j 8` runs it per
// ID-range shard, clipping blobs that straddle a shard's window and
// concatenating the shards' output. The two must produce the same bytes. The fixture is laid out blob by blob so the
// shard planner really splits every kind and the interesting cases sit on
// shard boundaries.
//
// Shard count: the planner takes `n = min(-j, old blobs of the kind)` and
// cuts after old blobs `k * len / n` for k in 1..n, at those blobs' maximum
// IDs. 16 old node blobs and 16 old way blobs cut after blobs 2, 4, ..., 14
// (1-based); 12 old relation blobs cut after blobs 2, 4, 5, 7, 8, 10, 11.
// All maxima are distinct, so `-j 8` runs eight shards per kind:
//   nodes      at 30, 50, 70, 90, 110, 130, 150
//   ways       at 10, 20, 30, 40, 50, 60, 70
//   relations  at 6, 12, 15, 21, 24, 30, 33

/// One element kind's layout: every inner vector becomes one data blob.
struct ShardFixture {
    nodes: Vec<Vec<TestNode>>,
    ways: Vec<Vec<TestWay>>,
    relations: Vec<Vec<TestRelation>>,
}

fn shard_meta(version: i32) -> TestMeta {
    TestMeta {
        version,
        ..TestMeta::default()
    }
}

/// Node `id`, version 1 or (when `modified`) a moved version 2. Every 13th
/// id carries no metadata, so deletes without a version are covered.
fn shard_node(id: i64, modified: bool) -> TestNode {
    let base = 1000 * i32::try_from(id % 10_000).unwrap();
    TestNode {
        id,
        lat: base + i32::from(modified),
        lon: base,
        tags: if id % 7 == 0 {
            vec![("amenity", "bench")]
        } else {
            vec![]
        },
        meta: (id % 13 != 0).then(|| shard_meta(if modified { 2 } else { 1 })),
    }
}

/// Way `id`; every 11th carries no metadata.
fn shard_way(id: i64, modified: bool) -> TestWay {
    TestWay {
        id,
        refs: if modified {
            vec![12 + id, 11 + id]
        } else {
            vec![11 + id, 12 + id]
        },
        tags: if modified {
            vec![("highway", "primary")]
        } else if id % 2 == 0 {
            vec![("highway", "residential")]
        } else {
            vec![]
        },
        meta: (id % 11 != 0).then(|| shard_meta(if modified { 2 } else { 1 })),
    }
}

fn shard_relation(id: i64, modified: bool) -> TestRelation {
    let mut members = vec![
        TestMember {
            id: MemberId::Way(id),
            role: "outer",
        },
        TestMember {
            id: MemberId::Way(id + 1),
            role: "outer",
        },
    ];
    if modified {
        members.push(TestMember {
            id: MemberId::Node(id),
            role: "label",
        });
    }
    TestRelation {
        id,
        members,
        tags: vec![("type", "multipolygon")],
        meta: Some(shard_meta(if modified { 2 } else { 1 })),
    }
}

/// A blob of `ids`, with `modified` ids at version 2 and changed content.
fn shard_blob<T>(
    ids: impl IntoIterator<Item = i64>,
    modified: &[i64],
    make: fn(i64, bool) -> T,
) -> Vec<T> {
    ids.into_iter()
        .map(|id| make(id, modified.contains(&id)))
        .collect()
}

fn without(range: std::ops::RangeInclusive<i64>, gone: &[i64]) -> Vec<i64> {
    range.filter(|id| !gone.contains(id)).collect()
}

/// Old side: 16 node blobs of ids 11..=170 (51 and 105 absent), 16 way
/// blobs of 1..=80 (33 absent), 12 relation blobs of 1..=36.
fn shard_fixture_old() -> ShardFixture {
    let nodes = (0..16)
        .map(|b| {
            let lo = 11 + 10 * b;
            shard_blob(without(lo..=lo + 9, &[51, 105]), &[], shard_node)
        })
        .collect();
    let ways = (0..16)
        .map(|b| {
            let lo = 1 + 5 * b;
            shard_blob(without(lo..=lo + 4, &[33]), &[], shard_way)
        })
        .collect();
    let relations = (0..12)
        .map(|b| {
            let lo = 1 + 3 * b;
            shard_blob(lo..=lo + 2, &[], shard_relation)
        })
        .collect();
    ShardFixture {
        nodes,
        ways,
        relations,
    }
}

/// New side. Per kind: untouched blobs, a lone interior delete in a blob
/// whose neighbours are untouched, changes on and just above shard
/// thresholds, a whole old blob gone, creates before the old minimum, in
/// ID gaps and past the old maximum, and one re-chunked blob that spans
/// several thresholds with changed and unchanged elements on both sides.
fn shard_fixture_new() -> ShardFixture {
    let nodes = vec![
        shard_blob([5, 6].into_iter().chain(11..=20), &[], shard_node),
        shard_blob(21..=30, &[], shard_node),
        shard_blob(without(31..=40, &[35]), &[], shard_node),
        shard_blob(41..=50, &[50], shard_node),
        shard_blob(51..=60, &[], shard_node),
        shard_blob(without(61..=70, &[65, 70]), &[], shard_node),
        // Old 71..=80 is gone. 81..=130 spans thresholds 90, 110 and 130.
        shard_blob(without(81..=130, &[100]), &[90, 111], shard_node),
        shard_blob(131..=140, &[], shard_node),
        shard_blob(141..=150, &[], shard_node),
        shard_blob(151..=160, &[], shard_node),
        shard_blob(161..=170, &[165], shard_node),
        shard_blob((171..=175).chain([1000]), &[], shard_node),
    ];
    let ways = vec![
        shard_blob(1..=5, &[], shard_way),
        shard_blob(without(6..=10, &[8]), &[], shard_way),
        shard_blob(11..=15, &[11], shard_way),
        shard_blob(16..=19, &[], shard_way),
        // Old 21..=25 is gone. 26..=45 spans thresholds 30 and 40.
        shard_blob(26..=45, &[30], shard_way),
        shard_blob(46..=50, &[], shard_way),
        shard_blob(51..=55, &[], shard_way),
        shard_blob(56..=60, &[], shard_way),
        shard_blob(61..=65, &[63], shard_way),
        shard_blob(66..=70, &[], shard_way),
        shard_blob(71..=75, &[], shard_way),
        shard_blob(76..=80, &[], shard_way),
        shard_blob(81..=82, &[], shard_way),
    ];
    let relations = vec![
        shard_blob(1..=3, &[], shard_relation),
        shard_blob(4..=5, &[], shard_relation),
        shard_blob(7..=9, &[8], shard_relation),
        shard_blob(10..=12, &[], shard_relation),
        // Old 13..=15 is gone. 16..=24 spans threshold 21 and ends on 24.
        shard_blob(without(16..=24, &[18]), &[22], shard_relation),
        shard_blob(25..=27, &[], shard_relation),
        shard_blob(28..=30, &[], shard_relation),
        shard_blob(31..=33, &[], shard_relation),
        shard_blob(34..=36, &[], shard_relation),
        shard_blob(37..=37, &[], shard_relation),
    ];
    ShardFixture {
        nodes,
        ways,
        relations,
    }
}

/// One kind's changed ids, ascending.
struct ExpectedChanges {
    kind: char,
    created: Vec<i64>,
    modified: Vec<i64>,
    deleted: Vec<i64>,
}

/// The changes the two fixtures differ by, written out independently of
/// either code path.
fn shard_expected() -> [ExpectedChanges; 3] {
    let mut node_deletes = vec![35, 65, 70];
    node_deletes.extend(71..=80);
    node_deletes.push(100);
    let mut way_deletes = vec![8, 20];
    way_deletes.extend(21..=25);
    [
        ExpectedChanges {
            kind: 'n',
            created: vec![5, 6, 51, 105, 171, 172, 173, 174, 175, 1000],
            modified: vec![50, 90, 111, 165],
            deleted: node_deletes,
        },
        ExpectedChanges {
            kind: 'w',
            created: vec![33, 81, 82],
            modified: vec![11, 30, 63],
            deleted: way_deletes,
        },
        ExpectedChanges {
            kind: 'r',
            created: vec![37],
            modified: vec![8, 22],
            deleted: vec![6, 13, 14, 15, 18],
        },
    ]
}

fn write_shard_fixture(path: &Path, fixture: &ShardFixture) {
    use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData, Metadata};
    use pbfhogg::writer::{Compression, PbfWriter};

    fn meta(m: Option<&TestMeta>) -> Option<Metadata<'static>> {
        m.map(|m| Metadata {
            version: m.version,
            timestamp: m.timestamp,
            changeset: m.changeset,
            uid: m.uid,
            user: m.user,
            visible: m.visible,
        })
    }
    fn end_blob(bb: &mut BlockBuilder, writer: &mut PbfWriter<std::io::BufWriter<std::fs::File>>) {
        if let Some(bytes) = bb.take().expect("take") {
            writer.write_primitive_block(bytes).expect("write block");
        }
    }

    let file = std::fs::File::create(path).expect("create fixture");
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    let header = HeaderBuilder::new().sorted().build().expect("header");
    writer.write_header(&header).expect("write header");
    let mut bb = BlockBuilder::new();
    for blob in &fixture.nodes {
        for n in blob {
            let m = meta(n.meta.as_ref());
            bb.add_node(n.id, n.lat, n.lon, n.tags.iter().copied(), m.as_ref());
        }
        end_blob(&mut bb, &mut writer);
    }
    for blob in &fixture.ways {
        for w in blob {
            let m = meta(w.meta.as_ref());
            bb.add_way(w.id, w.tags.iter().copied(), &w.refs, m.as_ref());
        }
        end_blob(&mut bb, &mut writer);
    }
    for blob in &fixture.relations {
        for r in blob {
            let members: Vec<MemberData<'_>> = r
                .members
                .iter()
                .map(|m| MemberData {
                    id: m.id,
                    role: m.role,
                })
                .collect();
            let m = meta(r.meta.as_ref());
            bb.add_relation(r.id, r.tags.iter().copied(), &members, m.as_ref());
        }
        end_blob(&mut bb, &mut writer);
    }
    writer.flush().expect("flush");
}

/// Data blobs per kind as (nodes, ways, relations), read back from the
/// file rather than trusted from the layout.
fn data_blobs_per_kind(path: &Path) -> (usize, usize, usize) {
    let mut counts = (0, 0, 0);
    for blob in pbfhogg::BlobReader::from_path(path).expect("open fixture") {
        let blob = blob.expect("read blob");
        if !matches!(blob.get_type(), pbfhogg::BlobType::OsmData) {
            continue;
        }
        let block = blob.to_primitiveblock().expect("decode blob");
        match block.elements().next().expect("non-empty blob") {
            pbfhogg::Element::DenseNode(_) | pbfhogg::Element::Node(_) => counts.0 += 1,
            pbfhogg::Element::Way(_) => counts.1 += 1,
            pbfhogg::Element::Relation(_) => counts.2 += 1,
            _ => panic!("unexpected element kind in a data blob"),
        }
    }
    counts
}

/// Build the fixture pair and pin that it is shaped to shard.
fn write_shard_pair(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let old = dir.join("old.osm.pbf");
    let new = dir.join("new.osm.pbf");
    write_shard_fixture(&old, &shard_fixture_old());
    write_shard_fixture(&new, &shard_fixture_new());
    assert_indexed(&old);
    assert_indexed(&new);
    assert_eq!(
        data_blobs_per_kind(&old),
        (16, 16, 12),
        "old fixture lost the blob layout the eight-shard plan relies on"
    );
    assert_eq!(data_blobs_per_kind(&new), (12, 13, 10));
    (old, new)
}

/// The ids on `prefix` lines of kind `type_char` in diff text output.
fn diff_line_ids(text: &str, prefix: char, type_char: char) -> Vec<i64> {
    text.lines()
        .filter_map(|line| {
            let mut chars = line.chars();
            if chars.next()? != prefix || chars.next()? != type_char {
                return None;
            }
            let rest = chars.as_str();
            let digits = rest.split(' ').next()?;
            digits.parse().ok()
        })
        .collect()
}

#[test]
fn diff_sequential_and_sharded_output_byte_identical() {
    let dir = crate::common::test_scratch();
    let (old, new) = write_shard_pair(dir.path());

    for suppress_common in [false, true] {
        let opts = |jobs| DiffOpts {
            suppress_common,
            jobs: Some(jobs),
            ..DiffOpts::default()
        };
        let (text_seq, stats_seq) = run_diff(&old, &new, opts(1));
        let (text_par, stats_par) = run_diff(&old, &new, opts(8));

        let mut expected = DiffStats::default();
        for ExpectedChanges {
            kind,
            created,
            modified,
            deleted,
        } in shard_expected()
        {
            assert_eq!(
                diff_line_ids(&text_seq, '+', kind),
                created,
                "{kind} creates"
            );
            assert_eq!(
                diff_line_ids(&text_seq, '*', kind),
                modified,
                "{kind} modifies"
            );
            assert_eq!(
                diff_line_ids(&text_seq, '-', kind),
                deleted,
                "{kind} deletes"
            );
            expected.created += created.len() as u64;
            expected.modified += modified.len() as u64;
            expected.deleted += deleted.len() as u64;
        }
        // 158 old nodes, 79 old ways, 36 old relations; everything neither
        // modified nor deleted is common.
        expected.common = 158 + 79 + 36 - expected.modified - expected.deleted;
        assert_eq!(
            stats_seq, expected,
            "-c {suppress_common}: sequential stats"
        );
        assert_eq!(
            diff_line_ids(&text_seq, ' ', 'n').len() as u64
                + diff_line_ids(&text_seq, ' ', 'w').len() as u64
                + diff_line_ids(&text_seq, ' ', 'r').len() as u64,
            if suppress_common { 0 } else { expected.common },
            "-c {suppress_common}: common lines"
        );

        assert_eq!(stats_seq, stats_par, "-c {suppress_common}: stats drift");
        assert_eq!(
            text_seq, text_par,
            "-c {suppress_common}: -j 8 text diverges from -j 1"
        );
    }
}

#[test]
fn derive_sequential_and_sharded_output_byte_identical() {
    use std::io::Read as _;

    let dir = crate::common::test_scratch();
    let (old, new) = write_shard_pair(dir.path());

    let derive = |jobs: usize, increment_version: bool| {
        let osc = dir
            .path()
            .join(format!("j{jobs}-i{}.osc.gz", u8::from(increment_version)));
        let mut cli = CliInvoker::new()
            .arg("diff")
            .arg("--format")
            .arg("osc")
            .arg("-o")
            .arg(&osc)
            .arg("-j")
            .arg(jobs.to_string());
        if increment_version {
            cli = cli.arg("--increment-version");
        }
        let out = cli.arg(&old).arg(&new).run();
        // The pair differs, so the OSC diff answers 1, as a text diff does.
        assert_eq!(
            out.status.code(),
            Some(1),
            "diff --format osc -j {jobs}; stderr:\n{}",
            out.stderr_str()
        );
        let stderr = out.stderr_str();
        assert!(
            !stderr.contains("Using sequential comparison"),
            "-j {jobs} fell back to the sequential path; stderr:\n{stderr}"
        );
        assert!(
            stderr.contains("49 changes: 14 creates, 9 modifies, 26 deletes"),
            "-j {jobs} change counts; stderr:\n{stderr}"
        );
        let bytes = std::fs::read(&osc).expect("read osc");
        let mut xml = String::new();
        flate2::read::MultiGzDecoder::new(bytes.as_slice())
            .read_to_string(&mut xml)
            .expect("decompress osc");
        (bytes, xml)
    };

    for increment_version in [false, true] {
        let (gz_seq, xml_seq) = derive(1, increment_version);
        let (gz_par, xml_par) = derive(8, increment_version);
        assert_eq!(
            xml_seq, xml_par,
            "--increment-version {increment_version}: -j 8 XML diverges from -j 1"
        );
        assert!(
            gz_seq == gz_par,
            "--increment-version {increment_version}: identical XML, different gzip bytes"
        );

        // Deletes carry the old version (bumped under --increment-version)
        // when the source had one, and no version attribute when it did not.
        // Node 65 has no metadata in a dense block whose other nodes do, so
        // the block builder stores a padding slot (version 0) for it; that is
        // not a version claim and must not print as version 0, or as 1 once
        // bumped.
        let v = if increment_version { 2 } else { 1 };
        for want in [
            format!("<node id=\"35\" version=\"{v}\"/>"),
            "<node id=\"65\"/>".to_owned(),
            format!("<node id=\"70\" version=\"{v}\"/>"),
            "<way id=\"22\"/>".to_owned(),
            format!("<relation id=\"6\" version=\"{v}\"/>"),
        ] {
            assert!(xml_seq.contains(&want), "missing {want} in:\n{xml_seq}");
        }
    }
}
