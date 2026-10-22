//! CLI-driven integration tests for `pbfhogg tags-filter`.
//!
//! Replaces the removed library-API `tags_filter.rs` test. Fixture PBFs
//! are built with the stable-allowlist writers; `tags-filter`
//! runs through `CliInvoker`; output is verified by reading the
//! resulting PBF with the stable-allowlist readers. No imports
//! from `pbfhogg::tags_filter::*` - a rewrite of
//! `src/commands/tags_filter/` cannot break these tests by type
//! changes alone.
//!
//! The original library tests asserted on individual `TagsFilterStats`
//! counters (`nodes_matched`, `nodes_from_ways`, etc.). For the CLI
//! shape those assertions are redundant with the element-set
//! assertions on every test except the jobs-parity test, so they are
//! dropped here. The jobs-parity test compares the full stderr
//! summary lines between `-j 1` and `-j 4` to pin classifier
//! consistency under sharding.

#![allow(clippy::unwrap_used)]

use std::path::Path;

use crate::common::cli::CliInvoker;
use crate::common::{
    PbfContentsIdOnly, TestMember, TestNode, TestRelation, TestWay, generate_nodes,
    generate_relations, generate_ways, node_ids_id_only as node_ids,
    read_all_elements_id_only as read_all_elements, relation_ids_id_only as relation_ids,
    way_ids_id_only as way_ids, write_multi_block_test_pbf, write_test_pbf,
};
use pbfhogg::MemberId;
use tempfile::TempDir;

/// Invoke `pbfhogg tags-filter <input> -o <output> [-R] [-i] [-t]
/// [-j N] <expressions...> --force` and assert success. Returns
/// the captured stderr (which carries the
/// `TagsFilterStats::write_summary` line).
fn run_filter_full(
    input: &Path,
    output: &Path,
    expressions: &[&str],
    omit_referenced: bool,
    invert: bool,
    remove_tags: bool,
    jobs: Option<usize>,
) -> String {
    let mut cli = CliInvoker::new()
        .arg("tags-filter")
        .arg(input)
        .arg("-o")
        .arg(output);
    if omit_referenced {
        cli = cli.arg("-R");
    }
    if invert {
        cli = cli.arg("-i");
    }
    if remove_tags {
        cli = cli.arg("-t");
    }
    if let Some(j) = jobs {
        cli = cli.arg("-j").arg(j.to_string());
    }
    cli = cli.arg("--force");
    for expr in expressions {
        cli = cli.arg(*expr);
    }
    cli.assert_success().stderr_str()
}

fn run_filter(input: &Path, output: &Path, expressions: &[&str], omit_referenced: bool) -> String {
    run_filter_full(
        input,
        output,
        expressions,
        omit_referenced,
        false,
        false,
        None,
    )
}

/// Blob-level tag-index skipping is a trust boundary: nothing validates that a
/// blob's `tagdata` describes its payload, so a file whose index lies makes
/// `tags-filter` skip blobs whose elements match and emit a short answer
/// without a hint that anything went wrong.
///
/// `degrade --corrupt-tagdata` produces exactly that file - it did not exist
/// before, because `--strip-tagdata` only *removes* the index and a missing
/// index is decoded rather than trusted - and `--distrust-metadata` is the
/// escape hatch that gets the right answer back out of it.
#[test]
fn distrust_metadata_recovers_matches_a_lying_tag_index_would_hide() {
    let dir = TempDir::new().expect("tempdir");
    let pristine = dir.path().join("pristine.osm.pbf");
    let corrupt = dir.path().join("corrupt.osm.pbf");

    crate::common::write_indexed_pbf(
        &pristine,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("amenity", "bench")],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("name", "foo")],
                meta: None,
            },
        ],
        &[],
        &[],
    );

    // Baseline on the honest file: the tag index agrees with the payload, so
    // the fast path and the right answer coincide.
    let out = dir.path().join("pristine-out.osm.pbf");
    run_filter(&pristine, &out, &["amenity"], false);
    assert_eq!(
        node_ids(&read_all_elements(&out)),
        vec![1],
        "the fixture itself must match, or the assertions below prove nothing"
    );

    CliInvoker::new()
        .arg("degrade")
        .arg(&pristine)
        .arg("-o")
        .arg(&corrupt)
        .arg("--corrupt-tagdata")
        .assert_success();

    // Default: the index is believed, the blob is skipped unread, and n1 is
    // silently missing from the output. This is the documented trust boundary,
    // asserted so a change to it cannot pass unnoticed.
    let believed = dir.path().join("believed.osm.pbf");
    let stderr = run_filter(&corrupt, &believed, &["amenity"], false);
    assert!(
        node_ids(&read_all_elements(&believed)).is_empty(),
        "a lying tag index makes tags-filter skip a blob whose elements match"
    );
    assert!(
        stderr.contains("skipped by tag index"),
        "the skip is reported as a cheerful optimisation; stderr was:\n{stderr}"
    );

    // With --distrust-metadata the answer depends only on element data.
    let distrusted = dir.path().join("distrusted.osm.pbf");
    CliInvoker::new()
        .arg("tags-filter")
        .arg(&corrupt)
        .arg("-o")
        .arg(&distrusted)
        .arg("--distrust-metadata")
        .arg("amenity")
        .assert_success();
    assert_eq!(
        node_ids(&read_all_elements(&distrusted)),
        vec![1],
        "--distrust-metadata must decode the blob the tag index disowned"
    );
}

/// `--distrust-metadata` has to hold on the *reference-expansion* scans, not
/// only on the pass that finds direct matches.
///
/// Relation closure and way-dependency collection each build a per-kind blob
/// schedule, and `build_classify_schedule` selects blobs on the kind their
/// indexdata declares - the exact claim the flag says it will not believe.
/// The first version of the flag left both narrowed, so on a file with
/// mislabelled blob kinds the direct match was found and written while its
/// referenced members were never collected: an output missing required
/// dependencies, produced by a run that promised the opposite, with nothing
/// on stderr to notice.
///
/// The fixture bites through both helpers at once. `boundary` matches only
/// r100, so w10 can enter the output solely through relation closure and n1/n2
/// solely through way-dependency collection off a non-direct way.
/// `degrade --corrupt-indexdata-kind` rotates every blob's declared kind, so
/// the relation blob claims Node and the way blob claims Relation and each
/// narrowed schedule comes back empty.
#[test]
fn distrust_metadata_holds_through_reference_expansion() {
    let dir = TempDir::new().expect("tempdir");
    let pristine = dir.path().join("pristine.osm.pbf");
    let corrupt = dir.path().join("corrupt.osm.pbf");

    crate::common::write_indexed_pbf(
        &pristine,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("name", "member way")],
            meta: None,
        }],
        &[TestRelation {
            id: 100,
            members: vec![TestMember {
                id: MemberId::Way(10),
                role: "outer",
            }],
            tags: vec![("boundary", "administrative")],
            meta: None,
        }],
    );

    // Baseline on the honest file: closure pulls w10 in, way deps pull n1/n2.
    let out = dir.path().join("pristine-out.osm.pbf");
    run_filter(&pristine, &out, &["boundary"], false);
    let c = read_all_elements(&out);
    assert_eq!(relation_ids(&c), vec![100]);
    assert_eq!(
        way_ids(&c),
        vec![10],
        "w10 is not a direct match; it can only arrive through relation closure"
    );
    assert_eq!(
        node_ids(&c),
        vec![1, 2],
        "n1/n2 are untagged; they can only arrive through way-dependency collection"
    );

    CliInvoker::new()
        .arg("degrade")
        .arg(&pristine)
        .arg("-o")
        .arg(&corrupt)
        .arg("--corrupt-indexdata-kind")
        .assert_success();

    // The default run on the corrupt file must lose the expanded references.
    // Without this arm the test below would pass just as happily if
    // `--corrupt-indexdata-kind` were a no-op - the fixture would simply be
    // honest and the distrust assertion would succeed for the wrong reason.
    let believed = dir.path().join("believed.osm.pbf");
    run_filter(&corrupt, &believed, &["boundary"], false);
    let c = read_all_elements(&believed);
    assert_eq!(
        relation_ids(&c),
        vec![100],
        "pass 1 and pass 2 already decline blob filtering here, so the direct \
         match survives - it is the expansion that a mislabelled kind breaks"
    );
    assert!(
        way_ids(&c).is_empty() && node_ids(&c).is_empty(),
        "a relation blob mislabelled as Node empties the closure schedule, so \
         nothing is pulled in through membership; got ways {:?} nodes {:?}",
        way_ids(&c),
        node_ids(&c),
    );

    // With --distrust-metadata every scan reads every blob and decides on
    // decoded elements, so the mislabelling changes nothing about the answer.
    let distrusted = dir.path().join("distrusted.osm.pbf");
    CliInvoker::new()
        .arg("tags-filter")
        .arg(&corrupt)
        .arg("-o")
        .arg(&distrusted)
        .arg("--distrust-metadata")
        .arg("boundary")
        .assert_success();
    let c = read_all_elements(&distrusted);
    assert_eq!(relation_ids(&c), vec![100]);
    assert_eq!(
        way_ids(&c),
        vec![10],
        "relation closure must not select blobs on the kind indexdata declares"
    );
    assert_eq!(
        node_ids(&c),
        vec![1, 2],
        "way-dependency collection must not select blobs on the kind indexdata declares"
    );
}

/// `--distrust-metadata` is meaningless on OSC input, which has no blobs, and
/// says so instead of accepting a flag that would do nothing.
#[test]
fn distrust_metadata_is_rejected_on_osc_input() {
    let dir = TempDir::new().expect("tempdir");
    let osc = dir.path().join("changes.osc.gz");
    {
        use std::io::Write;
        let file = std::fs::File::create(&osc).expect("create osc");
        let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        enc.write_all(b"<?xml version='1.0'?>\n<osmChange version=\"0.6\"></osmChange>\n")
            .expect("write xml");
        enc.finish().expect("finish gz");
    }
    let out = dir.path().join("out.osc.gz");

    let failure = CliInvoker::new()
        .arg("tags-filter")
        .arg(&osc)
        .arg("-o")
        .arg(&out)
        .arg("--distrust-metadata")
        .arg("amenity")
        .assert_failure();
    failure.assert_stderr_contains("carries no blob metadata");

    // Passing counterpart: the same OSC run without the flag is accepted.
    CliInvoker::new()
        .arg("tags-filter")
        .arg(&osc)
        .arg("-o")
        .arg(&out)
        .arg("amenity")
        .assert_success();
}

#[test]
fn key_only_filter() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("amenity", "bench")],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("name", "foo")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![("amenity", "restaurant"), ("name", "bar")],
                meta: None,
            },
        ],
        &[],
        &[],
    );

    run_filter(&input, &output, &["amenity"], true);
    let c = read_all_elements(&output);
    assert_eq!(node_ids(&c), vec![1, 3]);
}

/// `--expressions` files follow osmium's reader: inline `#` comments, CRLF
/// tolerated, and a tab is part of the key rather than whitespace. The
/// reader once ran `str::trim` over each line, so a tab-led key matched the
/// plain key instead.
#[test]
fn expressions_file_keeps_a_tab_led_key_and_strips_inline_comments() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let exprs = dir.path().join("exprs.txt");

    write_test_pbf(
        &input,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("highway", "primary")],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("\thighway", "primary")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![("amenity", "bench")],
                meta: None,
            },
        ],
        &[],
        &[],
    );
    std::fs::write(
        &exprs,
        "# header comment\r\n\thighway=primary # inline\r\n   \r\n  amenity=bench\r\n",
    )
    .expect("write expressions");

    CliInvoker::new()
        .arg("tags-filter")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("-R")
        .arg("--force")
        .arg("-e")
        .arg(&exprs)
        .assert_success();
    let c = read_all_elements(&output);
    assert_eq!(
        node_ids(&c),
        vec![2, 3],
        "the tab-led key must match only the tab-led tag"
    );
}

/// An indented type prefix is an error, as it is in osmium, rather than the
/// silent zero-match literal key `w/highway`.
#[test]
fn expressions_file_rejects_an_indented_type_prefix() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");
    let exprs = dir.path().join("exprs.txt");
    write_test_pbf(&input, &generate_nodes(3, 1), &[], &[]);
    std::fs::write(&exprs, "  w/highway\n").expect("write expressions");

    let out = CliInvoker::new()
        .arg("tags-filter")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--force")
        .arg("-e")
        .arg(&exprs)
        .run();
    assert!(!out.status.success(), "an indented type prefix must fail");
    assert!(
        out.stderr_str().contains("contains spaces"),
        "stderr: {}",
        out.stderr_str()
    );
}

#[test]
fn exact_value_filter() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[
            TestWay {
                id: 10,
                refs: vec![1, 2],
                tags: vec![("highway", "primary")],
                meta: None,
            },
            TestWay {
                id: 11,
                refs: vec![2, 3],
                tags: vec![("highway", "secondary")],
                meta: None,
            },
            TestWay {
                id: 12,
                refs: vec![1, 3],
                tags: vec![("name", "road")],
                meta: None,
            },
        ],
        &[],
    );

    run_filter(&input, &output, &["highway=primary"], true);
    let c = read_all_elements(&output);
    assert_eq!(way_ids(&c), vec![10]);
    assert!(node_ids(&c).is_empty());
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
            TestRelation {
                id: 1,
                members: vec![],
                tags: vec![("type", "multipolygon")],
                meta: None,
            },
            TestRelation {
                id: 2,
                members: vec![],
                tags: vec![("type", "boundary")],
                meta: None,
            },
            TestRelation {
                id: 3,
                members: vec![],
                tags: vec![("type", "route")],
                meta: None,
            },
        ],
    );

    run_filter(&input, &output, &["type=multipolygon,boundary"], true);
    let c = read_all_elements(&output);
    assert_eq!(relation_ids(&c), vec![1, 2]);
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
            TestWay {
                id: 10,
                refs: vec![],
                tags: vec![("highway", "primary")],
                meta: None,
            },
            TestWay {
                id: 11,
                refs: vec![],
                tags: vec![("highway", "secondary")],
                meta: None,
            },
            TestWay {
                id: 12,
                refs: vec![],
                tags: vec![("name", "road")],
                meta: None,
            }, // no highway tag
        ],
        &[],
    );

    run_filter(&input, &output, &["highway!=primary"], true);
    let c = read_all_elements(&output);
    // Only way 11 matches: has highway tag with value != primary
    // Way 10: highway=primary -> excluded by negation
    // Way 12: no highway tag -> no match
    assert_eq!(way_ids(&c), vec![11]);
}

#[test]
fn wildcard_prefix_filter() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("addr:street", "Main St")],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("addr:city", "Berlin")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![("name", "foo")],
                meta: None,
            },
        ],
        &[],
        &[],
    );

    run_filter(&input, &output, &["addr:*"], true);
    let c = read_all_elements(&output);
    assert_eq!(node_ids(&c), vec![1, 2]);
}

#[test]
fn type_prefix_filter() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("building", "yes")],
            meta: None,
        }],
        &[TestWay {
            id: 10,
            refs: vec![],
            tags: vec![("building", "yes")],
            meta: None,
        }],
        &[],
    );

    // w/ prefix - only ways
    run_filter(&input, &output, &["w/building=yes"], true);
    let c = read_all_elements(&output);
    assert!(node_ids(&c).is_empty());
    assert_eq!(way_ids(&c), vec![10]);
}

#[test]
fn combined_type_prefix_nw() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("natural", "tree")],
            meta: None,
        }],
        &[TestWay {
            id: 10,
            refs: vec![],
            tags: vec![("natural", "tree")],
            meta: None,
        }],
        &[TestRelation {
            id: 100,
            members: vec![],
            tags: vec![("natural", "tree")],
            meta: None,
        }],
    );

    run_filter(&input, &output, &["nw/natural=tree"], true);
    let c = read_all_elements(&output);
    assert_eq!(node_ids(&c), vec![1]);
    assert_eq!(way_ids(&c), vec![10]);
    assert!(relation_ids(&c).is_empty());
}

#[test]
fn two_pass_includes_way_dep_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 4,
                lat: 130_000_000,
                lon: 230_000_000,
                tags: vec![],
                meta: None,
            }, // not referenced
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2, 3],
            tags: vec![("highway", "primary")],
            meta: None,
        }],
        &[],
    );

    // Default mode (include references)
    let stderr = run_filter(&input, &output, &["highway=primary"], false);
    let c = read_all_elements(&output);
    assert_eq!(node_ids(&c), vec![1, 2, 3]); // referenced nodes included
    assert_eq!(way_ids(&c), vec![10]);
    // Pin the from-ways count specifically: this test is the canonical
    // proof that referenced nodes are pulled in via the second pass.
    assert!(
        stderr.contains("3 nodes (0 direct + 3 from ways + 0 from relations)"),
        "stats line missing expected counters; stderr =\n{stderr}",
    );
}

#[test]
fn omit_referenced_excludes_way_dep_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "primary")],
            meta: None,
        }],
        &[],
    );

    // -R mode (omit references)
    run_filter(&input, &output, &["highway=primary"], true);
    let c = read_all_elements(&output);
    assert!(node_ids(&c).is_empty());
    assert_eq!(way_ids(&c), vec![10]);
}

#[test]
fn two_pass_direct_node_match_plus_way_deps() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("amenity", "bench")],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 4,
                lat: 130_000_000,
                lon: 230_000_000,
                tags: vec![],
                meta: None,
            }, // excluded
        ],
        &[TestWay {
            id: 10,
            refs: vec![2, 3],
            tags: vec![("highway", "primary")],
            meta: None,
        }],
        &[],
    );

    let stderr = run_filter(&input, &output, &["amenity", "highway=primary"], false);
    let c = read_all_elements(&output);
    assert_eq!(node_ids(&c), vec![1, 2, 3]); // 1 direct, 2+3 from way
    assert_eq!(way_ids(&c), vec![10]);
    // Pin the direct/from-ways split: this test specifically exercises
    // the case where direct matches and reference expansion overlap.
    assert!(
        stderr.contains("3 nodes (1 direct + 2 from ways + 0 from relations)"),
        "stats line missing expected counters; stderr =\n{stderr}",
    );
}

#[test]
fn empty_result_produces_valid_pbf() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[TestNode {
            id: 1,
            lat: 100_000_000,
            lon: 200_000_000,
            tags: vec![("name", "foo")],
            meta: None,
        }],
        &[],
        &[],
    );

    run_filter(&input, &output, &["nonexistent_key"], true);
    let c = read_all_elements(&output);
    assert!(node_ids(&c).is_empty());
    assert!(way_ids(&c).is_empty());
    assert!(relation_ids(&c).is_empty());
}

#[test]
fn multiple_expressions_or_semantics() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("amenity", "bench")],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("shop", "bakery")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![("name", "foo")],
                meta: None,
            },
        ],
        &[],
        &[],
    );

    // Both "amenity" and "shop" - OR semantics
    run_filter(&input, &output, &["amenity", "shop"], true);
    let c = read_all_elements(&output);
    assert_eq!(node_ids(&c), vec![1, 2]);
}

#[test]
fn relation_match_includes_member_way_and_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![],
                meta: None,
            }, // unrelated
        ],
        &[
            TestWay {
                id: 10,
                refs: vec![1, 2],
                tags: vec![],
                meta: None,
            },
            TestWay {
                id: 11,
                refs: vec![3],
                tags: vec![],
                meta: None,
            }, // unrelated
        ],
        &[TestRelation {
            id: 100,
            members: vec![TestMember {
                id: MemberId::Way(10),
                role: "outer",
            }],
            tags: vec![("type", "multipolygon")],
            meta: None,
        }],
    );

    let stderr = run_filter(&input, &output, &["type=multipolygon"], false);
    let c = read_all_elements(&output);
    assert_eq!(relation_ids(&c), vec![100]);
    assert_eq!(way_ids(&c), vec![10]);
    assert_eq!(node_ids(&c), vec![1, 2]);
    // Pin the relation-pulls-way and way-pulls-node chain explicitly.
    // The way-total is 1 (0 direct + 1 pulled from the relation).
    assert!(
        stderr.contains("1 ways (0 direct + 1 from relations)"),
        "stats line missing 'ways from relations' counter; stderr =\n{stderr}",
    );
    assert!(
        stderr.contains("2 nodes (0 direct + 0 from ways + 2 from relations)"),
        "stats line missing 'nodes from relations' counter; stderr =\n{stderr}",
    );
}

#[test]
fn relation_match_includes_nested_relation_members() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![],
            meta: None,
        }],
        &[
            TestRelation {
                id: 100,
                members: vec![TestMember {
                    id: MemberId::Relation(200),
                    role: "",
                }],
                tags: vec![("type", "route")],
                meta: None,
            },
            TestRelation {
                id: 200,
                members: vec![TestMember {
                    id: MemberId::Way(10),
                    role: "outer",
                }],
                tags: vec![],
                meta: None,
            },
        ],
    );

    run_filter(&input, &output, &["type=route"], false);
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
                members: vec![TestMember {
                    id: MemberId::Relation(200),
                    role: "",
                }],
                tags: vec![("type", "route")],
                meta: None,
            },
            TestRelation {
                id: 200,
                members: vec![TestMember {
                    id: MemberId::Relation(100),
                    role: "",
                }],
                tags: vec![],
                meta: None,
            },
        ],
    );

    run_filter(&input, &output, &["type=route"], false);
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
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![],
            meta: None,
        }],
        &[TestRelation {
            id: 100,
            members: vec![TestMember {
                id: MemberId::Way(10),
                role: "outer",
            }],
            tags: vec![("type", "multipolygon")],
            meta: None,
        }],
    );

    run_filter(&input, &output, &["type=multipolygon"], true);
    let c = read_all_elements(&output);
    assert_eq!(relation_ids(&c), vec![100]);
    assert!(way_ids(&c).is_empty());
    assert!(node_ids(&c).is_empty());
}

// ---------------------------------------------------------------------------
// --invert-match
// ---------------------------------------------------------------------------

#[test]
fn invert_match_excludes_matching_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("amenity", "bench")],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("name", "foo")],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 120_000_000,
                lon: 220_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[],
        &[],
    );

    // Invert: output nodes that do NOT match "amenity"
    run_filter_full(&input, &output, &["amenity"], true, true, false, None);
    let c = read_all_elements(&output);
    // Node 1 has amenity → excluded. Nodes 2 and 3 → included.
    assert_eq!(node_ids(&c), vec![2, 3]);
}

#[test]
fn invert_match_excludes_matching_ways() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![],
                meta: None,
            },
        ],
        &[
            TestWay {
                id: 10,
                refs: vec![1, 2],
                tags: vec![("highway", "primary")],
                meta: None,
            },
            TestWay {
                id: 11,
                refs: vec![1, 2],
                tags: vec![("highway", "secondary")],
                meta: None,
            },
            TestWay {
                id: 12,
                refs: vec![1, 2],
                tags: vec![("building", "yes")],
                meta: None,
            },
        ],
        &[],
    );

    // Invert: output ways that do NOT match "highway"
    run_filter_full(&input, &output, &["highway"], true, true, false, None);
    let c = read_all_elements(&output);
    // Ways 10, 11 have highway → excluded. Way 12 → included.
    assert_eq!(way_ids(&c), vec![12]);
}

// ---------------------------------------------------------------------------
// --remove-tags
// ---------------------------------------------------------------------------

#[test]
fn remove_tags_strips_tags_from_referenced_nodes() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("input.osm.pbf");
    let output = dir.path().join("output.osm.pbf");

    write_test_pbf(
        &input,
        &[
            TestNode {
                id: 1,
                lat: 100_000_000,
                lon: 200_000_000,
                tags: vec![("shop", "bakery")],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 110_000_000,
                lon: 210_000_000,
                tags: vec![("name", "corner")],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "residential")],
            meta: None,
        }],
        &[],
    );

    // Two-pass (omit_referenced=false) with remove_tags: way 10 matches
    // "highway", its referenced nodes 1,2 are pulled in but with tags stripped.
    run_filter_full(&input, &output, &["highway"], false, false, true, None);

    // Use the with-coords reader (the id-only reader doesn't return tags),
    // brought in just for this assertion.
    let c = crate::common::read_all_elements_with_coords(&output);

    // Way 10 should keep its tags (directly matched)
    assert_eq!(c.ways.iter().map(|w| w.0).collect::<Vec<_>>(), vec![10]);
    let way_tags: Vec<_> = c
        .ways
        .iter()
        .find(|(id, _, _)| *id == 10)
        .map(|(_, _, tags)| tags.clone())
        .unwrap_or_default();
    assert!(
        !way_tags.is_empty(),
        "directly matched way should keep tags"
    );

    // Nodes 1,2 should be present but with empty tags (referenced only).
    let node_id_list: Vec<i64> = c.nodes.iter().map(|(id, _, _, _)| *id).collect();
    assert_eq!(node_id_list, vec![1, 2]);
    for (id, _, _, tags) in &c.nodes {
        assert!(
            tags.is_empty(),
            "node {id} should have tags stripped (referenced only)"
        );
    }
}

// ---------------------------------------------------------------------------
// Parallel classify parity: jobs=1 vs jobs=4 across multiple blobs
// ---------------------------------------------------------------------------
//
// Two-pass `tags-filter` routes blob scanning through
// `parallel_classify_phase` and the follow-up relation/way-node
// dependency closures through two `parallel_classify_accumulate`
// calls. `-j N` caps the worker-pool size for all three calls. With
// single-blob fixtures the sharding is trivial; this test forces
// multiple blobs and asserts that jobs=1 and jobs=4 produce the same
// element set and identical stats. The full stderr summary line is
// compared verbatim - that is the user-observable surface for every
// counter the library tracks.

fn run_parity(input: &Path, jobs: usize) -> (PbfContentsIdOnly, String) {
    let dir = TempDir::new().expect("tempdir");
    let output = dir.path().join("output.osm.pbf");
    let stderr = run_filter_full(
        input,
        &output,
        &["w/highway=primary"],
        false,
        false,
        false,
        Some(jobs),
    );
    (read_all_elements(&output), stderr)
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

    let (c_seq, stderr_seq) = run_parity(&input, 1);
    let (c_par, stderr_par) = run_parity(&input, 4);

    // Element sets must match (ids only, order preserved on sorted input).
    assert_eq!(
        node_ids(&c_seq),
        node_ids(&c_par),
        "node id set diverges under -j 4"
    );
    assert_eq!(
        way_ids(&c_seq),
        way_ids(&c_par),
        "way id set diverges under -j 4"
    );
    assert_eq!(
        relation_ids(&c_seq),
        relation_ids(&c_par),
        "relation id set diverges under -j 4"
    );

    // The full summary line must match. This pins every counter
    // (`nodes_matched`, `nodes_from_ways`, `nodes_from_relations`,
    // `ways_matched`, `ways_from_relations`, `relations_matched`,
    // `relations_from_relations`) without parsing each one.
    let line_seq = stderr_seq
        .lines()
        .find(|l| l.starts_with("Wrote "))
        .expect("stats line in -j 1 stderr");
    let line_par = stderr_par
        .lines()
        .find(|l| l.starts_with("Wrote "))
        .expect("stats line in -j 4 stderr");
    assert_eq!(
        line_seq, line_par,
        "TagsFilterStats summary diverges under -j 4"
    );

    // Sanity: the filter must have matched SOME ways (otherwise the
    // test is trivially parity-clean because nothing is emitted).
    assert!(
        !way_ids(&c_seq).is_empty(),
        "filter must match at least one way"
    );
}
