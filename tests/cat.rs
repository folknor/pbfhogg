//! Integration tests for the cat command.

mod common;

use common::{
    node_ids_with_coords as node_ids, way_ids_with_coords as way_ids,
    relation_ids_with_coords as relation_ids,
    read_all_elements_with_coords, write_test_pbf, TestNode, TestWay, TestRelation, TestMember,
};
use pbfhogg::cat::{cat, CleanAttrs};
use pbfhogg::writer::Compression;
use pbfhogg::MemberId;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn cat_passthrough_buffered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "b")] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "path")] }],
        &[TestRelation {
            id: 20,
            members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
            tags: vec![("type", "multipolygon")],
        }],
    );

    let stats = cat(
        &[input.as_path()],
        &output,
        None,
        &CleanAttrs::default(),
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("cat");

    assert!(stats.blobs_passthrough > 0, "expected passthrough blobs");

    let contents = read_all_elements_with_coords(&output);
    assert_eq!(contents.nodes.len(), 2);
    assert_eq!(contents.ways.len(), 1);
    assert_eq!(contents.relations.len(), 1);

    // Verify element data preserved
    assert_eq!(contents.nodes[0].0, 1);
    assert_eq!(contents.nodes[1].0, 2);
    assert_eq!(contents.ways[0].0, 10);
    assert_eq!(contents.relations[0].0, 20);
}

// ---------------------------------------------------------------------------
// O_DIRECT variant
// ---------------------------------------------------------------------------

#[cfg(feature = "linux-direct-io")]
#[test]
fn cat_passthrough_direct_io() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "b")] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "path")] }],
        &[TestRelation {
            id: 20,
            members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
            tags: vec![("type", "multipolygon")],
        }],
    );

    let result = cat(
        &[input.as_path()],
        &output,
        None,
        &CleanAttrs::default(),
        Compression::default(),
        true,
        true,
        &pbfhogg::HeaderOverrides::default(),
    );

    match result {
        Ok(stats) => {
            assert!(stats.blobs_passthrough > 0, "expected passthrough blobs");

            let contents = read_all_elements_with_coords(&output);
            assert_eq!(contents.nodes.len(), 2);
            assert_eq!(contents.ways.len(), 1);
            assert_eq!(contents.relations.len(), 1);

            // Verify element data preserved
            assert_eq!(contents.nodes[0].0, 1);
            assert_eq!(contents.nodes[1].0, 2);
            assert_eq!(contents.ways[0].0, 10);
            assert_eq!(contents.relations[0].0, 20);
        }
        Err(e) if common::is_einval(&*e) => {
            eprintln!("O_DIRECT not supported on this filesystem, skipping test");
            return;
        }
        Err(e) => panic!("unexpected error: {e}"),
    }
}

// ---------------------------------------------------------------------------
// F53: Type filtering
// ---------------------------------------------------------------------------

#[test]
fn cat_type_filter_nodes_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "path")] }],
        &[TestRelation {
            id: 20,
            members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
            tags: vec![("type", "multipolygon")],
        }],
    );

    let _stats = cat(
        &[input.as_path()],
        &output,
        Some("node"),
        &CleanAttrs::default(),
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("cat --type node");

    let c = read_all_elements_with_coords(&output);
    assert_eq!(node_ids(&c), vec![1, 2]);
    assert!(way_ids(&c).is_empty(), "ways should be filtered out");
    assert!(relation_ids(&c).is_empty(), "relations should be filtered out");
}

#[test]
fn cat_type_filter_ways_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![] },
        ],
        &[TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "path")] }],
        &[TestRelation {
            id: 20,
            members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
            tags: vec![("type", "multipolygon")],
        }],
    );

    let _stats = cat(
        &[input.as_path()],
        &output,
        Some("way"),
        &CleanAttrs::default(),
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("cat --type way");

    let c = read_all_elements_with_coords(&output);
    assert!(node_ids(&c).is_empty(), "nodes should be filtered out");
    assert_eq!(way_ids(&c), vec![10]);
    assert!(relation_ids(&c).is_empty(), "relations should be filtered out");
}

#[test]
fn cat_type_filter_node_way() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![] },
        ],
        &[TestWay { id: 10, refs: vec![1], tags: vec![("highway", "path")] }],
        &[TestRelation {
            id: 20,
            members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
            tags: vec![("type", "multipolygon")],
        }],
    );

    let _stats = cat(
        &[input.as_path()],
        &output,
        Some("node,way"),
        &CleanAttrs::default(),
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("cat --type node,way");

    let c = read_all_elements_with_coords(&output);
    assert_eq!(node_ids(&c), vec![1]);
    assert_eq!(way_ids(&c), vec![10]);
    assert!(relation_ids(&c).is_empty(), "relations should be filtered out");
}

// ---------------------------------------------------------------------------
// F53: Multi-file concatenation
// ---------------------------------------------------------------------------

#[test]
fn cat_multi_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input1 = dir.path().join("a.osm.pbf");
    let input2 = dir.path().join("b.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input1,
        &[TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "a")] }],
        &[],
        &[],
    );
    write_test_pbf(
        &input2,
        &[TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "b")] }],
        &[TestWay { id: 10, refs: vec![2], tags: vec![("highway", "road")] }],
        &[],
    );

    let _stats = cat(
        &[input1.as_path(), input2.as_path()],
        &output,
        None,
        &CleanAttrs::default(),
        Compression::default(),
        false,
        true,
        &pbfhogg::HeaderOverrides::default(),
    )
    .expect("cat multi-file");

    let c = read_all_elements_with_coords(&output);
    assert_eq!(c.nodes.len(), 2);
    assert_eq!(c.ways.len(), 1);
    assert_eq!(node_ids(&c), vec![1, 2]);
    assert_eq!(way_ids(&c), vec![10]);
}
