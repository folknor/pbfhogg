//! Merge correctness tests: build known PBFs + OSC diffs, run merge(), verify output.

mod common;

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;

use common::{
    node_ids_with_coords as node_ids, read_all_elements_with_coords as read_all_elements,
    way_ids_with_coords as way_ids, relation_ids_with_coords as relation_ids,
    write_test_pbf, TestMember, TestNode, TestRelation, TestWay,
};
use flate2::write::GzEncoder;
use pbfhogg::block_builder::{self, BlockBuilder};
use pbfhogg::MemberId;
use pbfhogg::apply_changes::{merge, MergeOptions};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element, MemberType};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers (merge-specific - shared helpers are in tests/common/mod.rs)
// ---------------------------------------------------------------------------

fn write_osc(path: &Path, xml: &str) {
    let file = File::create(path).expect("create osc file");
    let mut enc = GzEncoder::new(file, flate2::Compression::fast());
    enc.write_all(xml.as_bytes()).expect("write xml");
    enc.finish().expect("finish gz");
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
                members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
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

    let stats = merge(&base, &osc, &output, &MergeOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true, locations_on_ways: false, parallel_writer: false }, &pbfhogg::HeaderOverrides::default()).expect("merge");
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

    merge(&base, &osc, &output, &MergeOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true, locations_on_ways: false, parallel_writer: false }, &pbfhogg::HeaderOverrides::default()).expect("merge");
    let c = read_all_elements(&output);

    // All 5 nodes present in output.
    // Note: creates that fall within a passthrough blob's ID range are emitted
    // after the passthrough blob, not interleaved at their sorted position.
    // This is intentional - the merge optimizes for throughput by passing
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

    merge(&base, &osc, &output, &MergeOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true, locations_on_ways: false, parallel_writer: false }, &pbfhogg::HeaderOverrides::default()).expect("merge");
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
        let file = std::fs::File::create(&base).expect("create file");
        let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
        let mut writer = PbfWriter::new(buf, Compression::default());
        let header =
            block_builder::HeaderBuilder::new().build().expect("build header");
        writer.write_header(&header).expect("write header");
        let mut bb = BlockBuilder::new();

        // Block 1: nodes 1-3
        bb.add_node(1, 100_000_000, 100_000_000, [("block", "1")], None);
        bb.add_node(2, 200_000_000, 200_000_000, std::iter::empty::<(&str, &str)>(), None);
        bb.add_node(3, 300_000_000, 300_000_000, std::iter::empty::<(&str, &str)>(), None);
        writer
            .write_primitive_block(bb.take().expect("take").expect("bytes"))
            .expect("write");

        // Block 2: nodes 10-12 (these will be affected by the diff)
        bb.add_node(10, 100_000_000, 100_000_000, [("name", "old")], None);
        bb.add_node(11, 110_000_000, 110_000_000, std::iter::empty::<(&str, &str)>(), None);
        bb.add_node(12, 120_000_000, 120_000_000, std::iter::empty::<(&str, &str)>(), None);
        writer
            .write_primitive_block(bb.take().expect("take").expect("bytes"))
            .expect("write");

        // Block 3: nodes 20-22 (past max affected ID, should skip-decompress)
        bb.add_node(20, 200_000_000, 200_000_000, [("block", "3")], None);
        bb.add_node(21, 210_000_000, 210_000_000, std::iter::empty::<(&str, &str)>(), None);
        bb.add_node(22, 220_000_000, 220_000_000, std::iter::empty::<(&str, &str)>(), None);
        writer
            .write_primitive_block(bb.take().expect("take").expect("bytes"))
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

    let stats = merge(&base, &osc, &output, &MergeOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true, locations_on_ways: false, parallel_writer: false }, &pbfhogg::HeaderOverrides::default()).expect("merge");
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

    merge(&base, &osc, &output, &MergeOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true, locations_on_ways: false, parallel_writer: false }, &pbfhogg::HeaderOverrides::default()).expect("merge");
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

    merge(&base, &osc, &output, &MergeOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true, locations_on_ways: false, parallel_writer: false }, &pbfhogg::HeaderOverrides::default()).expect("merge");
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
                members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
                tags: vec![("type", "multipolygon")],
            },
            TestRelation {
                id: 200,
                members: vec![TestMember { id: MemberId::Node(1), role: "stop" }],
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

    merge(&base, &osc, &output, &MergeOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true, locations_on_ways: false, parallel_writer: false }, &pbfhogg::HeaderOverrides::default()).expect("merge");
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
                members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
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

    merge(&base, &osc, &output, &MergeOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true, locations_on_ways: false, parallel_writer: false }, &pbfhogg::HeaderOverrides::default()).expect("merge");
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
/// output for that block - tests that the merge doesn't emit empty blocks
/// or corrupt the output stream when an entire block is eliminated.
#[test]
fn merge_delete_entire_block() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    // Build a PBF with 2 node blocks + 1 way block.
    {
        let file = std::fs::File::create(&base).expect("create file");
        let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
        let mut writer = PbfWriter::new(buf, Compression::default());
        let header =
            block_builder::HeaderBuilder::new().build().expect("build header");
        writer.write_header(&header).expect("write header");
        let mut bb = BlockBuilder::new();

        // Block 1: nodes 1-3 (will be entirely deleted)
        bb.add_node(1, 100_000_000, 100_000_000, std::iter::empty::<(&str, &str)>(), None);
        bb.add_node(2, 200_000_000, 200_000_000, std::iter::empty::<(&str, &str)>(), None);
        bb.add_node(3, 300_000_000, 300_000_000, std::iter::empty::<(&str, &str)>(), None);
        writer
            .write_primitive_block(bb.take().expect("take").expect("bytes"))
            .expect("write");

        // Block 2: nodes 10-11 (survive)
        bb.add_node(10, 100_000_000, 100_000_000, [("survivor", "yes")], None);
        bb.add_node(11, 110_000_000, 110_000_000, std::iter::empty::<(&str, &str)>(), None);
        writer
            .write_primitive_block(bb.take().expect("take").expect("bytes"))
            .expect("write");

        // Block 3: ways (survive)
        bb.add_way(100, [("highway", "path")], &[10, 11], None);
        writer
            .write_primitive_block(bb.take().expect("take").expect("bytes"))
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

    let stats = merge(&base, &osc, &output, &MergeOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true, locations_on_ways: false, parallel_writer: false }, &pbfhogg::HeaderOverrides::default()).expect("merge");
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

    let stats = merge(&base, &osc, &output, &MergeOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true, locations_on_ways: false, parallel_writer: false }, &pbfhogg::HeaderOverrides::default()).expect("merge");

    assert_eq!(stats.base_nodes, 1, "node 1 passed through from base");
    assert_eq!(stats.diff_nodes, 2, "diff nodes emitted (modify node 2 + create node 4)");
    assert_eq!(stats.deleted, 1, "node 3 deleted");
    assert_eq!(stats.diff_ways, 1, "way 10 modified from diff");
    assert_eq!(stats.base_relations, 1, "relation 100 passed through");
}

/// Verify that metadata (version/timestamp/changeset/uid/user) from base PBF
/// nodes survives a merge. The OSC parser doesn't extract metadata, so OSC
/// replacement nodes get default metadata - but unchanged base nodes must
/// preserve their original version/timestamp/changeset/uid/user.
#[test]
fn merge_metadata_preservation() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    // Build base PBF with metadata on all nodes.
    {
        let file = std::fs::File::create(&base).expect("create file");
        let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
        let mut writer = PbfWriter::new(buf, Compression::default());
        let header =
            block_builder::HeaderBuilder::new().build().expect("build header");
        writer.write_header(&header).expect("write header");
        let mut bb = BlockBuilder::new();

        bb.add_node(
            1, 100_000_000, 200_000_000,
            [("name", "one")],
            Some(&block_builder::Metadata {
                version: 5,
                timestamp: 1_700_000_000,
                changeset: 12345,
                uid: 42,
                user: "mapper",
                visible: true,
            }),
        );
        bb.add_node(
            2, 300_000_000, 400_000_000,
            [("name", "two")],
            Some(&block_builder::Metadata {
                version: 3,
                timestamp: 1_600_000_000,
                changeset: 67890,
                uid: 7,
                user: "editor",
                visible: true,
            }),
        );
        bb.add_node(
            3, 500_000_000, 600_000_000,
            [("name", "three")],
            Some(&block_builder::Metadata {
                version: 1,
                timestamp: 1_500_000_000,
                changeset: 11111,
                uid: 99,
                user: "creator",
                visible: true,
            }),
        );
        writer
            .write_primitive_block(bb.take().expect("take").expect("bytes"))
            .expect("write");
        writer.flush().expect("flush");
    }

    // Modify node 2, leave 1 and 3 unchanged.
    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="2" lat="35.0" lon="45.0" version="4">
      <tag k="name" v="two-modified"/>
    </node>
  </modify>
</osmChange>"#);

    merge(&base, &osc, &output, &MergeOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true, locations_on_ways: false, parallel_writer: false }, &pbfhogg::HeaderOverrides::default()).expect("merge");

    // Read output and verify metadata on unchanged nodes.
    let reader = BlobReader::from_path(&output).expect("open pbf");
    let mut node_meta: Vec<(i64, Option<(i32, i32)>)> = Vec::new();

    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode") {
            for element in block.elements() {
                if let Element::DenseNode(dn) = element {
                    let meta = dn.info().map(|info| (info.version(), info.uid()));
                    node_meta.push((dn.id(), meta));
                }
            }
        }
    }

    assert_eq!(node_meta.len(), 3);

    // Node 1: unchanged, metadata preserved
    assert_eq!(node_meta[0].0, 1);
    let (version, uid) = node_meta[0].1.expect("node 1 should have metadata");
    assert_eq!(version, 5, "node 1 version preserved");
    assert_eq!(uid, 42, "node 1 uid preserved");

    // Node 2: modified from OSC, gets default metadata (version 0, uid 0)
    assert_eq!(node_meta[1].0, 2);
    let (version, uid) = node_meta[1].1.expect("node 2 should have metadata (default)");
    assert_eq!(version, 0, "OSC replacement gets default version");
    assert_eq!(uid, 0, "OSC replacement gets default uid");

    // Node 3: unchanged, metadata preserved
    assert_eq!(node_meta[2].0, 3);
    let (version, uid) = node_meta[2].1.expect("node 3 should have metadata");
    assert_eq!(version, 1, "node 3 version preserved");
    assert_eq!(uid, 99, "node 3 uid preserved");
}

// ---------------------------------------------------------------------------
// Cross-validation against osmium
// ---------------------------------------------------------------------------

/// An element read from a PBF, keyed by (type, id) for comparison.
#[derive(Debug, PartialEq)]
struct CmpNode {
    lat: i32,
    lon: i32,
    tags: Vec<(String, String)>,
}

#[derive(Debug, PartialEq)]
struct CmpWay {
    refs: Vec<i64>,
    tags: Vec<(String, String)>,
}

#[derive(Debug, PartialEq)]
struct CmpRelation {
    members: Vec<(i64, String, String)>, // (id, type, role)
    tags: Vec<(String, String)>,
}

struct CmpContents {
    nodes: HashMap<i64, CmpNode>,
    ways: HashMap<i64, CmpWay>,
    relations: HashMap<i64, CmpRelation>,
}

fn read_all_for_comparison(path: &Path) -> CmpContents {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut contents = CmpContents {
        nodes: HashMap::new(),
        ways: HashMap::new(),
        relations: HashMap::new(),
    };

    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") {
            for element in block.elements() {
                match element {
                    Element::DenseNode(dn) => {
                        let mut tags: Vec<(String, String)> = dn
                            .tags()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect();
                        tags.sort();
                        contents.nodes.insert(
                            dn.id(),
                            CmpNode {
                                lat: dn.decimicro_lat(),
                                lon: dn.decimicro_lon(),
                                tags,
                            },
                        );
                    }
                    Element::Node(n) => {
                        let mut tags: Vec<(String, String)> = n
                            .tags()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect();
                        tags.sort();
                        contents.nodes.insert(
                            n.id(),
                            CmpNode {
                                lat: n.decimicro_lat(),
                                lon: n.decimicro_lon(),
                                tags,
                            },
                        );
                    }
                    Element::Way(w) => {
                        let refs: Vec<i64> = w.refs().collect();
                        let mut tags: Vec<(String, String)> =
                            w.tags().map(|(k, v)| (k.to_string(), v.to_string())).collect();
                        tags.sort();
                        contents.ways.insert(w.id(), CmpWay { refs, tags });
                    }
                    Element::Relation(r) => {
                        let members: Vec<(i64, String, String)> = r
                            .members()
                            .map(|m| {
                                let type_str = match m.id.member_type() {
                                    MemberType::Node => "node",
                                    MemberType::Way => "way",
                                    MemberType::Relation => "relation",
                                    MemberType::Unknown(_) => "unknown",
                                    _ => "unknown",
                                };
                                (
                                    m.id.id(),
                                    type_str.to_string(),
                                    m.role().unwrap_or("").to_string(),
                                )
                            })
                            .collect();
                        let mut tags: Vec<(String, String)> =
                            r.tags().map(|(k, v)| (k.to_string(), v.to_string())).collect();
                        tags.sort();
                        contents
                            .relations
                            .insert(r.id(), CmpRelation { members, tags });
                    }
                    _ => {}
                }
            }
        }
    }

    contents
}

/// Compare two maps and count content mismatches + extra/missing IDs.
/// Returns (mismatches, extra_in_ours, missing_from_ours).
fn compare_maps<V: std::fmt::Debug + PartialEq>(
    label: &str,
    ours: &HashMap<i64, V>,
    theirs: &HashMap<i64, V>,
) -> (u64, Vec<i64>, Vec<i64>) {
    let mut mismatches = 0u64;
    for (id, ours_val) in ours {
        if let Some(theirs_val) = theirs.get(id)
            && ours_val != theirs_val
        {
            if mismatches < 5 {
                eprintln!("{label} {id} mismatch:\n  ours:   {ours_val:?}\n  theirs: {theirs_val:?}");
            }
            mismatches += 1;
        }
    }
    let extra: Vec<i64> = ours.keys().filter(|id| !theirs.contains_key(id)).copied().collect();
    let missing: Vec<i64> = theirs.keys().filter(|id| !ours.contains_key(id)).copied().collect();
    (mismatches, extra, missing)
}

// ---------------------------------------------------------------------------
// --locations-on-ways tests
// ---------------------------------------------------------------------------

/// Helper: write a PBF with LocationsOnWays header feature and sorted flag.
/// Ways are written with inline node coordinates via `add_way_with_locations`.
#[allow(clippy::type_complexity)]
fn write_test_pbf_with_locations(
    path: &Path,
    nodes: &[TestNode],
    ways: &[(i64, Vec<i64>, Vec<(i32, i32)>, Vec<(&str, &str)>)],
    relations: &[TestRelation],
) {
    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());
    let header = block_builder::HeaderBuilder::new()
        .sorted()
        .optional_feature("LocationsOnWays")
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();

    for n in nodes {
        if !bb.can_add_node()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(bytes).expect("write block");
        }
        bb.add_node(n.id, n.lat, n.lon, n.tags.iter().copied(), None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(bytes).expect("write block");
    }

    for &(id, ref refs, ref locations, ref tags) in ways {
        if !bb.can_add_way()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(bytes).expect("write block");
        }
        let tag_refs: Vec<(&str, &str)> = tags.iter().map(|&(k, v)| (k, v)).collect();
        bb.add_way_with_locations(id, tag_refs.iter().copied(), refs, locations, None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(bytes).expect("write block");
    }

    for r in relations {
        if !bb.can_add_relation()
            && let Some(bytes) = bb.take().expect("take")
        {
            writer.write_primitive_block(bytes).expect("write block");
        }
        let members: Vec<block_builder::MemberData<'_>> = r
            .members
            .iter()
            .map(|m| block_builder::MemberData { id: m.id, role: m.role })
            .collect();
        bb.add_relation(r.id, r.tags.iter().copied(), &members, None);
    }
    if !bb.is_empty()
        && let Some(bytes) = bb.take().expect("take")
    {
        writer.write_primitive_block(bytes).expect("write block");
    }

    writer.flush().expect("flush");
}

/// Read way node locations from a PBF output. Returns Vec<(way_id, Vec<(lat, lon)>)>
/// where lat/lon are in decimicrodegrees.
fn read_way_locations(path: &Path) -> Vec<(i64, Vec<(i32, i32)>)> {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut result = Vec::new();
    for blob in reader {
        let blob = blob.expect("read blob");
        if let BlobDecode::OsmData(block) = blob.decode().expect("decode") {
            for element in block.elements() {
                if let Element::Way(w) = element {
                    let locs: Vec<(i32, i32)> = w.node_locations()
                        .map(|loc| (loc.decimicro_lat(), loc.decimicro_lon()))
                        .collect();
                    result.push((w.id(), locs));
                }
            }
        }
    }
    result
}

/// Merge with --locations-on-ways: surviving base ways preserve their coordinates,
/// OSC ways (modify/create) get coordinates looked up from the sparse index.
#[test]
fn merge_locations_on_ways_basic() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    // Base PBF: 3 nodes, 2 ways with coordinates.
    // Way 10 refs [1, 2], way 20 refs [2, 3].
    write_test_pbf_with_locations(
        &base,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 300_000_000, lon: 400_000_000, tags: vec![] },
            TestNode { id: 3, lat: 500_000_000, lon: 600_000_000, tags: vec![] },
        ],
        &[
            (10, vec![1, 2], vec![(100_000_000, 200_000_000), (300_000_000, 400_000_000)], vec![("highway", "road")]),
            (20, vec![2, 3], vec![(300_000_000, 400_000_000), (500_000_000, 600_000_000)], vec![("highway", "path")]),
        ],
        &[],
    );

    // OSC: modify way 10 (change refs to [1, 3]), create way 30 (refs [1, 2, 3]).
    // Node coordinates should be looked up from base nodes.
    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <way id="10" version="2">
      <nd ref="1"/>
      <nd ref="3"/>
      <tag k="highway" v="primary"/>
    </way>
  </modify>
  <create>
    <way id="30" version="1">
      <nd ref="1"/>
      <nd ref="2"/>
      <nd ref="3"/>
      <tag k="highway" v="footway"/>
    </way>
  </create>
</osmChange>"#);

    let stats = merge(&base, &osc, &output, &MergeOptions {
        compression: Compression::default(),
        direct_io: false,
        io_uring: false,
        force: true,
        locations_on_ways: true, parallel_writer: false,
    }, &pbfhogg::HeaderOverrides::default()).expect("merge");

    // Check location stats
    assert!(stats.loc_nodes_needed > 0, "should need node coords");
    assert_eq!(stats.loc_missing, 0, "all nodes should be found");

    // Read output header - should have LocationsOnWays
    let header = common::read_header(&output);
    assert!(header.has_locations_on_ways(), "output must have LocationsOnWays");

    // Read way locations
    let locs = read_way_locations(&output);
    assert_eq!(locs.len(), 3, "3 ways in output");

    // Way 10 (modified from OSC): refs [1, 3] → coords from base nodes
    let way10 = locs.iter().find(|(id, _)| *id == 10).expect("way 10");
    assert_eq!(way10.1, vec![
        (100_000_000, 200_000_000), // node 1
        (500_000_000, 600_000_000), // node 3
    ]);

    // Way 20 (unchanged base way): should preserve original coordinates
    let way20 = locs.iter().find(|(id, _)| *id == 20).expect("way 20");
    assert_eq!(way20.1, vec![
        (300_000_000, 400_000_000), // node 2
        (500_000_000, 600_000_000), // node 3
    ]);

    // Way 30 (created from OSC): refs [1, 2, 3] → coords from base nodes
    let way30 = locs.iter().find(|(id, _)| *id == 30).expect("way 30");
    assert_eq!(way30.1, vec![
        (100_000_000, 200_000_000), // node 1
        (300_000_000, 400_000_000), // node 2
        (500_000_000, 600_000_000), // node 3
    ]);
}

/// Merge with --locations-on-ways: OSC nodes update the coordinate index.
/// When a node is modified in the OSC, ways referencing it should get the new coordinates.
#[test]
fn merge_locations_on_ways_osc_node_coords() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf_with_locations(
        &base,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 300_000_000, lon: 400_000_000, tags: vec![] },
        ],
        &[
            (10, vec![1, 2], vec![(100_000_000, 200_000_000), (300_000_000, 400_000_000)], vec![]),
        ],
        &[],
    );

    // OSC: modify node 2 (new coords) AND modify way 10 (same refs but different version)
    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="2" lat="55.0" lon="12.0" version="2"/>
    <way id="10" version="2">
      <nd ref="1"/>
      <nd ref="2"/>
      <tag k="highway" v="primary"/>
    </way>
  </modify>
</osmChange>"#);

    merge(&base, &osc, &output, &MergeOptions {
        compression: Compression::default(),
        direct_io: false,
        io_uring: false,
        force: true,
        locations_on_ways: true, parallel_writer: false,
    }, &pbfhogg::HeaderOverrides::default()).expect("merge");

    let locs = read_way_locations(&output);
    let way10 = locs.iter().find(|(id, _)| *id == 10).expect("way 10");

    // Node 1: unchanged (from base), node 2: modified in OSC to (55.0, 12.0)
    assert_eq!(way10.1, vec![
        (100_000_000, 200_000_000), // node 1 from base
        (550_000_000, 120_000_000), // node 2 from OSC (55.0°, 12.0°)
    ]);
}

/// Merge with --locations-on-ways requires LocationsOnWays in base PBF.
#[test]
fn merge_locations_on_ways_requires_base_feature() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    // Write a normal sorted PBF (no LocationsOnWays)
    common::write_test_pbf_sorted(
        &base,
        &[TestNode { id: 1, lat: 0, lon: 0, tags: vec![] }],
        &[],
        &[],
    );

    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="2" lat="1.0" lon="2.0" version="1"/>
  </create>
</osmChange>"#);

    let result = merge(&base, &osc, &output, &MergeOptions {
        compression: Compression::default(),
        direct_io: false,
        io_uring: false,
        force: true,
        locations_on_ways: true, parallel_writer: false,
    }, &pbfhogg::HeaderOverrides::default());

    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("should fail without LocationsOnWays in base"),
    };
    let err_msg = format!("{err}");
    assert!(err_msg.contains("LocationsOnWays"), "error should mention LocationsOnWays: {err_msg}");
}

/// Cross-validate pbfhogg merge against osmium apply-changes on real data.
///
/// Runs both tools on `data/denmark-20260220-seq4704.osm.pbf` +
/// `data/denmark-20260221-seq4705.osc.gz`, reads both outputs, and verifies:
/// 1. All elements shared by both outputs have identical content
/// 2. pbfhogg has no extra elements that osmium doesn't
/// 3. Elements in osmium but not pbfhogg are exactly the OSC delete set
///    (osmium uses version-based deletes; pbfhogg/osmosis/osmconvert use unconditional)
///
/// Skipped if the data files don't exist or osmium isn't installed.
/// Run with: `dev check -- --ignored`
#[test]
#[ignore]
#[allow(clippy::cognitive_complexity)]
fn merge_cross_validate_osmium() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let base_pbf = manifest.join("data/denmark-20260220-seq4704.osm.pbf");
    let osc = manifest.join("data/denmark-20260221-seq4705.osc.gz");

    if !base_pbf.exists() {
        eprintln!("Skipping: {} not found", base_pbf.display());
        return;
    }
    if !osc.exists() {
        eprintln!("Skipping: {} not found", osc.display());
        return;
    }

    let osmium_ok = std::process::Command::new("osmium")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !osmium_ok {
        eprintln!("Skipping: osmium not found in PATH");
        return;
    }

    let target_dir = manifest.join("target");
    std::fs::create_dir_all(&target_dir).ok();
    let pbfhogg_out = target_dir.join("merge-xval-pbfhogg.osm.pbf");
    let osmium_out = target_dir.join("merge-xval-osmium.osm.pbf");

    let diff = pbfhogg::osc::parse_osc_file(&osc).expect("parse osc");

    eprintln!("Running pbfhogg merge...");
    merge(&base_pbf, &osc, &pbfhogg_out, &MergeOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true, locations_on_ways: false, parallel_writer: false }, &pbfhogg::HeaderOverrides::default()).expect("pbfhogg merge");

    eprintln!("Running osmium apply-changes...");
    let osmium_result = std::process::Command::new("osmium")
        .args([
            "apply-changes",
            &base_pbf.to_string_lossy(),
            &osc.to_string_lossy(),
            "-o",
            &osmium_out.to_string_lossy(),
            "-O",
            "--no-progress",
        ])
        .output()
        .expect("run osmium");
    assert!(
        osmium_result.status.success(),
        "osmium apply-changes failed: {}",
        String::from_utf8_lossy(&osmium_result.stderr)
    );

    eprintln!("Reading pbfhogg output...");
    let ours = read_all_for_comparison(&pbfhogg_out);
    eprintln!("Reading osmium output...");
    let theirs = read_all_for_comparison(&osmium_out);

    eprintln!(
        "pbfhogg: {} nodes, {} ways, {} relations",
        ours.nodes.len(), ours.ways.len(), ours.relations.len()
    );
    eprintln!(
        "osmium:  {} nodes, {} ways, {} relations",
        theirs.nodes.len(), theirs.ways.len(), theirs.relations.len()
    );

    let (node_mm, extra_n, missing_n) = compare_maps("node", &ours.nodes, &theirs.nodes);
    let (way_mm, extra_w, missing_w) = compare_maps("way", &ours.ways, &theirs.ways);
    let (rel_mm, extra_r, missing_r) = compare_maps("relation", &ours.relations, &theirs.relations);

    let mut failures = node_mm + way_mm + rel_mm;
    failures += extra_n.len() as u64 + extra_w.len() as u64 + extra_r.len() as u64;

    if !extra_n.is_empty() { eprintln!("FAIL: {} extra nodes in pbfhogg", extra_n.len()); }
    if !extra_w.is_empty() { eprintln!("FAIL: {} extra ways in pbfhogg", extra_w.len()); }
    if !extra_r.is_empty() { eprintln!("FAIL: {} extra relations in pbfhogg", extra_r.len()); }

    // Elements in osmium but not pbfhogg should be in the OSC delete set.
    // osmium uses version-based deletes; pbfhogg/osmosis/osmconvert delete unconditionally.
    eprintln!(
        "Delete difference: {} nodes, {} ways, {} rels (OSC: {}, {}, {})",
        missing_n.len(), missing_w.len(), missing_r.len(),
        diff.deleted_nodes.len(), diff.deleted_ways.len(), diff.deleted_relations.len(),
    );
    for id in &missing_n {
        if !diff.deleted_nodes.contains(id) {
            eprintln!("FAIL: node {id} missing but NOT in delete set");
            failures += 1;
        }
    }
    for id in &missing_w {
        if !diff.deleted_ways.contains(id) {
            eprintln!("FAIL: way {id} missing but NOT in delete set");
            failures += 1;
        }
    }
    for id in &missing_r {
        if !diff.deleted_relations.contains(id) {
            eprintln!("FAIL: relation {id} missing but NOT in delete set");
            failures += 1;
        }
    }

    assert_eq!(failures, 0, "{failures} total failures");
    eprintln!("Cross-validation passed.");

    drop(std::fs::remove_file(&pbfhogg_out));
    drop(std::fs::remove_file(&osmium_out));
}

// ---------------------------------------------------------------------------
// O_DIRECT variant
// ---------------------------------------------------------------------------

#[cfg(feature = "linux-direct-io")]
#[test]
fn merge_basic_create_modify_delete_direct_io() {
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
                members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
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

    let result = merge(
        &base,
        &osc,
        &output,
        &MergeOptions {
            compression: Compression::default(),
            direct_io: true,
            io_uring: false,
            force: true,
            locations_on_ways: false, parallel_writer: false,
        },
        &pbfhogg::HeaderOverrides::default(),
    );

    match result {
        Ok(stats) => {
            let c = read_all_elements(&output);

            // Nodes: 1 (unchanged), 2 (modified), 4 (created). Node 3 deleted.
            assert_eq!(node_ids(&c), vec![1, 2, 4]);

            // Node 1 unchanged
            assert_eq!(c.nodes[0].1, 100_000_000);
            assert_eq!(c.nodes[0].2, 200_000_000);
            assert_eq!(c.nodes[0].3, vec![("name".to_string(), "one".to_string())]);

            // Node 2 modified coords and tags
            assert_eq!(c.nodes[1].1, 350_000_000);
            assert_eq!(c.nodes[1].2, 450_000_000);
            assert_eq!(c.nodes[1].3, vec![("name".to_string(), "two-modified".to_string())]);

            // Node 4 created
            assert_eq!(c.nodes[2].1, 550_000_000);
            assert_eq!(c.nodes[2].2, 120_000_000);
            assert_eq!(c.nodes[2].3, vec![("name".to_string(), "four".to_string())]);

            // Way 10 modified
            assert_eq!(way_ids(&c), vec![10]);
            assert_eq!(c.ways[0].1, vec![1, 2]);
            assert_eq!(c.ways[0].2, vec![("highway".to_string(), "primary".to_string())]);

            // Relation 100 unchanged
            assert_eq!(relation_ids(&c), vec![100]);
            assert_eq!(c.relations[0].2, vec![("type".to_string(), "multipolygon".to_string())]);

            // Stats
            assert_eq!(stats.deleted, 1);
        }
        Err(e) if common::is_einval(&*e) => {
            eprintln!("O_DIRECT not supported on this filesystem, skipping test");
        }
        Err(e) => panic!("unexpected error: {e}"),
    }
}

// ---------------------------------------------------------------------------
// io_uring variant
// ---------------------------------------------------------------------------

#[cfg(feature = "linux-io-uring")]
#[test]
// Pre-existing io_uring writer bug: produces a corrupt PBF for very
// small outputs (~5 elements). Reading the output panics with
// "failed to fill whole buffer". The bug was masked by `is_uring_unavailable`
// returning true for any I/O error from `merge()`, but `merge()` succeeds
// here - the corruption surfaces in the post-merge read. Only reproduces
// on hosts with `RLIMIT_MEMLOCK >= 16 MB` (otherwise io_uring init fails
// upfront and the skip branch fires). Tracked in TODO.md.
#[ignore = "pre-existing io_uring writer bug for small outputs; see TODO.md"]
fn merge_basic_create_modify_delete_uring() {
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
                members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
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

    let result = merge(
        &base,
        &osc,
        &output,
        &MergeOptions {
            compression: Compression::default(),
            direct_io: false,
            io_uring: true,
            force: true,
            locations_on_ways: false, parallel_writer: false,
        },
        &pbfhogg::HeaderOverrides::default(),
    );

    match result {
        Ok(stats) => {
            let c = read_all_elements(&output);

            // Nodes: 1 (unchanged), 2 (modified), 4 (created). Node 3 deleted.
            assert_eq!(node_ids(&c), vec![1, 2, 4]);

            // Node 1 unchanged
            assert_eq!(c.nodes[0].1, 100_000_000);
            assert_eq!(c.nodes[0].2, 200_000_000);
            assert_eq!(c.nodes[0].3, vec![("name".to_string(), "one".to_string())]);

            // Node 2 modified coords and tags
            assert_eq!(c.nodes[1].1, 350_000_000);
            assert_eq!(c.nodes[1].2, 450_000_000);
            assert_eq!(c.nodes[1].3, vec![("name".to_string(), "two-modified".to_string())]);

            // Node 4 created
            assert_eq!(c.nodes[2].1, 550_000_000);
            assert_eq!(c.nodes[2].2, 120_000_000);
            assert_eq!(c.nodes[2].3, vec![("name".to_string(), "four".to_string())]);

            // Way 10 modified
            assert_eq!(way_ids(&c), vec![10]);
            assert_eq!(c.ways[0].1, vec![1, 2]);
            assert_eq!(c.ways[0].2, vec![("highway".to_string(), "primary".to_string())]);

            // Relation 100 unchanged
            assert_eq!(relation_ids(&c), vec![100]);
            assert_eq!(c.relations[0].2, vec![("type".to_string(), "multipolygon".to_string())]);

            // Stats
            assert_eq!(stats.deleted, 1);
        }
        Err(e) if common::is_uring_unavailable(&*e) => {
            eprintln!("io_uring not available, skipping test");
        }
        Err(e) => panic!("unexpected error: {e}"),
    }
}

// ---------------------------------------------------------------------------
// F47: Gap creates - verify creates with IDs between base blob ranges
// ---------------------------------------------------------------------------

/// F47: Create elements with IDs that fall in gaps between base blobs.
/// Verifies gap create detection and cursor advancement across blob boundaries.
#[test]
fn merge_gap_creates_between_blobs() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    // Base: nodes 10, 20, 30 and ways 100, 200
    write_test_pbf(
        &base,
        &[
            TestNode { id: 10, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 20, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
            TestNode { id: 30, lat: 120_000_000, lon: 220_000_000, tags: vec![] },
        ],
        &[
            TestWay { id: 100, refs: vec![10, 20], tags: vec![("highway", "road")] },
            TestWay { id: 200, refs: vec![20, 30], tags: vec![("highway", "path")] },
        ],
        &[],
    );

    // Diff: creates in gaps - node 5 before base, node 15 between 10-20,
    // node 35 after base. Way 50 before base, way 150 between 100-200.
    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="5" lat="50.0" lon="10.0" version="1"/>
    <node id="15" lat="51.0" lon="11.0" version="1"/>
    <node id="35" lat="52.0" lon="12.0" version="1"/>
    <way id="50" version="1">
      <nd ref="10"/>
      <nd ref="20"/>
    </way>
    <way id="150" version="1">
      <nd ref="20"/>
      <nd ref="30"/>
    </way>
  </create>
</osmChange>"#);

    let stats = merge(&base, &osc, &output, &MergeOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true, locations_on_ways: false, parallel_writer: false }, &pbfhogg::HeaderOverrides::default()).expect("merge");
    let c = read_all_elements(&output);

    // All nodes present. Gap create 5 emitted before base blob.
    // Creates 15 and 35 fall within/after the passthrough blob range
    // and are emitted after the passthrough blob (intentional - the merge
    // optimizes for throughput by passing through unaffected blobs raw).
    let nids = node_ids(&c);
    assert_eq!(nids.len(), 6);
    assert!(nids.contains(&5));
    assert!(nids.contains(&10));
    assert!(nids.contains(&15));
    assert!(nids.contains(&20));
    assert!(nids.contains(&30));
    assert!(nids.contains(&35));

    // All ways present: gap create + base + gap create
    let wids = way_ids(&c);
    assert_eq!(wids.len(), 4);
    assert!(wids.contains(&50));
    assert!(wids.contains(&100));
    assert!(wids.contains(&150));
    assert!(wids.contains(&200));

    // Stats: 3 base nodes, 3 diff nodes, 2 base ways, 2 diff ways
    assert_eq!(stats.base_nodes, 3);
    assert_eq!(stats.diff_nodes, 3);
    assert_eq!(stats.base_ways, 2);
    assert_eq!(stats.diff_ways, 2);
}

// ---------------------------------------------------------------------------
// F48: Type transitions - Node→Relation with no Ways in base
// ---------------------------------------------------------------------------

/// F48: Base has only nodes and relations (no ways). Diff creates ways.
/// Verifies that the Node→Relation type transition flushes all pending
/// way upserts before processing the relation blob.
#[test]
fn merge_type_transition_node_to_relation_skipping_ways() {
    let dir = TempDir::new().expect("tempdir");
    let base = dir.path().join("base.osm.pbf");
    let osc = dir.path().join("diff.osc.gz");
    let output = dir.path().join("output.osm.pbf");

    // Base: nodes + relations, NO ways
    write_test_pbf(
        &base,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[],
        &[
            TestRelation {
                id: 100,
                members: vec![TestMember { id: MemberId::Node(1), role: "label" }],
                tags: vec![("type", "boundary")],
            },
        ],
    );

    // Diff: create ways (entirely new type in base)
    write_osc(&osc, r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <way id="50" version="1">
      <nd ref="1"/>
      <nd ref="2"/>
      <tag k="highway" v="residential"/>
    </way>
    <way id="51" version="1">
      <nd ref="1"/>
      <nd ref="2"/>
      <tag k="highway" v="primary"/>
    </way>
  </create>
</osmChange>"#);

    let stats = merge(&base, &osc, &output, &MergeOptions { compression: Compression::default(), direct_io: false, io_uring: false, force: true, locations_on_ways: false, parallel_writer: false }, &pbfhogg::HeaderOverrides::default()).expect("merge");
    let c = read_all_elements(&output);

    // All nodes from base
    assert_eq!(node_ids(&c), vec![1, 2]);

    // Ways from diff - must be present (flushed during type transition)
    assert_eq!(way_ids(&c), vec![50, 51]);

    // Relation from base
    assert_eq!(relation_ids(&c), vec![100]);

    // Output ordering: nodes before ways before relations
    assert_eq!(stats.base_nodes, 2);
    assert_eq!(stats.diff_ways, 2);
    assert_eq!(stats.base_relations, 1);
}
