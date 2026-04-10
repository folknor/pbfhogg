//! Renumber correctness tests.

mod common;

use common::{
    read_all_elements_id_only as read_all_elements,
    read_all_elements_with_coords,
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

// ---------------------------------------------------------------------------
// F54: Verify way refs and relation member IDs are actually remapped
// ---------------------------------------------------------------------------

#[test]
fn renumber_way_refs_actually_remapped() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode { id: 10, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 20, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 30, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 100, refs: vec![10, 20, 30], tags: vec![("highway", "primary")] },
        ],
        &[],
    );

    renumber(&input, &output, &default_opts(), Compression::default(), false, &pbfhogg::HeaderOverrides::default())
        .expect("renumber");

    let c = read_all_elements_with_coords(&output);

    // Nodes renumbered: 10→1, 20→2, 30→3
    assert_eq!(c.nodes.len(), 3);
    assert_eq!(c.nodes[0].0, 1);
    assert_eq!(c.nodes[1].0, 2);
    assert_eq!(c.nodes[2].0, 3);

    // Way refs must reference the NEW node IDs, not the old ones
    assert_eq!(c.ways.len(), 1);
    assert_eq!(c.ways[0].0, 1); // way 100→1
    assert_eq!(c.ways[0].1, vec![1, 2, 3], "way refs should be remapped to new node IDs");
}

#[test]
fn renumber_relation_member_ids_actually_remapped() {
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

    renumber(&input, &output, &default_opts(), Compression::default(), false, &pbfhogg::HeaderOverrides::default())
        .expect("renumber");

    let c = read_all_elements_with_coords(&output);

    // Node 50→1, Way 80→1, Relation 200→1
    assert_eq!(c.relations.len(), 1);
    let members = &c.relations[0].1;
    assert_eq!(members.len(), 2);
    // Member node ref should be remapped: 50→1
    assert_eq!(members[0].0, 1, "node member ref should be remapped");
    assert_eq!(members[0].1, "node");
    assert_eq!(members[0].2, "stop");
    // Member way ref should be remapped: 80→1
    assert_eq!(members[1].0, 1, "way member ref should be remapped");
    assert_eq!(members[1].1, "way");
    assert_eq!(members[1].2, "outer");
}

// ---------------------------------------------------------------------------
// F55: Relation referencing relation
// ---------------------------------------------------------------------------

#[test]
fn renumber_relation_referencing_relation() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode { id: 10, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
        ],
        &[],
        &[
            TestRelation {
                id: 500,
                members: vec![
                    TestMember { id: MemberId::Node(10), role: "label" },
                ],
                tags: vec![("type", "boundary")],
            },
            TestRelation {
                id: 600,
                members: vec![
                    TestMember { id: MemberId::Relation(500), role: "subarea" },
                ],
                tags: vec![("type", "boundary")],
            },
        ],
    );

    renumber(&input, &output, &default_opts(), Compression::default(), false, &pbfhogg::HeaderOverrides::default())
        .expect("renumber");

    let c = read_all_elements_with_coords(&output);

    // Node 10→1, Relation 500→1, Relation 600→2
    assert_eq!(c.nodes[0].0, 1);
    assert_eq!(c.relations.len(), 2);
    assert_eq!(c.relations[0].0, 1); // rel 500→1
    assert_eq!(c.relations[1].0, 2); // rel 600→2

    // Relation 600's member should reference remapped relation 500→1
    let members = &c.relations[1].1;
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].0, 1, "relation member ref should be remapped: 500→1");
    assert_eq!(members[0].1, "relation");
    assert_eq!(members[0].2, "subarea");
}
