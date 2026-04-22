//! Tests for reading path equivalence: pipeline, par_map_reduce, and seek operations.
//!
//! Verifies that all reading modes produce identical results and that seek
//! operations work correctly on BlobReader.
#![allow(clippy::unwrap_used, clippy::cognitive_complexity, clippy::too_many_lines)]

mod common;

use std::io::SeekFrom;
use std::path::Path;

use pbfhogg::block_builder::{self, BlockBuilder, MemberData};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{
    BlobFilter, BlobReader, BlobType, ByteOffset, Element, ElementReader, IndexedReader, MemberId,
};
use tempfile::TempDir;

/// Write a multi-block PBF to the given path.
/// Contains: header + 3 data blocks (3 nodes, 2 ways, 1 relation).
fn write_test_pbf(path: &Path) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(file, Compression::default());

    let header =
        block_builder::HeaderBuilder::new().bbox(9.0, 54.0, 13.0, 58.0).build()
            .unwrap();
    writer.write_header(&header).unwrap();

    let mut bb = BlockBuilder::new();

    // Block 1: 3 nodes
    bb.add_node(100, 550_000_000, 120_000_000, [("name", "A")], None);
    bb.add_node(200, 560_000_000, 130_000_000, [("name", "B")], None);
    bb.add_node(300, -330_000_000, -580_000_000, std::iter::empty::<(&str, &str)>(), None);
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();

    // Block 2: 2 ways
    bb.add_way(1000, [("highway", "primary")], &[100, 200, 300], None);
    bb.add_way(2000, [("building", "yes")], &[200, 300, 200], None);
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();

    // Block 3: 1 relation
    bb.add_relation(
        5000,
        [("type", "multipolygon")],
        &[MemberData {
            id: MemberId::Way(1000),
            role: "outer",
        }],
        None,
    );
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();

    writer.flush().unwrap();
}

/// Extract (type_char, id) from an element.
fn element_id(element: &Element<'_>) -> (char, i64) {
    match element {
        Element::Node(n) => ('n', n.id()),
        Element::DenseNode(dn) => ('n', dn.id()),
        Element::Way(w) => ('w', w.id()),
        Element::Relation(r) => ('r', r.id()),
        _ => ('?', 0),
    }
}

/// Collect all element IDs using sequential for_each.
fn collect_sequential(path: &Path) -> Vec<(char, i64)> {
    let mut result = Vec::new();
    let reader = ElementReader::from_path(path).unwrap();
    reader
        .for_each(|element| {
            result.push(element_id(&element));
        })
        .unwrap();
    result
}

// ---------------------------------------------------------------------------
// Pipeline tests (via ElementReader::for_each_pipelined)
// ---------------------------------------------------------------------------

/// Pipelined reading produces elements in the same order as sequential reading.
#[test]
fn pipelined_matches_sequential() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let sequential = collect_sequential(&path);

    let mut pipelined = Vec::new();
    let reader = ElementReader::from_path(&path).unwrap();
    reader
        .for_each_pipelined(|element| {
            pipelined.push(element_id(&element));
        })
        .unwrap();

    assert_eq!(sequential, pipelined);
}

/// into_blocks_pipelined yields the same elements as for_each_pipelined.
#[test]
fn block_iterator_matches_pipelined() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let sequential = collect_sequential(&path);

    let mut from_iter = Vec::new();
    let reader = ElementReader::from_path(&path).unwrap();
    for block_result in reader.into_blocks_pipelined() {
        let block = block_result.unwrap();
        for element in block.elements() {
            from_iter.push(element_id(&element));
        }
    }

    assert_eq!(sequential, from_iter);
}

/// into_blocks_pipelined handles early drop without hanging.
#[test]
fn block_iterator_early_drop() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let reader = ElementReader::from_path(&path).unwrap();
    let mut blocks = reader.into_blocks_pipelined();
    // Take just the first block and drop the iterator
    let _first = blocks.next();
    drop(blocks);
    // If we get here without hanging, the test passes.
}

/// block_type() correctly classifies each block in a sorted PBF.
#[test]
fn block_type_classification() {
    use pbfhogg::BlockType;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let reader = ElementReader::from_path(&path).unwrap();
    let mut types = Vec::new();
    for block_result in reader.into_blocks_pipelined() {
        let block = block_result.unwrap();
        types.push(block.block_type());
    }

    // write_test_pbf creates 3 blocks: dense nodes, ways, relations
    assert_eq!(types, vec![BlockType::DenseNodes, BlockType::Ways, BlockType::Relations]);

    // Convenience methods
    assert!(BlockType::DenseNodes.is_nodes());
    assert!(BlockType::Nodes.is_nodes());
    assert!(!BlockType::Ways.is_nodes());
    assert!(BlockType::Ways.is_ways());
    assert!(BlockType::Relations.is_relations());
    assert!(!BlockType::Mixed.is_nodes());
}

// ---------------------------------------------------------------------------
// par_map_reduce tests
// ---------------------------------------------------------------------------

/// par_map_reduce counts match sequential counts.
#[test]
fn par_map_reduce_count() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let sequential = collect_sequential(&path);
    let expected_nodes = sequential.iter().filter(|(t, _)| *t == 'n').count() as u64;
    let expected_ways = sequential.iter().filter(|(t, _)| *t == 'w').count() as u64;
    let expected_relations = sequential.iter().filter(|(t, _)| *t == 'r').count() as u64;

    let reader = ElementReader::from_path(&path).unwrap();
    let (nodes, ways, relations) = reader
        .par_map_reduce(
            |element| match element {
                Element::Node(_) | Element::DenseNode(_) => (1u64, 0u64, 0u64),
                Element::Way(_) => (0, 1, 0),
                Element::Relation(_) => (0, 0, 1),
                _ => (0, 0, 0),
            },
            || (0, 0, 0),
            |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2),
        )
        .unwrap();

    assert_eq!(nodes, expected_nodes);
    assert_eq!(ways, expected_ways);
    assert_eq!(relations, expected_relations);
}

/// par_map_reduce collects the same set of element IDs as sequential (order may differ).
#[test]
fn par_map_reduce_collect_ids() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let mut expected = collect_sequential(&path);
    expected.sort();

    let reader = ElementReader::from_path(&path).unwrap();
    let mut actual: Vec<(char, i64)> = reader
        .par_map_reduce(
            |element| vec![element_id(&element)],
            Vec::new,
            |mut a, b| {
                a.extend(b);
                a
            },
        )
        .unwrap();
    actual.sort();

    assert_eq!(expected, actual);
}

// ---------------------------------------------------------------------------
// BlobReader seek tests
// ---------------------------------------------------------------------------

/// Seeking back to the start re-reads the first blob.
#[test]
fn blobreader_seek_to_start() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let mut reader = BlobReader::seekable_from_path(&path).unwrap();
    let first = reader.next().unwrap().unwrap();
    assert_eq!(first.get_type(), BlobType::OsmHeader);
    assert_eq!(first.offset(), Some(ByteOffset(0)));

    // Seek back to start
    reader.seek(ByteOffset(0)).unwrap();
    let first_again = reader.next().unwrap().unwrap();
    assert_eq!(first_again.get_type(), BlobType::OsmHeader);
    assert_eq!(first_again.offset(), Some(ByteOffset(0)));
}

/// blob_from_offset can random-access any blob by its recorded offset.
#[test]
fn blobreader_blob_from_offset() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    // First pass: collect all blob types (as strings) and offsets
    let mut reader = BlobReader::seekable_from_path(&path).unwrap();
    let mut blobs_info: Vec<(String, ByteOffset)> = Vec::new();
    for blob in reader.by_ref() {
        let blob = blob.unwrap();
        blobs_info.push((
            blob.get_type().as_str().to_string(),
            blob.offset().unwrap(),
        ));
    }

    // Random access each blob by its offset
    for (expected_type, offset) in &blobs_info {
        let blob = reader.blob_from_offset(*offset).unwrap();
        assert_eq!(blob.get_type().as_str(), expected_type.as_str());
        assert_eq!(blob.offset(), Some(*offset));
    }
}

/// seek_raw with SeekFrom::Start(0) restarts; SeekFrom::End(0) reaches EOF.
#[test]
fn blobreader_seek_raw() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let mut reader = BlobReader::seekable_from_path(&path).unwrap();

    // Read first blob
    let _ = reader.next().unwrap().unwrap();

    // Seek back to start
    let pos = reader.seek_raw(SeekFrom::Start(0)).unwrap();
    assert_eq!(pos, 0);
    let blob = reader.next().unwrap().unwrap();
    assert_eq!(blob.get_type(), BlobType::OsmHeader);

    // Seek to end - next should return None (clean EOF)
    let end_pos = reader.seek_raw(SeekFrom::End(0)).unwrap();
    assert!(end_pos > 0);
    assert!(reader.next().is_none());
}

/// next_header_skip_blob scans all headers without decoding blob content.
#[test]
fn blobreader_next_header_skip_blob() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    // Normal iteration: collect types (as strings) and offsets
    let reader = BlobReader::from_path(&path).unwrap();
    let mut expected: Vec<(String, Option<ByteOffset>)> = Vec::new();
    for blob in reader {
        let blob = blob.unwrap();
        expected.push((blob.get_type().as_str().to_string(), blob.offset()));
    }

    // Header-skip iteration: should match types and offsets without decoding
    let mut reader = BlobReader::seekable_from_path(&path).unwrap();
    let mut actual: Vec<(String, Option<ByteOffset>)> = Vec::new();
    while let Some(result) = reader.next_header_skip_blob() {
        let (header, offset) = result.unwrap();
        actual.push((header.blob_type().as_str().to_string(), offset));
    }

    assert_eq!(expected.len(), actual.len());
    for (e, a) in expected.iter().zip(actual.iter()) {
        assert_eq!(e.0, a.0, "blob types must match");
        assert_eq!(e.1, a.1, "offsets must match");
    }
}

// ---------------------------------------------------------------------------
// Header accessor tests
// ---------------------------------------------------------------------------

/// Write a PBF with Sort.Type_then_ID and verify header().is_sorted().
fn write_sorted_pbf(path: &Path) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(file, Compression::default());

    let header = block_builder::HeaderBuilder::new()
        .bbox(9.0, 54.0, 13.0, 58.0)
        .sorted()
        .build()
        .unwrap();
    writer.write_header(&header).unwrap();

    let mut bb = BlockBuilder::new();
    bb.add_node(1, 550_000_000, 120_000_000, std::iter::empty::<(&str, &str)>(), None);
    bb.add_node(2, 560_000_000, 130_000_000, std::iter::empty::<(&str, &str)>(), None);
    bb.add_node(3, 570_000_000, 140_000_000, std::iter::empty::<(&str, &str)>(), None);
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();

    writer.flush().unwrap();
}

/// ElementReader exposes the parsed header via header().
#[test]
fn header_accessor() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let reader = ElementReader::from_path(&path).unwrap();
    let header = reader.header();

    // write_test_pbf sets bbox to (9.0, 54.0, 13.0, 58.0)
    let bbox = header.bbox().unwrap();
    assert!((bbox.left - 9.0).abs() < 1e-6);
    assert!((bbox.bottom - 54.0).abs() < 1e-6);

    // writing_program is "pbfhogg"
    assert_eq!(header.writing_program(), Some("pbfhogg"));
}

/// header().is_sorted() returns true when Sort.Type_then_ID is set.
#[test]
fn header_is_sorted_true() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("sorted.osm.pbf");
    write_sorted_pbf(&path);

    let reader = ElementReader::from_path(&path).unwrap();
    assert!(reader.header().is_sorted());
}

/// header().is_sorted() returns false when Sort.Type_then_ID is absent.
#[test]
fn header_is_sorted_false() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("unsorted.osm.pbf");
    write_test_pbf(&path);

    let reader = ElementReader::from_path(&path).unwrap();
    assert!(!reader.header().is_sorted());
}

/// Elements are still delivered correctly after header is consumed at construction.
#[test]
fn header_consumed_elements_still_work() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_sorted_pbf(&path);

    let reader = ElementReader::from_path(&path).unwrap();
    assert!(reader.header().is_sorted());

    let mut count = 0u64;
    reader
        .for_each(|_element| {
            count += 1;
        })
        .unwrap();

    assert_eq!(count, 3); // 3 nodes from write_sorted_pbf
}

/// Sorted PBF iterates without assertion failure (nodes in ascending ID order).
#[test]
fn sorted_pbf_no_assertion_failure() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("sorted.osm.pbf");
    write_sorted_pbf(&path);

    // for_each path
    let reader = ElementReader::from_path(&path).unwrap();
    reader.for_each(|_| {}).unwrap();

    // for_each_pipelined path
    let reader = ElementReader::from_path(&path).unwrap();
    reader.for_each_pipelined(|_| {}).unwrap();
}

/// Debug assertion fires on unsorted nodes when Sort.Type_then_ID is declared.
///
/// Requires `debug_assertions` to be enabled in the test profile.
/// Nightly 1.95 (2026-02-25) has a regression where `debug_assertions` is off
/// in test builds, so this test is ignored until the regression is fixed.
#[test]
#[ignore]
#[should_panic(expected = "Sort.Type_then_ID violated")]
fn sorted_flag_but_unsorted_nodes_panics() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("liar.osm.pbf");

    // Write a PBF that declares Sort.Type_then_ID but has nodes out of order
    let file = std::fs::File::create(&path).unwrap();
    let mut writer = PbfWriter::new(file, Compression::default());

    let header = block_builder::HeaderBuilder::new()
        .sorted()
        .build()
        .unwrap();
    writer.write_header(&header).unwrap();

    let mut bb = BlockBuilder::new();
    bb.add_node(100, 550_000_000, 120_000_000, std::iter::empty::<(&str, &str)>(), None);
    bb.add_node(50, 560_000_000, 130_000_000, std::iter::empty::<(&str, &str)>(), None); // out of order!
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();

    writer.flush().unwrap();

    let reader = ElementReader::from_path(&path).unwrap();
    reader.for_each(|_| {}).unwrap();
}

// ---------------------------------------------------------------------------
// BlobFilter conservative pass-through on non-indexed PBFs
// ---------------------------------------------------------------------------
//
// `should_skip_blob` in `src/read/pipeline.rs:20-33` short-circuits to
// `false` (do not skip) when `blob.index()` is `None`. The doc comment
// calls this out: "Blobs without indexdata or tagdata always pass
// through (conservative)." The consequence is that a filter like
// `BlobFilter::only_ways()` skips node blobs on an indexed PBF but
// does NOT on a non-indexed one - every blob is decompressed and every
// element is delivered to the caller's closure.
//
// The element-level delivery path does not apply any element-type
// filter downstream of the pipeline, so on non-indexed input an
// only_ways filter will silently hand the caller every element type.
// That's the contract these tests pin.

#[test]
fn blobfilter_only_ways_skips_node_blobs_on_indexed_input() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("indexed.osm.pbf");
    common::write_test_pbf_sorted(
        &path,
        &common::generate_nodes(10, 1),
        &common::generate_ways(5, 1_000, 2, 1),
        &[],
    );
    common::assert_indexed(&path);

    let reader = ElementReader::from_path(&path)
        .unwrap()
        .with_blob_filter(BlobFilter::only_ways());

    let mut saw_nodes = 0u64;
    let mut saw_ways = 0u64;
    reader
        .for_each_pipelined(|element| match element {
            Element::Node(_) | Element::DenseNode(_) => saw_nodes += 1,
            Element::Way(_) => saw_ways += 1,
            _ => {}
        })
        .unwrap();

    assert_eq!(saw_nodes, 0, "only_ways filter must skip node blobs on indexed input");
    assert_eq!(saw_ways, 5, "only_ways filter must deliver all ways on indexed input");
}

#[test]
fn blobfilter_only_ways_passes_through_on_non_indexed_input() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("non_indexed.osm.pbf");
    common::write_test_pbf_non_indexed(
        &path,
        &common::generate_nodes(10, 1),
        &common::generate_ways(5, 1_000, 2, 1),
        &[],
    );
    common::assert_non_indexed(&path);

    let reader = ElementReader::from_path(&path)
        .unwrap()
        .with_blob_filter(BlobFilter::only_ways());

    let mut saw_nodes = 0u64;
    let mut saw_ways = 0u64;
    reader
        .for_each_pipelined(|element| match element {
            Element::Node(_) | Element::DenseNode(_) => saw_nodes += 1,
            Element::Way(_) => saw_ways += 1,
            _ => {}
        })
        .unwrap();

    // Node blobs are NOT skipped because the filter's blob-level
    // decision requires indexdata. All 10 nodes reach the closure.
    assert_eq!(
        saw_nodes, 10,
        "BlobFilter on non-indexed input must NOT drop node blobs - callers get every element"
    );
    assert_eq!(saw_ways, 5, "ways still delivered");
}

// ---------------------------------------------------------------------------
// IndexedReader on non-indexed input
// ---------------------------------------------------------------------------
//
// `IndexedReader::create_index` walks only blob headers (not bodies),
// so it does not itself depend on `BlobHeader.indexdata`. The per-blob
// `id_ranges` used by `ways_available` / `node_range_included` are
// populated lazily from decoded blocks via `update_element_id_ranges`
// (src/read/indexed.rs:184) - the same code path runs whether or not
// the input carries indexdata. This test pins that contract: the
// output of `read_ways_and_deps` on a non-indexed PBF must match the
// output on its indexed twin.

#[test]
fn indexed_reader_output_matches_on_indexed_and_non_indexed_twins() {
    let dir = TempDir::new().unwrap();
    let indexed = dir.path().join("indexed.osm.pbf");
    let non_indexed = dir.path().join("non_indexed.osm.pbf");

    // 8 nodes + 4 ways; each way refs two consecutive nodes. "building"
    // tag on odd-numbered ways so read_ways_and_deps has a meaningful
    // filter and node-dependency resolution.
    let nodes = common::generate_nodes(8, 1);
    let mut ways = common::generate_ways(4, 1_000, 2, 1);
    for (i, w) in ways.iter_mut().enumerate() {
        if i % 2 == 0 {
            w.tags = vec![("building", "yes")];
        }
    }

    common::write_test_pbf_sorted(&indexed, &nodes, &ways, &[]);
    common::write_test_pbf_non_indexed(&non_indexed, &nodes, &ways, &[]);
    common::assert_indexed(&indexed);
    common::assert_non_indexed(&non_indexed);

    let collect = |path: &Path| -> (Vec<i64>, Vec<i64>) {
        let mut reader = IndexedReader::from_path(path).unwrap();
        let mut way_ids = Vec::new();
        let mut node_ids = Vec::new();
        reader
            .read_ways_and_deps(
                |w| w.tags().any(|(k, v)| k == "building" && v == "yes"),
                |element| match element {
                    Element::Way(w) => way_ids.push(w.id()),
                    Element::Node(n) => node_ids.push(n.id()),
                    Element::DenseNode(n) => node_ids.push(n.id()),
                    _ => {}
                },
            )
            .unwrap();
        way_ids.sort_unstable();
        node_ids.sort_unstable();
        (way_ids, node_ids)
    };

    let (ways_idx, nodes_idx) = collect(&indexed);
    let (ways_non, nodes_non) = collect(&non_indexed);

    assert_eq!(ways_idx, ways_non, "way set diverges on non-indexed input");
    assert_eq!(nodes_idx, nodes_non, "node dep set diverges on non-indexed input");
    assert!(!ways_idx.is_empty(), "filter must match at least one way");
}
