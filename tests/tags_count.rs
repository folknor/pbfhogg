//! Integration tests for `tags_count` (the library backing for
//! `pbfhogg inspect --tags`). `tags_count` had no prior integration
//! coverage, so this file seeds a minimal test plus the parallel
//! classify parity check called out in the testing-infra sprint.

mod common;

use common::cli::CliInvoker;
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

/// `tags-count -e <expr>` must skip blobs on the tag index without changing a
/// single count.
///
/// This is a blob-*skipping* optimization, so getting it wrong drops matches
/// silently - the exact failure mode that makes it worth a test rather than a
/// benchmark. The reference is the same run with the tag index stripped from
/// the file, which forces every blob to be decoded: the two must agree
/// key-for-key and count-for-count.
#[test]
fn expression_filtered_counts_are_identical_with_and_without_a_tag_index() {
    let dir = TempDir::new().expect("tempdir");
    let indexed = dir.path().join("indexed.osm.pbf");
    let no_tagdata = dir.path().join("no-tagdata.osm.pbf");

    // 60 nodes over 6 blobs at block_size=10. Only two blobs carry `amenity`
    // at all, so four blobs are skippable and a skip bug has somewhere to
    // hide. Every blob carries *some* tag, so "skipped" cannot be confused
    // with "had no tagdata".
    let mut nodes = generate_nodes(60, 1);
    for (i, n) in nodes.iter_mut().enumerate() {
        n.tags = if (10..30).contains(&i) {
            vec![("amenity", "cafe"), ("name", "A")]
        } else {
            vec![("building", "yes")]
        };
    }
    write_multi_block_test_pbf(&indexed, &nodes, &[], &[], 10);

    // Reference file: identical elements, no tag index, so no blob can be
    // skipped and every count comes from a full decode.
    common::cli::CliInvoker::new()
        .arg("degrade")
        .arg(&indexed)
        .arg("-o")
        .arg(&no_tagdata)
        .arg("--strip-tagdata")
        .assert_success();

    let exprs = vec!["amenity=cafe".to_string()];
    let mut opts = default_opts();
    opts.expressions = &exprs;

    let with_index = tags_count(&indexed, &opts).expect("tags_count indexed");
    let without_index = tags_count(&no_tagdata, &opts).expect("tags_count reference");

    let norm = |mut v: Vec<TagCount>| {
        v.sort_by_key(|t: &TagCount| (t.key.clone(), t.value.clone()));
        v.into_iter()
            .map(|t| (t.key, t.value, t.count))
            .collect::<Vec<_>>()
    };
    let with_index = norm(with_index);
    let without_index = norm(without_index);

    assert_eq!(
        with_index, without_index,
        "skipping blobs on the tag index must not change a single count"
    );
    // And the fixture must actually have matched something, or the equality
    // above is two empty vectors agreeing.
    assert_eq!(
        with_index,
        vec![("amenity".to_string(), "cafe".to_string(), 20)],
        "20 of the 60 nodes carry amenity=cafe"
    );

    // The parity assertions above are only half the claim, and on their own
    // they are satisfied by the optimization not existing at all: two full
    // decodes agree just as well as a skipping run and a full one. The skip
    // has to be observed. It is reported on stderr rather than returned, so
    // this half goes through the CLI.
    let skipped = CliInvoker::new()
        .arg("inspect")
        .arg("tags")
        .arg(&indexed)
        .arg("amenity=cafe")
        .assert_success()
        .stderr_str();
    assert!(
        skipped.contains("blobs skipped by tag index"),
        "the tag index must actually have skipped blobs, or this test passes \
         with the optimization removed: {skipped}"
    );
    assert!(
        !skipped.contains("0 blobs skipped"),
        "four of the six blobs carry no `amenity` key and must be skipped: \
         {skipped}"
    );

    // Counterpart: the same run against the file with no tag index cannot
    // skip anything, so the line must be absent. Without this, a command that
    // printed the line unconditionally would pass the assertion above.
    let unskipped = CliInvoker::new()
        .arg("inspect")
        .arg("tags")
        .arg(&no_tagdata)
        .arg("amenity=cafe")
        .assert_success()
        .stderr_str();
    assert!(
        !unskipped.contains("blobs skipped by tag index"),
        "a file with no tag index has nothing to skip on: {unskipped}"
    );
}

/// The blob filter must fall back to decoding everything when an expression's
/// key matcher cannot be expressed as key-set membership.
///
/// `*=yes` has an `Always` key matcher, which constrains no key at all. If the
/// filter kept the keys contributed by the *other* expression it would skip
/// blobs the unconstrained one still matches - the filter is a union.
#[test]
fn an_unconstrainable_key_matcher_does_not_skip_blobs() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");

    let mut nodes = generate_nodes(40, 1);
    for (i, n) in nodes.iter_mut().enumerate() {
        n.tags = if i < 20 {
            vec![("amenity", "cafe")]
        } else {
            vec![("building", "yes")]
        };
    }
    write_multi_block_test_pbf(&input, &nodes, &[], &[], 10);

    let exprs = vec!["amenity=cafe".to_string(), "*=yes".to_string()];
    let mut opts = default_opts();
    opts.expressions = &exprs;
    let mut counts = tags_count(&input, &opts).expect("tags_count");
    counts.sort_by_key(|t: &TagCount| (t.key.clone(), t.value.clone()));
    let got: Vec<_> = counts
        .into_iter()
        .map(|t| (t.key, t.value, t.count))
        .collect();

    assert_eq!(
        got,
        vec![
            ("amenity".to_string(), "cafe".to_string(), 20),
            ("building".to_string(), "yes".to_string(), 20),
        ],
        "the `building=yes` blobs carry no `amenity` key, so a filter that \
         kept `amenity` from the first expression would skip them and lose \
         every `*=yes` match in them"
    );
}
