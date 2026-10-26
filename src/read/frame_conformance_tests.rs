//! Frame-decoder conformance: every truncation shape of
//! `reference/truncation-handling.md`, plus the frame size guards, run
//! through every frame source, each held to one expectation per case.
//!
//! The crate decodes the frame format in one place, `read::frame::Frames`,
//! over several sources with different I/O: `BlobReader`'s streaming read
//! path (`next`) and its seek-and-sentinel skip path
//! (`next_header_skip_blob`), the raw-frame walk over a buffered
//! `FileReader` (`Frames::next_raw`), the front probes' header-then-skip
//! over the same reader, and the pread `HeaderWalker`. They used to be
//! separate decoders that disagreed about where an error was located,
//! whether it named the blob and how a truncation was worded; this table is
//! what keeps a source-specific path from bringing a disagreement back. A
//! change to any expectation is a behaviour change at every source.
//!
//! Every refusal is located at the frame - the file, blob 2 and the frame's
//! start - and every source words it the same: the test compares the whole
//! error text across sources, not only its kind.
//!
//! Scope is the frame layer. Shape 5 (decompression failure) is invisible to
//! every source here, which none of them decompress; the separate test below
//! shows the damage is real and is caught at decode. The one case where the
//! sources differ is a payload that is not a `Blob` message: only
//! `BlobReader::next` parses the envelope, because a parsed `Blob` is what it
//! returns, while passthrough and header walks never look inside a payload.

#![allow(clippy::unwrap_used)]

use super::{BlobReader, MAX_BLOB_DATASIZE, MAX_BLOB_HEADER_SIZE, WireBlobHeader};
use crate::error::{BlobError, Error, ErrorKind};
use crate::read::file_reader::FileReader;
use crate::read::frame::Frames;
use crate::read::header_walker::HeaderWalker;
use std::path::Path;

/// One frame source, driven over a whole file.
#[derive(Clone, Copy, Debug)]
enum Source {
    /// `BlobReader::from_path`, iterated: reads every payload.
    Read,
    /// `BlobReader::seekable_from_path` + `next_header_skip_blob`: seeks past
    /// every payload and probes one sentinel byte.
    Skip,
    /// `Frames::next_raw` over a buffered `FileReader`.
    Raw,
    /// `Frames::next_header` then `skip_payload` over a buffered
    /// `FileReader`, as the front probes walk.
    HeaderThenSkip,
    /// `HeaderWalker::next_header`: pread, payload extent checked against the
    /// file size taken at open.
    Walk,
}

const SOURCES: [Source; 5] = [
    Source::Read,
    Source::Skip,
    Source::Raw,
    Source::HeaderThenSkip,
    Source::Walk,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// `ErrorKind::Io` of kind `UnexpectedEof`.
    Eof,
    InvalidHeaderSize,
    HeaderTooBig(u64),
    DataSizeTooBig(u64),
    InvalidDataSize(i32),
    /// `ErrorKind::WireFormat`, any message.
    Wire,
}

/// What one source did with one file.
#[derive(Debug, PartialEq, Eq)]
enum Seen {
    /// `frames` frames delivered, then end of input with no error.
    End { frames: usize },
    /// `frames` frames delivered, then this error. `named` is whether it
    /// names the input file (checked to be the right one when it does).
    Refused {
        frames: usize,
        kind: Kind,
        named: bool,
        blob: Option<u64>,
        offset: Option<u64>,
    },
}

fn u(x: usize) -> u64 {
    u64::try_from(x).unwrap()
}

fn kind_of(err: &Error) -> Kind {
    match err.kind() {
        ErrorKind::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Kind::Eof,
        ErrorKind::Blob(BlobError::InvalidHeaderSize) => Kind::InvalidHeaderSize,
        ErrorKind::Blob(BlobError::HeaderTooBig { size }) => Kind::HeaderTooBig(*size),
        ErrorKind::Blob(BlobError::DataSizeTooBig { size }) => Kind::DataSizeTooBig(*size),
        ErrorKind::Blob(BlobError::InvalidDataSize { size }) => Kind::InvalidDataSize(*size),
        ErrorKind::WireFormat { .. } => Kind::Wire,
        other => panic!("unclassified frame error {other:?}: {err}"),
    }
}

/// The outcome, and the error text when there was an error.
type Outcome = (Seen, Option<String>);

fn refused(frames: usize, err: &Error, path: &Path) -> Outcome {
    let named = match err.source_path() {
        Some(named) => {
            assert_eq!(named, path, "an error naming the wrong file: {err}");
            true
        }
        None => false,
    };
    (
        Seen::Refused {
            frames,
            kind: kind_of(err),
            named,
            blob: err.blob_index(),
            offset: err.byte_offset().map(|o| o.0),
        },
        Some(err.to_string()),
    )
}

fn ended(frames: usize) -> Outcome {
    (Seen::End { frames }, None)
}

fn run(source: Source, path: &Path) -> Outcome {
    match source {
        Source::Read => run_read(path),
        Source::Skip => run_skip(path),
        Source::Raw => run_raw(path),
        Source::HeaderThenSkip => run_header_then_skip(path),
        Source::Walk => run_walk(path),
    }
}

fn run_read(path: &Path) -> Outcome {
    let mut frames = 0;
    for item in BlobReader::from_path(path).unwrap() {
        match item {
            Ok(_) => frames += 1,
            Err(e) => return refused(frames, &e, path),
        }
    }
    ended(frames)
}

fn run_skip(path: &Path) -> Outcome {
    let mut reader = BlobReader::seekable_from_path(path).unwrap();
    let mut frames = 0;
    while let Some(item) = reader.next_header_skip_blob() {
        match item {
            Ok(_) => frames += 1,
            Err(e) => return refused(frames, &e, path),
        }
    }
    ended(frames)
}

fn file_frames(path: &Path) -> Frames<FileReader> {
    Frames::of_input(
        FileReader::buffered(path).unwrap(),
        path,
        &crate::host::Host::for_tests(),
    )
}

fn run_raw(path: &Path) -> Outcome {
    let mut walk = file_frames(path);
    let mut frames = 0;
    loop {
        match walk.next_raw() {
            Ok(Some(_)) => frames += 1,
            Ok(None) => return ended(frames),
            Err(e) => return refused(frames, &e, path),
        }
    }
}

fn run_header_then_skip(path: &Path) -> Outcome {
    let mut walk = file_frames(path);
    let mut frames = 0;
    loop {
        match walk.next_header() {
            Ok(Some(_)) => {
                if let Err(e) = walk.skip_payload() {
                    return refused(frames, &e, path);
                }
                frames += 1;
            }
            Ok(None) => return ended(frames),
            Err(e) => return refused(frames, &e, path),
        }
    }
}

fn run_walk(path: &Path) -> Outcome {
    let mut walker = HeaderWalker::open(path, &crate::host::Host::for_tests()).unwrap();
    let mut frames = 0;
    loop {
        match walker.next_header() {
            Ok(Some(_)) => frames += 1,
            Ok(None) => return ended(frames),
            Err(e) => return refused(frames, &e, path),
        }
    }
}

// ---------------------------------------------------------------------------
// Fixture and byte surgery
// ---------------------------------------------------------------------------

/// An OSMHeader blob and two zlib-compressed OSMData blobs.
fn intact_pbf() -> Vec<u8> {
    use crate::block_builder::{BlockBuilder, HeaderBuilder};
    use crate::writer::{Compression, PbfWriter};

    let mut buf: Vec<u8> = Vec::new();
    {
        let mut writer = PbfWriter::new(&mut buf, Compression::Zlib(6));
        writer
            .write_header(&HeaderBuilder::new().build().unwrap())
            .unwrap();
        for base in [0_i64, 1000] {
            let mut block = BlockBuilder::new();
            for id in 1..=32_i64 {
                let coord = i32::try_from(id * 1000).unwrap();
                block.add_node(base + id, coord, coord, [("name", "frame")], None);
            }
            writer
                .write_primitive_block(block.take().unwrap().unwrap())
                .unwrap();
        }
        writer.flush().unwrap();
    }
    buf
}

#[derive(Clone, Copy, Debug)]
struct FrameAt {
    start: usize,
    header_len: usize,
    datasize: usize,
}

/// The frame layout, walked by hand so no decoder under test derives it.
fn frames_of(pbf: &[u8]) -> Vec<FrameAt> {
    let mut frames = Vec::new();
    let mut at = 0;
    while at < pbf.len() {
        let header_len =
            usize::try_from(u32::from_be_bytes(pbf[at..at + 4].try_into().unwrap())).unwrap();
        let header =
            WireBlobHeader::parse(&pbf[at + 4..at + 4 + header_len], false, false, false).unwrap();
        let datasize = usize::try_from(header.datasize).unwrap();
        frames.push(FrameAt {
            start: at,
            header_len,
            datasize,
        });
        at += 4 + header_len + datasize;
    }
    assert_eq!(at, pbf.len(), "the fixture ends at a frame boundary");
    frames
}

fn push_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = u8::try_from(v & 0x7f).unwrap();
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn read_varint(bytes: &[u8], at: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let byte = bytes[*at];
        *at += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return v;
        }
        shift += 7;
    }
}

/// Start and length of the value of length-delimited `field` in `msg`.
fn len_field(msg: &[u8], field: u64) -> (usize, usize) {
    let mut at = 0;
    while at < msg.len() {
        let tag = read_varint(msg, &mut at);
        match tag & 7 {
            0 => {
                read_varint(msg, &mut at);
            }
            2 => {
                let len = usize::try_from(read_varint(msg, &mut at)).unwrap();
                if tag >> 3 == field {
                    return (at, len);
                }
                at += len;
            }
            other => panic!("unexpected wire type {other} in the fixture"),
        }
    }
    panic!("field {field} absent from the fixture message");
}

/// A hand-built BlobHeader: `type` when non-empty, `datasize` when given
/// (as a raw varint, so `u64::MAX` is the sign-extended int32 -1).
fn blob_header(kind: &str, datasize: Option<u64>) -> Vec<u8> {
    let mut out = Vec::new();
    if !kind.is_empty() {
        out.push(0x0A);
        push_varint(&mut out, u(kind.len()));
        out.extend_from_slice(kind.as_bytes());
    }
    if let Some(datasize) = datasize {
        out.push(0x18);
        push_varint(&mut out, datasize);
    }
    out
}

/// A valid OSMData BlobHeader padded with an unknown field (15) to exactly
/// `total` bytes.
fn padded_header(datasize: usize, total: usize) -> Vec<u8> {
    let mut out = blob_header("OSMData", Some(u(datasize)));
    // Tag byte plus a three-byte length varint, which holds for any pad of
    // 16384..2097151 bytes.
    let pad = total - out.len() - 4;
    out.push(0x7A);
    push_varint(&mut out, u(pad));
    out.resize(out.len() + pad, 0);
    assert_eq!(out.len(), total, "the pad length must take three bytes");
    out
}

struct Fixture {
    dir: tempfile::TempDir,
    intact: Vec<u8>,
    /// The last frame (blob index 2): its start, BlobHeader length, payload
    /// start and declared datasize. Every case damages this frame.
    f: usize,
    h: usize,
    p: usize,
    d: usize,
}

impl Fixture {
    fn new() -> Self {
        let intact = intact_pbf();
        let frames = frames_of(&intact);
        assert_eq!(frames.len(), 3, "header blob plus two data blobs");
        let last = frames[2];
        let (f, h, d) = (last.start, last.header_len, last.datasize);
        let p = f + 4 + h;
        assert!(
            h > 1 && d > 1,
            "the last frame must be cuttable mid-header and mid-payload"
        );
        assert_eq!(p + d, intact.len(), "the last payload ends the file");
        assert!(
            u(h + d + 1) < MAX_BLOB_HEADER_SIZE,
            "the past-EOF prefix must stay under the cap"
        );
        Fixture {
            dir: crate::path_guard::tests::scratch(),
            intact,
            f,
            h,
            p,
            d,
        }
    }

    fn cut(&self, len: usize) -> Vec<u8> {
        self.intact[..len].to_vec()
    }

    fn with_tail(&self, tail: usize) -> Vec<u8> {
        let mut bytes = self.intact.clone();
        bytes.resize(bytes.len() + tail, 0xAA);
        bytes
    }

    /// The intact file with the last frame's length prefix overwritten.
    fn with_prefix(&self, prefix: u32) -> Vec<u8> {
        let mut bytes = self.intact.clone();
        bytes[self.f..self.f + 4].copy_from_slice(&prefix.to_be_bytes());
        bytes
    }

    /// The first two frames, then a last frame of `header` and `payload`.
    fn with_last_frame(&self, header: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut bytes = self.intact[..self.f].to_vec();
        bytes.extend_from_slice(&u32::try_from(header.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(header);
        bytes.extend_from_slice(payload);
        bytes
    }

    fn payload(&self) -> &[u8] {
        &self.intact[self.p..]
    }

    /// Shape 5: the last blob's zlib stream gets compression method 0 in its
    /// CMF byte, which no inflater accepts. The Blob envelope is untouched.
    fn zlib_damaged(&self) -> Vec<u8> {
        let (start, len) = len_field(self.payload(), 3);
        assert!(len > 2, "the fixture blob carries zlib_data");
        let mut bytes = self.intact.clone();
        assert_eq!(bytes[self.p + start] & 0x0F, 8, "a deflate CMF byte");
        bytes[self.p + start] = 0x00;
        bytes
    }

    /// A refusal of the last frame: two frames delivered, then `kind`,
    /// located at the file, blob 2 and the frame start.
    fn refusal(&self, kind: Kind) -> Seen {
        Seen::Refused {
            frames: 2,
            kind,
            named: true,
            blob: Some(2),
            offset: Some(u(self.f)),
        }
    }
}

struct Case {
    name: String,
    bytes: Vec<u8>,
    expect: Seen,
    /// A substring every source's error text must contain.
    text: Option<String>,
}

fn case(name: impl Into<String>, bytes: Vec<u8>, expect: Seen) -> Case {
    Case {
        name: name.into(),
        bytes,
        expect,
        text: None,
    }
}

/// The truncation of `len` bytes of `what` from `start`, as every source
/// words it.
fn truncation(by: &str, what: &str, len: usize, start: usize) -> Option<String> {
    Some(format!(
        "frame truncated: {by} declares a {len}-byte {what} at bytes [{start}, {}), \
         past the end of the input",
        start + len
    ))
}

/// Shapes 1-3: the frame boundary, the length prefix and the BlobHeader.
fn frame_and_header_cases(fx: &Fixture) -> Vec<Case> {
    let (f, h, d) = (fx.f, fx.h, fx.d);
    let cap = u32::try_from(MAX_BLOB_HEADER_SIZE).unwrap();
    let header_cut = |name: &str, bytes: Vec<u8>, len: usize| Case {
        text: truncation("its length prefix", "BlobHeader", len, f + 4),
        ..case(name, bytes, fx.refusal(Kind::Eof))
    };
    let mut cases = vec![case("intact", fx.intact.clone(), Seen::End { frames: 3 })];
    for tail in 1..=3 {
        // Every source ends silently; none reports the tail.
        cases.push(case(
            format!("shape 1: {tail}-byte tail"),
            fx.with_tail(tail),
            Seen::End { frames: 3 },
        ));
    }
    cases.extend([
        case(
            "shape 1: cut 2 bytes into the last prefix",
            fx.cut(f + 2),
            Seen::End { frames: 2 },
        ),
        header_cut("shape 2: cut right after the last prefix", fx.cut(f + 4), h),
        header_cut(
            "shape 3: cut inside the last BlobHeader",
            fx.cut(f + 4 + h / 2),
            h,
        ),
        header_cut(
            "shape 2: last prefix runs one byte past EOF",
            fx.with_prefix(u32::try_from(h + d + 1).unwrap()),
            h + d + 1,
        ),
        header_cut(
            "shape 2: last prefix one below the cap, past EOF",
            fx.with_prefix(cap - 1),
            usize::try_from(cap - 1).unwrap(),
        ),
    ]);
    cases
}

/// Shapes 4 and 5. A payload that is not a Blob message is the one case
/// the sources do not share; it is pinned by its own test below.
fn payload_cases(fx: &Fixture) -> Vec<Case> {
    let (p, d) = (fx.p, fx.d);
    let mut cases = Vec::new();
    for (label, cut) in [
        ("at the payload start", p),
        ("mid-payload", p + d / 2),
        ("one byte short", p + d - 1),
    ] {
        cases.push(Case {
            text: truncation("its BlobHeader", "Blob", d, p),
            ..case(
                format!("shape 4: last payload cut {label}"),
                fx.cut(cut),
                fx.refusal(Kind::Eof),
            )
        });
    }
    // Shape 5 is not a frame-layer shape: every source passes the frame,
    // and the damage surfaces at decode (pinned by the test below).
    cases.push(case(
        "shape 5: zlib stream damaged",
        fx.zlib_damaged(),
        Seen::End { frames: 3 },
    ));
    cases
}

/// The size guards, the BlobHeader parse and the header-position rule, all
/// refused before the payload is touched.
fn guard_cases(fx: &Fixture) -> Vec<Case> {
    let (h, d) = (fx.h, fx.d);
    let cap = u32::try_from(MAX_BLOB_HEADER_SIZE).unwrap();
    let data =
        |datasize: u64| fx.with_last_frame(&blob_header("OSMData", Some(datasize)), fx.payload());
    vec![
        case(
            "prefix 0",
            fx.with_prefix(0),
            fx.refusal(Kind::InvalidHeaderSize),
        ),
        case(
            "prefix at the header cap",
            fx.with_prefix(cap),
            fx.refusal(Kind::HeaderTooBig(u64::from(cap))),
        ),
        case(
            "prefix 0xFFFFFFFF",
            fx.with_prefix(u32::MAX),
            fx.refusal(Kind::HeaderTooBig(u64::from(u32::MAX))),
        ),
        case(
            "datasize at the cap",
            data(MAX_BLOB_DATASIZE),
            fx.refusal(Kind::DataSizeTooBig(MAX_BLOB_DATASIZE)),
        ),
        case(
            "datasize -1",
            data(u64::MAX),
            fx.refusal(Kind::InvalidDataSize(-1)),
        ),
        case(
            "datasize past int32",
            data((1_u64 << 32) + 100),
            fx.refusal(Kind::Wire),
        ),
        case(
            "BlobHeader bytes unparseable",
            fx.with_last_frame(&vec![0u8; h], fx.payload()),
            fx.refusal(Kind::Wire),
        ),
        case(
            "BlobHeader without a type",
            fx.with_last_frame(&blob_header("", Some(u(d))), fx.payload()),
            fx.refusal(Kind::Wire),
        ),
        // The position rule fires before the missing payload is noticed.
        case(
            "second OSMHeader, payload absent",
            fx.with_last_frame(&blob_header("OSMHeader", Some(u(d))), &[]),
            fx.refusal(Kind::Wire),
        ),
    ]
}

/// Just inside each guard: the largest accepted values, and the empty payload.
fn boundary_cases(fx: &Fixture) -> Vec<Case> {
    let max_data = usize::try_from(MAX_BLOB_DATASIZE).unwrap();
    let header_max = usize::try_from(MAX_BLOB_HEADER_SIZE).unwrap() - 1;

    let below_cap = blob_header("OSMData", Some(u(max_data - 1)));
    let below_cap_payload = fx.f + 4 + below_cap.len();
    vec![
        // Passes the datasize guard, then is shape 4.
        Case {
            text: truncation("its BlobHeader", "Blob", max_data - 1, below_cap_payload),
            ..case(
                "datasize one below the cap, payload absent",
                fx.with_last_frame(&below_cap, &[]),
                fx.refusal(Kind::Eof),
            )
        },
        case(
            "datasize 0, file ends after the header",
            fx.with_last_frame(&blob_header("OSMData", Some(0)), &[]),
            Seen::End { frames: 3 },
        ),
        // Wider than the walk's probe window, so it takes the second pread.
        case(
            "valid BlobHeader one below the cap",
            fx.with_last_frame(&padded_header(fx.d, header_max), fx.payload()),
            Seen::End { frames: 3 },
        ),
    ]
}

/// Every truncation shape, guard and boundary through every frame source,
/// with one expectation per case, and the same error text from every
/// source that refuses. All mismatches are reported at once, as
/// `case: source`.
#[test]
fn every_frame_source_on_every_truncation_shape() {
    let fx = Fixture::new();
    let mut cases = frame_and_header_cases(&fx);
    cases.extend(payload_cases(&fx));
    cases.extend(guard_cases(&fx));
    cases.extend(boundary_cases(&fx));

    let mut mismatches = Vec::new();
    for (i, case) in cases.iter().enumerate() {
        let path = fx.dir.path().join(format!("case{i}.osm.pbf"));
        std::fs::write(&path, &case.bytes).unwrap();
        let mut texts: Vec<(Source, String)> = Vec::new();
        for source in SOURCES {
            let (seen, text) = run(source, &path);
            if seen != case.expect {
                mismatches.push(format!(
                    "{}: {source:?}\n  expected {:?}\n  seen     {seen:?}",
                    case.name, case.expect
                ));
            }
            if let Some(text) = text {
                if let Some(want) = &case.text
                    && !text.contains(want.as_str())
                {
                    mismatches.push(format!(
                        "{}: {source:?}\n  text {text:?}\n  lacks {want:?}",
                        case.name
                    ));
                }
                texts.push((source, text));
            }
        }
        if let Some((first_source, first)) = texts.first() {
            for (source, text) in &texts[1..] {
                if text != first {
                    mismatches.push(format!(
                        "{}: {source:?} words it differently from {first_source:?}\n  \
                         {first:?}\n  {text:?}",
                        case.name
                    ));
                }
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} cell(s) differ from the table:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

/// The one difference between the sources, and it is not framing: only
/// `BlobReader::next` parses the `Blob` envelope, so a payload that is not a
/// `Blob` message is refused there - located at the frame, like every other
/// failure of a blob - and delivered by the rest, which surface it at
/// decode or pass it through untouched.
#[test]
fn only_the_blob_reader_parses_the_envelope() {
    let fx = Fixture::new();
    let mut garbage = fx.intact.clone();
    garbage[fx.p..].fill(0);
    let path = fx.dir.path().join("garbage-payload.osm.pbf");
    std::fs::write(&path, &garbage).unwrap();
    for source in SOURCES {
        let expect = match source {
            Source::Read => fx.refusal(Kind::Wire),
            _ => Seen::End { frames: 3 },
        };
        assert_eq!(run(source, &path).0, expect, "{source:?}");
    }
}

/// The shape-5 fixture above really is shape 5: the frame every source
/// passed fails at decode, and the same blob intact decodes.
#[test]
fn the_damaged_zlib_stream_fails_at_decode() {
    let fx = Fixture::new();
    let decode_last = |bytes: &[u8]| {
        let blobs = BlobReader::new(std::io::Cursor::new(bytes.to_vec()))
            .collect::<crate::error::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(blobs.len(), 3);
        blobs[2].decode().map(|_| ())
    };
    decode_last(&fx.intact).unwrap();
    assert!(decode_last(&fx.zlib_damaged()).is_err());
}
