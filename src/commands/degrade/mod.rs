//! Produce a valid-but-adversarial PBF by stripping properties or
//! perturbing structure. A "make our lives difficult" tool for exercising
//! code paths that require less-optimised inputs (unsorted, missing
//! indexdata, scattered coords).
//!
//! The transformations are defined by [`DegradeFlags`]: header edits, which
//! compose with either route, and an optional [`Reencode`], which holds the
//! transformations that need elements decoded and the block cap they are
//! re-encoded at.
//!
//! Implementation paths, chosen by [`DegradeFlags::reencode`] alone:
//!
//! - Pure passthrough (`reencode: None`; header edits only: `--strip-indexdata`,
//!   `--strip-tagdata`, `--strip-bbox` and the `--corrupt-*` rewrites): the input
//!   `HeaderBlock` payload is preserved verbatim (the outer Blob envelope is
//!   re-compressed, so the decompressed `HeaderBlock` protobuf is
//!   field-identical, not blob-byte-identical) - it is forwarded
//!   field-for-field, only the bbox (field 1) is surgically removed under
//!   `--strip-bbox`, and a `--generator` / `--output-header` override
//!   replaces only the field it names. `source`, custom optional features
//!   (`pbfhogg.WayMembers-v1`, `SharedNodePins-v1`), a non-default
//!   `writingprogram`, replication metadata, and unknown fields all survive
//!   byte-for-byte. Raw OsmData blob frames are then reframed by copying
//!   the original `BlobHeader` through byte-for-byte and clearing (or, under
//!   `--corrupt-*`, replacing) only the targeted field(s) - `indexdata`
//!   (field 2) and/or `tagdata` (field 4).
//!   `--strip-bbox` is entirely an OSMHeader change; it never touches an
//!   OsmData `BlobHeader` or payload.
//!   Every other header field is preserved verbatim: the untouched hint's
//!   original bytes (a v1 index stays v1), `WayMembers-v1` (field 5), and
//!   any unknown/extension fields. Blob bytes are bit-identical; only the
//!   targeted header field changes. Mirrors `cat`'s passthrough but drops
//!   header hints instead of adding them.
//! - Decode path (`reencode: Some`; on the command line, either unsort mode,
//!   `--strip-locations`, or `--drop-ids`): three
//!   sequential per-kind phases driven by `parallel_classify_phase_ordered`.
//!   Workers decode one input blob, filter to the current kind, and
//!   re-encode. Without an unsort mode, each worker encodes and frames its
//!   blob whole, cap-sized blocks and then the remaining `M % cap` elements
//!   as their own block, so output blobs follow input blob boundaries
//!   (sort preserved) and the merge thread only writes frames. Under
//!   either unsort mode, workers ship every element as `Owned*` so the
//!   merge thread can apply the cap-1 swap per kind in a serial state
//!   machine, in a central `BlockBuilder` whose blocks the pipelined
//!   writer frames.
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
    read_dense_node, read_node, read_relation, read_way, relation_members_into,
};
use crate::read::frame::Frames;
use crate::read::input::InputSource;
use crate::write::WriteOptions;
use crate::writer::{Compression, PbfWriter, strip_blob_header_fields};
use crate::{Element, ElementReader};

use crate::tunables::CORRUPT_INDEXDATA_INFLATION;

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
                "a drop spec is N:SEED (e.g. 5000:42); the ':' separator is required",
            )
        })?;
        let n = n_str.trim().parse().map_err(|_| {
            Error::invalid_argument(format!(
                "drop spec: N must be a non-negative integer, got {n_str:?}"
            ))
        })?;
        let seed = seed_str.trim().parse().map_err(|_| {
            Error::invalid_argument(format!(
                "drop spec: SEED must be a non-negative integer, got {seed_str:?}"
            ))
        })?;
        if n == 0 {
            return Err(Error::invalid_argument(
                "drop spec: N must be >= 1 (dropping zero elements is a no-op)",
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

/// Which unsort shape a [`Reencode`] produces. Both clear `Sort.Type_then_ID`
/// and swap one adjacent same-kind element pair per kind; see the module
/// documentation for where each pair lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unsort {
    /// Cross-blob unsort (`--unsort`): adjacent same-kind blobs get
    /// overlapping ID ranges. Valid at any block cap.
    CrossBlob,
    /// Intra-blob unsort (`--unsort-intra`): one same-kind blob gets an
    /// internal ID inversion. Needs a block cap of at least 2.
    IntraBlob,
}

/// Decode every element and re-encode it at `block_cap` elements per block
/// at most, applying the transformations that need decoded elements.
///
/// Every re-encode writes ways without inline coordinates, rebuilds each
/// blob's `indexdata` and `tagdata` from the elements it wrote (stripped
/// again where [`DegradeFlags`] asks), and builds the output header anew,
/// so a `LocationsOnWays` input loses that feature with a warning unless
/// `strip_locations` declares the loss intended. Output blocks follow the
/// input's blob boundaries, except under [`Unsort::CrossBlob`], which packs
/// each kind continuously to the cap. A `Reencode` with none of its
/// transformations set is that re-encode alone.
#[derive(Clone, Copy, Debug)]
pub struct Reencode {
    /// The per-block element cap: the most elements any output data blob
    /// holds, and under [`Unsort::CrossBlob`] the boundary the swap
    /// straddles. [`Reencode::default`] uses [`DEFAULT_BLOCK_CAP`]; a smaller
    /// cap shrinks a block too large for the format, unless single elements
    /// are that large, and lets the unsort swaps fire on small inputs.
    pub block_cap: std::num::NonZeroUsize,
    pub unsort: Option<Unsort>,
    /// The way coordinates every re-encode drops are the request
    /// (`--strip-locations`), so their loss is not warned about.
    pub strip_locations: bool,
    /// Deterministically remove exactly this many unique element IDs.
    pub drop_ids: Option<DropSpec>,
}

impl Default for Reencode {
    fn default() -> Self {
        Self {
            block_cap: DEFAULT_BLOCK_CAP,
            unsort: None,
            strip_locations: false,
            drop_ids: None,
        }
    }
}

impl Reencode {
    /// Cross-blob unsort suppresses the per-input-blob boundary flush so
    /// the central `BlockBuilder` packs to `block_cap` and the swap straddles
    /// a genuine output-blob boundary (adjacent blobs overlap). Every other
    /// re-encode keeps output blobs mirroring input blobs: [`Unsort::IntraBlob`]
    /// through this flush, the rest by encoding each blob on its worker.
    /// (The intra-blob swap is confined to a single
    /// blob by its hold-at-position-1 placement, not by the flush, so it
    /// stays intra-blob regardless of input blob sizes.)
    fn suppress_boundary_flush(self) -> bool {
        self.unsort == Some(Unsort::CrossBlob)
    }
}

/// Set of degradations to apply: header edits and an optional [`Reencode`].
/// At least one must be requested.
#[derive(Clone, Copy, Debug, Default)]
pub struct DegradeFlags {
    /// Decode and re-encode every element (`None`: the header-only
    /// passthrough). Every transformation that needs decoded elements lives
    /// inside it, and it may also be asked for with none of them set. The
    /// command line sets it exactly when `--unsort`, `--unsort-intra`,
    /// `--strip-locations` or `--drop-ids` is given.
    pub reencode: Option<Reencode>,
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
    /// Header-only, so it runs on the passthrough path only: a re-encode
    /// would rebuild the hint correctly. Mutually exclusive with
    /// `--strip-tagdata`: both target field 4.
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
}

impl DegradeFlags {
    /// What the run is asked to do, each operation by name - the library's
    /// own vocabulary, not a front end's flag spelling. The first element of
    /// each pair says whether it is requested; the second whether it is one
    /// of the top-level requests (`reencode` and the header edits), as
    /// opposed to a transformation inside the re-encode.
    fn operations(self) -> [(bool, bool, &'static str); 11] {
        let reencode = self.reencode;
        let inside = |f: fn(Reencode) -> bool| reencode.is_some_and(f);
        [
            (reencode.is_some(), true, "reencode"),
            (
                inside(|r| r.unsort == Some(Unsort::CrossBlob)),
                false,
                "unsort",
            ),
            (
                inside(|r| r.unsort == Some(Unsort::IntraBlob)),
                false,
                "unsort_intra",
            ),
            (inside(|r| r.strip_locations), false, "strip_locations"),
            (self.strip_indexdata, true, "strip_indexdata"),
            (self.strip_tagdata, true, "strip_tagdata"),
            (self.corrupt_tagdata, true, "corrupt_tagdata"),
            (self.corrupt_indexdata, true, "corrupt_indexdata"),
            (self.corrupt_indexdata_kind, true, "corrupt_indexdata_kind"),
            (self.strip_bbox, true, "strip_bbox"),
            (inside(|r| r.drop_ids.is_some()), false, "drop_ids"),
        ]
    }

    /// Whether anything at all is requested: a re-encode or a header edit.
    pub fn any(self) -> bool {
        self.operations().into_iter().any(|(enabled, _, _)| enabled)
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
            .operations()
            .into_iter()
            .filter_map(|(enabled, _, name)| if enabled { Some(name) } else { None })
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
/// `write.force` skips the indexdata precondition required by the decode
/// path's per-kind classify pipeline. Has no effect on the passthrough path.
/// `write.compression` applies to every blob the run frames: on the
/// passthrough that is the OSMHeader alone, since data blobs are copied with
/// their own compression.
///
/// The route is chosen from [`DegradeFlags::reencode`] alone, before the
/// input is opened. The
/// passthrough reads the input once, sequentially, so any readable input
/// serves it, a FIFO included (with buffered reads). The decode path reads
/// it in several passes, so there it must be a regular file or a block
/// device (`InvalidArgument` otherwise) and is pinned for the run (see
/// [`PinnedInput`](crate::read::input::PinnedInput)). A caller's pin is
/// used as it is on either route; where a later open cannot reach the
/// pinned file, an input replaced mid-run fails with `InputReplaced` and
/// nothing is published.
pub fn degrade<'a>(
    input: impl Into<InputSource<'a>>,
    output: impl crate::output::BindOutput,
    flags: DegradeFlags,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<DegradeStats> {
    let host = &host.enter()?;
    let output = crate::output::bind_unissued(output, &host.invocation)?;
    host.invocation.check()?;
    let result = degrade_to(input.into(), &output, flags, write, overrides, host)?;
    output.publish()?;
    Ok(result)
}

/// [`degrade`] writing to a borrowed output it does not publish.
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn degrade_to(
    input: InputSource<'_>,
    output: &crate::output::BoundOutput,
    flags: DegradeFlags,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<DegradeStats> {
    if !flags.any() {
        let names: Vec<&str> = flags
            .operations()
            .into_iter()
            .filter_map(|(_, top_level, name)| top_level.then_some(name))
            .collect();
        return Err(Error::invalid_argument(format!(
            "degrade requires at least one transformation ({})",
            names.join(", ")
        )));
    }
    // The decode path rebuilds each blob's hints from the elements it just
    // re-encoded, so a corrupt hint written there would be overwritten by a
    // correct one. Refuse rather than accept a flag that would do nothing.
    if (flags.corrupt_tagdata || flags.corrupt_indexdata || flags.corrupt_indexdata_kind)
        && flags.reencode.is_some()
    {
        return Err(Error::invalid_argument(
            "corrupt_tagdata, corrupt_indexdata and corrupt_indexdata_kind are \
             header-only rewrites and cannot be combined with reencode, which \
             would rebuild the hint correctly",
        ));
    }
    if flags.strip_tagdata && flags.corrupt_tagdata {
        return Err(Error::invalid_argument(
            "strip_tagdata and corrupt_tagdata are mutually exclusive: \
             both rewrite BlobHeader field 4, one to nothing and one to a \
             deliberately wrong index",
        ));
    }
    if flags.strip_indexdata && (flags.corrupt_indexdata || flags.corrupt_indexdata_kind) {
        return Err(Error::invalid_argument(
            "strip_indexdata is mutually exclusive with corrupt_indexdata and \
             corrupt_indexdata_kind: all three rewrite BlobHeader field 2, \
             one to nothing and the others to a deliberately wrong index",
        ));
    }
    // An intra-blob inversion needs two same-kind elements sitting inside
    // one output block. A cap of 1 puts every element in its own block, so
    // the requested shape is impossible - reject it rather than silently
    // clearing Sort.Type_then_ID and emitting an untouched (still-monotone)
    // stream. Cross-blob `unsort` IS achievable at cap 1: two adjacent
    // single-element blobs with a descending step overlap, so it stays
    // supported.
    if let Some(reencode) = flags.reencode
        && reencode.unsort == Some(Unsort::IntraBlob)
        && reencode.block_cap.get() < 2
    {
        return Err(Error::invalid_argument(
            "an intra-blob unsort needs a block cap of at least 2: an \
             intra-blob inversion requires at least two same-kind elements \
             inside one output block, which a cap of 1 cannot hold",
        ));
    }

    let reencode = flags.reencode;
    let unsort = reencode.and_then(|r| r.unsort);
    host.invocation
        .counter("degrade_reencode", reencode.is_some());
    host.invocation
        .counter("degrade_unsort", unsort == Some(Unsort::CrossBlob));
    host.invocation
        .counter("degrade_unsort_intra", unsort == Some(Unsort::IntraBlob));
    host.invocation.counter(
        "degrade_strip_locations",
        reencode.is_some_and(|r| r.strip_locations),
    );
    host.invocation
        .counter("degrade_strip_indexdata", flags.strip_indexdata);
    host.invocation
        .counter("degrade_strip_tagdata", flags.strip_tagdata);
    host.invocation
        .counter("degrade_corrupt_tagdata", flags.corrupt_tagdata);
    host.invocation
        .counter("degrade_corrupt_indexdata", flags.corrupt_indexdata);
    host.invocation.counter(
        "degrade_corrupt_indexdata_kind",
        flags.corrupt_indexdata_kind,
    );
    host.invocation
        .counter("degrade_strip_bbox", flags.strip_bbox);
    let drop_ids = reencode.and_then(|r| r.drop_ids);
    host.invocation
        .counter("degrade_drop_ids", drop_ids.is_some());
    if let Some(spec) = drop_ids {
        // Both are user-supplied u64s and have always been emitted as their
        // bit pattern reinterpreted as i64 (a value from 2^63 up arrives
        // negative), so they keep that rather than the saturation
        // `emit_counter` applies to an unsigned count.
        host.invocation
            .counter("degrade_drop_n", spec.n.cast_signed());
        host.invocation
            .counter("degrade_drop_seed", spec.seed.cast_signed());
    }

    let stats = if let Some(reencode) = reencode {
        // The cap is a re-encode setting: a passthrough run has none to report.
        host.invocation
            .counter("degrade_block_cap", reencode.block_cap.get());
        degrade_decode_path(input, output, flags, reencode, write, overrides, host)?
    } else {
        degrade_passthrough(input, output, flags, write, overrides, host)?
    };

    host.invocation
        .counter("degrade_blobs_written", stats.blobs_written);
    host.invocation
        .counter("degrade_elements_written", stats.elements_written);
    host.invocation
        .counter("degrade_dropped_elements", stats.dropped);

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Passthrough: header edits only (no reencode)
// ---------------------------------------------------------------------------

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
///
/// Reads the first frame from `frames`, so the caller goes on to the data
/// frames through the same walk.
fn passthrough_header_bytes(
    frames: &mut Frames<FileReader>,
    input: &std::path::Path,
    flags: DegradeFlags,
    overrides: &HeaderOverrides,
) -> Result<Vec<u8>> {
    // The OSMHeader is always the first blob in a well-formed PBF.
    let frame = frames
        .next_raw()?
        .ok_or_else(|| crate::error::new_missing_header(input))?;
    let locate = |e: Error| frames.locate(frame.id, e);
    if !matches!(frame.blob_type, BlobKind::OsmHeader) {
        return Err(crate::error::new_missing_header(input));
    }
    let mut header_block = Vec::new();
    crate::read::decompress::decompress_blob_data_into(frame.blob_bytes(), &mut header_block)
        .map_err(locate)?;
    super::output_header::verbatim(&header_block, flags.strip_bbox, overrides).map_err(locate)
}

/// Raw blob frame iteration that applies the requested header edits to each
/// `BlobHeader` (clearing or rewriting `indexdata` and/or `tagdata`) while
/// preserving every other header field and the entire blob payload
/// byte-for-byte.
///
/// Each output `BlobHeader` is the input header with only the targeted
/// field(s) changed - so the untargeted hint keeps its exact original bytes
/// (a v1 index is never upgraded to v2, an undeserializable index is never
/// dropped), and `WayMembers-v1` (field 5) plus any unknown/extension fields
/// carry through unchanged. Blob payload bytes are forwarded verbatim -
/// inline `LocationsOnWays` coordinates, sortedness, compression and every
/// element-level property pass through unchanged; `write.compression`
/// frames only the OSMHeader. The output's `LocationsOnWays` and
/// `Sort.Type_then_ID` header features are preserved when the input declared
/// them, since the blob bytes still encode that data. `indexdata` survives
/// unless `--strip-indexdata` clears it or `--corrupt-indexdata` /
/// `--corrupt-indexdata-kind` replaces it; `tagdata` survives unless
/// `--strip-tagdata` clears it or `--corrupt-tagdata` replaces it - so
/// `--strip-tagdata` alone yields a still-indexed file.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn degrade_passthrough(
    source: InputSource<'_>,
    output: &crate::output::BoundOutput,
    flags: DegradeFlags,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<DegradeStats> {
    // One open, one sequential traversal: the header frame and then every
    // data frame come from this reader, so the run reads exactly one file
    // whatever happens to the path after the open. That is why this route
    // takes no pin of its own (a FIFO or stdin serves it, buffered) and the
    // decode path does; a caller's pin is opened as the file it pinned.
    let input = source.path();
    let mut frames = Frames::of_file_input(
        FileReader::open_in(source, write.direct_reads, &host.invocation)?,
        input,
        host,
    );
    let header_bytes = passthrough_header_bytes(&mut frames, input, flags, overrides)?;

    let mut writer = writer_from_header_bytes(
        output,
        write.compression,
        &header_bytes,
        write.backend,
        host,
    )?;

    let mut blobs_written: u64 = 0;

    host.invocation.marker("DEGRADE_PASSTHROUGH_START");
    while let Some(frame) = frames.next_raw()? {
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
                // A blob with no readable indexdata (absent, or bytes the
                // reader treats as absent) has nothing to corrupt: refuse
                // rather than write it back honest while the flag reads as
                // applied.
                if corrupt_index.is_none()
                    && (flags.corrupt_indexdata || flags.corrupt_indexdata_kind)
                {
                    return Err(frames.locate(
                        frame.id,
                        Error::invalid_data(
                            "corrupt_indexdata and corrupt_indexdata_kind rewrite a \
                             readable indexdata, and this blob has none",
                        ),
                    ));
                }
                let reframed = reframe_raw(
                    frame.header_bytes(),
                    frame.blob_bytes(),
                    flags.strip_indexdata || corrupt_index.is_some(),
                    flags.strip_tagdata || flags.corrupt_tagdata,
                    flags.corrupt_tagdata,
                    corrupt_index,
                )
                .map_err(|e| frames.locate(frame.id, e))?;
                writer.write_raw_owned(reframed)?;
                blobs_written += 1;
            }
            _ => {}
        }
    }
    host.invocation.marker("DEGRADE_PASSTHROUGH_END");

    host.invocation.marker("DEGRADE_FLUSH_START");
    writer.flush()?;
    host.invocation.marker("DEGRADE_FLUSH_END");

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
// Decode path: a Reencode, with optional --strip-indexdata / --strip-tagdata / --strip-bbox
// ---------------------------------------------------------------------------

/// An input blob's matching elements, owned for the merge thread's builder:
/// under either unsort mode, so the merge thread can apply the swap.
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

/// One worker's output for one input blob.
enum WorkerOutput {
    /// Without an unsort mode: the blob's matching elements re-encoded and
    /// framed by the worker, blocks of `block_cap` and the remainder as its
    /// own block, which is the block the merge thread's boundary flush made
    /// of it when it re-encoded remainders itself.
    Framed { frames: Vec<Vec<u8>>, elements: u64 },
    /// Under either unsort mode: every matching element, owned.
    Owned(KindPayload),
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
    fn new(mode: Unsort, cap: usize) -> Self {
        // block_cap validation upstream guarantees hold_at is reachable:
        // >= 1 for cross-blob, >= 2 for intra-blob.
        let hold_at = match mode {
            Unsort::CrossBlob => cap as u64,
            Unsort::IntraBlob => 1,
        };
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

    /// The held element, taken, once the element after which it goes back
    /// in has arrived; `None` otherwise.
    fn take_due(&mut self) -> Option<OwnedElement> {
        if self.seen == self.hold_at {
            self.held.take()
        } else {
            None
        }
    }
}

#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn degrade_decode_path(
    input: InputSource<'_>,
    output: &crate::output::BoundOutput,
    flags: DegradeFlags,
    reencode: Reencode,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<DegradeStats> {
    let WriteOptions {
        compression,
        backend,
        direct_reads: direct_io,
        force,
    } = *write;

    // The input is opened three times, each through its own open file
    // description: the indexdata probe, the header reader, then the schedule
    // walk whose descriptor the drop selection and every per-kind pool
    // pread (its fstat is the pre-allocation bound below). All three are
    // derived from this one pin, so a file renamed over the input mid-run
    // cannot be probed, headed and re-encoded partly from each file: where
    // `/proc/self/fd` reopens the pin every open reaches the pinned file,
    // otherwise on Unix a replaced input fails the run (`PinnedInput` says
    // what it does off Unix). Taken before any other open of the input.
    let pin = input.pin(host)?;
    let input = &pin;

    require_indexdata(
        input,
        direct_io,
        force,
        "input PBF has no blob-level indexdata. degrade's decode path uses the \
         parallel per-kind classify pipeline, which needs indexdata to build \
         per-kind blob schedules.",
        host,
    )?;

    let header = ElementReader::open_in(input, direct_io, host)?
        .header()
        .clone();

    // Each per-kind phase re-encodes only its own kind, so an unindexed blob
    // (only under --force) sits in all three and is decoded once per phase.
    let (schedules, shared_file, _max_ids) = crate::scan::classify::build_classify_schedules_split(
        input,
        crate::scan::classify::KindClaims::Refuse,
        host,
    )?;
    let (node_schedule, way_schedule, rel_schedule) = schedules.into_replicated();
    // Use the input size as a preallocation budget for the caller-supplied
    // cap. Compressed input can expand to more elements than bytes; the
    // vectors grow as needed without changing the requested block cap.
    let prealloc_bound =
        usize::try_from(shared_file.file().metadata()?.len()).unwrap_or(usize::MAX);

    // Selection must finish before opening the writer: an over-large N must
    // not leave a header-only output behind.
    let drop_sets = if let Some(spec) = reencode.drop_ids {
        host.invocation.marker("DEGRADE_DROP_SELECT_START");
        let sets = select_drop_sets(
            &shared_file,
            &node_schedule,
            &way_schedule,
            &rel_schedule,
            spec,
            host,
        )?;
        host.invocation.marker("DEGRADE_DROP_SELECT_END");
        Some(sets)
    } else {
        None
    };

    // Every way is re-encoded without inline coordinates or prepass fields;
    // under --strip-locations that is the request, not a loss to warn about.
    let facts = OutputFacts {
        order: if reencode.unsort.is_some() {
            Order::Unsorted
        } else {
            Order::AsInput
        },
        bbox: if flags.strip_bbox {
            Bbox::Drop
        } else {
            Bbox::Carry
        },
        ..OutputFacts::carry(if reencode.strip_locations {
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

    host.invocation.marker("DEGRADE_NODES_START");
    let s = run_kind_phase(
        &shared_file,
        &node_schedule,
        ElementKind::Node,
        reencode,
        prealloc_bound,
        flags,
        compression,
        drop_sets
            .as_ref()
            .map(|sets| sets.for_kind(ElementKind::Node)),
        &mut writer,
        host,
    )?;
    host.invocation.marker("DEGRADE_NODES_END");
    blobs_written += s.blobs;
    elements_written += s.elements;
    unsort_fired[0] = s.unsort_fired;

    host.invocation.marker("DEGRADE_WAYS_START");
    let s = run_kind_phase(
        &shared_file,
        &way_schedule,
        ElementKind::Way,
        reencode,
        prealloc_bound,
        flags,
        compression,
        drop_sets
            .as_ref()
            .map(|sets| sets.for_kind(ElementKind::Way)),
        &mut writer,
        host,
    )?;
    host.invocation.marker("DEGRADE_WAYS_END");
    blobs_written += s.blobs;
    elements_written += s.elements;
    unsort_fired[1] = s.unsort_fired;

    host.invocation.marker("DEGRADE_RELATIONS_START");
    let s = run_kind_phase(
        &shared_file,
        &rel_schedule,
        ElementKind::Relation,
        reencode,
        prealloc_bound,
        flags,
        compression,
        drop_sets
            .as_ref()
            .map(|sets| sets.for_kind(ElementKind::Relation)),
        &mut writer,
        host,
    )?;
    host.invocation.marker("DEGRADE_RELATIONS_END");
    blobs_written += s.blobs;
    elements_written += s.elements;
    unsort_fired[2] = s.unsort_fired;

    host.invocation.marker("DEGRADE_FLUSH_START");
    writer.flush()?;
    host.invocation.marker("DEGRADE_FLUSH_END");

    #[allow(clippy::cast_possible_wrap)]
    {
        host.invocation
            .counter("degrade_unsort_fired_nodes", i64::from(unsort_fired[0]));
        host.invocation
            .counter("degrade_unsort_fired_ways", i64::from(unsort_fired[1]));
        host.invocation
            .counter("degrade_unsort_fired_relations", i64::from(unsort_fired[2]));
    }

    Ok(DegradeStats {
        blobs_written,
        elements_written,
        dropped: reencode.drop_ids.map_or(0, |spec| spec.n),
        flags,
    })
}

fn select_drop_sets(
    shared_file: &crate::read::input::InputFile,
    node_schedule: &[crate::scan::classify::ScheduledBlob],
    way_schedule: &[crate::scan::classify::ScheduledBlob],
    relation_schedule: &[crate::scan::classify::ScheduledBlob],
    spec: DropSpec,
    host: &crate::host::Entered<'_>,
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
            |block, _| Ok(select_block_keys(block, kind, spec)),
            |_seq, block_drop| {
                total += block_drop.matched;
                for key in block_drop.smallest {
                    keep_smallest(&mut heap, spec.n, key);
                }
            },
            host,
        )?;
    }
    if spec.n > total {
        return Err(Error::invalid_argument(format!(
            "drop_ids: cannot drop {} elements, input has only {}",
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

/// Run one per-kind phase. Without an unsort mode every worker re-encodes
/// and frames its blob whole (blocks of the cap, then the remainder as its
/// own block), so output blobs follow the input's blob boundaries, IDs stay
/// ascending, and the merge thread only writes frames in schedule order.
/// Under either unsort mode workers ship every matching element as
/// `Owned*` so the merge thread can run the adjacent-pair swap in its
/// central `BlockBuilder`. `--unsort` additionally suppresses the boundary
/// flush so the central builder packs to cap and the swap straddles a real
/// output-blob boundary (cross-blob overlap); `--unsort-intra` keeps the
/// flush and swaps the first two same-kind elements, so the inversion
/// stays inside the first output block (intra-blob inversion).
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn run_kind_phase(
    shared_file: &crate::read::input::InputFile,
    schedule: &[crate::scan::classify::ScheduledBlob],
    kind: ElementKind,
    reencode: Reencode,
    prealloc_bound: usize,
    flags: DegradeFlags,
    compression: Compression,
    drop: Option<&FxHashSet<i64>>,
    writer: &mut PbfWriter<FileWriter>,
    host: &crate::host::Entered<'_>,
) -> Result<PhaseStats> {
    let cap = reencode.block_cap;

    if schedule.is_empty() {
        return Ok(PhaseStats {
            blobs: 0,
            elements: 0,
            unsort_fired: false,
        });
    }

    type PhaseResult = crate::Result<WorkerOutput>;

    let mut bb = BlockBuilder::with_element_cap_prealloc(cap, prealloc_bound);
    let mut output: Vec<OwnedBlock> = Vec::new();
    // Present only under an unsort mode; without one no element reaches the
    // central builder.
    let mut unsort = reencode
        .unsort
        .map(|mode| UnsortKindState::new(mode, cap.get()));

    let mut blobs: u64 = 0;
    let mut elements: u64 = 0;

    // Results arrive in schedule order through the ordered pool, whose window
    // bounds the decoded payloads in flight. A write or encode error ends the
    // phase at its position.
    crate::scan::classify::parallel_classify_phase_ordered(
        shared_file,
        schedule,
        None,
        None,
        // Each worker keeps one builder and its refs scratch across blobs.
        // Its preallocation is the default block's at most: the cap is user
        // input, and the vectors grow if a block needs more.
        || {
            (
                crate::commands::WorkerBuilder::new(BlockBuilder::with_element_cap_prealloc(
                    cap,
                    prealloc_bound.min(crate::tunables::DEFAULT_ELEMENT_CAP.get()),
                )),
                Vec::<i64>::new(),
            )
        },
        |block, (worker, refs_buf)| -> PhaseResult {
            worker_decode_kind(
                block,
                kind,
                reencode,
                drop,
                worker,
                refs_buf,
                crate::commands::FramedBlocks::with_fields(
                    compression,
                    &host.invocation.metrics().writer,
                    !flags.strip_indexdata,
                    !flags.strip_tagdata,
                ),
            )
        },
        |_position, out| {
            match out {
                WorkerOutput::Framed {
                    frames,
                    elements: n,
                } => {
                    // No unsort mode: nothing is ever in the central
                    // builder, so the frames go out as they are, in
                    // schedule order.
                    blobs += frames.len() as u64;
                    for frame in frames {
                        writer.write_raw_owned(frame)?;
                    }
                    elements += n;
                }
                WorkerOutput::Owned(tail) => {
                    // `--unsort-intra` flushes the central builder at every
                    // input-blob boundary, so output blobs mirror input
                    // blobs (the swap of the first two same-kind elements
                    // lands inside the first output block whatever the
                    // boundaries). `--unsort` suppresses the flush so the
                    // central builder packs continuously to cap across
                    // input blobs and the boundary swap straddles a real
                    // output-blob boundary (cross-blob overlap).
                    if !reencode.suppress_boundary_flush() && !bb.is_empty() {
                        flush_local(&mut bb, &mut output)?;
                    }
                    let tail_n = tail.len() as u64;
                    // The worker and this state both follow
                    // `reencode.unsort`, so owned elements come only with it.
                    let Some(state) = unsort.as_mut() else {
                        return Err(crate::error::Error::internal(
                            "a degrade worker shipped owned elements without an unsort mode",
                        ));
                    };
                    feed_tail_unsort(tail, state, &mut bb, &mut output)?;
                    blobs += write_central(&mut output, writer, flags)?;
                    elements += tail_n;
                }
            }
            Ok(())
        },
        host,
    )?;

    // End-of-phase: re-inject any held element (partial swap fire), then
    // flush the central builder one last time.
    if let Some(elem) = unsort.as_mut().and_then(|state| state.held.take()) {
        write_owned_to_central(&elem, &mut bb, &mut output)?;
        // This element was already counted in its input tail.
    }

    flush_local(&mut bb, &mut output)?;
    // Under the unsort modes this flush can be the writer executor's first
    // job, after the phase's pool has ended, and the pool started there idles
    // through the next phase holding the exited workers' arenas
    // (`reference/performance/memory.md`, "Idle executor threads take exited
    // workers' arenas"). Left as is on purpose: the unsort modes generate
    // adversarial test inputs, where a few hundred MB of peak is no cost.
    blobs += write_central(&mut output, writer, flags)?;

    Ok(PhaseStats {
        blobs,
        elements,
        unsort_fired: unsort.is_some_and(|state| state.fired),
    })
}

/// Worker body: decode and filter to one kind. Without an unsort mode,
/// re-encode every matching element through the worker's builder, each
/// block framed as it fills, the remainder as its own block. Under either
/// unsort mode, ship every matching element owned so the merge thread can
/// apply the cap-1 swap.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
fn worker_decode_kind(
    block: &crate::PrimitiveBlock,
    kind: ElementKind,
    reencode: Reencode,
    drop: Option<&FxHashSet<i64>>,
    worker: &mut crate::commands::WorkerBuilder,
    refs_buf: &mut Vec<i64>,
    mut framed: crate::commands::FramedBlocks<'_>,
) -> crate::Result<WorkerOutput> {
    // Trust: the schedules (drop selection included) are built with
    // `KindClaims::Refuse`, so the pool that decoded this block has already
    // refused it if it holds an element of a kind other than the one its
    // indexdata claimed; nothing is dropped silently. An unindexed blob
    // (`--force`) carries no claim and is not refused (see the same note in
    // repack's worker_split_blob and reference/pipeline.md "Blob Filtering").
    // `element_matches_kind` already discriminates by `kind`, so no per-kind
    // match is needed.
    if reencode.unsort.is_some() {
        let total = block
            .elements()
            .filter(|e| element_matches_kind(e, kind, drop))
            .count();
        let mut tail: KindPayload = match kind {
            ElementKind::Node => KindPayload::Nodes(Vec::with_capacity(total)),
            ElementKind::Way => KindPayload::Ways(Vec::with_capacity(total)),
            ElementKind::Relation => KindPayload::Relations(Vec::with_capacity(total)),
        };
        for element in block.elements() {
            if !element_matches_kind(&element, kind, drop) {
                continue;
            }
            match (&element, &mut tail) {
                (Element::DenseNode(dn), KindPayload::Nodes(v)) => v.push(read_dense_node(dn)?),
                (Element::Node(n), KindPayload::Nodes(v)) => v.push(read_node(n)?),
                (Element::Way(w), KindPayload::Ways(v)) => v.push(read_way(w)?),
                (Element::Relation(r), KindPayload::Relations(v)) => v.push(read_relation(r)?),
                _ => {}
            }
        }
        return Ok(WorkerOutput::Owned(tail));
    }

    let bb = worker.task()?;
    let mut elements: u64 = 0;
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();
    for element in block.elements() {
        if !element_matches_kind(&element, kind, drop) {
            continue;
        }
        match (&element, kind) {
            (Element::DenseNode(dn), ElementKind::Node) => {
                ensure_node_capacity_local(bb, &mut framed)?;
                let meta = dense_node_metadata(dn)?;
                bb.add_node(
                    dn.id(),
                    dn.decimicro_lat(),
                    dn.decimicro_lon(),
                    dn.tags(),
                    meta.as_ref(),
                );
            }
            (Element::Node(n), ElementKind::Node) => {
                ensure_node_capacity_local(bb, &mut framed)?;
                let meta = element_metadata(ElementKind::Node, n.id(), &n.info())?;
                bb.add_node(
                    n.id(),
                    n.decimicro_lat(),
                    n.decimicro_lon(),
                    n.tags(),
                    meta.as_ref(),
                );
            }
            (Element::Way(w), ElementKind::Way) => {
                ensure_way_capacity_local(bb, &mut framed)?;
                refs_buf.clear();
                refs_buf.extend(w.refs());
                let meta = element_metadata(ElementKind::Way, w.id(), &w.info())?;
                // add_way drops inline LOW coords - the documented
                // behaviour for the decode path regardless of
                // --strip-locations.
                bb.add_way(w.id(), w.tags(), refs_buf, meta.as_ref());
            }
            (Element::Relation(r), ElementKind::Relation) => {
                ensure_relation_capacity_local(bb, &mut framed)?;
                relation_members_into(r, &mut members_buf)?;
                let meta = element_metadata(ElementKind::Relation, r.id(), &r.info())?;
                bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
            }
            _ => {}
        }
        elements += 1;
    }

    // The final block, framed under the same strip flags as every other.
    flush_local(bb, &mut framed)?;
    crate::commands::release_scratch(refs_buf);
    let frames = framed.frames.into_iter().map(|(_, frame)| frame).collect();

    Ok(WorkerOutput::Framed { frames, elements })
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
                } else if let Some(held) = unsort.take_due() {
                    crate::owned::write_single_node_local(&n, bb, output)?;
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
                } else if let Some(held) = unsort.take_due() {
                    crate::owned::write_single_way_local(&w, bb, output)?;
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
                } else if let Some(held) = unsort.take_due() {
                    crate::owned::write_single_relation_local(&r, bb, output)?;
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

/// Hand every central block in `output` to the pipelined writer, which
/// frames it on its executor and writes it in submission order, under the
/// same strip flags as the workers' frames; returns how many there were.
fn write_central(
    output: &mut Vec<OwnedBlock>,
    writer: &mut PbfWriter<FileWriter>,
    flags: DegradeFlags,
) -> Result<u64> {
    let n = output.len() as u64;
    for mut block in output.drain(..) {
        if flags.strip_tagdata {
            block.tagdata = None;
        }
        if flags.strip_indexdata {
            writer.write_primitive_block_owned_no_indexdata(block)?;
        } else {
            writer.write_primitive_block_owned(block)?;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::{DropKey, drop_hash, hash_kind, keep_smallest, mix64};
    use crate::ElementKind;
    use std::collections::BinaryHeap;

    #[test]
    fn unsort_accepts_the_largest_block_cap_without_overflow() {
        let mut state = super::UnsortKindState::new(super::Unsort::CrossBlob, usize::MAX);
        assert!(!state.should_hold());
        assert!(state.take_due().is_none());
        state.seen = 1;
        assert!(!state.should_hold());
        assert!(state.take_due().is_none());
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
