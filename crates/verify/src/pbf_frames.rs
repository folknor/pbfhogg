//! A header-only walk of a PBF's blob framing and its blob-level indexdata.
//!
//! `verify getid-removeid` picks the IDs it requests from the dataset under
//! test, and at planet scale it may not read the dataset to do so; `verify
//! check-ids` reads from the same walk the kind claims pbfhogg judges type
//! order from ([`framing`]), and `verify repack` and `verify degrade` read
//! every blob's claim, pulled one blob at a time ([`Claims`]), to hold an
//! output's block layout to a prediction. This walk
//! reads each blob's `BlobHeader` (a few dozen bytes) and seeks past the
//! payload without reading it, so its I/O is proportional to the blob count,
//! never to the data. The one payload it reads is the leading `OSMHeader`'s,
//! for the header's required features; [`blob_ids`] reads one named data
//! blob's elements on request, the bounded exception `verify degrade` takes
//! for the few elements it locates, and [`index_truth`] reads every data
//! blob carrying indexdata to hold each claim's kind, count and ID range to
//! its elements. [`header`] reads the header's required and optional
//! features, the claims checks route on, without asking the pbfhogg CLI
//! under test.
//!
//! Where pbfhogg and libosmium disagree on a frame, the payload or header
//! fields it reads, the reader refuses: the checks that read content also
//! judge osmium's decoding, so admitting what libosmium refuses (zstd, a
//! compressed blob without a valid `raw_size`) adds nothing, and admitting
//! what pbfhogg refuses (an unsupported required feature, invalid UTF-8 in a
//! header string or blob type, an ID split across `DenseNodes` occurrences)
//! could pass an output pbfhogg cannot reopen. reference/verify.md lists the
//! rules and the places it is stricter than both. Optional fields are
//! recognised by number and wire type as both readers recognise them; the
//! framing fields `type` and `datasize` refuse another wire type, as pbfhogg
//! does. The header's bbox is judged for shape only (at most one occurrence,
//! all four coordinates present); its values are not read. It never reads a value differently
//! from them.
//!
//! The indexdata decoder follows pbfhogg's wire format (its `blob_meta`
//! module): a version byte (1 with exactly 26 bytes, 2 or 3 with exactly 42),
//! the element kind ordinal (node 0, way 1, relation 2), the minimum and
//! maximum ID as little-endian `i64` at offsets 2 and 10, and the element
//! count as little-endian `u64` at 18. It refuses every claim pbfhogg cannot
//! read - an unknown version, an unknown kind, an inverted range - and two it
//! reads as no index for that blob: a length the version does not declare,
//! and two non-empty indexdata fields on one header. A verifier treats a
//! present but unusable claim as a finding, never as absence. Every claim
//! this walk accepts is one pbfhogg reads on its indexdata path. An unknown
//! future version is a loud refusal naming it.
//!
//! The walk assumes the file is not modified in place while it runs: one
//! open handle serves the whole walk, and its length is read once.

#![allow(
    dead_code,
    reason = "shared verifier helpers consumed by the remaining family slices"
)]

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::Error;

/// Who wrote the file a reader opens, which decides what its refusal says.
///
/// The same reader judges a pbfhogg output and reads the files a comparison
/// stands on, so every entry point that opens a file is told which it is.
/// On a pbfhogg output a refusal (or a file that cannot be opened) is a
/// check failure: verify's pbfhogg runs are fresh writes, whose every
/// refusal breaks the writer's contract (zlib by default, `raw_size` always
/// written, one ID per element), or passthroughs of input blobs the
/// comparison already read. On any other file - the dataset input, an
/// osmium output or copy, a fixture the verifier changed - the reader's
/// refusal says the comparison cannot be made on it, a verifier error, never
/// a pbfhogg verdict. A file two producers wrote is read as each before and
/// after the other's change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// Bytes a pbfhogg run under test wrote.
    Pbfhogg,
    /// The dataset input, a reference tool's output, or a verifier-made file.
    Other,
}

impl Origin {
    /// The error a refusal or I/O failure on a file of this origin is.
    pub(crate) fn refuse(self, why: String) -> Error {
        match self {
            Self::Pbfhogg => Error::Failed(why),
            Self::Other => Error::Tool(format!("{why}; the comparison cannot be made on it")),
        }
    }
}

/// pbfhogg refuses a `BlobHeader` length of zero or of 64 KiB and above.
const MAX_HEADER_LEN: u32 = 64 * 1024;
/// pbfhogg refuses a blob payload (`datasize`) of 32 MiB and above, and the
/// same bound caps a decompressed `OSMHeader`.
const MAX_BLOB_LEN: u64 = 32 * 1024 * 1024;

/// What the indexdata claims about one element kind across the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KindClaim {
    pub min_id: i64,
    pub max_id: i64,
    pub count: u64,
}

/// The walk's answer: per kind (node, way, relation) the claimed ID range
/// over blobs claiming a nonzero count, and the header's required features.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct IndexSummary {
    pub kinds: [Option<KindClaim>; 3],
    pub required_features: Vec<String>,
}

impl IndexSummary {
    pub fn count(&self, kind: usize) -> u64 {
        self.kinds
            .get(kind)
            .copied()
            .flatten()
            .map_or(0, |claim| claim.count)
    }

    pub fn is_history(&self) -> bool {
        self.required_features
            .iter()
            .any(|f| f == "HistoricalInformation")
    }
}

/// Walk `path`'s framing; refuse anything pbfhogg would not read through its
/// indexdata. Errors name the blob ordinal and offset.
pub(crate) fn walk(path: &Path, origin: Origin) -> Result<IndexSummary, Error> {
    let (mut file, len) = open(path, origin)?;
    walk_reader(&mut file, len).map_err(|why| {
        origin.refuse(format!(
            "{} is not a fully indexed PBF this check can select IDs from: {why}",
            path.display()
        ))
    })
}

/// Open `path` and read its length, an error of `origin` if either fails.
fn open(path: &Path, origin: Origin) -> Result<(std::fs::File, u64), Error> {
    let file = std::fs::File::open(path)
        .map_err(|e| origin.refuse(format!("cannot open {}: {e}", path.display())))?;
    let len = file
        .metadata()
        .map_err(|e| origin.refuse(format!("cannot stat {}: {e}", path.display())))?
        .len();
    Ok((file, len))
}

fn walk_reader(r: &mut (impl Read + Seek), len: u64) -> Result<IndexSummary, String> {
    let mut summary = IndexSummary::default();
    let features = walk_frames(r, len, |indexdata| {
        let index = match indexdata {
            Indexdata::Absent => return Err("data blob has no indexdata".into()),
            Indexdata::Conflicting => return Err("data blob has two indexdata fields".into()),
            Indexdata::Present(bytes) => decode_index(&bytes)?,
        };
        fold(&mut summary, index)
    })?;
    summary.required_features = features;
    Ok(summary)
}

/// How a PBF's data blobs carry indexdata, read by the same header-only walk:
/// what pbfhogg's `check --ids` reads its kind claims from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
    /// No data blob carries indexdata.
    Unindexed,
    /// Every data blob carries indexdata, or there is no data blob (pbfhogg
    /// reads a header-only file as indexed). `claims_in_kind_order` is
    /// whether the kind every data blob claims, an empty one's included,
    /// never decreases from node to way to relation: on an indexed file
    /// pbfhogg's `check --ids` judges type order from those claims, not from
    /// the decoded elements.
    Indexed { claims_in_kind_order: bool },
}

/// Read `path`'s [`Framing`], refusing what pbfhogg could not read through
/// its indexdata (as [`walk`] does) and a file mixing data blobs with and
/// without indexdata, which pbfhogg classifies by its first blob alone.
pub(crate) fn framing(path: &Path, origin: Origin) -> Result<Framing, Error> {
    let (mut file, len) = open(path, origin)?;
    framing_reader(&mut file, len).map_err(|why| {
        origin.refuse(format!(
            "cannot read the framing of {}: {why}",
            path.display()
        ))
    })
}

fn framing_reader(r: &mut (impl Read + Seek), len: u64) -> Result<Framing, String> {
    let (mut with, mut without) = (0u64, 0u64);
    let mut highest: Option<usize> = None;
    let mut in_order = true;
    walk_frames(r, len, |indexdata| {
        match indexdata {
            Indexdata::Absent => without += 1,
            Indexdata::Conflicting => return Err("data blob has two indexdata fields".into()),
            Indexdata::Present(bytes) => {
                let index = decode_index(&bytes)?;
                with += 1;
                if highest.is_some_and(|h| index.kind < h) {
                    in_order = false;
                }
                highest = Some(highest.map_or(index.kind, |h| h.max(index.kind)));
            }
        }
        Ok(())
    })?;
    match (with, without) {
        (_, 0) => Ok(Framing::Indexed {
            claims_in_kind_order: in_order,
        }),
        (0, _) => Ok(Framing::Unindexed),
        (with, without) => Err(format!(
            "{with} data blobs carry indexdata and {without} do not"
        )),
    }
}

/// Walk every frame, handing each data blob's indexdata to `on_data` and
/// seeking past its payload; returns the header's required features.
fn walk_frames(
    r: &mut (impl Read + Seek),
    len: u64,
    mut on_data: impl FnMut(Indexdata) -> Result<(), String>,
) -> Result<Vec<String>, String> {
    let mut frames = FrameWalk::open(r, len)?;
    while let Some(frame) = frames.next_data()? {
        let at = frame.at();
        on_data(frame.indexdata).map_err(|why| format!("{at}: {why}"))?;
    }
    Ok(frames.required_features)
}

/// One data blob's frame: its indexdata and where its payload lies.
struct DataFrame {
    indexdata: Indexdata,
    /// The frame's ordinal among every blob, the OSMHeader's being 0.
    ordinal: u64,
    /// Where the frame starts.
    offset: u64,
    payload_offset: u64,
    datasize: u64,
}

impl DataFrame {
    fn at(&self) -> String {
        format!("blob {} at byte {}", self.ordinal, self.offset)
    }
}

/// The frames of one PBF, pulled one data blob at a time: the leading
/// `OSMHeader` is read by [`open`](Self::open), every data payload is sought
/// past, never read.
struct FrameWalk<'r, R> {
    r: &'r mut R,
    len: u64,
    offset: u64,
    ordinal: u64,
    required_features: Vec<String>,
    optional_features: Vec<String>,
}

impl<'r, R: Read + Seek> FrameWalk<'r, R> {
    fn open(r: &'r mut R, len: u64) -> Result<Self, String> {
        let mut walk = Self {
            r,
            len,
            offset: 0,
            ordinal: 0,
            required_features: Vec::new(),
            optional_features: Vec::new(),
        };
        if len == 0 {
            return Err("no OSMHeader blob".into());
        }
        let (header, payload_end) = walk.frame_header()?;
        let at = walk.at();
        if header.blob_type != "OSMHeader" {
            return Err(format!(
                "{at}: {}",
                if header.blob_type == "OSMData" {
                    "data blob before the OSMHeader".to_owned()
                } else {
                    format!("unknown blob type {:?}", header.blob_type)
                }
            ));
        }
        let size = usize::try_from(header.datasize).map_err(|e| format!("{at}: {e}"))?;
        let mut payload = vec![0u8; size];
        walk.r
            .read_exact(&mut payload)
            .map_err(|e| format!("{at}: {e}"))?;
        let block = blob_content(&payload).map_err(|why| format!("{at}: {why}"))?;
        (walk.required_features, walk.optional_features) =
            header_features(&block).map_err(|why| format!("{at}: {why}"))?;
        walk.offset = payload_end;
        walk.ordinal = 1;
        Ok(walk)
    }

    fn at(&self) -> String {
        format!("blob {} at byte {}", self.ordinal, self.offset)
    }

    /// Read the frame header at the current offset, leaving the reader at
    /// its payload; returns the header and where the payload ends.
    fn frame_header(&mut self) -> Result<(BlobHeader, u64), String> {
        let at = |why: String| format!("blob {} at byte {}: {why}", self.ordinal, self.offset);
        let header_len_end = self
            .offset
            .checked_add(4)
            .filter(|end| *end <= self.len)
            .ok_or_else(|| at("truncated blob header length".into()))?;
        let mut len_bytes = [0u8; 4];
        self.r
            .read_exact(&mut len_bytes)
            .map_err(|e| at(e.to_string()))?;
        let header_len = u32::from_be_bytes(len_bytes);
        if header_len == 0 || header_len >= MAX_HEADER_LEN {
            return Err(at(format!("blob header length {header_len} out of range")));
        }
        let header_end = header_len_end
            .checked_add(u64::from(header_len))
            .filter(|end| *end <= self.len)
            .ok_or_else(|| at("truncated blob header".into()))?;
        let mut header = vec![0u8; header_len as usize];
        self.r
            .read_exact(&mut header)
            .map_err(|e| at(e.to_string()))?;
        let header = parse_blob_header(&header).map_err(at)?;
        if header.datasize >= MAX_BLOB_LEN {
            return Err(at(format!("datasize {} out of range", header.datasize)));
        }
        let payload_end = header_end
            .checked_add(header.datasize)
            .filter(|end| *end <= self.len)
            .ok_or_else(|| at("truncated blob payload".into()))?;
        Ok((header, payload_end))
    }

    /// The next data blob, its payload sought past; `None` at the end.
    fn next_data(&mut self) -> Result<Option<DataFrame>, String> {
        if self.offset >= self.len {
            return Ok(None);
        }
        let (header, payload_end) = self.frame_header()?;
        let at = self.at();
        match header.blob_type.as_str() {
            "OSMData" => {}
            "OSMHeader" => return Err(format!("{at}: OSMHeader not first, or repeated")),
            other => return Err(format!("{at}: unknown blob type {other:?}")),
        }
        let frame = DataFrame {
            indexdata: header.indexdata,
            ordinal: self.ordinal,
            offset: self.offset,
            payload_offset: payload_end - header.datasize,
            datasize: header.datasize,
        };
        self.r
            .seek(SeekFrom::Start(payload_end))
            .map_err(|e| format!("{at}: {e}"))?;
        self.offset = payload_end;
        self.ordinal += 1;
        Ok(Some(frame))
    }
}

/// What one data blob's indexdata claims about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlobClaim {
    /// Node 0, way 1, relation 2.
    pub kind: usize,
    pub min_id: i64,
    pub max_id: i64,
    pub count: u64,
}

/// One data blob as [`Claims`] reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClaimedBlob {
    /// `None` for a blob without indexdata.
    pub claim: Option<BlobClaim>,
    /// The blob's ordinal among the data blobs, from 0.
    pub index: u64,
    /// Where its payload (the `Blob` message) lies, for [`blob_ids`].
    pub payload_offset: u64,
    pub datasize: u64,
}

/// The data blobs of a PBF with what each one's indexdata claims, pulled one
/// at a time by the same header-only walk as [`walk`]: two files can be read
/// side by side in constant memory. A blob with two non-empty indexdata
/// fields, or with one that does not decode, is an error.
pub(crate) struct Claims {
    path: std::path::PathBuf,
    origin: Origin,
    file: std::io::BufReader<std::fs::File>,
    len: u64,
    offset: u64,
    ordinal: u64,
    index: u64,
    required_features: Vec<String>,
}

impl Claims {
    pub(crate) fn open(path: &Path, origin: Origin) -> Result<Self, Error> {
        let (file, len) = open(path, origin)?;
        let mut file = std::io::BufReader::new(file);
        let walk =
            FrameWalk::open(&mut file, len).map_err(|why| Self::error_at(path, origin, &why))?;
        let (offset, ordinal, required_features) =
            (walk.offset, walk.ordinal, walk.required_features);
        Ok(Self {
            path: path.to_path_buf(),
            origin,
            file,
            len,
            offset,
            ordinal,
            index: 0,
            required_features,
        })
    }

    /// The file's origin, which the per-blob readers it names are read under.
    pub(crate) fn origin(&self) -> Origin {
        self.origin
    }

    fn error_at(path: &Path, origin: Origin, why: &str) -> Error {
        origin.refuse(format!(
            "cannot read the blob claims of {}: {why}",
            path.display()
        ))
    }

    /// The header's required features.
    pub(crate) fn required_features(&self) -> &[String] {
        &self.required_features
    }

    /// The next data blob, `None` at the end of the file.
    pub(crate) fn next_blob(&mut self) -> Result<Option<ClaimedBlob>, Error> {
        let mut walk = FrameWalk {
            r: &mut self.file,
            len: self.len,
            offset: self.offset,
            ordinal: self.ordinal,
            required_features: Vec::new(),
            optional_features: Vec::new(),
        };
        let frame = walk
            .next_data()
            .map_err(|why| Self::error_at(&self.path, self.origin, &why))?;
        self.offset = walk.offset;
        self.ordinal = walk.ordinal;
        let Some(frame) = frame else {
            return Ok(None);
        };
        let claim = match &frame.indexdata {
            Indexdata::Absent => None,
            Indexdata::Conflicting => {
                return Err(Self::error_at(
                    &self.path,
                    self.origin,
                    &format!("{}: data blob has two indexdata fields", frame.at()),
                ));
            }
            Indexdata::Present(bytes) => {
                let index = decode_index(bytes).map_err(|why| {
                    Self::error_at(&self.path, self.origin, &format!("{}: {why}", frame.at()))
                })?;
                Some(BlobClaim {
                    kind: index.kind,
                    min_id: index.min_id,
                    max_id: index.max_id,
                    count: index.count,
                })
            }
        };
        let blob = ClaimedBlob {
            claim,
            index: self.index,
            payload_offset: frame.payload_offset,
            datasize: frame.datasize,
        };
        self.index += 1;
        Ok(Some(blob))
    }
}

/// Whether `path`'s first data blob carries indexdata: the probe pbfhogg's
/// raw-copying `cat` paths decide their way handling from. A file without
/// a data blob answers false here, where pbfhogg's probe answers true
/// (vacuously indexed); such a file has no ways for the answer to govern.
pub(crate) fn first_data_blob_indexed(path: &Path, origin: Origin) -> Result<bool, Error> {
    Ok(Claims::open(path, origin)?
        .next_blob()?
        .is_some_and(|blob| blob.claim.is_some()))
}

/// The elements of one data blob, as `(kind, id)` in the order the blob
/// stores them: its groups in turn, and within a group its nodes, dense
/// nodes, ways and relations in field order. The payload at
/// `payload_offset` (`datasize` bytes, as [`Claims`] reports them) is read
/// and decompressed, raw or zlib; any other compression is an error naming
/// it. Changesets are skipped. Bounded by one blob.
pub(crate) fn blob_ids(
    path: &Path,
    origin: Origin,
    payload_offset: u64,
    datasize: u64,
) -> Result<Vec<(usize, i64)>, Error> {
    let (block, fail) = read_block(path, origin, payload_offset, datasize)?;
    block_ids(&block).map_err(fail)
}

/// [`blob_ids`] plus whether every group of the block carries at most one
/// element field (plain nodes, dense nodes, ways or relations): a group with
/// two is a block no single index describes, whatever its IDs decode to.
pub(crate) fn blob_ids_and_shape(
    path: &Path,
    origin: Origin,
    payload_offset: u64,
    datasize: u64,
) -> Result<(Vec<(usize, i64)>, bool), Error> {
    let (block, fail) = read_block(path, origin, payload_offset, datasize)?;
    let ids = block_ids(&block).map_err(&fail)?;
    let mut single = true;
    for field in Fields::new(&block) {
        if let (2, Value::Bytes(group)) = field.map_err(&fail)? {
            let mut seen: Option<u32> = None;
            for f in Fields::new(group) {
                // Only a length-delimited field 1 to 4 is an element field;
                // another wire type is an unknown field `block_ids` skips,
                // so it never makes a group mixed.
                if let (number @ 1..=4, Value::Bytes(_)) = f.map_err(&fail)? {
                    if seen.is_some_and(|s| s != number) {
                        single = false;
                    }
                    seen = Some(number);
                }
            }
        }
    }
    Ok((ids, single))
}

/// The decompressed `PrimitiveBlock` of the payload at `payload_offset`,
/// with the error constructor naming that blob.
fn read_block(
    path: &Path,
    origin: Origin,
    payload_offset: u64,
    datasize: u64,
) -> Result<(Vec<u8>, impl Fn(String) -> Error), Error> {
    let display = path.display().to_string();
    let fail = move |why: String| {
        origin.refuse(format!(
            "cannot read the data blob at byte {payload_offset} of {display}: {why}"
        ))
    };
    if datasize >= MAX_BLOB_LEN {
        return Err(fail(format!("datasize {datasize} out of range")));
    }
    let mut file = std::fs::File::open(path).map_err(|e| fail(e.to_string()))?;
    file.seek(SeekFrom::Start(payload_offset))
        .map_err(|e| fail(e.to_string()))?;
    let size = usize::try_from(datasize).map_err(|e| fail(e.to_string()))?;
    let mut payload = vec![0u8; size];
    file.read_exact(&mut payload)
        .map_err(|e| fail(e.to_string()))?;
    let block = blob_content(&payload).map_err(&fail)?;
    Ok((block, fail))
}

/// What the verifier's own decode of every data blob finds against the
/// blob's indexdata: the part of an indexdata claim it holds independently
/// of pbfhogg (kind, count, exact minimum and maximum ID). Bounding boxes
/// and tag indexes are not read here.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct IndexTruth {
    pub blobs: u64,
    pub blobs_with_indexdata: u64,
    pub violations: u64,
    /// The first few violations, each naming its data blob.
    pub shown: Vec<String>,
}

impl IndexTruth {
    pub(crate) fn passed(&self) -> bool {
        self.violations == 0
    }
}

/// Decode every data blob of `path` that carries indexdata (raw or zlib; any
/// other compression is an error naming it, never a skip; a blob without
/// indexdata is counted, not decoded) and hold each claim to the blob's
/// elements: every element of the claimed kind, the claimed count,
/// and, for a nonempty blob, the claimed minimum and maximum ID exactly.
pub(crate) fn index_truth(path: &Path, origin: Origin) -> Result<IndexTruth, Error> {
    const SHOWN: usize = 10;
    let mut truth = IndexTruth::default();
    let mut claims = Claims::open(path, origin)?;
    while let Some(blob) = claims.next_blob()? {
        truth.blobs += 1;
        let Some(claim) = blob.claim else { continue };
        truth.blobs_with_indexdata += 1;
        let ids = blob_ids(path, origin, blob.payload_offset, blob.datasize)?;
        let mut found = Vec::new();
        if let Some((kind, id)) = ids.iter().find(|(kind, _)| *kind != claim.kind) {
            found.push(format!(
                "claims kind {} but holds element {id} of kind {kind}",
                claim.kind
            ));
        }
        if ids.len() as u64 != claim.count {
            found.push(format!(
                "claims {} elements but holds {}",
                claim.count,
                ids.len()
            ));
        }
        if let (Some(min), Some(max)) = (
            ids.iter().map(|(_, id)| *id).min(),
            ids.iter().map(|(_, id)| *id).max(),
        ) {
            if min != claim.min_id {
                found.push(format!(
                    "claims min_id {} but its smallest ID is {min}",
                    claim.min_id
                ));
            }
            if max != claim.max_id {
                found.push(format!(
                    "claims max_id {} but its largest ID is {max}",
                    claim.max_id
                ));
            }
        }
        for why in found {
            truth.violations += 1;
            if truth.shown.len() < SHOWN {
                truth.shown.push(format!("data blob {}: {why}", blob.index));
            }
        }
    }
    Ok(truth)
}

/// The `(kind, id)` of every element of a `PrimitiveBlock`.
///
/// Fields are recognised by number and wire type together, as pbfhogg's
/// block, group and element parsers and libosmium's `pbf_decoder` do: a
/// known number with another wire type is an unknown field and is skipped,
/// so a file both readers accept is never refused here for it. Changesets
/// (group field 5) are skipped by both and here too. What this walk refuses
/// beyond them - a repeated or absent scalar ID, a wrapping dense delta, an
/// ID split across `DenseNodes` occurrences the two readers resolve
/// differently - is its declared domain, never a reading of its own.
fn block_ids(block: &[u8]) -> Result<Vec<(usize, i64)>, String> {
    let mut ids = Vec::new();
    for field in Fields::new(block) {
        if let (2, Value::Bytes(group)) = field? {
            group_ids(group, &mut ids)?;
        }
    }
    Ok(ids)
}

fn group_ids(group: &[u8], ids: &mut Vec<(usize, i64)>) -> Result<(), String> {
    let mut dense_seen = false;
    for field in Fields::new(group) {
        match field? {
            (1, Value::Bytes(node)) => ids.push((0, zigzag(message_id(node)?))),
            (2, Value::Bytes(dense)) => {
                // pbfhogg refuses a second `dense` field in one group, empty
                // or not; libosmium decodes each.
                if std::mem::replace(&mut dense_seen, true) {
                    return Err("a group holds two DenseNodes".into());
                }
                dense_ids(dense, ids)?;
            }
            (3, Value::Bytes(way)) => ids.push((1, int64(message_id(way)?))),
            (4, Value::Bytes(relation)) => ids.push((2, int64(message_id(relation)?))),
            _ => {}
        }
    }
    Ok(())
}

/// Field 1 of a `Node`, `Way` or `Relation` message, as its raw varint.
/// Another wire type on field 1 is an unknown field, skipped.
fn message_id(message: &[u8]) -> Result<u64, String> {
    let mut id = None;
    for field in Fields::new(message) {
        if let (1, Value::Varint(v)) = field?
            && id.replace(v).is_some()
        {
            return Err("an element carries two ids".into());
        }
    }
    id.ok_or_else(|| "an element without an id".to_owned())
}

/// `DenseNodes.id` (field 1): delta-coded `sint64`, packed or not.
///
/// The IDs must be ones pbfhogg and libosmium both read: pbfhogg refuses a
/// second non-empty occurrence and ignores an empty packed one, libosmium
/// takes the last occurrence, empty included. So a second non-empty
/// occurrence, or an empty packed one after a non-empty one, is refused; a
/// leading empty one agrees in both and is read as nothing.
fn dense_ids(dense: &[u8], ids: &mut Vec<(usize, i64)>) -> Result<(), String> {
    let mut last: i64 = 0;
    let mut filled = false;
    let mut push = |raw: u64| -> Result<(), String> {
        last = last
            .checked_add(zigzag(raw))
            .ok_or("dense node id overflows")?;
        ids.push((0, last));
        Ok(())
    };
    for field in Fields::new(dense) {
        let field = field?;
        if !matches!(field, (1, Value::Bytes(_) | Value::Varint(_))) {
            continue;
        }
        if filled {
            return Err("dense ids span two occurrences".into());
        }
        match field {
            (_, Value::Bytes(packed)) => {
                let mut values = Fields::new(packed);
                filled = !values.rest.is_empty();
                while !values.rest.is_empty() {
                    push(values.varint()?)?;
                }
            }
            (_, Value::Varint(v)) => {
                filled = true;
                push(v)?;
            }
            (_, Value::Fixed) => {}
        }
    }
    Ok(())
}

fn zigzag(v: u64) -> i64 {
    #[allow(clippy::cast_possible_wrap)] // zigzag decoding is a bit pattern
    let half = (v >> 1) as i64;
    if v & 1 == 0 { half } else { !half }
}

fn int64(v: u64) -> i64 {
    #[allow(clippy::cast_possible_wrap)] // protobuf int64 is the bit pattern
    let signed = v as i64;
    signed
}

/// Whether the data blobs of `a` and `b` carry the same payload bytes in the
/// same sequence, whatever their blob headers say: what a header-only
/// rewrite (`degrade --strip-indexdata`) must keep. `Err` names the first
/// blob that differs; I/O and framing failures are the outer error.
pub(crate) fn same_data_payloads(
    (a, a_origin): (&Path, Origin),
    (b, b_origin): (&Path, Origin),
) -> Result<Result<(), String>, Error> {
    let mut left = Claims::open(a, a_origin)?;
    let mut right = Claims::open(b, b_origin)?;
    let (mut fa, _) = open(a, a_origin)?;
    let (mut fb, _) = open(b, b_origin)?;
    let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let (x, y) = match (left.next_blob()?, right.next_blob()?) {
            (None, None) => return Ok(Ok(())),
            (Some(x), None) => {
                return Ok(Err(format!("data blob {} is only in the first", x.index)));
            }
            (None, Some(y)) => {
                return Ok(Err(format!("data blob {} is only in the second", y.index)));
            }
            (Some(x), Some(y)) => (x, y),
        };
        if x.datasize != y.datasize {
            return Ok(Err(format!(
                "data blob {} holds {} payload bytes in the first and {} in the second",
                x.index, x.datasize, y.datasize
            )));
        }
        fa.seek(SeekFrom::Start(x.payload_offset))?;
        fb.seek(SeekFrom::Start(y.payload_offset))?;
        let mut rest = x.datasize;
        while rest > 0 {
            let n = usize::try_from(rest.min(ba.len() as u64))
                .map_err(|e| Error::tool(e.to_string()))?;
            fa.read_exact(&mut ba[..n])?;
            fb.read_exact(&mut bb[..n])?;
            if ba[..n] != bb[..n] {
                return Ok(Err(format!("data blob {} payload bytes differ", x.index)));
            }
            rest -= n as u64;
        }
    }
}

/// Whether data blob `x` of `a` and data blob `y` of `b` carry the same
/// serialized `Blob` payload, compression included, whatever their blob
/// headers say.
pub(crate) fn same_payload(
    (a, a_origin): (&Path, Origin),
    x: &ClaimedBlob,
    (b, b_origin): (&Path, Origin),
    y: &ClaimedBlob,
) -> Result<bool, Error> {
    if x.datasize != y.datasize {
        return Ok(false);
    }
    let (mut fa, _) = open(a, a_origin)?;
    let (mut fb, _) = open(b, b_origin)?;
    fa.seek(SeekFrom::Start(x.payload_offset))?;
    fb.seek(SeekFrom::Start(y.payload_offset))?;
    let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    let mut rest = x.datasize;
    while rest > 0 {
        let n =
            usize::try_from(rest.min(ba.len() as u64)).map_err(|e| Error::tool(e.to_string()))?;
        fa.read_exact(&mut ba[..n])?;
        fb.read_exact(&mut bb[..n])?;
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
        rest -= n as u64;
    }
    Ok(true)
}

/// Append to the PBF at `path` one `OSMData` blob holding an empty
/// `PrimitiveBlock` (an empty string table, no groups) whose indexdata
/// (version 1) claims `kind` with no elements: a fixture whose decoded
/// elements can be in order while its kind claims are not. Written raw,
/// uncompressed.
pub(crate) fn append_empty_claim(path: &Path, kind: usize) -> Result<(), Error> {
    use std::io::Write as _;
    let kind = u8::try_from(kind)
        .ok()
        .filter(|k| *k <= 2)
        .ok_or_else(|| Error::tool(format!("element kind {kind} unknown")))?;
    let mut block = Vec::new();
    bytes_field(1, &[], &mut block);
    let mut blob = Vec::new();
    bytes_field(1, &block, &mut blob);
    varint(2 << 3, &mut blob);
    varint(block.len() as u64, &mut blob);
    let mut index = vec![1, kind];
    index.extend_from_slice(&0i64.to_le_bytes());
    index.extend_from_slice(&0i64.to_le_bytes());
    index.extend_from_slice(&0u64.to_le_bytes());
    let mut header = Vec::new();
    bytes_field(1, b"OSMData", &mut header);
    bytes_field(2, &index, &mut header);
    varint(3 << 3, &mut header);
    varint(blob.len() as u64, &mut header);
    let header_len =
        u32::try_from(header.len()).map_err(|e| Error::tool(format!("blob header length: {e}")))?;
    let mut frame = header_len.to_be_bytes().to_vec();
    frame.extend_from_slice(&header);
    frame.extend_from_slice(&blob);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| Error::tool(format!("cannot open {}: {e}", path.display())))?;
    file.write_all(&frame)?;
    file.sync_all()?;
    Ok(())
}

/// A protobuf varint.
fn varint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        #[allow(clippy::cast_possible_truncation)] // masked to seven bits
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// A length-delimited protobuf field.
fn bytes_field(number: u64, v: &[u8], out: &mut Vec<u8>) {
    varint(number << 3 | 2, out);
    varint(v.len() as u64, out);
    out.extend_from_slice(v);
}

fn fold(summary: &mut IndexSummary, index: BlobIndex) -> Result<(), String> {
    if index.count == 0 {
        return Ok(());
    }
    let slot = &mut summary.kinds[index.kind];
    *slot = Some(match *slot {
        None => KindClaim {
            min_id: index.min_id,
            max_id: index.max_id,
            count: index.count,
        },
        Some(claim) => KindClaim {
            min_id: claim.min_id.min(index.min_id),
            max_id: claim.max_id.max(index.max_id),
            count: claim
                .count
                .checked_add(index.count)
                .ok_or("element count overflows")?,
        },
    });
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlobIndex {
    kind: usize,
    min_id: i64,
    max_id: i64,
    count: u64,
}

fn decode_index(bytes: &[u8]) -> Result<BlobIndex, String> {
    let version = bytes.first().copied().ok_or("empty indexdata")?;
    let declared = match version {
        1 => 26,
        2 | 3 => 42,
        other => return Err(format!("indexdata version {other} is not one brokkr reads")),
    };
    if bytes.len() != declared {
        return Err(format!(
            "indexdata version {version} is {} bytes, not {declared}",
            bytes.len()
        ));
    }
    let word = |at: usize| -> [u8; 8] {
        let mut w = [0u8; 8];
        w.copy_from_slice(&bytes[at..at + 8]);
        w
    };
    let kind = usize::from(bytes[1]);
    if kind > 2 {
        return Err(format!("indexdata kind {kind} unknown"));
    }
    let min_id = i64::from_le_bytes(word(2));
    let max_id = i64::from_le_bytes(word(10));
    let count = u64::from_le_bytes(word(18));
    if min_id > max_id {
        return Err(format!("indexdata range {min_id}..{max_id} inverted"));
    }
    Ok(BlobIndex {
        kind,
        min_id,
        max_id,
        count,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Indexdata {
    Absent,
    Present(Vec<u8>),
    Conflicting,
}

struct BlobHeader {
    blob_type: String,
    indexdata: Indexdata,
    datasize: u64,
}

fn parse_blob_header(bytes: &[u8]) -> Result<BlobHeader, String> {
    let mut blob_type = None;
    let mut indexdata = Indexdata::Absent;
    let mut datasize = None;
    for field in Fields::new(bytes) {
        match field? {
            // Each occurrence must be UTF-8, as pbfhogg checks each one; a
            // later valid type does not excuse an earlier invalid one.
            (1, Value::Bytes(v)) => {
                blob_type = Some(
                    String::from_utf8(v.to_vec())
                        .map_err(|_| "blob header type is not UTF-8".to_owned())?,
                );
            }
            (2, Value::Bytes(v)) => {
                // pbfhogg ignores an empty occurrence and reads two non-empty
                // ones as conflicting claims: no usable index.
                if !v.is_empty() {
                    indexdata = match indexdata {
                        Indexdata::Absent => Indexdata::Present(v.to_vec()),
                        _ => Indexdata::Conflicting,
                    };
                }
            }
            (3, Value::Varint(v)) => datasize = Some(int32(v)?),
            // type and datasize frame the blob, so a wrong wire type there is
            // refused, as pbfhogg does. Indexdata is optional: another wire
            // type on field 2 is an unknown field, skipped by pbfhogg and
            // libosmium alike and kept by pbfhogg's header strippers.
            (1 | 3, _) => return Err("blob header field has the wrong wire type".into()),
            _ => {}
        }
    }
    let datasize = datasize.ok_or("blob header has no datasize")?;
    let datasize = u64::try_from(datasize)
        .ok()
        .filter(|d| *d > 0)
        .ok_or_else(|| format!("datasize {datasize} out of range"))?;
    Ok(BlobHeader {
        blob_type: blob_type.ok_or("blob header has no type")?,
        indexdata,
        datasize,
    })
}

/// A protobuf `int32` varint, validated per occurrence as pbfhogg does: a
/// non-negative value fits 31 bits, a negative one arrives sign-extended to
/// 64; anything else is not an `int32`.
fn int32(v: u64) -> Result<i64, String> {
    #[allow(clippy::cast_possible_wrap)] // the sign-extended reading is the point
    let signed = v as i64;
    if i32::try_from(signed).is_ok() {
        Ok(signed)
    } else {
        Err(format!("varint {v} is not an int32"))
    }
}

/// The decompressed content of a `Blob`: `raw` (field 1) or `zlib_data`
/// (field 3) with its `raw_size` (field 2). Two payload fields, or any other
/// compression, are refused.
fn blob_content(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut payload: Option<(u32, &[u8])> = None;
    let mut raw_size = None;
    // libosmium refuses a zero or over-cap raw_size at its occurrence on a
    // compressed blob, even when a later one is valid.
    let mut out_of_range = None;
    for field in Fields::new(bytes) {
        match field? {
            (2, Value::Varint(v)) => {
                let size =
                    u64::try_from(int32(v)?).map_err(|_| "raw_size is negative".to_owned())?;
                if size == 0 || size > MAX_BLOB_LEN {
                    out_of_range.get_or_insert(size);
                }
                raw_size = Some(size);
            }
            (field @ (1 | 3..=7), Value::Bytes(v)) => {
                if payload.replace((field, v)).is_some() {
                    return Err("blob has more than one payload field".into());
                }
            }
            // A known number with another supported wire type is an unknown
            // field, skipped as pbfhogg and libosmium do: it never becomes a
            // payload or a raw_size.
            _ => {}
        }
    }
    match payload {
        Some((1, raw)) => {
            if raw.len() as u64 >= MAX_BLOB_LEN {
                return Err("raw block too large".into());
            }
            // raw_size is optional on a raw blob, but a declared one must
            // match, as pbfhogg checks.
            if let Some(size) = raw_size
                && size != raw.len() as u64
            {
                return Err(format!(
                    "raw block is {} bytes, raw_size says {size}",
                    raw.len()
                ));
            }
            Ok(raw.to_vec())
        }
        Some((3, zlib)) => {
            // libosmium refuses a compressed blob without raw_size, and a
            // negative, zero or over-cap one at the occurrence, where pbfhogg
            // decodes the first and lets a later raw_size override the rest.
            // They stay refused: the checks that read payloads also judge
            // osmium's decoding, so admitting them would judge nothing more.
            let size = raw_size.ok_or("zlib blob has no raw_size")?;
            // A decompressed block of exactly 32 MiB is legal to pbfhogg and
            // libosmium alike.
            if let Some(bad) = out_of_range {
                return Err(format!("raw_size {bad} out of range"));
            }
            let size = usize::try_from(size).map_err(|e| e.to_string())?;
            let mut out = Vec::with_capacity(size);
            let mut decoder = flate2::read::ZlibDecoder::new(zlib).take(size as u64 + 1);
            decoder
                .read_to_end(&mut out)
                .map_err(|e| format!("block does not inflate: {e}"))?;
            if out.len() != size {
                return Err(format!(
                    "block inflates to {} bytes, raw_size says {size}",
                    out.len()
                ));
            }
            Ok(out)
        }
        // zstd (field 7) pbfhogg decodes and libosmium refuses; lz4 the
        // reverse when osmium is built with it; lzma both refuse. The checks
        // that read payloads also judge osmium's decoding of the file, so
        // supporting either here would add no coverage.
        Some((field, _)) => Err(format!("blob compression field {field} unsupported")),
        None => Err("blob has no payload".into()),
    }
}

/// The required features a header may name: the ones pbfhogg implements,
/// matched exactly, as pbfhogg matches them. libosmium prefix-matches the
/// same three names, so every header accepted here both readers accept.
/// Spelled here, not taken from pbfhogg, so the list stays the verifier's.
const READABLE_REQUIRED_FEATURES: [&str; 3] =
    ["OsmSchema-V0.6", "DenseNodes", "HistoricalInformation"];

/// `HeaderBlock.required_features` (field 4) and `optional_features`
/// (field 5), both repeated strings.
///
/// The header must be one pbfhogg reopens: a required feature outside
/// [`READABLE_REQUIRED_FEATURES`] is refused, and so is invalid UTF-8 in
/// any string field pbfhogg decodes as text (the two feature lists,
/// `writingprogram` 16, `source` 17, `osmosis_replication_base_url` 34),
/// where libosmium would carry the bytes. A judged output whose header
/// pbfhogg cannot read therefore never passes on this reader. The bbox
/// submessage (field 1) is refused when repeated or missing a coordinate,
/// as pbfhogg refuses it; its values are not read.
fn header_features(block: &[u8]) -> Result<(Vec<String>, Vec<String>), String> {
    let text = |name: &str, v: &[u8]| {
        String::from_utf8(v.to_vec()).map_err(|_| format!("invalid UTF-8 in {name}"))
    };
    let (mut required, mut optional) = (Vec::new(), Vec::new());
    let mut bbox_seen = false;
    for field in Fields::new(block) {
        match field? {
            (1, Value::Bytes(v)) => {
                if bbox_seen {
                    return Err("HeaderBlock.bbox appears twice".into());
                }
                bbox_seen = true;
                header_bbox_complete(v)?;
            }
            (4, Value::Bytes(v)) => {
                let feature = text("required_features", v)?;
                if !READABLE_REQUIRED_FEATURES.contains(&feature.as_str()) {
                    return Err(format!("required feature {feature:?} unsupported"));
                }
                required.push(feature);
            }
            (5, Value::Bytes(v)) => optional.push(text("optional_features", v)?),
            (16, Value::Bytes(v)) => {
                text("writingprogram", v)?;
            }
            (17, Value::Bytes(v)) => {
                text("source", v)?;
            }
            (34, Value::Bytes(v)) => {
                text("osmosis_replication_base_url", v)?;
            }
            // Another wire type on a known number is an unknown field,
            // skipped as pbfhogg and libosmium do.
            _ => {}
        }
    }
    Ok((required, optional))
}

/// A `HeaderBBox` body (`HeaderBlock` field 1) carries each of its four
/// required coordinates (fields 1 to 4) as a varint, as pbfhogg and libosmium
/// both demand. Only presence is judged; the values are not read.
fn header_bbox_complete(body: &[u8]) -> Result<(), String> {
    let mut seen = [false; 4];
    for field in Fields::new(body) {
        if let (n @ 1..=4, Value::Varint(_)) = field? {
            seen[usize::try_from(n - 1).map_err(|e| e.to_string())?] = true;
        }
    }
    if seen.iter().all(|s| *s) {
        Ok(())
    } else {
        Err("HeaderBlock.bbox is missing a required coordinate".into())
    }
}

/// The `Sort.Type_then_ID` feature pbfhogg branches on.
pub(crate) const SORT_TYPE_THEN_ID: &str = "Sort.Type_then_ID";

/// A PBF's `OSMHeader` features, read by the verifier itself: the header
/// claims a check routes on or holds an output to, never read through the
/// pbfhogg CLI under test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Header {
    pub required: Vec<String>,
    pub optional: Vec<String>,
}

impl Header {
    /// `Sort.Type_then_ID` among the optional features, which is where
    /// pbfhogg reads it (`HeaderBlock::is_sorted`).
    pub(crate) fn optional_sorted(&self) -> bool {
        self.optional_feature(SORT_TYPE_THEN_ID)
    }

    pub(crate) fn optional_feature(&self, feature: &str) -> bool {
        self.optional.iter().any(|f| f == feature)
    }
}

/// Read `path`'s leading `OSMHeader` (and the framing up to it).
pub(crate) fn header(path: &Path, origin: Origin) -> Result<Header, Error> {
    let (file, len) = open(path, origin)?;
    let mut file = std::io::BufReader::new(file);
    let walk = FrameWalk::open(&mut file, len).map_err(|why| {
        origin.refuse(format!(
            "cannot read the header of {}: {why}",
            path.display()
        ))
    })?;
    Ok(Header {
        required: walk.required_features,
        optional: walk.optional_features,
    })
}

/// Protobuf's largest field number, `2^29 - 1`.
const MAX_FIELD_NUMBER: u32 = (1 << 29) - 1;

enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

/// Protobuf fields of one message, in order. Groups (wire types 3 and 4) and
/// unknown wire types are an error.
struct Fields<'a> {
    rest: &'a [u8],
    failed: bool,
}

impl<'a> Fields<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            rest: bytes,
            failed: false,
        }
    }

    fn varint(&mut self) -> Result<u64, String> {
        let mut value: u64 = 0;
        for i in 0..10 {
            let (&b, rest) = self.rest.split_first().ok_or("truncated varint")?;
            self.rest = rest;
            // The tenth byte holds bit 63 only; anything more overflows.
            if i == 9 && b > 1 {
                return Err("varint overflows 64 bits".into());
            }
            value |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err("varint too long".into())
    }

    fn take(&mut self, n: u64) -> Result<&'a [u8], String> {
        let n = usize::try_from(n).map_err(|e| e.to_string())?;
        if n > self.rest.len() {
            return Err("truncated field".into());
        }
        let (head, rest) = self.rest.split_at(n);
        self.rest = rest;
        Ok(head)
    }

    fn field(&mut self) -> Result<(u32, Value<'a>), String> {
        let key = self.varint()?;
        let number = u32::try_from(key >> 3)
            .ok()
            .filter(|n| (1..=MAX_FIELD_NUMBER).contains(n))
            .ok_or("field number out of range")?;
        let value = match key & 7 {
            // Decoded in full even when the field is then skipped, so a
            // skipped varint past 64 bits is refused where pbfhogg's skip
            // only scans for its end: stricter, never a different value.
            0 => Value::Varint(self.varint()?),
            1 => {
                self.take(8)?;
                Value::Fixed
            }
            2 => {
                let n = self.varint()?;
                Value::Bytes(self.take(n)?)
            }
            5 => {
                self.take(4)?;
                Value::Fixed
            }
            wire => return Err(format!("unsupported wire type {wire}")),
        };
        Ok((number, value))
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<(u32, Value<'a>), String>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.rest.is_empty() {
            return None;
        }
        let field = self.field();
        self.failed = field.is_err();
        Some(field)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]
    use super::{
        Framing, IndexSummary, KindClaim, bytes_field, framing_reader, varint, walk_reader,
    };
    use std::io::{Cursor, Write};

    fn index(version: u8, kind: u8, min: i64, max: i64, count: u64) -> Vec<u8> {
        let mut b = vec![version, kind];
        b.extend_from_slice(&min.to_le_bytes());
        b.extend_from_slice(&max.to_le_bytes());
        b.extend_from_slice(&count.to_le_bytes());
        if version != 1 {
            b.extend_from_slice(&[0u8; 16]);
        }
        b
    }

    fn frame(blob_type: &str, indexdata: &[&[u8]], payload: &[u8], out: &mut Vec<u8>) {
        let mut header = Vec::new();
        bytes_field(1, blob_type.as_bytes(), &mut header);
        for idx in indexdata {
            bytes_field(2, idx, &mut header);
        }
        varint(3 << 3, &mut header);
        varint(payload.len() as u64, &mut header);
        out.extend_from_slice(&(header.len() as u32).to_be_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(payload);
    }

    /// A varint field 2 is an unknown field beside the indexdata bytes, as
    /// in pbfhogg and libosmium; a wrong-typed type or datasize is refused,
    /// and two non-empty indexdata occurrences still conflict.
    #[test]
    fn blob_header_varint_field_2_is_unknown() {
        use super::{Indexdata, parse_blob_header};
        let idx = index(2, 0, 1, 2, 3);
        let mut header = Vec::new();
        bytes_field(1, b"OSMData", &mut header);
        varint(2 << 3, &mut header);
        varint(7, &mut header);
        bytes_field(2, &idx, &mut header);
        varint(3 << 3, &mut header);
        varint(10, &mut header);
        let parsed = parse_blob_header(&header).unwrap();
        assert!(matches!(parsed.indexdata, Indexdata::Present(ref v) if *v == idx));

        let mut twice = header.clone();
        bytes_field(2, &idx, &mut twice);
        assert!(matches!(
            parse_blob_header(&twice).unwrap().indexdata,
            Indexdata::Conflicting
        ));

        let mut bad_size = Vec::new();
        bytes_field(1, b"OSMData", &mut bad_size);
        bytes_field(3, b"x", &mut bad_size);
        crate::test_support::refused(
            parse_blob_header(&bad_size),
            "blob header field has the wrong wire type",
        );
    }

    #[test]
    fn wrong_typed_blob_and_feature_fields_are_unknown() {
        use super::{blob_content, header_features};
        let mut block = Vec::new();
        varint(4 << 3, &mut block);
        varint(1, &mut block);
        bytes_field(4, b"OsmSchema-V0.6", &mut block);
        varint((5 << 3) | 5, &mut block);
        block.extend_from_slice(&[0; 4]);
        bytes_field(5, b"Sort.Type_then_ID", &mut block);
        let (required, optional) = header_features(&block).unwrap();
        assert_eq!(required, ["OsmSchema-V0.6"]);
        assert_eq!(optional, ["Sort.Type_then_ID"]);

        let mut blob = Vec::new();
        varint(1 << 3, &mut blob);
        varint(9, &mut blob);
        bytes_field(2, b"zz", &mut blob);
        varint((3 << 3) | 1, &mut blob);
        blob.extend_from_slice(&[0; 8]);
        bytes_field(1, &block, &mut blob);
        assert_eq!(blob_content(&blob).unwrap(), block);
    }

    /// The header reader admits only a header pbfhogg reopens: exact
    /// supported required features and valid UTF-8 in its text fields.
    #[test]
    fn header_features_refuse_what_pbfhogg_cannot_reopen() {
        use super::header_features;
        let with = |number: u64, v: &[u8]| {
            let mut block = Vec::new();
            bytes_field(4, b"DenseNodes", &mut block);
            bytes_field(number, v, &mut block);
            header_features(&block)
        };
        assert!(with(4, b"HistoricalInformation").is_ok());
        assert!(with(16, b"osmium/1.0").is_ok());
        for (refused, why) in [
            (
                with(4, b"DenseNode"),
                "required feature \"DenseNode\" unsupported",
            ),
            (
                with(4, b"Sort.Type_then_ID"),
                "required feature \"Sort.Type_then_ID\" unsupported",
            ),
            (with(4, b"\xff"), "invalid UTF-8 in required_features"),
            (with(5, b"\xff"), "invalid UTF-8 in optional_features"),
            (with(16, b"\xff"), "invalid UTF-8 in writingprogram"),
            (with(17, b"\xff"), "invalid UTF-8 in source"),
            (
                with(34, b"\xff"),
                "invalid UTF-8 in osmosis_replication_base_url",
            ),
        ] {
            crate::test_support::refused(refused, why);
        }
    }

    /// The header bbox appears at most once with all four coordinates.
    #[test]
    fn header_bbox_must_be_single_and_complete() {
        use super::header_features;
        let complete = [0x08, 0x02, 0x10, 0x04, 0x18, 0x06, 0x20, 0x08];
        let header = |bodies: &[&[u8]]| {
            let mut block = Vec::new();
            for body in bodies {
                bytes_field(1, body, &mut block);
            }
            header_features(&block)
        };
        assert!(header(&[]).is_ok());
        assert!(header(&[&complete]).is_ok());
        crate::test_support::refused(
            header(&[&[]]),
            "HeaderBlock.bbox is missing a required coordinate",
        );
        crate::test_support::refused(
            header(&[&complete[..6]]),
            "HeaderBlock.bbox is missing a required coordinate",
        );
        crate::test_support::refused(header(&[&complete, &[]]), "HeaderBlock.bbox appears twice");
        crate::test_support::refused(
            header(&[&complete, &complete]),
            "HeaderBlock.bbox appears twice",
        );
        // A varint on field 1 is an unknown field, not a second bbox.
        let mut block = Vec::new();
        bytes_field(1, &complete, &mut block);
        varint(1 << 3, &mut block);
        varint(1, &mut block);
        assert!(header_features(&block).is_ok());
    }

    /// A zlib block of exactly 32 MiB inflates; one byte more is refused.
    #[test]
    fn zlib_raw_size_at_the_cap_is_accepted() {
        use super::{MAX_BLOB_LEN, blob_content};
        let blob = |raw_size: u64| {
            let mut out = Vec::new();
            varint(2 << 3, &mut out);
            varint(raw_size, &mut out);
            let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            enc.write_all(&vec![0u8; MAX_BLOB_LEN as usize]).unwrap();
            bytes_field(3, &enc.finish().unwrap(), &mut out);
            out
        };
        assert_eq!(
            blob_content(&blob(MAX_BLOB_LEN)).unwrap().len() as u64,
            MAX_BLOB_LEN
        );
        crate::test_support::refused(
            blob_content(&blob(MAX_BLOB_LEN + 1)),
            "raw_size 33554433 out of range",
        );
    }

    /// The ID walk skips wrong-typed and changeset fields as pbfhogg and
    /// libosmium do, and refuses dense IDs the two readers resolve apart.
    #[test]
    fn id_walk_skips_unknown_fields_and_refuses_split_dense_ids() {
        use super::block_ids;
        let way = |extra: &dyn Fn(&mut Vec<u8>)| {
            let mut way = Vec::new();
            bytes_field(1, &[], &mut way);
            varint(1 << 3, &mut way);
            varint(7, &mut way);
            extra(&mut way);
            way
        };
        let mut group = Vec::new();
        bytes_field(3, &way(&|_| {}), &mut group);
        varint(1 << 3, &mut group);
        varint(5, &mut group);
        bytes_field(5, b"changeset", &mut group);
        let mut block = Vec::new();
        varint(2 << 3, &mut block);
        varint(1, &mut block);
        bytes_field(2, &group, &mut block);
        assert_eq!(block_ids(&block).unwrap(), [(1, 7)]);

        let dense = |occurrences: &[&[u8]]| {
            let mut dense = Vec::new();
            for o in occurrences {
                bytes_field(1, o, &mut dense);
            }
            let mut group = Vec::new();
            bytes_field(2, &dense, &mut group);
            let mut block = Vec::new();
            bytes_field(2, &group, &mut block);
            block_ids(&block)
        };
        // zigzag 2 is ID 1, then a delta of 2 is ID 3.
        assert_eq!(dense(&[&[], &[2, 4]]).unwrap(), [(0, 1), (0, 3)]);
        crate::test_support::refused(dense(&[&[2], &[4]]), "dense ids span two occurrences");
        crate::test_support::refused(dense(&[&[2], &[]]), "dense ids span two occurrences");

        let mut two = Vec::new();
        bytes_field(2, &[], &mut two);
        bytes_field(2, &[], &mut two);
        let mut block = Vec::new();
        bytes_field(2, &two, &mut block);
        crate::test_support::refused(block_ids(&block), "a group holds two DenseNodes");
    }

    fn header_payload(features: &[&str], zlib: bool) -> Vec<u8> {
        let mut block = Vec::new();
        for f in features {
            bytes_field(4, f.as_bytes(), &mut block);
        }
        let mut blob = Vec::new();
        // An empty block compresses to a raw_size of zero, which libosmium
        // and so the reader refuse; it is written raw instead.
        if zlib && !block.is_empty() {
            let mut enc =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(&block).unwrap();
            let compressed = enc.finish().unwrap();
            varint(2 << 3, &mut blob);
            varint(block.len() as u64, &mut blob);
            bytes_field(3, &compressed, &mut blob);
        } else {
            bytes_field(1, &block, &mut blob);
        }
        blob
    }

    fn file(data_blobs: &[Vec<Vec<u8>>], features: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        frame("OSMHeader", &[], &header_payload(features, true), &mut out);
        for idx in data_blobs {
            let refs: Vec<&[u8]> = idx.iter().map(Vec::as_slice).collect();
            frame("OSMData", &refs, b"payload-not-read", &mut out);
        }
        out
    }

    fn run(bytes: &[u8]) -> Result<IndexSummary, String> {
        walk_reader(&mut Cursor::new(bytes), bytes.len() as u64)
    }

    #[test]
    fn folds_endpoints_per_kind_and_skips_empty_claims() {
        let bytes = file(
            &[
                vec![index(3, 0, 5, 90, 10)],
                vec![index(1, 0, 100, 200, 4)],
                vec![index(3, 0, -7, 1_000_000, 0)],
                vec![index(2, 2, 3, 3, 1)],
            ],
            &["OsmSchema-V0.6", "DenseNodes"],
        );
        let summary = run(&bytes).unwrap();
        assert_eq!(
            summary.kinds[0],
            Some(KindClaim {
                min_id: 5,
                max_id: 200,
                count: 14
            })
        );
        assert_eq!(summary.kinds[1], None);
        assert_eq!(summary.count(2), 1);
        assert!(!summary.is_history());
    }

    #[test]
    fn reads_history_from_a_raw_or_zlib_header() {
        let mut bytes = Vec::new();
        frame(
            "OSMHeader",
            &[],
            &header_payload(&["HistoricalInformation"], false),
            &mut bytes,
        );
        assert!(run(&bytes).unwrap().is_history());
        let bytes = file(&[], &["HistoricalInformation"]);
        assert!(run(&bytes).unwrap().is_history());
    }

    #[test]
    fn refuses_what_pbfhogg_cannot_read_through_indexdata() {
        let good = index(3, 0, 1, 2, 2);
        // Each case is the indexdata fields of one data blob.
        let cases: Vec<(Vec<Vec<u8>>, &str)> = vec![
            (vec![], "data blob has no indexdata"),
            (
                vec![index(4, 0, 1, 2, 2)],
                "indexdata version 4 is not one brokkr reads",
            ),
            (
                vec![index(1, 0, 1, 2, 2)[..20].to_vec()],
                "indexdata version 1 is 20 bytes, not 26",
            ),
            (
                vec![{
                    let mut v = index(1, 0, 1, 2, 2);
                    v.extend_from_slice(&[0; 16]);
                    v
                }],
                "indexdata version 1 is 42 bytes, not 26",
            ),
            (vec![index(3, 3, 1, 2, 2)], "indexdata kind 3 unknown"),
            (vec![index(3, 1, 9, 2, 2)], "indexdata range 9..2 inverted"),
            (
                vec![good.clone(), good.clone()],
                "data blob has two indexdata fields",
            ),
        ];
        for (fields, why) in cases {
            crate::test_support::refused(run(&file(&[fields], &[])), why);
        }
        // An empty duplicate occurrence is ignored, as pbfhogg does.
        assert!(run(&file(&[vec![good, Vec::new()]], &[])).is_ok());
    }

    #[test]
    fn refuses_overflowing_varints_and_bad_field_numbers() {
        let mut overflow =
            super::Fields::new(&[0x81, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02]);
        crate::test_support::refused(overflow.varint(), "varint overflows 64 bits");
        let mut top =
            super::Fields::new(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01]);
        assert_eq!(top.varint().unwrap(), 1 << 63);
        crate::test_support::refused(
            super::int32(1 << 32 | 1),
            "varint 4294967297 is not an int32",
        );
        assert_eq!(super::int32(u64::MAX).unwrap(), -1);
        assert_eq!(super::int32(7).unwrap(), 7);
        // Field number zero (key 0x02: field 0, wire type 2).
        crate::test_support::refused(
            super::Fields::new(&[0x02, 0x00]).next().unwrap(),
            "field number out of range",
        );
    }

    #[test]
    fn refuses_bad_framing() {
        let bytes = file(&[vec![index(3, 0, 1, 2, 2)]], &[]);
        crate::test_support::refused(
            run(&bytes[..bytes.len() - 1]),
            "blob 1 at byte 19: truncated blob payload",
        );
        crate::test_support::refused(run(&[]), "no OSMHeader blob");
        let mut twice = bytes.clone();
        frame("OSMHeader", &[], &header_payload(&[], true), &mut twice);
        crate::test_support::refused(
            run(&twice),
            "blob 2 at byte 94: OSMHeader not first, or repeated",
        );
        let mut data_first = Vec::new();
        frame("OSMData", &[&index(3, 0, 1, 2, 2)], b"x", &mut data_first);
        crate::test_support::refused(
            run(&data_first),
            "blob 0 at byte 0: data blob before the OSMHeader",
        );
        let mut zero = vec![0u8; 4];
        zero.extend_from_slice(&bytes);
        crate::test_support::refused(
            run(&zero),
            "blob 0 at byte 0: blob header length 0 out of range",
        );
    }

    fn read_framing(bytes: &[u8]) -> Result<Framing, String> {
        framing_reader(&mut Cursor::new(bytes), bytes.len() as u64)
    }

    #[test]
    fn framing_reads_claim_order_over_every_blob_an_empty_one_included() {
        let ordered = file(
            &[
                vec![index(3, 0, 1, 2, 2)],
                vec![index(3, 0, 3, 3, 1)],
                vec![index(1, 1, 5, 5, 1)],
                vec![index(3, 2, 7, 7, 1)],
            ],
            &[],
        );
        assert_eq!(
            read_framing(&ordered).unwrap(),
            Framing::Indexed {
                claims_in_kind_order: true
            }
        );
        // An empty node claim after a way claim is out of order, though no
        // element is.
        let empty_after = file(
            &[
                vec![index(3, 0, 1, 2, 2)],
                vec![index(3, 1, 5, 5, 1)],
                vec![index(1, 0, 0, 0, 0)],
            ],
            &[],
        );
        assert_eq!(
            read_framing(&empty_after).unwrap(),
            Framing::Indexed {
                claims_in_kind_order: false
            }
        );
        // A header-only file is indexed, as pbfhogg reads it.
        assert_eq!(
            read_framing(&file(&[], &[])).unwrap(),
            Framing::Indexed {
                claims_in_kind_order: true
            }
        );
        assert_eq!(
            read_framing(&file(&[vec![], vec![]], &[])).unwrap(),
            Framing::Unindexed
        );
        crate::test_support::refused(
            read_framing(&file(&[vec![index(3, 0, 1, 2, 2)], vec![]], &[])),
            "1 data blobs carry indexdata and 1 do not",
        );
        let good = index(3, 0, 1, 2, 2);
        crate::test_support::refused(
            read_framing(&file(&[vec![good.clone(), good]], &[])),
            "data blob has two indexdata fields",
        );
    }

    /// A raw `Blob` holding a `PrimitiveBlock` of one group.
    fn data_blob(group: &[u8]) -> Vec<u8> {
        let mut block = Vec::new();
        bytes_field(1, &[], &mut block);
        bytes_field(2, group, &mut block);
        let mut blob = Vec::new();
        bytes_field(1, &block, &mut blob);
        blob
    }

    fn element(id_varint: u64) -> Vec<u8> {
        let mut m = Vec::new();
        varint(1 << 3, &mut m);
        varint(id_varint, &mut m);
        m
    }

    #[test]
    fn claims_stream_per_blob_and_a_named_blob_decodes_its_ids() {
        // Dense nodes 5, 7, 9 (zigzag deltas 5, 2, 2), then ways 10 and 11.
        let mut packed = Vec::new();
        for delta in [10u64, 4, 4] {
            varint(delta, &mut packed);
        }
        let mut dense = Vec::new();
        bytes_field(1, &packed, &mut dense);
        let mut nodes = Vec::new();
        bytes_field(2, &dense, &mut nodes);
        let mut ways = Vec::new();
        bytes_field(3, &element(10), &mut ways);
        bytes_field(3, &element(11), &mut ways);
        let mut bytes = Vec::new();
        frame("OSMHeader", &[], &header_payload(&[], true), &mut bytes);
        frame(
            "OSMData",
            &[&index(3, 0, 5, 9, 3)],
            &data_blob(&nodes),
            &mut bytes,
        );
        frame("OSMData", &[], &data_blob(&ways), &mut bytes);
        let scratch_dir = pbfhogg_test_scratch::scratch();
        let dir = scratch_dir.path().join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.osm.pbf");
        std::fs::write(&path, &bytes).unwrap();

        let mut claims = super::Claims::open(&path, super::Origin::Other).unwrap();
        let first = claims.next_blob().unwrap().unwrap();
        assert_eq!(
            first.claim,
            Some(super::BlobClaim {
                kind: 0,
                min_id: 5,
                max_id: 9,
                count: 3
            })
        );
        let second = claims.next_blob().unwrap().unwrap();
        assert_eq!((second.index, second.claim), (1, None));
        assert!(claims.next_blob().unwrap().is_none());
        assert_eq!(
            super::blob_ids(
                &path,
                super::Origin::Other,
                first.payload_offset,
                first.datasize
            )
            .unwrap(),
            [(0, 5), (0, 7), (0, 9)]
        );
        assert_eq!(
            super::blob_ids(
                &path,
                super::Origin::Other,
                second.payload_offset,
                second.datasize
            )
            .unwrap(),
            [(1, 10), (1, 11)]
        );
        assert_eq!(super::zigzag(3), -2);

        // The same payloads under other headers are the same payloads.
        let mut stripped = Vec::new();
        frame("OSMHeader", &[], &header_payload(&[], false), &mut stripped);
        frame("OSMData", &[], &data_blob(&nodes), &mut stripped);
        frame("OSMData", &[], &data_blob(&ways), &mut stripped);
        let other = dir.join("g.osm.pbf");
        std::fs::write(&other, &stripped).unwrap();
        assert_eq!(
            super::same_data_payloads(
                (&path, super::Origin::Other),
                (&other, super::Origin::Other)
            )
            .unwrap(),
            Ok(())
        );
        let mut changed = Vec::new();
        frame("OSMHeader", &[], &header_payload(&[], false), &mut changed);
        frame("OSMData", &[], &data_blob(&nodes), &mut changed);
        frame("OSMData", &[], &data_blob(&nodes), &mut changed);
        std::fs::write(&other, &changed).unwrap();
        crate::test_support::refused(
            super::same_data_payloads(
                (&path, super::Origin::Other),
                (&other, super::Origin::Other),
            )
            .unwrap(),
            "data blob 1 holds 14 payload bytes in the first and 13 in the second",
        );
    }

    #[test]
    fn a_refusal_fails_only_a_pbfhogg_output() {
        let scratch_dir = pbfhogg_test_scratch::scratch();
        let path = scratch_dir.path().join("bad.osm.pbf");
        std::fs::write(&path, [0u8, 0, 0, 0]).unwrap();
        let missing = scratch_dir.path().join("missing.osm.pbf");
        for p in [&path, &missing] {
            assert!(matches!(
                super::header(p, super::Origin::Pbfhogg),
                Err(crate::error::Error::Failed(_))
            ));
            assert!(matches!(
                super::header(p, super::Origin::Other),
                Err(crate::error::Error::Tool(why)) if why.contains("cannot be made on it")
            ));
            assert!(matches!(
                super::Claims::open(p, super::Origin::Other),
                Err(crate::error::Error::Tool(_))
            ));
        }
    }

    #[test]
    fn an_appended_empty_claim_reads_back_as_one() {
        let scratch_dir = pbfhogg_test_scratch::scratch();
        let dir = scratch_dir.path().join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.osm.pbf");
        std::fs::write(&path, file(&[vec![index(3, 1, 5, 5, 1)]], &[])).unwrap();
        super::append_empty_claim(&path, 0).unwrap();
        assert_eq!(
            super::framing(&path, super::Origin::Other).unwrap(),
            Framing::Indexed {
                claims_in_kind_order: false
            }
        );
        let summary = super::walk(&path, super::Origin::Other).unwrap();
        assert_eq!(summary.count(0), 0);
        assert_eq!(summary.count(1), 1);
        crate::test_support::refused(
            super::append_empty_claim(&path, 3),
            "element kind 3 unknown",
        );
    }
}
