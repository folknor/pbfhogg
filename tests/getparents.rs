//! getparents correctness tests.

mod common;

use common::{
    node_ids_id_only as node_ids, read_all_elements_id_only as read_all_elements,
    way_ids_id_only as way_ids, relation_ids_id_only as relation_ids,
    write_test_pbf, TestMember, TestNode, TestRelation, TestWay,
};
use pbfhogg::getid::parse_ids;
use pbfhogg::getparents::{getparents, GetparentsOptions};
use pbfhogg::writer::Compression;
use pbfhogg::MemberId;
use tempfile::TempDir;

fn ids(strs: &[&str]) -> Vec<String> {
    strs.iter().map(ToString::to_string).collect()
}

fn default_opts() -> GetparentsOptions {
    GetparentsOptions { add_self: false }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn ways_referencing_node() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] },
            TestWay { id: 11, refs: vec![2, 3], tags: vec![("highway", "secondary")] },
            TestWay { id: 12, refs: vec![3], tags: vec![] },
        ],
        &[],
    );

    // Find ways referencing node 2
    let id_set = parse_ids(&ids(&["n2"])).expect("parse ids");
    let stats = getparents(&input, &output, &id_set, &default_opts(), Compression::default(), false, &pbfhogg::HeaderOverrides::default()).expect("getparents");
    let c = read_all_elements(&output);

    // Ways 10 and 11 reference node 2
    assert_eq!(way_ids(&c), vec![10, 11]);
    assert!(node_ids(&c).is_empty(), "no nodes without --add-self");
    assert_eq!(stats.ways_written, 2);
    assert_eq!(stats.nodes_written, 0);
}

#[test]
fn relations_referencing_way() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![] },
            TestWay { id: 11, refs: vec![2, 3], tags: vec![] },
        ],
        &[
            TestRelation {
                id: 100,
                members: vec![
                    TestMember { id: MemberId::Way(10), role: "outer" },
                ],
                tags: vec![("type", "multipolygon")],
            },
            TestRelation {
                id: 101,
                members: vec![
                    TestMember { id: MemberId::Way(11), role: "inner" },
                ],
                tags: vec![("type", "multipolygon")],
            },
        ],
    );

    // Find relations referencing way 10
    let id_set = parse_ids(&ids(&["w10"])).expect("parse ids");
    let stats = getparents(&input, &output, &id_set, &default_opts(), Compression::default(), false, &pbfhogg::HeaderOverrides::default()).expect("getparents");
    let c = read_all_elements(&output);

    assert_eq!(relation_ids(&c), vec![100]);
    assert!(way_ids(&c).is_empty());
    assert_eq!(stats.relations_written, 1);
}

#[test]
fn add_self_includes_queried_objects() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "test")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] },
        ],
        &[],
    );

    // Find parents of node 1 WITH --add-self
    let id_set = parse_ids(&ids(&["n1"])).expect("parse ids");
    let opts = GetparentsOptions { add_self: true };
    let stats = getparents(&input, &output, &id_set, &opts, Compression::default(), false, &pbfhogg::HeaderOverrides::default()).expect("getparents");
    let c = read_all_elements(&output);

    // Node 1 itself + way 10 (references node 1)
    assert_eq!(node_ids(&c), vec![1]);
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(stats.nodes_written, 1);
    assert_eq!(stats.ways_written, 1);
}

#[test]
fn no_transitive_relations() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1], tags: vec![] },
        ],
        &[
            TestRelation {
                id: 100,
                members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
                tags: vec![("type", "multipolygon")],
            },
        ],
    );

    // Find parents of node 1: should find way 10 but NOT relation 100
    // (relation references way, not node directly)
    let id_set = parse_ids(&ids(&["n1"])).expect("parse ids");
    let stats = getparents(&input, &output, &id_set, &default_opts(), Compression::default(), false, &pbfhogg::HeaderOverrides::default()).expect("getparents");
    let c = read_all_elements(&output);

    assert_eq!(way_ids(&c), vec![10]);
    assert!(relation_ids(&c).is_empty(), "no transitive relations");
    assert_eq!(stats.ways_written, 1);
    assert_eq!(stats.relations_written, 0);
}

#[test]
fn empty_result() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1], tags: vec![] },
        ],
        &[],
    );

    // Node 999 doesn't exist — no parents found
    let id_set = parse_ids(&ids(&["n999"])).expect("parse ids");
    let stats = getparents(&input, &output, &id_set, &default_opts(), Compression::default(), false, &pbfhogg::HeaderOverrides::default()).expect("getparents");

    assert_eq!(stats.nodes_written, 0);
    assert_eq!(stats.ways_written, 0);
    assert_eq!(stats.relations_written, 0);
}
