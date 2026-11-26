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

const NODE_BLOB: usize = 1;

/// `query` with `--add-self` over `input`, every element of the output as
/// (kind, id) in file order, on each arm this build can select: the
/// dispatched arm, and (under the test-hook cfg) both forced arms.
fn add_self_on_every_arm(input: &std::path::Path, query: &[&str]) -> Vec<Vec<(char, i64)>> {
    let id_set = parse_ids(&ids(query)).expect("parse ids");
    let opts = GetparentsOptions { add_self: true };
    let dir = crate::common::test_scratch();
    #[cfg_attr(not(pbfhogg_test_hooks), allow(unused_mut))]
    let mut runs: Vec<Option<u64>> = vec![None];
    #[cfg(pbfhogg_test_hooks)]
    runs.extend([Some(1), Some(u64::MAX)]);
    let mut outputs = Vec::new();
    for (i, min_blobs) in runs.into_iter().enumerate() {
        let output = dir.path().join(format!("out-{i}.osm.pbf"));
        match min_blobs {
            None => getparents(
                input,
                crate::common::out(&output),
                &id_set,
                &opts,
                &WriteOptions::default(),
                &pbfhogg::HeaderOverrides::default(),
                &crate::common::test_host(),
            ),
            #[cfg(pbfhogg_test_hooks)]
            Some(min_blobs) => pbfhogg::getparents::getparents_with_min_blobs(
                input,
                crate::common::out(&output),
                &id_set,
                &opts,
                &WriteOptions::default(),
                &pbfhogg::HeaderOverrides::default(),
                min_blobs,
                &crate::common::test_host(),
            ),
            #[cfg(not(pbfhogg_test_hooks))]
            Some(_) => unreachable!("forced arms need the test-hook cfg"),
        }
        .expect("getparents");
        let elements = read_all_elements(&output);
        outputs.push(
            node_ids(&elements)
                .into_iter()
                .map(|id| ('n', id))
                .chain(way_ids(&elements).into_iter().map(|id| ('w', id)))
                .chain(relation_ids(&elements).into_iter().map(|id| ('r', id)))
                .collect(),
        );
    }
    outputs
}

/// `--add-self` selects node blobs, and way blobs when no node is queried,
/// by their claimed ID range, believing it (`reference/indexdata-trust.md`):
/// a blob whose claimed range misses the queried ID is not read, so a
/// forged range hides the queried element itself. That a forged range
/// changes the output is the proof the blob was skipped. A way blob is
/// still read whole when a node is queried, whatever its range.
#[test]
fn add_self_selects_node_and_way_blobs_by_claimed_range() {
    let dir = crate::common::test_scratch();
    let honest = dir.path().join("honest.osm.pbf");
    write_claim_fixture(&honest);
    let bytes = std::fs::read(&honest).expect("read the fixture");
    let forge = |blob: usize, kind: u8| {
        let lying =
            crate::common::adversarial::mutate_blob_header_indexdata(&bytes, blob, |index| {
                assert_eq!(index[1], kind, "frame {blob} holds the expected kind");
                index[2..10].copy_from_slice(&100i64.to_le_bytes());
                index[10..18].copy_from_slice(&200i64.to_le_bytes());
            });
        let path = dir.path().join(format!("forged-{blob}.osm.pbf"));
        std::fs::write(&path, lying).expect("write the forged file");
        path
    };
    let nodes_forged = forge(NODE_BLOB, 0);
    let ways_forged = forge(WAY_BLOB, 1);

    let all = |input: &std::path::Path, query: &[&str], expected: &[(char, i64)]| {
        for run in add_self_on_every_arm(input, query) {
            assert_eq!(run, expected, "{query:?} on {}", input.display());
        }
    };
    all(&honest, &["n1"], &[('n', 1), ('w', 10)]);
    all(&nodes_forged, &["n1"], &[('w', 10)]);
    all(&honest, &["w10"], &[('w', 10), ('r', 30)]);
    all(&ways_forged, &["w10"], &[('r', 30)]);
    // With a node queried every way blob is read: the forged way range
    // hides nothing.
    all(
        &ways_forged,
        &["n1", "w10"],
        &[('n', 1), ('w', 10), ('r', 30)],
    );
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
