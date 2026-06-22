//! Integration tests for `tags_count` (the library backing for
//! `pbfhogg inspect --tags`). `tags_count` had no prior integration
//! coverage, so this file seeds a minimal test plus the parallel
//! classify parity check called out in the testing-infra sprint.

mod common;

use common::{generate_nodes, generate_ways, write_multi_block_test_pbf};
use pbfhogg::tags_count::{TagCount, TagCountOptions, TagCountSort, tags_count};
use tempfile::TempDir;

fn default_opts() -> TagCountOptions<'static> {
    TagCountOptions {
        min_count: 0,
        max_count: None,
        sort: TagCountSort::NameAsc,
        expressions: &[],
        type_filter: None,
        direct_io: false,
        force: true,
        jobs: 1,
    }
}

/// Tag key=value counts from a tiny hand-crafted fixture.
#[test]
fn basic_tag_counting() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");

    // 5 nodes with two distinct tags, 3 ways with one common tag.
    // Distribute so counts are non-trivial.
    let mut nodes = generate_nodes(5, 1);
    for (i, n) in nodes.iter_mut().enumerate() {
        n.tags = if i < 3 {
            vec![("amenity", "cafe")]
        } else {
            vec![("shop", "bakery")]
        };
    }
    let mut ways = generate_ways(3, 1_000, 2, 1);
    for w in &mut ways {
        w.tags = vec![("highway", "primary")];
    }
    write_multi_block_test_pbf(&input, &nodes, &ways, &[], 100);

    let counts = tags_count(&input, &default_opts()).expect("tags_count");

    // With NameAsc sort, the order is amenity=cafe, highway=primary, shop=bakery.
    let find = |key: &str, value: &str| -> Option<&TagCount> {
        counts.iter().find(|t| t.key == key && t.value == value)
    };
    assert_eq!(find("amenity", "cafe").expect("amenity=cafe").count, 3);
    assert_eq!(find("shop", "bakery").expect("shop=bakery").count, 2);
    assert_eq!(
        find("highway", "primary").expect("highway=primary").count,
        3
    );
}

/// jobs=1 and jobs=4 must produce identical per-blob classification +
/// per-tag count merges. Multi-blob fixture forces the sharding path
/// to actually split work across workers.
#[test]
fn tags_count_parallel_classify_parity() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");

    // 40 nodes, each alternating between two keys; block_size=10 -> 4
    // node blobs. Each blob gets a representative mix so no worker sees
    // a trivially-empty blob.
    let mut nodes = generate_nodes(40, 1);
    for (i, n) in nodes.iter_mut().enumerate() {
        n.tags = if i % 2 == 0 {
            vec![("amenity", "cafe")]
        } else {
            vec![("amenity", "bar"), ("outdoor_seating", "yes")]
        };
    }
    write_multi_block_test_pbf(&input, &nodes, &[], &[], 10);

    let mut opts = default_opts();
    opts.jobs = 1;
    let seq = tags_count(&input, &opts).expect("tags_count seq");

    opts.jobs = 4;
    let par = tags_count(&input, &opts).expect("tags_count par");

    // Sort by (key, value) for order-independent comparison - NameAsc
    // sort above guarantees the orderings match, but be explicit.
    let key = |t: &TagCount| (t.key.clone(), t.value.clone());
    let mut seq_sorted = seq;
    let mut par_sorted = par;
    seq_sorted.sort_by_key(key);
    par_sorted.sort_by_key(|t: &TagCount| (t.key.clone(), t.value.clone()));

    assert_eq!(
        seq_sorted.len(),
        par_sorted.len(),
        "count entry cardinality diverges under -j 4"
    );
    for (s, p) in seq_sorted.iter().zip(par_sorted.iter()) {
        assert_eq!(s.key, p.key, "key drift");
        assert_eq!(s.value, p.value, "value drift");
        assert_eq!(
            s.count, p.count,
            "count drift for {}={}: seq={} par={}",
            s.key, s.value, s.count, p.count
        );
    }

    // Sanity: we actually have some counts to compare.
    assert!(
        !seq_sorted.is_empty(),
        "fixture must produce at least one tag count"
    );
}
