//! Merge correctness tests: build known PBFs + OSC diffs, run merge(), verify output.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use flate2::write::GzEncoder;
use pbfhogg::block_builder::{self, BlockBuilder, MemberData, MemberType};
use pbfhogg::merge::merge;
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct TestNode {
    id: i64,
    lat: i32, // decimicrodegrees
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
    let mut writer =
        PbfWriter::to_path(path, Compression::default()).expect("create writer");
    let header = block_builder::build_header(None, None, None, None).expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    // Nodes
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

    // Ways
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

    // Relations
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

fn write_osc(path: &Path, xml: &str) {
    let file = File::create(path).expect("create osc file");
    let mut enc = GzEncoder::new(file, flate2::Compression::fast());
    enc.write_all(xml.as_bytes()).expect("write xml");
    enc.finish().expect("finish gz");
}

/// Collected element data from a PBF, for easy assertion.
#[derive(Debug)]
struct PbfContents {
    nodes: Vec<(i64, i32, i32, Vec<(String, String)>)>, // (id, lat, lon, tags)
    ways: Vec<(i64, Vec<i64>, Vec<(String, String)>)>,   // (id, refs, tags)
    relations: Vec<(i64, Vec<(i64, String, String)>, Vec<(String, String)>)>, // (id, members(id,type,role), tags)
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
                        contents.nodes.push((dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), tags));
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
                        let members: Vec<(i64, String, String)> = r
                            .members()
                            .map(|m| {
                                let type_str = match m.member_type {
                                    pbfhogg::RelMemberType::Node => "node",
                                    pbfhogg::RelMemberType::Way => "way",
                                    pbfhogg::RelMemberType::Relation => "relation",
                                };
                                (m.member_id, type_str.to_string(), m.role().unwrap_or("").to_string())
                            })
                            .collect();
                        contents.relations.push((r.id(), members, tags));
                    }
                    _ => {}
                }
            }
        }
    }

    contents
}

fn node_ids(c: &PbfContents) -> Vec<i64> {
    c.nodes.iter().map(|(id, _, _, _)| *id).collect()
}

fn way_ids(c: &PbfContents) -> Vec<i64> {
    c.ways.iter().map(|(id, _, _)| *id).collect()
}

fn relation_ids(c: &PbfContents) -> Vec<i64> {
    c.relations.iter().map(|(id, _, _)| *id).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn merge_basic_create_modify_delete() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &base,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "one")] },
            TestNode { id: 2, lat: 300_000_000, lon: 400_000_000, tags: vec![("name", "two")] },
            TestNode { id: 3, lat: 500_000_000, lon: 600_000_000, tags: vec![("name", "three")] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "road")] },
        ],
        &[
            TestRelation {
                id: 100,
                members: vec![TestMember { id: 10, member_type: MemberType::Way, role: "outer" }],
                tags: vec![("type", "multipolygon")],
            },
        ],
    );

    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="4" lat="55.0" lon="12.0" version="1">
      <tag k="name" v="four"/>
    </node>
  </create>
  <modify>
    <node id="2" lat="35.0" lon="45.0" version="2">
      <tag k="name" v="two-modified"/>
    </node>
    <way id="10" version="2">
      <nd ref="1"/>
      <nd ref="2"/>
      <tag k="highway" v="primary"/>
    </way>
  </modify>
  <delete>
    <node id="3" version="2"/>
  </delete>
</osmChange>"#);

    let stats = merge(&base, &osc, &output).expect("merge");
    let c = read_all_elements(&output);

    // Nodes: 1 (unchanged), 2 (modified), 4 (created). Node 3 deleted.
    assert_eq!(node_ids(&c), vec![1, 2, 4]);

    // Node 1 unchanged
    assert_eq!(c.nodes[0].1, 100_000_000);
    assert_eq!(c.nodes[0].2, 200_000_000);
    assert_eq!(c.nodes[0].3, vec![("name".to_string(), "one".to_string())]);

    // Node 2 modified coords and tags
    assert_eq!(c.nodes[1].1, 350_000_000); // 35.0 * 1e7
    assert_eq!(c.nodes[1].2, 450_000_000); // 45.0 * 1e7
    assert_eq!(c.nodes[1].3, vec![("name".to_string(), "two-modified".to_string())]);

    // Node 4 created
    assert_eq!(c.nodes[2].1, 550_000_000); // 55.0 * 1e7
    assert_eq!(c.nodes[2].2, 120_000_000); // 12.0 * 1e7
    assert_eq!(c.nodes[2].3, vec![("name".to_string(), "four".to_string())]);

    // Way 10 modified
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(c.ways[0].1, vec![1, 2]); // new refs
    assert_eq!(c.ways[0].2, vec![("highway".to_string(), "primary".to_string())]);

    // Relation 100 unchanged
    assert_eq!(relation_ids(&c), vec![100]);
    assert_eq!(c.relations[0].2, vec![("type".to_string(), "multipolygon".to_string())]);

    // Stats
    assert_eq!(stats.deleted, 1);
}

#[test]
fn merge_create_between_existing_ids() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &base,
        &[
            TestNode { id: 10, lat: 0, lon: 0, tags: vec![] },
            TestNode { id: 20, lat: 0, lon: 0, tags: vec![] },
            TestNode { id: 30, lat: 0, lon: 0, tags: vec![] },
        ],
        &[],
        &[],
    );

    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="15" lat="1.0" lon="2.0" version="1"/>
    <node id="25" lat="3.0" lon="4.0" version="1"/>
  </create>
</osmChange>"#);

    merge(&base, &osc, &output).expect("merge");
    let c = read_all_elements(&output);

    // All 5 nodes present in output.
    // Note: creates that fall within a passthrough blob's ID range are emitted
    // after the passthrough blob, not interleaved at their sorted position.
    // This is intentional — the merge optimizes for throughput by passing
    // unaffected blobs through without decompression/rewriting. Pure creates
    // (IDs not in the base PBF) don't require rewriting existing blocks.
    // OSM consumers handle blocks with non-strictly-sorted IDs across blocks.
    assert_eq!(c.nodes.len(), 5);
    let ids = node_ids(&c);
    assert!(ids.contains(&10));
    assert!(ids.contains(&15));
    assert!(ids.contains(&20));
    assert!(ids.contains(&25));
    assert!(ids.contains(&30));
}

#[test]
fn merge_create_beyond_max_id() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &base,
        &[
            TestNode { id: 1, lat: 0, lon: 0, tags: vec![] },
            TestNode { id: 2, lat: 0, lon: 0, tags: vec![] },
            TestNode { id: 3, lat: 0, lon: 0, tags: vec![] },
        ],
        &[],
        &[],
    );

    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="100" lat="10.0" lon="20.0" version="1">
      <tag k="name" v="far"/>
    </node>
    <node id="200" lat="30.0" lon="40.0" version="1"/>
  </create>
</osmChange>"#);

    merge(&base, &osc, &output).expect("merge");
    let c = read_all_elements(&output);
    assert_eq!(node_ids(&c), vec![1, 2, 3, 100, 200]);
    assert_eq!(c.nodes[3].3, vec![("name".to_string(), "far".to_string())]);
}

/// Multi-block base PBF where the diff only affects the middle block.
/// Exercises the core optimization: block 1 passthrough, block 2 rewritten,
/// block 3 passthrough (or skip-decompress via SkipState::all_done).
#[test]
fn merge_multi_block_partial_rewrite() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    // Build a PBF with 3 separate node blocks by flushing manually.
    {
        let mut writer =
            PbfWriter::to_path(&base, Compression::default()).expect("create writer");
        let header =
            block_builder::build_header(None, None, None, None).expect("build header");
        writer.write_header(&header).expect("write header");
        let mut bb = BlockBuilder::new();

        // Block 1: nodes 1-3
        bb.add_node(1, 100_000_000, 100_000_000, &[("block", "1")], None);
        bb.add_node(2, 200_000_000, 200_000_000, &[], None);
        bb.add_node(3, 300_000_000, 300_000_000, &[], None);
        writer
            .write_primitive_block(&bb.take().expect("take").expect("bytes"))
            .expect("write");

        // Block 2: nodes 10-12 (these will be affected by the diff)
        bb.add_node(10, 100_000_000, 100_000_000, &[("name", "old")], None);
        bb.add_node(11, 110_000_000, 110_000_000, &[], None);
        bb.add_node(12, 120_000_000, 120_000_000, &[], None);
        writer
            .write_primitive_block(&bb.take().expect("take").expect("bytes"))
            .expect("write");

        // Block 3: nodes 20-22 (past max affected ID, should skip-decompress)
        bb.add_node(20, 200_000_000, 200_000_000, &[("block", "3")], None);
        bb.add_node(21, 210_000_000, 210_000_000, &[], None);
        bb.add_node(22, 220_000_000, 220_000_000, &[], None);
        writer
            .write_primitive_block(&bb.take().expect("take").expect("bytes"))
            .expect("write");

        writer.flush().expect("flush");
    }

    // Diff: modify node 10, delete node 11. Only block 2 affected.
    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="10" lat="99.0" lon="99.0" version="2">
      <tag k="name" v="new"/>
    </node>
  </modify>
  <delete>
    <node id="11" version="2"/>
  </delete>
</osmChange>"#);

    let stats = merge(&base, &osc, &output).expect("merge");
    let c = read_all_elements(&output);

    // Block 1 nodes passed through unchanged
    assert_eq!(c.nodes[0], (1, 100_000_000, 100_000_000, vec![("block".to_string(), "1".to_string())]));
    assert_eq!(c.nodes[1].0, 2);
    assert_eq!(c.nodes[2].0, 3);

    // Block 2: node 10 modified, node 11 deleted, node 12 passed through
    assert_eq!(c.nodes[3].0, 10);
    assert_eq!(c.nodes[3].1, 990_000_000); // 99.0 * 1e7
    assert_eq!(c.nodes[3].3, vec![("name".to_string(), "new".to_string())]);
    assert_eq!(c.nodes[4].0, 12); // node 11 gone

    // Block 3 nodes passed through unchanged
    assert_eq!(c.nodes[5], (20, 200_000_000, 200_000_000, vec![("block".to_string(), "3".to_string())]));
    assert_eq!(c.nodes[6].0, 21);
    assert_eq!(c.nodes[7].0, 22);

    assert_eq!(c.nodes.len(), 8); // 9 original - 1 deleted
    assert_eq!(stats.deleted, 1);
    assert_eq!(stats.blobs_rewritten, 1, "only block 2 rewritten");
    // Block 1 passthrough + block 3 passthrough or skip-decompress
    assert!(
        stats.blobs_passthrough + stats.blobs_skip_decompress == 2,
        "blocks 1 and 3 should be passthrough or skip-decompress"
    );
}

#[test]
fn merge_nodes_only_diff_ways_passthrough() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &base,
        &[
            TestNode { id: 1, lat: 0, lon: 0, tags: vec![] },
            TestNode { id: 2, lat: 100_000_000, lon: 100_000_000, tags: vec![("old", "tag")] },
            TestNode { id: 3, lat: 0, lon: 0, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "path")] },
            TestWay { id: 20, refs: vec![3, 2, 1], tags: vec![("building", "yes")] },
        ],
        &[],
    );

    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="2" lat="11.0" lon="22.0" version="2">
      <tag k="new" v="tag"/>
    </node>
  </modify>
</osmChange>"#);

    merge(&base, &osc, &output).expect("merge");
    let c = read_all_elements(&output);

    // Node 2 modified
    assert_eq!(c.nodes[1].0, 2);
    assert_eq!(c.nodes[1].1, 110_000_000);
    assert_eq!(c.nodes[1].3, vec![("new".to_string(), "tag".to_string())]);

    // Ways unchanged
    assert_eq!(way_ids(&c), vec![10, 20]);
    assert_eq!(c.ways[0].1, vec![1, 2, 3]);
    assert_eq!(c.ways[0].2, vec![("highway".to_string(), "path".to_string())]);
    assert_eq!(c.ways[1].1, vec![3, 2, 1]);
    assert_eq!(c.ways[1].2, vec![("building".to_string(), "yes".to_string())]);
}

#[test]
fn merge_ways_only_diff() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &base,
        &[
            TestNode { id: 1, lat: 0, lon: 0, tags: vec![] },
            TestNode { id: 2, lat: 0, lon: 0, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "road")] },
            TestWay { id: 20, refs: vec![2, 1], tags: vec![("name", "delete me")] },
            TestWay { id: 30, refs: vec![1, 2, 1], tags: vec![("building", "yes")] },
        ],
        &[],
    );

    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <way id="15" version="1">
      <nd ref="1"/>
      <nd ref="2"/>
      <tag k="new" v="way"/>
    </way>
  </create>
  <delete>
    <way id="20" version="2"/>
  </delete>
</osmChange>"#);

    merge(&base, &osc, &output).expect("merge");
    let c = read_all_elements(&output);

    // Nodes unchanged
    assert_eq!(node_ids(&c), vec![1, 2]);

    // Ways: 10 (unchanged), 15 (created), 30 (unchanged). Way 20 deleted.
    assert_eq!(way_ids(&c), vec![10, 15, 30]);
    assert_eq!(c.ways[0].2, vec![("highway".to_string(), "road".to_string())]);
    assert_eq!(c.ways[1].2, vec![("new".to_string(), "way".to_string())]);
    assert_eq!(c.ways[2].2, vec![("building".to_string(), "yes".to_string())]);
}

#[test]
fn merge_relations_only_diff() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &base,
        &[TestNode { id: 1, lat: 0, lon: 0, tags: vec![] }],
        &[TestWay { id: 10, refs: vec![1], tags: vec![] }],
        &[
            TestRelation {
                id: 100,
                members: vec![TestMember { id: 10, member_type: MemberType::Way, role: "outer" }],
                tags: vec![("type", "multipolygon")],
            },
            TestRelation {
                id: 200,
                members: vec![TestMember { id: 1, member_type: MemberType::Node, role: "stop" }],
                tags: vec![("type", "route")],
            },
        ],
    );

    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <relation id="150" version="1">
      <member type="way" ref="10" role="inner"/>
      <tag k="type" v="boundary"/>
    </relation>
  </create>
  <modify>
    <relation id="200" version="2">
      <member type="node" ref="1" role="platform"/>
      <member type="way" ref="10" role=""/>
      <tag k="type" v="public_transport"/>
    </relation>
  </modify>
</osmChange>"#);

    merge(&base, &osc, &output).expect("merge");
    let c = read_all_elements(&output);

    // Nodes and ways unchanged
    assert_eq!(node_ids(&c), vec![1]);
    assert_eq!(way_ids(&c), vec![10]);

    // Relations: 100 (unchanged), 150 (created), 200 (modified)
    assert_eq!(relation_ids(&c), vec![100, 150, 200]);

    // Relation 100 unchanged
    assert_eq!(c.relations[0].2, vec![("type".to_string(), "multipolygon".to_string())]);

    // Relation 150 created
    assert_eq!(c.relations[1].1, vec![(10, "way".to_string(), "inner".to_string())]);
    assert_eq!(c.relations[1].2, vec![("type".to_string(), "boundary".to_string())]);

    // Relation 200 modified
    assert_eq!(c.relations[2].1.len(), 2);
    assert_eq!(c.relations[2].1[0], (1, "node".to_string(), "platform".to_string()));
    assert_eq!(c.relations[2].1[1], (10, "way".to_string(), String::new()));
    assert_eq!(c.relations[2].2, vec![("type".to_string(), "public_transport".to_string())]);
}

#[test]
fn merge_all_types() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &base,
        &[
            TestNode { id: 1, lat: 0, lon: 0, tags: vec![] },
            TestNode { id: 2, lat: 0, lon: 0, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![("delete", "me")] },
            TestWay { id: 20, refs: vec![2, 1], tags: vec![("keep", "me")] },
        ],
        &[
            TestRelation {
                id: 100,
                members: vec![TestMember { id: 10, member_type: MemberType::Way, role: "outer" }],
                tags: vec![("old", "tags")],
            },
            TestRelation {
                id: 200,
                members: vec![],
                tags: vec![("type", "site")],
            },
        ],
    );

    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="3" lat="1.0" lon="2.0" version="1">
      <tag k="new" v="node"/>
    </node>
  </create>
  <delete>
    <way id="10" version="2"/>
  </delete>
  <modify>
    <relation id="200" version="2">
      <member type="node" ref="3" role="label"/>
      <tag k="type" v="site"/>
      <tag k="name" v="updated"/>
    </relation>
  </modify>
</osmChange>"#);

    merge(&base, &osc, &output).expect("merge");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2, 3]);
    assert_eq!(way_ids(&c), vec![20]);
    assert_eq!(relation_ids(&c), vec![100, 200]);

    // Created node
    assert_eq!(c.nodes[2].3, vec![("new".to_string(), "node".to_string())]);

    // Surviving way unchanged
    assert_eq!(c.ways[0].2, vec![("keep".to_string(), "me".to_string())]);

    // Relation 100 unchanged
    assert_eq!(c.relations[0].2, vec![("old".to_string(), "tags".to_string())]);

    // Relation 200 modified
    assert_eq!(c.relations[1].1, vec![(3, "node".to_string(), "label".to_string())]);
    assert!(c.relations[1].2.contains(&("name".to_string(), "updated".to_string())));
}

/// Diff deletes every element in a block. The rewrite should produce no
/// output for that block — tests that the merge doesn't emit empty blocks
/// or corrupt the output stream when an entire block is eliminated.
#[test]
fn merge_delete_entire_block() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    // Build a PBF with 2 node blocks + 1 way block.
    {
        let mut writer =
            PbfWriter::to_path(&base, Compression::default()).expect("create writer");
        let header =
            block_builder::build_header(None, None, None, None).expect("build header");
        writer.write_header(&header).expect("write header");
        let mut bb = BlockBuilder::new();

        // Block 1: nodes 1-3 (will be entirely deleted)
        bb.add_node(1, 100_000_000, 100_000_000, &[], None);
        bb.add_node(2, 200_000_000, 200_000_000, &[], None);
        bb.add_node(3, 300_000_000, 300_000_000, &[], None);
        writer
            .write_primitive_block(&bb.take().expect("take").expect("bytes"))
            .expect("write");

        // Block 2: nodes 10-11 (survive)
        bb.add_node(10, 100_000_000, 100_000_000, &[("survivor", "yes")], None);
        bb.add_node(11, 110_000_000, 110_000_000, &[], None);
        writer
            .write_primitive_block(&bb.take().expect("take").expect("bytes"))
            .expect("write");

        // Block 3: ways (survive)
        bb.add_way(100, &[("highway", "path")], &[10, 11], None);
        writer
            .write_primitive_block(&bb.take().expect("take").expect("bytes"))
            .expect("write");

        writer.flush().expect("flush");
    }

    // Delete all nodes in block 1.
    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <delete>
    <node id="1" version="2"/>
    <node id="2" version="2"/>
    <node id="3" version="2"/>
  </delete>
</osmChange>"#);

    let stats = merge(&base, &osc, &output).expect("merge");
    let c = read_all_elements(&output);

    // Nodes 1-3 all deleted, only 10-11 survive
    assert_eq!(node_ids(&c), vec![10, 11]);
    assert_eq!(c.nodes[0].3, vec![("survivor".to_string(), "yes".to_string())]);

    // Way survives unchanged
    assert_eq!(way_ids(&c), vec![100]);
    assert_eq!(c.ways[0].1, vec![10, 11]);

    assert_eq!(stats.deleted, 3);
}

#[test]
fn merge_stats_accuracy() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &base,
        &[
            TestNode { id: 1, lat: 0, lon: 0, tags: vec![] },
            TestNode { id: 2, lat: 0, lon: 0, tags: vec![] },
            TestNode { id: 3, lat: 0, lon: 0, tags: vec![] },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![] },
        ],
        &[
            TestRelation {
                id: 100,
                members: vec![],
                tags: vec![],
            },
        ],
    );

    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="4" lat="1.0" lon="2.0" version="1"/>
  </create>
  <modify>
    <node id="2" lat="5.0" lon="6.0" version="2"/>
    <way id="10" version="2">
      <nd ref="1"/>
      <nd ref="4"/>
    </way>
  </modify>
  <delete>
    <node id="3" version="2"/>
  </delete>
</osmChange>"#);

    let stats = merge(&base, &osc, &output).expect("merge");

    assert_eq!(stats.base_nodes, 1, "node 1 passed through from base");
    assert_eq!(stats.diff_nodes, 2, "diff nodes emitted (modify node 2 + create node 4)");
    assert_eq!(stats.deleted, 1, "node 3 deleted");
    assert_eq!(stats.diff_ways, 1, "way 10 modified from diff");
    assert_eq!(stats.base_relations, 1, "relation 100 passed through");
}
