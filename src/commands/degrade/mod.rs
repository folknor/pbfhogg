//! Produce a valid-but-adversarial PBF by stripping properties or
//! perturbing structure. A "make our lives difficult" tool for exercising
//! code paths that require less-optimised inputs (unsorted, missing
//! indexdata, scattered coords).
//!
//! v1 transformations: `--unsort`, `--unsort-intra`, `--strip-locations`,
//! `--strip-indexdata`. Flags compose (except the two unsort modes, which
//! are mutually exclusive).
//!
//! Implementation paths:
//!
//! - Pure passthrough (only `--strip-indexdata`): raw blob frames are
//!   reframed with a cleared `BlobHeader.indexdata` field. Blob bytes
//!   are bit-identical; only the header changes. Mirrors `cat`'s
//!   passthrough but drops the index instead of adding one.
//! - Decode path (either unsort mode and/or `--strip-locations`): three
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

use std::path::Path;

use super::{
    HeaderOverrides, Result, build_output_header, ensure_node_capacity_local,
    ensure_relation_capacity_local, ensure_way_capacity_local, flush_local, require_indexdata,
    writer_from_header_bytes,
};
use crate::blob::BlobKind;
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::file_reader::FileReader;
use crate::file_writer::FileWriter;
use crate::owned::{
    OwnedElement, OwnedNode, OwnedRelation, OwnedWay, dense_node_metadata, element_metadata,
    read_dense_node, read_node, read_relation, read_way,
};
use crate::read::raw_frame::read_raw_frame;
use crate::writer::{Compression, PbfWriter, encode_blob_header_into, frame_blob_pipelined};
use crate::{Element, ElementReader};

/// Default per-block element cap. Matches the `BlockBuilder` default and
/// the PBF interop convention. Tests pass a smaller cap via the hidden
/// `--block-cap` CLI flag so fixtures don't need 8000+ elements per kind
/// to exercise the `--unsort` swap.
pub const DEFAULT_BLOCK_CAP: usize = 8000;

const KIND_NODE: u8 = 0;
const KIND_WAY: u8 = 1;
const KIND_RELATION: u8 = 2;

/// Batch size for the parallel-framing fan-out on the merge thread.
const FRAME_BATCH: usize = 32;

/// Set of degradations to apply. At least one flag must be set.
#[derive(Clone, Copy, Debug, Default)]
pub struct DegradeFlags {
    /// Cross-blob unsort: adjacent same-kind blobs get overlapping ID ranges.
    pub unsort: bool,
    /// Intra-blob unsort: one same-kind blob gets an internal ID inversion.
    pub unsort_intra: bool,
    pub strip_locations: bool,
    pub strip_indexdata: bool,
}

impl DegradeFlags {
    pub fn any(self) -> bool {
        self.unsort || self.unsort_intra || self.strip_locations || self.strip_indexdata
    }

    /// Returns `true` if elements must be decoded and re-encoded. Only
    /// `--strip-indexdata` alone can run as a pure blob-level passthrough.
    fn needs_decode(self) -> bool {
        self.unsort || self.unsort_intra || self.strip_locations
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
        eprintln!(
            "Degraded {} elements across {} blobs (applied: {})",
            self.elements_written,
            self.blobs_written,
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
#[allow(clippy::too_many_arguments)]
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
                    (--unsort, --unsort-intra, --strip-locations, --strip-indexdata)"
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
        degrade_passthrough_strip_indexdata(
            input,
            output,
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
    }

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Passthrough: --strip-indexdata only
// ---------------------------------------------------------------------------

/// Raw blob frame iteration with cleared `BlobHeader.indexdata`.
///
/// Blob payload bytes are forwarded verbatim - inline `LocationsOnWays`
/// coordinates, sortedness, and every element-level property pass through
/// unchanged. The output's `LocationsOnWays` and `Sort.Type_then_ID`
/// header features are preserved when the input declared them, since the
/// blob bytes still encode that data.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn degrade_passthrough_strip_indexdata(
    input: &Path,
    output: &Path,
    compression: Compression,
    direct_io: bool,
    io_uring: bool,
    overrides: &HeaderOverrides,
) -> Result<DegradeStats> {
    let header_bytes = {
        let reader = ElementReader::open(input, direct_io)?;
        let header = reader.header().clone();
        build_output_header(&header, header.is_sorted(), overrides, |hb| {
            let mut hb = hb;
            if header.has_locations_on_ways() {
                hb = hb.optional_feature("LocationsOnWays");
            }
            hb
        })?
    };

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
                let blob_bytes = frame.blob_bytes();
                let tagdata = frame.tagdata.as_deref();
                let reframed = reframe_raw_without_index(blob_bytes, tagdata)?;
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
        flags: DegradeFlags {
            unsort: false,
            unsort_intra: false,
            strip_locations: false,
            strip_indexdata: true,
        },
    })
}

/// Reframe a raw OSMData blob with a `BlobHeader` that omits the
/// `indexdata` field. `tagdata` is preserved (a separate flag will strip
/// it; v1 only targets indexdata).
fn reframe_raw_without_index(
    blob_bytes: &[u8],
    tagdata: Option<&[u8]>,
) -> std::io::Result<Vec<u8>> {
    let datasize = i32::try_from(blob_bytes.len()).map_err(|_| {
        std::io::Error::other(format!(
            "blob datasize overflow: {} bytes",
            blob_bytes.len()
        ))
    })?;
    let mut header_buf = Vec::new();
    encode_blob_header_into("OSMData", datasize, None, tagdata, &mut header_buf);
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
// Decode path: --unsort / --unsort-intra and/or --strip-locations (with optional --strip-indexdata)
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
    fn empty(kind: u8) -> Self {
        match kind {
            KIND_WAY => Self::Ways(Vec::new()),
            KIND_RELATION => Self::Relations(Vec::new()),
            _ => Self::Nodes(Vec::new()),
        }
    }

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

#[allow(clippy::too_many_arguments)]
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

    let preserve_sorted = !flags.unsort_any() && header.is_sorted();
    let header_bytes = build_output_header(&header, preserve_sorted, overrides, |hb| hb)?;
    let mut writer =
        writer_from_header_bytes(output, compression, &header_bytes, direct_io, io_uring)?;

    let (node_schedule, way_schedule, rel_schedule, shared_file) =
        crate::scan::classify::build_classify_schedules_split(input)?;

    let mut blobs_written: u64 = 0;
    let mut elements_written: u64 = 0;
    let mut unsort_fired = [false; 3];

    crate::debug::emit_marker("DEGRADE_NODES_START");
    let s = run_kind_phase(
        &shared_file,
        &node_schedule,
        KIND_NODE,
        block_cap,
        flags,
        compression,
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
        KIND_WAY,
        block_cap,
        flags,
        compression,
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
        KIND_RELATION,
        block_cap,
        flags,
        compression,
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
        flags,
    })
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
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn run_kind_phase(
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[(usize, u64, usize)],
    kind: u8,
    block_cap: usize,
    flags: DegradeFlags,
    compression: Compression,
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
            worker_decode_kind(block, kind, block_cap, flags, &compression)
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
                    match frame_and_write_batch(batch, compression, writer, flags.strip_indexdata) {
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
        let written =
            frame_and_write_batch(final_batch, compression, writer, flags.strip_indexdata)?;
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
    kind: u8,
    cap: usize,
    flags: DegradeFlags,
    compression: &Compression,
) -> std::result::Result<WorkerOutput, String> {
    let total = match kind {
        KIND_NODE => block
            .elements()
            .filter(|e| matches!(e, Element::DenseNode(_) | Element::Node(_)))
            .count(),
        KIND_WAY => block
            .elements()
            .filter(|e| matches!(e, Element::Way(_)))
            .count(),
        KIND_RELATION => block
            .elements()
            .filter(|e| matches!(e, Element::Relation(_)))
            .count(),
        _ => return Err(format!("invalid kind constant: {kind}")),
    };

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
        KIND_NODE => KindPayload::Nodes(Vec::with_capacity(tail_size)),
        KIND_WAY => KindPayload::Ways(Vec::with_capacity(tail_size)),
        KIND_RELATION => KindPayload::Relations(Vec::with_capacity(tail_size)),
        _ => KindPayload::empty(kind),
    };

    let mut idx: usize = 0;
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();
    for element in block.elements() {
        let is_match = matches!(
            (&element, kind),
            (Element::DenseNode(_) | Element::Node(_), KIND_NODE)
                | (Element::Way(_), KIND_WAY)
                | (Element::Relation(_), KIND_RELATION)
        );
        if !is_match {
            continue;
        }

        if idx < full_count {
            match (&element, kind) {
                (Element::DenseNode(dn), KIND_NODE) => {
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
                (Element::Node(n), KIND_NODE) => {
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
                (Element::Way(w), KIND_WAY) => {
                    ensure_way_capacity_local(&mut bb, &mut output)?;
                    refs_buf.clear();
                    refs_buf.extend(w.refs());
                    let meta = element_metadata(&w.info());
                    // add_way drops inline LOW coords - the documented
                    // behaviour for the decode path regardless of
                    // --strip-locations.
                    bb.add_way(w.id(), w.tags(), &refs_buf, meta.as_ref());
                }
                (Element::Relation(r), KIND_RELATION) => {
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
        )?);
    }

    Ok(WorkerOutput { full_framed, tail })
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
) -> std::result::Result<Vec<u8>, String> {
    let (block_bytes, index, tagdata) = owned;
    let indexdata_buf = index.serialize();
    let indexdata = if strip_indexdata {
        None
    } else {
        Some(indexdata_buf.as_slice())
    };
    let blob = frame_blob_pipelined(&block_bytes, compression, indexdata, tagdata.as_deref())
        .map_err(|e| e.to_string())?;
    Ok(blob.into_vec())
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn frame_and_write_batch(
    batch: Vec<OwnedBlock>,
    compression: Compression,
    writer: &mut PbfWriter<FileWriter>,
    strip_indexdata: bool,
) -> std::result::Result<u64, Box<dyn std::error::Error>> {
    use rayon::prelude::*;

    let framed: Vec<std::io::Result<Vec<u8>>> = batch
        .into_par_iter()
        .map(
            |(block_bytes, index, tagdata)| -> std::io::Result<Vec<u8>> {
                let indexdata_buf = index.serialize();
                let indexdata = if strip_indexdata {
                    None
                } else {
                    Some(indexdata_buf.as_slice())
                };
                let blob = frame_blob_pipelined(
                    &block_bytes,
                    &compression,
                    indexdata,
                    tagdata.as_deref(),
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
