//! End-to-end tests for the planet-safe external renumber implementation.
//!
//! These tests exercise `pbfhogg::renumber_external::renumber_external`,
//! which lives alongside the in-memory `renumber` module. Every test
//! cross-checks the external path against the in-memory path via
//! `assert_elements_equivalent`.

mod common;

use common::{
    assert_elements_equivalent, assert_sorted_file, read_normalized, write_test_pbf_sorted,
    TestMember, TestNode, TestRelation, TestWay,
};
use pbfhogg::renumber::{renumber, RenumberOptions};
use pbfhogg::renumber_external::renumber_external;
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
// Pass 1: nodes only
// ---------------------------------------------------------------------------

#[test]
fn external_pass1_renumbers_nodes_sequentially() {
    // Nodes-only input: verifies pass 1 wire-format rewriter assigns
    // sequential ids and preserves coords + tags.
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
        &input,
        &output,
        &default_opts(),
        Compression::default(),
        false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber_external");

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
fn external_pass1_respects_custom_start_id() {
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
    .expect("renumber_external");

    assert_eq!(stats.nodes_written, 2);
    let norm = read_normalized(&output);
    assert_eq!(norm.nodes.len(), 2);
    assert_eq!(norm.nodes[0].id, 5000);
    assert_eq!(norm.nodes[1].id, 5001);
}

// ---------------------------------------------------------------------------
// Relations: R1 + R2 end-to-end
// ---------------------------------------------------------------------------

#[test]
fn external_preserves_sorted_header_and_type_order() {
    // Dedicated sortedness check: an input with every element type
    // must produce an output that passes `assert_sorted_file`. Failure
    // modes this catches:
    // - Missing sorted header flag on the output
    // - Out-of-order ids within a type
    // - Types interleaved across blobs (nodes after ways, etc.)
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
    .expect("renumber_external");

    assert_sorted_file(&output);
}

#[test]
fn external_relations_basic_end_to_end() {
    // Input with all four member types + tag preservation + node/way/
    // relation remap. Cross-checked against the in-memory path.
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output_ext = dir.path().join("output_ext.osm.pbf");
    let output_inmem = dir.path().join("output_inmem.osm.pbf");

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
                    // Relation 600 references relation 500 (backward ref).
                    TestMember { id: MemberId::Relation(500), role: "subarea" },
                    TestMember { id: MemberId::Node(20), role: "label" },
                ],
                tags: vec![("type", "boundary")],
            },
        ],
    );

    let stats_ext = renumber_external(
        &input, &output_ext, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber_external");
    let stats_inmem = renumber(
        &input, &output_inmem, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber (in-memory)");

    assert_eq!(stats_ext.nodes_written, 2);
    assert_eq!(stats_ext.ways_written, 1);
    assert_eq!(stats_ext.relations_written, 2);
    assert_eq!(stats_inmem.relations_written, 2);

    // The external and in-memory paths must produce element-equivalent
    // output even though their block layouts, string tables, and dense
    // encoding may differ byte-wise.
    assert_elements_equivalent(&output_ext, &output_inmem);

    let norm = read_normalized(&output_ext);
    assert_eq!(norm.relations.len(), 2);

    // Relation 500 → new id 1, relation 600 → new id 2.
    let rel_1 = &norm.relations.iter().find(|r| r.id == 1).expect("rel 500→1");
    let rel_2 = &norm.relations.iter().find(|r| r.id == 2).expect("rel 600→2");

    // Rel 500's members: Node(10→1), Way(100→1)
    assert_eq!(rel_1.members.len(), 2);
    assert_eq!(rel_1.members[0].member_type, "node");
    assert_eq!(rel_1.members[0].ref_id, 1);
    assert_eq!(rel_1.members[0].role, "stop");
    assert_eq!(rel_1.members[1].member_type, "way");
    assert_eq!(rel_1.members[1].ref_id, 1);
    assert_eq!(rel_1.members[1].role, "outer");

    // Rel 600's members: Relation(500→1), Node(20→2)
    assert_eq!(rel_2.members.len(), 2);
    assert_eq!(rel_2.members[0].member_type, "relation");
    assert_eq!(rel_2.members[0].ref_id, 1);
    assert_eq!(rel_2.members[1].member_type, "node");
    assert_eq!(rel_2.members[1].ref_id, 2);
}

#[test]
fn external_relation_forward_ref() {
    // Rel 500 references rel 600 (forward ref — target appears later
    // in sort order). R1 collects all IDs before R2d resolves, so this
    // works. Cross-checked against in-memory two-pass path.
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output_ext = dir.path().join("output_ext.osm.pbf");
    let output_inmem = dir.path().join("output_inmem.osm.pbf");

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
        &input, &output_ext, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber_external");
    renumber(
        &input, &output_inmem, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber (in-memory)");

    assert_elements_equivalent(&output_ext, &output_inmem);

    let norm = read_normalized(&output_ext);
    // Rel 500 → 1, rel 600 → 2. Rel 500's forward ref to rel 600 must
    // resolve to new id 2, not old id 600.
    let rel_1 = &norm.relations.iter().find(|r| r.id == 1).expect("rel 500→1");
    assert_eq!(rel_1.members[0].ref_id, 2, "forward ref: 600→2, not 600");
}

#[test]
fn external_relation_self_reference() {
    // Rel 42 → member Relation(42). R1 assigns the new id before R2d
    // reads it, so the self-reference resolves correctly.
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output_ext = dir.path().join("output_ext.osm.pbf");
    let output_inmem = dir.path().join("output_inmem.osm.pbf");

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
        &input, &output_ext, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber_external");
    renumber(
        &input, &output_inmem, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber (in-memory)");

    assert_elements_equivalent(&output_ext, &output_inmem);

    let norm = read_normalized(&output_ext);
    assert_eq!(norm.relations.len(), 1);
    assert_eq!(norm.relations[0].id, 1, "rel 42 → 1");
    assert_eq!(norm.relations[0].members[0].ref_id, 1, "self: 42→1");
}

#[test]
fn external_relation_mixed_member_types_interleaved() {
    // Relation with members in non-type-sorted order (node, way, node,
    // relation, way, node) to stress the interleaved types + memids
    // cursor walk in the wire-format splice rewriter.
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output_ext = dir.path().join("output_ext.osm.pbf");
    let output_inmem = dir.path().join("output_inmem.osm.pbf");

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
        &input, &output_ext, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber_external");
    renumber(
        &input, &output_inmem, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber (in-memory)");

    assert_elements_equivalent(&output_ext, &output_inmem);
}

// ---------------------------------------------------------------------------
// Nodes + ways end-to-end
// ---------------------------------------------------------------------------

#[test]
fn external_nodes_and_ways_end_to_end() {
    // Input deliberately includes:
    // - A duplicate ref (way 100 refs [1, 2, 2])
    // - A way with refs in non-sorted order (way 300 [3, 4, 1])
    // Cross-checked against in-memory path.
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output_ext = dir.path().join("output_ext.osm.pbf");
    let output_inmem = dir.path().join("output_inmem.osm.pbf");

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

    // Run both paths on the same input.
    let stats_ext = renumber_external(
        &input, &output_ext, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber_external");
    let stats_inmem = renumber(
        &input, &output_inmem, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber (in-memory)");

    assert_eq!(stats_ext.nodes_written, 4);
    assert_eq!(stats_ext.ways_written, 3);
    assert_eq!(stats_ext.relations_written, 0);
    assert_eq!(stats_inmem.nodes_written, 4);
    assert_eq!(stats_inmem.ways_written, 3);

    // Element-equivalence cross-check: the two outputs must contain
    // semantically identical elements (same ids, tags, refs, members,
    // metadata) even if their byte representations differ.
    assert_elements_equivalent(&output_ext, &output_inmem);

    // Sortedness invariant: both outputs must have the sorted header
    // flag set and emit elements in monotonic file order within each
    // type. `assert_elements_equivalent` sorts by id internally so it
    // can miss file-order regressions — this check catches them.
    assert_sorted_file(&output_ext);
    assert_sorted_file(&output_inmem);

    // Spot-check the expected remapping.
    let norm = read_normalized(&output_ext);
    assert_eq!(norm.nodes.len(), 4);
    assert_eq!(norm.ways.len(), 3);
    assert_eq!(norm.nodes[0].id, 1);
    assert_eq!(norm.nodes[3].id, 4);

    // Find each way by new id (sorted by id in normalized form). With
    // default_opts() both paths start way ids at 1, and file order is
    // 100 → 1, 200 → 2, 300 → 3.
    let way_100 = &norm.ways.iter().find(|w| w.id == 1).expect("way 100→1");
    let way_200 = &norm.ways.iter().find(|w| w.id == 2).expect("way 200→2");
    let way_300 = &norm.ways.iter().find(|w| w.id == 3).expect("way 300→3");

    // Way 100 had refs [1, 2, 2] — all remapped to new node ids
    // [1, 2, 2] (identity here since we start at 1).
    assert_eq!(way_100.refs, vec![1, 2, 2]);
    assert_eq!(way_200.refs, vec![2, 3]);
    assert_eq!(way_300.refs, vec![3, 4, 1]);
    // Tag preservation:
    assert_eq!(way_100.tags.get("highway"), Some(&"stop".to_string()));
    assert_eq!(way_300.tags.get("barrier"), Some(&"gate".to_string()));
}

#[test]
fn external_custom_start_ids_nodes_and_ways() {
    // Run with non-default start ids, element-equivalence check against
    // the in-memory path.
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output_ext = dir.path().join("output_ext.osm.pbf");
    let output_inmem = dir.path().join("output_inmem.osm.pbf");

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
        &input, &output_ext, &opts, Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber_external");
    renumber(
        &input, &output_inmem, &opts, Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber (in-memory)");

    assert_elements_equivalent(&output_ext, &output_inmem);

    let norm = read_normalized(&output_ext);
    // Nodes start at 1000 → [1000, 1001, 1002].
    assert_eq!(norm.nodes.iter().map(|n| n.id).collect::<Vec<_>>(), vec![1000, 1001, 1002]);
    // Way 500 → 5000 with refs remapped to [1000, 1001, 1002].
    assert_eq!(norm.ways.len(), 1);
    assert_eq!(norm.ways[0].id, 5000);
    assert_eq!(norm.ways[0].refs, vec![1000, 1001, 1002]);
}

// ---------------------------------------------------------------------------
// Negative-id rejection (design doc section 5)
// ---------------------------------------------------------------------------

#[test]
fn external_rejects_negative_node_id() {
    // External mode rejects negative input ids with a clear error
    // pointing users at --mode inmem. In-memory renumber continues to
    // handle them transparently for users with JOSM-local staging data.
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
    assert!(msg.contains("inmem"), "error message should suggest --mode inmem: {msg}");
}

#[test]
fn external_rejects_negative_way_ref() {
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

#[test]
fn external_orphan_way_ref_preserves_old_id() {
    // If a way references a node id that doesn't exist in the input,
    // the in-memory path writes the old id through (unwrap_or(r)).
    // The external path must match so the two produce element-
    // equivalent output.
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output_ext = dir.path().join("output_ext.osm.pbf");
    let output_inmem = dir.path().join("output_inmem.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode { id: 10, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 20, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[
            // Way 100 refs node 99999 which doesn't exist.
            TestWay { id: 100, refs: vec![10, 99999, 20], tags: vec![] },
        ],
        &[],
    );

    renumber_external(
        &input, &output_ext, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber_external");
    renumber(
        &input, &output_inmem, &default_opts(), Compression::default(), false,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("renumber (in-memory)");

    assert_elements_equivalent(&output_ext, &output_inmem);

    let norm = read_normalized(&output_ext);
    // Orphan ref 99999 survives as 99999 in the output.
    assert_eq!(norm.ways[0].refs, vec![1, 99999, 2]);
}
