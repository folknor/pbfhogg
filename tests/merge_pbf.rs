//! Multi-PBF merge correctness tests.

mod common;

use common::{
    read_all_elements_id_only as read_all_elements, node_ids_id_only as node_ids,
    way_ids_id_only as way_ids, relation_ids_id_only as relation_ids,
    write_test_pbf_sorted, TestMember, TestNode, TestRelation, TestWay,
};
use pbfhogg::merge_pbf::{merge_pbf, MergePbfOptions};
use pbfhogg::writer::Compression;
use pbfhogg::MemberId;
use tempfile::TempDir;

fn default_opts() -> MergePbfOptions {
    MergePbfOptions {
        compression: Compression::default(),
        direct_io: false,
        io_uring: false,
        force: true, // test PBFs lack indexdata
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn merge_disjoint_node_files() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &a,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &b,
        &[
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![("name", "c")] },
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![] },
        ],
        &[],
        &[],
    );

    let inputs: Vec<&std::path::Path> = vec![a.as_path(), b.as_path()];
    let stats = merge_pbf(&inputs, &output, &default_opts()).expect("merge_pbf");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2, 3, 4]);
    assert_eq!(stats.nodes, 4);
    assert_eq!(stats.duplicates_removed, 0);
}

#[test]
fn merge_overlapping_nodes_dedup() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    // Both files contain nodes 2 and 3 (exact duplicates — same id, no metadata = version 0)
    write_test_pbf_sorted(
        &a,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "shared")] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
        ],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &b,
        &[
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "shared")] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![] },
        ],
        &[],
        &[],
    );

    let inputs: Vec<&std::path::Path> = vec![a.as_path(), b.as_path()];
    let stats = merge_pbf(&inputs, &output, &default_opts()).expect("merge_pbf");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2, 3, 4]);
    assert_eq!(stats.nodes, 4);
    assert_eq!(stats.duplicates_removed, 2);
}

#[test]
fn merge_with_ways_and_relations() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &a,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] },
        ],
        &[],
    );
    write_test_pbf_sorted(
        &b,
        &[
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 20, refs: vec![3], tags: vec![("highway", "secondary")] },
        ],
        &[
            TestRelation {
                id: 100,
                members: vec![
                    TestMember { id: MemberId::Way(10), role: "outer" },
                    TestMember { id: MemberId::Way(20), role: "inner" },
                ],
                tags: vec![("type", "multipolygon")],
            },
        ],
    );

    let inputs: Vec<&std::path::Path> = vec![a.as_path(), b.as_path()];
    let stats = merge_pbf(&inputs, &output, &default_opts()).expect("merge_pbf");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2, 3]);
    assert_eq!(way_ids(&c), vec![10, 20]);
    assert_eq!(relation_ids(&c), vec![100]);
    assert_eq!(stats.nodes, 3);
    assert_eq!(stats.ways, 2);
    assert_eq!(stats.relations, 1);
}

#[test]
fn merge_single_file() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &a,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[],
        &[],
    );

    let inputs: Vec<&std::path::Path> = vec![a.as_path()];
    let stats = merge_pbf(&inputs, &output, &default_opts()).expect("merge_pbf");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2]);
    assert_eq!(stats.nodes, 2);
    assert_eq!(stats.duplicates_removed, 0);
}

#[test]
fn merge_three_files() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    let c_file = dir.path().join("c.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &a,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &b,
        &[TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &c_file,
        &[TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] }],
        &[],
        &[],
    );

    let inputs: Vec<&std::path::Path> = vec![a.as_path(), b.as_path(), c_file.as_path()];
    let stats = merge_pbf(&inputs, &output, &default_opts()).expect("merge_pbf");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2, 3]);
    assert_eq!(stats.nodes, 3);
    assert_eq!(stats.duplicates_removed, 0);
}

#[test]
fn merge_empty_files() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(&a, &[], &[], &[]);
    write_test_pbf_sorted(&b, &[], &[], &[]);

    let inputs: Vec<&std::path::Path> = vec![a.as_path(), b.as_path()];
    let stats = merge_pbf(&inputs, &output, &default_opts()).expect("merge_pbf");

    assert_eq!(stats.nodes, 0);
    assert_eq!(stats.ways, 0);
    assert_eq!(stats.relations, 0);
    assert_eq!(stats.duplicates_removed, 0);
}
