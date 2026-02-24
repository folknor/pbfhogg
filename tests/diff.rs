//! diff correctness tests.

use std::path::Path;

use pbfhogg::block_builder::{self, BlockBuilder, MemberData};
use pbfhogg::diff::{DiffOptions, diff};
use pbfhogg::MemberId;
use pbfhogg::writer::{Compression, PbfWriter};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct TestNode {
    id: i64,
    lat: i32,
    lon: i32,
    tags: Vec<(&'static str, &'static str)>,
}

struct TestWay {
    id: i64,
    refs: Vec<i64>,
    tags: Vec<(&'static str, &'static str)>,
}

struct TestRelation {
    id: i64,
    members: Vec<TestMember>,
    tags: Vec<(&'static str, &'static str)>,
}

struct TestMember {
    id: MemberId,
    role: &'static str,
}

fn write_test_pbf(path: &Path, nodes: &[TestNode], ways: &[TestWay], relations: &[TestRelation]) {
    let mut writer = PbfWriter::to_path(path, Compression::default()).expect("create writer");
    let header = block_builder::build_header(None, None, None, None, &[]).expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    for n in nodes {
        if !bb.can_add_node()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(&bytes).expect("write block");
        }
        bb.add_node(n.id, n.lat, n.lon, &n.tags, None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(&bytes).expect("write block");
    }

    for w in ways {
        if !bb.can_add_way()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(&bytes).expect("write block");
        }
        bb.add_way(w.id, &w.tags, &w.refs, None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(&bytes).expect("write block");
    }

    for r in relations {
        if !bb.can_add_relation()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(&bytes).expect("write block");
        }
        let members: Vec<MemberData<'_>> = r
            .members
            .iter()
            .map(|m| MemberData {
                id: m.id,
                role: m.role,
            })
            .collect();
        bb.add_relation(r.id, &r.tags, &members, None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(&bytes).expect("write block");
    }

    writer.flush().expect("flush");
}

fn run_diff(old: &Path, new: &Path, options: &DiffOptions) -> (String, pbfhogg::diff::DiffStats) {
    let mut output = Vec::new();
    let stats = diff(old, new, &mut output, options).expect("diff");
    let text = String::from_utf8(output).expect("utf8");
    (text, stats)
}

fn default_options() -> DiffOptions {
    DiffOptions {
        suppress_common: false,
        verbose: false,
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

    write_test_pbf(&old, &nodes, &ways, &[]);
    write_test_pbf(&new, &nodes, &ways, &[]);

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

    write_test_pbf(&old, &nodes, &ways, &[]);
    write_test_pbf(&new, &nodes, &ways, &[]);

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

    write_test_pbf(
        &old,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
        &[],
        &[],
    );
    write_test_pbf(
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

    write_test_pbf(
        &old,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![] }],
        &[],
    );
    write_test_pbf(
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

    write_test_pbf(
        &old,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
        &[],
        &[],
    );
    write_test_pbf(
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

    write_test_pbf(
        &old,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "old")] }],
        &[],
        &[],
    );
    write_test_pbf(
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

    write_test_pbf(
        &old,
        &[],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] }],
        &[],
    );
    write_test_pbf(
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

    write_test_pbf(
        &old,
        &[],
        &[],
        &[TestRelation {
            id: 100,
            members: vec![TestMember { id: MemberId::Node(1), role: "stop" }],
            tags: vec![("type", "route")],
        }],
    );
    write_test_pbf(
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
    write_test_pbf(
        &old,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "old")] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")] }],
        &[],
    );
    write_test_pbf(
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

    write_test_pbf(
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
    write_test_pbf(
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

    write_test_pbf(
        &old,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
        &[],
        &[],
    );
    write_test_pbf(
        &new,
        &[TestNode { id: 1, lat: 150_000_000, lon: 250_000_000, tags: vec![] }],
        &[],
        &[],
    );

    let opts = DiffOptions {
        suppress_common: true,
        verbose: true,
        type_filter: None,
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

    write_test_pbf(
        &old,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
        &[TestWay { id: 10, refs: vec![1], tags: vec![] }],
        &[],
    );
    write_test_pbf(
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
