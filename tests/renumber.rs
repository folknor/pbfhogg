//! Renumber correctness tests.

mod common;

use common::{
    read_all_elements_id_only as read_all_elements,
    node_ids_id_only as node_ids, way_ids_id_only as way_ids,
    relation_ids_id_only as relation_ids,
    write_test_pbf_sorted, TestMember, TestNode, TestRelation, TestWay,
};
use pbfhogg::renumber::{renumber, RenumberOptions};
use pbfhogg::writer::Compression;
use pbfhogg::MemberId;
use tempfile::TempDir;

fn default_opts() -> RenumberOptions {
    RenumberOptions {
        start_node_id: 1,
        start_way_id: 1,
        start_relation_id: 1,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn renumber_nodes_sequential() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode { id: 100, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")] },
            TestNode { id: 200, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 300, lat: 120_000_000, lon: 220_000_000, tags: vec![("name", "c")] },
        ],
        &[],
        &[],
    );

    let stats = renumber(&input, &output, &default_opts(), Compression::default(), false, &pbfhogg::HeaderOverrides::default())
        .expect("renumber");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2, 3]);
    assert_eq!(stats.nodes_written, 3);
    assert_eq!(stats.ways_written, 0);
    assert_eq!(stats.relations_written, 0);
}

#[test]
fn renumber_ways_remap_refs() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode { id: 10, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 20, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 100, refs: vec![10, 20], tags: vec![("highway", "primary")] },
        ],
        &[],
    );

    let stats = renumber(&input, &output, &default_opts(), Compression::default(), false, &pbfhogg::HeaderOverrides::default())
        .expect("renumber");
    let c = read_all_elements(&output);

    // Nodes renumbered: 10→1, 20→2
    assert_eq!(node_ids(&c), vec![1, 2]);
    // Way renumbered: 100→1
    assert_eq!(way_ids(&c), vec![1]);
    assert_eq!(stats.nodes_written, 2);
    assert_eq!(stats.ways_written, 1);
}

#[test]
fn renumber_relations_remap_members() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode { id: 50, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 80, refs: vec![50], tags: vec![] },
        ],
        &[
            TestRelation {
                id: 200,
                members: vec![
                    TestMember { id: MemberId::Node(50), role: "stop" },
                    TestMember { id: MemberId::Way(80), role: "outer" },
                ],
                tags: vec![("type", "multipolygon")],
            },
        ],
    );

    let stats = renumber(&input, &output, &default_opts(), Compression::default(), false, &pbfhogg::HeaderOverrides::default())
        .expect("renumber");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1]);
    assert_eq!(way_ids(&c), vec![1]);
    assert_eq!(relation_ids(&c), vec![1]);
    assert_eq!(stats.nodes_written, 1);
    assert_eq!(stats.ways_written, 1);
    assert_eq!(stats.relations_written, 1);
}

#[test]
fn custom_start_ids() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![] },
        ],
        &[
            TestRelation {
                id: 100,
                members: vec![
                    TestMember { id: MemberId::Way(10), role: "outer" },
                ],
                tags: vec![("type", "multipolygon")],
            },
        ],
    );

    let opts = RenumberOptions {
        start_node_id: 1000,
        start_way_id: 2000,
        start_relation_id: 3000,
    };

    let stats = renumber(&input, &output, &opts, Compression::default(), false, &pbfhogg::HeaderOverrides::default())
        .expect("renumber");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1000, 1001]);
    assert_eq!(way_ids(&c), vec![2000]);
    assert_eq!(relation_ids(&c), vec![3000]);
    assert_eq!(stats.nodes_written, 2);
    assert_eq!(stats.ways_written, 1);
    assert_eq!(stats.relations_written, 1);
}

#[test]
fn empty_input() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(&input, &[], &[], &[]);

    let stats = renumber(&input, &output, &default_opts(), Compression::default(), false, &pbfhogg::HeaderOverrides::default())
        .expect("renumber");

    assert_eq!(stats.nodes_written, 0);
    assert_eq!(stats.ways_written, 0);
    assert_eq!(stats.relations_written, 0);
}
