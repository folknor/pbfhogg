//! getid / removeid correctness tests.

mod common;

use common::{
    generate_nodes, generate_ways, node_ids_id_only as node_ids,
    read_all_elements_id_only as read_all_elements, relation_ids_id_only as relation_ids,
    way_ids_id_only as way_ids, write_multi_block_test_pbf, write_test_pbf, TestNode,
    TestRelation, TestWay,
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "b")], meta: None },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![("name", "c")], meta: None },
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")], meta: None },
            TestWay { id: 11, refs: vec![1], tags: vec![], meta: None },
        ],
        &[
            TestRelation { id: 100, members: vec![], tags: vec![("type", "route")], meta: None },
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![], meta: None },
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![], meta: None },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")], meta: None },
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![], meta: None },
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![], meta: None },
            TestWay { id: 11, refs: vec![1], tags: vec![], meta: None },
        ],
        &[
            TestRelation { id: 100, members: vec![], tags: vec![], meta: None },
            TestRelation { id: 101, members: vec![], tags: vec![], meta: None },
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "test"), ("amenity", "bench")], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![], meta: None },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![], meta: None },
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![], meta: None },
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![], meta: None },
        ],
        &[
            TestWay { id: 10, refs: vec![2, 3], tags: vec![], meta: None },
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "keep")], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "strip")], meta: None },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![("name", "strip")], meta: None },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")], meta: None },
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

// ---------------------------------------------------------------------------
// Multi-blob raw passthrough for `removeid` (the --invert path)
// ---------------------------------------------------------------------------
//
// `filter_by_id` with `include = false` (the `removeid` entry) takes
// the raw-passthrough path at `src/commands/getid/mod.rs:347-358` for
// any OsmData blob whose ID range does not intersect the removal set.
// Blobs that DO intersect are decoded, filtered element-by-element,
// and re-emitted. With existing tests using single-blob fixtures, the
// raw-passthrough branch never fires under test - the intersection is
// always true because there's only one blob containing all ids.
//
// This test forces four node blobs, then removes ids confined to ONE
// blob. The three remaining blobs should pass through raw; the
// affected blob should be decoded, filtered, and re-emitted without
// the removed ids. `GetidStats` does not expose a passthrough counter,
// so the assertion is correctness-based: the non-removed ids in
// every blob must survive in file order, and the removed ids must be
// absent from the output.

#[test]
fn removeid_multi_blob_passthrough() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    // 40 nodes at block_size=10 -> 4 node blobs covering id ranges
    // 1-10, 11-20, 21-30, 31-40. Remove ids 11-20 (one blob's worth);
    // the other three blobs should take the raw-passthrough branch.
    let nodes = generate_nodes(40, 1);
    let ways = generate_ways(5, 1_000, 2, 1);
    write_multi_block_test_pbf(&input, &nodes, &ways, &[], 10);

    let id_set = parse_ids(&ids(
        &(11..=20).map(|i| format!("n{i}")).collect::<Vec<_>>()
            .iter().map(String::as_str).collect::<Vec<_>>(),
    ))
    .expect("parse ids");
    let stats = removeid(
        &input,
        &output,
        &id_set,
        Compression::default(),
        false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("removeid");

    let c = read_all_elements(&output);
    let want_nodes: Vec<i64> = (1..=10).chain(21..=40).collect();
    assert_eq!(
        node_ids(&c),
        want_nodes,
        "non-removed node ids must survive across all blobs"
    );
    // Ways untouched.
    assert_eq!(way_ids(&c), (1_000..1_005).collect::<Vec<_>>());
    assert_eq!(stats.nodes_written, 30);
    assert_eq!(stats.ways_written, 5);
}
