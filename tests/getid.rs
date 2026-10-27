//! getid / removeid correctness tests.

use crate::common::{
    TestMeta, TestNode, TestRelation, TestWay, generate_nodes, generate_ways,
    node_ids_id_only as node_ids, read_all_elements_id_only as read_all_elements,
    relation_ids_id_only as relation_ids, way_ids_id_only as way_ids, write_multi_block_test_pbf,
    write_test_pbf,
};
use pbfhogg::getid::{GetidOptions, getid, parse_ids, removeid};
use pbfhogg::write::WriteOptions;
use pbfhogg::{BlobDecode, BlobReader, Element};

/// Default compression and backend with `force` set: the fixtures here are
/// written without indexdata.
fn forced_write_options() -> WriteOptions {
    WriteOptions {
        force: true,
        ..WriteOptions::default()
    }
}

fn ids(strs: &[&str]) -> Vec<String> {
    strs.iter().map(ToString::to_string).collect()
}

fn default_opts() -> GetidOptions {
    GetidOptions {
        add_referenced: false,
        remove_tags: false,
        report_found: false,
    }
}

fn add_ref_opts() -> GetidOptions {
    GetidOptions {
        add_referenced: true,
        remove_tags: false,
        report_found: false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawNodeMeta {
    id: i64,
    version: Option<i32>,
    changeset: Option<i64>,
    uid: Option<i32>,
    user: Option<String>,
    visible: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawWayMeta {
    id: i64,
    version: Option<i32>,
    changeset: Option<i64>,
    uid: Option<i32>,
    user: Option<String>,
    visible: Option<bool>,
}

fn read_raw_node_meta(path: &std::path::Path) -> Vec<RawNodeMeta> {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut metas = Vec::new();

    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            for element in block.elements() {
                match element {
                    Element::DenseNode(dn) => {
                        let info = dn.info();
                        metas.push(RawNodeMeta {
                            id: dn.id(),
                            version: info.as_ref().map(|i| i.version()),
                            changeset: info.as_ref().map(|i| i.changeset()),
                            uid: info.as_ref().map(|i| i.uid()),
                            user: info.as_ref().map(|i| i.user().unwrap_or("").to_string()),
                            visible: info.as_ref().map(|i| i.visible()),
                        });
                    }
                    Element::Node(n) => {
                        let info = n.info();
                        metas.push(RawNodeMeta {
                            id: n.id(),
                            version: info.version(),
                            changeset: info.changeset(),
                            uid: info.uid(),
                            user: info
                                .user()
                                .and_then(std::result::Result::ok)
                                .map(ToString::to_string),
                            visible: Some(info.visible()),
                        });
                    }
                    _ => {}
                }
            }
        }
    }

    metas
}

fn read_raw_way_meta(path: &std::path::Path) -> Vec<RawWayMeta> {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut metas = Vec::new();

    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            for element in block.elements() {
                if let Element::Way(w) = element {
                    let info = w.info();
                    metas.push(RawWayMeta {
                        id: w.id(),
                        version: info.version(),
                        changeset: info.changeset(),
                        uid: info.uid(),
                        user: info
                            .user()
                            .and_then(std::result::Result::ok)
                            .map(ToString::to_string),
                        visible: Some(info.visible()),
                    });
                }
            }
        }
    }

    metas
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `report_found` collects the requested IDs that were written, on the
/// write path. The set must be exactly the requested IDs present in the
/// input - not the requested set (which would hide misses) and not every
/// written element (which would report referenced nodes nobody asked
/// for). `None` when the caller did not ask, so "not requested" stays
/// distinguishable from "requested, found nothing".
#[test]
fn getid_reports_found_ids_without_rereading_output() {
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
        ],
        &[],
        &[],
    );

    // Not asked for: no set at all.
    let id_set = parse_ids(&ids(&["n1"])).expect("parse ids");
    let stats = getid(
        &input,
        crate::common::out_replace(&output),
        &id_set,
        &default_opts(),
        &forced_write_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getid");
    assert!(
        stats.found.is_none(),
        "found must be None unless report_found was set",
    );

    // Asked for: n1 exists, n99 does not.
    let id_set = parse_ids(&ids(&["n1", "n99"])).expect("parse ids");
    let opts = GetidOptions {
        add_referenced: false,
        remove_tags: false,
        report_found: true,
    };
    let stats = getid(
        &input,
        crate::common::out_replace(&output),
        &id_set,
        &opts,
        &forced_write_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getid");
    let found = stats.found.expect("report_found was set");
    assert!(found.node_ids.get(1), "n1 was written and must be reported");
    assert!(
        !found.node_ids.get(99),
        "n99 is absent from the input and must not be reported as found",
    );
}

/// With `--add-referenced` the output also carries nodes pulled in by a
/// matched way. Those were never requested, so they must not appear in
/// the found set - otherwise the "did not find" report would go silent
/// on genuinely missing IDs. This path runs `process_block` on decode
/// worker threads, so it exercises the parallel collection route.
#[test]
fn getid_found_ids_exclude_referenced_pull_ins() {
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
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![],
            meta: None,
        }],
        &[],
    );

    let id_set = parse_ids(&ids(&["w10", "w11"])).expect("parse ids");
    let opts = GetidOptions {
        add_referenced: true,
        remove_tags: false,
        report_found: true,
    };
    let stats = getid(
        &input,
        crate::common::out(&output),
        &id_set,
        &opts,
        &forced_write_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getid");
    let found = stats.found.expect("report_found was set");
    assert!(
        found.way_ids.get(10),
        "w10 was written and must be reported"
    );
    assert!(
        !found.way_ids.get(11),
        "w11 does not exist and must not be reported as found",
    );
    assert!(
        !found.node_ids.has_any(),
        "referenced nodes 1 and 2 were pulled in, not requested, and must not be reported",
    );
}

#[test]
fn getid_nodes() {
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
                tags: vec![("name", "a")],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("name", "b")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![("name", "c")],
                meta: None,
            },
        ],
        &[],
        &[],
    );

    let id_set = parse_ids(&ids(&["n1", "n3"])).expect("parse ids");
    let stats = getid(
        &input,
        crate::common::out(&output),
        &id_set,
        &default_opts(),
        &forced_write_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getid");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 3]);
    assert_eq!(stats.nodes_written, 2);
}

#[test]
fn getid_mixed_types() {
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
                refs: vec![1],
                tags: vec![],
                meta: None,
            },
        ],
        &[TestRelation {
            id: 100,
            members: vec![],
            tags: vec![("type", "route")],
            meta: None,
        }],
    );

    let id_set = parse_ids(&ids(&["n2", "w10", "r100"])).expect("parse ids");
    let stats = getid(
        &input,
        crate::common::out(&output),
        &id_set,
        &default_opts(),
        &forced_write_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getid");
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
            TestNode {
                id: 4,
                lat: 130_000_000,
                lon: 230_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2, 3],
            tags: vec![("highway", "primary")],
            meta: None,
        }],
        &[],
    );

    // Without --add-referenced: only the way
    let id_set = parse_ids(&ids(&["w10"])).expect("parse ids");
    let stats = getid(
        &input,
        crate::common::out(&output),
        &id_set,
        &default_opts(),
        &forced_write_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getid");
    let c = read_all_elements(&output);
    assert!(node_ids(&c).is_empty());
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(stats.ways_written, 1);
    assert_eq!(stats.nodes_written, 0);

    // With --add-referenced: way + its referenced nodes
    let output2 = dir.path().join("output2.osm.pbf");
    let stats = getid(
        &input,
        crate::common::out(&output2),
        &id_set,
        &add_ref_opts(),
        &forced_write_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getid");
    let c = read_all_elements(&output2);
    assert_eq!(node_ids(&c), vec![1, 2, 3]);
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(stats.nodes_written, 3);
    assert_eq!(stats.ways_written, 1);
}

#[test]
fn removeid_nodes() {
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
        &[],
        &[],
    );

    let id_set = parse_ids(&ids(&["n2"])).expect("parse ids");
    let stats = removeid(
        &input,
        crate::common::out(&output),
        &id_set,
        &forced_write_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("removeid");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 3]);
    assert_eq!(stats.nodes_written, 2);
}

#[test]
fn removeid_mixed_types() {
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
        ],
        &[
            TestWay {
                id: 10,
                refs: vec![1, 2],
                tags: vec![],
                meta: None,
            },
            TestWay {
                id: 11,
                refs: vec![1],
                tags: vec![],
                meta: None,
            },
        ],
        &[
            TestRelation {
                id: 100,
                members: vec![],
                tags: vec![],
                meta: None,
            },
            TestRelation {
                id: 101,
                members: vec![],
                tags: vec![],
                meta: None,
            },
        ],
    );

    let id_set = parse_ids(&ids(&["n1", "w11", "r100"])).expect("parse ids");
    let stats = removeid(
        &input,
        crate::common::out(&output),
        &id_set,
        &forced_write_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("removeid");
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
        &[],
        &[],
    );

    let id_set = parse_ids(&ids(&["n999"])).expect("parse ids");
    let stats = getid(
        &input,
        crate::common::out(&output),
        &id_set,
        &default_opts(),
        &forced_write_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getid");
    let c = read_all_elements(&output);

    assert!(node_ids(&c).is_empty());
    assert_eq!(stats.nodes_written, 0);
}

#[test]
fn getid_preserves_tags() {
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
                tags: vec![("name", "test"), ("amenity", "bench")],
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

    let id_set = parse_ids(&ids(&["n1"])).expect("parse ids");
    getid(
        &input,
        crate::common::out(&output),
        &id_set,
        &default_opts(),
        &forced_write_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getid");
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
fn getid_normalizes_dense_node_version_minus_one_to_absent_metadata() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "sentinel")],
            meta: Some(TestMeta {
                version: -1,
                timestamp: 0,
                changeset: -1,
                uid: 0,
                user: "",
                visible: true,
            }),
        }],
        &[],
        &[],
    );

    let id_set = parse_ids(&ids(&["n1"])).expect("parse ids");
    getid(
        &input,
        crate::common::out(&output),
        &id_set,
        &default_opts(),
        &forced_write_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getid");

    let nodes = read_raw_node_meta(&output);
    assert_eq!(nodes.len(), 1);
    assert_eq!(
        nodes[0],
        RawNodeMeta {
            id: 1,
            version: None,
            changeset: None,
            uid: None,
            user: None,
            visible: None,
        }
    );
}

#[test]
fn getid_normalizes_dense_node_changeset_minus_one_to_zero() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "sentinel")],
            meta: Some(TestMeta {
                version: 7,
                timestamp: 1_700_000_000,
                changeset: -1,
                uid: 42,
                user: "mapper",
                visible: false,
            }),
        }],
        &[],
        &[],
    );

    let id_set = parse_ids(&ids(&["n1"])).expect("parse ids");
    getid(
        &input,
        crate::common::out(&output),
        &id_set,
        &default_opts(),
        &forced_write_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getid");

    let nodes = read_raw_node_meta(&output);
    assert_eq!(nodes.len(), 1);
    assert_eq!(
        nodes[0],
        RawNodeMeta {
            id: 1,
            version: Some(7),
            changeset: Some(0),
            uid: Some(42),
            user: Some("mapper".to_string()),
            visible: Some(false),
        }
    );
}

#[test]
fn getid_normalizes_way_version_minus_one_to_absent_metadata() {
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
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "service")],
            meta: Some(TestMeta {
                version: -1,
                timestamp: 0,
                changeset: -1,
                uid: 0,
                user: "",
                visible: true,
            }),
        }],
        &[],
    );

    let id_set = parse_ids(&ids(&["w10"])).expect("parse ids");
    getid(
        &input,
        crate::common::out(&output),
        &id_set,
        &default_opts(),
        &forced_write_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getid");

    let ways = read_raw_way_meta(&output);
    assert_eq!(ways.len(), 1);
    assert_eq!(
        ways[0],
        RawWayMeta {
            id: 10,
            version: None,
            changeset: None,
            uid: None,
            user: None,
            visible: Some(true),
        }
    );
}

#[test]
fn getid_normalizes_way_changeset_minus_one_to_zero() {
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
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "service")],
            meta: Some(TestMeta {
                version: 4,
                timestamp: 1_700_000_000,
                changeset: -1,
                uid: 9,
                user: "osmosis",
                visible: false,
            }),
        }],
        &[],
    );

    let id_set = parse_ids(&ids(&["w10"])).expect("parse ids");
    getid(
        &input,
        crate::common::out(&output),
        &id_set,
        &default_opts(),
        &forced_write_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getid");

    let ways = read_raw_way_meta(&output);
    assert_eq!(ways.len(), 1);
    assert_eq!(
        ways[0],
        RawWayMeta {
            id: 10,
            version: Some(4),
            changeset: Some(0),
            uid: Some(9),
            user: Some("osmosis".to_string()),
            visible: Some(false),
        }
    );
}

#[test]
fn getid_from_id_file() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let id_file = dir.path().join("ids.txt");

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
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![],
            meta: None,
        }],
        &[],
    );

    std::fs::write(&id_file, "# comment\nn1\nw10\n\nn3\n").expect("write id file");

    let id_set = pbfhogg::getid::parse_ids_from_file(&id_file).expect("parse file");
    let stats = getid(
        &input,
        crate::common::out(&output),
        &id_set,
        &default_opts(),
        &forced_write_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getid");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 3]);
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(stats.nodes_written, 2);
    assert_eq!(stats.ways_written, 1);
}

#[test]
fn getid_add_referenced_plus_direct_node() {
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
            TestNode {
                id: 4,
                lat: 130_000_000,
                lon: 230_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![2, 3],
            tags: vec![],
            meta: None,
        }],
        &[],
    );

    // Request node 1 directly + way 10 with refs → should get nodes 1,2,3 + way 10
    let id_set = parse_ids(&ids(&["n1", "w10"])).expect("parse ids");
    let stats = getid(
        &input,
        crate::common::out(&output),
        &id_set,
        &add_ref_opts(),
        &forced_write_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getid");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2, 3]);
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(stats.nodes_written, 3);
    assert_eq!(stats.ways_written, 1);
}

#[test]
fn getid_remove_tags_strips_referenced_nodes() {
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
                tags: vec![("name", "keep")],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("name", "strip")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![("name", "strip")],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2, 3],
            tags: vec![("highway", "primary")],
            meta: None,
        }],
        &[],
    );

    // Request n1 + w10 with --add-referenced --remove-tags.
    // Node 1 is explicitly requested → keep tags.
    // Nodes 2,3 are referenced-only → strip tags.
    let id_set = parse_ids(&ids(&["n1", "w10"])).expect("parse ids");
    let opts = GetidOptions {
        add_referenced: true,
        remove_tags: true,
        report_found: false,
    };
    let stats = getid(
        &input,
        crate::common::out(&output),
        &id_set,
        &opts,
        &forced_write_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("getid");

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
    assert!(
        tags2.is_empty(),
        "referenced-only node 2 should have no tags, got: {tags2:?}"
    );

    let (id3, ref tags3) = c.nodes[2];
    assert_eq!(id3, 3);
    assert!(
        tags3.is_empty(),
        "referenced-only node 3 should have no tags, got: {tags3:?}"
    );
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
    let dir = crate::common::test_scratch();
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    // 40 nodes at block_size=10 -> 4 node blobs covering id ranges
    // 1-10, 11-20, 21-30, 31-40. Remove ids 11-20 (one blob's worth);
    // the other three blobs should take the raw-passthrough branch.
    let nodes = generate_nodes(40, 1);
    let ways = generate_ways(5, 1_000, 2, 1);
    write_multi_block_test_pbf(&input, &nodes, &ways, &[], 10);

    let id_set = parse_ids(&ids(&(11..=20)
        .map(|i| format!("n{i}"))
        .collect::<Vec<_>>()
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()))
    .expect("parse ids");
    let stats = removeid(
        &input,
        crate::common::out(&output),
        &id_set,
        &forced_write_options(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
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
    // Every surviving element here rides through on the raw-passthrough
    // branch, which never decodes the blob - so `removeid` observed none of
    // them. The counts it can report are the ones its `BlobHeader.indexdata`
    // claims, and those are reported separately rather than folded into
    // "elements written", which used to present unverified metadata as an
    // observation. The output itself is checked element-by-element above, so
    // this is only about which number the command is entitled to print.
    assert_eq!(
        (
            stats.nodes_written,
            stats.ways_written,
            stats.relations_written
        ),
        (0, 0, 0),
        "nothing was decoded on the passthrough branch, so nothing is observed"
    );
    assert_eq!(
        stats.passthrough_claimed, 35,
        "30 surviving nodes + 5 ways, per the indexdata of the blobs copied \
         through without decoding"
    );
}

// ---------------------------------------------------------------------------
// Kind-claim refusal
// ---------------------------------------------------------------------------

/// Frame ordinals of the claim fixture: header, nodes, ways, relations.
const NODE_BLOB: usize = 1;
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
            tags: vec![("highway", "primary")],
            meta: None,
        }],
        &[TestRelation {
            id: 30,
            members: vec![crate::common::TestMember {
                id: pbfhogg::MemberId::Way(10),
                role: "",
            }],
            tags: vec![],
            meta: None,
        }],
    );
}

/// The fixture with frame `blob`'s indexdata kind byte set to `kind`
/// (0 node, 1 way, 2 relation): a lie no producer writes.
fn forged_claim(dir: &std::path::Path, blob: usize, kind: u8) -> std::path::PathBuf {
    let honest = dir.join("honest.osm.pbf");
    write_claim_fixture(&honest);
    let bytes = std::fs::read(&honest).expect("read the fixture");
    let lying = crate::common::adversarial::mutate_blob_header_indexdata(&bytes, blob, |index| {
        index[1] = kind;
    });
    let path = dir.join(format!("blob-{blob}-claims-{kind}.osm.pbf"));
    std::fs::write(&path, lying).expect("write the forged file");
    path
}

fn assert_contradiction(
    err: &pbfhogg::Error,
    claimed: pbfhogg::ElementKind,
    found: pbfhogg::ElementKind,
    blob: usize,
    what: &str,
) {
    assert!(
        matches!(err.kind(), pbfhogg::ErrorKind::IndexdataMismatch { .. }),
        "{what}: {err:?}"
    );
    assert_eq!(
        err.kind_contradiction(),
        Some(pbfhogg::KindContradiction { claimed, found }),
        "{what}"
    );
    assert_eq!(err.blob_index(), Some(blob as u64), "{what}");
}

fn run_getid(
    input: &std::path::Path,
    output: &std::path::Path,
    specs: &[&str],
    opts: &GetidOptions,
) -> pbfhogg::Result<()> {
    let id_set = parse_ids(&ids(specs))?;
    getid(
        input,
        crate::common::out(output),
        &id_set,
        opts,
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .map(|_| ())
}

/// A way blob claiming nodes, decoded because a requested node ID falls in
/// its claimed range, is refused by the include walker. Before the
/// refusal the way was found by its real kind and written: a lie made
/// visible, not an answer changed.
#[test]
fn getid_include_refuses_a_way_blob_claiming_nodes() {
    let dir = crate::common::test_scratch();
    let input = forged_claim(dir.path(), WAY_BLOB, 0);
    let output = dir.path().join("out.osm.pbf");
    let err = run_getid(&input, &output, &["n10", "w10"], &default_opts())
        .expect_err("a lying claim must be refused");
    assert_contradiction(
        &err,
        pbfhogg::ElementKind::Node,
        pbfhogg::ElementKind::Way,
        WAY_BLOB,
        "include",
    );
    assert!(!output.exists(), "nothing is published on a refusal");
}

/// `--add-referenced` pass 2 (the pipelined reader) refuses the same lie.
#[test]
fn getid_add_referenced_refuses_a_way_blob_claiming_nodes() {
    let dir = crate::common::test_scratch();
    let input = forged_claim(dir.path(), WAY_BLOB, 0);
    let output = dir.path().join("out.osm.pbf");
    let err = run_getid(&input, &output, &["n10", "w10"], &add_ref_opts())
        .expect_err("a lying claim must be refused");
    assert_contradiction(
        &err,
        pbfhogg::ElementKind::Node,
        pbfhogg::ElementKind::Way,
        WAY_BLOB,
        "add-referenced pass 2",
    );
    assert!(!output.exists(), "nothing is published on a refusal");
}

/// `--add-referenced` pass 1 schedules way blobs on the claim: a node blob
/// claiming ways is decoded there and refused. Before the refusal the
/// node blob was scanned as a way blob, found no ways, and the referenced
/// nodes of way 10 were silently never collected.
#[test]
fn getid_add_referenced_pass1_refuses_a_node_blob_claiming_ways() {
    let dir = crate::common::test_scratch();
    let input = forged_claim(dir.path(), NODE_BLOB, 1);
    let output = dir.path().join("out.osm.pbf");
    let err = run_getid(&input, &output, &["w10"], &add_ref_opts())
        .expect_err("a lying claim must be refused");
    assert_contradiction(
        &err,
        pbfhogg::ElementKind::Way,
        pbfhogg::ElementKind::Node,
        NODE_BLOB,
        "add-referenced pass 1",
    );
    assert!(!output.exists(), "nothing is published on a refusal");
}

/// Invert mode (`removeid`): the way blob claiming nodes is decoded when a
/// removed node ID falls in its claimed range, and would be re-encoded
/// without regard to the way enrichment the output header carries. It is
/// refused first.
#[test]
fn removeid_refuses_a_way_blob_claiming_nodes() {
    let dir = crate::common::test_scratch();
    let input = forged_claim(dir.path(), WAY_BLOB, 0);
    let output = dir.path().join("out.osm.pbf");
    let id_set = parse_ids(&ids(&["n10"])).expect("parse ids");
    let Err(err) = removeid(
        &input,
        crate::common::out(&output),
        &id_set,
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    ) else {
        panic!("a lying claim must be refused");
    };
    assert_contradiction(
        &err,
        pbfhogg::ElementKind::Node,
        pbfhogg::ElementKind::Way,
        WAY_BLOB,
        "removeid",
    );
    assert!(!output.exists(), "nothing is published on a refusal");
}

/// The honest fixture runs every one of those requests.
#[test]
fn getid_honest_claims_are_not_refused() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("honest.osm.pbf");
    write_claim_fixture(&input);
    for (name, specs, opts) in [
        ("include", &["n10", "w10"][..], default_opts()),
        ("add-referenced", &["n10", "w10"][..], add_ref_opts()),
        ("add-referenced-pass1", &["w10"][..], add_ref_opts()),
    ] {
        let output = dir.path().join(format!("{name}.osm.pbf"));
        run_getid(&input, &output, specs, &opts).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert!(output.exists(), "{name}");
    }
    let output = dir.path().join("removeid.osm.pbf");
    let id_set = parse_ids(&ids(&["n10"])).expect("parse ids");
    removeid(
        &input,
        crate::common::out(&output),
        &id_set,
        &WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .expect("removeid");
    assert!(output.exists());
}
