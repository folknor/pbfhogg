//! Tests for error handling with corrupt/malformed PBF input.
//!
//! Verifies that BlobReader and MmapBlobReader produce correct errors (not panics)
//! when given truncated, oversized, or garbage data.
#![allow(
    clippy::unwrap_used,
    clippy::cognitive_complexity,
    clippy::too_many_lines,
    clippy::cast_possible_truncation
)]

use std::io::Cursor;

use pbfhogg::block_builder::{self, BlockBuilder};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobError, BlobReader, BlobType, ErrorKind, Mmap, MmapBlobReader};
use tempfile::TempDir;

/// Write a minimal valid PBF (header blob only, no data blocks) into a Vec.
fn write_header_only_pbf() -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut writer = PbfWriter::new(&mut buf, Compression::default());
        let header = block_builder::HeaderBuilder::new().build().unwrap();
        writer.write_header(&header).unwrap();
        writer.flush().unwrap();
    }
    buf
}

/// Write a valid PBF with a header blob and one data block.
fn write_one_block_pbf() -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut writer = PbfWriter::new(&mut buf, Compression::default());
        let header = block_builder::HeaderBuilder::new().build().unwrap();
        writer.write_header(&header).unwrap();

        let mut bb = BlockBuilder::new();
        bb.add_node(1, 0, 0, &[], None);
        writer
            .write_primitive_block(bb.take().unwrap().unwrap())
            .unwrap();
        writer.flush().unwrap();
    }
    buf
}

// ---------------------------------------------------------------------------
// BlobReader error tests
// ---------------------------------------------------------------------------

/// Empty input yields None (clean EOF), not an error.
#[test]
fn empty_file() {
    let data: &[u8] = &[];
    let mut reader = BlobReader::new(Cursor::new(data));
    assert!(reader.next().is_none(), "empty input should yield None");
}

/// 1-3 bytes: not enough for the 4-byte header length prefix.
/// The first byte reads successfully, then the second read_exact (bytes 1..4)
/// fails with UnexpectedEof, which is now propagated as ErrorKind::Io.
#[test]
fn truncated_header_size() {
    for len in 1..=3 {
        let data = vec![0xAA; len];
        let mut reader = BlobReader::new(Cursor::new(data));
        let err = reader.next().unwrap().unwrap_err();
        match err.into_kind() {
            ErrorKind::Io(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
            other => panic!("expected Io(UnexpectedEof) for {len} bytes, got {other:?}"),
        }
    }
}

/// Header size >= MAX_BLOB_HEADER_SIZE (64 KB) triggers HeaderTooBig.
#[test]
fn header_too_big() {
    // 65536 = 0x00010000 = MAX_BLOB_HEADER_SIZE; check is >=
    let data = 65536u32.to_be_bytes().to_vec();
    let mut reader = BlobReader::new(Cursor::new(data));
    let err = reader.next().unwrap().unwrap_err();
    match err.into_kind() {
        ErrorKind::Blob(BlobError::HeaderTooBig { size }) => {
            assert_eq!(size, 65536);
        }
        other => panic!("expected HeaderTooBig, got {other:?}"),
    }
}

/// Valid header length but too few bytes for the header data → wire-format parse error.
#[test]
fn truncated_header_data() {
    let mut data = Vec::new();
    data.extend_from_slice(&20u32.to_be_bytes()); // claims 20 bytes of header
    data.extend_from_slice(&[0x0A; 5]); // only 5 bytes follow
    let mut reader = BlobReader::new(Cursor::new(data));
    let err = reader.next().unwrap().unwrap_err();
    match err.into_kind() {
        ErrorKind::WireFormat { .. } => {}
        other => panic!("expected WireFormat error for truncated header, got {other:?}"),
    }
}

/// Valid header length with garbage bytes → wire-format parse error.
#[test]
fn garbage_header() {
    let mut data = Vec::new();
    data.extend_from_slice(&10u32.to_be_bytes()); // claims 10 bytes of header
    data.extend_from_slice(&[0xFF; 10]); // 10 bytes of garbage
    let mut reader = BlobReader::new(Cursor::new(data));
    let err = reader.next().unwrap().unwrap_err();
    match err.into_kind() {
        ErrorKind::WireFormat { .. } => {}
        other => panic!("expected WireFormat error for garbage header, got {other:?}"),
    }
}

/// Valid header blob followed by truncated data for the second blob.
#[test]
fn truncated_blob_data() {
    let full = write_one_block_pbf();

    // Find offset of the second blob (data block)
    let second_offset = {
        let mut reader =
            BlobReader::new_seekable(Cursor::new(full.as_slice())).unwrap();
        let _ = reader.next().unwrap().unwrap(); // header blob
        let second = reader.next().unwrap().unwrap(); // data blob
        second.offset().unwrap().0 as usize
    };

    // Keep the full header blob + only 6 bytes into the second blob
    // (enough for the 4-byte header size prefix + 2 bytes of header data)
    let truncated = &full[..second_offset + 6];
    let mut reader = BlobReader::new(Cursor::new(truncated));

    // First blob (header) should still succeed
    let first = reader.next().unwrap().unwrap();
    assert_eq!(first.get_type(), BlobType::OsmHeader);

    // Second blob should be an error (truncated)
    match reader.next() {
        Some(Err(_)) => {} // expected
        other => panic!("expected error for truncated blob, got {other:?}"),
    }
}

/// After an error, BlobReader stops iteration (returns None on subsequent calls).
#[test]
fn iteration_stops_after_error() {
    let mut data = write_header_only_pbf();
    // Append 2 garbage bytes (not enough for a 4-byte header length prefix)
    data.extend_from_slice(&[0xAA, 0xBB]);

    let mut reader = BlobReader::new(Cursor::new(data));

    // First blob (header) succeeds
    let first = reader.next().unwrap().unwrap();
    assert_eq!(first.get_type(), BlobType::OsmHeader);

    // Second read fails (2 bytes = InvalidHeaderSize)
    let second = reader.next().unwrap();
    assert!(second.is_err());

    // Third read: iteration has stopped (last_blob_ok = false)
    assert!(reader.next().is_none());
}

// ---------------------------------------------------------------------------
// MmapBlobReader error tests
// ---------------------------------------------------------------------------

/// Mmapping an empty file either fails at mmap creation or returns None on iteration.
#[test]
fn mmap_empty_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("empty.pbf");
    std::fs::write(&path, b"").unwrap();

    let file = std::fs::File::open(&path).unwrap();
    match unsafe { Mmap::from_file(&file) } {
        Ok(mmap) => {
            // If mmap succeeds on a zero-length file, iteration should yield None
            let mut reader = MmapBlobReader::new(mmap);
            assert!(reader.next().is_none());
        }
        Err(_) => {
            // mmap of empty file failed — also acceptable (EINVAL on most platforms)
        }
    }
}

/// MmapBlobReader on 1-3 bytes returns InvalidHeaderSize.
#[test]
fn mmap_truncated_header_size() {
    let dir = TempDir::new().unwrap();
    for len in 1..=3 {
        let path = dir.path().join(format!("truncated_{len}.pbf"));
        std::fs::write(&path, vec![0xAA; len]).unwrap();

        let mmap = unsafe { Mmap::from_path(&path).unwrap() };
        let mut reader = MmapBlobReader::new(mmap);
        let err = reader.next().unwrap().unwrap_err();
        match err.into_kind() {
            ErrorKind::Blob(BlobError::InvalidHeaderSize) => {}
            other => panic!("expected InvalidHeaderSize for {len} bytes, got {other:?}"),
        }
    }
}

/// MmapBlobReader on garbage data returns HeaderTooBig (0xFFFFFFFF >> MAX).
#[test]
fn mmap_corrupt_data() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("corrupt.pbf");
    std::fs::write(&path, [0xFF; 100]).unwrap();

    let mmap = unsafe { Mmap::from_path(&path).unwrap() };
    let mut reader = MmapBlobReader::new(mmap);
    let err = reader.next().unwrap().unwrap_err();
    match err.into_kind() {
        ErrorKind::Blob(BlobError::HeaderTooBig { .. }) => {}
        other => panic!("expected HeaderTooBig for all-0xFF data, got {other:?}"),
    }
}
