//! `inspect tags` through the binary: its tag-index blob skipping, its
//! summary and its row quoting. The library's `tags_count` tests are in
//! `crates/lib/tests/tags_count.rs`.

use crate::common::{generate_nodes, write_multi_block_test_pbf};
use crate::invoker::CliInvoker;

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
    let invocation_scratch = pbfhogg_test_scratch::scratch();
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
    CliInvoker::new(&invocation_scratch)
        .arg("degrade")
        .arg(&indexed)
        .arg("-o")
        .arg(&no_tagdata)
        .arg("--strip-tagdata")
        .assert_success();

    let count = |input: &std::path::Path| {
        CliInvoker::new(&invocation_scratch)
            .arg("inspect")
            .arg("tags")
            .arg(input)
            .arg("amenity=cafe")
            .assert_success()
    };
    let with_index = count(&indexed);
    let without_index = count(&no_tagdata);

    assert_eq!(
        with_index.stdout_str(),
        without_index.stdout_str(),
        "skipping blobs on the tag index must not change a single count"
    );
    // And the fixture must actually have matched something, or the equality
    // above is two empty outputs agreeing.
    assert_eq!(
        with_index.stdout_str(),
        "20\t\"amenity\"\t\"cafe\"\n",
        "20 of the 60 nodes carry amenity=cafe"
    );

    // The parity assertions above are only half the claim, and on their own
    // they are satisfied by the optimization not existing at all: two full
    // decodes agree just as well as a skipping run and a full one. The skip
    // has to be observed, on stderr.
    let skipped = with_index.stderr_str();
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
    let unskipped = without_index.stderr_str();
    assert!(
        !unskipped.contains("blobs skipped by tag index"),
        "a file with no tag index has nothing to skip on: {unskipped}"
    );
}

/// The closing summary on stderr names all three figures: the distinct key
/// rows and key/value rows counted, and how many passed `--min-count`, which
/// are the rows on stdout. A bare run counts keys; a key row is two fields.
#[test]
fn summary_counts_distinct_rows_and_the_rows_that_passed() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let input = dir.path().join("summary.osm.pbf");
    let mut nodes = generate_nodes(5, 1);
    for (i, n) in nodes.iter_mut().enumerate() {
        n.tags = if i < 3 {
            vec![("amenity", "cafe")]
        } else {
            vec![("shop", "bakery")]
        };
    }
    write_multi_block_test_pbf(&input, &nodes, &[], &[], 10);

    let run = |extra: &[&str]| {
        CliInvoker::new(&invocation_scratch)
            .arg("inspect")
            .arg("tags")
            .arg(&input)
            .arg("--min-count")
            .arg("3")
            .args(extra)
            .assert_success()
    };
    let out = run(&[]);
    assert_eq!(out.stdout_str(), "3\t\"amenity\"\n");
    assert_eq!(
        out.stderr_str(),
        "2 key rows and 0 key/value rows counted, 1 passed the count thresholds\n"
    );
    let out = run(&["amenity", "*=*"]);
    assert_eq!(
        out.stdout_str(),
        "3\t\"amenity\"\n3\t\"amenity\"\t\"cafe\"\n"
    );
    assert_eq!(
        out.stderr_str(),
        "1 key rows and 2 key/value rows counted, 2 passed the count thresholds\n"
    );
}

/// An `inspect tags` row's key and value are JSON-compatible literals: a tab
/// or newline in the text stays inside its field and row. A pair row is
/// three tab-separated fields, a key row two.
#[test]
fn rows_quote_and_escape_key_and_value() {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let dir = crate::common::test_scratch();
    let input = dir.path().join("text.osm.pbf");
    let mut nodes = generate_nodes(2, 1);
    nodes[0].tags = vec![("a\tb", "x\ny\"\\")];
    nodes[1].tags = vec![("a\tb", "x\ny\"\\")];
    write_multi_block_test_pbf(&input, &nodes, &[], &[], 10);

    let stdout = |extra: &[&str]| {
        CliInvoker::new(&invocation_scratch)
            .arg("inspect")
            .arg("tags")
            .arg(&input)
            .args(extra)
            .assert_success()
            .stdout_str()
    };
    assert_eq!(stdout(&["*=*"]), "2\t\"a\\tb\"\t\"x\\ny\\\"\\\\\"\n");
    assert_eq!(stdout(&[]), "2\t\"a\\tb\"\n");
}
