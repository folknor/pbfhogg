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

/// A plain-XML OSC of node creates `first..first + count`; the node at
/// `oversize.0`, if any, carries a tag value of `oversize.1` bytes.
fn write_node_creates(path: &Path, first: i64, count: usize, oversize: Option<(i64, usize)>) {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osmChange version=\"0.6\">\n<create>\n",
    );
    for id in first..first + i64::try_from(count).unwrap() {
        match oversize {
            Some((at, bytes)) if at == id => {
                write!(
                    xml,
                    "<node id=\"{id}\" lat=\"1.0\" lon=\"2.0\" version=\"1\"><tag k=\"note\" v=\""
                )
                .unwrap();
                xml.extend(std::iter::repeat_n('x', bytes));
                xml.push_str("\"/></node>\n");
            }
            _ => writeln!(
                xml,
                "<node id=\"{id}\" lat=\"1.0\" lon=\"2.0\" version=\"1\"/>"
            )
            .unwrap(),
        }
    }
    xml.push_str("</create>\n</osmChange>\n");
    std::fs::write(path, xml).unwrap();
}

/// Merge `osc` into a base of one node blob (ids 1..=50) and one way blob,
/// uncompressed, at the automatic parallelism (more than one worker, so a
/// run past one block is encoded on the executor). `batch_blocks` is the
/// fault plan's create-batch override (0 keeps the production batch).
/// Every create lies past the one base node blob, so the whole run is
/// written at the node-to-way transition, as planet's is.
fn merge_creates(
    dir: &Path,
    osc: &Path,
    output: &Path,
    batch_blocks: usize,
) -> pbfhogg::Result<pbfhogg::apply_changes::MergeStats> {
    let base = dir.join("base.osm.pbf");
    write_multi_block_test_pbf(
        &base,
        &generate_nodes(50, 1),
        &generate_ways(10, 1, 2, 1),
        &[],
        50,
    );
    let opts = pbfhogg::apply_changes::MergeOptions {
        // Plain XML and uncompressed output: the batching is independent
        // of both, and gzip and zlib in a debug build would be most of the
        // run.
        write: pbfhogg::write::WriteOptions {
            compression: pbfhogg::writer::Compression::None,
            ..pbfhogg::write::WriteOptions::default()
        },
        locations_on_ways: None,
        parallelism: pbfhogg::Parallelism::Auto,
        allow_version_regression: false,
    };
    let host = crate::common::test_host();
    #[cfg(pbfhogg_test_hooks)]
    host.invocation
        .faults()
        .apply_changes_create_batch_blocks
        .store(batch_blocks, std::sync::atomic::Ordering::Relaxed);
    #[cfg(not(pbfhogg_test_hooks))]
    assert_eq!(batch_blocks, 0, "a batch override needs test-hooks");
    pbfhogg::apply_changes::merge(
        &base,
        osc,
        crate::common::out(output),
        &opts,
        &pbfhogg::HeaderOverrides::default(),
        &host,
    )
}

/// A run of `full_blocks` full blocks and one element, merged at
/// `batch_blocks`, comes out whole, cut at the cap and in order, its final
/// partial batch included.
fn assert_run_written_whole(full_blocks: usize, batch_blocks: usize) {
    let dir = crate::common::test_scratch();
    let osc = dir.path().join("diff.osc");
    let output = dir.path().join("out.osm.pbf");
    let cap = pbfhogg::block_builder::DEFAULT_ELEMENT_CAP.get();
    let first = 1000_i64;
    let count = full_blocks * cap + 1;
    write_node_creates(&osc, first, count, None);
    let stats = merge_creates(dir.path(), &osc, &output, batch_blocks).unwrap();

    let mut expected_blobs = vec![(50, 0, 0)];
    expected_blobs.extend(std::iter::repeat_n((cap, 0, 0), full_blocks));
    expected_blobs.push((1, 0, 0));
    expected_blobs.push((0, 10, 0));
    assert_eq!(elements_per_blob(&output), expected_blobs);
    let c = read_all_elements_id_only(&output);
    let mut expected_nodes: Vec<i64> = (1..=50).collect();
    expected_nodes.extend(first..first + i64::try_from(count).unwrap());
    assert_eq!(node_ids_id_only(&c), expected_nodes);
    assert_eq!(way_ids_id_only(&c), (1..=10).collect::<Vec<i64>>());
    assert_eq!(stats.diff_nodes, count as u64);
    crate::common::assert_every_data_blob_indexed(&output);
    crate::common::assert_sorted_file(&output);
}

/// Batches of two blocks over three full blocks and one element: two full
/// batches' worth crossed, the last batch partial.
#[cfg(pbfhogg_test_hooks)]
#[test]
fn a_create_run_past_one_executor_batch_is_written_whole_and_in_order() {
    assert_run_written_whole(3, 2);
}

/// The blob payload limit is the block failure real input can reach in a
/// create run: one tag value over the 32 MiB a raw blob payload may hold,
/// which no element cap bounds. In batches of one block it fails block 1,
/// a batch past the first, after block 0 was written; the run fails with
/// that refusal and nothing is published. Which of two failures wins, and
/// what reached the writer first, is `write_run`'s, pinned by its unit
/// tests in `crates/lib/src/commands/apply_changes/stream_output.rs`.
#[cfg(pbfhogg_test_hooks)]
#[test]
fn a_create_block_over_the_blob_limit_fails_the_merge_from_a_later_batch() {
    const OVERSIZE: usize = 33 << 20;
    let dir = crate::common::test_scratch();
    let osc = dir.path().join("diff.osc");
    let output = dir.path().join("out.osm.pbf");
    let cap = pbfhogg::block_builder::DEFAULT_ELEMENT_CAP.get();
    let first = 1000_i64;
    let failing = first + i64::try_from(cap).unwrap();
    write_node_creates(&osc, first, 2 * cap + 1, Some((failing, OVERSIZE)));
    let Err(err) = merge_creates(dir.path(), &osc, &output, 1) else {
        panic!("a create block over the blob payload limit must fail the merge")
    };
    match err.kind() {
        pbfhogg::ErrorKind::BlobTooLarge { size, .. } => {
            assert!(*size >= OVERSIZE as u64, "{size} bytes");
        }
        _ => panic!("expected the oversize block's refusal, got {err}"),
    }
    assert!(!output.exists(), "a failed merge published its output");
}

// The unit batch guard in `stream_output.rs` reads this same fixture
// definition, so the tier-3 run keeps crossing the production batch.
#[path = "create_run_fixture.rs"]
mod create_run_fixture;

/// The production batch, at a size it acts on: tier 3, run by the full
/// profile and the gate.
mod tier3 {
    use super::create_run_fixture::BOUNDARY_RUN_FULL_BLOCKS;

    /// `BOUNDARY_RUN_FULL_BLOCKS` full blocks and one element cross the
    /// production batch: a take-one mutation of the batch loop once dropped
    /// every create past the first batch with every smaller test green.
    #[test]
    fn a_create_run_past_the_production_batch_is_written_whole_and_in_order() {
        super::assert_run_written_whole(BOUNDARY_RUN_FULL_BLOCKS, 0);
    }
}
