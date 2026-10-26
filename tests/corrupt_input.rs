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
use pbfhogg::{BlobError, BlobReader, BlobType, ErrorKind, MAX_BLOB_DATASIZE};

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
/// crashed mid-prefix) - tolerated as EOF, no error, as for every source
/// of the frame walk.
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

/// A zero-length `BlobHeader` is rejected, so a run of zero bytes cannot read
/// as a stream of empty frames.
///
/// A `header_len` of 0 forms a legal-looking 4-byte frame: the header parses to
/// an unknown blob type with `datasize == 0`, which every consumer skips
/// without decoding. A file of `N` zero bytes therefore used to read as `N/4`
/// empty blobs - exit 0, empty output, no diagnostic. That is worse than a hard
/// error and worse than a hang: it silently substitutes an empty result for the
/// user's data.
#[test]
fn zero_length_blob_header_is_rejected() {
    let data = 0u32.to_be_bytes().to_vec();
    let mut reader = BlobReader::new(Cursor::new(data));
    let err = reader
        .next()
        .expect("a complete 4-byte length prefix is a committed frame, not EOF")
        .unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::Blob(BlobError::InvalidHeaderSize)),
        "a zero-length BlobHeader must be InvalidHeaderSize"
    );
}

/// The same rejection applies mid-file: zeros appended to (or substituted
/// into) an otherwise valid PBF must not read as clean success.
#[test]
fn trailing_zero_run_after_valid_blobs_is_rejected() {
    let mut data = write_one_block_pbf();
    let good_blobs = BlobReader::new(Cursor::new(data.clone()))
        .filter(Result::is_ok)
        .count();
    assert!(good_blobs >= 2, "fixture must have header + data blobs");

    data.extend_from_slice(&[0u8; 64]);
    let mut reader = BlobReader::new(Cursor::new(data));
    let mut seen = 0;
    let mut errored = false;
    for result in reader.by_ref() {
        match result {
            Ok(_) => seen += 1,
            Err(_) => {
                errored = true;
                break;
            }
        }
    }
    assert_eq!(seen, good_blobs, "the real blobs must still be readable");
    assert!(
        errored,
        "a 64-byte zero run must surface as an error, not 16 empty blobs"
    );
}

/// Header size >= MAX_BLOB_HEADER_SIZE (64 KB) triggers HeaderTooBig.
#[test]
fn header_too_big() {
    // 65536 = 0x00010000 = MAX_BLOB_HEADER_SIZE; check is >=
    let data = 65536u32.to_be_bytes().to_vec();
    let mut reader = BlobReader::new(Cursor::new(data));
    let err = reader.next().unwrap().unwrap_err();
    match err.kind() {
        ErrorKind::Blob(BlobError::HeaderTooBig { size }) => {
            assert_eq!(*size, 65536);
        }
        other => panic!("expected HeaderTooBig, got {other:?}"),
    }
}

/// A framing error must say *where*. `BlobReader::new` cannot know the
/// stream position (an arbitrary reader may start anywhere), but the
/// file-backed constructors every command uses do, and a blob error from a
/// 90 GB planet file is unactionable without it.
#[test]
fn framing_error_carries_byte_offset() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("header-too-big.osm.pbf");
    std::fs::write(&path, 65536u32.to_be_bytes()).expect("write fixture");
    let mut reader = BlobReader::from_path(&path).expect("open fixture");
    let err = reader.next().expect("an item").expect_err("HeaderTooBig");
    assert_eq!(
        err.byte_offset().map(|o| o.0),
        Some(0),
        "a frame error is located at its frame, whose prefix is the defect here"
    );
}

/// Build a one-block PBF truncated `got` bytes into the second blob's
/// payload, and return it alongside the frame start and payload start
/// offsets. Shape 4.
fn truncated_in_payload(got: usize) -> (Vec<u8>, u64, u64) {
    let full = write_one_block_pbf();
    let frame_start = {
        let mut r = BlobReader::new_seekable(Cursor::new(full.as_slice())).unwrap();
        let _ = r.next().unwrap().unwrap(); // header blob
        let second = r.next().unwrap().unwrap(); // data blob
        second.offset().unwrap().0 as usize
    };
    let header_size =
        u32::from_be_bytes(full[frame_start..frame_start + 4].try_into().unwrap()) as usize;
    let payload_start = frame_start + 4 + header_size;
    assert!(
        payload_start + got < full.len(),
        "fixture must actually be truncated"
    );
    (
        full[..payload_start + got].to_vec(),
        frame_start as u64,
        payload_start as u64,
    )
}

/// A truncation is located at the frame it broke, like every other failure
/// of a blob (the ordinal and frame start a decode error on the same blob
/// would carry), and the message names the declared extent that runs past
/// the end of the input - the same words whether the payload was read or
/// skipped, since how the shortfall was noticed is not something the user
/// can act on.
#[test]
fn truncated_payload_is_located_at_its_frame_and_names_the_declared_extent() {
    let (data, frame_start, payload_start) = truncated_in_payload(3);
    let declared_end = write_one_block_pbf().len() as u64;
    let extent = format!("[{payload_start}, {declared_end})");

    let mut reader = BlobReader::new_seekable(Cursor::new(data.clone())).unwrap();
    let _ = reader.next().unwrap().unwrap(); // header blob
    let read = reader.next().unwrap().unwrap_err();

    // Header-only consumers return the skip error verbatim, so it has to be
    // located by the frame walk itself.
    let mut reader = BlobReader::new_seekable(Cursor::new(data)).unwrap();
    let _ = reader.next_header_skip_blob().unwrap().unwrap(); // header blob
    let skipped = reader.next_header_skip_blob().unwrap().unwrap_err();

    for err in [&read, &skipped] {
        assert_eq!(err.byte_offset().map(|o| o.0), Some(frame_start), "{err}");
        assert_eq!(err.blob_index(), Some(1), "{err}");
        assert!(err.to_string().contains(&extent), "{err}");
    }
    assert_eq!(read.to_string(), skipped.to_string());
}

/// Valid length prefix but too few bytes for the declared BlobHeader.
/// Per `reference/truncation-handling.md` shape 3, this is a hard
/// error - the frame walk's length check catches the short read and
/// surfaces it as `Io(UnexpectedEof)` before the wire-format parser sees
/// corrupt bytes.
#[test]
fn truncated_header_data() {
    let mut data = Vec::new();
    data.extend_from_slice(&20u32.to_be_bytes()); // claims 20 bytes of header
    data.extend_from_slice(&[0x0A; 5]); // only 5 bytes follow
    let mut reader = BlobReader::new(Cursor::new(data));
    let err = reader.next().unwrap().unwrap_err();
    match err.kind() {
        ErrorKind::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        other => panic!("expected Io(UnexpectedEof) for truncated header, got {other:?}"),
    }
}

/// The blob index stamped onto a framing error counts sequential
/// iteration from the stream start - and is dropped, not guessed, once a
/// seek breaks that sequence. A stale count would name the wrong blob,
/// which is worse than naming none; a rewind to offset 0 restarts the
/// count because the sequence is genuinely restarting.
#[test]
fn blob_index_attribution_survives_iteration_but_not_seeks() {
    let (data, _, _) = truncated_in_payload(3);

    // The failing data blob's frame start, from the intact fixture.
    let second_offset = {
        let full = write_one_block_pbf();
        let mut r = BlobReader::new_seekable(Cursor::new(full.as_slice())).unwrap();
        let _ = r.next().unwrap().unwrap(); // header blob
        r.next().unwrap().unwrap().offset().unwrap() // data blob
    };
    assert_ne!(second_offset.0, 0, "sanity: the data blob is not at 0");

    // Sequential from the start: the failing data blob is blob 1.
    let mut reader = BlobReader::new_seekable(Cursor::new(data.as_slice())).unwrap();
    let _ = reader.next().unwrap().unwrap();
    let err = reader.next().unwrap().unwrap_err();
    assert_eq!(
        err.blob_index(),
        Some(1),
        "sequential iteration must attribute the error to blob 1"
    );

    // Seek away from the start: the count is no longer knowable.
    let mut reader = BlobReader::new_seekable(Cursor::new(data.as_slice())).unwrap();
    reader.seek(second_offset).unwrap();
    let err = reader.next().unwrap().unwrap_err();
    assert_eq!(
        err.blob_index(),
        None,
        "an error after a non-zero seek must not claim a blob index"
    );
    assert!(
        err.byte_offset().is_some(),
        "the byte offset is still known and must survive"
    );

    // Rewind to 0: sequential counting restarts from blob 0.
    let mut reader = BlobReader::new_seekable(Cursor::new(data.as_slice())).unwrap();
    let _ = reader.next().unwrap().unwrap();
    reader.seek(pbfhogg::ByteOffset(0)).unwrap();
    let _ = reader.next().unwrap().unwrap(); // header again, blob 0
    let err = reader.next().unwrap().unwrap_err();
    assert_eq!(
        err.blob_index(),
        Some(1),
        "a rewind to the stream start restarts the count"
    );
}

/// Valid header length with garbage bytes → wire-format parse error.
#[test]
fn garbage_header() {
    let mut data = Vec::new();
    data.extend_from_slice(&10u32.to_be_bytes()); // claims 10 bytes of header
    data.extend_from_slice(&[0xFF; 10]); // 10 bytes of garbage
    let mut reader = BlobReader::new(Cursor::new(data));
    let err = reader.next().unwrap().unwrap_err();
    match err.kind() {
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
        let mut reader = BlobReader::new_seekable(Cursor::new(full.as_slice())).unwrap();
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
// The raw-frame and header-walk paths once decoded frames separately from
// `BlobReader` and went without its MAX_BLOB_HEADER_SIZE cap. Without the
// cap, an adversarial 4-byte file (length prefix of `u32::MAX`) triggers a
// multi-GB header allocation - an OOM / process-abort DoS vector on every
// command that routes through these primitives (cat passthrough, getid raw
// passthrough, has_indexdata, check_sorted_and_indexed, apply-changes /
// altw / extract strategies / geocode classify via HeaderWalker). They now
// share one frame walk; these tests keep each route covered end to end.
// ---------------------------------------------------------------------------

/// The front probe (via `has_indexdata`) rejects an oversized
/// length prefix with HeaderTooBig rather than attempting a huge
/// allocation.
#[test]
fn has_indexdata_rejects_oversized_header_length() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("adversarial.osm.pbf");
    // 4-byte length prefix only, value = MAX_BLOB_HEADER_SIZE (64 KiB).
    // The file has no header payload; pre-fix the function would
    // allocate 64 KiB, read_exact would return UnexpectedEof. Post-fix
    // the guard trips first and returns HeaderTooBig. We use the
    // minimum value that trips the `>=` check so the test doesn't
    // depend on allocator behaviour for very large sizes.
    std::fs::write(&path, 65536u32.to_be_bytes()).unwrap();

    let err =
        pbfhogg::has_indexdata(&path, false, &pbfhogg::host::Diagnostics::discard()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("header") && (msg.contains("too big") || msg.contains("65536")),
        "expected HeaderTooBig surface, got: {msg}",
    );
}

/// The raw-frame walk (via `cat`) rejects an oversized length prefix
/// with HeaderTooBig rather than attempting a huge allocation.
#[test]
fn cat_rejects_oversized_header_length() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("adversarial.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    std::fs::write(&input, 65536u32.to_be_bytes()).unwrap();

    let result = pbfhogg::cat::cat(
        &[input.as_path()],
        crate::common::out(&output),
        None,
        &pbfhogg::cat::CleanAttrs::default(),
        &pbfhogg::write::WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
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

/// A schedule builder never accepts an entry whose blob body extends
/// past EOF, to fail later at `read_exact_at` in a pread worker: the
/// `HeaderWalker` every builder walks with refuses the frame first,
/// naming the file and the frame's byte offset. Driven through
/// `check_refs`, which routes through `build_classify_schedules_split`;
/// `cli_input_contract` pins the same refusal for every command.
#[cfg(feature = "commands")]
#[test]
fn check_refs_rejects_schedule_entry_past_eof() {
    let dir = crate::common::test_scratch();
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

    let opts = pbfhogg::check::refs::CheckRefsOptions {
        check: pbfhogg::check::CheckOptions {
            max_errors: 0,
            direct_io: false,
        },
        check_relations: false,
        show_ids: false,
    };
    let result = pbfhogg::check::refs::check_refs(&input, &opts, &crate::common::test_host());
    let err = match result {
        Ok(_) => panic!("expected check_refs to error on truncated PBF"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    // Shape 4 per `reference/truncation-handling.md`, refused by the walk
    // itself, so the schedule never gets built.
    let frame_start = crate::common::adversarial::locate_blobs(&full)
        .last()
        .expect("fixture has frames")
        .frame_start;
    assert!(
        msg.contains("frame truncated")
            && msg.contains(&input.display().to_string())
            && msg.contains(&format!("at byte offset {frame_start}")),
        "expected the walker's located truncation error, got: {msg}",
    );
}

/// `HeaderWalker::next_header` (via `inspect`'s index-only fast path)
/// rejects an oversized length prefix with HeaderTooBig rather than
/// attempting a huge allocation on the two-pread fallback.
#[cfg(feature = "commands")]
#[test]
fn inspect_rejects_oversized_header_length_via_walker() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("adversarial.osm.pbf");
    std::fs::write(&path, 65536u32.to_be_bytes()).unwrap();

    let result = pbfhogg::inspect::inspect(
        &path,
        &pbfhogg::inspect::InspectOptions {
            show_blocks: false,
            show_id_ranges: false,
            show_locations: false,
            extended: false,
            distrust_metadata: false,
            direct_io: false,
        },
        &crate::common::test_host(),
    );
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

// ---------------------------------------------------------------------------
// Raw-frame / BlobReader / HeaderWalker MAX_BLOB_DATASIZE guards
//
// The decompressed-content cap (`MAX_BLOB_MESSAGE_SIZE`, 32 MiB) only fires
// after a blob's compressed body has been read into memory. A hostile or
// corrupt `BlobHeader.datasize` therefore forces the pre-decompression body
// allocation (`Vec::with_capacity(datasize)` in `BlobReader::next`,
// the frame-sized buffer of the raw-frame walk, `pread_data` in `HeaderWalker`
// consumers) to an arbitrary size ahead of that guard - a memory-amplification
// DoS. `MAX_BLOB_DATASIZE` (32 MiB, mirroring the reference reader's
// `MAX_BODY_SIZE` applied directly to `BlobHeader.datasize`) rejects the
// declared size at the boundary. These tests append an adversarial OSMData
// frame - a valid BlobHeader declaring `datasize == MAX_BLOB_DATASIZE` with NO
// Blob payload following - after a valid header-only PBF, and assert each read
// path hard-errors before the allocation.
//
// Command entry points (`cat`, `inspect`, `has_indexdata`) return
// `pbfhogg::Error`, so these check the typed
// `ErrorKind::Blob(BlobError::DataSizeTooBig)` rather than matching display text.
// ---------------------------------------------------------------------------

/// Assert a command error is the crate's typed `DataSizeTooBig` carrying the
/// expected declared size.
fn assert_datasize_too_big(err: &pbfhogg::Error, expected: u64) {
    match err.kind() {
        ErrorKind::Blob(BlobError::DataSizeTooBig { size }) => {
            assert_eq!(*size, expected);
        }
        other => panic!("expected DataSizeTooBig, got {other:?}"),
    }
}

/// Append `[4-byte BE header_len][BlobHeader]` for an OSMData blob declaring
/// `datasize`, with no Blob payload. Hand-encoded to keep the fixture on the
/// public API (no writer produces an oversized datasize).
fn append_oversized_datasize_frame(buf: &mut Vec<u8>, datasize: u64) {
    let mut header = Vec::new();
    // Field 1 (type): tag 0x0A, len 7, "OSMData".
    header.push(0x0A);
    header.push(7);
    header.extend_from_slice(b"OSMData");
    // Field 3 (datasize): tag 0x18, LEB128 varint.
    header.push(0x18);
    let mut v = datasize;
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        header.push(byte);
        if v == 0 {
            break;
        }
    }
    buf.extend_from_slice(&(header.len() as u32).to_be_bytes());
    buf.extend_from_slice(&header);
}

/// `BlobReader` (the public reader spine every command inherits) rejects an
/// oversized declared datasize with `DataSizeTooBig` before allocating the
/// payload buffer.
#[test]
fn blob_reader_rejects_oversized_datasize() {
    let mut data = write_header_only_pbf();
    append_oversized_datasize_frame(&mut data, MAX_BLOB_DATASIZE);

    let mut reader = BlobReader::new(Cursor::new(data));
    // First blob (header) parses fine.
    let first = reader.next().unwrap().unwrap();
    assert_eq!(first.get_type(), BlobType::OsmHeader);
    // Second (adversarial) blob is rejected before payload allocation.
    let err = reader.next().unwrap().unwrap_err();
    match err.kind() {
        ErrorKind::Blob(BlobError::DataSizeTooBig { size }) => {
            assert_eq!(*size, MAX_BLOB_DATASIZE);
        }
        other => panic!("expected DataSizeTooBig, got {other:?}"),
    }
}

/// The raw-frame walk (via `cat` passthrough) rejects an oversized declared
/// datasize before the frame-sized buffer is allocated.
#[test]
fn cat_rejects_oversized_datasize() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("adversarial.osm.pbf");
    let output = dir.path().join("out.osm.pbf");
    let mut data = write_header_only_pbf();
    append_oversized_datasize_frame(&mut data, MAX_BLOB_DATASIZE);
    std::fs::write(&input, &data).unwrap();

    let result = pbfhogg::cat::cat(
        &[input.as_path()],
        crate::common::out(&output),
        None,
        &pbfhogg::cat::CleanAttrs::default(),
        &pbfhogg::write::WriteOptions::default(),
        &pbfhogg::HeaderOverrides::default(),
        &crate::common::test_host(),
    );
    let err = match result {
        Ok(_) => panic!("expected cat() to error on adversarial datasize"),
        Err(e) => e,
    };
    assert_datasize_too_big(&err, MAX_BLOB_DATASIZE);
}

/// The front probe (via `has_indexdata`) rejects an oversized declared
/// datasize with the typed `DataSizeTooBig` before any body allocation. This is
/// the one route not exercised by the cat / inspect / BlobReader
/// tests: `has_indexdata` walks blob headers, skipping payloads, until
/// the first OSMData blob. The fixture is a valid header-only PBF (whose
/// OsmHeader blob is walked past) followed by an adversarial OSMData frame, so
/// the guard trips on that second header.
#[test]
fn has_indexdata_rejects_oversized_datasize() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("adversarial.osm.pbf");
    let mut data = write_header_only_pbf();
    append_oversized_datasize_frame(&mut data, MAX_BLOB_DATASIZE);
    std::fs::write(&path, &data).unwrap();

    let err =
        pbfhogg::has_indexdata(&path, false, &pbfhogg::host::Diagnostics::discard()).unwrap_err();
    assert_datasize_too_big(&err, MAX_BLOB_DATASIZE);
}

/// `HeaderWalker::next_header` (via `inspect`'s index-only fast path) rejects an
/// oversized declared datasize before any consumer `pread_data`.
#[cfg(feature = "commands")]
#[test]
fn inspect_rejects_oversized_datasize_via_walker() {
    let dir = crate::common::test_scratch();
    let path = dir.path().join("adversarial.osm.pbf");
    let mut data = write_header_only_pbf();
    append_oversized_datasize_frame(&mut data, MAX_BLOB_DATASIZE);
    std::fs::write(&path, &data).unwrap();

    let result = pbfhogg::inspect::inspect(
        &path,
        &pbfhogg::inspect::InspectOptions {
            show_blocks: false,
            show_id_ranges: false,
            show_locations: false,
            extended: false,
            distrust_metadata: false,
            direct_io: false,
        },
        &crate::common::test_host(),
    );
    let err = match result {
        Ok(_) => panic!("expected inspect() to error on adversarial datasize"),
        Err(e) => e,
    };
    assert_datasize_too_big(&err, MAX_BLOB_DATASIZE);
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
