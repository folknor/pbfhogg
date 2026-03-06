//! getid / removeid correctness tests.

mod common;

use common::{
    node_ids_id_only as node_ids, read_all_elements_id_only as read_all_elements,
    way_ids_id_only as way_ids, relation_ids_id_only as relation_ids,
    write_test_pbf, TestNode, TestRelation, TestWay,
};
use pbfhogg::getid::{getid, parse_ids, removeid, GetidOptions};
use pbfhogg::writer::Compression;
use tempfile::TempDir;

fn ids(strs: &[&str]) -> Vec<String> {
    strs.iter().map(ToString::to_string).collect()
}

fn default_opts() -> GetidOptions {
    GetidOptions { add_referenced: false, remove_tags: false }
}

fn add_ref_opts() -> GetidOptions {
    GetidOptions { add_referenced: true, remove_tags: false }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn getid_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "b")] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![("name", "c")] },
        ],
        &[],
        &[],
    );

    let id_set = parse_ids(&ids(&["n1", "n3"])).expect("parse ids");
    let stats = getid(&input, &output, &id_set, &default_opts(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("getid");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 3]);
    assert_eq!(stats.nodes_written, 2);
}

#[test]
fn getid_mixed_types() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")] },
            TestWay { id: 11, refs: vec![1], tags: vec![] },
        ],
        &[
            TestRelation { id: 100, members: vec![], tags: vec![("type", "route")] },
        ],
    );

    let id_set = parse_ids(&ids(&["n2", "w10", "r100"])).expect("parse ids");
    let stats = getid(&input, &output, &id_set, &default_opts(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("getid");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![2]);
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(relation_ids(&c), vec![100]);
    assert_eq!(stats.nodes_written, 1);
    assert_eq!(stats.ways_written, 1);
    assert_eq!(stats.relations_written, 1);
}

#[test]
fn getid_add_referenced() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")] },
        ],
        &[],
    );

    // Without --add-referenced: only the way
    let id_set = parse_ids(&ids(&["w10"])).expect("parse ids");
    let stats = getid(&input, &output, &id_set, &default_opts(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("getid");
    let c = read_all_elements(&output);
    assert!(node_ids(&c).is_empty());
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(stats.ways_written, 1);
    assert_eq!(stats.nodes_written, 0);

    // With --add-referenced: way + its referenced nodes
    let output2 = dir.path().join("output2.osm.pbf");
    let stats = getid(&input, &output2, &id_set, &add_ref_opts(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("getid");
    let c = read_all_elements(&output2);
    assert_eq!(node_ids(&c), vec![1, 2, 3]);
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(stats.nodes_written, 3);
    assert_eq!(stats.ways_written, 1);
}

#[test]
fn removeid_nodes() {
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
        &[],
        &[],
    );

    let id_set = parse_ids(&ids(&["n2"])).expect("parse ids");
    let stats = removeid(&input, &output, &id_set, Compression::default(), false, &pbfhogg::HeaderOverrides::default()).expect("removeid");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 3]);
    assert_eq!(stats.nodes_written, 2);
}

#[test]
fn removeid_mixed_types() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![] },
            TestWay { id: 11, refs: vec![1], tags: vec![] },
        ],
        &[
            TestRelation { id: 100, members: vec![], tags: vec![] },
            TestRelation { id: 101, members: vec![], tags: vec![] },
        ],
    );

    let id_set = parse_ids(&ids(&["n1", "w11", "r100"])).expect("parse ids");
    let stats = removeid(&input, &output, &id_set, Compression::default(), false, &pbfhogg::HeaderOverrides::default()).expect("removeid");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![2]);
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(relation_ids(&c), vec![101]);
    assert_eq!(stats.nodes_written, 1);
    assert_eq!(stats.ways_written, 1);
    assert_eq!(stats.relations_written, 1);
}

#[test]
fn getid_empty_result() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
        ],
        &[],
        &[],
    );

    let id_set = parse_ids(&ids(&["n999"])).expect("parse ids");
    let stats = getid(&input, &output, &id_set, &default_opts(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("getid");
    let c = read_all_elements(&output);

    assert!(node_ids(&c).is_empty());
    assert_eq!(stats.nodes_written, 0);
}

#[test]
fn getid_preserves_tags() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "test"), ("amenity", "bench")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[],
        &[],
    );

    let id_set = parse_ids(&ids(&["n1"])).expect("parse ids");
    getid(&input, &output, &id_set, &default_opts(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("getid");
    let c = read_all_elements(&output);

    assert_eq!(c.nodes.len(), 1);
    let (id, ref tags) = c.nodes[0];
    assert_eq!(id, 1);
    assert_eq!(
        tags,
        &[
            ("name".to_string(), "test".to_string()),
            ("amenity".to_string(), "bench".to_string()),
        ]
    );
}

#[test]
fn getid_from_id_file() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let id_file = dir.path().join("ids.txt");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![] },
        ],
        &[],
    );

    std::fs::write(&id_file, "# comment\nn1\nw10\n\nn3\n").expect("write id file");

    let id_set = pbfhogg::getid::parse_ids_from_file(&id_file).expect("parse file");
    let stats = getid(&input, &output, &id_set, &default_opts(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("getid");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 3]);
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(stats.nodes_written, 2);
    assert_eq!(stats.ways_written, 1);
}

#[test]
fn getid_add_referenced_plus_direct_node() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![2, 3], tags: vec![] },
        ],
        &[],
    );

    // Request node 1 directly + way 10 with refs → should get nodes 1,2,3 + way 10
    let id_set = parse_ids(&ids(&["n1", "w10"])).expect("parse ids");
    let stats = getid(&input, &output, &id_set, &add_ref_opts(), Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("getid");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2, 3]);
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(stats.nodes_written, 3);
    assert_eq!(stats.ways_written, 1);
}

#[test]
fn getid_remove_tags_strips_referenced_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "keep")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "strip")] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![("name", "strip")] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")] },
        ],
        &[],
    );

    // Request n1 + w10 with --add-referenced --remove-tags.
    // Node 1 is explicitly requested → keep tags.
    // Nodes 2,3 are referenced-only → strip tags.
    let id_set = parse_ids(&ids(&["n1", "w10"])).expect("parse ids");
    let opts = GetidOptions { add_referenced: true, remove_tags: true };
    let stats = getid(&input, &output, &id_set, &opts, Compression::default(), false, true, &pbfhogg::HeaderOverrides::default()).expect("getid");

    assert_eq!(stats.nodes_written, 3);
    assert_eq!(stats.ways_written, 1);

    let c = read_all_elements(&output);
    assert_eq!(c.nodes.len(), 3);

    // Node 1: explicitly requested, tags preserved
    let (id1, ref tags1) = c.nodes[0];
    assert_eq!(id1, 1);
    assert_eq!(tags1, &[("name".to_string(), "keep".to_string())]);

    // Nodes 2,3: referenced-only, tags stripped
    let (id2, ref tags2) = c.nodes[1];
    assert_eq!(id2, 2);
    assert!(tags2.is_empty(), "referenced-only node 2 should have no tags, got: {tags2:?}");

    let (id3, ref tags3) = c.nodes[2];
    assert_eq!(id3, 3);
    assert!(tags3.is_empty(), "referenced-only node 3 should have no tags, got: {tags3:?}");
}
