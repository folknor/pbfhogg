//! `tags_count`'s tag-index blob skipping, observed through the CLI. The
//! library-only `tags_count` tests are in the root package's
//! `tests/tags_count.rs`; this one needs the binary, both to strip the tag
//! index from a fixture and to read the skip report on stderr.

use crate::common::{generate_nodes, write_multi_block_test_pbf};
use crate::invoker::CliInvoker;
use pbfhogg::tags_count::{TagCount, TagCountOptions, TagCountSort, tags_count};

fn default_opts() -> TagCountOptions<'static> {
    TagCountOptions {
        min_count: 0,
        max_count: None,
        sort: TagCountSort::NameAsc,
        expressions: &[],
        type_filter: None,
        direct_io: false,
        force: true,
        parallelism: pbfhogg::Parallelism::from_jobs(1),
    }
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
    let dir = crate::common::test_scratch();
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
    CliInvoker::new()
        .arg("degrade")
        .arg(&indexed)
        .arg("-o")
        .arg(&no_tagdata)
        .arg("--strip-tagdata")
        .assert_success();

    let exprs = vec!["amenity=cafe".to_string()];
    let mut opts = default_opts();
    opts.expressions = &exprs;

    let with_index =
        tags_count(&indexed, &opts, &crate::common::test_host()).expect("tags_count indexed");
    let without_index =
        tags_count(&no_tagdata, &opts, &crate::common::test_host()).expect("tags_count reference");

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

/// Each `inspect tags` row is three tab-separated fields, the key and value
/// as JSON-compatible literals: a tab or newline in the text stays inside
/// its field and row.
#[test]
fn rows_quote_and_escape_key_and_value() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("text.osm.pbf");
    let mut nodes = generate_nodes(2, 1);
    nodes[0].tags = vec![("a\tb", "x\ny\"\\")];
    nodes[1].tags = vec![("a\tb", "x\ny\"\\")];
    write_multi_block_test_pbf(&input, &nodes, &[], &[], 10);

    let stdout = CliInvoker::new()
        .arg("inspect")
        .arg("tags")
        .arg(&input)
        .assert_success()
        .stdout_str();
    assert_eq!(stdout, "2\t\"a\\tb\"\t\"x\\ny\\\"\\\\\"\n");
}
