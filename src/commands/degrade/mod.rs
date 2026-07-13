//! Produce a valid-but-adversarial PBF by stripping properties or
//! perturbing structure. A "make our lives difficult" tool for exercising
//! code paths that require less-optimised inputs (unsorted, missing
//! indexdata, scattered coords).
//!
//! Transformations: `--unsort`, `--unsort-intra`, `--strip-locations`,
//! `--strip-indexdata`, `--strip-tagdata`, `--strip-bbox`, `--drop-ids`.
//! Flags compose (except the two unsort modes, which are mutually
//! exclusive).
//!
//! Implementation paths:
//!
//! - Pure passthrough (`--strip-indexdata`, `--strip-tagdata`, and/or
//!   `--strip-bbox` with no unsort/strip-locations/drop-ids): with no
//!   `--generator`/`--output-header` override the input `HeaderBlock`
//!   payload is preserved verbatim (the outer Blob envelope is
//!   re-compressed, so the decompressed `HeaderBlock` protobuf is
//!   field-identical, not blob-byte-identical) - it is forwarded
//!   field-for-field and only the bbox (field 1) is surgically removed under
//!   `--strip-bbox`, so `source`, custom optional features
//!   (`pbfhogg.WayMembers-v1`, `SharedNodePins-v1`), a non-default
//!   `writingprogram`, replication metadata, and unknown fields all survive
//!   byte-for-byte. (When a header override is set the header is rebuilt via
//!   `HeaderBuilder` so the override wins, dropping the bbox under
//!   `--strip-bbox`.) Raw OsmData blob frames are then reframed by copying
//!   the original `BlobHeader` through byte-for-byte and clearing only the
//!   targeted field(s) - `indexdata` (field 2) and/or `tagdata` (field 4).
//!   `--strip-bbox` is entirely an OSMHeader change; it never touches an
//!   OsmData `BlobHeader` or payload.
//!   Every other header field is preserved verbatim: the untouched hint's
//!   original bytes (a v1 index stays v1), `WayMembers-v1` (field 5), and
//!   any unknown/extension fields. Blob bytes are bit-identical; only the
//!   targeted header field changes. Mirrors `cat`'s passthrough but drops
//!   header hints instead of adding them.
//! - Decode path (either unsort mode, `--strip-locations`, or `--drop-ids`): three
//!   sequential per-kind phases driven by `parallel_classify_phase`.
//!   Workers decode one input blob, filter to the current kind, and
//!   re-encode. Without an unsort mode, workers pre-frame full cap-sized
//!   blocks (parallel re-encode) and ship the trailing `M%cap` elements
//!   as `Owned*` to a merge thread that flushes a central `BlockBuilder`
//!   between input blobs (sort preserved). Under either unsort mode,
//!   workers ship every element as `Owned*` so the merge thread can
//!   apply the cap-1 swap per kind in a serial state machine.
//!
//! Both unsort modes clear `Sort.Type_then_ID` and swap one adjacent
//! same-kind element pair per kind. They differ in which pair is swapped,
//! which decides whether the disorder lands across an output-blob
//! boundary or inside a single blob:
//!
//! - `--unsort` (cross-blob overlap): swaps the pair straddling the
//!   `block_cap` boundary (elements #block_cap and #block_cap+1). The
//!   per-input-blob boundary flush is suppressed, so the central
//!   `BlockBuilder` packs continuously to `block_cap`: the newer element
//!   fills and flushes the current output block, and the held
//!   smaller-id element opens the next one. The result is exactly one
//!   adjacent same-kind blob pair per kind whose indexdata ID ranges
//!   overlap - the minimum perturbation that makes `sort`'s
//!   `detect_overlaps` dispatch to the overlap-rewrite path. The two
//!   output blobs stay internally ID-monotone. Valid for any
//!   `block_cap >= 1`.
//! - `--unsort-intra` (intra-blob inversion): swaps the first two
//!   same-kind elements. That pair always lands at the start of the
//!   first output block (positions 1 and 2), so the descending step
//!   sits inside a blob for any `block_cap >= 2` - independent of where
//!   input- or output-blob boundaries fall, and in particular even when
//!   one input blob carries more than `block_cap` same-kind elements.
//!   Blob ID ranges stay non-overlapping, so `detect_overlaps` returns
//!   zero - this is the adversarial shape for `sort`'s intra-blob
//!   monotonicity blind spot (a blob internally unsorted but
//!   range-disjoint passes straight through while the header still
//!   claims sortedness). Requires `block_cap >= 2`; a cap of 1 cannot
//!   hold two same-kind elements in one block and is rejected up front.

use std::collections::BinaryHeap;
use std::path::Path;

use rustc_hash::FxHashSet;

use super::{
    HeaderOverrides, Result, build_output_header, ensure_node_capacity_local,
    ensure_relation_capacity_local, ensure_way_capacity_local, flush_local, require_indexdata,
    writer_from_header_bytes,
};
use crate::blob::BlobKind;
use crate::blob_meta::ElemKind;
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::file_reader::FileReader;
use crate::file_writer::FileWriter;
use crate::owned::{
    OwnedElement, OwnedNode, OwnedRelation, OwnedWay, dense_node_metadata, element_metadata,
    read_dense_node, read_node, read_relation, read_way,
};
use crate::read::raw_frame::read_raw_frame;
use crate::writer::{
    Compression, PbfWriter, frame_blob_pipelined, strip_blob_header_fields,
    strip_header_block_fields,
};
use crate::{Element, ElementReader};

/// Default per-block element cap. Matches the `BlockBuilder` default and
/// the PBF interop convention. Tests pass a smaller cap via the hidden
/// `--block-cap` CLI flag so fixtures don't need 8000+ elements per kind
/// to exercise the `--unsort` swap.
pub const DEFAULT_BLOCK_CAP: usize = 8000;

/// Batch size for the parallel-framing fan-out on the merge thread.
const FRAME_BATCH: usize = 32;

/// Reproducibility contract: map an `ElemKind` to the byte that feeds
/// `--drop-ids` selection. The exact values (Node=0, Way=1, Relation=2)
/// are load-bearing - `drop_hash` mixes this byte numerically into the
/// splitmix64 finalizer and `DropKey` orders on it, so the same
/// `--drop-ids N:SEED` must drop the byte-identical element set across
/// builds. `ElemKind` carries no `repr(u8)` and no `Ord`, so this
/// explicit match is the only sanctioned bridge from the kind enum into
/// the numeric selection domain; never route around it. The golden
/// vectors in this module's tests pin these values.
fn hash_kind(kind: ElemKind) -> u8 {
    match kind {
        ElemKind::Node => 0,
        ElemKind::Way => 1,
        ElemKind::Relation => 2,
    }
}

/// Parsed `--drop-ids N:SEED` argument.
#[derive(Clone, Copy, Debug)]
pub struct DropSpec {
    pub n: u64,
    pub seed: u64,
}

impl DropSpec {
    /// Parse the required absolute-count and seed pair.
    pub fn parse(s: &str) -> Result<Self> {
        let (n_str, seed_str) = s
            .split_once(':')
            .ok_or("--drop-ids expects N:SEED (e.g. 5000:42); the ':' separator is required")?;
        let n = n_str
            .trim()
            .parse()
            .map_err(|_| format!("--drop-ids: N must be a non-negative integer, got {n_str:?}"))?;
        let seed = seed_str.trim().parse().map_err(|_| {
            format!("--drop-ids: SEED must be a non-negative integer, got {seed_str:?}")
        })?;
        if n == 0 {
            return Err("--drop-ids: N must be >= 1 (dropping zero elements is a no-op)".into());
        }
        Ok(Self { n, seed })
    }
}

/// splitmix64 finalizer used exclusively for reproducible drop selection.
fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn drop_hash(kind: u8, id: i64, seed: u64) -> u64 {
    #[allow(clippy::cast_sign_loss)]
    let idw = id as u64;
    mix64(
        idw.wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ u64::from(kind).wrapping_mul(0xD1B5_4A32_D192_ED03)
            ^ seed,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DropKey {
    hash: u64,
    kind: u8,
    id: i64,
}

#[derive(Default)]
struct DropSets {
    nodes: FxHashSet<i64>,
    ways: FxHashSet<i64>,
    relations: FxHashSet<i64>,
}

impl DropSets {
    fn for_kind(&self, kind: ElemKind) -> &FxHashSet<i64> {
        match kind {
            ElemKind::Node => &self.nodes,
            ElemKind::Way => &self.ways,
            ElemKind::Relation => &self.relations,
        }
    }

    fn insert(&mut self, key: DropKey) {
        // `key.kind` is the `hash_kind()` selection byte (the domain
        // `DropKey` orders on; `ElemKind` has no `Ord`). Route to the
        // matching set through the same boundary so the byte mapping lives
        // in exactly one place.
        let set = if key.kind == hash_kind(ElemKind::Node) {
            &mut self.nodes
        } else if key.kind == hash_kind(ElemKind::Way) {
            &mut self.ways
        } else {
            &mut self.relations
        };
        set.insert(key.id);
    }
}

struct BlockDrop {
    matched: u64,
    smallest: Vec<DropKey>,
}

fn keep_smallest(heap: &mut BinaryHeap<DropKey>, n: u64, key: DropKey) {
    if (heap.len() as u64) < n {
        heap.push(key);
    } else if let Some(largest) = heap.peek()
        && key < *largest
    {
        heap.pop();
        heap.push(key);
    }
}

/// Set of degradations to apply. At least one flag must be set.
#[derive(Clone, Copy, Debug, Default)]
pub struct DegradeFlags {
    /// Cross-blob unsort: adjacent same-kind blobs get overlapping ID ranges.
    pub unsort: bool,
    /// Intra-blob unsort: one same-kind blob gets an internal ID inversion.
    pub unsort_intra: bool,
    pub strip_locations: bool,
    pub strip_indexdata: bool,
    /// Clear the per-blob `BlobHeader.tagdata` (field 4) tag key index so
    /// `tags-filter`'s no-hint fallback path is exercised. Like
    /// `--strip-indexdata` it is a header-only change, so it composes with
    /// the passthrough path and leaves `indexdata` alone.
    pub strip_tagdata: bool,
    /// Clear the `HeaderBlock.bbox` (field 1) from the OSMHeader so the
    /// output declares no file-level bounding box. Purely an OSMHeader
    /// rewrite - every OsmData blob passes through untouched - so like the
    /// other header-only strips it composes with the passthrough path and
    /// carries no indexdata precondition.
    pub strip_bbox: bool,
    /// Deterministically remove exactly this many unique element IDs.
    pub drop_ids: Option<DropSpec>,
}

impl DegradeFlags {
    pub fn any(self) -> bool {
        self.unsort
            || self.unsort_intra
            || self.strip_locations
            || self.strip_indexdata
            || self.strip_tagdata
            || self.strip_bbox
            || self.drop_ids.is_some()
    }

    /// Returns `true` if elements must be decoded and re-encoded. Only the
    /// header-only strips (`--strip-indexdata` / `--strip-tagdata` /
    /// `--strip-bbox`, alone or together) can run as a pure blob-level
    /// passthrough.
    fn needs_decode(self) -> bool {
        self.unsort || self.unsort_intra || self.strip_locations || self.drop_ids.is_some()
    }

    /// Either unsort mode: workers ship every matching element as `Owned*`,
    /// the merge thread runs the cap-1 swap state machine, and the output
    /// header's `Sort.Type_then_ID` flag is cleared.
    fn unsort_any(self) -> bool {
        self.unsort || self.unsort_intra
    }

    /// Cross-blob `--unsort` suppresses the per-input-blob boundary flush so
    /// the central `BlockBuilder` packs to `block_cap` and the swap straddles
    /// a genuine output-blob boundary (adjacent blobs overlap). Every other
    /// mode - including `--unsort-intra` - keeps the flush so output blobs
    /// mirror input blobs. (`--unsort-intra`'s swap is confined to a single
    /// blob by its hold-at-position-1 placement, not by the flush, so it
    /// stays intra-blob regardless of input blob sizes.)
    fn suppress_boundary_flush(self) -> bool {
        self.unsort
    }
}

/// Per-run statistics from a degrade operation.
pub struct DegradeStats {
    pub blobs_written: u64,
    pub elements_written: u64,
    pub dropped: u64,
    pub flags: DegradeFlags,
}

impl DegradeStats {
    pub fn print_summary(&self) {
        let mut applied: Vec<&str> = Vec::new();
        if self.flags.unsort {
            applied.push("--unsort");
        }
        if self.flags.unsort_intra {
            applied.push("--unsort-intra");
        }
        if self.flags.strip_locations {
            applied.push("--strip-locations");
        }
        if self.flags.strip_indexdata {
            applied.push("--strip-indexdata");
        }
        if self.flags.strip_tagdata {
            applied.push("--strip-tagdata");
        }
        if self.flags.strip_bbox {
            applied.push("--strip-bbox");
        }
        if self.flags.drop_ids.is_some() {
            applied.push("--drop-ids");
        }
        let dropped = if self.dropped > 0 {
            format!(" (dropped {} elements)", self.dropped)
        } else {
            String::new()
        };
        eprintln!(
            "Degraded {} elements across {} blobs{} (applied: {})",
            self.elements_written,
            self.blobs_written,
            dropped,
            applied.join(" "),
        );
    }
}

/// Apply the requested degradations to `input` and emit `output`.
///
/// `block_cap` is the per-block element cap used by the decode path's
/// `BlockBuilder`. Production callers pass `DEFAULT_BLOCK_CAP`; tests pass
/// a smaller value so the `--unsort` swap can be exercised on fixtures of
/// modest size.
///
/// `force` skips the indexdata precondition required by the decode path's
/// per-kind classify pipeline. Has no effect on the passthrough path.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub fn degrade(
    input: &Path,
    output: &Path,
    flags: DegradeFlags,
    block_cap: usize,
    compression: Compression,
    direct_io: bool,
    io_uring: bool,
    force: bool,
    overrides: &HeaderOverrides,
) -> Result<DegradeStats> {
    if !flags.any() {
        return Err("degrade requires at least one transformation flag \
                    (--unsort, --unsort-intra, --strip-locations, \
                     --strip-indexdata, --strip-tagdata, --strip-bbox)"
            .into());
    }
    if flags.unsort && flags.unsort_intra {
        return Err(
            "--unsort and --unsort-intra are mutually exclusive: --unsort \
                    produces cross-blob ID-range overlap, --unsort-intra produces \
                    an intra-blob inversion"
                .into(),
        );
    }
    if block_cap == 0 {
        return Err("--block-cap must be > 0".into());
    }
    // An intra-blob inversion needs two same-kind elements sitting inside
    // one output block. A cap of 1 puts every element in its own block, so
    // the requested shape is impossible - reject it rather than silently
    // clearing Sort.Type_then_ID and emitting an untouched (still-monotone)
    // stream. `--unsort` (cross-blob overlap) IS achievable at cap 1: two
    // adjacent single-element blobs with a descending step overlap, so it
    // stays supported.
    if flags.unsort_intra && block_cap < 2 {
        return Err("--unsort-intra needs --block-cap >= 2: an intra-blob \
                    inversion requires at least two same-kind elements inside \
                    one output block, which a cap of 1 cannot hold"
            .into());
    }

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("degrade_unsort", i64::from(flags.unsort));
        crate::debug::emit_counter("degrade_unsort_intra", i64::from(flags.unsort_intra));
        crate::debug::emit_counter("degrade_strip_locations", i64::from(flags.strip_locations));
        crate::debug::emit_counter("degrade_strip_indexdata", i64::from(flags.strip_indexdata));
        crate::debug::emit_counter("degrade_strip_tagdata", i64::from(flags.strip_tagdata));
        crate::debug::emit_counter("degrade_strip_bbox", i64::from(flags.strip_bbox));
        crate::debug::emit_counter("degrade_drop_ids", i64::from(flags.drop_ids.is_some()));
        if let Some(spec) = flags.drop_ids {
            crate::debug::emit_counter("degrade_drop_n", spec.n as i64);
            crate::debug::emit_counter("degrade_drop_seed", spec.seed as i64);
        }
        crate::debug::emit_counter("degrade_block_cap", block_cap as i64);
    }

    let stats = if flags.needs_decode() {
        degrade_decode_path(
            input,
            output,
            flags,
            block_cap,
            compression,
            direct_io,
            io_uring,
            force,
            overrides,
        )?
    } else {
        degrade_passthrough(
            input,
            output,
            flags,
            compression,
            direct_io,
            io_uring,
            overrides,
        )?
    };

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("degrade_blobs_written", stats.blobs_written as i64);
        crate::debug::emit_counter("degrade_elements_written", stats.elements_written as i64);
        crate::debug::emit_counter("degrade_dropped_elements", stats.dropped as i64);
    }

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Passthrough: --strip-indexdata and/or --strip-tagdata only
// ---------------------------------------------------------------------------

/// Raw blob frame iteration that clears the requested header-only fields
/// (`BlobHeader.indexdata` and/or `tagdata`) while preserving every other
/// header field and the entire blob payload byte-for-byte.
///
/// Each output `BlobHeader` is the input header with only the targeted
/// field(s) removed - so the untargeted hint keeps its exact original bytes
/// (a v1 index is never upgraded to v2, an undeserializable index is never
/// dropped), and `WayMembers-v1` (field 5) plus any unknown/extension fields
/// carry through unchanged. Blob payload bytes are forwarded verbatim -
/// inline `LocationsOnWays` coordinates, sortedness, and every element-level
/// property pass through unchanged. The output's `LocationsOnWays` and
/// `Sort.Type_then_ID` header features are preserved when the input declared
/// them, since the blob bytes still encode that data. `indexdata` survives
/// unless `--strip-indexdata` is set; `tagdata` survives unless
/// `--strip-tagdata` is set - so `--strip-tagdata` alone yields a still-indexed
/// file.
/// Build the OSMHeader payload (raw `HeaderBlock` protobuf bytes) for the
/// passthrough path.
///
/// With no `--generator` / `--output-header` override the input `HeaderBlock`
/// payload is preserved verbatim (the outer Blob envelope is re-compressed,
/// so this is field-identical, not blob-byte-identical): the decompressed
/// `HeaderBlock` protobuf is forwarded field-for-field, and only the bbox
/// (field 1) is surgically removed under
/// `--strip-bbox`. So `source` (field 17), custom/optional features
/// (`pbfhogg.WayMembers-v1`, `SharedNodePins-v1`, `LocationsOnWays`,
/// `Sort.Type_then_ID`), a non-default `writingprogram`, the osmosis
/// replication metadata, and any unknown/extension fields all survive
/// byte-for-byte. This matters because rebuilding through
/// [`HeaderBuilder::from_header`](crate::block_builder::HeaderBuilder::from_header)
/// deliberately drops `source`, custom optional features, and unknown fields
/// and replaces a non-default `writingprogram` - so a plain `--strip-bbox`
/// (or `--strip-indexdata` / `--strip-tagdata`) would otherwise silently
/// mutate far more than its target field.
///
/// When an override *is* present the user is explicitly rewriting header
/// fields, so the header is rebuilt through `HeaderBuilder` (with the bbox
/// omitted under `--strip-bbox`) and the overrides applied. `LocationsOnWays`
/// is re-declared on this rebuild path when the input carried it, since the
/// blob payloads still encode inline way coordinates.
fn passthrough_header_bytes(
    input: &Path,
    flags: DegradeFlags,
    overrides: &HeaderOverrides,
    direct_io: bool,
) -> Result<Vec<u8>> {
    if overrides.is_empty() {
        // Verbatim path: forward the original HeaderBlock protobuf, removing
        // only the bbox field when requested. The OSMHeader is always the
        // first blob in a well-formed PBF.
        let mut reader = FileReader::open(input, direct_io)?;
        let mut file_offset: u64 = 0;
        let frame = read_raw_frame(&mut reader, &mut file_offset)?
            .ok_or("input PBF is empty: no OSMHeader blob")?;
        if !matches!(frame.blob_type, BlobKind::OsmHeader) {
            return Err("input PBF does not start with an OSMHeader blob".into());
        }
        let mut header_block = Vec::new();
        crate::read::decompress::decompress_blob_data_into(frame.blob_bytes(), &mut header_block)?;
        if flags.strip_bbox {
            let mut stripped = Vec::with_capacity(header_block.len());
            strip_header_block_fields(&header_block, &[1], &mut stripped)?;
            Ok(stripped)
        } else {
            Ok(header_block)
        }
    } else {
        // Override present: rebuild so --generator / --output-header take
        // effect. This is the lossy-but-intentional path.
        let reader = ElementReader::open(input, direct_io)?;
        let header = reader.header().clone();
        build_output_header(&header, header.is_sorted(), overrides, |hb| {
            let mut hb = hb;
            if header.has_locations_on_ways() {
                hb = hb.optional_feature("LocationsOnWays");
            }
            if flags.strip_bbox {
                hb = hb.without_bbox();
            }
            hb
        })
    }
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn degrade_passthrough(
    input: &Path,
    output: &Path,
    flags: DegradeFlags,
    compression: Compression,
    direct_io: bool,
    io_uring: bool,
    overrides: &HeaderOverrides,
) -> Result<DegradeStats> {
    let header_bytes = passthrough_header_bytes(input, flags, overrides, direct_io)?;

    let mut writer =
        writer_from_header_bytes(output, compression, &header_bytes, direct_io, io_uring)?;

    let mut reader = FileReader::open(input, direct_io)?;
    let mut file_offset: u64 = 0;
    let mut blobs_written: u64 = 0;

    crate::debug::emit_marker("DEGRADE_PASSTHROUGH_START");
    while let Some(frame) = read_raw_frame(&mut reader, &mut file_offset)? {
        match &frame.blob_type {
            BlobKind::OsmHeader => {}
            BlobKind::OsmData => {
                // Preserve every original BlobHeader field verbatim and clear
                // only the field(s) the active strip flags target. Working
                // from the original header bytes (not parsed values) keeps the
                // untouched indexdata byte-identical - a v1 index stays v1 -
                // and carries WayMembers-v1 and any unknown header fields
                // through unchanged.
                let reframed = reframe_raw(
                    frame.header_bytes(),
                    frame.blob_bytes(),
                    flags.strip_indexdata,
                    flags.strip_tagdata,
                )?;
                writer.write_raw_owned(reframed)?;
                blobs_written += 1;
            }
            _ => {}
        }
    }
    crate::debug::emit_marker("DEGRADE_PASSTHROUGH_END");

    crate::debug::emit_marker("DEGRADE_FLUSH_START");
    writer.flush()?;
    crate::debug::emit_marker("DEGRADE_FLUSH_END");

    Ok(DegradeStats {
        blobs_written,
        elements_written: 0,
        dropped: 0,
        flags,
    })
}

/// Reframe a raw OSMData blob, re-emitting its original `BlobHeader` with only
/// the flagged hint fields removed - `indexdata` (field 2) under
/// `strip_indexdata`, `tagdata` (field 4) under `strip_tagdata`. Every other
/// header field is copied through byte-for-byte via `strip_blob_header_fields`,
/// so the preserved `indexdata` keeps its exact on-wire form (a 26-byte v1
/// index is never upgraded to the 42-byte v2 layout), and `WayMembers-v1`
/// (field 5) plus any unknown/extension fields survive untouched. The blob
/// payload bytes - and therefore the preserved `datasize` (field 3) - are
/// copied verbatim.
fn reframe_raw(
    header_bytes: &[u8],
    blob_bytes: &[u8],
    strip_indexdata: bool,
    strip_tagdata: bool,
) -> std::io::Result<Vec<u8>> {
    let mut strip_fields: Vec<u32> = Vec::with_capacity(2);
    if strip_indexdata {
        strip_fields.push(2);
    }
    if strip_tagdata {
        strip_fields.push(4);
    }
    let mut header_buf = Vec::new();
    strip_blob_header_fields(header_bytes, &strip_fields, &mut header_buf)?;
    let header_len = u32::try_from(header_buf.len()).map_err(|_| {
        std::io::Error::other(format!("header too large: {} bytes", header_buf.len()))
    })?;
    let total_len = 4 + header_buf.len() + blob_bytes.len();
    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&header_len.to_be_bytes());
    out.extend_from_slice(&header_buf);
    out.extend_from_slice(blob_bytes);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Decode path: --unsort / --unsort-intra and/or --strip-locations (with optional --strip-indexdata / --strip-tagdata)
// ---------------------------------------------------------------------------

/// Trailing-partial payload: 0 to `cap-1` elements that didn't fill a
/// full output block in the worker. Under either unsort mode workers pack
/// every matching element here so the merge thread can apply the swap.
enum KindPayload {
    Nodes(Vec<OwnedNode>),
    Ways(Vec<OwnedWay>),
    Relations(Vec<OwnedRelation>),
}

impl KindPayload {
    fn len(&self) -> usize {
        match self {
            Self::Nodes(v) => v.len(),
            Self::Ways(v) => v.len(),
            Self::Relations(v) => v.len(),
        }
    }
}

/// One worker's output for one input blob: framed full blocks plus the
/// trailing partial. Under either unsort mode, `full_framed` is always empty.
struct WorkerOutput {
    full_framed: Vec<Vec<u8>>,
    tail: KindPayload,
}

/// Per-kind unsort state held on the merge thread. Holds one element at
/// the 1-based arrival position `hold_at` and re-injects it one element
/// later, producing a single adjacent-pair swap per kind.
///
/// The two modes differ only in `hold_at`, which decides where the swap
/// lands:
///
/// - `--unsort` (cross-blob overlap): `hold_at = block_cap`. With the
///   boundary flush suppressed the central builder packs to `block_cap`,
///   so the newer element fills and flushes the current output block and
///   the held smaller-id element opens the next one - the two blobs'
///   ID ranges overlap. Reachable for any `block_cap >= 1`.
/// - `--unsort-intra` (intra-blob inversion): `hold_at = 1`. The swap
///   fires on the first two same-kind elements, which always land at the
///   start of the first output block (positions 1 and 2), so the
///   descending step sits inside a blob for any `block_cap >= 2`. This is
///   independent of input/output blob boundaries, so it stays intra-blob
///   even when one input blob carries more than `block_cap` same-kind
///   elements.
struct UnsortKindState {
    held: Option<OwnedElement>,
    seen: u64,
    fired: bool,
    hold_at: u64,
}

impl UnsortKindState {
    fn new(flags: DegradeFlags, cap: usize) -> Self {
        // block_cap validation upstream guarantees hold_at is reachable:
        // >= 1 for --unsort, >= 2 for --unsort-intra.
        let hold_at = if flags.unsort_intra { 1 } else { cap as u64 };
        Self {
            held: None,
            seen: 0,
            fired: false,
            hold_at,
        }
    }

    fn should_hold(&self) -> bool {
        self.held.is_none() && !self.fired && self.seen + 1 == self.hold_at
    }

    fn should_inject_after(&self) -> bool {
        self.seen + 1 == self.hold_at + 1 && self.held.is_some()
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn degrade_decode_path(
    input: &Path,
    output: &Path,
    flags: DegradeFlags,
    block_cap: usize,
    compression: Compression,
    direct_io: bool,
    io_uring: bool,
    force: bool,
    overrides: &HeaderOverrides,
) -> Result<DegradeStats> {
    require_indexdata(
        input,
        direct_io,
        force,
        "input PBF has no blob-level indexdata. degrade's decode path uses the \
         parallel per-kind classify pipeline, which needs indexdata to build \
         per-kind blob schedules.",
    )?;

    // Cap glibc arenas to prevent cross-thread alloc/free fragmentation in
    // the per-blob worker pool. Same precedent as `cat --clean` and `repack`.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }

    let header = {
        let reader = ElementReader::open(input, direct_io)?;
        // Warn about LocationsOnWays loss on the decode path - BlockBuilder
        // does not preserve inline way-node coordinates. `--strip-locations`
        // makes the loss explicit, so suppress the warning in that case.
        if !flags.strip_locations {
            super::warn_locations_on_ways_loss(reader.header());
        }
        reader.header().clone()
    };

    let (node_schedule, way_schedule, rel_schedule, shared_file) =
        crate::scan::classify::build_classify_schedules_split(input)?;

    // Selection must finish before opening the writer: an over-large N must
    // not leave a header-only output behind.
    let drop_sets = if let Some(spec) = flags.drop_ids {
        crate::debug::emit_marker("DEGRADE_DROP_SELECT_START");
        let sets = select_drop_sets(
            &shared_file,
            &node_schedule,
            &way_schedule,
            &rel_schedule,
            spec,
        )?;
        crate::debug::emit_marker("DEGRADE_DROP_SELECT_END");
        Some(sets)
    } else {
        None
    };

    let preserve_sorted = !flags.unsort_any() && header.is_sorted();
    let header_bytes = build_output_header(&header, preserve_sorted, overrides, |hb| {
        if flags.strip_bbox {
            hb.without_bbox()
        } else {
            hb
        }
    })?;
    let mut writer =
        writer_from_header_bytes(output, compression, &header_bytes, direct_io, io_uring)?;

    let mut blobs_written: u64 = 0;
    let mut elements_written: u64 = 0;
    let mut unsort_fired = [false; 3];

    crate::debug::emit_marker("DEGRADE_NODES_START");
    let s = run_kind_phase(
        &shared_file,
        &node_schedule,
        ElemKind::Node,
        block_cap,
        flags,
        compression,
        drop_sets.as_ref().map(|sets| sets.for_kind(ElemKind::Node)),
        &mut writer,
    )?;
    crate::debug::emit_marker("DEGRADE_NODES_END");
    blobs_written += s.blobs;
    elements_written += s.elements;
    unsort_fired[0] = s.unsort_fired;

    crate::debug::emit_marker("DEGRADE_WAYS_START");
    let s = run_kind_phase(
        &shared_file,
        &way_schedule,
        ElemKind::Way,
        block_cap,
        flags,
        compression,
        drop_sets.as_ref().map(|sets| sets.for_kind(ElemKind::Way)),
        &mut writer,
    )?;
    crate::debug::emit_marker("DEGRADE_WAYS_END");
    blobs_written += s.blobs;
    elements_written += s.elements;
    unsort_fired[1] = s.unsort_fired;

    crate::debug::emit_marker("DEGRADE_RELATIONS_START");
    let s = run_kind_phase(
        &shared_file,
        &rel_schedule,
        ElemKind::Relation,
        block_cap,
        flags,
        compression,
        drop_sets
            .as_ref()
            .map(|sets| sets.for_kind(ElemKind::Relation)),
        &mut writer,
    )?;
    crate::debug::emit_marker("DEGRADE_RELATIONS_END");
    blobs_written += s.blobs;
    elements_written += s.elements;
    unsort_fired[2] = s.unsort_fired;

    crate::debug::emit_marker("DEGRADE_FLUSH_START");
    writer.flush()?;
    crate::debug::emit_marker("DEGRADE_FLUSH_END");

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("degrade_unsort_fired_nodes", i64::from(unsort_fired[0]));
        crate::debug::emit_counter("degrade_unsort_fired_ways", i64::from(unsort_fired[1]));
        crate::debug::emit_counter("degrade_unsort_fired_relations", i64::from(unsort_fired[2]));
    }

    Ok(DegradeStats {
        blobs_written,
        elements_written,
        dropped: flags.drop_ids.map_or(0, |spec| spec.n),
        flags,
    })
}

fn select_drop_sets(
    shared_file: &std::sync::Arc<std::fs::File>,
    node_schedule: &[crate::scan::classify::ScheduleEntry],
    way_schedule: &[crate::scan::classify::ScheduleEntry],
    relation_schedule: &[crate::scan::classify::ScheduleEntry],
    spec: DropSpec,
) -> Result<DropSets> {
    let mut total = 0_u64;
    let mut heap = BinaryHeap::new();
    for (kind, schedule) in [
        (ElemKind::Node, node_schedule),
        (ElemKind::Way, way_schedule),
        (ElemKind::Relation, relation_schedule),
    ] {
        crate::scan::classify::parallel_classify_phase(
            shared_file,
            schedule,
            None,
            || (),
            |block, _| select_block_keys(block, kind, spec),
            |_seq, block_drop| {
                total += block_drop.matched;
                for key in block_drop.smallest {
                    keep_smallest(&mut heap, spec.n, key);
                }
            },
        )?;
    }
    if spec.n > total {
        return Err(format!(
            "--drop-ids: cannot drop {} elements, input has only {}",
            spec.n, total,
        )
        .into());
    }
    let mut sets = DropSets::default();
    for key in heap {
        sets.insert(key);
    }
    Ok(sets)
}

fn select_block_keys(block: &crate::PrimitiveBlock, kind: ElemKind, spec: DropSpec) -> BlockDrop {
    // Cross the reproducibility boundary once: everything downstream (the
    // hash input and the DropKey ordering) uses this byte, not the enum.
    let kind_byte = hash_kind(kind);
    let mut matched = 0_u64;
    let mut heap = BinaryHeap::new();
    for element in block.elements() {
        let id = match (&element, kind) {
            (Element::DenseNode(node), ElemKind::Node) => node.id(),
            (Element::Node(node), ElemKind::Node) => node.id(),
            (Element::Way(way), ElemKind::Way) => way.id(),
            (Element::Relation(relation), ElemKind::Relation) => relation.id(),
            _ => continue,
        };
        matched += 1;
        keep_smallest(
            &mut heap,
            spec.n,
            DropKey {
                hash: drop_hash(kind_byte, id, spec.seed),
                kind: kind_byte,
                id,
            },
        );
    }
    BlockDrop {
        matched,
        smallest: heap.into_vec(),
    }
}

struct PhaseStats {
    blobs: u64,
    elements: u64,
    unsort_fired: bool,
}

/// Run one per-kind phase. Workers decode + filter + (when not an unsort
/// mode) pre-frame full cap-multiples; the merge thread writes them in seq
/// order, flushing the central `BlockBuilder` between input blobs to
/// keep IDs ascending. Under either unsort mode workers ship every
/// matching element as `Owned*` so the merge thread can run the
/// adjacent-pair swap. `--unsort` additionally suppresses the boundary
/// flush so the central builder packs to cap and the swap straddles a real
/// output-blob boundary (cross-blob overlap); `--unsort-intra` keeps the
/// flush and swaps the first two same-kind elements, so the inversion
/// stays inside the first output block (intra-blob inversion).
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn run_kind_phase(
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[(usize, u64, usize)],
    kind: ElemKind,
    block_cap: usize,
    flags: DegradeFlags,
    compression: Compression,
    drop: Option<&FxHashSet<i64>>,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<PhaseStats> {
    use crate::reorder_buffer::ReorderBuffer;

    if schedule.is_empty() {
        return Ok(PhaseStats {
            blobs: 0,
            elements: 0,
            unsort_fired: false,
        });
    }

    type PhaseResult = std::result::Result<WorkerOutput, String>;
    let mut reorder: ReorderBuffer<PhaseResult> = ReorderBuffer::with_capacity(32);

    let mut bb = BlockBuilder::with_element_cap(block_cap);
    let mut output: Vec<OwnedBlock> = Vec::new();
    let mut pending: Vec<OwnedBlock> = Vec::with_capacity(FRAME_BATCH);
    let mut unsort = UnsortKindState::new(flags, block_cap);

    let mut blobs: u64 = 0;
    let mut elements: u64 = 0;
    let mut write_error: Option<Box<dyn std::error::Error>> = None;
    let mut classify_error: Option<String> = None;

    crate::scan::classify::parallel_classify_phase(
        shared_file,
        schedule,
        None,
        || (),
        |block, _state| -> PhaseResult {
            worker_decode_kind(block, kind, block_cap, flags, &compression, drop)
        },
        |seq, r| {
            reorder.push(seq, r);
            while let Some(item) = reorder.pop_ready() {
                if write_error.is_some() {
                    continue;
                }
                let out = match item {
                    Ok(out) => out,
                    Err(e) => {
                        classify_error.get_or_insert(e);
                        continue;
                    }
                };

                // Sort preservation: flush central BB before writing this
                // input blob's worker frames. Anything left in central
                // belongs to a strictly lower ID range than the next
                // blob's full frames; flushing now keeps the output
                // monotone. Empty central is a no-op.
                //
                // `--unsort` suppresses this flush so the central builder
                // packs continuously to cap across input blobs and the
                // boundary swap straddles a real output-blob boundary
                // (cross-blob overlap). `--unsort-intra` and every plain
                // decode-path mode keep the flush so output blobs mirror
                // input blobs, which preserves sort order for
                // `--strip-locations`. (`--unsort-intra` stays intra-blob
                // because it swaps the first two same-kind elements, which
                // always land inside the first output block - not because
                // of this flush.)
                if !flags.suppress_boundary_flush() && !bb.is_empty() {
                    if let Err(e) = flush_local(&mut bb, &mut output) {
                        classify_error.get_or_insert(e);
                        continue;
                    }
                    pending.append(&mut output);
                    if !pending.is_empty() {
                        let batch = std::mem::take(&mut pending);
                        match frame_and_write_batch(
                            batch,
                            compression,
                            writer,
                            flags.strip_indexdata,
                            flags.strip_tagdata,
                        ) {
                            Ok(written) => blobs += written,
                            Err(e) => {
                                write_error = Some(e);
                                continue;
                            }
                        }
                    }
                }

                let WorkerOutput { full_framed, tail } = out;
                let full_count = full_framed.len() as u64 * block_cap as u64;
                let tail_n = tail.len() as u64;

                for framed in full_framed {
                    if let Err(e) = writer.write_raw_owned(framed) {
                        write_error = Some(e.into());
                        break;
                    }
                    blobs += 1;
                }
                if write_error.is_some() {
                    continue;
                }

                let consume_res: std::result::Result<(), String> = if flags.unsort_any() {
                    feed_tail_unsort(tail, &mut unsort, &mut bb, &mut output)
                } else {
                    feed_tail_plain(tail, &mut bb, &mut output)
                };
                if let Err(e) = consume_res {
                    classify_error.get_or_insert(e);
                    continue;
                }

                pending.append(&mut output);
                while pending.len() >= FRAME_BATCH {
                    let batch: Vec<OwnedBlock> = pending.drain(..FRAME_BATCH).collect();
                    match frame_and_write_batch(
                        batch,
                        compression,
                        writer,
                        flags.strip_indexdata,
                        flags.strip_tagdata,
                    ) {
                        Ok(written) => blobs += written,
                        Err(e) => {
                            write_error = Some(e);
                            break;
                        }
                    }
                }

                elements += full_count + tail_n;
            }
        },
    )?;

    if let Some(e) = write_error {
        return Err(e);
    }
    if let Some(e) = classify_error {
        return Err(e.into());
    }

    // End-of-phase: re-inject any held element (partial swap fire), then
    // flush the central builder one last time.
    if let Some(elem) = unsort.held.take() {
        write_owned_to_central(&elem, &mut bb, &mut output)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        elements += 1;
    }

    flush_local(&mut bb, &mut output).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    pending.append(&mut output);
    if !pending.is_empty() {
        let final_batch = std::mem::take(&mut pending);
        let written = frame_and_write_batch(
            final_batch,
            compression,
            writer,
            flags.strip_indexdata,
            flags.strip_tagdata,
        )?;
        blobs += written;
    }

    Ok(PhaseStats {
        blobs,
        elements,
        unsort_fired: unsort.fired,
    })
}

/// Worker body: decode + filter to one kind, optionally pre-frame full
/// cap-multiples, ship the trailing partial as owned data. Under either
/// unsort mode everything goes into the tail so the merge thread can
/// apply the cap-1 swap.
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn worker_decode_kind(
    block: &crate::PrimitiveBlock,
    kind: ElemKind,
    cap: usize,
    flags: DegradeFlags,
    compression: &Compression,
    drop: Option<&FxHashSet<i64>>,
) -> std::result::Result<WorkerOutput, String> {
    // `element_matches_kind` already discriminates by `kind`, so the count is
    // identical for every kind - no per-kind match needed.
    let total = block
        .elements()
        .filter(|e| element_matches_kind(e, kind, drop))
        .count();

    // Under either unsort mode the merge thread must see every element in
    // order to apply the cap-1 swap, so workers ship everything as tail.
    let (full_count, tail_size) = if flags.unsort_any() {
        (0usize, total)
    } else {
        let tail = total % cap;
        (total - tail, tail)
    };

    let mut bb = BlockBuilder::with_element_cap(cap);
    let mut output: Vec<OwnedBlock> = Vec::new();
    let mut full_framed: Vec<Vec<u8>> = Vec::new();
    let mut tail: KindPayload = match kind {
        ElemKind::Node => KindPayload::Nodes(Vec::with_capacity(tail_size)),
        ElemKind::Way => KindPayload::Ways(Vec::with_capacity(tail_size)),
        ElemKind::Relation => KindPayload::Relations(Vec::with_capacity(tail_size)),
    };

    let mut idx: usize = 0;
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();
    for element in block.elements() {
        if !element_matches_kind(&element, kind, drop) {
            continue;
        }

        if idx < full_count {
            match (&element, kind) {
                (Element::DenseNode(dn), ElemKind::Node) => {
                    ensure_node_capacity_local(&mut bb, &mut output)?;
                    let meta = dense_node_metadata(dn);
                    bb.add_node(
                        dn.id(),
                        dn.decimicro_lat(),
                        dn.decimicro_lon(),
                        dn.tags(),
                        meta.as_ref(),
                    );
                }
                (Element::Node(n), ElemKind::Node) => {
                    ensure_node_capacity_local(&mut bb, &mut output)?;
                    let meta = element_metadata(&n.info());
                    bb.add_node(
                        n.id(),
                        n.decimicro_lat(),
                        n.decimicro_lon(),
                        n.tags(),
                        meta.as_ref(),
                    );
                }
                (Element::Way(w), ElemKind::Way) => {
                    ensure_way_capacity_local(&mut bb, &mut output)?;
                    refs_buf.clear();
                    refs_buf.extend(w.refs());
                    let meta = element_metadata(&w.info());
                    // add_way drops inline LOW coords - the documented
                    // behaviour for the decode path regardless of
                    // --strip-locations.
                    bb.add_way(w.id(), w.tags(), &refs_buf, meta.as_ref());
                }
                (Element::Relation(r), ElemKind::Relation) => {
                    ensure_relation_capacity_local(&mut bb, &mut output)?;
                    members_buf.clear();
                    members_buf.extend(r.members().map(|m| MemberData {
                        id: m.id,
                        role: m.role().unwrap_or(""),
                    }));
                    let meta = element_metadata(&r.info());
                    bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
                }
                _ => {}
            }
            for owned_block in output.drain(..) {
                full_framed.push(frame_owned(
                    owned_block,
                    compression,
                    flags.strip_indexdata,
                    flags.strip_tagdata,
                )?);
            }
        } else {
            match (&element, &mut tail) {
                (Element::DenseNode(dn), KindPayload::Nodes(v)) => v.push(read_dense_node(dn)),
                (Element::Node(n), KindPayload::Nodes(v)) => v.push(read_node(n)),
                (Element::Way(w), KindPayload::Ways(v)) => v.push(read_way(w)),
                (Element::Relation(r), KindPayload::Relations(v)) => v.push(read_relation(r)),
                _ => {}
            }
        }
        idx += 1;
    }

    // Flush any final full block from the worker BB.
    flush_local(&mut bb, &mut output)?;
    for owned_block in output.drain(..) {
        full_framed.push(frame_owned(
            owned_block,
            compression,
            flags.strip_indexdata,
            flags.strip_tagdata,
        )?);
    }

    Ok(WorkerOutput { full_framed, tail })
}

fn element_matches_kind(
    element: &Element<'_>,
    kind: ElemKind,
    drop: Option<&FxHashSet<i64>>,
) -> bool {
    let id = match (element, kind) {
        (Element::DenseNode(node), ElemKind::Node) => node.id(),
        (Element::Node(node), ElemKind::Node) => node.id(),
        (Element::Way(way), ElemKind::Way) => way.id(),
        (Element::Relation(relation), ElemKind::Relation) => relation.id(),
        _ => return false,
    };
    drop.is_none_or(|ids| !ids.contains(&id))
}

/// Feed an entire unsort-mode tail into the central builder, applying the
/// per-kind adjacent-pair swap. Elements are moved out of the tail (no
/// clones). Mid-stream cap fires append blocks to `output`. Shared by
/// `--unsort` and `--unsort-intra`; the two differ only in `hold_at` (the
/// arrival position of the held element) and in whether the merge loop
/// flushes the central builder at input-blob boundaries.
fn feed_tail_unsort(
    tail: KindPayload,
    unsort: &mut UnsortKindState,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> std::result::Result<(), String> {
    match tail {
        KindPayload::Nodes(v) => {
            for n in v {
                if unsort.should_hold() {
                    unsort.held = Some(OwnedElement::Node(n));
                    unsort.seen += 1;
                } else if unsort.should_inject_after() {
                    crate::owned::write_single_node_local(&n, bb, output)?;
                    let held = unsort.held.take().expect("checked");
                    write_owned_to_central(&held, bb, output)?;
                    unsort.fired = true;
                    unsort.seen += 1;
                } else {
                    crate::owned::write_single_node_local(&n, bb, output)?;
                    unsort.seen += 1;
                }
            }
        }
        KindPayload::Ways(v) => {
            for w in v {
                if unsort.should_hold() {
                    unsort.held = Some(OwnedElement::Way(w));
                    unsort.seen += 1;
                } else if unsort.should_inject_after() {
                    crate::owned::write_single_way_local(&w, bb, output)?;
                    let held = unsort.held.take().expect("checked");
                    write_owned_to_central(&held, bb, output)?;
                    unsort.fired = true;
                    unsort.seen += 1;
                } else {
                    crate::owned::write_single_way_local(&w, bb, output)?;
                    unsort.seen += 1;
                }
            }
        }
        KindPayload::Relations(v) => {
            for r in v {
                if unsort.should_hold() {
                    unsort.held = Some(OwnedElement::Relation(r));
                    unsort.seen += 1;
                } else if unsort.should_inject_after() {
                    crate::owned::write_single_relation_local(&r, bb, output)?;
                    let held = unsort.held.take().expect("checked");
                    write_owned_to_central(&held, bb, output)?;
                    unsort.fired = true;
                    unsort.seen += 1;
                } else {
                    crate::owned::write_single_relation_local(&r, bb, output)?;
                    unsort.seen += 1;
                }
            }
        }
    }
    Ok(())
}

/// Feed a non-unsort tail straight into the central builder.
fn feed_tail_plain(
    tail: KindPayload,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> std::result::Result<(), String> {
    match tail {
        KindPayload::Nodes(v) => {
            for n in &v {
                crate::owned::write_single_node_local(n, bb, output)?;
            }
        }
        KindPayload::Ways(v) => {
            for w in &v {
                crate::owned::write_single_way_local(w, bb, output)?;
            }
        }
        KindPayload::Relations(v) => {
            for r in &v {
                crate::owned::write_single_relation_local(r, bb, output)?;
            }
        }
    }
    Ok(())
}

fn write_owned_to_central(
    element: &OwnedElement,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> std::result::Result<(), String> {
    match element {
        OwnedElement::Node(n) => crate::owned::write_single_node_local(n, bb, output),
        OwnedElement::Way(w) => crate::owned::write_single_way_local(w, bb, output),
        OwnedElement::Relation(r) => crate::owned::write_single_relation_local(r, bb, output),
    }
}

fn frame_owned(
    owned: OwnedBlock,
    compression: &Compression,
    strip_indexdata: bool,
    strip_tagdata: bool,
) -> std::result::Result<Vec<u8>, String> {
    let OwnedBlock {
        bytes: block_bytes,
        index,
        tagdata,
        way_members,
    } = owned;
    let indexdata_buf = index.serialize();
    let indexdata = if strip_indexdata {
        None
    } else {
        Some(indexdata_buf.as_slice())
    };
    let tagdata = if strip_tagdata {
        None
    } else {
        tagdata.as_deref()
    };
    let blob = frame_blob_pipelined(
        &block_bytes,
        compression,
        indexdata,
        tagdata,
        way_members.as_deref(),
    )
    .map_err(|e| e.to_string())?;
    Ok(blob.into_vec())
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn frame_and_write_batch(
    batch: Vec<OwnedBlock>,
    compression: Compression,
    writer: &mut PbfWriter<FileWriter>,
    strip_indexdata: bool,
    strip_tagdata: bool,
) -> std::result::Result<u64, Box<dyn std::error::Error>> {
    use rayon::prelude::*;

    let framed: Vec<std::io::Result<Vec<u8>>> = batch
        .into_par_iter()
        .map(
            |OwnedBlock {
                 bytes: block_bytes,
                 index,
                 tagdata,
                 way_members,
             }|
             -> std::io::Result<Vec<u8>> {
                let indexdata_buf = index.serialize();
                let indexdata = if strip_indexdata {
                    None
                } else {
                    Some(indexdata_buf.as_slice())
                };
                let tagdata = if strip_tagdata {
                    None
                } else {
                    tagdata.as_deref()
                };
                let blob = frame_blob_pipelined(
                    &block_bytes,
                    &compression,
                    indexdata,
                    tagdata,
                    way_members.as_deref(),
                )?;
                Ok(blob.into_vec())
            },
        )
        .collect();

    let mut written: u64 = 0;
    for r in framed {
        let bytes = r?;
        writer.write_raw_owned(bytes)?;
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::{DropKey, drop_hash, hash_kind, keep_smallest, mix64};
    use crate::blob_meta::ElemKind;
    use std::collections::BinaryHeap;

    fn select(keys: impl IntoIterator<Item = DropKey>, n: u64) -> Vec<DropKey> {
        let mut heap = BinaryHeap::new();
        for key in keys {
            keep_smallest(&mut heap, n, key);
        }
        let mut out = heap.into_vec();
        out.sort();
        out
    }

    fn keys() -> Vec<DropKey> {
        vec![
            DropKey {
                hash: 9,
                kind: 2,
                id: 1,
            },
            DropKey {
                hash: 2,
                kind: 1,
                id: 4,
            },
            DropKey {
                hash: 2,
                kind: 0,
                id: 9,
            },
            DropKey {
                hash: 7,
                kind: 0,
                id: 2,
            },
            DropKey {
                hash: 1,
                kind: 2,
                id: 8,
            },
            DropKey {
                hash: 5,
                kind: 1,
                id: 3,
            },
        ]
    }

    /// Pins the `ElemKind` -> byte bridge itself. The golden vectors below
    /// are expressed as bare literals (0/1/2) for historical/readability
    /// reasons, but this assertion is what actually guarantees those
    /// literals still match what `hash_kind` produces - an accidental
    /// reordering of the `hash_kind` match arms would fail here even
    /// though the bare-literal vectors alone could not detect it.
    #[test]
    fn hash_kind_matches_pinned_bytes() {
        assert_eq!(hash_kind(ElemKind::Node), 0);
        assert_eq!(hash_kind(ElemKind::Way), 1);
        assert_eq!(hash_kind(ElemKind::Relation), 2);
    }

    #[test]
    fn drop_hash_golden_vectors() {
        assert_eq!(mix64(0), 0);
        assert_eq!(mix64(1), 0x5692_161d_100b_05e5);
        assert_eq!(mix64(u64::MAX), 0xb4d0_55fc_f2cb_bd7b);
        assert_eq!(drop_hash(0, 1, 0), 0xe220_a839_7b1d_cdaf);
        assert_eq!(drop_hash(1, 1, 0), 0xd28f_0491_68bd_d34c);
        assert_eq!(drop_hash(2, 42, 0), 0x454c_0046_9e53_63e2);
        assert_eq!(drop_hash(0, 1, 1), 0xe4d9_7177_1b65_2c20);
        assert_eq!(drop_hash(0, 1, 0x1_0000_0000), 0x219f_c13d_6bc5_b015);
        assert_eq!(
            drop_hash(2, 42, 0xdead_beef_cafe_babe),
            0xdd1c_b91c_cef4_8036
        );

        // Same golden vectors, but routed through `hash_kind` so this test
        // fails if the ElemKind-to-byte bridge ever drifts from the
        // reproducibility contract, not just if `drop_hash` itself changes.
        assert_eq!(
            drop_hash(hash_kind(ElemKind::Node), 1, 0),
            0xe220_a839_7b1d_cdaf
        );
        assert_eq!(
            drop_hash(hash_kind(ElemKind::Way), 1, 0),
            0xd28f_0491_68bd_d34c
        );
        assert_eq!(
            drop_hash(hash_kind(ElemKind::Relation), 42, 0),
            0x454c_0046_9e53_63e2
        );
    }

    #[test]
    fn drop_selection_matches_full_sort() {
        let all = keys();
        for n in [3, all.len() as u64, all.len() as u64 + 2] {
            let mut expected = all.clone();
            expected.sort();
            expected.truncate(usize::try_from(n).unwrap_or(usize::MAX));
            assert_eq!(select(all.clone(), n), expected);
        }
    }

    #[test]
    fn drop_selection_permutation_invariant() {
        let all = keys();
        let expected = select(all.clone(), 4);
        let mut reversed = all;
        reversed.reverse();
        assert_eq!(select(reversed, 4), expected);
    }

    #[test]
    fn drop_selection_partition_invariant() {
        let all = keys();
        let expected = select(all.clone(), 4);
        let mut global = BinaryHeap::new();
        for chunk in all.chunks(2) {
            let mut local = BinaryHeap::new();
            for &key in chunk {
                keep_smallest(&mut local, 4, key);
            }
            for key in local {
                keep_smallest(&mut global, 4, key);
            }
        }
        let mut actual = global.into_vec();
        actual.sort();
        assert_eq!(actual, expected);
    }

    #[test]
    fn drop_key_orders_by_hash_then_kind_then_id() {
        let keys = vec![
            DropKey {
                hash: 1,
                kind: 1,
                id: 1,
            },
            DropKey {
                hash: 1,
                kind: 0,
                id: 9,
            },
            DropKey {
                hash: 1,
                kind: 0,
                id: 2,
            },
        ];
        assert_eq!(
            select(keys, 3),
            vec![
                DropKey {
                    hash: 1,
                    kind: 0,
                    id: 2
                },
                DropKey {
                    hash: 1,
                    kind: 0,
                    id: 9
                },
                DropKey {
                    hash: 1,
                    kind: 1,
                    id: 1
                },
            ]
        );
    }
}
