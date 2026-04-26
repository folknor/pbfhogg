//! Tests for error handling with corrupt/malformed PBF input.
//!
//! Verifies that BlobReader produces correct errors (not panics)
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
use pbfhogg::{BlobError, BlobReader, BlobType, ErrorKind};

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
        bb.add_node(1, 0, 0, std::iter::empty::<(&str, &str)>(), None);
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

/// 1-3 bytes: not enough for the 4-byte header length prefix. Per
/// `reference/truncation-handling.md` shape 1, this is a clean cut at
/// a frame boundary (with 1-3 leftover bytes from a writer that
/// crashed mid-prefix) - tolerated as EOF, no error. Aligns
/// `BlobReader` with `read_raw_frame` and the documented stance.
#[test]
fn truncated_header_size() {
    for len in 1..=3 {
        let data = vec![0xAA; len];
        let mut reader = BlobReader::new(Cursor::new(data));
        assert!(
            reader.next().is_none(),
            "1-3 trailing bytes should be tolerated as clean EOF (got Some for len={len})",
        );
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

/// Valid length prefix but too few bytes for the declared BlobHeader.
/// Per `reference/truncation-handling.md` shape 3, this is a hard
/// error - the BlobReader's post-`read_to_end` length check catches
/// the short read and surfaces it as `Io(UnexpectedEof)` before the
/// wire-format parser sees corrupt bytes.
#[test]
fn truncated_header_data() {
    let mut data = Vec::new();
    data.extend_from_slice(&20u32.to_be_bytes()); // claims 20 bytes of header
    data.extend_from_slice(&[0x0A; 5]); // only 5 bytes follow
    let mut reader = BlobReader::new(Cursor::new(data));
    let err = reader.next().unwrap().unwrap_err();
    match err.into_kind() {
        ErrorKind::Io(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        other => panic!("expected Io(UnexpectedEof) for truncated header, got {other:?}"),
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

// ---------------------------------------------------------------------------
// Raw-frame and HeaderWalker MAX_BLOB_HEADER_SIZE guards
//
// `BlobReader::read_blob_header` at blob.rs:390 has always had a
// MAX_BLOB_HEADER_SIZE cap, but `read_raw_frame` / `read_blob_header_only`
// (raw_frame.rs) and `HeaderWalker::next_header` (header_walker.rs) did
// not. Without the cap, an adversarial 4-byte file (length prefix of
// `u32::MAX`) triggers a multi-GB `vec![0u8; header_len]` allocation -
// an OOM / process-abort DoS vector on every command that routes
// through these primitives (cat passthrough, getid raw passthrough,
// has_indexdata, check_sorted_and_indexed, apply-changes / altw /
// extract strategies / geocode classify via HeaderWalker).
// ---------------------------------------------------------------------------

/// `read_blob_header_only` (via `has_indexdata`) rejects an oversized
/// length prefix with HeaderTooBig rather than attempting a huge
/// allocation.
#[test]
fn has_indexdata_rejects_oversized_header_length() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("adversarial.osm.pbf");
    // 4-byte length prefix only, value = MAX_BLOB_HEADER_SIZE (64 KiB).
    // The file has no header payload; pre-fix the function would
    // allocate 64 KiB, read_exact would return UnexpectedEof. Post-fix
    // the guard trips first and returns HeaderTooBig. We use the
    // minimum value that trips the `>=` check so the test doesn't
    // depend on allocator behaviour for very large sizes.
    std::fs::write(&path, 65536u32.to_be_bytes()).unwrap();

    let err = pbfhogg::has_indexdata(&path, false).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("header") && (msg.contains("too big") || msg.contains("65536")),
        "expected HeaderTooBig surface, got: {msg}",
    );
}

/// `read_raw_frame` (via `cat`) rejects an oversized length prefix
/// with HeaderTooBig rather than attempting a huge allocation.
#[test]
fn cat_rejects_oversized_header_length() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("adversarial.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    std::fs::write(&input, 65536u32.to_be_bytes()).unwrap();

    let result = pbfhogg::cat::cat(
        &[input.as_path()],
        &output,
        None,
        &pbfhogg::cat::CleanAttrs::default(),
        Compression::default(),
        false,
        false,
        &pbfhogg::HeaderOverrides::default(),
    );
    let err = match result {
        Ok(_) => panic!("expected cat() to error on adversarial file"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("header") && (msg.contains("too big") || msg.contains("65536")),
        "expected HeaderTooBig surface, got: {msg}",
    );
}

/// `build_classify_schedule*` rejects a schedule entry whose blob
/// body extends past EOF, rather than accepting the entry and then
/// failing at `read_exact_at` in a pread worker much later. The fix
/// adds an explicit `meta.data_offset + meta.data_size <= file_size`
/// check in both schedule builders. This test drives the check via
/// `check_refs`, which routes through `build_classify_schedules_split`.
#[cfg(feature = "commands")]
#[test]
fn check_refs_rejects_schedule_entry_past_eof() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("truncated.osm.pbf");

    // Write a valid header-only PBF plus one data blob, then truncate
    // in the middle of the data blob's body. The walker still parses
    // the BlobHeader (which claims the full data_size), so the
    // schedule builder sees a `(data_offset, data_size)` pair whose
    // end lies past the truncated EOF.
    let full = write_one_block_pbf();
    // `write_one_block_pbf` always produces a file > 60 bytes (header
    // blob + data blob); cutting the last 20 bytes puts EOF squarely
    // inside the data blob's body rather than on a clean blob boundary.
    assert!(full.len() > 60);
    let truncated = &full[..full.len() - 20];
    std::fs::write(&input, truncated).unwrap();

    // `check --refs` routes through `build_classify_schedules_split`.
    // Pre-fix: the schedule gets built cleanly, workers fail later
    // with an opaque `read_exact_at` UnexpectedEof. Post-fix: the
    // schedule builder catches the mismatch up front.
    let result = pbfhogg::check::refs::check_refs(
        &input,
        false, // check_relations
        false, // show_ids
        false, // direct_io
    );
    let err = match result {
        Ok(_) => panic!("expected check_refs to error on truncated PBF"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    // Pre-fix: schedule built cleanly, workers later failed with opaque
    // `read_exact_at` UnexpectedEof. Post-truncation-alignment: the
    // `HeaderWalker` payload-extent check fires up front (shape 4 per
    // `reference/truncation-handling.md`), so the schedule never gets
    // built. Either error class is acceptable - both pin "truncation
    // hard-errors before workers see corrupt bytes".
    assert!(
        msg.contains("data_size")
            || msg.contains("file is only")
            || msg.contains("blob payload truncated"),
        "expected schedule-builder or walker truncation error, got: {msg}",
    );
}

/// `HeaderWalker::next_header` (via `inspect`'s index-only fast path)
/// rejects an oversized length prefix with HeaderTooBig rather than
/// attempting a huge allocation on the two-pread fallback.
#[cfg(feature = "commands")]
#[test]
fn inspect_rejects_oversized_header_length_via_walker() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("adversarial.osm.pbf");
    std::fs::write(&path, 65536u32.to_be_bytes()).unwrap();

    let result = pbfhogg::inspect::inspect(&path, false, false, false, false, false);
    let err = match result {
        Ok(_) => panic!("expected inspect() to error on adversarial file"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("header") && (msg.contains("too big") || msg.contains("65536")),
        "expected HeaderTooBig surface, got: {msg}",
    );
}

/// After an error, BlobReader stops iteration (returns None on
/// subsequent calls). Use a fixture that produces a real error per the
/// reference truncation stance: a complete length prefix declaring N
/// bytes of header but with too few following (shape 3). 2-byte
/// trailing garbage no longer triggers an error - that's now
/// tolerated as a clean cut.
#[test]
fn iteration_stops_after_error() {
    let mut data = write_header_only_pbf();
    // Append a complete length prefix promising 20 header bytes,
    // then only 5 bytes - shape 3, hard error.
    data.extend_from_slice(&20u32.to_be_bytes());
    data.extend_from_slice(&[0x0A; 5]);

    let mut reader = BlobReader::new(Cursor::new(data));

    // First blob (header) succeeds
    let first = reader.next().unwrap().unwrap();
    assert_eq!(first.get_type(), BlobType::OsmHeader);

    // Second read fails (truncated header data, shape 3)
    let second = reader.next().unwrap();
    assert!(second.is_err());

    // Third read: iteration has stopped (last_blob_ok = false)
    assert!(reader.next().is_none());
}

