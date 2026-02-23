//! Tags-filter correctness tests.

use std::path::Path;

use pbfhogg::block_builder::{self, BlockBuilder, MemberData, MemberType};
use pbfhogg::tags_filter::tags_filter;
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers (same pattern as tests/merge.rs)
// ---------------------------------------------------------------------------

struct TestNode {
    id: i64,
    lat: i32,
    lon: i32,
    tags: Vec<(&'static str, &'static str)>,
}

struct TestWay {
    id: i64,
    refs: Vec<i64>,
    tags: Vec<(&'static str, &'static str)>,
}

struct TestRelation {
    id: i64,
    members: Vec<TestMember>,
    tags: Vec<(&'static str, &'static str)>,
}

struct TestMember {
    id: i64,
    member_type: MemberType,
    role: &'static str,
}

fn write_test_pbf(path: &Path, nodes: &[TestNode], ways: &[TestWay], relations: &[TestRelation]) {
    let mut writer = PbfWriter::to_path(path, Compression::default()).expect("create writer");
    let header = block_builder::build_header(None, None, None, None).expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    for n in nodes {
        if !bb.can_add_node() {
            if let Some(bytes) = bb.take().expect("take") {
                writer.write_primitive_block(&bytes).expect("write block");
            }
        }
        bb.add_node(n.id, n.lat, n.lon, &n.tags, None);
    }
    if !bb.is_empty() {
        if let Some(bytes) = bb.take().expect("take") {
            writer.write_primitive_block(&bytes).expect("write block");
        }
    }

    for w in ways {
        if !bb.can_add_way() {
            if let Some(bytes) = bb.take().expect("take") {
                writer.write_primitive_block(&bytes).expect("write block");
            }
        }
        bb.add_way(w.id, &w.tags, &w.refs, None);
    }
    if !bb.is_empty() {
        if let Some(bytes) = bb.take().expect("take") {
            writer.write_primitive_block(&bytes).expect("write block");
        }
    }

    for r in relations {
        if !bb.can_add_relation() {
            if let Some(bytes) = bb.take().expect("take") {
                writer.write_primitive_block(&bytes).expect("write block");
            }
        }
        let members: Vec<MemberData<'_>> = r
            .members
            .iter()
            .map(|m| MemberData {
                member_id: m.id,
                member_type: m.member_type,
                role: m.role,
            })
            .collect();
        bb.add_relation(r.id, &r.tags, &members, None);
    }
    if !bb.is_empty() {
        if let Some(bytes) = bb.take().expect("take") {
            writer.write_primitive_block(&bytes).expect("write block");
        }
    }

    writer.flush().expect("flush");
}

#[derive(Debug)]
struct PbfContents {
    nodes: Vec<(i64, Vec<(String, String)>)>,
    ways: Vec<(i64, Vec<i64>, Vec<(String, String)>)>,
    relations: Vec<(i64, Vec<(String, String)>)>,
}

fn read_all_elements(path: &Path) -> PbfContents {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut contents = PbfContents {
        nodes: Vec::new(),
        ways: Vec::new(),
        relations: Vec::new(),
    };

    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            for element in block.elements() {
                match element {
                    Element::DenseNode(dn) => {
                        let tags: Vec<(String, String)> = dn
                            .tags()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect();
                        contents.nodes.push((dn.id(), tags));
                    }
                    Element::Node(n) => {
                        let tags: Vec<(String, String)> = n
                            .tags()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect();
                        contents.nodes.push((n.id(), tags));
                    }
                    Element::Way(w) => {
                        let tags: Vec<(String, String)> =
                            w.tags().map(|(k, v)| (k.to_string(), v.to_string())).collect();
                        let refs: Vec<i64> = w.refs().collect();
                        contents.ways.push((w.id(), refs, tags));
                    }
                    Element::Relation(r) => {
                        let tags: Vec<(String, String)> =
                            r.tags().map(|(k, v)| (k.to_string(), v.to_string())).collect();
                        contents.relations.push((r.id(), tags));
                    }
                }
            }
        }
    }

    contents
}

fn node_ids(c: &PbfContents) -> Vec<i64> {
    c.nodes.iter().map(|(id, _)| *id).collect()
}

fn way_ids(c: &PbfContents) -> Vec<i64> {
    c.ways.iter().map(|(id, _, _)| *id).collect()
}

fn relation_ids(c: &PbfContents) -> Vec<i64> {
    c.relations.iter().map(|(id, _)| *id).collect()
}

fn exprs(strs: &[&str]) -> Vec<String> {
    strs.iter().map(|s| s.to_string()).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn key_only_filter() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("amenity", "bench")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "foo")] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![("amenity", "restaurant"), ("name", "bar")] },
        ],
        &[],
        &[],
    );

    let stats = tags_filter(&input, &output, &exprs(&["amenity"]), true).expect("filter");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 3]);
    assert_eq!(stats.nodes_matched, 2);
}

#[test]
fn exact_value_filter() {
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
            TestWay { id: 12, refs: vec![1, 3], tags: vec![("name", "road")] },
        ],
        &[],
    );

    let stats = tags_filter(&input, &output, &exprs(&["highway=primary"]), true).expect("filter");
    let c = read_all_elements(&output);

    assert_eq!(way_ids(&c), vec![10]);
    assert!(node_ids(&c).is_empty());
    assert_eq!(stats.ways_matched, 1);
}

#[test]
fn multi_value_filter() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[],
        &[],
        &[
            TestRelation { id: 1, members: vec![], tags: vec![("type", "multipolygon")] },
            TestRelation { id: 2, members: vec![], tags: vec![("type", "boundary")] },
            TestRelation { id: 3, members: vec![], tags: vec![("type", "route")] },
        ],
    );

    let stats = tags_filter(&input, &output, &exprs(&["type=multipolygon,boundary"]), true).expect("filter");
    let c = read_all_elements(&output);

    assert_eq!(relation_ids(&c), vec![1, 2]);
    assert_eq!(stats.relations_matched, 2);
}

#[test]
fn negation_filter() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[],
        &[
            TestWay { id: 10, refs: vec![], tags: vec![("highway", "primary")] },
            TestWay { id: 11, refs: vec![], tags: vec![("highway", "secondary")] },
            TestWay { id: 12, refs: vec![], tags: vec![("name", "road")] }, // no highway tag
        ],
        &[],
    );

    let stats = tags_filter(&input, &output, &exprs(&["highway!=primary"]), true).expect("filter");
    let c = read_all_elements(&output);

    // Only way 11 matches: has highway tag with value != primary
    // Way 10: highway=primary -> excluded by negation
    // Way 12: no highway tag -> no match
    assert_eq!(way_ids(&c), vec![11]);
    assert_eq!(stats.ways_matched, 1);
}

#[test]
fn wildcard_prefix_filter() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("addr:street", "Main St")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("addr:city", "Berlin")] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![("name", "foo")] },
        ],
        &[],
        &[],
    );

    let stats = tags_filter(&input, &output, &exprs(&["addr:*"]), true).expect("filter");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2]);
    assert_eq!(stats.nodes_matched, 2);
}

#[test]
fn type_prefix_filter() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("building", "yes")] },
        ],
        &[
            TestWay { id: 10, refs: vec![], tags: vec![("building", "yes")] },
        ],
        &[],
    );

    // w/ prefix — only ways
    let stats = tags_filter(&input, &output, &exprs(&["w/building=yes"]), true).expect("filter");
    let c = read_all_elements(&output);

    assert!(node_ids(&c).is_empty());
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(stats.ways_matched, 1);
    assert_eq!(stats.nodes_matched, 0);
}

#[test]
fn combined_type_prefix_nw() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("natural", "tree")] },
        ],
        &[
            TestWay { id: 10, refs: vec![], tags: vec![("natural", "tree")] },
        ],
        &[
            TestRelation { id: 100, members: vec![], tags: vec![("natural", "tree")] },
        ],
    );

    let stats = tags_filter(&input, &output, &exprs(&["nw/natural=tree"]), true).expect("filter");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1]);
    assert_eq!(way_ids(&c), vec![10]);
    assert!(relation_ids(&c).is_empty());
    assert_eq!(stats.nodes_matched, 1);
    assert_eq!(stats.ways_matched, 1);
    assert_eq!(stats.relations_matched, 0);
}

#[test]
fn two_pass_includes_way_dep_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![] }, // not referenced
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")] },
        ],
        &[],
    );

    // Default mode (include references)
    let stats = tags_filter(&input, &output, &exprs(&["highway=primary"]), false).expect("filter");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2, 3]); // referenced nodes included
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(stats.ways_matched, 1);
    assert_eq!(stats.nodes_from_ways, 3);
    assert_eq!(stats.nodes_matched, 0);
}

#[test]
fn omit_referenced_excludes_way_dep_nodes() {
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
        ],
        &[],
    );

    // -R mode (omit references)
    let stats = tags_filter(&input, &output, &exprs(&["highway=primary"]), true).expect("filter");
    let c = read_all_elements(&output);

    assert!(node_ids(&c).is_empty()); // no referenced nodes
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(stats.ways_matched, 1);
    assert_eq!(stats.nodes_from_ways, 0);
}

#[test]
fn two_pass_direct_node_match_plus_way_deps() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("amenity", "bench")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![] }, // excluded
        ],
        &[
            TestWay { id: 10, refs: vec![2, 3], tags: vec![("highway", "primary")] },
        ],
        &[],
    );

    let stats = tags_filter(
        &input,
        &output,
        &exprs(&["amenity", "highway=primary"]),
        false,
    )
    .expect("filter");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2, 3]); // 1 direct, 2+3 from way
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(stats.nodes_matched, 1);
    assert_eq!(stats.nodes_from_ways, 2);
    assert_eq!(stats.ways_matched, 1);
}

#[test]
fn empty_result_produces_valid_pbf() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "foo")] },
        ],
        &[],
        &[],
    );

    let stats = tags_filter(&input, &output, &exprs(&["nonexistent_key"]), true).expect("filter");
    let c = read_all_elements(&output);

    assert!(node_ids(&c).is_empty());
    assert!(way_ids(&c).is_empty());
    assert!(relation_ids(&c).is_empty());
    assert_eq!(stats.nodes_matched, 0);
    assert_eq!(stats.ways_matched, 0);
    assert_eq!(stats.relations_matched, 0);
}

#[test]
fn multiple_expressions_or_semantics() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("amenity", "bench")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("shop", "bakery")] },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![("name", "foo")] },
        ],
        &[],
        &[],
    );

    // Both "amenity" and "shop" — OR semantics
    let stats = tags_filter(&input, &output, &exprs(&["amenity", "shop"]), true).expect("filter");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2]);
    assert_eq!(stats.nodes_matched, 2);
}
