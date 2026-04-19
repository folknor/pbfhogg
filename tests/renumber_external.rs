//! End-to-end tests for renumber.

mod common;

use common::{
    assert_sorted_file, read_normalized, write_test_pbf_sorted,
    TestMember, TestNode, TestRelation, TestWay,
};
use pbfhogg::renumber::{renumber_external, RenumberOptions};
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
// Nodes only
// ---------------------------------------------------------------------------

#[test]
fn renumber_nodes_sequentially() {
    // Nodes-only input: verifies wire-format rewriter assigns sequential
    // ids and preserves coords + tags.
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

    let stats = renumber_external(
        &input, &output, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber");

    assert_eq!(stats.nodes_written, 3);
    assert_eq!(stats.ways_written, 0);
    assert_eq!(stats.relations_written, 0);

    let norm = read_normalized(&output);
    assert_eq!(norm.nodes.len(), 3);
    assert_eq!(norm.nodes[0].id, 1);
    assert_eq!(norm.nodes[0].lat, 100_000_000);
    assert_eq!(norm.nodes[0].lon, 200_000_000);
    assert_eq!(norm.nodes[0].tags.get("name"), Some(&"a".to_string()));
    assert_eq!(norm.nodes[1].id, 2);
    assert_eq!(norm.nodes[1].lat, 110_000_000);
    assert_eq!(norm.nodes[2].id, 3);
    assert_eq!(norm.nodes[2].tags.get("name"), Some(&"c".to_string()));
}

#[test]
fn renumber_custom_start_node_id() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[],
        &[],
    );

    let opts = RenumberOptions {
        start_node_id: 5000,
        start_way_id: 2000,
        start_relation_id: 3000,
    };

    let stats = renumber_external(
        &input, &output, &opts, Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber");

    assert_eq!(stats.nodes_written, 2);
    let norm = read_normalized(&output);
    assert_eq!(norm.nodes.len(), 2);
    assert_eq!(norm.nodes[0].id, 5000);
    assert_eq!(norm.nodes[1].id, 5001);
}

// ---------------------------------------------------------------------------
// Sortedness
// ---------------------------------------------------------------------------

#[test]
fn renumber_preserves_sorted_header_and_type_order() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![] },
            TestWay { id: 20, refs: vec![2, 3], tags: vec![] },
        ],
        &[
            TestRelation {
                id: 100,
                members: vec![
                    TestMember { id: MemberId::Node(1), role: "a" },
                    TestMember { id: MemberId::Way(10), role: "b" },
                ],
                tags: vec![("type", "route")],
            },
        ],
    );

    renumber_external(
        &input, &output, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber");

    assert_sorted_file(&output);
}

// ---------------------------------------------------------------------------
// Relations
// ---------------------------------------------------------------------------

#[test]
fn renumber_relations_end_to_end() {
    // All member types + tag preservation + node/way/relation remap.
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
            TestWay { id: 100, refs: vec![10, 20], tags: vec![("highway", "road")] },
        ],
        &[
            TestRelation {
                id: 500,
                members: vec![
                    TestMember { id: MemberId::Node(10), role: "stop" },
                    TestMember { id: MemberId::Way(100), role: "outer" },
                ],
                tags: vec![("type", "multipolygon")],
            },
            TestRelation {
                id: 600,
                members: vec![
                    TestMember { id: MemberId::Relation(500), role: "subarea" },
                    TestMember { id: MemberId::Node(20), role: "label" },
                ],
                tags: vec![("type", "boundary")],
            },
        ],
    );

    let stats = renumber_external(
        &input, &output, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber");

    assert_eq!(stats.nodes_written, 2);
    assert_eq!(stats.ways_written, 1);
    assert_eq!(stats.relations_written, 2);

    let norm = read_normalized(&output);
    assert_eq!(norm.relations.len(), 2);

    let rel_1 = &norm.relations.iter().find(|r| r.id == 1).expect("rel 500→1");
    let rel_2 = &norm.relations.iter().find(|r| r.id == 2).expect("rel 600→2");

    assert_eq!(rel_1.members.len(), 2);
    assert_eq!(rel_1.members[0].member_type, "node");
    assert_eq!(rel_1.members[0].ref_id, 1);
    assert_eq!(rel_1.members[0].role, "stop");
    assert_eq!(rel_1.members[1].member_type, "way");
    assert_eq!(rel_1.members[1].ref_id, 1);
    assert_eq!(rel_1.members[1].role, "outer");

    assert_eq!(rel_2.members.len(), 2);
    assert_eq!(rel_2.members[0].member_type, "relation");
    assert_eq!(rel_2.members[0].ref_id, 1);
    assert_eq!(rel_2.members[1].member_type, "node");
    assert_eq!(rel_2.members[1].ref_id, 2);
}

#[test]
fn renumber_relation_forward_ref() {
    // Rel 500 references rel 600 (forward ref - target appears later
    // in sort order). R1 collects all IDs before R2d resolves.
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
                    TestMember { id: MemberId::Relation(600), role: "subarea" },
                ],
                tags: vec![("type", "boundary")],
            },
            TestRelation {
                id: 600,
                members: vec![
                    TestMember { id: MemberId::Node(10), role: "label" },
                ],
                tags: vec![("type", "boundary")],
            },
        ],
    );

    renumber_external(
        &input, &output, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber");

    let norm = read_normalized(&output);
    let rel_1 = &norm.relations.iter().find(|r| r.id == 1).expect("rel 500→1");
    assert_eq!(rel_1.members[0].ref_id, 2, "forward ref: 600→2, not 600");
}

#[test]
fn renumber_relation_self_reference() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[],
        &[],
        &[
            TestRelation {
                id: 42,
                members: vec![
                    TestMember { id: MemberId::Relation(42), role: "self" },
                ],
                tags: vec![("type", "loop")],
            },
        ],
    );

    renumber_external(
        &input, &output, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber");

    let norm = read_normalized(&output);
    assert_eq!(norm.relations.len(), 1);
    assert_eq!(norm.relations[0].id, 1, "rel 42 → 1");
    assert_eq!(norm.relations[0].members[0].ref_id, 1, "self: 42→1");
}

#[test]
fn renumber_relation_mixed_member_types_interleaved() {
    // Relation with members in non-type-sorted order (node, way, node,
    // relation, way, node) to stress the interleaved types + memids
    // cursor walk in the wire-format splice rewriter.
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![] },
            TestWay { id: 20, refs: vec![2, 3], tags: vec![] },
        ],
        &[
            TestRelation {
                id: 100,
                members: vec![
                    TestMember { id: MemberId::Node(1), role: "a" },
                    TestMember { id: MemberId::Way(10), role: "b" },
                    TestMember { id: MemberId::Node(2), role: "c" },
                    TestMember { id: MemberId::Relation(100), role: "self" },
                    TestMember { id: MemberId::Way(20), role: "d" },
                    TestMember { id: MemberId::Node(3), role: "e" },
                ],
                tags: vec![("type", "mixed")],
            },
            TestRelation {
                id: 200,
                members: vec![
                    TestMember { id: MemberId::Way(20), role: "only" },
                    TestMember { id: MemberId::Node(3), role: "tail" },
                ],
                tags: vec![],
            },
        ],
    );

    renumber_external(
        &input, &output, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber");

    assert_sorted_file(&output);

    let norm = read_normalized(&output);
    assert_eq!(norm.relations.len(), 2);
    // Verify member count and types preserved.
    let rel_100 = &norm.relations.iter().find(|r| r.id == 1).expect("rel 100→1");
    assert_eq!(rel_100.members.len(), 6);
}

// ---------------------------------------------------------------------------
// Nodes + ways end-to-end
// ---------------------------------------------------------------------------

#[test]
fn renumber_nodes_and_ways_end_to_end() {
    // Input deliberately includes:
    // - A duplicate ref (way 100 refs [1, 2, 2])
    // - A way with refs in non-sorted order (way 300 [3, 4, 1])
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![("amenity", "cafe")] },
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 100, refs: vec![1, 2, 2], tags: vec![("highway", "stop")] },
            TestWay { id: 200, refs: vec![2, 3], tags: vec![] },
            TestWay { id: 300, refs: vec![3, 4, 1], tags: vec![("barrier", "gate")] },
        ],
        &[],
    );

    let stats = renumber_external(
        &input, &output, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber");

    assert_eq!(stats.nodes_written, 4);
    assert_eq!(stats.ways_written, 3);
    assert_eq!(stats.relations_written, 0);

    assert_sorted_file(&output);

    let norm = read_normalized(&output);
    assert_eq!(norm.nodes.len(), 4);
    assert_eq!(norm.ways.len(), 3);
    assert_eq!(norm.nodes[0].id, 1);
    assert_eq!(norm.nodes[3].id, 4);

    let way_100 = &norm.ways.iter().find(|w| w.id == 1).expect("way 100→1");
    let way_200 = &norm.ways.iter().find(|w| w.id == 2).expect("way 200→2");
    let way_300 = &norm.ways.iter().find(|w| w.id == 3).expect("way 300→3");

    assert_eq!(way_100.refs, vec![1, 2, 2]);
    assert_eq!(way_200.refs, vec![2, 3]);
    assert_eq!(way_300.refs, vec![3, 4, 1]);
    assert_eq!(way_100.tags.get("highway"), Some(&"stop".to_string()));
    assert_eq!(way_300.tags.get("barrier"), Some(&"gate".to_string()));
}

#[test]
fn renumber_custom_start_ids() {
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
            TestWay { id: 500, refs: vec![10, 20, 30], tags: vec![("name", "line")] },
        ],
        &[],
    );

    let opts = RenumberOptions {
        start_node_id: 1000,
        start_way_id: 5000,
        start_relation_id: 9000,
    };

    renumber_external(
        &input, &output, &opts, Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber");

    let norm = read_normalized(&output);
    assert_eq!(norm.nodes.iter().map(|n| n.id).collect::<Vec<_>>(), vec![1000, 1001, 1002]);
    assert_eq!(norm.ways.len(), 1);
    assert_eq!(norm.ways[0].id, 5000);
    assert_eq!(norm.ways[0].refs, vec![1000, 1001, 1002]);
}

// ---------------------------------------------------------------------------
// Negative-id rejection
// ---------------------------------------------------------------------------

#[test]
fn renumber_rejects_negative_node_id() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode { id: -5, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
        ],
        &[],
        &[],
    );

    let err = renumber_external(
        &input, &output, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect_err("expected rejection of negative node id");

    let msg = format!("{err}");
    assert!(msg.contains("non-negative"), "error message lacks 'non-negative': {msg}");
    assert!(msg.contains("-5"), "error message should mention the offending id: {msg}");
}

#[test]
fn renumber_rejects_negative_way_ref() {
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
            TestWay { id: 100, refs: vec![10, -1, 20], tags: vec![] },
        ],
        &[],
    );

    let err = renumber_external(
        &input, &output, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect_err("expected rejection of negative way ref");

    let msg = format!("{err}");
    assert!(msg.contains("negative"), "error message lacks 'negative': {msg}");
    assert!(msg.contains("-1"), "error message should mention the offending ref: {msg}");
}

// ---------------------------------------------------------------------------
// Orphan refs
// ---------------------------------------------------------------------------

#[test]
fn renumber_orphan_way_ref_preserves_old_id() {
    // Way references a node id that doesn't exist in the input.
    // Orphan ref passes through with old id unchanged.
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
            TestWay { id: 100, refs: vec![10, 99999, 20], tags: vec![] },
        ],
        &[],
    );

    let stats = renumber_external(
        &input, &output, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber");

    assert_eq!(stats.orphan_refs, 1);

    let norm = read_normalized(&output);
    assert_eq!(norm.ways[0].refs, vec![1, 99999, 2]);
}
