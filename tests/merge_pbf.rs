//! Multi-PBF merge correctness tests.

mod common;

use common::{
    TestMember, TestNode, TestRelation, TestWay, node_ids_id_only as node_ids,
    read_all_elements_id_only as read_all_elements, relation_ids_id_only as relation_ids,
    way_ids_id_only as way_ids, write_test_pbf_sorted,
};
use pbfhogg::MemberId;
use pbfhogg::cat::dedupe::{MergePbfOptions, merge_pbf};
use pbfhogg::writer::Compression;
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
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("name", "a")],
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
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &b,
        &[
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![("name", "c")],
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
        &[],
        &[],
    );

    let inputs: Vec<&std::path::Path> = vec![a.as_path(), b.as_path()];
    let stats = merge_pbf(
        &inputs,
        &output,
        &default_opts(),
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("merge_pbf");
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

    // Both files contain nodes 2 and 3 (exact duplicates - same id, no metadata = version 0)
    write_test_pbf_sorted(
        &a,
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
                tags: vec![("name", "shared")],
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
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &b,
        &[
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("name", "shared")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![],
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
        &[],
        &[],
    );

    let inputs: Vec<&std::path::Path> = vec![a.as_path(), b.as_path()];
    let stats = merge_pbf(
        &inputs,
        &output,
        &default_opts(),
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("merge_pbf");
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
    write_test_pbf_sorted(
        &b,
        &[TestNode {
            id: 3,
            lat: 120_000_000,
            lon: 220_000_000,
            tags: vec![],
            meta: None,
        }],
        &[TestWay {
            id: 20,
            refs: vec![3],
            tags: vec![("highway", "secondary")],
            meta: None,
        }],
        &[TestRelation {
            id: 100,
            members: vec![
                TestMember {
                    id: MemberId::Way(10),
                    role: "outer",
                },
                TestMember {
                    id: MemberId::Way(20),
                    role: "inner",
                },
            ],
            tags: vec![("type", "multipolygon")],
            meta: None,
        }],
    );

    let inputs: Vec<&std::path::Path> = vec![a.as_path(), b.as_path()];
    let stats = merge_pbf(
        &inputs,
        &output,
        &default_opts(),
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("merge_pbf");
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
        &[],
        &[],
    );

    let inputs: Vec<&std::path::Path> = vec![a.as_path()];
    let stats = merge_pbf(
        &inputs,
        &output,
        &default_opts(),
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("merge_pbf");
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
        &b,
        &[TestNode {
            id: 2,
            lat: 110_000_000,
            lon: 210_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &c_file,
        &[TestNode {
            id: 3,
            lat: 120_000_000,
            lon: 220_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[],
    );

    let inputs: Vec<&std::path::Path> = vec![a.as_path(), b.as_path(), c_file.as_path()];
    let stats = merge_pbf(
        &inputs,
        &output,
        &default_opts(),
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("merge_pbf");
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
    let stats = merge_pbf(
        &inputs,
        &output,
        &default_opts(),
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("merge_pbf");

    assert_eq!(stats.nodes, 0);
    assert_eq!(stats.ways, 0);
    assert_eq!(stats.relations, 0);
    assert_eq!(stats.duplicates_removed, 0);
}

// Pins that `merge_pbf` with the same input file supplied twice
// preserves every element type. The earlier regression dropped ways
// and relations because `cat/dedupe.rs`'s pass-2 loop grouped
// consecutive `overlaps[i]=true` entries into one `write_overlap_run`
// call without checking that the run stayed within a single element
// kind. Two back-to-back same-kind overlap pairs (node|node, way|way)
// merged into one run, which then inherited `entries[0].index.kind`
// and dropped elements of every other kind. Fixed 2026-04-23 by
// requiring the overlap-run walker to also match `index.kind`.
#[test]
#[allow(clippy::too_many_lines)]
fn merge_same_input_preserves_ways_and_relations() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &a,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("name", "n1")],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("name", "n2")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![("name", "n3")],
                meta: None,
            },
            TestNode {
                id: 4,
                lat: 130_000_000,
                lon: 230_000_000,
                tags: vec![("name", "n4")],
                meta: None,
            },
            TestNode {
                id: 5,
                lat: 140_000_000,
                lon: 240_000_000,
                tags: vec![("name", "n5")],
                meta: None,
            },
            TestNode {
                id: 6,
                lat: 150_000_000,
                lon: 250_000_000,
                tags: vec![("name", "n6")],
                meta: None,
            },
            TestNode {
                id: 7,
                lat: 160_000_000,
                lon: 260_000_000,
                tags: vec![("name", "n7")],
                meta: None,
            },
            TestNode {
                id: 8,
                lat: 170_000_000,
                lon: 270_000_000,
                tags: vec![("name", "n8")],
                meta: None,
            },
            TestNode {
                id: 9,
                lat: 180_000_000,
                lon: 280_000_000,
                tags: vec![("name", "n9")],
                meta: None,
            },
            TestNode {
                id: 10,
                lat: 190_000_000,
                lon: 290_000_000,
                tags: vec![("name", "n10")],
                meta: None,
            },
        ],
        &[
            TestWay {
                id: 100,
                refs: vec![1, 2, 3],
                tags: vec![("highway", "residential")],
                meta: None,
            },
            TestWay {
                id: 101,
                refs: vec![3, 4, 5],
                tags: vec![("highway", "service")],
                meta: None,
            },
            TestWay {
                id: 102,
                refs: vec![5, 6, 7],
                tags: vec![("waterway", "stream")],
                meta: None,
            },
            TestWay {
                id: 103,
                refs: vec![7, 8, 9, 10],
                tags: vec![("landuse", "meadow")],
                meta: None,
            },
        ],
        &[TestRelation {
            id: 1000,
            members: vec![
                TestMember {
                    id: MemberId::Way(100),
                    role: "outer",
                },
                TestMember {
                    id: MemberId::Way(101),
                    role: "inner",
                },
                TestMember {
                    id: MemberId::Way(102),
                    role: "subarea",
                },
                TestMember {
                    id: MemberId::Way(103),
                    role: "label",
                },
            ],
            tags: vec![("type", "multipolygon"), ("name", "fixture")],
            meta: None,
        }],
    );

    let inputs: Vec<&std::path::Path> = vec![a.as_path(), a.as_path()];
    let stats = merge_pbf(
        &inputs,
        &output,
        &default_opts(),
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("merge_pbf");
    let merged = read_all_elements(&output);

    assert_eq!(node_ids(&merged), vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    assert_eq!(way_ids(&merged), vec![100, 101, 102, 103]);
    assert_eq!(relation_ids(&merged), vec![1000]);
    assert_eq!(stats.nodes, 10);
    assert_eq!(stats.ways, 4);
    assert_eq!(stats.relations, 1);
}

/// F60: Three files with overlapping ID ranges - exercises 3-way heap merge.
#[test]
#[allow(clippy::too_many_lines)]
fn merge_three_files_overlapping_ids() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a.osm.pbf");
    let b = dir.path().join("b.osm.pbf");
    let c_file = dir.path().join("c.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    // Three files with overlapping node ID ranges:
    // A: 1, 3, 5    B: 2, 3, 4    C: 3, 5, 6
    // Node 3 appears in all three, node 5 in A+C - tests 3-way dedup
    write_test_pbf_sorted(
        &a,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("src", "a")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![("src", "a")],
                meta: None,
            },
            TestNode {
                id: 5,
                lat: 140_000_000,
                lon: 240_000_000,
                tags: vec![("src", "a")],
                meta: None,
            },
        ],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &b,
        &[
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("src", "b")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![("src", "b")],
                meta: None,
            },
            TestNode {
                id: 4,
                lat: 130_000_000,
                lon: 230_000_000,
                tags: vec![("src", "b")],
                meta: None,
            },
        ],
        &[],
        &[],
    );
    write_test_pbf_sorted(
        &c_file,
        &[
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![("src", "c")],
                meta: None,
            },
            TestNode {
                id: 5,
                lat: 140_000_000,
                lon: 240_000_000,
                tags: vec![("src", "c")],
                meta: None,
            },
            TestNode {
                id: 6,
                lat: 150_000_000,
                lon: 250_000_000,
                tags: vec![("src", "c")],
                meta: None,
            },
        ],
        &[],
        &[],
    );

    let inputs: Vec<&std::path::Path> = vec![a.as_path(), b.as_path(), c_file.as_path()];
    let stats = merge_pbf(
        &inputs,
        &output,
        &default_opts(),
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("merge_pbf");
    let c = read_all_elements(&output);

    // Output should contain exactly 6 unique nodes (1-6), sorted
    assert_eq!(node_ids(&c), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(stats.nodes, 6);
    // Node 3 appears in 3 files (2 duplicates), node 5 in 2 files (1 duplicate)
    assert_eq!(
        stats.duplicates_removed, 3,
        "3 duplicates: node 3 ×2 + node 5 ×1"
    );
}
