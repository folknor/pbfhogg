//! apply-changes' create runs: the creates the drain writes between base
//! blobs (gap creates), at a type transition (a kind's creates past its last
//! blob) and at end of input. A run longer than one block is encoded and
//! framed in parallel; its blocks must be cut where a single builder filling
//! to the element cap would cut them, and written in id order, on the
//! indexed and the forced unindexed route alike. The executor takes a long
//! run in batches of blocks; a run past one batch must still come out
//! whole, its final partial batch included. A block that fails fails the
//! run with the earliest failing block's error, and nothing is published.

#![allow(clippy::unwrap_used)]

use std::fmt::Write as _;
use std::fs::File;
use std::io::Write as _;
use std::path::Path;

use crate::common::{
    generate_nodes, generate_relations, generate_ways, node_ids_id_only, read_all_elements_id_only,
    relation_ids_id_only, way_ids_id_only, write_multi_block_non_indexed_pbf,
    write_multi_block_test_pbf,
};
use pbfhogg::{BlobDecode, BlobReader, Element};

const GAP_NODES: std::ops::RangeInclusive<i64> = 51..=99;
const TAIL_NODES: std::ops::RangeInclusive<i64> = 1000..=21_000;
const TAIL_WAYS: std::ops::RangeInclusive<i64> = 100..=9_099;
const TAIL_RELATIONS: std::ops::RangeInclusive<i64> = 100..=10_099;

fn write_osc(path: &Path) {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osmChange version=\"0.6\">\n<create>\n",
    );
    for id in GAP_NODES.chain(TAIL_NODES) {
        writeln!(
            xml,
            "<node id=\"{id}\" lat=\"1.0\" lon=\"2.0\" version=\"1\"><tag k=\"name\" v=\"n{id}\"/></node>"
        )
        .unwrap();
    }
    for id in TAIL_WAYS {
        writeln!(
            xml,
            "<way id=\"{id}\" version=\"1\"><nd ref=\"1\"/><nd ref=\"2\"/><tag k=\"highway\" v=\"w{}\"/></way>",
            id % 7
        )
        .unwrap();
    }
    for id in TAIL_RELATIONS {
        writeln!(
            xml,
            "<relation id=\"{id}\" version=\"1\"><member type=\"way\" ref=\"1\" role=\"outer\"/><tag k=\"type\" v=\"multipolygon\"/></relation>"
        )
        .unwrap();
    }
    xml.push_str("</create>\n</osmChange>\n");
    let file = File::create(path).unwrap();
    let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    enc.write_all(xml.as_bytes()).unwrap();
    enc.finish().unwrap();
}

/// `(nodes, ways, relations)` per data blob, in file order.
fn elements_per_blob(path: &Path) -> Vec<(usize, usize, usize)> {
    let mut per_blob = Vec::new();
    for blob in BlobReader::from_path(path).unwrap() {
        if let BlobDecode::OsmData(block) = blob.unwrap().decode().unwrap() {
            let mut counts = (0, 0, 0);
            for element in block.elements() {
                match element {
                    Element::Node(_) | Element::DenseNode(_) => counts.0 += 1,
                    Element::Way(_) => counts.1 += 1,
                    Element::Relation(_) => counts.2 += 1,
                    _ => {}
                }
            }
            per_blob.push(counts);
        }
    }
    per_blob
}

#[test]
fn create_runs_are_cut_at_the_element_cap_and_written_in_order() {
    let dir = crate::common::test_scratch();
    let osc = dir.path().join("diff.osc.gz");
    write_osc(&osc);

    // Two node blobs (1..=50, 101..=150) with room for gap creates between
    // them, one way blob and one relation blob.
    let mut nodes = generate_nodes(50, 1);
    nodes.extend(generate_nodes(50, 101));
    let ways = generate_ways(10, 1, 2, 1);
    let relations = generate_relations(3, 1, 1, 1);

    let expected_blobs = vec![
        (50, 0, 0),
        (GAP_NODES.count(), 0, 0),
        (50, 0, 0),
        (8000, 0, 0),
        (8000, 0, 0),
        (4001, 0, 0),
        (0, 10, 0),
        (0, 8000, 0),
        (0, 1000, 0),
        (0, 0, 3),
        (0, 0, 8000),
        (0, 0, 2000),
    ];
    let mut expected_nodes: Vec<i64> = (1..=50).collect();
    expected_nodes.extend(GAP_NODES);
    expected_nodes.extend(101..=150);
    expected_nodes.extend(TAIL_NODES);
    let mut expected_ways: Vec<i64> = (1..=10).collect();
    expected_ways.extend(TAIL_WAYS);
    let mut expected_relations: Vec<i64> = (1..=3).collect();
    expected_relations.extend(TAIL_RELATIONS);

    // A single worker (`-j 1`) encodes every create block on the drain
    // thread; the automatic sizing encodes long runs on the executor.
    let one = pbfhogg::Parallelism::Fixed(std::num::NonZeroUsize::MIN);
    for (indexed, parallelism) in [
        (true, pbfhogg::Parallelism::Auto),
        (false, pbfhogg::Parallelism::Auto),
        (true, one),
    ] {
        let label = format!("indexed={indexed} parallelism={parallelism:?}");
        let base = dir.path().join(format!("base-{indexed}.osm.pbf"));
        let output = dir
            .path()
            .join(format!("out-{indexed}-{parallelism:?}.osm.pbf"));
        if indexed {
            write_multi_block_test_pbf(&base, &nodes, &ways, &relations, 50);
        } else {
            write_multi_block_non_indexed_pbf(&base, &nodes, &ways, &relations, 50);
        }
        let opts = pbfhogg::apply_changes::MergeOptions {
            write: pbfhogg::write::WriteOptions {
                force: !indexed,
                ..pbfhogg::write::WriteOptions::default()
            },
            locations_on_ways: None,
            parallelism,
            allow_version_regression: false,
        };
        let stats = pbfhogg::apply_changes::merge(
            &base,
            &osc,
            crate::common::out(&output),
            &opts,
            &pbfhogg::HeaderOverrides::default(),
            &crate::common::test_host(),
        )
        .unwrap();

        assert_eq!(elements_per_blob(&output), expected_blobs, "{label}");
        let c = read_all_elements_id_only(&output);
        assert_eq!(node_ids_id_only(&c), expected_nodes, "{label}");
        assert_eq!(way_ids_id_only(&c), expected_ways, "{label}");
        assert_eq!(relation_ids_id_only(&c), expected_relations, "{label}");
        assert_eq!(
            stats.diff_nodes,
            (GAP_NODES.count() + TAIL_NODES.count()) as u64,
            "{label}"
        );
        assert_eq!(stats.diff_ways, TAIL_WAYS.count() as u64, "{label}");
        assert_eq!(
            stats.diff_relations,
            TAIL_RELATIONS.count() as u64,
            "{label}"
        );
        crate::common::assert_every_data_blob_indexed(&output);
        crate::common::assert_sorted_file(&output);
    }
}

/// Full blocks in the run that crosses the batch boundary. The executor
/// encodes a long run in batches of `APPLY_CHANGES_CREATE_BATCH_BLOCKS`
/// (64) blocks, so 65 full blocks and a one-element block make a full first
/// batch and a second, final batch of one full and one partial block. The
/// constant is private to the crate, so this test cannot read it; raised to
/// 66 or more it would hold this whole run in one batch and the test would
/// stop crossing the boundary. A unit test in
/// `crates/lib/src/commands/apply_changes/stream_output.rs` fails first in that case.
const BOUNDARY_RUN_FULL_BLOCKS: usize = 65;

#[test]
fn a_create_run_past_one_executor_batch_is_written_whole_and_in_order() {
    let dir = crate::common::test_scratch();
    // Plain XML and uncompressed output: the batching is independent of
    // both, and gzip and zlib in a debug build would be most of the run.
    let osc = dir.path().join("diff.osc");
    let cap = pbfhogg::block_builder::DEFAULT_ELEMENT_CAP.get();
    let first = 1000_i64;
    let count = BOUNDARY_RUN_FULL_BLOCKS * cap + 1;
    let creates = first..first + i64::try_from(count).unwrap();
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osmChange version=\"0.6\">\n<create>\n",
    );
    for id in creates.clone() {
        writeln!(
            xml,
            "<node id=\"{id}\" lat=\"1.0\" lon=\"2.0\" version=\"1\"/>"
        )
        .unwrap();
    }
    xml.push_str("</create>\n</osmChange>\n");
    std::fs::write(&osc, xml).unwrap();

    // Every create lies past the one base node blob, so the whole run is
    // written at the node-to-way transition, as planet's is.
    let base = dir.path().join("base.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    write_multi_block_test_pbf(
        &base,
        &generate_nodes(50, 1),
        &generate_ways(10, 1, 2, 1),
        &[],
        50,
    );
    let opts = pbfhogg::apply_changes::MergeOptions {
        write: pbfhogg::write::WriteOptions {
            compression: pbfhogg::writer::Compression::None,
            ..pbfhogg::write::WriteOptions::default()
        },
        locations_on_ways: None,
        // The automatic sizing gives more than one worker, so the run is
        // encoded on the executor, in batches.
        parallelism: pbfhogg::Parallelism::Auto,
        allow_version_regression: false,
    };
    let stats = pbfhogg::apply_changes::merge(
        &base,
        &osc,
        crate::common::out(&output),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    )
    .unwrap();

    let mut expected_blobs = vec![(50, 0, 0)];
    expected_blobs.extend(std::iter::repeat_n((cap, 0, 0), BOUNDARY_RUN_FULL_BLOCKS));
    expected_blobs.push((1, 0, 0));
    expected_blobs.push((0, 10, 0));
    assert_eq!(elements_per_blob(&output), expected_blobs);
    let c = read_all_elements_id_only(&output);
    let mut expected_nodes: Vec<i64> = (1..=50).collect();
    expected_nodes.extend(creates);
    assert_eq!(node_ids_id_only(&c), expected_nodes);
    assert_eq!(way_ids_id_only(&c), (1..=10).collect::<Vec<i64>>());
    assert_eq!(stats.diff_nodes, count as u64);
    crate::common::assert_every_data_blob_indexed(&output);
    crate::common::assert_sorted_file(&output);
}

/// Bytes of the tag value that fails each of the two failing blocks: the
/// earlier failing block's larger, so its encoding tends to finish last
/// and the reported size says which block's error came back. Both exceed
/// the 32 MiB a raw blob payload may hold, which no element cap bounds.
const EARLIER_OVERSIZE: usize = 40 << 20;
const LATER_OVERSIZE: usize = 33 << 20;

/// A create run whose blocks fail on real input, in a batch past the first:
/// after `BOUNDARY_RUN_FULL_BLOCKS` clean full blocks, block
/// `BOUNDARY_RUN_FULL_BLOCKS` carries a node with a tag value over the blob
/// payload limit and the one-element block after it another, smaller one.
/// The earlier block's refusal is the one returned and nothing is
/// published. Which blocks reached the writer first is not visible from
/// here (a failed merge publishes nothing); the unit tests on `write_run` in
/// `crates/lib/src/commands/apply_changes/stream_output.rs` pin that, and the order the
/// failures complete in. A unit test there also checks that these two
/// blocks share a batch that is not the first.
#[test]
fn a_create_run_failing_in_a_later_batch_returns_the_earliest_failure() {
    let dir = crate::common::test_scratch();
    let osc = dir.path().join("diff.osc");
    let cap = pbfhogg::block_builder::DEFAULT_ELEMENT_CAP.get();
    let first = 1000_i64;
    let clean = BOUNDARY_RUN_FULL_BLOCKS * cap;
    let count = clean + cap + 1;
    let creates = first..first + i64::try_from(count).unwrap();
    let earlier = first + i64::try_from(clean).unwrap();
    let later = creates.end - 1;
    let mut xml = String::with_capacity(EARLIER_OVERSIZE + LATER_OVERSIZE + count * 64);
    xml.push_str(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osmChange version=\"0.6\">\n<create>\n",
    );
    for id in creates {
        let oversize = if id == earlier {
            EARLIER_OVERSIZE
        } else if id == later {
            LATER_OVERSIZE
        } else {
            0
        };
        if oversize == 0 {
            writeln!(
                xml,
                "<node id=\"{id}\" lat=\"1.0\" lon=\"2.0\" version=\"1\"/>"
            )
            .unwrap();
        } else {
            write!(
                xml,
                "<node id=\"{id}\" lat=\"1.0\" lon=\"2.0\" version=\"1\"><tag k=\"note\" v=\""
            )
            .unwrap();
            xml.extend(std::iter::repeat_n('x', oversize));
            xml.push_str("\"/></node>\n");
        }
    }
    xml.push_str("</create>\n</osmChange>\n");
    std::fs::write(&osc, xml).unwrap();

    let base = dir.path().join("base.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    write_multi_block_test_pbf(
        &base,
        &generate_nodes(50, 1),
        &generate_ways(10, 1, 2, 1),
        &[],
        50,
    );
    let opts = pbfhogg::apply_changes::MergeOptions {
        write: pbfhogg::write::WriteOptions {
            compression: pbfhogg::writer::Compression::None,
            ..pbfhogg::write::WriteOptions::default()
        },
        locations_on_ways: None,
        parallelism: pbfhogg::Parallelism::Auto,
        allow_version_regression: false,
    };
    let Err(err) = pbfhogg::apply_changes::merge(
        &base,
        &osc,
        crate::common::out(&output),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    ) else {
        panic!("a create block over the blob payload limit must fail the merge")
    };
    match err.kind() {
        pbfhogg::ErrorKind::BlobTooLarge { size, .. } => assert!(
            *size >= EARLIER_OVERSIZE as u64,
            "the later failing block's refusal ({size} bytes) came back, not the earlier one's"
        ),
        _ => panic!("expected the oversize block's refusal, got {err}"),
    }
    assert!(!output.exists(), "a failed merge published its output");
}
