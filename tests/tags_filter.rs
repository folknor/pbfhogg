//! Tags-filter correctness tests.

mod common;

use common::{
    generate_nodes, generate_relations, generate_ways,
    node_ids_id_only as node_ids, read_all_elements_id_only as read_all_elements,
    relation_ids_id_only as relation_ids, way_ids_id_only as way_ids,
    write_multi_block_test_pbf, write_test_pbf, TestMember, TestNode, TestRelation, TestWay,
};
use pbfhogg::MemberId;
use pbfhogg::tags_filter::{tags_filter, TagsFilterOptions};
use pbfhogg::writer::Compression;
use tempfile::TempDir;

fn run_filter(
    input: &std::path::Path,
    output: &std::path::Path,
    expression_strs: &[String],
    omit_referenced: bool,
) -> pbfhogg::tags_filter::TagsFilterStats {
    let opts = TagsFilterOptions {
        expression_strs,
        omit_referenced,
        invert: false,
        remove_tags: false,
        compression: Compression::default(),
        direct_io: false,
        force: true,
        jobs: None,
    };
    tags_filter(input, output, &opts, &pbfhogg::HeaderOverrides::default()).expect("filter")
}

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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("amenity", "bench")], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "foo")], meta: None },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![("amenity", "restaurant"), ("name", "bar")], meta: None },
        ],
        &[],
        &[],
    );

    let stats = run_filter(&input, &output, &exprs(&["amenity"]), true);
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![], meta: None },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")], meta: None },
            TestWay { id: 11, refs: vec![2, 3], tags: vec![("highway", "secondary")], meta: None },
            TestWay { id: 12, refs: vec![1, 3], tags: vec![("name", "road")], meta: None },
        ],
        &[],
    );

    let stats = run_filter(&input, &output, &exprs(&["highway=primary"]), true);
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
            TestRelation { id: 1, members: vec![], tags: vec![("type", "multipolygon")], meta: None },
            TestRelation { id: 2, members: vec![], tags: vec![("type", "boundary")], meta: None },
            TestRelation { id: 3, members: vec![], tags: vec![("type", "route")], meta: None },
        ],
    );

    let stats = run_filter(&input, &output, &exprs(&["type=multipolygon,boundary"]), true);
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
            TestWay { id: 10, refs: vec![], tags: vec![("highway", "primary")], meta: None },
            TestWay { id: 11, refs: vec![], tags: vec![("highway", "secondary")], meta: None },
            TestWay { id: 12, refs: vec![], tags: vec![("name", "road")], meta: None }, // no highway tag
        ],
        &[],
    );

    let stats = run_filter(&input, &output, &exprs(&["highway!=primary"]), true);
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("addr:street", "Main St")], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("addr:city", "Berlin")], meta: None },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![("name", "foo")], meta: None },
        ],
        &[],
        &[],
    );

    let stats = run_filter(&input, &output, &exprs(&["addr:*"]), true);
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("building", "yes")], meta: None },
        ],
        &[
            TestWay { id: 10, refs: vec![], tags: vec![("building", "yes")], meta: None },
        ],
        &[],
    );

    // w/ prefix - only ways
    let stats = run_filter(&input, &output, &exprs(&["w/building=yes"]), true);
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("natural", "tree")], meta: None },
        ],
        &[
            TestWay { id: 10, refs: vec![], tags: vec![("natural", "tree")], meta: None },
        ],
        &[
            TestRelation { id: 100, members: vec![], tags: vec![("natural", "tree")], meta: None },
        ],
    );

    let stats = run_filter(&input, &output, &exprs(&["nw/natural=tree"]), true);
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![], meta: None },
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![], meta: None }, // not referenced
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2, 3], tags: vec![("highway", "primary")], meta: None },
        ],
        &[],
    );

    // Default mode (include references)
    let stats = run_filter(&input, &output, &exprs(&["highway=primary"]), false);
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")], meta: None },
        ],
        &[],
    );

    // -R mode (omit references)
    let stats = run_filter(&input, &output, &exprs(&["highway=primary"]), true);
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("amenity", "bench")], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![], meta: None },
            TestNode { id: 4, lat: 130_000_000, lon: 230_000_000, tags: vec![], meta: None }, // excluded
        ],
        &[
            TestWay { id: 10, refs: vec![2, 3], tags: vec![("highway", "primary")], meta: None },
        ],
        &[],
    );

    let stats = run_filter(&input, &output, &exprs(&["amenity", "highway=primary"]), false);
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("name", "foo")], meta: None },
        ],
        &[],
        &[],
    );

    let stats = run_filter(&input, &output, &exprs(&["nonexistent_key"]), true);
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
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("amenity", "bench")], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("shop", "bakery")], meta: None },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![("name", "foo")], meta: None },
        ],
        &[],
        &[],
    );

    // Both "amenity" and "shop" - OR semantics
    let stats = run_filter(&input, &output, &exprs(&["amenity", "shop"]), true);
    let c = read_all_elements(&output);

    assert_eq!(node_ids(&c), vec![1, 2]);
    assert_eq!(stats.nodes_matched, 2);
}

#[test]
fn relation_match_includes_member_way_and_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![], meta: None }, // unrelated
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![], meta: None },
            TestWay { id: 11, refs: vec![3], tags: vec![], meta: None }, // unrelated
        ],
        &[
            TestRelation {
                id: 100,
                members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
                tags: vec![("type", "multipolygon")],
                meta: None,
            },
        ],
    );

    let stats = run_filter(&input, &output, &exprs(&["type=multipolygon"]), false);
    let c = read_all_elements(&output);

    assert_eq!(relation_ids(&c), vec![100]);
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(node_ids(&c), vec![1, 2]);
    assert_eq!(stats.relations_matched, 1);
    assert_eq!(stats.ways_matched, 0);
    assert_eq!(stats.ways_from_relations, 1);
    assert_eq!(stats.nodes_from_ways, 0);
    assert_eq!(stats.nodes_from_relations, 2);
}

#[test]
fn relation_match_includes_nested_relation_members() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![], meta: None },
        ],
        &[
            TestRelation {
                id: 100,
                members: vec![TestMember { id: MemberId::Relation(200), role: "" }],
                tags: vec![("type", "route")],
                meta: None,
            },
            TestRelation {
                id: 200,
                members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
                tags: vec![],
                meta: None,
            },
        ],
    );

    let _stats = run_filter(&input, &output, &exprs(&["type=route"]), false);
    let c = read_all_elements(&output);

    assert_eq!(relation_ids(&c), vec![100, 200]);
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(node_ids(&c), vec![1, 2]);
}

#[test]
fn relation_cycle_terminates_and_includes_each_once() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[],
        &[],
        &[
            TestRelation {
                id: 100,
                members: vec![TestMember { id: MemberId::Relation(200), role: "" }],
                tags: vec![("type", "route")],
                meta: None,
            },
            TestRelation {
                id: 200,
                members: vec![TestMember { id: MemberId::Relation(100), role: "" }],
                tags: vec![],
                meta: None,
            },
        ],
    );

    let _stats = run_filter(&input, &output, &exprs(&["type=route"]), false);
    let c = read_all_elements(&output);

    assert_eq!(relation_ids(&c), vec![100, 200]);
}

#[test]
fn omit_referenced_does_not_expand_relation_members() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![], meta: None },
        ],
        &[
            TestRelation {
                id: 100,
                members: vec![TestMember { id: MemberId::Way(10), role: "outer" }],
                tags: vec![("type", "multipolygon")],
                meta: None,
            },
        ],
    );

    let stats = run_filter(&input, &output, &exprs(&["type=multipolygon"]), true);
    let c = read_all_elements(&output);

    assert_eq!(relation_ids(&c), vec![100]);
    assert!(way_ids(&c).is_empty());
    assert!(node_ids(&c).is_empty());
    assert_eq!(stats.relations_matched, 1);
}

// ---------------------------------------------------------------------------
// F49: --invert-match
// ---------------------------------------------------------------------------

fn run_filter_opts(
    input: &std::path::Path,
    output: &std::path::Path,
    expression_strs: &[String],
    omit_referenced: bool,
    invert: bool,
    remove_tags: bool,
) -> pbfhogg::tags_filter::TagsFilterStats {
    let opts = TagsFilterOptions {
        expression_strs,
        omit_referenced,
        invert,
        remove_tags,
        compression: Compression::default(),
        direct_io: false,
        force: true,
        jobs: None,
    };
    tags_filter(input, output, &opts, &pbfhogg::HeaderOverrides::default()).expect("filter")
}

#[test]
fn invert_match_excludes_matching_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("amenity", "bench")], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "foo")], meta: None },
            TestNode { id: 3, lat: 120_000_000, lon: 220_000_000, tags: vec![], meta: None },
        ],
        &[],
        &[],
    );

    // Invert: output nodes that do NOT match "amenity"
    let stats = run_filter_opts(&input, &output, &exprs(&["amenity"]), true, true, false);
    let c = read_all_elements(&output);

    // Node 1 has amenity → excluded. Nodes 2 and 3 → included.
    assert_eq!(node_ids(&c), vec![2, 3]);
    assert_eq!(stats.nodes_matched, 2);
}

#[test]
fn invert_match_excludes_matching_ways() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![], meta: None },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "primary")], meta: None },
            TestWay { id: 11, refs: vec![1, 2], tags: vec![("highway", "secondary")], meta: None },
            TestWay { id: 12, refs: vec![1, 2], tags: vec![("building", "yes")], meta: None },
        ],
        &[],
    );

    // Invert: output ways that do NOT match "highway"
    let _stats = run_filter_opts(&input, &output, &exprs(&["highway"]), true, true, false);
    let c = read_all_elements(&output);

    // Ways 10, 11 have highway → excluded. Way 12 → included.
    assert_eq!(way_ids(&c), vec![12]);
}

// ---------------------------------------------------------------------------
// F50: --remove-tags
// ---------------------------------------------------------------------------

#[test]
fn remove_tags_strips_tags_from_referenced_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode { id: 1, lat: 100_000_000, lon: 200_000_000, tags: vec![("shop", "bakery")], meta: None },
            TestNode { id: 2, lat: 110_000_000, lon: 210_000_000, tags: vec![("name", "corner")], meta: None },
        ],
        &[
            TestWay { id: 10, refs: vec![1, 2], tags: vec![("highway", "residential")], meta: None },
        ],
        &[],
    );

    // Two-pass (omit_referenced=false) with remove_tags: way 10 matches
    // "highway", its referenced nodes 1,2 are pulled in but with tags stripped.
    let _stats = run_filter_opts(&input, &output, &exprs(&["highway"]), false, false, true);
    let c = read_all_elements(&output);

    // Way 10 should keep its tags (directly matched)
    assert_eq!(way_ids(&c), vec![10]);
    let way_tags: Vec<_> = c.ways.iter()
        .find(|(id, _, _)| *id == 10)
        .map(|(_, _, tags)| tags.clone())
        .unwrap_or_default();
    assert!(!way_tags.is_empty(), "directly matched way should keep tags");

    // Nodes 1,2 should be present but with empty tags (referenced only)
    assert_eq!(node_ids(&c), vec![1, 2]);
    for (id, tags) in &c.nodes {
        assert!(tags.is_empty(), "node {id} should have tags stripped (referenced only)");
    }
}

// ---------------------------------------------------------------------------
// Parallel classify parity: jobs=1 vs jobs=4 across multiple blobs
// ---------------------------------------------------------------------------
//
// Two-pass `tags_filter` routes blob scanning through
// `parallel_classify_phase` and the follow-up relation/way-node
// dependency closures through two `parallel_classify_accumulate` calls.
// `TagsFilterOptions.jobs` caps the worker-pool size for all three
// calls (src/commands/tags_filter/mod.rs:150-160). With single-blob
// fixtures the sharding is trivial; this test forces 4 node blobs +
// 2 way blobs + 2 relation blobs and asserts that jobs=1 and jobs=4
// produce the same element set and identical stats.

fn run_tags_filter(input: &std::path::Path, jobs: Option<usize>) -> (common::PbfContentsIdOnly, pbfhogg::tags_filter::TagsFilterStats) {
    let dir = TempDir::new().expect("tempdir");
    let output = dir.path().join("output.osm.pbf");
    let expression_strs = vec!["w/highway=primary".to_string()];
    let opts = TagsFilterOptions {
        expression_strs: &expression_strs,
        omit_referenced: false,
        invert: false,
        remove_tags: false,
        compression: Compression::default(),
        direct_io: false,
        force: true,
        jobs,
    };
    let stats = tags_filter(input, &output, &opts, &pbfhogg::HeaderOverrides::default())
        .expect("tags_filter");
    let c = read_all_elements(&output);
    (c, stats)
}

#[test]
fn tags_filter_parallel_classify_parity() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");

    // 40 nodes + 20 ways (half tagged highway=primary, half building=yes)
    // + 4 relations. block_size=10 -> 4 node blobs, 2 way blobs, 1 rel blob.
    let nodes = generate_nodes(40, 1);
    let mut ways = generate_ways(20, 1_000, 2, 1);
    for (i, w) in ways.iter_mut().enumerate() {
        if i % 2 == 0 {
            w.tags = vec![("highway", "primary")];
        } else {
            w.tags = vec![("building", "yes")];
        }
    }
    let relations = generate_relations(4, 10_000, 2, 1_000);

    write_multi_block_test_pbf(&input, &nodes, &ways, &relations, 10);

    let (c_seq, s_seq) = run_tags_filter(&input, Some(1));
    let (c_par, s_par) = run_tags_filter(&input, Some(4));

    // Stats must match exactly.
    assert_eq!(s_seq.nodes_matched, s_par.nodes_matched, "nodes_matched parity");
    assert_eq!(s_seq.nodes_from_ways, s_par.nodes_from_ways, "nodes_from_ways parity");
    assert_eq!(s_seq.ways_matched, s_par.ways_matched, "ways_matched parity");
    assert_eq!(s_seq.relations_matched, s_par.relations_matched, "relations_matched parity");

    // Element sets must match (ids only, order preserved since sorted input).
    assert_eq!(node_ids(&c_seq), node_ids(&c_par), "node id set diverges under -j 4");
    assert_eq!(way_ids(&c_seq), way_ids(&c_par), "way id set diverges under -j 4");
    assert_eq!(relation_ids(&c_seq), relation_ids(&c_par), "relation id set diverges under -j 4");

    // Sanity: the filter must have matched SOME ways (otherwise the
    // test is trivially parity-clean because nothing is emitted).
    assert!(s_seq.ways_matched > 0, "filter must match at least one way");
}
