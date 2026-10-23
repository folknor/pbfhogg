//! Produce a valid-but-adversarial PBF by stripping properties or
//! perturbing structure. A "make our lives difficult" tool for exercising
//! code paths that require less-optimised inputs (unsorted, missing
//! indexdata, scattered coords).
//!
//! The transformation flags are defined by [`DegradeFlags`].
//! Flags compose (except the two unsort modes, which are mutually
//! exclusive).
//!
//! Implementation paths:
//!
//! - Pure passthrough (`--strip-indexdata`, `--strip-tagdata`, and/or
//!   `--strip-bbox` with no unsort/strip-locations/drop-ids): the input
//!   `HeaderBlock` payload is preserved verbatim (the outer Blob envelope is
//!   re-compressed, so the decompressed `HeaderBlock` protobuf is
//!   field-identical, not blob-byte-identical) - it is forwarded
//!   field-for-field, only the bbox (field 1) is surgically removed under
//!   `--strip-bbox`, and a `--generator` / `--output-header` override
//!   replaces only the field it names. `source`, custom optional features
//!   (`pbfhogg.WayMembers-v1`, `SharedNodePins-v1`), a non-default
//!   `writingprogram`, replication metadata, and unknown fields all survive
//!   byte-for-byte. Raw OsmData blob frames are then reframed by copying
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

use super::output_header::{Bbox, Order, OutputFacts, Ways};
use crate::read::wire_schema::BlobHeaderField;
use std::collections::BinaryHeap;
use std::path::Path;

use rustc_hash::FxHashSet;

use super::{
    HeaderOverrides, Result, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, flush_local, require_indexdata, writer_from_header_bytes,
};
use crate::ElementKind;
use crate::blob::BlobKind;
/// Compatibility alias for [`crate::block_builder::DEFAULT_ELEMENT_CAP`].
pub use crate::block_builder::DEFAULT_ELEMENT_CAP as DEFAULT_BLOCK_CAP;
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::error::Error;
use crate::file_reader::FileReader;
use crate::file_writer::FileWriter;
use crate::owned::{
    OwnedElement, OwnedNode, OwnedRelation, OwnedWay, dense_node_metadata, element_metadata,
    read_dense_node, read_node, read_relation, read_way,
};
use crate::read::raw_frame::{frame_location, read_raw_frame};
use crate::write::WriteOptions;
use crate::writer::{Compression, PbfWriter, frame_blob_pipelined, strip_blob_header_fields};
use crate::{Element, ElementReader};

use crate::tunables::{CORRUPT_INDEXDATA_INFLATION, FRAME_BATCH};

/// Reproducibility contract: map an `ElementKind` to the byte that feeds
/// `--drop-ids` selection. The exact values (Node=0, Way=1, Relation=2)
/// are load-bearing - `drop_hash` mixes this byte numerically into the
/// splitmix64 finalizer and `DropKey` orders on it, so the same
/// `--drop-ids N:SEED` must drop the byte-identical element set across
/// builds. `ElementKind` carries no `repr(u8)` and no `Ord`, so this
/// explicit match is the only sanctioned bridge from the kind enum into
/// the numeric selection domain; never route around it. The golden
/// vectors in this module's tests pin these values.
fn hash_kind(kind: ElementKind) -> u8 {
    match kind {
        ElementKind::Node => 0,
        ElementKind::Way => 1,
        ElementKind::Relation => 2,
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
        let (n_str, seed_str) = s.split_once(':').ok_or_else(|| {
            Error::invalid_argument(
                "--drop-ids expects N:SEED (e.g. 5000:42); the ':' separator is required",
            )
        })?;
        let n = n_str.trim().parse().map_err(|_| {
            Error::invalid_argument(format!(
                "--drop-ids: N must be a non-negative integer, got {n_str:?}"
            ))
        })?;
        let seed = seed_str.trim().parse().map_err(|_| {
            Error::invalid_argument(format!(
                "--drop-ids: SEED must be a non-negative integer, got {seed_str:?}"
            ))
        })?;
        if n == 0 {
            return Err(Error::invalid_argument(
                "--drop-ids: N must be >= 1 (dropping zero elements is a no-op)",
            ));
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
    fn for_kind(&self, kind: ElementKind) -> &FxHashSet<i64> {
        match kind {
            ElementKind::Node => &self.nodes,
            ElementKind::Way => &self.ways,
            ElementKind::Relation => &self.relations,
        }
    }

    fn insert(&mut self, key: DropKey) {
        // `key.kind` is the `hash_kind()` selection byte (the domain
        // `DropKey` orders on; `ElementKind` has no `Ord`). Route to the
        // matching set through the same boundary so the byte mapping lives
        // in exactly one place.
        let set = if key.kind == hash_kind(ElementKind::Node) {
            &mut self.nodes
        } else if key.kind == hash_kind(ElementKind::Way) {
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
    /// Replace every blob's `BlobHeader.tagdata` (field 4) with a *valid but
    /// wrong* tag key index: a well-formed index naming one key no OSM file
    /// uses, so it parses cleanly and claims the blob carries none of the keys
    /// it actually carries.
    ///
    /// This is the adversarial case `--strip-tagdata` cannot produce.
    /// Stripping removes the hint, and a blob with no hint is decoded rather
    /// than skipped, so a stripped file only exercises the slow path and can
    /// never produce a wrong answer. A *lying* hint makes `tags-filter` skip
    /// blobs whose elements match, emit fewer elements than it should, and
    /// report a cheerful "N blobs skipped by tag index" while doing it. That
    /// is the failure mode blob-level tag-index skipping risks, and until this
    /// flag existed there was no way to write a test that produced it.
    ///
    /// Header-only, so it composes with the passthrough path. Mutually
    /// exclusive with `--strip-tagdata`: both target field 4.
    pub corrupt_tagdata: bool,
    /// Replace every blob's `BlobHeader.indexdata` (field 2) with a *valid but
    /// wrong* index: same element kind, inflated `count`, widened `max_id`.
    ///
    /// The counterpart of `corrupt_tagdata` for the other sidecar hint, and
    /// the fixture `inspect --distrust-metadata` exists for. `inspect`
    /// answers counts and ID ranges straight out of indexdata whenever every
    /// blob has some, so on a file with a lying index it reports the lie as a
    /// measurement. The kind is preserved deliberately: mislabelling it would
    /// derail the per-kind classify schedules everywhere and obscure the one
    /// property under test.
    ///
    /// Header-only, and mutually exclusive with `--strip-indexdata`.
    pub corrupt_indexdata: bool,
    /// Rotate every blob's `BlobHeader.indexdata` element *kind* - node ->
    /// way -> relation -> node - leaving count, ID range and bbox honest.
    ///
    /// The kind is the field the per-kind classify schedules select blobs on
    /// (`build_classify_schedule`), so mislabelling it is the corruption that
    /// makes a consumer skip a blob outright rather than merely misreport it.
    /// `--corrupt-indexdata` deliberately preserves the kind so its own
    /// fixture stays readable; this flag is the separate mode for the
    /// blob-selection question, and it is what makes a
    /// `tags-filter --distrust-metadata` test bite through reference
    /// expansion rather than only through the direct-match pass.
    ///
    /// Header-only, mutually exclusive with `--strip-indexdata`, and
    /// combinable with `--corrupt-indexdata`.
    pub corrupt_indexdata_kind: bool,
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
    fn flag_states(self) -> [(bool, &'static str, bool); 10] {
        [
            (self.unsort, "--unsort", true),
            (self.unsort_intra, "--unsort-intra", true),
            (self.strip_locations, "--strip-locations", true),
            (self.strip_indexdata, "--strip-indexdata", false),
            (self.strip_tagdata, "--strip-tagdata", false),
            (self.corrupt_tagdata, "--corrupt-tagdata", false),
            (self.corrupt_indexdata, "--corrupt-indexdata", false),
            (
                self.corrupt_indexdata_kind,
                "--corrupt-indexdata-kind",
                false,
            ),
            (self.strip_bbox, "--strip-bbox", false),
            (self.drop_ids.is_some(), "--drop-ids", true),
        ]
    }

    fn flag_names(self) -> [&'static str; 10] {
        self.flag_states().map(|(_, name, _)| name)
    }

    pub fn any(self) -> bool {
        self.flag_states()
            .into_iter()
            .any(|(enabled, _, _)| enabled)
    }

    /// Returns `true` if elements must be decoded and re-encoded. Only the
    /// header-only strips (`--strip-indexdata` / `--strip-tagdata` /
    /// `--strip-bbox`, alone or together) can run as a pure blob-level
    /// passthrough.
    fn needs_decode(self) -> bool {
        self.flag_states()
            .into_iter()
            .any(|(enabled, _, needs_decode)| enabled && needs_decode)
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
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        let applied: Vec<&str> = self
            .flags
            .flag_states()
            .into_iter()
            .filter_map(|(enabled, name, _)| if enabled { Some(name) } else { None })
            .collect();
        let dropped = if self.dropped > 0 {
            format!(" (dropped {} elements)", self.dropped)
        } else {
            String::new()
        };
        writeln!(
            w,
            "Degraded {} elements across {} blobs{} (applied: {})",
            self.elements_written,
            self.blobs_written,
            dropped,
            applied.join(" "),
        )?;

        Ok(())
    }
}

/// Apply the requested degradations to `input` and emit `output`.
///
/// `block_cap` is the per-block element cap used by the decode path's
/// `BlockBuilder`. Production callers pass [`DEFAULT_BLOCK_CAP`]; tests pass
/// a smaller value so the `--unsort` swap can be exercised on fixtures of
/// modest size.
///
/// `force` skips the indexdata precondition required by the decode path's
/// per-kind classify pipeline. Has no effect on the passthrough path.
pub fn degrade(
    input: &Path,
    output: crate::output::Output,
    flags: DegradeFlags,
    block_cap: usize,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<DegradeStats> {
    let result = degrade_to(input, &output, flags, block_cap, write, overrides, host)?;
    output.publish()?;
    Ok(result)
}

/// [`degrade`] writing to a borrowed output it does not publish.
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn degrade_to(
    input: &Path,
    output: &crate::output::Output,
    flags: DegradeFlags,
    block_cap: usize,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<DegradeStats> {
    if !flags.any() {
        let flag_names = flags.flag_names().join(", ");
        return Err(Error::invalid_argument(format!(
            "degrade requires at least one transformation flag ({flag_names})"
        )));
    }
    // The decode path rebuilds each blob's hints from the elements it just
    // re-encoded, so a corrupt hint written there would be overwritten by a
    // correct one. Refuse rather than accept a flag that would do nothing.
    if (flags.corrupt_tagdata || flags.corrupt_indexdata || flags.corrupt_indexdata_kind)
        && flags.needs_decode()
    {
        return Err(Error::invalid_argument(
            "--corrupt-tagdata, --corrupt-indexdata and --corrupt-indexdata-kind are \
             header-only rewrites and cannot be combined with a transformation \
             that re-encodes blobs \
             (--unsort, --unsort-intra, --strip-locations, --drop-ids), which \
             would rebuild the hint correctly",
        ));
    }
    if flags.strip_tagdata && flags.corrupt_tagdata {
        return Err(Error::invalid_argument(
            "--strip-tagdata and --corrupt-tagdata are mutually exclusive: \
             both rewrite BlobHeader field 4, one to nothing and one to a \
             deliberately wrong index",
        ));
    }
    if flags.strip_indexdata && (flags.corrupt_indexdata || flags.corrupt_indexdata_kind) {
        return Err(Error::invalid_argument(
            "--strip-indexdata is mutually exclusive with --corrupt-indexdata and \
             --corrupt-indexdata-kind: all three rewrite BlobHeader field 2, \
             one to nothing and the others to a deliberately wrong index",
        ));
    }
    if flags.unsort && flags.unsort_intra {
        return Err(Error::invalid_argument(
            "--unsort and --unsort-intra are mutually exclusive: --unsort \
             produces cross-blob ID-range overlap, --unsort-intra produces \
             an intra-blob inversion",
        ));
    }
    if block_cap == 0 {
        return Err(Error::invalid_argument("--block-cap must be > 0"));
    }
    // An intra-blob inversion needs two same-kind elements sitting inside
    // one output block. A cap of 1 puts every element in its own block, so
    // the requested shape is impossible - reject it rather than silently
    // clearing Sort.Type_then_ID and emitting an untouched (still-monotone)
    // stream. `--unsort` (cross-blob overlap) IS achievable at cap 1: two
    // adjacent single-element blobs with a descending step overlap, so it
    // stays supported.
    if flags.unsort_intra && block_cap < 2 {
        return Err(Error::invalid_argument(
            "--unsort-intra needs --block-cap >= 2: an intra-blob \
             inversion requires at least two same-kind elements inside \
             one output block, which a cap of 1 cannot hold",
        ));
    }

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("degrade_unsort", i64::from(flags.unsort));
        crate::debug::emit_counter("degrade_unsort_intra", i64::from(flags.unsort_intra));
        crate::debug::emit_counter("degrade_strip_locations", i64::from(flags.strip_locations));
        crate::debug::emit_counter("degrade_strip_indexdata", i64::from(flags.strip_indexdata));
        crate::debug::emit_counter("degrade_strip_tagdata", i64::from(flags.strip_tagdata));
        crate::debug::emit_counter("degrade_corrupt_tagdata", i64::from(flags.corrupt_tagdata));
        crate::debug::emit_counter(
            "degrade_corrupt_indexdata",
            i64::from(flags.corrupt_indexdata),
        );
        crate::debug::emit_counter(
            "degrade_corrupt_indexdata_kind",
            i64::from(flags.corrupt_indexdata_kind),
        );
        crate::debug::emit_counter("degrade_strip_bbox", i64::from(flags.strip_bbox));
        crate::debug::emit_counter("degrade_drop_ids", i64::from(flags.drop_ids.is_some()));
        if let Some(spec) = flags.drop_ids {
            crate::debug::emit_counter("degrade_drop_n", spec.n as i64);
            crate::debug::emit_counter("degrade_drop_seed", spec.seed as i64);
        }
        crate::debug::emit_counter("degrade_block_cap", block_cap as i64);
    }

    let stats = if flags.needs_decode() {
        degrade_decode_path(input, output, flags, block_cap, write, overrides, host)?
    } else {
        degrade_passthrough(input, output, flags, write, overrides, host)?
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
/// The passthrough copies every blob payload and every `BlobHeader` field but
/// the targeted hint, so the input header still describes the output: it is
/// forwarded field for field by
/// [`output_header::verbatim`](super::output_header::verbatim). `source`,
/// every optional feature (`Sort.Type_then_ID`, `LocationsOnWays`,
/// `pbfhogg.WayMembers-v1`, `pbfhogg.SharedNodePins-v1`, custom extension
/// features), the writing program, the replication metadata and unknown
/// fields survive byte for byte. `--strip-bbox` removes the bbox, and each
/// `--generator` / `--output-header` override replaces only the field it
/// names, so adding `--generator` no longer changes the declared features.
fn passthrough_header_bytes(
    input: &Path,
    flags: DegradeFlags,
    overrides: &HeaderOverrides,
    direct_io: bool,
    host: &crate::host::Host,
) -> Result<Vec<u8>> {
    // The OSMHeader is always the first blob in a well-formed PBF.
    let mut reader = FileReader::open(input, direct_io)?;
    let mut file_offset: u64 = 0;
    let frame = read_raw_frame(&mut reader, &mut file_offset, &host.diagnostics, input)?
        .ok_or_else(|| crate::error::new_missing_header(input))?;
    let locate = |e: Error| e.locate(frame_location(input, frame.file_offset));
    if !matches!(frame.blob_type, BlobKind::OsmHeader) {
        return Err(crate::error::new_missing_header(input));
    }
    let mut header_block = Vec::new();
    crate::read::decompress::decompress_blob_data_into(frame.blob_bytes(), &mut header_block)
        .map_err(locate)?;
    super::output_header::verbatim(&header_block, flags.strip_bbox, overrides).map_err(locate)
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn degrade_passthrough(
    input: &Path,
    output: &crate::output::Output,
    flags: DegradeFlags,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<DegradeStats> {
    let direct_io = write.direct_reads;
    let header_bytes = passthrough_header_bytes(input, flags, overrides, direct_io, host)?;

    let mut writer = writer_from_header_bytes(
        output,
        write.compression,
        &header_bytes,
        write.backend,
        host,
    )?;

    let mut reader = FileReader::open(input, direct_io)?;
    let mut file_offset: u64 = 0;
    let mut blobs_written: u64 = 0;

    crate::debug::emit_marker("DEGRADE_PASSTHROUGH_START");
    while let Some(frame) = read_raw_frame(&mut reader, &mut file_offset, &host.diagnostics, input)?
    {
        match &frame.blob_type {
            BlobKind::OsmHeader => {}
            BlobKind::OsmData => {
                // Preserve every original BlobHeader field verbatim and clear
                // only the field(s) the active strip flags target. Working
                // from the original header bytes (not parsed values) keeps the
                // untouched indexdata byte-identical - a v1 index stays v1 -
                // and carries WayMembers-v1 and any unknown header fields
                // through unchanged.
                let corrupt_index = corrupted_index(frame.index, flags);
                let reframed = reframe_raw(
                    frame.header_bytes(),
                    frame.blob_bytes(),
                    flags.strip_indexdata || corrupt_index.is_some(),
                    flags.strip_tagdata || flags.corrupt_tagdata,
                    flags.corrupt_tagdata,
                    corrupt_index,
                )
                .map_err(|e| e.locate(frame_location(input, frame.file_offset)))?;
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

/// The key a `--corrupt-tagdata` index claims the blob carries. Chosen so no
/// real OSM tag can collide with it: any expression naming it would have to be
/// written on purpose.
const CORRUPT_TAGDATA_KEY: &[u8] = b"pbfhogg:degrade:no-such-key";

/// The lying `BlobIndex` to write for one blob, or `None` when no
/// index-corrupting flag is active.
///
/// Both corruptions go through `BlobIndex::serialize`, the same encoder the
/// honest path uses, so what lands on the wire is a structurally valid v2
/// index that deserializes to exactly the values set here. That is the whole
/// point: a malformed index would be rejected by the reader's own validation
/// and every test built on it would be proving that validation works rather
/// than that the consumer stops believing a plausible lie.
fn corrupted_index(
    base: Option<crate::blob_meta::BlobIndex>,
    flags: DegradeFlags,
) -> Option<crate::blob_meta::BlobIndex> {
    if !flags.corrupt_indexdata && !flags.corrupt_indexdata_kind {
        return None;
    }
    let mut index = base?;
    if flags.corrupt_indexdata {
        index.count = index
            .count
            .saturating_add(CORRUPT_INDEXDATA_INFLATION.cast_unsigned());
        index.max_id = index.max_id.saturating_add(CORRUPT_INDEXDATA_INFLATION);
    }
    if flags.corrupt_indexdata_kind {
        index.kind = match index.kind {
            crate::ElementKind::Node => crate::ElementKind::Way,
            crate::ElementKind::Way => crate::ElementKind::Relation,
            crate::ElementKind::Relation => crate::ElementKind::Node,
        };
    }
    Some(index)
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
    corrupt_tagdata: bool,
    corrupt_index: Option<crate::blob_meta::BlobIndex>,
) -> Result<Vec<u8>> {
    let mut strip_fields: Vec<BlobHeaderField> = Vec::with_capacity(2);
    if strip_indexdata {
        strip_fields.push(BlobHeaderField::Indexdata);
    }
    if strip_tagdata {
        strip_fields.push(BlobHeaderField::Tagdata);
    }
    let mut header_buf = Vec::new();
    strip_blob_header_fields(header_bytes, &strip_fields, &mut header_buf)?;
    if let Some(index) = corrupt_index {
        // Field 2 was just removed above; re-emit the lying index built by
        // `corrupted_index`.
        protohoggr::encode_bytes_field(&mut header_buf, 2, &index.serialize());
    }
    if corrupt_tagdata {
        // Field 4 was just removed above; re-emit it carrying an index that
        // parses cleanly and describes a blob other than this one.
        let bogus = crate::blob_meta::encode_tag_index(std::iter::once(CORRUPT_TAGDATA_KEY))
            .ok_or_else(|| Error::internal("could not encode the corrupt tag index"))?;
        protohoggr::encode_bytes_field(&mut header_buf, 4, &bogus);
    }
    let header_len = u32::try_from(header_buf.len())
        .map_err(|_| Error::internal(format!("header too large: {} bytes", header_buf.len())))?;
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
        self.held.is_some() && self.seen == self.hold_at
    }
}

#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn degrade_decode_path(
    input: &Path,
    output: &crate::output::Output,
    flags: DegradeFlags,
    block_cap: usize,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<DegradeStats> {
    let WriteOptions {
        compression,
        backend,
        direct_reads: direct_io,
        force,
    } = *write;
    require_indexdata(
        input,
        direct_io,
        force,
        "input PBF has no blob-level indexdata. degrade's decode path uses the \
         parallel per-kind classify pipeline, which needs indexdata to build \
         per-kind blob schedules.",
        host,
    )?;

    // Per-blob worker pool; same precedent as `cat --clean` and `repack`.
    host.process.cross_thread_alloc_pool();

    let header = ElementReader::open_in(input, direct_io, host)?
        .header()
        .clone();

    let (node_schedule, way_schedule, rel_schedule, shared_file, _max_ids) =
        crate::scan::classify::build_classify_schedules_split(input, &host.diagnostics)?;
    // Use the input size as a preallocation budget for the caller-supplied
    // cap. Compressed input can expand to more elements than bytes; the
    // vectors grow as needed without changing the requested block cap.
    let prealloc_bound = usize::try_from(shared_file.metadata()?.len()).unwrap_or(usize::MAX);

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
            host,
        )?;
        crate::debug::emit_marker("DEGRADE_DROP_SELECT_END");
        Some(sets)
    } else {
        None
    };

    // Every way is re-encoded without inline coordinates or prepass fields;
    // under --strip-locations that is the request, not a loss to warn about.
    let facts = OutputFacts {
        order: if flags.unsort_any() {
            Order::Unsorted
        } else {
            Order::AsInput
        },
        bbox: if flags.strip_bbox {
            Bbox::Drop
        } else {
            Bbox::Carry
        },
        ..OutputFacts::carry(if flags.strip_locations {
            Ways::Stripped
        } else {
            Ways::Reencoded
        })
    };
    let header_bytes = super::output_header::build_one(&header, &facts, overrides, host)?.bytes;
    let mut writer = writer_from_header_bytes(output, compression, &header_bytes, backend, host)?;

    let mut blobs_written: u64 = 0;
    let mut elements_written: u64 = 0;
    let mut unsort_fired = [false; 3];

    crate::debug::emit_marker("DEGRADE_NODES_START");
    let s = run_kind_phase(
        &shared_file,
        &node_schedule,
        ElementKind::Node,
        block_cap,
        prealloc_bound,
        flags,
        compression,
        drop_sets
            .as_ref()
            .map(|sets| sets.for_kind(ElementKind::Node)),
        &mut writer,
        host,
    )?;
    crate::debug::emit_marker("DEGRADE_NODES_END");
    blobs_written += s.blobs;
    elements_written += s.elements;
    unsort_fired[0] = s.unsort_fired;

    crate::debug::emit_marker("DEGRADE_WAYS_START");
    let s = run_kind_phase(
        &shared_file,
        &way_schedule,
        ElementKind::Way,
        block_cap,
        prealloc_bound,
        flags,
        compression,
        drop_sets
            .as_ref()
            .map(|sets| sets.for_kind(ElementKind::Way)),
        &mut writer,
        host,
    )?;
    crate::debug::emit_marker("DEGRADE_WAYS_END");
    blobs_written += s.blobs;
    elements_written += s.elements;
    unsort_fired[1] = s.unsort_fired;

    crate::debug::emit_marker("DEGRADE_RELATIONS_START");
    let s = run_kind_phase(
        &shared_file,
        &rel_schedule,
        ElementKind::Relation,
        block_cap,
        prealloc_bound,
        flags,
        compression,
        drop_sets
            .as_ref()
            .map(|sets| sets.for_kind(ElementKind::Relation)),
        &mut writer,
        host,
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
    host: &crate::host::Host,
) -> Result<DropSets> {
    let mut total = 0_u64;
    let mut heap = BinaryHeap::new();
    for (kind, schedule) in [
        (ElementKind::Node, node_schedule),
        (ElementKind::Way, way_schedule),
        (ElementKind::Relation, relation_schedule),
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
            &*host.resources,
        )?;
    }
    if spec.n > total {
        return Err(Error::invalid_argument(format!(
            "--drop-ids: cannot drop {} elements, input has only {}",
            spec.n, total,
        )));
    }
    let mut sets = DropSets::default();
    for key in heap {
        sets.insert(key);
    }
    Ok(sets)
}

fn select_block_keys(
    block: &crate::PrimitiveBlock,
    kind: ElementKind,
    spec: DropSpec,
) -> BlockDrop {
    // Cross the reproducibility boundary once: everything downstream (the
    // hash input and the DropKey ordering) uses this byte, not the enum.
    let kind_byte = hash_kind(kind);
    let mut matched = 0_u64;
    let mut heap = BinaryHeap::new();
    for element in block.elements() {
        let id = match (&element, kind) {
            (Element::DenseNode(node), ElementKind::Node) => node.id(),
            (Element::Node(node), ElementKind::Node) => node.id(),
            (Element::Way(way), ElementKind::Way) => way.id(),
            (Element::Relation(relation), ElementKind::Relation) => relation.id(),
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
    kind: ElementKind,
    block_cap: usize,
    prealloc_bound: usize,
    flags: DegradeFlags,
    compression: Compression,
    drop: Option<&FxHashSet<i64>>,
    writer: &mut PbfWriter<FileWriter>,
    host: &crate::host::Host,
) -> Result<PhaseStats> {
    use crate::reorder_buffer::ReorderBuffer;

    if schedule.is_empty() {
        return Ok(PhaseStats {
            blobs: 0,
            elements: 0,
            unsort_fired: false,
        });
    }

    type PhaseResult = crate::Result<WorkerOutput>;
    let mut reorder: ReorderBuffer<PhaseResult> =
        ReorderBuffer::with_capacity(crate::tunables::POOL_REORDER_CAPACITY);

    let mut bb = BlockBuilder::with_element_cap_prealloc(block_cap, prealloc_bound);
    let mut output: Vec<OwnedBlock> = Vec::new();
    let mut pending: Vec<OwnedBlock> = Vec::with_capacity(FRAME_BATCH);
    let mut unsort = UnsortKindState::new(flags, block_cap);

    let mut blobs: u64 = 0;
    let mut elements: u64 = 0;
    let mut write_error: Option<Error> = None;
    let mut classify_error: Option<Error> = None;

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
                            &host.executor,
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
                        write_error = Some(e);
                        break;
                    }
                    blobs += 1;
                }
                if write_error.is_some() {
                    continue;
                }

                let consume_res: crate::Result<()> = if flags.unsort_any() {
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
                        &host.executor,
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
        &*host.resources,
    )?;

    if let Some(e) = write_error {
        return Err(e);
    }
    if let Some(e) = classify_error {
        return Err(e);
    }

    // End-of-phase: re-inject any held element (partial swap fire), then
    // flush the central builder one last time.
    if let Some(elem) = unsort.held.take() {
        write_owned_to_central(&elem, &mut bb, &mut output)?;
        // This element was already counted in its input tail.
    }

    flush_local(&mut bb, &mut output)?;
    pending.append(&mut output);
    if !pending.is_empty() {
        let final_batch = std::mem::take(&mut pending);
        let written = frame_and_write_batch(
            final_batch,
            compression,
            writer,
            flags.strip_indexdata,
            flags.strip_tagdata,
            &host.executor,
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
    kind: ElementKind,
    cap: usize,
    flags: DegradeFlags,
    compression: &Compression,
    drop: Option<&FxHashSet<i64>>,
) -> crate::Result<WorkerOutput> {
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

    let mut bb = BlockBuilder::with_element_cap_prealloc(cap, total);
    let mut output: Vec<OwnedBlock> = Vec::new();
    let mut full_framed: Vec<Vec<u8>> = Vec::new();
    let mut tail: KindPayload = match kind {
        ElementKind::Node => KindPayload::Nodes(Vec::with_capacity(tail_size)),
        ElementKind::Way => KindPayload::Ways(Vec::with_capacity(tail_size)),
        ElementKind::Relation => KindPayload::Relations(Vec::with_capacity(tail_size)),
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
                (Element::DenseNode(dn), ElementKind::Node) => {
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
                (Element::Node(n), ElementKind::Node) => {
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
                (Element::Way(w), ElementKind::Way) => {
                    ensure_way_capacity_local(&mut bb, &mut output)?;
                    refs_buf.clear();
                    refs_buf.extend(w.refs());
                    let meta = element_metadata(&w.info());
                    // add_way drops inline LOW coords - the documented
                    // behaviour for the decode path regardless of
                    // --strip-locations.
                    bb.add_way(w.id(), w.tags(), &refs_buf, meta.as_ref());
                }
                (Element::Relation(r), ElementKind::Relation) => {
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
    kind: ElementKind,
    drop: Option<&FxHashSet<i64>>,
) -> bool {
    let id = match (element, kind) {
        (Element::DenseNode(node), ElementKind::Node) => node.id(),
        (Element::Node(node), ElementKind::Node) => node.id(),
        (Element::Way(way), ElementKind::Way) => way.id(),
        (Element::Relation(relation), ElementKind::Relation) => relation.id(),
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
) -> crate::Result<()> {
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
) -> crate::Result<()> {
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
) -> crate::Result<()> {
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
) -> crate::Result<Vec<u8>> {
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
    )?;
    Ok(blob.into_vec())
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn frame_and_write_batch(
    batch: Vec<OwnedBlock>,
    compression: Compression,
    writer: &mut PbfWriter<FileWriter>,
    strip_indexdata: bool,
    strip_tagdata: bool,
    executor: &crate::host::Executor,
) -> Result<u64> {
    use rayon::prelude::*;

    let framed: Vec<Result<Vec<u8>>> = executor.install(|| {
        batch
            .into_par_iter()
            .map(
                |OwnedBlock {
                     bytes: block_bytes,
                     index,
                     tagdata,
                     way_members,
                 }|
                 -> Result<Vec<u8>> {
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
            .collect()
    });

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
    use crate::ElementKind;
    use std::collections::BinaryHeap;

    #[test]
    fn unsort_accepts_the_largest_block_cap_without_overflow() {
        let flags = super::DegradeFlags {
            unsort: true,
            ..super::DegradeFlags::default()
        };
        let mut state = super::UnsortKindState::new(flags, usize::MAX);
        assert!(!state.should_hold());
        assert!(!state.should_inject_after());
        state.seen = 1;
        assert!(!state.should_hold());
        assert!(!state.should_inject_after());
    }

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

    /// Pins the `ElementKind` -> byte bridge itself. The golden vectors below
    /// are expressed as bare literals (0/1/2) for historical/readability
    /// reasons, but this assertion is what actually guarantees those
    /// literals still match what `hash_kind` produces - an accidental
    /// reordering of the `hash_kind` match arms would fail here even
    /// though the bare-literal vectors alone could not detect it.
    #[test]
    fn hash_kind_matches_pinned_bytes() {
        assert_eq!(hash_kind(ElementKind::Node), 0);
        assert_eq!(hash_kind(ElementKind::Way), 1);
        assert_eq!(hash_kind(ElementKind::Relation), 2);
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
        // fails if the ElementKind-to-byte bridge ever drifts from the
        // reproducibility contract, not just if `drop_hash` itself changes.
        assert_eq!(
            drop_hash(hash_kind(ElementKind::Node), 1, 0),
            0xe220_a839_7b1d_cdaf
        );
        assert_eq!(
            drop_hash(hash_kind(ElementKind::Way), 1, 0),
            0xd28f_0491_68bd_d34c
        );
        assert_eq!(
            drop_hash(hash_kind(ElementKind::Relation), 42, 0),
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
