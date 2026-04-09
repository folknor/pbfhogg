//! diff correctness tests.

mod common;

use std::path::Path;

use common::{write_test_pbf, write_test_pbf_sorted, TestMember, TestNode, TestRelation, TestWay};
use pbfhogg::diff::{DiffOptions, diff};
use pbfhogg::MemberId;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers (diff-specific — shared helpers are in tests/common/mod.rs)
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
        TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
        TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
    ];
    let ways = [
        TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] },
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
        TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
        TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
    ];
    let ways = [
        TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] },
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
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] }],
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![] }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
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
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode { id: 1, lat: 150_000_000, lon: 250_000_000, tags: vec![] }],
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
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "old")] }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "new")] }],
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
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[],
        &[TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")] }],
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "old")] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")] }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "new")] },
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")] }],
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
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode { id: 1, lat: 150_000_000, lon: 250_000_000, tags: vec![] }],
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
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
        &[TestWay { id: 10, refs: vec![1], tags: vec![] }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![] },
            TestWay { id: 20, refs: vec![2], tags: vec![] },
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
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
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
        .map(|id| TestNode { id, lat: id as i32, lon: id as i32, tags: vec![] })
        .collect();
    // New side: 7500 nodes (different block split point), node 9000 deleted,
    // but nodes 1-7500 are identical.
    #[allow(clippy::cast_possible_truncation)]
    let new_nodes: Vec<TestNode> = (1_i64..=7500)
        .map(|id| TestNode { id, lat: id as i32, lon: id as i32, tags: vec![] })
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] }],
        &[TestRelation {
            id: 100,
            members: vec![TestMember { id: MemberId::Node(1), role: "stop" }],
            tags: vec![("type", "route")],
        }],
    );
    write_test_pbf_sorted(
        &new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] },
            TestWay { id: 20, refs: vec![1, 3], tags: vec![("highway", "secondary")] },
        ],
        &[TestRelation {
            id: 200,
            members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
            tags: vec![("type", "multipolygon")],
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
    // Line format: <prefix><type_char><id> — prefix is at index 0, type char at index 1.
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "old")] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")] }],
        &[],
    );
    write_test_pbf_sorted(
        &new,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "new")] },
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")] }],
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
