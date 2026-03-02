//! Tags-filter correctness tests.

mod common;

use common::{
    node_ids_id_only as node_ids, read_all_elements_id_only as read_all_elements,
    way_ids_id_only as way_ids, relation_ids_id_only as relation_ids,
    write_test_pbf, TestNode, TestRelation, TestWay,
};
use pbfhogg::tags_filter::tags_filter;
use pbfhogg::writer::Compression;
use tempfile::TempDir;

fn exprs(strs: &[&str]) -> Vec<String> {
    strs.iter().map(ToString::to_string).collect()
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

    let stats = tags_filter(&input, &output, &exprs(&["amenity"]), true, Compression::default(), false, true).expect("filter");
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

    let stats = tags_filter(&input, &output, &exprs(&["highway=primary"]), true, Compression::default(), false, true).expect("filter");
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

    let stats = tags_filter(&input, &output, &exprs(&["type=multipolygon,boundary"]), true, Compression::default(), false, true).expect("filter");
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

    let stats = tags_filter(&input, &output, &exprs(&["highway!=primary"]), true, Compression::default(), false, true).expect("filter");
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

    let stats = tags_filter(&input, &output, &exprs(&["addr:*"]), true, Compression::default(), false, true).expect("filter");
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
    let stats = tags_filter(&input, &output, &exprs(&["w/building=yes"]), true, Compression::default(), false, true).expect("filter");
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

    let stats = tags_filter(&input, &output, &exprs(&["nw/natural=tree"]), true, Compression::default(), false, true).expect("filter");
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
    let stats = tags_filter(&input, &output, &exprs(&["highway=primary"]), false, Compression::default(), false, true).expect("filter");
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
    let stats = tags_filter(&input, &output, &exprs(&["highway=primary"]), true, Compression::default(), false, true).expect("filter");
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
        Compression::default(),
        false,
        true,
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

    let stats = tags_filter(&input, &output, &exprs(&["nonexistent_key"]), true, Compression::default(), false, true).expect("filter");
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
    let stats = tags_filter(&input, &output, &exprs(&["amenity", "shop"]), true, Compression::default(), false, true).expect("filter");
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2]);
    assert_eq!(stats.nodes_matched, 2);
}
