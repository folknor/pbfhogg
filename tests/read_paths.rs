//! Tests for reading path equivalence: mmap, pipeline, par_map_reduce, and seek operations.
//!
//! Verifies that all reading modes produce identical results and that seek
//! operations work correctly on both BlobReader and MmapBlobReader.
#![allow(clippy::unwrap_used, clippy::cognitive_complexity, clippy::too_many_lines)]

use std::io::SeekFrom;
use std::path::Path;

use pbfhogg::block_builder::{self, BlockBuilder, MemberData};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{
    BlobDecode, BlobReader, BlobType, ByteOffset, Element, ElementReader, Mmap, MemberId,
    MmapBlobReader,
};
use tempfile::TempDir;

/// Write a multi-block PBF to the given path.
/// Contains: header + 3 data blocks (3 nodes, 2 ways, 1 relation).
fn write_test_pbf(path: &Path) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(file, Compression::default());

    let header =
        block_builder::build_header(Some((9.0, 54.0, 13.0, 58.0)), None, None, None, &[])
            .unwrap();
    writer.write_header(&header).unwrap();

    let mut bb = BlockBuilder::new();

    // Block 1: 3 nodes
    bb.add_node(100, 550_000_000, 120_000_000, &[("name", "A")], None);
    bb.add_node(200, 560_000_000, 130_000_000, &[("name", "B")], None);
    bb.add_node(300, -330_000_000, -580_000_000, &[], None);
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();

    // Block 2: 2 ways
    bb.add_way(1000, &[("highway", "primary")], &[100, 200, 300], None);
    bb.add_way(2000, &[("building", "yes")], &[200, 300, 200], None);
    writer
        .write_primitive_block(bb.take().unwrap().unwrap())
        .unwrap();

    // Block 3: 1 relation
    bb.add_relation(
        5000,
        &[("type", "multipolygon")],
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
// MmapBlobReader tests
// ---------------------------------------------------------------------------

/// MmapBlobReader produces the same elements as BlobReader for the same file.
#[test]
fn mmap_matches_blobreader() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    // Collect via BlobReader
    let blob_elements: Vec<(char, i64)> = {
        let mut result = Vec::new();
        let reader = BlobReader::from_path(&path).unwrap();
        for blob in reader {
            let blob = blob.unwrap();
            if let BlobDecode::OsmData(block) = blob.decode().unwrap() {
                for element in block.elements() {
                    result.push(element_id(&element));
                }
            }
        }
        result
    };

    // Collect via MmapBlobReader
    let mmap_elements: Vec<(char, i64)> = {
        let mut result = Vec::new();
        let mmap = unsafe { Mmap::from_path(&path).unwrap() };
        let reader = MmapBlobReader::new(mmap);
        for blob in reader {
            let blob = blob.unwrap();
            if let BlobDecode::OsmData(block) = blob.decode().unwrap() {
                for element in block.elements() {
                    result.push(element_id(&element));
                }
            }
        }
        result
    };

    assert_eq!(blob_elements, mmap_elements);
    assert_eq!(blob_elements.len(), 6); // 3 nodes + 2 ways + 1 relation
}

/// MmapBlobReader reports correct blob types and monotonically increasing offsets.
#[test]
fn mmap_blob_types_and_offsets() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let mmap = unsafe { Mmap::from_path(&path).unwrap() };
    let reader = MmapBlobReader::new(mmap);

    let blobs: Vec<_> = reader.map(|b| b.unwrap()).collect();

    // 4 blobs: 1 header + 3 data
    assert_eq!(blobs.len(), 4);
    assert_eq!(blobs[0].get_type(), BlobType::OsmHeader);
    for blob in &blobs[1..] {
        assert_eq!(blob.get_type(), BlobType::OsmData);
    }

    // Offsets start at 0 and increase
    assert_eq!(blobs[0].offset(), ByteOffset(0));
    for i in 1..blobs.len() {
        assert!(
            blobs[i].offset().0 > blobs[i - 1].offset().0,
            "offsets must be monotonically increasing"
        );
    }
}

/// MmapBlobReader::seek repositions to a known blob.
#[test]
fn mmap_seek() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.osm.pbf");
    write_test_pbf(&path);

    let mmap = unsafe { Mmap::from_path(&path).unwrap() };
    let mut reader = MmapBlobReader::new(mmap);

    let _header = reader.next().unwrap().unwrap();
    let first_data = reader.next().unwrap().unwrap();
    let target_offset = first_data.offset();

    // Decode the first data block
    let expected: Vec<(char, i64)> = match first_data.decode().unwrap() {
        BlobDecode::OsmData(block) => block.elements().map(|e| element_id(&e)).collect(),
        _ => panic!("expected OsmData"),
    };

    // Seek back and re-read
    reader.seek(target_offset);
    let re_read = reader.next().unwrap().unwrap();
    assert_eq!(re_read.offset(), target_offset);

    let actual: Vec<(char, i64)> = match re_read.decode().unwrap() {
        BlobDecode::OsmData(block) => block.elements().map(|e| element_id(&e)).collect(),
        _ => panic!("expected OsmData"),
    };

    assert_eq!(expected, actual);
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

    // Seek to end — next should return None (clean EOF)
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
