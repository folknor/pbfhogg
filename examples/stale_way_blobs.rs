//! Measure how much of a LocationsOnWays base a daily OSC makes stale.
//!
//! Usage:
//!     cargo run --release --example stale_way_blobs -- <base-altw.osm.pbf> <next.osc.gz>
//!         [--verify-output <apply-changes-l-output.osm.pbf>]
//!
//! Research program for the `apply-changes --locations-on-ways` defect: today
//! a base way the OSC does not touch keeps its old inline coordinates, so a
//! node the OSC moves leaves every untouched way using it stale. A fix scans
//! way blobs and rewrites the ones holding a stale way; how much of the way
//! splice fast path survives depends on the numbers this program reports.
//! Read-only: it writes nothing but its report (stdout) and progress
//! (stderr).
//!
//! What it measures, per way blob of the base, against the OSC's final state
//! (the parsed overlay: live nodes are creates and modifies with their target
//! coordinates, deletes carry no coordinate):
//!
//! - S, the inline rule: the blob holds an untouched way (neither live nor
//!   deleted in the OSC) whose inline coordinate for some ref N differs from
//!   N's OSC coordinate. Coordinates are compared decoded (block granularity
//!   and offsets applied), never as raw deltas, and in decimicrodegrees (e7):
//!   the exact nanodegree value truncated toward zero, the contract of the
//!   reader's `decimicro_lat` and of `check --locations`. A block framed
//!   finer than e7 therefore compares equal where it agrees to the e7; how
//!   many inline coordinates carry sub-e7 precision is reported.
//! - M, ID-only membership: the blob holds an untouched way with some ref
//!   that is a live OSC node at all, whether or not its coordinate differs.
//!   The gap between S and M is what tag-only node edits cost an ID rule.
//! - T, what today's merge already rewrites (an emulation of the indexed
//!   worker path): some way in the blob is live or deleted in the OSC, or a
//!   live OSC way ID lies inside the blob's way-ID range. Ways the OSC
//!   itself replaces or deletes are rewritten anyway, so they never count
//!   toward S or M; their stale refs are reported separately. Production
//!   routes on the blob's claimed indexdata, so the program reads each
//!   BlobHeader's indexdata itself and marks a way blob unsupported (U)
//!   when the claim is missing, unreadable, or disagrees with the decoded
//!   kind, way-ID range or way count, as well as for mixed-kind blocks,
//!   negative way IDs and out-of-order way IDs.
//!
//! With `--verify-output`, pass the output of `apply-changes
//! --locations-on-ways` run on the same pair: the program hashes every
//! output blob body and counts an input blob as actually rewritten when its
//! body does not appear there (a splice or an owned passthrough keeps the
//! compressed body byte for byte, whatever its header). It then reports
//! predicted T against actual rewrites for way blobs, and actual rewrites
//! for the other kinds.
//!
//! The report gives these sets and their combinations (S minus T is the
//! extra work a fix adds, S intersect T the stale survivors inside blobs
//! today's merge already rewrites) in blobs, framed compressed bytes (the
//! bytes a splice copies) and decompressed bytes; stale ways and refs by
//! way class; probe nodes that differ from or agree with base inline
//! coordinates; refs to OSC-deleted nodes; ways without inline geometry;
//! per-blob staleness histograms; clustering of the sets along the way-blob
//! sequence; and summed decode and probe times.
//!
//! Memory: the OSC overlay's live and deleted node IDs and coordinates plus
//! its way IDs, a compact record (about 24 bytes) per base way blob, and a
//! bounded batch of blobs in flight; under `--verify-output`, also one hash
//! per output blob. Verification reads every base blob body a second time.

#![allow(clippy::too_many_lines)]

use std::error::Error;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

use pbfhogg::osc::{CompactDiffOverlay, parse_osc_file};
use pbfhogg::{Blob, BlobReader, BlobType, BlockType, Element, ElementKind, PrimitiveBlock, Way};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use xxhash_rust::xxh3::xxh3_64;

/// Blobs decoded per parallel batch. Bounds the frames held in memory.
const BATCH_BLOBS: usize = 512;

/// Number of log2 histogram buckets; larger values land in the last one.
const HIST_BUCKETS: usize = 32;

// Per live OSC node, set while scanning base ways.
/// Referenced by some base way, any class.
const NODE_REF_ANY: u8 = 1;
/// Referenced by some untouched base way.
const NODE_REF_UNTOUCHED: u8 = 2;
/// Some base way's inline coordinate for it differs from the OSC's.
const NODE_DIFF_ANY: u8 = 4;
/// Differs on an untouched base way: the defect's case.
const NODE_DIFF_UNTOUCHED: u8 = 8;
/// Some base way's inline coordinate for it equals the OSC's.
const NODE_EQ_ANY: u8 = 16;
/// Referenced by a base way that carries no inline geometry.
const NODE_REF_NOGEOM: u8 = 32;
/// Referenced by a live OSC way (set from the overlay before the scan).
const NODE_OSC_WAY_REF: u8 = 64;

// Per OSC-deleted node.
/// Referenced by an untouched base way: base and OSC disagree.
const DEL_REF_UNTOUCHED: u8 = 1;
/// Referenced by a base way the OSC replaces (its old version).
const DEL_REF_LIVE: u8 = 2;
/// Referenced by a base way the OSC deletes.
const DEL_REF_DELETED: u8 = 4;
/// Referenced by a live OSC way (the OSC contradicts itself).
const DEL_OSC_WAY_REF: u8 = 8;

// Per way blob, on the compact record.
/// T: today's merge rewrites the blob.
const REC_T: u16 = 1;
/// S: an untouched way holds a coordinate that differs from the OSC's.
const REC_S: u16 = 2;
/// M: an untouched way references a live OSC node.
const REC_M: u16 = 4;
/// An untouched way references an OSC-deleted node.
const REC_D: u16 = 8;
/// The T emulation does not hold for this blob (see `Unsupported`).
const REC_UNSUPPORTED: u16 = 16;
/// Any way, of any class, holds a differing coordinate (raw staleness).
const REC_RAW_STALE: u16 = 32;
/// An untouched way without inline geometry references a live OSC node.
const REC_NOGEOM_HIT: u16 = 64;
/// At least one non-way frame sits between this and the previous way blob.
const REC_PHYS_GAP: u16 = 128;
/// `--verify-output`: the blob's body is absent from the output, so the
/// real run rewrote it.
const REC_ACTUAL: u16 = 256;

const LAT_LIMIT_NANO: i64 = 90_000_000_000;
const LON_LIMIT_NANO: i64 = 180_000_000_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum WayClass {
    /// Neither live nor deleted in the OSC: keeps its inline coordinates.
    Untouched,
    /// Live in the OSC (created or modified): replaced by the OSC version.
    Live,
    /// Deleted by the OSC.
    Deleted,
}

impl WayClass {
    const ALL: [WayClass; 3] = [WayClass::Untouched, WayClass::Live, WayClass::Deleted];

    fn idx(self) -> usize {
        match self {
            WayClass::Untouched => 0,
            WayClass::Live => 1,
            WayClass::Deleted => 2,
        }
    }

    fn label(self) -> &'static str {
        match self {
            WayClass::Untouched => "untouched",
            WayClass::Live => "live OSC way",
            WayClass::Deleted => "OSC-deleted way",
        }
    }

    fn deleted_bit(self) -> u8 {
        match self {
            WayClass::Untouched => DEL_REF_UNTOUCHED,
            WayClass::Live => DEL_REF_LIVE,
            WayClass::Deleted => DEL_REF_DELETED,
        }
    }
}

/// The OSC, reduced to what the scan probes.
struct Probe {
    /// Node ID to slot. Slots below `live_len` are live nodes (index into
    /// `e7` and `live_flags`); the rest are deleted nodes (minus
    /// `live_len`, index into `deleted_flags`). The overlay keeps the two
    /// sets disjoint.
    index: FxHashMap<i64, u32>,
    live_len: usize,
    /// OSC coordinate per live node, `(lat, lon)` in decimicrodegrees.
    e7: Vec<(i64, i64)>,
    live_flags: Vec<AtomicU8>,
    deleted_flags: Vec<AtomicU8>,
    live_ways: FxHashSet<i64>,
    deleted_ways: FxHashSet<i64>,
    /// Live OSC way IDs, ascending.
    live_ways_sorted: Vec<i64>,
    osc_negative_way_ids: u64,
}

impl Probe {
    fn build(overlay: &CompactDiffOverlay) -> Result<Probe, Box<dyn Error>> {
        let live_len = overlay.node_count();
        let deleted_nodes = overlay.deleted_ids(ElementKind::Node);
        let mut index = FxHashMap::default();
        index.reserve(live_len + deleted_nodes.len());
        let mut e7 = Vec::with_capacity(live_len);
        for &id in overlay.node_ids() {
            let Some(node) = overlay.get_node(id) else {
                continue;
            };
            index.insert(id, u32::try_from(e7.len())?);
            e7.push((
                i64::from(node.decimicro_lat()),
                i64::from(node.decimicro_lon()),
            ));
        }
        let live_len = e7.len();
        for (k, &id) in deleted_nodes.iter().enumerate() {
            index.insert(id, u32::try_from(live_len + k)?);
        }
        let live_flags = (0..live_len).map(|_| AtomicU8::new(0)).collect();
        let deleted_flags = (0..deleted_nodes.len()).map(|_| AtomicU8::new(0)).collect();
        let mut live_ways_sorted: Vec<i64> = overlay.way_ids().copied().collect();
        live_ways_sorted.sort_unstable();
        let deleted_ways: FxHashSet<i64> = overlay
            .deleted_ids(ElementKind::Way)
            .iter()
            .copied()
            .collect();
        let osc_negative_way_ids = live_ways_sorted
            .iter()
            .chain(deleted_ways.iter())
            .filter(|&&id| id < 0)
            .count() as u64;
        let probe = Probe {
            index,
            live_len,
            e7,
            live_flags,
            deleted_flags,
            live_ways: live_ways_sorted.iter().copied().collect(),
            deleted_ways,
            live_ways_sorted,
            osc_negative_way_ids,
        };
        for &way_id in overlay.way_ids() {
            if let Some(way) = overlay.get_way(way_id) {
                for node_id in way.refs() {
                    probe.mark_osc_way_ref(node_id);
                }
            }
        }
        Ok(probe)
    }

    fn mark_osc_way_ref(&self, node_id: i64) {
        let Some(&slot) = self.index.get(&node_id) else {
            return;
        };
        let slot = slot as usize;
        if slot < self.live_len {
            mark(&self.live_flags[slot], NODE_OSC_WAY_REF);
        } else {
            mark(&self.deleted_flags[slot - self.live_len], DEL_OSC_WAY_REF);
        }
    }

    fn way_class(&self, id: i64) -> WayClass {
        if self.live_ways.contains(&id) {
            WayClass::Live
        } else if self.deleted_ways.contains(&id) {
            WayClass::Deleted
        } else {
            WayClass::Untouched
        }
    }

    /// Whether a live OSC way ID lies in `[min, max]`.
    fn live_way_in_range(&self, min: i64, max: i64) -> bool {
        let i = self.live_ways_sorted.partition_point(|&x| x < min);
        self.live_ways_sorted.get(i).is_some_and(|&x| x <= max)
    }
}

/// Set `bit` once; the load keeps repeat hits off the cache line's write path.
fn mark(flags: &AtomicU8, bit: u8) {
    if flags.load(Ordering::Relaxed) & bit == 0 {
        flags.fetch_or(bit, Ordering::Relaxed);
    }
}

/// What one blob looked like.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum BlobKind {
    #[default]
    NonWay,
    Header,
    Unknown,
    Way,
}

/// Per-blob scan result. Counters indexed by `WayClass::idx`.
#[derive(Default)]
struct BlobScan {
    kind: BlobKind,
    /// Framed bytes: length prefix, BlobHeader and body.
    frame: u64,
    /// The BlobHeader's indexdata claim against the decoded contents (way
    /// blobs only; `Agrees` elsewhere).
    index: IndexVerdict,
    /// Hash of the compressed body, under `--verify-output` only.
    body_hash: Option<u64>,
    mixed: bool,
    negative_ids: bool,
    unordered_within: bool,
    min_id: i64,
    max_id: i64,
    last_id: Option<i64>,
    decompressed: u64,
    ways: [u64; 3],
    refs: [u64; 3],
    zero_ref_ways: u64,
    nogeom_ways: u64,
    geom_len_mismatch: u64,
    nogeom_hit_ways: [u64; 3],
    coords: u64,
    sub_decimicro: u64,
    invalid_coords: u64,
    member_ways: [u64; 3],
    member_refs: [u64; 3],
    stale_ways: [u64; 3],
    stale_refs: [u64; 3],
    deleted_ref_ways: [u64; 3],
    deleted_refs: [u64; 3],
    decode_ns: u64,
    probe_ns: u64,
}

impl BlobScan {
    fn note_way_id(&mut self, id: i64) {
        if id < 0 {
            self.negative_ids = true;
        }
        match self.last_id {
            None => {
                self.min_id = id;
                self.max_id = id;
            }
            Some(last) => {
                if id <= last {
                    self.unordered_within = true;
                }
                self.min_id = self.min_id.min(id);
                self.max_id = self.max_id.max(id);
            }
        }
        self.last_id = Some(id);
    }

    fn has_ways(&self) -> bool {
        self.ways.iter().sum::<u64>() > 0
    }
}

/// Per-way probe hits, folded into the blob once the way is done.
#[derive(Default)]
struct WayHits {
    member_refs: u64,
    stale_refs: u64,
    deleted_refs: u64,
    nogeom_hits: u64,
}

impl WayHits {
    fn fold_into(&self, scan: &mut BlobScan, c: usize) {
        scan.member_refs[c] += self.member_refs;
        scan.stale_refs[c] += self.stale_refs;
        scan.deleted_refs[c] += self.deleted_refs;
        scan.member_ways[c] += u64::from(self.member_refs > 0);
        scan.stale_ways[c] += u64::from(self.stale_refs > 0);
        scan.deleted_ref_ways[c] += u64::from(self.deleted_refs > 0);
        scan.nogeom_hit_ways[c] += u64::from(self.nogeom_hits > 0);
    }
}

fn elapsed_ns(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// Read-only state the parallel workers share.
struct Ctx<'a> {
    probe: &'a Probe,
    /// The base, opened a second time for positioned reads of BlobHeaders
    /// (and bodies under `--verify-output`), which the public reader does
    /// not expose.
    file: File,
    verify: bool,
}

/// The fields of one frame's BlobHeader this program needs.
struct FrameHeader {
    indexdata: Option<Vec<u8>>,
    /// Offset of the blob body, just past the BlobHeader.
    body_offset: u64,
    datasize: Option<u64>,
}

fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// Walk a BlobHeader message for field 2 (indexdata) and field 3
/// (datasize). `None` when the message does not parse.
fn parse_blob_header(buf: &[u8]) -> Option<(Option<&[u8]>, Option<u64>)> {
    let mut pos = 0usize;
    let mut indexdata = None;
    let mut datasize = None;
    while pos < buf.len() {
        let key = read_varint(buf, &mut pos)?;
        match (key >> 3, key & 7) {
            (3, 0) => datasize = Some(read_varint(buf, &mut pos)?),
            (_, 0) => {
                read_varint(buf, &mut pos)?;
            }
            (field, 2) => {
                let len = usize::try_from(read_varint(buf, &mut pos)?).ok()?;
                let bytes = buf.get(pos..pos.checked_add(len)?)?;
                pos += len;
                if field == 2 {
                    indexdata = Some(bytes);
                }
            }
            (_, 1) => pos += 8,
            (_, 5) => pos += 4,
            _ => return None,
        }
    }
    (pos == buf.len()).then_some((indexdata, datasize))
}

fn read_frame_header(
    file: &File,
    offset: u64,
) -> Result<FrameHeader, Box<dyn Error + Send + Sync>> {
    let mut len = [0u8; 4];
    file.read_exact_at(&mut len, offset)?;
    let header_len = u32::from_be_bytes(len);
    let mut buf = vec![0u8; usize::try_from(header_len)?];
    file.read_exact_at(&mut buf, offset + 4)?;
    let (indexdata, datasize) =
        parse_blob_header(&buf).ok_or_else(|| format!("unparsable BlobHeader at byte {offset}"))?;
    Ok(FrameHeader {
        indexdata: indexdata.filter(|d| !d.is_empty()).map(<[u8]>::to_vec),
        body_offset: offset + 4 + u64::from(header_len),
        datasize,
    })
}

/// What a way blob's indexdata says about it, against its decoded contents.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum IndexVerdict {
    #[default]
    Agrees,
    Missing,
    /// Unknown version, wrong length, unknown kind or inverted range.
    Unreadable,
    /// Claims a kind other than way.
    WrongKind,
    /// Claims a way-ID range other than the decoded one.
    WrongRange,
    /// Claims a way count other than the decoded one.
    WrongCount,
}

/// The indexdata layout (`src/blob_meta`): version, kind ordinal, min ID,
/// max ID and count, little-endian, then a bbox in the 42-byte versions.
/// Version 1 is 26 bytes, versions 2 and 3 are 42.
fn decode_indexdata(bytes: &[u8]) -> Option<(u8, i64, i64, u64)> {
    let declared = match *bytes.first()? {
        1 => 26,
        2 | 3 => 42,
        _ => return None,
    };
    if bytes.len() != declared {
        return None;
    }
    let word = |at: usize| -> Option<[u8; 8]> { bytes.get(at..at + 8)?.try_into().ok() };
    let kind = *bytes.get(1)?;
    ElementKind::from_ordinal(kind)?;
    let min = i64::from_le_bytes(word(2)?);
    let max = i64::from_le_bytes(word(10)?);
    let count = u64::from_le_bytes(word(18)?);
    (min <= max).then_some((kind, min, max, count))
}

fn judge_index(indexdata: Option<&[u8]>, scan: &BlobScan) -> IndexVerdict {
    let Some(bytes) = indexdata else {
        return IndexVerdict::Missing;
    };
    let Some((kind, min, max, count)) = decode_indexdata(bytes) else {
        return IndexVerdict::Unreadable;
    };
    if kind != ElementKind::Way.ordinal() {
        IndexVerdict::WrongKind
    } else if (min, max) != (scan.min_id, scan.max_id) {
        IndexVerdict::WrongRange
    } else if count != scan.ways.iter().sum::<u64>() {
        IndexVerdict::WrongCount
    } else {
        IndexVerdict::Agrees
    }
}

type WorkerResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

fn scan_blob(blob: &Blob, frame: u64, ctx: &Ctx<'_>) -> WorkerResult<BlobScan> {
    let mut scan = scan_contents(blob, ctx.probe)?;
    scan.frame = frame;
    if scan.kind == BlobKind::Header {
        return Ok(scan);
    }
    let offset = blob.offset().ok_or("blob without a byte offset")?.0;
    let header = read_frame_header(&ctx.file, offset)?;
    if scan.kind == BlobKind::Way {
        scan.index = judge_index(header.indexdata.as_deref(), &scan);
    }
    if ctx.verify {
        let mut body = vec![0u8; usize::try_from(offset + frame - header.body_offset)?];
        ctx.file.read_exact_at(&mut body, header.body_offset)?;
        scan.body_hash = Some(xxh3_64(&body));
    }
    Ok(scan)
}

fn scan_contents(blob: &Blob, probe: &Probe) -> pbfhogg::Result<BlobScan> {
    let mut scan = BlobScan::default();
    match blob.get_type() {
        BlobType::OsmHeader => {
            scan.kind = BlobKind::Header;
            return Ok(scan);
        }
        BlobType::OsmData => {}
        _ => {
            scan.kind = BlobKind::Unknown;
            return Ok(scan);
        }
    }
    let start = Instant::now();
    let block = blob.to_primitiveblock()?;
    scan.decode_ns = elapsed_ns(start);
    scan.decompressed = block.decompressed_size() as u64;
    let block_type = block.block_type();
    if !matches!(block_type, BlockType::Ways | BlockType::Mixed) {
        return Ok(scan);
    }
    let start = Instant::now();
    scan_block(&block, probe, &mut scan);
    scan.probe_ns = elapsed_ns(start);
    if scan.has_ways() {
        scan.kind = BlobKind::Way;
        scan.mixed = block_type == BlockType::Mixed;
    }
    Ok(scan)
}

fn scan_block(block: &PrimitiveBlock, probe: &Probe, scan: &mut BlobScan) {
    let mut refs: Vec<i64> = Vec::new();
    let mut locs: Vec<(i64, i64)> = Vec::new();
    for element in block.elements_skip_metadata() {
        if let Element::Way(way) = element {
            scan_way(&way, probe, scan, &mut refs, &mut locs);
        }
    }
}

fn scan_way(
    way: &Way<'_>,
    probe: &Probe,
    scan: &mut BlobScan,
    refs: &mut Vec<i64>,
    locs: &mut Vec<(i64, i64)>,
) {
    let id = way.id();
    scan.note_way_id(id);
    let class = probe.way_class(id);
    let c = class.idx();
    refs.clear();
    refs.extend(way.refs());
    locs.clear();
    locs.extend(way.node_locations().map(|l| (l.nano_lat(), l.nano_lon())));
    scan.ways[c] += 1;
    scan.refs[c] += refs.len() as u64;
    if refs.is_empty() {
        scan.zero_ref_ways += 1;
        return;
    }
    let has_geom = locs.len() == refs.len();
    if has_geom {
        for (lat, lon) in locs.iter().copied() {
            check_coord(lat, lon, scan);
        }
    } else if locs.is_empty() {
        scan.nogeom_ways += 1;
    } else {
        // Block parsing refuses present arrays of unequal length, so this
        // counts a broken invariant, not an input property.
        scan.geom_len_mismatch += 1;
    }
    let mut hits = WayHits::default();
    for (i, &node_id) in refs.iter().enumerate() {
        let loc = if has_geom { locs.get(i).copied() } else { None };
        probe_ref(node_id, loc, class, probe, &mut hits);
    }
    hits.fold_into(scan, c);
}

fn check_coord(lat: i64, lon: i64, scan: &mut BlobScan) {
    scan.coords += 1;
    if lat % 100 != 0 || lon % 100 != 0 {
        scan.sub_decimicro += 1;
    }
    if !(-LAT_LIMIT_NANO..=LAT_LIMIT_NANO).contains(&lat)
        || !(-LON_LIMIT_NANO..=LON_LIMIT_NANO).contains(&lon)
    {
        scan.invalid_coords += 1;
    }
}

fn probe_ref(
    node_id: i64,
    loc: Option<(i64, i64)>,
    class: WayClass,
    probe: &Probe,
    hits: &mut WayHits,
) {
    let Some(&slot) = probe.index.get(&node_id) else {
        return;
    };
    let slot = slot as usize;
    if slot >= probe.live_len {
        hits.deleted_refs += 1;
        mark(
            &probe.deleted_flags[slot - probe.live_len],
            class.deleted_bit(),
        );
        return;
    }
    hits.member_refs += 1;
    let flags = &probe.live_flags[slot];
    mark(flags, NODE_REF_ANY);
    let untouched = class == WayClass::Untouched;
    if untouched {
        mark(flags, NODE_REF_UNTOUCHED);
    }
    match loc {
        None => {
            hits.nogeom_hits += 1;
            mark(flags, NODE_REF_NOGEOM);
        }
        // e7 equality: the exact nanodegree value truncated toward zero, as
        // `decimicro_lat` and `check --locations` do.
        Some((lat, lon)) if (lat / 100, lon / 100) == probe.e7[slot] => mark(flags, NODE_EQ_ANY),
        Some(_) => {
            hits.stale_refs += 1;
            mark(flags, NODE_DIFF_ANY);
            if untouched {
                mark(flags, NODE_DIFF_UNTOUCHED);
            }
        }
    }
}

/// Compact per-way-blob record, kept in file order for the set and
/// clustering analysis.
struct WayBlobRec {
    frame: u64,
    decompressed: u64,
    bits: u16,
}

/// Why the T emulation does not hold for a blob. Counted, not refused:
/// these are discovered mid-pass, and the report bounds what they affect.
#[derive(Default)]
struct Unsupported {
    mixed: u64,
    negative_ids: u64,
    unordered_within: u64,
    unordered_across: u64,
    index_missing: u64,
    index_unreadable: u64,
    index_wrong_kind: u64,
    index_wrong_range: u64,
    index_wrong_count: u64,
}

/// `--verify-output` tallies for blobs that are not way blobs.
#[derive(Default)]
struct NonWayActual {
    rewritten: u64,
    rewritten_bytes: u64,
}

#[derive(Default)]
struct Totals {
    frames: u64,
    frame_bytes: u64,
    header_frames: u64,
    unknown_frames: u64,
    nonway_frames: u64,
    nonway_bytes: u64,
    nonway_between_way_blobs: u64,
    ways: [u64; 3],
    refs: [u64; 3],
    zero_ref_ways: u64,
    nogeom_ways: u64,
    geom_len_mismatch: u64,
    nogeom_hit_ways: [u64; 3],
    coords: u64,
    sub_decimicro: u64,
    invalid_coords: u64,
    member_ways: [u64; 3],
    member_refs: [u64; 3],
    stale_ways: [u64; 3],
    stale_refs: [u64; 3],
    deleted_ref_ways: [u64; 3],
    deleted_refs: [u64; 3],
    decode_ns: u64,
    probe_ns: u64,
    unsupported: Unsupported,
    nonway_actual: NonWayActual,
    /// Per S blob: untouched stale ways, untouched stale refs (log2 buckets).
    stale_ways_hist: [u64; HIST_BUCKETS],
    stale_refs_hist: [u64; HIST_BUCKETS],
    /// Per S blob: untouched stale ways as a share of the blob's ways, in
    /// 10 % buckets (index 10 is exactly 100 %).
    stale_share_hist: [u64; 11],
}

fn add3(into: &mut [u64; 3], from: &[u64; 3]) {
    for (a, b) in into.iter_mut().zip(from) {
        *a += b;
    }
}

fn log2_bucket(v: u64) -> usize {
    ((63 - v.max(1).leading_zeros()) as usize).min(HIST_BUCKETS - 1)
}

/// Folds blob scans, in file order, into totals and way-blob records.
struct Aggregator<'p> {
    probe: &'p Probe,
    /// `--verify-output`: hashes of every output blob body.
    output_bodies: Option<&'p FxHashSet<u64>>,
    totals: Totals,
    recs: Vec<WayBlobRec>,
    prev_way_max: Option<i64>,
    nonway_since_way: u64,
}

impl<'p> Aggregator<'p> {
    fn new(probe: &'p Probe, output_bodies: Option<&'p FxHashSet<u64>>) -> Self {
        Aggregator {
            probe,
            output_bodies,
            totals: Totals::default(),
            recs: Vec::new(),
            prev_way_max: None,
            nonway_since_way: 0,
        }
    }

    /// Whether the real run rewrote this blob; `false` without
    /// `--verify-output`.
    fn actually_rewritten(&self, scan: &BlobScan) -> bool {
        match (self.output_bodies, scan.body_hash) {
            (Some(bodies), Some(hash)) => !bodies.contains(&hash),
            _ => false,
        }
    }

    fn absorb(&mut self, scan: &BlobScan) {
        let frame = scan.frame;
        let actual = self.actually_rewritten(scan);
        let t = &mut self.totals;
        t.frames += 1;
        t.frame_bytes += frame;
        t.decode_ns += scan.decode_ns;
        t.probe_ns += scan.probe_ns;
        match scan.kind {
            BlobKind::Header => t.header_frames += 1,
            BlobKind::Unknown => t.unknown_frames += 1,
            BlobKind::NonWay => {
                t.nonway_frames += 1;
                t.nonway_bytes += frame;
                if actual {
                    t.nonway_actual.rewritten += 1;
                    t.nonway_actual.rewritten_bytes += frame;
                }
                self.nonway_since_way += 1;
            }
            BlobKind::Way => self.absorb_way(scan, actual),
        }
    }

    fn absorb_way(&mut self, scan: &BlobScan, actual: bool) {
        let frame = scan.frame;
        let mut bits = 0u16;
        if actual {
            bits |= REC_ACTUAL;
        }
        if self.prev_way_max.is_some() && self.nonway_since_way > 0 {
            bits |= REC_PHYS_GAP;
            self.totals.nonway_between_way_blobs += self.nonway_since_way;
        }
        self.nonway_since_way = 0;
        bits |= self.unsupported_bits(scan);
        self.prev_way_max = Some(
            self.prev_way_max
                .map_or(scan.max_id, |m| m.max(scan.max_id)),
        );
        let overlap = scan.ways[WayClass::Live.idx()] + scan.ways[WayClass::Deleted.idx()] > 0;
        if overlap || self.probe.live_way_in_range(scan.min_id, scan.max_id) {
            bits |= REC_T;
        }
        let u = WayClass::Untouched.idx();
        if scan.stale_ways[u] > 0 {
            bits |= REC_S;
            self.record_s_histograms(scan);
        }
        if scan.member_ways[u] > 0 {
            bits |= REC_M;
        }
        if scan.deleted_ref_ways[u] > 0 {
            bits |= REC_D;
        }
        if scan.nogeom_hit_ways[u] > 0 {
            bits |= REC_NOGEOM_HIT;
        }
        if scan.stale_ways.iter().sum::<u64>() > 0 {
            bits |= REC_RAW_STALE;
        }
        self.recs.push(WayBlobRec {
            frame,
            decompressed: scan.decompressed,
            bits,
        });
        self.add_counts(scan);
    }

    fn unsupported_bits(&mut self, scan: &BlobScan) -> u16 {
        let un = &mut self.totals.unsupported;
        let across = self.prev_way_max.is_some_and(|m| scan.min_id <= m);
        un.mixed += u64::from(scan.mixed);
        un.negative_ids += u64::from(scan.negative_ids);
        un.unordered_within += u64::from(scan.unordered_within);
        un.unordered_across += u64::from(across);
        let counter = match scan.index {
            IndexVerdict::Agrees => None,
            IndexVerdict::Missing => Some(&mut un.index_missing),
            IndexVerdict::Unreadable => Some(&mut un.index_unreadable),
            IndexVerdict::WrongKind => Some(&mut un.index_wrong_kind),
            IndexVerdict::WrongRange => Some(&mut un.index_wrong_range),
            IndexVerdict::WrongCount => Some(&mut un.index_wrong_count),
        };
        if let Some(counter) = counter {
            *counter += 1;
        }
        if scan.mixed
            || scan.negative_ids
            || scan.unordered_within
            || across
            || scan.index != IndexVerdict::Agrees
        {
            REC_UNSUPPORTED
        } else {
            0
        }
    }

    fn record_s_histograms(&mut self, scan: &BlobScan) {
        let u = WayClass::Untouched.idx();
        let t = &mut self.totals;
        t.stale_ways_hist[log2_bucket(scan.stale_ways[u])] += 1;
        t.stale_refs_hist[log2_bucket(scan.stale_refs[u])] += 1;
        let all_ways: u64 = scan.ways.iter().sum();
        let share = (scan.stale_ways[u] * 10 / all_ways.max(1)).min(10);
        t.stale_share_hist[usize::try_from(share).unwrap_or(10)] += 1;
    }

    fn add_counts(&mut self, scan: &BlobScan) {
        let t = &mut self.totals;
        add3(&mut t.ways, &scan.ways);
        add3(&mut t.refs, &scan.refs);
        add3(&mut t.nogeom_hit_ways, &scan.nogeom_hit_ways);
        add3(&mut t.member_ways, &scan.member_ways);
        add3(&mut t.member_refs, &scan.member_refs);
        add3(&mut t.stale_ways, &scan.stale_ways);
        add3(&mut t.stale_refs, &scan.stale_refs);
        add3(&mut t.deleted_ref_ways, &scan.deleted_ref_ways);
        add3(&mut t.deleted_refs, &scan.deleted_refs);
        t.zero_ref_ways += scan.zero_ref_ways;
        t.nogeom_ways += scan.nogeom_ways;
        t.geom_len_mismatch += scan.geom_len_mismatch;
        t.coords += scan.coords;
        t.sub_decimicro += scan.sub_decimicro;
        t.invalid_coords += scan.invalid_coords;
    }
}

fn process_batch(
    batch: &mut Vec<(Blob, u64)>,
    ctx: &Ctx<'_>,
    agg: &mut Aggregator<'_>,
) -> Result<(), Box<dyn Error>> {
    let scans = std::mem::take(batch)
        .into_par_iter()
        .map(|(blob, frame)| scan_blob(&blob, frame, ctx))
        .collect::<WorkerResult<Vec<BlobScan>>>()
        .map_err(|e| -> Box<dyn Error> { e })?;
    for scan in &scans {
        agg.absorb(scan);
    }
    Ok(())
}

/// Hash the body of every blob in `path`, walking the frames directly.
fn hash_output_bodies(path: &Path) -> Result<FxHashSet<u64>, Box<dyn Error>> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let len = file.metadata()?.len();
    let mut bodies = FxHashSet::default();
    let mut offset = 0u64;
    let mut body = Vec::new();
    while offset < len {
        let header = read_frame_header(&file, offset).map_err(|e| -> Box<dyn Error> { e })?;
        let size = header
            .datasize
            .ok_or_else(|| format!("BlobHeader without datasize at byte {offset}"))?;
        body.resize(usize::try_from(size)?, 0);
        file.read_exact_at(&mut body, header.body_offset)?;
        bodies.insert(xxh3_64(&body));
        offset = header.body_offset + size;
    }
    Ok(bodies)
}

fn offset_of(blob: &Blob) -> Result<u64, Box<dyn Error>> {
    blob.offset()
        .map(|o| o.0)
        .ok_or_else(|| "blob without a byte offset".into())
}

/// One sequential read; frame lengths come from consecutive offsets (the
/// last from the file length), decoding runs in parallel batches, and
/// results are folded in file order.
fn scan_file(
    input: &Path,
    probe: &Probe,
    output_bodies: Option<&FxHashSet<u64>>,
) -> Result<ScanResult, Box<dyn Error>> {
    let file = File::open(input).map_err(|e| format!("{}: {e}", input.display()))?;
    let file_len = file.metadata()?.len();
    let ctx = Ctx {
        probe,
        file,
        verify: output_bodies.is_some(),
    };
    let mut reader = BlobReader::from_path(input)?;
    let header_blob = reader.leading_header()?;
    let header = header_blob.to_headerblock()?;
    if !header.has_locations_on_ways() {
        return Err(format!(
            "{} does not declare LocationsOnWays: it has no inline coordinates to measure",
            input.display()
        )
        .into());
    }
    if !header.is_sorted() {
        return Err(format!(
            "{} does not declare Sort.Type_then_ID: the rewrite emulation assumes sorted input",
            input.display()
        )
        .into());
    }
    let mut agg = Aggregator::new(probe, output_bodies);
    let mut batch: Vec<(Blob, u64)> = Vec::with_capacity(BATCH_BLOBS);
    let mut pending = header_blob;
    let mut seen: u64 = 0;
    for next in reader {
        let next = next?;
        let len = offset_of(&next)? - offset_of(&pending)?;
        batch.push((std::mem::replace(&mut pending, next), len));
        seen += 1;
        if batch.len() == BATCH_BLOBS {
            process_batch(&mut batch, &ctx, &mut agg)?;
        }
        if seen.is_multiple_of(10_000) {
            eprint!("\r  {seen} blobs scanned...");
        }
    }
    let len = file_len - offset_of(&pending)?;
    batch.push((pending, len));
    process_batch(&mut batch, &ctx, &mut agg)?;
    if seen >= 10_000 {
        eprintln!();
    }
    Ok(ScanResult {
        totals: agg.totals,
        recs: agg.recs,
    })
}

struct ScanResult {
    totals: Totals,
    recs: Vec<WayBlobRec>,
}

fn pct(a: u64, b: u64) -> f64 {
    if b == 0 {
        0.0
    } else {
        100.0 * a as f64 / b as f64
    }
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1u64 << 30) as f64
}

/// A named predicate over way-blob record bits.
struct BlobSet {
    name: &'static str,
    test: fn(u16) -> bool,
}

fn has(bits: u16, flag: u16) -> bool {
    bits & flag != 0
}

const SETS: &[BlobSet] = &[
    BlobSet {
        name: "T   rewritten by today's merge (predicted)",
        test: |b| has(b, REC_T),
    },
    BlobSet {
        name: "S   untouched way, coordinate differs",
        test: |b| has(b, REC_S),
    },
    BlobSet {
        name: "M   untouched way, ref is a live OSC node",
        test: |b| has(b, REC_M),
    },
    BlobSet {
        name: "S or T   rewritten after an inline-rule fix",
        test: |b| has(b, REC_S | REC_T),
    },
    BlobSet {
        name: "M or T   rewritten after an ID-only fix",
        test: |b| has(b, REC_M | REC_T),
    },
    BlobSet {
        name: "S minus T   extra rewrites, inline rule",
        test: |b| has(b, REC_S) && !has(b, REC_T),
    },
    BlobSet {
        name: "M minus T   extra rewrites, ID-only rule",
        test: |b| has(b, REC_M) && !has(b, REC_T),
    },
    BlobSet {
        name: "S and T   stale survivors in rewritten blobs",
        test: |b| has(b, REC_S) && has(b, REC_T),
    },
    BlobSet {
        name: "raw: any way class holds a differing coord",
        test: |b| has(b, REC_RAW_STALE),
    },
    BlobSet {
        name: "D   untouched way refs an OSC-deleted node",
        test: |b| has(b, REC_D),
    },
    BlobSet {
        name: "G   untouched way w/o geometry hits probe",
        test: |b| has(b, REC_NOGEOM_HIT),
    },
    BlobSet {
        name: "U   unsupported for the T emulation",
        test: |b| has(b, REC_UNSUPPORTED),
    },
];

struct SetSums {
    blobs: u64,
    frame: u64,
    decompressed: u64,
}

fn sum_set(recs: &[WayBlobRec], test: impl Fn(u16) -> bool) -> SetSums {
    let mut s = SetSums {
        blobs: 0,
        frame: 0,
        decompressed: 0,
    };
    for r in recs.iter().filter(|r| test(r.bits)) {
        s.blobs += 1;
        s.frame += r.frame;
        s.decompressed += r.decompressed;
    }
    s
}

fn print_inputs(base: &Path, osc: &Path, overlay_bytes: usize, probe: &Probe, t: &Totals) {
    println!("# Stale inline coordinates per way blob");
    println!();
    println!("Base: {}", base.display());
    println!("OSC:  {}", osc.display());
    println!();
    println!("## OSC (final state after parsing)");
    println!(
        "  live nodes (create/modify, the probe set): {}",
        probe.live_len
    );
    println!(
        "  deleted nodes:                             {}",
        probe.deleted_flags.len()
    );
    println!(
        "  live ways:                                 {}",
        probe.live_ways.len()
    );
    println!(
        "  deleted ways:                              {}",
        probe.deleted_ways.len()
    );
    println!(
        "  overlay heap estimate:                     {:.1} MiB",
        overlay_bytes as f64 / f64::from(1u32 << 20)
    );
    if probe.osc_negative_way_ids > 0 {
        println!(
            "  CAVEAT: {} negative OSC way IDs; the T range test sorts numerically, \
             production in OSM order, so T is unsupported for them",
            probe.osc_negative_way_ids
        );
    }
    println!();
    println!("## Base frames");
    println!(
        "  frames: {} ({:.2} GiB); header {}, unknown {}, node/relation {} ({:.2} GiB)",
        t.frames,
        gib(t.frame_bytes),
        t.header_frames,
        t.unknown_frames,
        t.nonway_frames,
        gib(t.nonway_bytes)
    );
    println!(
        "  non-way frames between way blobs: {} (0 means the way-blob sequence is \
         physically contiguous, so sequence runs are file runs)",
        t.nonway_between_way_blobs
    );
    let u = &t.unsupported;
    println!(
        "  unsupported way blobs: mixed-kind {}, negative way IDs {}, unordered within {}, \
         overlapping or unordered across blobs {}",
        u.mixed, u.negative_ids, u.unordered_within, u.unordered_across
    );
    println!(
        "  way-blob indexdata against decoded contents: missing {}, unreadable {}, \
         wrong kind {}, wrong way-ID range {}, wrong way count {} (all counted as U)",
        u.index_missing,
        u.index_unreadable,
        u.index_wrong_kind,
        u.index_wrong_range,
        u.index_wrong_count
    );
    println!(
        "  inline coordinates: {}; with sub-decimicro precision {} (nonzero proves \
         non-default framing; zero does not rule it out); outside lat/lon range {}",
        t.coords, t.sub_decimicro, t.invalid_coords
    );
    println!(
        "  decoding wraps on overflow (library contract), so a wrapped coordinate that \
         lands in range is not detectable here"
    );
    println!();
}

fn print_sets(recs: &[WayBlobRec]) {
    let all = sum_set(recs, |_| true);
    println!("## Way-blob sets");
    println!();
    println!(
        "  {} way blobs, {:.2} GiB framed, {:.2} GiB decompressed",
        all.blobs,
        gib(all.frame),
        gib(all.decompressed)
    );
    println!();
    println!(
        "  {:<46} {:>9} {:>7} {:>10} {:>7} {:>7}",
        "set", "blobs", "blob%", "framed GiB", "bytes%", "decomp%"
    );
    for set in SETS {
        let s = sum_set(recs, set.test);
        println!(
            "  {:<46} {:>9} {:>6.2}% {:>10.3} {:>6.2}% {:>6.2}%",
            set.name,
            s.blobs,
            pct(s.blobs, all.blobs),
            gib(s.frame),
            pct(s.frame, all.frame),
            pct(s.decompressed, all.decompressed)
        );
    }
    println!();
    println!("  T is predicted, not observed (see --verify-output). The prediction assumes");
    println!("  single-kind way blocks, positive way IDs ascending within and across blobs,");
    println!("  and indexdata whose kind, way-ID range and way count match the decoded");
    println!("  blob; every way blob that breaks one of those is in U. Node and relation");
    println!("  blob indexdata is not checked: it does not route way blobs.");
    println!();
    print_bounds(recs, &all);
    print_splice(recs, &all);
}

fn print_bounds(recs: &[WayBlobRec], all: &SetSums) {
    let sup = |b: u16| !has(b, REC_UNSUPPORTED);
    let s_lo = sum_set(recs, |b| sup(b) && has(b, REC_S) && !has(b, REC_T));
    let s_un = sum_set(recs, |b| !sup(b) && has(b, REC_S));
    let m_lo = sum_set(recs, |b| sup(b) && has(b, REC_M) && !has(b, REC_T));
    let m_un = sum_set(recs, |b| !sup(b) && has(b, REC_M));
    println!("  Extra rewrite bounds (U blobs counted as already rewritten / as spliced today):");
    println!(
        "    inline rule:  {:.2}% .. {:.2}% of framed way bytes",
        pct(s_lo.frame, all.frame),
        pct(s_lo.frame + s_un.frame, all.frame)
    );
    println!(
        "    ID-only rule: {:.2}% .. {:.2}% of framed way bytes",
        pct(m_lo.frame, all.frame),
        pct(m_lo.frame + m_un.frame, all.frame)
    );
    println!();
}

/// Maximal runs of physically adjacent way blobs the policy leaves spliced:
/// a non-way frame between two way blobs ends a run.
fn splice_runs(recs: &[WayBlobRec], rewritten: impl Fn(u16) -> bool) -> u64 {
    let mut runs = 0u64;
    let mut in_run = false;
    for r in recs {
        if has(r.bits, REC_PHYS_GAP) {
            in_run = false;
        }
        let spliced = !rewritten(r.bits);
        if spliced && !in_run {
            runs += 1;
        }
        in_run = spliced;
    }
    runs
}

fn print_splice(recs: &[WayBlobRec], all: &SetSums) {
    println!("## Way-blob splice kept, per policy");
    println!();
    println!(
        "  {:<28} {:>9} {:>7} {:>10} {:>7} {:>12} {:>14}",
        "policy", "blobs", "blob%", "framed GiB", "bytes%", "splice runs", "MiB per run"
    );
    let policies = [
        BlobSet {
            name: "today (rewrite T)",
            test: |b| has(b, REC_T),
        },
        BlobSet {
            name: "inline fix (rewrite S|T)",
            test: |b| has(b, REC_S | REC_T),
        },
        BlobSet {
            name: "ID-only fix (rewrite M|T)",
            test: |b| has(b, REC_M | REC_T),
        },
    ];
    for BlobSet {
        name,
        test: rewritten,
    } in policies
    {
        let kept = sum_set(recs, |b| !rewritten(b));
        let runs = splice_runs(recs, rewritten);
        let per_run = if runs == 0 {
            0.0
        } else {
            kept.frame as f64 / runs as f64 / f64::from(1u32 << 20)
        };
        println!(
            "  {:<28} {:>9} {:>6.2}% {:>10.3} {:>6.2}% {:>12} {:>14.2}",
            name,
            kept.blobs,
            pct(kept.blobs, all.blobs),
            gib(kept.frame),
            pct(kept.frame, all.frame),
            runs,
            per_run
        );
    }
    println!();
    println!("  Splice runs count physically adjacent way blobs: a node or relation frame");
    println!("  between two kept way blobs ends a run (frames between way blobs are counted");
    println!("  under Base frames). The rows apply predicted T to U blobs as well; when U is");
    println!("  nonzero, read them against the bounds above.");
    println!();
}

/// `--verify-output`: predicted T against the blobs the real run rewrote.
fn print_verification(recs: &[WayBlobRec], t: &Totals) {
    println!("## Verification against the apply-changes output");
    println!();
    println!("  A blob counts as actually rewritten when its compressed body appears nowhere");
    println!("  in the output.");
    println!();
    let cells = [
        BlobSet {
            name: "predicted T, rewritten",
            test: |b| has(b, REC_T) && has(b, REC_ACTUAL),
        },
        BlobSet {
            name: "predicted T, kept",
            test: |b| has(b, REC_T) && !has(b, REC_ACTUAL),
        },
        BlobSet {
            name: "predicted kept, rewritten",
            test: |b| !has(b, REC_T) && has(b, REC_ACTUAL),
        },
        BlobSet {
            name: "predicted kept, kept",
            test: |b| !has(b, REC_T) && !has(b, REC_ACTUAL),
        },
    ];
    let all = sum_set(recs, |_| true);
    println!(
        "  {:<28} {:>9} {:>10} {:>12}",
        "way blobs", "blobs", "framed GiB", "of them in U"
    );
    for BlobSet { name, test } in cells {
        let s = sum_set(recs, test);
        let u = sum_set(recs, |b| test(b) && has(b, REC_UNSUPPORTED));
        println!(
            "  {:<28} {:>9} {:>10.3} {:>12}",
            name,
            s.blobs,
            gib(s.frame),
            u.blobs
        );
    }
    let wrong = sum_set(recs, |b| has(b, REC_T) != has(b, REC_ACTUAL));
    println!(
        "  T prediction {} for these {} way blobs ({} disagree)",
        if wrong.blobs == 0 {
            "VERIFIED"
        } else {
            "WRONG"
        },
        all.blobs,
        wrong.blobs
    );
    println!(
        "  non-way blobs actually rewritten: {} of {} ({:.3} GiB)",
        t.nonway_actual.rewritten,
        t.nonway_frames,
        gib(t.nonway_actual.rewritten_bytes)
    );
    println!();
}

fn print_classes(t: &Totals) {
    println!("## Ways and refs by way class");
    println!();
    println!(
        "  {:<16} {:>12} {:>14} {:>11} {:>12} {:>11} {:>12} {:>10} {:>10} {:>10}",
        "class",
        "ways",
        "refs",
        "stale ways",
        "stale refs",
        "probe ways",
        "probe refs",
        "del ways",
        "del refs",
        "nogeom hit"
    );
    for class in WayClass::ALL {
        let c = class.idx();
        println!(
            "  {:<16} {:>12} {:>14} {:>11} {:>12} {:>11} {:>12} {:>10} {:>10} {:>10}",
            class.label(),
            t.ways[c],
            t.refs[c],
            t.stale_ways[c],
            t.stale_refs[c],
            t.member_ways[c],
            t.member_refs[c],
            t.deleted_ref_ways[c],
            t.deleted_refs[c],
            t.nogeom_hit_ways[c]
        );
    }
    let refs: u64 = t.refs.iter().sum();
    let probe_refs: u64 = t.member_refs.iter().sum::<u64>() + t.deleted_refs.iter().sum::<u64>();
    println!();
    println!("  stale = holds an inline coordinate that differs from the live OSC node's;");
    println!("  probe = refs a live OSC node; del = refs an OSC-deleted node; nogeom hit =");
    println!("  way without inline coordinate arrays that refs a live OSC node (a policy case).");
    println!(
        "  Probe-set hit rate per ref: {probe_refs} of {refs} ({:.4}%), the rate a",
        pct(probe_refs, refs)
    );
    println!("  pre-filter in front of the probe map would see.");
    println!(
        "  Ways with no refs: {}; with refs but no inline coordinates: {}; length mismatch: {}",
        t.zero_ref_ways, t.nogeom_ways, t.geom_len_mismatch
    );
    println!();
}

fn count_flags(flags: &[AtomicU8], test: impl Fn(u8) -> bool) -> u64 {
    flags
        .iter()
        .filter(|f| test(f.load(Ordering::Relaxed)))
        .count() as u64
}

fn print_probe(probe: &Probe) {
    let live = &probe.live_flags;
    let n = live.len() as u64;
    let row = |label: &str, v: u64| println!("  {label:<62} {v:>10} ({:.2}%)", pct(v, n));
    println!("## Live OSC nodes (probe set: {n})");
    println!();
    row(
        "referenced by some base way",
        count_flags(live, |f| f & NODE_REF_ANY != 0),
    );
    row(
        "referenced by some untouched base way",
        count_flags(live, |f| f & NODE_REF_UNTOUCHED != 0),
    );
    row(
        "differs from some base way's inline coordinate",
        count_flags(live, |f| f & NODE_DIFF_ANY != 0),
    );
    row(
        "differs on some untouched base way",
        count_flags(live, |f| f & NODE_DIFF_UNTOUCHED != 0),
    );
    row(
        "  ... and referenced by no live OSC way",
        count_flags(live, |f| {
            f & NODE_DIFF_UNTOUCHED != 0 && f & NODE_OSC_WAY_REF == 0
        }),
    );
    row(
        "equal on every base way referencing it (tag-only or no-op)",
        count_flags(live, |f| f & NODE_EQ_ANY != 0 && f & NODE_DIFF_ANY == 0),
    );
    row(
        "both equal and differing across base ways (inconsistent base)",
        count_flags(live, |f| f & NODE_EQ_ANY != 0 && f & NODE_DIFF_ANY != 0),
    );
    row(
        "referenced by a base way without inline coordinates",
        count_flags(live, |f| f & NODE_REF_NOGEOM != 0),
    );
    row(
        "referenced by a live OSC way",
        count_flags(live, |f| f & NODE_OSC_WAY_REF != 0),
    );
    row(
        "referenced by no base way (new, or only on ways outside the base)",
        count_flags(live, |f| f & NODE_REF_ANY == 0),
    );
    println!();
    println!("  \"differs\" means base inline and OSC target disagree; it reads as \"moved by");
    println!("  this OSC\" only if the base was consistent before it.");
    println!();
    let del = &probe.deleted_flags;
    let d = del.len() as u64;
    let drow = |label: &str, v: u64| println!("  {label:<62} {v:>10} ({:.2}%)", pct(v, d));
    println!("## OSC-deleted nodes ({d}; the overlay drops delete coordinates)");
    println!();
    drow(
        "referenced by an untouched base way (base/OSC mismatch)",
        count_flags(del, |f| f & DEL_REF_UNTOUCHED != 0),
    );
    drow(
        "referenced by the base version of a live OSC way",
        count_flags(del, |f| f & DEL_REF_LIVE != 0),
    );
    drow(
        "referenced by a base way the OSC deletes",
        count_flags(del, |f| f & DEL_REF_DELETED != 0),
    );
    drow(
        "referenced by a live OSC way (OSC contradicts itself)",
        count_flags(del, |f| f & DEL_OSC_WAY_REF != 0),
    );
    println!();
}

fn print_hist(title: &str, hist: &[u64]) {
    let total: u64 = hist.iter().sum();
    println!("  {title}");
    for (b, &count) in hist.iter().enumerate() {
        if count > 0 {
            let lo = 1u64 << b;
            let hi = (1u64 << (b + 1)) - 1;
            println!(
                "    {lo:>7} .. {hi:<7} {count:>9} ({:.1}%)",
                pct(count, total)
            );
        }
    }
}

fn print_distribution(t: &Totals) {
    println!("## Staleness inside S blobs (untouched ways only)");
    println!();
    print_hist("stale ways per S blob:", &t.stale_ways_hist);
    print_hist("stale refs per S blob:", &t.stale_refs_hist);
    let total: u64 = t.stale_share_hist.iter().sum();
    println!("  stale ways as a share of the blob's ways:");
    for (i, &count) in t.stale_share_hist.iter().enumerate() {
        if count > 0 {
            let label = if i == 10 {
                "100%".to_owned()
            } else {
                format!("{}-{}%", i * 10, i * 10 + 10)
            };
            println!("    {label:<8} {count:>9} ({:.1}%)", pct(count, total));
        }
    }
    println!();
}

struct Clustering {
    n: u64,
    k: u64,
    runs: u64,
    leading_gap: u64,
    trailing_gap: u64,
    gap_hist: [u64; HIST_BUCKETS],
}

fn clustering(recs: &[WayBlobRec], test: fn(u16) -> bool) -> Clustering {
    let mut c = Clustering {
        n: recs.len() as u64,
        k: 0,
        runs: 0,
        leading_gap: 0,
        trailing_gap: 0,
        gap_hist: [0; HIST_BUCKETS],
    };
    let mut prev_member = false;
    let mut gap: u64 = 0;
    for r in recs {
        let member = test(r.bits);
        if member {
            if c.k == 0 {
                c.leading_gap = gap;
            } else if gap > 0 {
                c.gap_hist[log2_bucket(gap)] += 1;
            }
            if !prev_member {
                c.runs += 1;
            }
            c.k += 1;
            gap = 0;
        } else {
            gap += 1;
        }
        prev_member = member;
    }
    c.trailing_gap = if c.k == 0 { 0 } else { gap };
    c
}

fn print_clustering(recs: &[WayBlobRec]) {
    println!("## Clustering along the way-blob sequence");
    println!();
    println!("  Runs = maximal stretches of consecutive member blobs. Expected runs for k");
    println!("  members placed uniformly at random among n blobs: k (n - k + 1) / n.");
    println!("  Observed well below expected means clustered; near it means spread out.");
    println!();
    let sets = [
        BlobSet {
            name: "S",
            test: |b| has(b, REC_S),
        },
        BlobSet {
            name: "S minus T",
            test: |b| has(b, REC_S) && !has(b, REC_T),
        },
        BlobSet {
            name: "T",
            test: |b| has(b, REC_T),
        },
        BlobSet {
            name: "M minus T",
            test: |b| has(b, REC_M) && !has(b, REC_T),
        },
    ];
    for BlobSet { name, test } in sets {
        let c = clustering(recs, test);
        let expected = if c.n == 0 {
            0.0
        } else {
            c.k as f64 * (c.n - c.k + 1) as f64 / c.n as f64
        };
        println!(
            "  {name}: k {} of n {}, runs {} (expected {:.1}, ratio {:.2}), \
             leading gap {}, trailing gap {}",
            c.k,
            c.n,
            c.runs,
            expected,
            if expected > 0.0 {
                c.runs as f64 / expected
            } else {
                0.0
            },
            c.leading_gap,
            c.trailing_gap
        );
        print_hist("internal gaps between member runs (blobs):", &c.gap_hist);
    }
    println!();
    print_deciles(recs);
}

fn print_deciles(recs: &[WayBlobRec]) {
    let n = recs.len();
    let total_bytes: u64 = recs.iter().map(|r| r.frame).sum();
    let mut by_count = [[0u64; 3]; 10];
    let mut by_bytes = [[0u64; 3]; 10];
    let mut before: u64 = 0;
    for (i, r) in recs.iter().enumerate() {
        let dc = (i * 10 / n.max(1)).min(9);
        let db = usize::try_from(before * 10 / total_bytes.max(1))
            .unwrap_or(9)
            .min(9);
        before += r.frame;
        let s = u64::from(has(r.bits, REC_S));
        let st = u64::from(has(r.bits, REC_S | REC_T));
        by_count[dc][0] += 1;
        by_count[dc][1] += s;
        by_count[dc][2] += st;
        by_bytes[db][0] += r.frame;
        by_bytes[db][1] += s * r.frame;
        by_bytes[db][2] += st * r.frame;
    }
    println!("  Per decile of the way section (in way-ID order):");
    println!(
        "    {:<7} {:>10} {:>10} {:>12}   {:>12} {:>12}",
        "decile", "blobs", "S blob%", "S|T blob%", "S bytes%", "S|T bytes%"
    );
    for (d, (c, b)) in by_count.iter().zip(&by_bytes).enumerate() {
        println!(
            "    {:<7} {:>10} {:>9.2}% {:>11.2}%   {:>11.2}% {:>11.2}%",
            d + 1,
            c[0],
            pct(c[1], c[0]),
            pct(c[2], c[0]),
            pct(b[1], b[0]),
            pct(b[2], b[0])
        );
    }
    println!("    (blob columns split by blob count, byte columns by framed bytes)");
    println!();
}

fn print_timing(t: &Totals, parse_s: f64, probe_s: f64, scan_s: f64) {
    println!("## Timing");
    println!();
    println!(
        "  OSC parse {parse_s:.1} s, probe build {probe_s:.1} s, base scan {scan_s:.1} s wall"
    );
    println!(
        "  summed over workers: decompress+parse {:.1} s, way probe loop {:.1} s",
        t.decode_ns as f64 / 1e9,
        t.probe_ns as f64 / 1e9
    );
    println!("  These are summed elapsed task times, not CPU time. Decode covers every blob");
    println!("  (this program decodes node and relation blobs to learn their kind); the probe");
    println!("  loop includes element iteration, ref and coordinate decoding and bookkeeping,");
    println!("  so it is a weak anchor for a wire-level detector, not a prediction.");
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().collect();
    let verify_output = match args.len() {
        3 => None,
        5 if args[3] == "--verify-output" => Some(Path::new(&args[4])),
        _ => {
            eprintln!(
                "Usage: stale_way_blobs <base-altw.osm.pbf> <next.osc.gz> \
                 [--verify-output <apply-changes-l-output.osm.pbf>]"
            );
            std::process::exit(1);
        }
    };
    let base = Path::new(&args[1]);
    let osc = Path::new(&args[2]);

    let output_bodies = match verify_output {
        Some(path) => {
            eprintln!("Hashing blob bodies of {}...", path.display());
            Some(hash_output_bodies(path)?)
        }
        None => None,
    };

    eprintln!("Parsing {}...", osc.display());
    let start = Instant::now();
    let overlay = parse_osc_file(osc)?;
    let parse_s = start.elapsed().as_secs_f64();
    let overlay_bytes = overlay.heap_size_estimate();

    let start = Instant::now();
    let probe = Probe::build(&overlay)?;
    drop(overlay);
    let probe_s = start.elapsed().as_secs_f64();

    eprintln!("Scanning {}...", base.display());
    let start = Instant::now();
    let ScanResult { totals, recs } = scan_file(base, &probe, output_bodies.as_ref())?;
    let scan_s = start.elapsed().as_secs_f64();

    print_inputs(base, osc, overlay_bytes, &probe, &totals);
    print_sets(&recs);
    if output_bodies.is_some() {
        print_verification(&recs, &totals);
    }
    print_classes(&totals);
    print_probe(&probe);
    print_distribution(&totals);
    print_clustering(&recs);
    print_timing(&totals, parse_s, probe_s, scan_s);
    Ok(())
}
