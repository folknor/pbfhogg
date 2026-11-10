//! getparents correctness tests.

use crate::common::{
    TestMember, TestNode, TestRelation, TestWay, node_ids_id_only as node_ids,
    read_all_elements_id_only as read_all_elements, relation_ids_id_only as relation_ids,
    way_ids_id_only as way_ids, write_test_pbf,
};
use pbfhogg::MemberId;
use pbfhogg::getid::parse_ids;
use pbfhogg::getparents::{GetparentsOptions, getparents};
use pbfhogg::write::WriteOptions;

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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
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
                tags: vec![("highway", "primary")],
                meta: None,
            },
            TestWay {
                id: 11,
                refs: vec![2, 3],
                tags: vec![("highway", "secondary")],
                meta: None,
            },
            TestWay {
                id: 12,
                refs: vec![3],
                tags: vec![],
                meta: None,
            },
        ],
        &[],
    );

    // Find ways referencing node 2
    let id_set = parse_ids(&ids(&["n2"])).expect("parse ids");
    let stats = getparents(
        &input,
        crate::common::out(&output),
        &id_set,
        &default_opts(),
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getparents");
    let c = read_all_elements(&output);

    // Ways 10 and 11 reference node 2
    assert_eq!(way_ids(&c), vec![10, 11]);
    assert!(node_ids(&c).is_empty(), "no nodes without --add-self");
    assert_eq!(stats.ways_written, 2);
    assert_eq!(stats.nodes_written, 0);
}

#[test]
fn relations_referencing_way() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[],
        &[
            TestWay {
                id: 10,
                refs: vec![1, 2],
                tags: vec![],
                meta: None,
            },
            TestWay {
                id: 11,
                refs: vec![2, 3],
                tags: vec![],
                meta: None,
            },
        ],
        &[
            TestRelation {
                id: 100,
                members: vec![TestMember {
                    id: MemberId::Way(10),
                    role: "outer",
                }],
                tags: vec![("type", "multipolygon")],
                meta: None,
            },
            TestRelation {
                id: 101,
                members: vec![TestMember {
                    id: MemberId::Way(11),
                    role: "inner",
                }],
                tags: vec![("type", "multipolygon")],
                meta: None,
            },
        ],
    );

    // Find relations referencing way 10
    let id_set = parse_ids(&ids(&["w10"])).expect("parse ids");
    let stats = getparents(
        &input,
        crate::common::out(&output),
        &id_set,
        &default_opts(),
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getparents");
    let c = read_all_elements(&output);

    assert_eq!(relation_ids(&c), vec![100]);
    assert!(way_ids(&c).is_empty());
    assert_eq!(stats.relations_written, 1);
}

#[test]
fn add_self_includes_queried_objects() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("name", "test")],
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

    // Find parents of node 1 WITH --add-self
    let id_set = parse_ids(&ids(&["n1"])).expect("parse ids");
    let opts = GetparentsOptions { add_self: true };
    let stats = getparents(
        &input,
        crate::common::out(&output),
        &id_set,
        &opts,
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getparents");
    let c = read_all_elements(&output);

    // Node 1 itself + way 10 (references node 1)
    assert_eq!(node_ids(&c), vec![1]);
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(stats.nodes_written, 1);
    assert_eq!(stats.ways_written, 1);
}

#[test]
fn no_transitive_relations() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[TestWay {
            id: 10,
            refs: vec![1],
            tags: vec![],
            meta: None,
        }],
        &[TestRelation {
            id: 100,
            members: vec![TestMember {
                id: MemberId::Way(10),
                role: "outer",
            }],
            tags: vec![("type", "multipolygon")],
            meta: None,
        }],
    );

    // Find parents of node 1: should find way 10 but NOT relation 100
    // (relation references way, not node directly)
    let id_set = parse_ids(&ids(&["n1"])).expect("parse ids");
    let stats = getparents(
        &input,
        crate::common::out(&output),
        &id_set,
        &default_opts(),
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getparents");
    let c = read_all_elements(&output);

    assert_eq!(way_ids(&c), vec![10]);
    assert!(relation_ids(&c).is_empty(), "no transitive relations");
    assert_eq!(stats.ways_written, 1);
    assert_eq!(stats.relations_written, 0);
}

#[test]
fn empty_result() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![],
            meta: None,
        }],
        &[TestWay {
            id: 10,
            refs: vec![1],
            tags: vec![],
            meta: None,
        }],
        &[],
    );

    // Node 999 doesn't exist - no parents found
    let id_set = parse_ids(&ids(&["n999"])).expect("parse ids");
    let stats = getparents(
        &input,
        crate::common::out(&output),
        &id_set,
        &default_opts(),
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getparents");

    assert_eq!(stats.nodes_written, 0);
    assert_eq!(stats.ways_written, 0);
    assert_eq!(stats.relations_written, 0);
}

// ---------------------------------------------------------------------------
// Kind-claim refusal
// ---------------------------------------------------------------------------

/// Frame ordinal of the way blob: header, nodes, ways, relations.
const WAY_BLOB: usize = 2;

/// A sorted, indexed file with one blob per kind: nodes 1 and 2, way 10
/// over them, relation 30 over the way.
fn write_claim_fixture(path: &std::path::Path) {
    let node = |id| TestNode {
        id,
        lat: 100_000_000,
        lon: 100_000_000,
        tags: vec![],
        meta: None,
    };
    crate::common::write_test_pbf_sorted(
        path,
        &[node(1), node(2)],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![],
            meta: None,
        }],
        &[TestRelation {
            id: 30,
            members: vec![TestMember {
                id: MemberId::Way(10),
                role: "",
            }],
            tags: vec![],
            meta: None,
        }],
    );
}

fn run_getparents(input: &std::path::Path, output: &std::path::Path) -> pbfhogg::Result<()> {
    let id_set = parse_ids(&ids(&["n1"]))?;
    getparents(
        input,
        crate::common::out(output),
        &id_set,
        &default_opts(),
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .map(|_| ())
}

/// A way blob claiming relations is scheduled for a node query (any
/// relation may reference the node) and refused after decoding. Before
/// the refusal the way holding the parent of node 1 was dropped from the
/// output with exit 0.
#[test]
fn getparents_refuses_a_way_blob_claiming_relations() {
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    write_claim_fixture(&honest);

    // The honest file finds way 10.
    let ok_output = dir.path().join("ok.osm.pbf");
    run_getparents(&honest, &ok_output).expect("honest claims run");
    assert_eq!(way_ids(&read_all_elements(&ok_output)), vec![10]);

    let bytes = std::fs::read(&honest).expect("read the fixture");
    let lying =
        crate::common::adversarial::mutate_blob_header_indexdata(&bytes, WAY_BLOB, |index| {
            assert_eq!(index[1], 1, "frame {WAY_BLOB} is the way blob");
            index[1] = 2;
        });
    let forged = dir.path().join("forged.osm.pbf");
    std::fs::write(&forged, lying).expect("write the forged file");

    let output = dir.path().join("out.osm.pbf");
    let err = run_getparents(&forged, &output).expect_err("a lying claim must be refused");
    assert!(
        matches!(err.kind(), pbfhogg::ErrorKind::IndexdataMismatch { .. }),
        "{err:?}"
    );
    assert_eq!(
        err.kind_contradiction(),
        Some(pbfhogg::KindContradiction {
            claimed: pbfhogg::ElementKind::Relation,
            found: pbfhogg::ElementKind::Way,
        })
    );
    assert_eq!(err.blob_index(), Some(WAY_BLOB as u64));
    assert!(!output.exists(), "nothing is published on a refusal");
}
