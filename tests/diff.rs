//! diff correctness tests.

mod common;

use std::path::Path;

use common::{
    assert_indexed, assert_non_indexed, generate_nodes, write_multi_block_test_pbf,
    write_test_pbf, write_test_pbf_non_indexed, write_test_pbf_sorted, TestMember, TestMeta,
    TestNode, TestRelation, TestWay,
};
use pbfhogg::diff::{DiffOptions, diff};
use pbfhogg::MemberId;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers (diff-specific - shared helpers are in tests/common/mod.rs)
// ---------------------------------------------------------------------------

fn run_diff(old: &Path, new: &Path, options: &DiffOptions) -> (String, pbfhogg::diff::DiffStats) {
    let mut output = Vec::new();
    let stats = diff(old, new, &mut output, options, false).expect("diff");
    let text = String::from_utf8(output).expect("utf8");
    (text, stats)
}

fn default_options() -> DiffOptions {
    DiffOptions {
        suppress_common: false,
        verbose: false,
        summary: false,
        type_filter: None,
        jobs: 1,
    }
}

// ---------------------------------------------------------------------------
// Tests
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
    let ways = [
        TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")], meta: None },
    ];

    write_test_pbf_sorted(&old, &nodes, &ways, &[]);
    write_test_pbf_sorted(&new, &nodes, &ways, &[]);

    let opts = DiffOptions {
        suppress_common: true,
        ..default_options()
    };
    let (text, stats) = run_diff(&old, &new, &opts);

    assert!(text.is_empty(), "suppress_common output should be empty for identical files");
    assert!(stats.common > 0, "common count should be positive");
    assert!(!stats.has_differences(), "identical files should have no differences");
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
    let ways = [
        TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")], meta: None },
    ];

    write_test_pbf_sorted(&old, &nodes, &ways, &[]);
    write_test_pbf_sorted(&new, &nodes, &ways, &[]);

    let opts = DiffOptions {
        suppress_common: false,
        ..default_options()
    };
    let (text, stats) = run_diff(&old, &new, &opts);

    for line in text.lines() {
        assert!(
            line.starts_with(' '),
            "all lines should start with space for identical files, got: {line:?}",
        );
    }
    assert_eq!(stats.common, 3, "expected 3 common elements (2 nodes + 1 way)");
}

#[test]
fn added_elements() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")], meta: None }],
        &[],
    );

    let opts = DiffOptions {
        suppress_common: true,
        ..default_options()
    };
    let (text, stats) = run_diff(&old, &new, &opts);

    assert!(text.contains("+n2"), "output should contain +n2, got:\n{text}");
    assert!(text.contains("+w10"), "output should contain +w10, got:\n{text}");
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
    write_test_pbf_sorted(
        &new,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None }],
        &[],
        &[],
    );

    let opts = DiffOptions {
        suppress_common: true,
        ..default_options()
    };
    let (text, stats) = run_diff(&old, &new, &opts);

    assert!(text.contains("-n2"), "output should contain -n2, got:\n{text}");
    assert!(text.contains("-w10"), "output should contain -w10, got:\n{text}");
    assert_eq!(stats.deleted, 2);
}

#[test]
fn modified_node_coordinates() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode { id: 1, lat: 150_000_000, lon: 250_000_000, tags: vec![], meta: None }],
        &[],
        &[],
    );

    let opts = DiffOptions {
        suppress_common: true,
        ..default_options()
    };
    let (text, stats) = run_diff(&old, &new, &opts);

    assert!(text.contains("*n1"), "output should contain *n1, got:\n{text}");
    assert_eq!(stats.modified, 1);
}

#[test]
fn modified_node_tags() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "old")], meta: None }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "new")], meta: None }],
        &[],
        &[],
    );

    let opts = DiffOptions {
        suppress_common: true,
        ..default_options()
    };
    let (text, stats) = run_diff(&old, &new, &opts);

    assert!(text.contains("*n1"), "output should contain *n1, got:\n{text}");
    assert_eq!(stats.modified, 1);
}

#[test]
fn modified_way_refs() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")], meta: None }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[],
        &[TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")], meta: None }],
        &[],
    );

    let opts = DiffOptions {
        suppress_common: true,
        ..default_options()
    };
    let (text, stats) = run_diff(&old, &new, &opts);

    assert!(text.contains("*w10"), "output should contain *w10, got:\n{text}");
    assert_eq!(stats.modified, 1);
}

#[test]
fn modified_relation_members() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[],
        &[],
        &[TestRelation {
            id: 100,
            members: vec![TestMember { id: MemberId::Node(1), role: "stop" }],
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
                TestMember { id: MemberId::Node(1), role: "stop" },
                TestMember { id: MemberId::Way(2), role: "outer" },
            ],
            tags: vec![("type", "route")],
            meta: None,
        }],
    );

    let opts = DiffOptions {
        suppress_common: true,
        ..default_options()
    };
    let (text, stats) = run_diff(&old, &new, &opts);

    assert!(text.contains("*r100"), "output should contain *r100, got:\n{text}");
    assert_eq!(stats.modified, 1);
}

#[test]
fn suppress_common_hides_unchanged() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    // Node 1: unchanged, Node 2: modified, Node 3: deleted, Node 4: created (new only)
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

    let opts = DiffOptions {
        suppress_common: true,
        ..default_options()
    };
    let (text, _stats) = run_diff(&old, &new, &opts);

    for line in text.lines() {
        assert!(
            !line.starts_with(' '),
            "suppress_common should hide unchanged elements, found: {line:?}",
        );
    }
}

#[test]
fn verbose_shows_tag_details() {
    let dir = TempDir::new().expect("tempdir");
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

    let opts = DiffOptions {
        suppress_common: true,
        verbose: true,
        type_filter: None,
        ..default_options()
    };
    let (text, stats) = run_diff(&old, &new, &opts);

    assert_eq!(stats.modified, 1);
    assert!(
        text.contains("~name: old -> new"),
        "verbose output should contain tag change, got:\n{text}",
    );
    assert!(
        text.contains("-amenity=cafe"),
        "verbose output should contain removed tag, got:\n{text}",
    );
    assert!(
        text.contains("+highway=primary"),
        "verbose output should contain added tag, got:\n{text}",
    );
}

#[test]
fn verbose_shows_coordinate_details() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode { id: 1, lat: 150_000_000, lon: 250_000_000, tags: vec![], meta: None }],
        &[],
        &[],
    );

    let opts = DiffOptions {
        suppress_common: true,
        verbose: true,
        type_filter: None,
        ..default_options()
    };
    let (text, stats) = run_diff(&old, &new, &opts);

    assert_eq!(stats.modified, 1);
    assert!(
        text.contains("coordinates:"),
        "verbose output should contain coordinates line, got:\n{text}",
    );
}

#[test]
fn type_filter_restricts_output() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None }],
        &[TestWay { id: 10, refs: vec![1], tags: vec![], meta: None }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
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

    let opts = DiffOptions {
        suppress_common: true,
        verbose: false,
        type_filter: Some("node".to_string()),
        ..default_options()
    };
    let (text, _stats) = run_diff(&old, &new, &opts);

    for line in text.lines() {
        // After the prefix character (+, -, *, space), the type char is the next character.
        // No line should reference a way type.
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let second_char = trimmed.chars().nth(1);
        assert_ne!(
            second_char,
            Some('w'),
            "type_filter=node should exclude way lines, found: {line:?}",
        );
    }
}

#[test]
fn unsorted_input_rejected() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    // Write without sorted header.
    write_test_pbf(
        &old,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None }],
        &[],
        &[],
    );

    let mut output = Vec::new();
    let err = diff(&old, &new, &mut output, &default_options(), false)
        .expect_err("diff should reject unsorted input");
    let msg = err.to_string();
    assert!(msg.contains("not sorted"), "error should mention 'not sorted', got: {msg}");
    assert!(
        msg.contains("Sort.Type_then_ID"),
        "error should mention Sort.Type_then_ID, got: {msg}",
    );
    assert!(
        msg.contains("pbfhogg sort"),
        "error should mention 'pbfhogg sort', got: {msg}",
    );
}

#[test]
fn empty_files_no_data_blocks() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    // Header-only sorted PBFs (no data blocks).
    write_test_pbf_sorted(&old, &[], &[], &[]);
    write_test_pbf_sorted(&new, &[], &[], &[]);

    let (text, stats) = run_diff(&old, &new, &default_options());
    assert!(text.is_empty(), "empty files should produce no output, got:\n{text}");
    assert!(!stats.has_differences(), "empty files should have no differences");
    assert_eq!(stats.common, 0);
}

#[test]
fn multi_block_boundary_asymmetric() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    // Create >8000 nodes on old side (forces 2 blocks) and a different count on new side.
    #[allow(clippy::cast_possible_truncation)]
    let old_nodes: Vec<TestNode> = (1_i64..=9000)
        .map(|id| TestNode { id, lat: id as i32, lon: id as i32, tags: vec![], meta: None })
        .collect();
    // New side: 7500 nodes (different block split point), node 9000 deleted,
    // but nodes 1-7500 are identical.
    #[allow(clippy::cast_possible_truncation)]
    let new_nodes: Vec<TestNode> = (1_i64..=7500)
        .map(|id| TestNode { id, lat: id as i32, lon: id as i32, tags: vec![], meta: None })
        .collect();

    write_test_pbf_sorted(&old, &old_nodes, &[], &[]);
    write_test_pbf_sorted(&new, &new_nodes, &[], &[]);

    let opts = DiffOptions {
        suppress_common: true,
        ..default_options()
    };
    let (text, stats) = run_diff(&old, &new, &opts);

    // Nodes 7501-9000 should be deleted.
    assert_eq!(stats.deleted, 1500, "expected 1500 deleted nodes, got {}", stats.deleted);
    assert_eq!(stats.common, 7500, "expected 7500 common nodes, got {}", stats.common);
    assert_eq!(stats.created, 0);
    assert_eq!(stats.modified, 0);

    // All output lines should be deletions.
    for line in text.lines() {
        assert!(
            line.starts_with('-'),
            "all lines should be deletions with suppress_common, got: {line:?}",
        );
    }
}

#[test]
fn type_filter_way_skips_phases() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    write_test_pbf_sorted(
        &old,
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
    write_test_pbf_sorted(
        &new,
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

    let opts = DiffOptions {
        suppress_common: false,
        verbose: false,
        type_filter: Some("way".to_string()),
        ..default_options()
    };
    let (text, stats) = run_diff(&old, &new, &opts);

    // Only way elements should appear in output.
    // Line format: <prefix><type_char><id> - prefix is at index 0, type char at index 1.
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let type_char = line.chars().nth(1);
        assert_eq!(
            type_char,
            Some('w'),
            "type_filter=way should only show ways, found: {line:?}",
        );
    }

    // Stats should reflect only ways: w10 common, w20 created.
    assert_eq!(stats.common, 1, "expected 1 common way");
    assert_eq!(stats.created, 1, "expected 1 created way");
    assert_eq!(stats.modified, 0);
    assert_eq!(stats.deleted, 0);
}

#[test]
fn osmium_summary_format() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    // Old: nodes 1,2,3 + way 10 = 4 elements
    // New: nodes 1,2 (modified),4 + way 10 = 4 elements
    // Common: n1, w10 = 2; Modified: n2 = 1; Deleted: n3 = 1; Created: n4 = 1
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

    let (_, stats) = run_diff(&old, &new, &default_options());

    // Verify the computed osmium-style counts:
    // left = common + modified + deleted = 2 + 1 + 1 = 4
    // right = common + modified + created = 2 + 1 + 1 = 4
    // same = common = 2
    // different = created + modified + deleted = 1 + 1 + 1 = 3
    assert_eq!(stats.common, 2);
    assert_eq!(stats.modified, 1);
    assert_eq!(stats.deleted, 1);
    assert_eq!(stats.created, 1);

    let left = stats.common + stats.modified + stats.deleted;
    let right = stats.common + stats.modified + stats.created;
    let different = stats.created + stats.modified + stats.deleted;
    assert_eq!(left, 4, "left should be total old elements");
    assert_eq!(right, 4, "right should be total new elements");
    assert_eq!(different, 3, "different should be total changes");
}

// ---------------------------------------------------------------------------
// diff_element_stream fallback path
// ---------------------------------------------------------------------------
//
// `diff_block_pair` is the optimized block-pair merge; it requires both
// inputs to carry the `BlobHeader.indexdata` field. When either side
// lacks indexdata, `diff` falls back to `diff_element_stream`, which
// performs an element-level merge-join with owned elements. Every other
// test in this file implicitly exercises the optimized path because
// `write_test_pbf_sorted` always emits indexdata. The tests here pair
// `write_test_pbf_non_indexed` with `write_test_pbf_sorted` to force the
// fallback for each of the three (old, new) combinations and assert the
// result is identical to the optimized path on the same logical inputs.

/// Old PBF for the fallback-path fixture: nodes 1,2,3 + way 10.
fn fallback_fixture_old(path: &Path, non_indexed: bool) {
    let nodes = [
        TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
        TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "old")], meta: None },
        TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![], meta: None },
    ];
    let ways = [
        TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")], meta: None },
    ];
    if non_indexed {
        write_test_pbf_non_indexed(path, &nodes, &ways, &[]);
    } else {
        write_test_pbf_sorted(path, &nodes, &ways, &[]);
    }
}

/// New PBF for the fallback-path fixture: node 1 unchanged, node 2
/// modified (tag value), node 3 deleted, node 4 created, way 10 unchanged.
fn fallback_fixture_new(path: &Path, non_indexed: bool) {
    let nodes = [
        TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
        TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "new")], meta: None },
        TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![], meta: None },
    ];
    let ways = [
        TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")], meta: None },
    ];
    if non_indexed {
        write_test_pbf_non_indexed(path, &nodes, &ways, &[]);
    } else {
        write_test_pbf_sorted(path, &nodes, &ways, &[]);
    }
}

/// Expected diff stats for `fallback_fixture_old` vs `fallback_fixture_new`:
/// n1 common, n2 modified, n3 deleted, n4 created, w10 common.
fn assert_fallback_stats(stats: &pbfhogg::diff::DiffStats) {
    assert_eq!(stats.common, 2, "expected 2 common (n1, w10), got {}", stats.common);
    assert_eq!(stats.modified, 1, "expected 1 modified (n2), got {}", stats.modified);
    assert_eq!(stats.deleted, 1, "expected 1 deleted (n3), got {}", stats.deleted);
    assert_eq!(stats.created, 1, "expected 1 created (n4), got {}", stats.created);
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

    let (_text, stats) = run_diff(&old, &new, &default_options());
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

    let (_text, stats) = run_diff(&old, &new, &default_options());
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

    let (_text, stats) = run_diff(&old, &new, &default_options());
    assert_fallback_stats(&stats);
}

/// All four (old, new) x (indexed, non-indexed) pairings must produce the
/// same text output and stats on identical logical inputs. A divergence
/// here means diff_block_pair and diff_element_stream have drifted apart
/// and is the signal that motivated adding the fallback-path coverage in
/// the first place.
#[test]
fn fallback_parity_with_indexed_path() {
    let dir = TempDir::new().expect("tempdir");
    let oi = dir.path().join("old_indexed.osm.pbf");
    let on_ = dir.path().join("old_nonindexed.osm.pbf");
    let ni = dir.path().join("new_indexed.osm.pbf");
    let nn = dir.path().join("new_nonindexed.osm.pbf");

    fallback_fixture_old(&oi, false);
    fallback_fixture_old(&on_, true);
    fallback_fixture_new(&ni, false);
    fallback_fixture_new(&nn, true);

    let (text_ii, stats_ii) = run_diff(&oi, &ni, &default_options());
    let (text_ni, stats_ni) = run_diff(&on_, &ni, &default_options());
    let (text_in, stats_in) = run_diff(&oi, &nn, &default_options());
    let (text_nn, stats_nn) = run_diff(&on_, &nn, &default_options());

    assert_eq!(text_ii, text_ni, "old non-indexed drifts from indexed baseline");
    assert_eq!(text_ii, text_in, "new non-indexed drifts from indexed baseline");
    assert_eq!(text_ii, text_nn, "both non-indexed drift from indexed baseline");

    for (label, s) in [
        ("indexed+indexed", &stats_ii),
        ("non-indexed+indexed", &stats_ni),
        ("indexed+non-indexed", &stats_in),
        ("non-indexed+non-indexed", &stats_nn),
    ] {
        assert_fallback_stats(s);
        let _ = label;
    }
}

/// Verbose mode also needs to work on the fallback path. The optimized
/// path uses `write_modified_details_borrowed`; the fallback uses
/// `write_node_details` (owned). They must produce the same verbose
/// output for the same element change.
#[test]
fn fallback_verbose_parity() {
    let dir = TempDir::new().expect("tempdir");
    let oi = dir.path().join("old_indexed.osm.pbf");
    let on_ = dir.path().join("old_nonindexed.osm.pbf");
    let ni = dir.path().join("new_indexed.osm.pbf");
    let nn = dir.path().join("new_nonindexed.osm.pbf");

    fallback_fixture_old(&oi, false);
    fallback_fixture_old(&on_, true);
    fallback_fixture_new(&ni, false);
    fallback_fixture_new(&nn, true);

    let opts = DiffOptions {
        verbose: true,
        ..default_options()
    };

    let (text_ii, _) = run_diff(&oi, &ni, &opts);
    let (text_nn, _) = run_diff(&on_, &nn, &opts);

    assert_eq!(
        text_ii, text_nn,
        "verbose output differs between indexed and fallback paths"
    );
}

// ---------------------------------------------------------------------------
// Metadata and version suffixes in diff output
// ---------------------------------------------------------------------------
//
// `write_compact_line` and `write_modified_line` in
// `src/commands/diff/mod.rs` append a ` v<N>` suffix whenever the element
// has metadata, and the modified-line format becomes `v<old> -> v<new>`
// when both sides carry metadata and the versions differ. Every other
// test in this file uses fixtures with `meta: None`, so the version-suffix
// paths (`Some(v)` arms in the `match` statements) are not exercised.
// These tests fix that.
//
// Note: verbose detail writers (`write_{node,way,relation}_details`) only
// emit coords / refs / members / tags diffs - they do NOT emit metadata
// deltas in the verbose output. That is a design decision, not a gap:
// metadata lives on the primary diff line via the `v<N>` suffix.

fn meta(v: i32, user: &'static str) -> TestMeta {
    TestMeta { version: v, timestamp: 0, changeset: 0, uid: 0, user, visible: true }
}

#[test]
fn diff_compact_line_shows_version_when_meta_present() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    // Same node, same version - should appear as common with a v1 suffix.
    let nodes = [TestNode {
        id: 1,
        lat: 100_000_000,
        lon: 200_000_000,
        tags: vec![],
        meta: Some(meta(1, "")),
    }];
    write_test_pbf_sorted(&old, &nodes, &[], &[]);
    write_test_pbf_sorted(&new, &nodes, &[], &[]);

    let (text, _) = run_diff(&old, &new, &default_options());
    assert!(
        text.contains(" n1 v1"),
        "common line should include version suffix, got:\n{text}"
    );
}

#[test]
fn diff_modified_line_shows_version_bump_on_payload_change() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    // Id, coords, AND tag change so the merge-join sees real payload
    // modification; versions differ so the modified-line formatter has
    // to choose the `Some -> Some, ov != nv` arm of `write_modified_line`.
    let old_nodes = [TestNode {
        id: 1,
        lat: 100_000_000,
        lon: 200_000_000,
        tags: vec![("name", "old")],
        meta: Some(meta(1, "alice")),
    }];
    let new_nodes = [TestNode {
        id: 1,
        lat: 100_000_000,
        lon: 200_000_000,
        tags: vec![("name", "new")],
        meta: Some(meta(2, "bob")),
    }];
    write_test_pbf_sorted(&old, &old_nodes, &[], &[]);
    write_test_pbf_sorted(&new, &new_nodes, &[], &[]);

    let (text, stats) = run_diff(&old, &new, &default_options());
    assert!(
        text.contains("*n1 v1 -> v2"),
        "modified line should show v<old> -> v<new>, got:\n{text}"
    );
    assert_eq!(stats.modified, 1);
    assert_eq!(stats.common, 0);
}

/// Pin the merge-join's design contract: a pure metadata change (same
/// coords, same tags, only version/user/timestamp advance) is
/// `ElementEqual`, not modified. `borrowed_nodes_equal` in
/// `src/osc/merge_join.rs` compares coords+tags deliberately, not
/// metadata - this matches osmium's content-equality semantics. The
/// common line carries the old-side version suffix.
#[test]
fn diff_pure_metadata_bump_is_common_not_modified() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    let old_nodes = [TestNode {
        id: 1,
        lat: 100_000_000,
        lon: 200_000_000,
        tags: vec![("name", "same")],
        meta: Some(meta(1, "alice")),
    }];
    let new_nodes = [TestNode {
        id: 1,
        lat: 100_000_000,
        lon: 200_000_000,
        tags: vec![("name", "same")],
        meta: Some(meta(2, "bob")),
    }];
    write_test_pbf_sorted(&old, &old_nodes, &[], &[]);
    write_test_pbf_sorted(&new, &new_nodes, &[], &[]);

    let (text, stats) = run_diff(&old, &new, &default_options());
    assert_eq!(stats.modified, 0, "pure metadata bumps must NOT be modified");
    assert_eq!(stats.common, 1, "pure metadata bumps must be common");
    assert!(
        text.contains(" n1 v1"),
        "common line should carry old-side version suffix, got:\n{text}"
    );
}

#[test]
fn diff_created_deleted_lines_carry_version() {
    let dir = TempDir::new().expect("tempdir");
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

    let (text, stats) = run_diff(&old, &new, &default_options());
    assert!(text.contains("-n1 v3"), "deleted line should include version, got:\n{text}");
    assert!(text.contains("+n2 v5"), "created line should include version, got:\n{text}");
    assert_eq!(stats.deleted, 1);
    assert_eq!(stats.created, 1);
}

/// When one side carries metadata and the other does not, the compact
/// line format for the present side must still include the version
/// suffix. This pins the mixed-metadata fallback in the match arms of
/// `write_modified_line`.
#[test]
fn diff_mixed_metadata_sides() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    // Old has metadata, new does not. Tags differ so diff sees the
    // elements as modified.
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

    let (text, stats) = run_diff(&old, &new, &default_options());
    // write_modified_line's (Some, None) -> (Some, None) arm falls
    // through to the second arm: `(_, Some(v))` matches nothing when
    // new is None, so we land on `(Some(v), None) => "*n1 v<old>"`.
    assert!(
        text.contains("*n1 v4"),
        "modified line must show old-side version when new lacks metadata, got:\n{text}"
    );
    assert_eq!(stats.modified, 1);
}

/// Verbose output emits coord/tag/ref/member deltas but NOT metadata
/// deltas. This test pins that contract on a case with a real payload
/// change (tag added) + a version bump: we should see exactly one
/// primary line plus one tag-add detail line - no `user: alice -> bob`
/// or similar metadata-derived detail line.
#[test]
fn diff_verbose_emits_payload_deltas_not_metadata_deltas() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    let old_nodes = [TestNode {
        id: 1,
        lat: 100_000_000,
        lon: 200_000_000,
        tags: vec![],
        meta: Some(meta(1, "alice")),
    }];
    let new_nodes = [TestNode {
        id: 1,
        lat: 100_000_000,
        lon: 200_000_000,
        tags: vec![("name", "added")],
        meta: Some(meta(2, "bob")),
    }];
    write_test_pbf_sorted(&old, &old_nodes, &[], &[]);
    write_test_pbf_sorted(&new, &new_nodes, &[], &[]);

    let opts = DiffOptions {
        verbose: true,
        ..default_options()
    };
    let (text, _) = run_diff(&old, &new, &opts);

    assert!(text.contains("*n1 v1 -> v2"), "v-bump line must fire, got:\n{text}");
    assert!(
        text.contains("+name=added"),
        "tag-add detail line must fire, got:\n{text}"
    );
    // No metadata detail lines like "user: alice -> bob" or similar.
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        assert!(
            !line.contains("alice") && !line.contains("bob"),
            "verbose must not leak metadata into detail lines: {line:?}"
        );
        assert!(
            !line.starts_with("  user") && !line.starts_with("  version"),
            "verbose must not emit metadata-keyed detail lines: {line:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Multi-blob diff_block_pair branches
// ---------------------------------------------------------------------------
//
// `block_pair_merge_phase` in `src/osc/merge_join.rs` has four shapes
// that only fire with multiple blobs per type on at least one side:
//
//   - `BlobEqual`: both sides have a blob at the same ID range with
//     byte-identical compressed payloads. All elements accounted as
//     common without decoding.
//   - `BlobOldOnly`: old has a blob whose ID range precedes every
//     remaining new blob. Elements accounted as deleted.
//   - `BlobNewOnly`: mirror - new has a blob past the old cursor.
//   - Overlapping decode: both sides have blobs with intersecting ID
//     ranges; decode both and run an element-level merge.
//
// Every existing diff test in this file uses fixtures with a single
// blob per type, so only the overlapping-decode branch ever fires.
// This test uses `write_multi_block_test_pbf` to force 3 node blobs
// per side and crafts the inputs so each branch fires exactly once:
// blob [1..=10] is byte-equal, blob [11..=20] is overlap+decode
// with one modified element, blob [21..=30] is old-only on one side,
// blob [31..=40] is new-only on the other.

#[test]
fn diff_block_pair_covers_all_four_branches() {
    let dir = TempDir::new().expect("tempdir");
    let old = dir.path().join("old.osm.pbf");
    let new = dir.path().join("new.osm.pbf");

    // Old: 30 sequential nodes at block_size=10 -> 3 blobs.
    //   blob-old-A: [ 1..=10]
    //   blob-old-B: [11..=20]
    //   blob-old-C: [21..=30]  (no counterpart in new -> BlobOldOnly)
    let old_nodes = generate_nodes(30, 1);
    write_multi_block_test_pbf(&old, &old_nodes, &[], &[], 10);

    // New: 30 nodes at block_size=10. First blob byte-equal to old,
    // second blob has one tag-modified node, third blob is a
    // separate id range.
    //   blob-new-A: [ 1..=10]  (byte-equal to blob-old-A)
    //   blob-new-B: [11..=20]  (id 15 has an added tag -> overlap decode)
    //   blob-new-C: [31..=40]  (no counterpart in old -> BlobNewOnly)
    let mut new_nodes = generate_nodes(10, 1);
    let mut blob_b = generate_nodes(10, 11);
    // Mutate exactly one element so blob B cannot be byte-equal.
    blob_b[4].tags = vec![("touched", "yes")];
    new_nodes.extend(blob_b);
    new_nodes.extend(generate_nodes(10, 31));
    write_multi_block_test_pbf(&new, &new_nodes, &[], &[], 10);

    let (_text, stats) = run_diff(&old, &new, &default_options());

    // Branch-by-branch accounting:
    //   BlobEqual        blob-A: common += 10
    //   Overlap decode   blob-B: common += 9, modified += 1
    //   BlobOldOnly      blob-old-C (21..=30): deleted += 10
    //   BlobNewOnly      blob-new-C (31..=40): created += 10
    assert_eq!(stats.common, 19, "common should be 10 (byte-equal) + 9 (overlap-equal)");
    assert_eq!(stats.modified, 1, "exactly one element (id 15) was modified");
    assert_eq!(stats.deleted, 10, "blob-old-C should fire BlobOldOnly for 10 elements");
    assert_eq!(stats.created, 10, "blob-new-C should fire BlobNewOnly for 10 elements");
}

/// Same scenario, but with the parallel shard-based path. `jobs > 1`
/// switches to `diff_block_pair_parallel`, which must produce the same
/// stats. Pin parity so a sharding bug doesn't silently miscount.
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

    let sequential = DiffOptions { jobs: 1, ..default_options() };
    let parallel = DiffOptions { jobs: 4, ..default_options() };

    let (text_seq, stats_seq) = run_diff(&old, &new, &sequential);
    let (text_par, stats_par) = run_diff(&old, &new, &parallel);

    assert_eq!(stats_seq.common, stats_par.common, "parallel common count drifts");
    assert_eq!(stats_seq.modified, stats_par.modified, "parallel modified count drifts");
    assert_eq!(stats_seq.deleted, stats_par.deleted, "parallel deleted count drifts");
    assert_eq!(stats_seq.created, stats_par.created, "parallel created count drifts");
    assert_eq!(text_seq, text_par, "parallel text diverges from sequential on multi-blob input");
}
