//! End-to-end tests for renumber.

use std::path::Path;

use crate::common::adversarial::{
    mutate_blob_header_indexdata, mutate_blob_payload, set_relation_memids_terminator_continuation,
};
use crate::common::{
    TestMember, TestNode, TestRelation, TestWay, assert_sorted_file, generate_nodes,
    read_normalized, write_multi_block_test_pbf, write_test_pbf_sorted,
};
use pbfhogg::MemberId;
use pbfhogg::renumber::{RenumberOptions, RenumberStats, renumber_external};
use pbfhogg::write::WriteOptions;

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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode {
                id: 100,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("name", "a")],
                meta: None,
            },
            TestNode {
                id: 200,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 300,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![("name", "c")],
                meta: None,
            },
        ],
        &[],
        &[],
    );

    let stats = renumber_external(
        &input,
        crate::common::out(&output),
        &default_opts(),
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("renumber");

    crate::common::assert_no_tagdata_all_blobs(&output);
    assert_eq!(stats.nodes_written, 3);
    assert_eq!(stats.ways_written, 0);
    assert_eq!(stats.relations_written, 0);

    let norm = read_normalized(&output);
    assert_eq!(norm.nodes.len(), 3);
    assert_eq!(norm.nodes[0].id, 1);
    assert_eq!(norm.nodes[0].decimicro_lat(), 100_000_000);
    assert_eq!(norm.nodes[0].decimicro_lon(), 200_000_000);
    assert_eq!(norm.nodes[0].tag("name"), Some("a"));
    assert_eq!(norm.nodes[1].id, 2);
    assert_eq!(norm.nodes[1].decimicro_lat(), 110_000_000);
    assert_eq!(norm.nodes[2].id, 3);
    assert_eq!(norm.nodes[2].tag("name"), Some("c"));
}

#[test]
fn renumber_custom_start_node_id() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
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

    let opts = RenumberOptions {
        start_node_id: 5000,
        start_way_id: 2000,
        start_relation_id: 3000,
    };

    let stats = renumber_external(
        &input,
        crate::common::out(&output),
        &opts,
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
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
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[
            TestWay {
                id: 10,
                refs: vec![1, 2],
                tags: vec![],
                meta: None,
            },
            TestWay {
                id: 20,
                refs: vec![2, 3],
                tags: vec![],
                meta: None,
            },
        ],
        &[TestRelation {
            id: 100,
            members: vec![
                TestMember {
                    id: MemberId::Node(1),
                    role: "a",
                },
                TestMember {
                    id: MemberId::Way(10),
                    role: "b",
                },
            ],
            tags: vec![("type", "route")],
            meta: None,
        }],
    );

    renumber_external(
        &input,
        crate::common::out(&output),
        &default_opts(),
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode {
                id: 10,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 20,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 100,
            refs: vec![10, 20],
            tags: vec![("highway", "road")],
            meta: None,
        }],
        &[
            TestRelation {
                id: 500,
                members: vec![
                    TestMember {
                        id: MemberId::Node(10),
                        role: "stop",
                    },
                    TestMember {
                        id: MemberId::Way(100),
                        role: "outer",
                    },
                ],
                tags: vec![("type", "multipolygon")],
                meta: None,
            },
            TestRelation {
                id: 600,
                members: vec![
                    TestMember {
                        id: MemberId::Relation(500),
                        role: "subarea",
                    },
                    TestMember {
                        id: MemberId::Node(20),
                        role: "label",
                    },
                ],
                tags: vec![("type", "boundary")],
                meta: None,
            },
        ],
    );

    let stats = renumber_external(
        &input,
        crate::common::out(&output),
        &default_opts(),
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("renumber");

    assert_eq!(stats.nodes_written, 2);
    assert_eq!(stats.ways_written, 1);
    assert_eq!(stats.relations_written, 2);

    let norm = read_normalized(&output);
    assert_eq!(norm.relations.len(), 2);

    let rel_1 = &norm
        .relations
        .iter()
        .find(|r| r.id == 1)
        .expect("rel 500→1");
    let rel_2 = &norm
        .relations
        .iter()
        .find(|r| r.id == 2)
        .expect("rel 600→2");

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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[TestNode {
            id: 10,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[],
        &[
            TestRelation {
                id: 500,
                members: vec![TestMember {
                    id: MemberId::Relation(600),
                    role: "subarea",
                }],
                tags: vec![("type", "boundary")],
                meta: None,
            },
            TestRelation {
                id: 600,
                members: vec![TestMember {
                    id: MemberId::Node(10),
                    role: "label",
                }],
                tags: vec![("type", "boundary")],
                meta: None,
            },
        ],
    );

    renumber_external(
        &input,
        crate::common::out(&output),
        &default_opts(),
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("renumber");

    let norm = read_normalized(&output);
    let rel_1 = &norm
        .relations
        .iter()
        .find(|r| r.id == 1)
        .expect("rel 500→1");
    assert_eq!(rel_1.members[0].ref_id, 2, "forward ref: 600→2, not 600");
}

#[test]
fn renumber_relation_self_reference() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[],
        &[],
        &[TestRelation {
            id: 42,
            members: vec![TestMember {
                id: MemberId::Relation(42),
                role: "self",
            }],
            tags: vec![("type", "loop")],
            meta: None,
        }],
    );

    renumber_external(
        &input,
        crate::common::out(&output),
        &default_opts(),
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("renumber");

    let norm = read_normalized(&output);
    assert_eq!(norm.relations.len(), 1);
    assert_eq!(norm.relations[0].id, 1, "rel 42 → 1");
    assert_eq!(norm.relations[0].members[0].ref_id, 1, "self: 42→1");
}

#[test]
#[allow(clippy::too_many_lines)]
fn renumber_relation_mixed_member_types_interleaved() {
    // Relation with members in non-type-sorted order (node, way, node,
    // relation, way, node) to stress the interleaved types + memids
    // cursor walk in the wire-format splice rewriter.
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
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
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[
            TestWay {
                id: 10,
                refs: vec![1, 2],
                tags: vec![],
                meta: None,
            },
            TestWay {
                id: 20,
                refs: vec![2, 3],
                tags: vec![],
                meta: None,
            },
        ],
        &[
            TestRelation {
                id: 100,
                members: vec![
                    TestMember {
                        id: MemberId::Node(1),
                        role: "a",
                    },
                    TestMember {
                        id: MemberId::Way(10),
                        role: "b",
                    },
                    TestMember {
                        id: MemberId::Node(2),
                        role: "c",
                    },
                    TestMember {
                        id: MemberId::Relation(100),
                        role: "self",
                    },
                    TestMember {
                        id: MemberId::Way(20),
                        role: "d",
                    },
                    TestMember {
                        id: MemberId::Node(3),
                        role: "e",
                    },
                ],
                tags: vec![("type", "mixed")],
                meta: None,
            },
            TestRelation {
                id: 200,
                members: vec![
                    TestMember {
                        id: MemberId::Way(20),
                        role: "only",
                    },
                    TestMember {
                        id: MemberId::Node(3),
                        role: "tail",
                    },
                ],
                tags: vec![],
                meta: None,
            },
        ],
    );

    renumber_external(
        &input,
        crate::common::out(&output),
        &default_opts(),
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("renumber");

    assert_sorted_file(&output);

    let norm = read_normalized(&output);
    assert_eq!(norm.relations.len(), 2);
    // Verify member count and types preserved.
    let rel_100 = &norm
        .relations
        .iter()
        .find(|r| r.id == 1)
        .expect("rel 100→1");
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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
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
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![("amenity", "cafe")],
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
        &[
            TestWay {
                id: 100,
                refs: vec![1, 2, 2],
                tags: vec![("highway", "stop")],
                meta: None,
            },
            TestWay {
                id: 200,
                refs: vec![2, 3],
                tags: vec![],
                meta: None,
            },
            TestWay {
                id: 300,
                refs: vec![3, 4, 1],
                tags: vec![("barrier", "gate")],
                meta: None,
            },
        ],
        &[],
    );

    let stats = renumber_external(
        &input,
        crate::common::out(&output),
        &default_opts(),
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
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
    assert_eq!(way_100.tag("highway"), Some("stop"));
    assert_eq!(way_300.tag("barrier"), Some("gate"));
}

/// The builder writes a node whose metadata a dense slot cannot carry as a
/// plain `Node` group, and a way with no refs without its `refs` field.
/// Renumber took neither. Both now renumber, plain stays plain with its
/// metadata claims intact, and the output renumbers again.
#[test]
fn plain_nodes_and_zero_ref_ways_renumber_as_the_builder_writes_them() {
    use pbfhogg::block_builder::{BlockBuilder, PartialMetadata};

    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let again = dir.path().join("again.osm.pbf");

    // Version 0 with explicit zero uid and changeset: written plain.
    let plain_meta = PartialMetadata {
        version: Some(0),
        timestamp: Some(1_600_000_000),
        changeset: Some(0),
        uid: Some(0),
        user: Some(""),
        visible: None,
    };
    let mut bb = BlockBuilder::new();
    bb.add_node(10, 1, 2, [("a", "b")], None);
    bb.add_node_partial(20, 3, 4, [("c", "d")], Some(&plain_meta));
    bb.add_node(30, 5, 6, [("e", "f")], None);
    let nodes = bb.take().expect("take").expect("node block").to_vec();
    bb.add_way(100, [("highway", "x")], &[10, 20, 30], None);
    bb.add_way(200, [("empty", "yes")], &[], None);
    bb.add_way(300, [("g", "h")], &[20], None);
    let ways = bb.take().expect("take").expect("way block").to_vec();
    crate::common::frame_fixture_blocks(
        &input,
        &[nodes, ways],
        crate::common::FixtureFraming::INDEXED,
    );

    let stats = run_renumber(&input, &output, &default_opts()).expect("renumber");
    assert_eq!(stats.nodes_written, 3);
    assert_eq!(stats.ways_written, 3);
    assert_sorted_file(&output);

    let before = read_normalized(&input);
    let after = read_normalized(&output);
    let ids: Vec<i64> = after.nodes.iter().map(|n| n.id).collect();
    assert_eq!(ids, vec![1, 2, 3]);
    for (old, new) in before.nodes.iter().zip(&after.nodes) {
        assert_eq!(
            (old.nano_lat, old.nano_lon, &old.tags, &old.meta),
            (new.nano_lat, new.nano_lon, &new.tags, &new.meta),
            "node {} keeps everything but its id",
            old.id
        );
    }
    assert!(
        after.nodes[1].meta.is_some(),
        "the plain node's claims survive"
    );
    let refs: Vec<(i64, Vec<i64>)> = after.ways.iter().map(|w| (w.id, w.refs.clone())).collect();
    assert_eq!(refs, vec![(1, vec![1, 2, 3]), (2, vec![]), (3, vec![2])]);

    run_renumber(&output, &again, &default_opts()).expect("the output renumbers again");
    let twice = read_normalized(&again);
    assert_eq!(twice.nodes, after.nodes);
    assert_eq!(twice.ways, after.ways);
}

#[test]
fn renumber_custom_start_ids() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode {
                id: 10,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 20,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 30,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 500,
            refs: vec![10, 20, 30],
            tags: vec![("name", "line")],
            meta: None,
        }],
        &[],
    );

    let opts = RenumberOptions {
        start_node_id: 1000,
        start_way_id: 5000,
        start_relation_id: 9000,
    };

    renumber_external(
        &input,
        crate::common::out(&output),
        &opts,
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("renumber");

    let norm = read_normalized(&output);
    assert_eq!(
        norm.nodes.iter().map(|n| n.id).collect::<Vec<_>>(),
        vec![1000, 1001, 1002]
    );
    assert_eq!(norm.ways.len(), 1);
    assert_eq!(norm.ways[0].id, 5000);
    assert_eq!(norm.ways[0].refs, vec![1000, 1001, 1002]);
}

// ---------------------------------------------------------------------------
// Negative ids: renumbered like any other, by canonical rank
// ---------------------------------------------------------------------------

/// Mixed-sign input in canonical order (0, then negatives by magnitude,
/// then positives) is numbered by canonical rank, refs and members of both
/// signs resolve, and orphans of both signs are named outside the live
/// range: node -2 has no absent ID canonically before it, so it keeps
/// ordinal 0 (ID 0); way -11 has ten (0 and -1..=-9; -10 is live), so it
/// becomes -10, which the live ways 1 and 2 leave free; positive orphans
/// past every member keep their ID.
#[test]
fn negative_ids_renumber_by_canonical_rank() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf_sorted(
        &input,
        &[plain_node(0), plain_node(-1), plain_node(-9), plain_node(4)],
        &[plain_way(-10, vec![-9, 0, -2, 4]), plain_way(3, vec![4, 7])],
        &[relation_of(
            -3,
            vec![
                member(MemberId::Node(-1), "a"),
                member(MemberId::Way(-10), "b"),
                member(MemberId::Way(-11), "c"),
                member(MemberId::Relation(-3), "d"),
                member(MemberId::Relation(8), "e"),
            ],
        )],
    );
    let stats = run_renumber(&input, &output, &default_opts()).expect("renumber");
    assert_eq!(stats.orphan_refs, 4, "node -2, node 7, way -11, relation 8");
    assert_sorted_file(&output);

    let norm = read_normalized(&output);
    assert_eq!(
        norm.nodes.iter().map(|n| n.id).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert_eq!(norm.ways[0].id, 1);
    assert_eq!(norm.ways[0].refs, vec![3, 1, 0, 4]);
    assert_eq!(norm.ways[1].refs, vec![4, 7]);
    assert_eq!(norm.relations[0].id, 1);
    let members: Vec<(&str, i64)> = norm.relations[0]
        .members
        .iter()
        .map(|m| (m.member_type.as_str(), m.ref_id))
        .collect();
    assert_eq!(
        members,
        vec![
            ("node", 2),
            ("way", 1),
            ("way", -10),
            ("relation", 1),
            ("relation", 8),
        ]
    );
}

/// Orphans at both ends of the ID range are named without error: the old
/// shift, `start + count + old`, overflowed for a ref near `i64::MAX`.
#[test]
fn orphans_at_the_ends_of_the_id_range_are_named() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf_sorted(
        &input,
        &[plain_node(10), plain_node(20)],
        &[plain_way(100, vec![10, i64::MAX - 1, i64::MIN, 20])],
        &[relation_of(
            5,
            vec![
                member(MemberId::Relation(i64::MAX), "x"),
                member(MemberId::Relation(i64::MIN), "y"),
            ],
        )],
    );
    let stats = run_renumber(&input, &output, &default_opts()).expect("renumber");
    assert_eq!(stats.orphan_refs, 4);
    let norm = read_normalized(&output);
    assert_eq!(norm.ways[0].refs, vec![1, i64::MAX - 1, i64::MIN, 2]);
    let members: Vec<i64> = norm.relations[0].members.iter().map(|m| m.ref_id).collect();
    assert_eq!(members, vec![i64::MAX, i64::MIN]);
}

// ---------------------------------------------------------------------------
// Orphan refs
// ---------------------------------------------------------------------------

#[test]
fn renumber_orphan_way_ref_is_named_outside_the_new_range() {
    // Way references a node id that doesn't exist in the input. No live
    // node lies above it, so it keeps its id (its id plus the number of
    // live nodes above it).
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode {
                id: 10,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 20,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 100,
            refs: vec![10, 99999, 20],
            tags: vec![],
            meta: None,
        }],
        &[],
    );

    let stats = renumber_external(
        &input,
        crate::common::out(&output),
        &default_opts(),
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("renumber");

    assert_eq!(stats.orphan_refs, 1);

    let norm = read_normalized(&output);
    assert_eq!(norm.ways[0].refs, vec![1, 99_999, 2]);
}

// ---------------------------------------------------------------------------
// Tier B contract gaps from 2026-04-26 review
// ---------------------------------------------------------------------------

/// B3 - relation-member orphans. Relation members pointing at objects
/// absent from the input are named outside their kind's renumbered range,
/// mirroring the way-ref orphan behavior pinned above
/// (`renumber_orphan_way_ref_is_named_outside_the_new_range`).
///
/// Reviewer pass 2 follow-up: previously the way orphan and node
/// orphan shared id 99999, so a `contains(&99999)` check could pass
/// when only one of the two survived (or when one was rewritten to
/// a different type). Now uses distinct ids (way 99998, node 99999)
/// and asserts the full member sequence: type + ref id + role for
/// every member, preserving order.
#[test]
fn renumber_orphan_relation_members_are_named_outside_the_new_range() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[
            TestNode {
                id: 10,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 20,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 100,
            refs: vec![10, 20],
            tags: vec![],
            meta: None,
        }],
        &[TestRelation {
            id: 1000,
            members: vec![
                TestMember {
                    id: MemberId::Way(100),
                    role: "outer",
                },
                TestMember {
                    id: MemberId::Way(99998),
                    role: "outer",
                },
                TestMember {
                    id: MemberId::Node(10),
                    role: "label",
                },
                TestMember {
                    id: MemberId::Node(99999),
                    role: "label",
                },
            ],
            tags: vec![("type", "multipolygon")],
            meta: None,
        }],
    );

    let stats = renumber_external(
        &input,
        crate::common::out(&output),
        &default_opts(),
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("renumber");

    assert_eq!(
        stats.orphan_refs, 2,
        "expected exactly 2 orphan refs (way 99998 + node 99999)"
    );

    let norm = read_normalized(&output);
    assert_eq!(norm.relations.len(), 1);
    let rel = &norm.relations[0];
    // In-input refs are remapped (way 100 -> way 1, node 10 -> node
    // 1); orphans keep their member type and, with no live element of
    // their kind above them, their id.
    let member_summary: Vec<(&str, i64, &str)> = rel
        .members
        .iter()
        .map(|m| (m.member_type.as_str(), m.ref_id, m.role.as_str()))
        .collect();
    assert_eq!(
        member_summary,
        vec![
            ("way", 1, "outer"),
            ("way", 99_998, "outer"),
            ("node", 1, "label"),
            ("node", 99_999, "label"),
        ],
        "member sequence must preserve type + ref id + role for both \
         in-input remaps and orphans (DEVIATIONS contract); got {member_summary:?}",
    );
}

fn member(id: MemberId, role: &'static str) -> TestMember {
    TestMember { id, role }
}

fn relation_of(id: i64, members: Vec<TestMember>) -> TestRelation<'static> {
    TestRelation {
        id,
        members,
        tags: vec![],
        meta: None,
    }
}

/// The orphan/new-id collision itself. Every orphan's old id equals the new id
/// some *other* element of its kind receives (node 1000, way 51, relation
/// 7), which passthrough turned into a reference to the wrong element. With
/// distinct starts per kind each orphan is named outside its own kind's
/// live range (below it when fewer absent IDs precede the orphan than the
/// start, above it otherwise), refs to one orphan stay equal across a way
/// and a relation, and no orphan ref names any element in the output.
#[test]
fn renumber_orphans_whose_old_id_is_a_new_id_do_not_alias() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_sorted(
        &input,
        &[plain_node(10), plain_node(20)],
        &[
            plain_way(100, vec![10, 20]),
            plain_way(200, vec![20, 1000, 10]),
        ],
        &[
            relation_of(
                1000,
                vec![
                    member(MemberId::Node(1000), "a"),
                    member(MemberId::Way(51), "b"),
                    member(MemberId::Relation(7), "c"),
                    member(MemberId::Node(1000), "d"),
                    member(MemberId::Way(100), "e"),
                    // Forward reference: relation 2000 comes later.
                    member(MemberId::Relation(2000), "f"),
                    member(MemberId::Node(20), "g"),
                ],
            ),
            relation_of(2000, vec![member(MemberId::Relation(1000), "h")]),
        ],
    );

    // New ranges: nodes 1000..=1001, ways 50..=51, relations 7..=8. Node
    // 1000 has 998 absent positive ids before it (1..=999 less 10 and 20),
    // fewer than the start: 998. Way 51 has 51, not fewer than the start
    // 50: 51 plus the 2 live ways = 53. Relation 7 likewise: 7 + 2 = 9.
    let opts = RenumberOptions {
        start_node_id: 1000,
        start_way_id: 50,
        start_relation_id: 7,
    };
    let stats = run_renumber(&input, &output, &opts).expect("renumber");
    assert_eq!(stats.orphan_refs, 5, "node 1000 thrice, way 51, relation 7");

    let norm = read_normalized(&output);
    let node_ids: Vec<i64> = norm.nodes.iter().map(|n| n.id).collect();
    let way_ids: Vec<i64> = norm.ways.iter().map(|w| w.id).collect();
    let rel_ids: Vec<i64> = norm.relations.iter().map(|r| r.id).collect();
    assert_eq!(node_ids, vec![1000, 1001]);
    assert_eq!(way_ids, vec![50, 51]);
    assert_eq!(rel_ids, vec![7, 8]);

    assert_eq!(norm.ways[0].refs, vec![1000, 1001]);
    assert_eq!(
        norm.ways[1].refs,
        vec![1001, 998, 1000],
        "orphan node 1000 -> 998, not the renumbered node 10"
    );

    let summary = |r: &crate::common::NormalizedRelation| -> Vec<(String, i64, String)> {
        r.members
            .iter()
            .map(|m| (m.member_type.as_str().to_owned(), m.ref_id, m.role.clone()))
            .collect()
    };
    let s = |t: &str, id: i64, role: &str| (t.to_string(), id, role.to_string());
    assert_eq!(
        summary(&norm.relations[0]),
        vec![
            s("node", 998, "a"),
            s("way", 53, "b"),
            s("relation", 9, "c"),
            s("node", 998, "d"),
            s("way", 50, "e"),
            s("relation", 8, "f"),
            s("node", 1001, "g"),
        ]
    );
    assert_eq!(summary(&norm.relations[1]), vec![s("relation", 7, "h")]);

    // No orphan names an element of its kind in the output.
    assert!(!node_ids.contains(&998));
    assert!(!way_ids.contains(&53));
    assert!(!rel_ids.contains(&9));
}

// ---------------------------------------------------------------------------
// Shared helpers for the defect tests below
// ---------------------------------------------------------------------------

fn run_renumber(
    input: &Path,
    output: &Path,
    opts: &RenumberOptions,
) -> Result<RenumberStats, String> {
    renumber_external(
        input,
        crate::common::out(output),
        opts,
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .map_err(|e| e.to_string())
}

const HUNG: &str = "renumber hung on a mid-stream error instead of failing";

fn plain_node(id: i64) -> TestNode<'static> {
    TestNode {
        id,
        lat: 100_000_000,
        lon: 200_000_000,
        tags: vec![],
        meta: None,
    }
}

fn plain_way(id: i64, refs: Vec<i64>) -> TestWay<'static> {
    TestWay {
        id,
        refs,
        tags: vec![],
        meta: None,
    }
}

fn plain_relation(id: i64, way: i64) -> TestRelation<'static> {
    TestRelation {
        id,
        members: vec![TestMember {
            id: MemberId::Way(way),
            role: "outer",
        }],
        tags: vec![],
        meta: None,
    }
}

// ---------------------------------------------------------------------------
// A mid-stream error ends the run instead of hanging it
// ---------------------------------------------------------------------------

/// Blobs per kind in the many-blob fixture. Far more than the old pools'
/// 32-slot channel plus their workers, so a failure in the middle left
/// enough work behind to park every worker in `send`.
const MANY: i64 = 200;

/// One element per blob: `MANY` node blobs, then `MANY` way blobs, then
/// `MANY` relation blobs. Frame 0 is the header, so node blob `i` is frame
/// `1 + i`, way blob `i` is `1 + MANY + i`, relation blob `i` is
/// `1 + 2 * MANY + i`.
fn write_many_blob_fixture(path: &Path) {
    let nodes: Vec<TestNode> = (1..=MANY).map(plain_node).collect();
    let ways: Vec<TestWay> = (1..=MANY).map(|id| plain_way(id, vec![1, 2])).collect();
    let relations: Vec<TestRelation> = (1..=MANY).map(|id| plain_relation(id, 1)).collect();
    write_multi_block_test_pbf(path, &nodes, &ways, &relations, 1);
}

fn frame(kind_offset: i64, i: i64) -> usize {
    usize::try_from(1 + kind_offset * MANY + i).expect("frame index")
}

/// The defect these tests pin was a hang: one fail-stop bound covers the
/// whole scenario, both runs included, and ends the test binary naming the
/// test rather than detaching a hung run or meeting the harness watchdog.
fn assert_fails_promptly(mutate: impl FnOnce(&[u8]) -> Vec<u8>, expect: &str) {
    let _bound = pbfhogg_test_scratch::HangBound::arm(
        pbfhogg::TEST_HANG_BOUND,
        pbfhogg::TEST_HANG_REPORT_GRACE,
        HUNG,
    );
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_many_blob_fixture(&input);

    // Sanity: the unmutated fixture renumbers cleanly.
    let stats = run_renumber(&input, &output, &default_opts()).expect("clean fixture");
    assert_eq!(stats.nodes_written, 200);
    assert_eq!(stats.ways_written, 200);
    assert_eq!(stats.relations_written, 200);

    let pbf = std::fs::read(&input).expect("read fixture");
    std::fs::write(&input, mutate(&pbf)).expect("rewrite fixture");
    let failed_output = dir.path().join("failed.osm.pbf");
    let err = run_renumber(&input, &failed_output, &default_opts())
        .expect_err("a corrupt blob mid-stream must fail the run");
    assert!(
        err.contains(expect),
        "expected an error naming {expect:?}; got: {err}"
    );
    // A failure mid-stream publishes nothing.
    assert!(
        !failed_output.exists(),
        "a failed renumber left a file at its output path"
    );
}

fn garbage_payload(payload: &mut Vec<u8>) {
    payload.clear();
    // A varint that never terminates: no wire parser accepts it.
    payload.extend_from_slice(&[0xFF; 12]);
}

#[test]
fn a_corrupt_node_blob_mid_stream_fails_instead_of_hanging() {
    assert_fails_promptly(
        |pbf| mutate_blob_payload(pbf, frame(0, MANY / 2), garbage_payload),
        "varint",
    );
}

#[test]
fn a_corrupt_way_blob_mid_stream_fails_instead_of_hanging() {
    assert_fails_promptly(
        |pbf| mutate_blob_payload(pbf, frame(1, MANY / 2), garbage_payload),
        "varint",
    );
}

/// R1 reads only relation ids, so a broken `memids` surfaces first in the
/// parallel R2d pool - the third copy of the hanging shape.
#[test]
fn a_corrupt_relation_blob_mid_stream_fails_instead_of_hanging() {
    assert_fails_promptly(
        |pbf| set_relation_memids_terminator_continuation(pbf, frame(2, MANY / 2), 0),
        "memids",
    );
}

// ---------------------------------------------------------------------------
// Input ids must strictly increase within each kind
// ---------------------------------------------------------------------------

/// Renumber a fixture whose header claims `Sort.Type_then_ID` but whose ids
/// are not strictly increasing, and assert the refusal. Before the check,
/// such input renumbered with exit 0 and ways pointing at the wrong nodes.
fn assert_order_refused(
    nodes: &[i64],
    ways: &[(i64, Vec<i64>)],
    relations: &[i64],
    block_size: usize,
    expect: &str,
) {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let nodes: Vec<TestNode> = nodes.iter().copied().map(plain_node).collect();
    let ways: Vec<TestWay> = ways
        .iter()
        .map(|(id, refs)| plain_way(*id, refs.clone()))
        .collect();
    let relations: Vec<TestRelation> = relations.iter().map(|&id| plain_relation(id, 10)).collect();
    write_multi_block_test_pbf(&input, &nodes, &ways, &relations, block_size);

    let err = run_renumber(&input, &output, &default_opts())
        .expect_err("non-increasing ids under a sorted header must be refused");
    assert!(
        err.contains("strictly increase") && err.contains(expect),
        "expected the ordering refusal naming {expect:?}; got: {err}"
    );
}

#[test]
fn nodes_out_of_order_within_a_blob_are_refused() {
    // Old behaviour: nodes renumbered by position (1->1, 3->2, 2->3) but way
    // ref 2 resolved by rank to 2, i.e. to the node that had id 3.
    assert_order_refused(
        &[1, 3, 2],
        &[(10, vec![2, 3])],
        &[],
        100,
        "node 2 comes after a larger id",
    );
}

#[test]
fn a_duplicate_node_within_a_blob_is_refused() {
    assert_order_refused(
        &[1, 2, 2, 3],
        &[(10, vec![3])],
        &[],
        100,
        "node 2 appears twice",
    );
}

#[test]
fn nodes_out_of_order_across_blobs_are_refused() {
    assert_order_refused(
        &[1, 2, 5, 6, 3, 4],
        &[(10, vec![4])],
        &[],
        2,
        "node 3 comes after a larger id",
    );
}

#[test]
fn a_duplicate_node_across_blobs_is_refused() {
    assert_order_refused(
        &[1, 2, 2, 3],
        &[(10, vec![3])],
        &[],
        2,
        "node 2 appears twice",
    );
}

#[test]
fn ways_out_of_order_are_refused() {
    // Old behaviour: relation member way 10 resolved by rank to the way
    // that had id 20.
    assert_order_refused(
        &[1, 2],
        &[(20, vec![1]), (10, vec![2])],
        &[100],
        1,
        "way 10 comes after a larger id",
    );
}

#[test]
fn a_duplicate_relation_is_refused() {
    assert_order_refused(
        &[1],
        &[(10, vec![1])],
        &[100, 100],
        1,
        "relation 100 appears twice",
    );
}

/// Passing counterpart: many small blobs with strictly increasing, gapped
/// ids renumber with every reference landing on the right element.
#[test]
fn increasing_ids_across_many_blobs_renumber_consistently() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let node_ids: Vec<i64> = (1..=40).map(|i| i * 7).collect();
    let nodes: Vec<TestNode> = node_ids.iter().copied().map(plain_node).collect();
    let small = |v: usize| i64::try_from(v).expect("small");
    // Way `i` (id 3i) references node_ids[39 - i] and node_ids[i].
    let ways: Vec<TestWay> = (1..=12_usize)
        .map(|i| plain_way(small(i * 3), vec![node_ids[39 - i], node_ids[i]]))
        .collect();
    let relations: Vec<TestRelation> = (1..=5).map(|i| plain_relation(i * 11, 33)).collect();
    write_multi_block_test_pbf(&input, &nodes, &ways, &relations, 3);

    run_renumber(&input, &output, &default_opts()).expect("renumber");
    let norm = read_normalized(&output);
    for i in 1..=12_usize {
        // Way id 3i is the i-th way; node_ids[k] is the (k+1)-th node.
        let way = norm.ways.iter().find(|w| w.id == small(i)).expect("way");
        assert_eq!(way.refs, vec![small(40 - i), small(i + 1)], "way {i}");
    }
    // Way 33 is the 11th way.
    assert!(norm.relations.iter().all(|r| r.members[0].ref_id == 11));
    assert_sorted_file(&output);
}

// ---------------------------------------------------------------------------
// A block the rewrite grows past the payload limit
// ---------------------------------------------------------------------------

/// Data blobs in `path` holding ways.
fn way_blobs(path: &Path) -> usize {
    pbfhogg::BlobReader::from_path(path)
        .expect("open")
        .filter(|blob| {
            matches!(
                blob.as_ref().expect("blob").decode().expect("decode"),
                pbfhogg::BlobDecode::OsmData(block)
                    if block.elements().any(|e| matches!(e, pbfhogg::Element::Way(_)))
            )
        })
        .count()
}

/// One way blob of a few MiB whose refs alternate between a live node and an
/// orphan one id above it: one-byte deltas on input. From a node start of
/// 2^40 the live node is renumbered far from the orphan (named 10, below the
/// new range), every delta takes
/// six bytes, and the rewritten block is several times the 32 MiB payload
/// limit. Renumber must cut it into blocks that fit, each with an index the
/// next renumber trusts, and lose nothing.
#[test]
fn a_way_block_the_rewrite_grows_past_the_limit_is_cut() {
    const WAYS: i64 = 6;
    const REFS: usize = 1_200_000;
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let refs: Vec<i64> = (0..REFS)
        .map(|k| if k % 2 == 0 { 10 } else { 11 })
        .collect();
    let ways: Vec<TestWay> = (1..=WAYS).map(|id| plain_way(id, refs.clone())).collect();
    write_test_pbf_sorted(&input, &[plain_node(10)], &ways, &[]);
    assert_eq!(way_blobs(&input), 1, "the fixture is one way blob");

    let opts = RenumberOptions {
        start_node_id: 1 << 40,
        ..default_opts()
    };
    let write = WriteOptions {
        compression: pbfhogg::writer::Compression::None,
        ..WriteOptions::default()
    };
    let renumber = |from: &Path, to: &Path, opts: &RenumberOptions| {
        renumber_external(
            from,
            crate::common::out(to),
            opts,
            &write,
            &pbfhogg::HeaderOverrides::default(),
            &crate::common::test_host(),
        )
        .unwrap_or_else(|e| panic!("renumber: {}", e.report()))
    };
    let stats = renumber(&input, &output, &opts);
    assert_eq!(stats.orphan_refs, WAYS as u64 * (REFS as u64 / 2));
    assert!(
        way_blobs(&output) >= 2,
        "the grown block is cut into pieces"
    );
    assert_sorted_file(&output);

    let expected: Vec<i64> = (0..REFS)
        .map(|k| if k % 2 == 0 { 1 << 40 } else { 10 })
        .collect();
    let norm = read_normalized(&output);
    assert_eq!(norm.ways.len(), 6);
    for (k, way) in norm.ways.iter().enumerate() {
        assert_eq!(way.id, i64::try_from(k).expect("small") + 1);
        assert!(
            way.refs == expected,
            "way {} refs start {:?}",
            way.id,
            &way.refs[..4]
        );
    }

    // The pieces' indexes hold: renumber assigns IDs from their counts and
    // refuses a blob whose count disagrees.
    let again = dir.path().join("again.osm.pbf");
    renumber(&output, &again, &default_opts());
    let norm = read_normalized(&again);
    assert_eq!(norm.ways.len(), 6);
    assert_eq!(norm.ways[5].id, 6);
}

// ---------------------------------------------------------------------------
// Indexdata may not size the node pre-allocation
// ---------------------------------------------------------------------------

/// A node blob whose indexdata claims `max_id = i64::MAX`. Sized from the
/// claim, `IdSet::pre_allocate` asked for about a terabyte before reading a
/// single blob and the process died. The bound now comes from the file, and
/// the output must match the honest file's exactly.
#[test]
fn an_overstated_indexdata_max_id_does_not_size_the_node_set() {
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    let lying = dir.path().join("lying.osm.pbf");
    let out_honest = dir.path().join("out_honest.osm.pbf");
    let out_lying = dir.path().join("out_lying.osm.pbf");
    write_test_pbf_sorted(
        &honest,
        &generate_nodes(20, 1),
        &[plain_way(5, vec![3, 1, 20])],
        &[],
    );
    let pbf = std::fs::read(&honest).expect("read");
    // Frame 1 is the node blob. Indexdata bytes 10..18 hold max_id (LE).
    let mutated = mutate_blob_header_indexdata(&pbf, 1, |ix| {
        assert!(ix.len() >= 18, "fixture node blob must carry indexdata");
        ix[10..18].copy_from_slice(&i64::MAX.to_le_bytes());
    });
    std::fs::write(&lying, mutated).expect("write");

    run_renumber(&honest, &out_honest, &default_opts()).expect("honest");
    run_renumber(&lying, &out_lying, &default_opts())
        .expect("an overstated max_id must neither abort nor refuse");
    let (a, b) = (read_normalized(&out_honest), read_normalized(&out_lying));
    assert_eq!(a.nodes.len(), b.nodes.len());
    assert_eq!(a.ways[0].refs, b.ways[0].refs);
    assert_eq!(b.ways[0].refs, vec![3, 1, 20]);
}

/// A tiny file with node ids far above anything its size could justify
/// pre-allocating - including one past the `IdSet` direct-chunk range. Those
/// ids take the overflow path, and every reference still resolves. Refusing
/// them would refuse every small extract of real OSM data.
#[test]
fn node_ids_far_above_the_file_size_bound_still_renumber() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let (a, b, c) = (5_i64, 1_i64 << 40, 1_i64 << 50);
    write_test_pbf_sorted(
        &input,
        &[plain_node(a), plain_node(b), plain_node(c)],
        &[plain_way(9, vec![c, a, b])],
        &[TestRelation {
            id: 4,
            members: vec![TestMember {
                id: MemberId::Node(c),
                role: "x",
            }],
            tags: vec![],
            meta: None,
        }],
    );
    run_renumber(&input, &output, &default_opts()).expect("renumber");
    let norm = read_normalized(&output);
    assert_eq!(
        norm.nodes.iter().map(|n| n.id).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(norm.ways[0].refs, vec![3, 1, 2]);
    assert_eq!(norm.relations[0].members[0].ref_id, 3);
}

// ---------------------------------------------------------------------------
// Start ids: 0 means 1, a negative start counts down (as osmium does)
// ---------------------------------------------------------------------------

/// Node and way counts down from -5 and -1, relations from 0 (which means
/// 1). Refs follow the countdown, the output stays in canonical order with
/// a true sorted claim, and only a run past `i64::MIN` is refused.
#[test]
fn a_negative_start_counts_down_and_zero_means_one() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    write_test_pbf_sorted(
        &input,
        &[plain_node(1), plain_node(2), plain_node(3)],
        &[plain_way(10, vec![3, 1]), plain_way(20, vec![2])],
        &[relation_of(5, vec![member(MemberId::Way(20), "a")])],
    );
    let opts = RenumberOptions {
        start_node_id: -5,
        start_way_id: -1,
        start_relation_id: 0,
    };
    run_renumber(&input, &output, &opts).expect("renumber");
    assert_sorted_file(&output);
    let ids: Vec<(char, i64, Vec<i64>)> = crate::common::read_in_file_order(&output)
        .into_iter()
        .map(|e| match e {
            crate::common::NormalizedElement::Node(n) => ('n', n.id, vec![]),
            crate::common::NormalizedElement::Way(w) => ('w', w.id, w.refs),
            crate::common::NormalizedElement::Relation(r) => {
                ('r', r.id, r.members.iter().map(|m| m.ref_id).collect())
            }
        })
        .collect();
    assert_eq!(
        ids,
        vec![
            ('n', -5, vec![]),
            ('n', -6, vec![]),
            ('n', -7, vec![]),
            ('w', -1, vec![-7, -5]),
            ('w', -2, vec![-6]),
            ('r', 1, vec![-2]),
        ]
    );

    let at_the_end = RenumberOptions {
        start_node_id: i64::MIN + 2,
        start_way_id: 1,
        start_relation_id: 1,
    };
    run_renumber(&input, &dir.path().join("end.osm.pbf"), &at_the_end)
        .expect("the last node is i64::MIN");
    let past = RenumberOptions {
        start_node_id: i64::MIN + 1,
        ..at_the_end
    };
    let err = run_renumber(&input, &dir.path().join("past.osm.pbf"), &past)
        .expect_err("runs past i64::MIN");
    assert!(err.contains("runs past the 64-bit id range"), "got: {err}");
}
