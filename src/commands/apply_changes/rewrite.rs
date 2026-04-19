//! Element rewriting, output streaming, and merge orchestration.

use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;

use rayon::prelude::*;
use rustc_hash::FxHashMap;

use crate::blob::{decode_blob_to_headerblock, BlobKind};
use crate::blob_meta::{BlobIndex, ElemKind};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::file_reader::FileReader;
use crate::file_writer::FileWriter;
use crate::osc::{parse_osc_file, CompactDiffOverlay};
use crate::writer::{Compression, PbfWriter};
use crate::{Element, PrimitiveBlock};

use crate::commands::{
    build_output_header,
    ensure_node_capacity, ensure_node_capacity_local, ensure_relation_capacity_local,
    flush_block, flush_local, flush_passthrough_buf,
    require_indexdata, writer_from_header_bytes, HeaderOverrides,
    BATCH_MAX_BLOBS, BATCH_MIN_BLOBS,
};
use crate::read::raw_frame::{read_raw_frame, RawBlobFrame};
use crate::blob::parse_blob_header_with_index;
use crate::reorder_buffer::ReorderBuffer;

use super::classify::{
    classify_only, BatchSlot, ClassifyResult, RewriteJob,
};
use super::diff_ranges::{DiffRanges, UpsertCursors};
use super::element_writes::{
    write_base_dense_node_local, write_base_node_local, write_base_relation_local,
    write_base_way_local, write_base_way_local_with_locations, write_osc_relation,
    write_osc_way, write_osc_way_local,
};
use super::node_locations::NodeLocationIndex;
use super::parallel_reader::spawn_parallel_reader;
use super::stats::{ClassifyCounters, MergeStats, PhaseTimers, StallAccumulator};
#[cfg(feature = "hotpath")]
use super::stats::{PhaseRss, read_rss_kb};

use super::Result;

/// Merge-specific batch byte budget. Larger than the shared `BATCH_BYTE_BUDGET`
/// (128 MB) so that per-batch `par_iter` has enough overlap blobs to saturate
/// rayon workers. At planet the shared budget capped batches at ~12 blobs/~6
/// overlap, limiting classify parallelism to ~4.15 cores of 22 available.
/// 512 MB raises the cap to ~25 overlap blobs per batch without changing
/// other commands' memory profile.
const MERGE_BATCH_BYTE_BUDGET: usize = 512 * 1024 * 1024;

/// Accumulated locations-on-ways statistics (populated during pre-scan).
#[derive(Default)]
struct LocStats {
    needed: u64,
    from_diff: u64,
    from_base: u64,
    missing: u64,
    blobs_scanned: u64,
}

// ---------------------------------------------------------------------------
// Header handling
// ---------------------------------------------------------------------------

#[allow(clippy::redundant_closure_for_method_calls)]
fn build_header_bytes(
    header: &crate::HeaderBlock,
    locations_on_ways: bool,
    overrides: &HeaderOverrides,
) -> Result<Vec<u8>> {
    if locations_on_ways {
        if !header.has_locations_on_ways() {
            return Err(
                "merge --locations-on-ways requires the base PBF to have LocationsOnWays. \
                 Run add-locations-to-ways first to bootstrap coordinates."
                    .into(),
            );
        }
        if !header.is_sorted() {
            return Err(
                "merge --locations-on-ways requires a sorted base PBF (Sort.Type_then_ID). \
                 All nodes must precede all ways in the file."
                    .into(),
            );
        }
        build_output_header(header, false, overrides, |hb| {
            hb.sorted().optional_feature("LocationsOnWays")
        })
    } else {
        crate::commands::warn_locations_on_ways_loss(header);
        build_output_header(header, false, overrides, |hb| hb.sorted())
    }
}

/// Read the OSMHeader blob from a base PBF and return rebuilt header bytes.
fn read_header(
    base_pbf: &Path,
    direct_io: bool,
    locations_on_ways: bool,
    overrides: &HeaderOverrides,
) -> Result<Vec<u8>> {
    let mut reader = FileReader::open(base_pbf, direct_io)?;
    let mut offset: u64 = 0;
    loop {
        match read_raw_frame(&mut reader, &mut offset)? {
            Some(frame) if frame.blob_type == BlobKind::OsmHeader => {
                let header = decode_blob_to_headerblock(frame.blob_bytes())?;
                return build_header_bytes(&header, locations_on_ways, overrides);
            }
            Some(_) => {}
            None => return Err("base PBF has no OSMHeader blob".into()),
        }
    }
}

/// Collect a byte-budgeted batch of raw frames from the reader channel.
/// Returns the estimated in-flight byte cost of the batch.
///
/// When the channel drains before the batch is big enough to start work, the
/// blocking `recv()` fallback is wrapped in a `WAIT_CONSUMER_RECV_*` span and
/// its elapsed time goes into `stalls.consumer_recv_us`. This is how we
/// distinguish "reader thread is the bottleneck" (big consumer_recv_us, zero
/// reader_send_us) from "consumer is the bottleneck" (the inverse) without
/// guessing from wall-clock.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn collect_batch(
    frame_rx: &mpsc::Receiver<RawBlobFrame>,
    ranges: &DiffRanges,
    batch: &mut Vec<RawBlobFrame>,
    stalls: &StallAccumulator,
) -> usize {
    use super::classify::estimate_blob_cost;
    batch.clear();
    let mut batch_bytes: usize = 0;
    while batch.len() < BATCH_MAX_BLOBS {
        if batch.len() >= BATCH_MIN_BLOBS && batch_bytes >= MERGE_BATCH_BYTE_BUDGET {
            break;
        }
        match frame_rx.try_recv() {
            Ok(frame) => {
                batch_bytes += estimate_blob_cost(&frame, ranges);
                batch.push(frame);
            }
            Err(mpsc::TryRecvError::Empty) => {
                if batch.is_empty() {
                    crate::debug::emit_marker("WAIT_CONSUMER_RECV_START");
                    let t0 = std::time::Instant::now();
                    let res = frame_rx.recv();
                    let elapsed_us = u64::try_from(t0.elapsed().as_micros())
                        .unwrap_or(u64::MAX);
                    stalls.consumer_recv_us.fetch_add(
                        elapsed_us,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    crate::debug::emit_marker("WAIT_CONSUMER_RECV_END");
                    match res {
                        Ok(frame) => {
                            batch_bytes += estimate_blob_cost(&frame, ranges);
                            batch.push(frame);
                        }
                        Err(_) => break,
                    }
                } else {
                    break;
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => break,
        }
    }
    batch_bytes
}

// ---------------------------------------------------------------------------
// Parallel rewrite
// ---------------------------------------------------------------------------

/// Output from `rewrite_block_parallel`: serialized blocks + local stats.
pub(super) struct RewriteOutput {
    pub(super) blocks: Vec<OwnedBlock>,
    pub(super) stats: MergeStats,
}

/// Emit a single create element into the local BlockBuilder.
#[allow(clippy::too_many_arguments)]
fn emit_create_local(
    id: i64,
    kind: ElemKind,
    diff: &CompactDiffOverlay,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    stats: &mut MergeStats,
    loc_map: Option<&FxHashMap<i64, (i32, i32)>>,
) -> Result<()> {
    match kind {
        ElemKind::Node => {
            if let Some(osc) = diff.get_node(id) {
                ensure_node_capacity_local(bb, output)?;
                bb.add_node(osc.id(), osc.decimicro_lat(), osc.decimicro_lon(), osc.tags(), None);
                stats.diff_nodes += 1;
            }
        }
        ElemKind::Way => {
            if let Some(osc) = diff.get_way(id) {
                write_osc_way_local(bb, output, &osc, loc_map, stats)?;
                stats.diff_ways += 1;
            }
        }
        ElemKind::Relation => {
            if let Some(osc) = diff.get_relation(id) {
                ensure_relation_capacity_local(bb, output)?;
                let members: Vec<MemberData<'_>> = osc
                    .members()
                    .map(|(mt, ref_id, role)| MemberData {
                        id: crate::MemberId::from_id_and_type(ref_id, mt),
                        role,
                    })
                    .collect();
                bb.add_relation(osc.id(), osc.tags(), &members, None);
                stats.diff_relations += 1;
            }
        }
    }
    Ok(())
}

/// Rewrite a block in parallel: same element-by-element logic as `rewrite_block`,
/// but flushes to local `Vec<Vec<u8>>` instead of `PbfWriter`. Interleaves
/// upserts at their sorted positions within the block - IDs that match base
/// elements are modifications (handled by normal element processing); IDs that
/// don't match are creates (emitted by the cursor).
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn rewrite_block_parallel(
    block: &PrimitiveBlock,
    diff: &CompactDiffOverlay,
    bb: &mut BlockBuilder,
    inline_upserts: &[i64],
    kind: ElemKind,
    loc_map: Option<&FxHashMap<i64, (i32, i32)>>,
) -> Result<RewriteOutput> {
    let mut output: Vec<OwnedBlock> = Vec::new();
    let mut stats = MergeStats::new();
    let mut upsert_cursor: usize = 0;

    bb.pre_seed_string_table(block);

    for element in block.elements() {
        let elem_id = match &element {
            Element::DenseNode(dn) => dn.id(),
            Element::Node(n) => n.id(),
            Element::Way(w) => w.id(),
            Element::Relation(r) => r.id(),
        };

        // Emit creates (upsert IDs not in base block) before this element
        while upsert_cursor < inline_upserts.len() && crate::osm_id::osm_id_cmp(inline_upserts[upsert_cursor], elem_id).is_lt() {
            let cid = inline_upserts[upsert_cursor];
            upsert_cursor += 1;
            emit_create_local(cid, kind, diff, bb, &mut output, &mut stats, loc_map)?;
        }
        // Skip modification IDs (handled below by normal element processing)
        if upsert_cursor < inline_upserts.len() && inline_upserts[upsert_cursor] == elem_id {
            upsert_cursor += 1;
        }

        match &element {
            Element::DenseNode(dn) => {
                let id = dn.id();
                if diff.deleted_nodes.contains(&id) {
                    stats.deleted += 1;
                } else if let Some(osc) = diff.get_node(id) {
                    ensure_node_capacity_local(bb, &mut output)?;
                    bb.add_node(osc.id(), osc.decimicro_lat(), osc.decimicro_lon(), osc.tags(), None);

                    stats.diff_nodes += 1;
                } else {
                    write_base_dense_node_local(bb, &mut output, dn, block)?;
                    stats.base_nodes += 1;
                }
            }
            Element::Node(n) => {
                let id = n.id();
                if diff.deleted_nodes.contains(&id) {
                    stats.deleted += 1;
                } else if let Some(osc) = diff.get_node(id) {
                    ensure_node_capacity_local(bb, &mut output)?;
                    bb.add_node(osc.id(), osc.decimicro_lat(), osc.decimicro_lon(), osc.tags(), None);

                    stats.diff_nodes += 1;
                } else {
                    write_base_node_local(bb, &mut output, n, block)?;
                    stats.base_nodes += 1;
                }
            }
            Element::Way(w) => {
                let id = w.id();
                if diff.deleted_ways.contains(&id) {
                    stats.deleted += 1;
                } else if let Some(osc) = diff.get_way(id) {
                    write_osc_way_local(bb, &mut output, &osc, loc_map, &mut stats)?;
                    stats.diff_ways += 1;
                } else if loc_map.is_some() {
                    // Forward existing raw lat/lon data for LocationsOnWays
                    write_base_way_local_with_locations(bb, &mut output, w, block)?;
                    stats.base_ways += 1;
                } else {
                    write_base_way_local(bb, &mut output, w, block)?;
                    stats.base_ways += 1;
                }
            }
            Element::Relation(r) => {
                let id = r.id();
                if diff.deleted_relations.contains(&id) {
                    stats.deleted += 1;
                } else if let Some(osc) = diff.get_relation(id) {
                    ensure_relation_capacity_local(bb, &mut output)?;
                    let members: Vec<MemberData<'_>> = osc
                        .members()
                        .map(|(mt, ref_id, role)| MemberData {
                            id: crate::MemberId::from_id_and_type(ref_id, mt),
                            role,
                        })
                        .collect();
                    bb.add_relation(osc.id(), osc.tags(), &members, None);

                    stats.diff_relations += 1;
                } else {
                    write_base_relation_local(bb, &mut output, r, block)?;
                    stats.base_relations += 1;
                }
            }
        }
    }

    // Emit remaining upserts after the last element (trailing creates)
    while upsert_cursor < inline_upserts.len() {
        let cid = inline_upserts[upsert_cursor];
        upsert_cursor += 1;
        emit_create_local(cid, kind, diff, bb, &mut output, &mut stats, loc_map)?;
    }

    // Flush remaining elements in the BlockBuilder
    flush_local(bb, &mut output)?;

    Ok(RewriteOutput {
        blocks: output,
        stats,
    })
}

// ---------------------------------------------------------------------------
// Gap-create emitter for Phase 4 sequential output
// ---------------------------------------------------------------------------

/// Emit a single create element via PbfWriter (for gap creates and trailing creates).
#[allow(clippy::too_many_arguments)]
fn emit_create_for_output(
    id: i64,
    kind: ElemKind,
    diff: &CompactDiffOverlay,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc_map: Option<&FxHashMap<i64, (i32, i32)>>,
) -> Result<()> {
    match kind {
        ElemKind::Node => {
            if let Some(osc) = diff.get_node(id) {
                ensure_node_capacity(bb, writer)?;
                bb.add_node(osc.id(), osc.decimicro_lat(), osc.decimicro_lon(), osc.tags(), None);
                stats.diff_nodes += 1;
            }
        }
        ElemKind::Way => {
            if let Some(osc) = diff.get_way(id) {
                write_osc_way(bb, writer, &osc, loc_map, stats)?;
                stats.diff_ways += 1;
            }
        }
        ElemKind::Relation => {
            if let Some(osc) = diff.get_relation(id) {
                write_osc_relation(bb, writer, &osc)?;
                stats.diff_relations += 1;
            }
        }
    }
    Ok(())
}

/// Flush remaining upserts for the previous element type during a type
/// transition. Also handles skipped types (e.g., Node -> Relation flushes
/// all Way upserts).
#[allow(clippy::too_many_arguments)]
fn flush_remaining_upserts(
    prev: ElemKind,
    next: ElemKind,
    ranges: &DiffRanges,
    diff: &CompactDiffOverlay,
    cursors: &mut UpsertCursors,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc_map: Option<&FxHashMap<i64, (i32, i32)>>,
) -> Result<()> {
    // Flush remaining creates of the previous type
    let (cursor, upserts) = cursors.get_mut(prev, ranges);
    while *cursor < upserts.len() {
        emit_create_for_output(upserts[*cursor], prev, diff, bb, writer, stats, loc_map)?;
        *cursor += 1;
    }
    flush_block(bb, writer)?;

    // Handle skipped type: Node -> Relation (flush all Way upserts)
    if prev == ElemKind::Node && next == ElemKind::Relation {
        let (cursor, upserts) = cursors.get_mut(ElemKind::Way, ranges);
        while *cursor < upserts.len() {
            emit_create_for_output(upserts[*cursor], ElemKind::Way, diff, bb, writer, stats, loc_map)?;
            *cursor += 1;
        }
        flush_block(bb, writer)?;
    }

    Ok(())
}

/// Emit gap creates: upsert IDs of the current type that fall before a blob's min_id.
#[allow(clippy::too_many_arguments)]
fn emit_gap_creates(
    blob_kind: ElemKind,
    min_id: i64,
    ranges: &DiffRanges,
    diff: &CompactDiffOverlay,
    cursors: &mut UpsertCursors,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc_map: Option<&FxHashMap<i64, (i32, i32)>>,
) -> Result<()> {
    let (cursor, upserts) = cursors.get_mut(blob_kind, ranges);
    while *cursor < upserts.len() && crate::osm_id::osm_id_cmp(upserts[*cursor], min_id).is_lt() {
        emit_create_for_output(upserts[*cursor], blob_kind, diff, bb, writer, stats, loc_map)?;
        *cursor += 1;
    }
    Ok(())
}

/// Append a passthrough blob's raw bytes to the coalescing buffer.
/// For indexed blobs, moves frame_bytes via std::mem::take (zero copy).
/// For non-indexed blobs, reframes with indexdata first.
fn coalesce_passthrough(
    frame: &mut RawBlobFrame,
    index: &BlobIndex,
    has_indexdata: bool,
    chunks: &mut Vec<Vec<u8>>,
) -> Result<()> {
    if has_indexdata {
        chunks.push(std::mem::take(&mut frame.frame_bytes));
    } else {
        let indexdata = index.serialize();
        let reframed = crate::write::writer::reframe_raw_with_index(
            frame.blob_bytes(),
            &indexdata,
            frame.tagdata.as_deref(),
        )?;
        chunks.push(reframed);
    }
    Ok(())
}

/// Check whether there are gap creates to emit before min_id (without mutating cursors).
fn has_gap_creates(
    blob_kind: ElemKind,
    min_id: i64,
    ranges: &DiffRanges,
    cursors: &UpsertCursors,
) -> bool {
    let (cursor, upserts) = cursors.get(blob_kind, ranges);
    cursor < upserts.len() && crate::osm_id::osm_id_cmp(upserts[cursor], min_id).is_lt()
}

// ---------------------------------------------------------------------------
// Public merge function
// ---------------------------------------------------------------------------

/// Options controlling merge I/O and compression behavior.
pub struct MergeOptions {
    pub compression: Compression,
    pub direct_io: bool,
    pub io_uring: bool,
    pub force: bool,
    pub locations_on_ways: bool,
}

/// Apply an OSC diff to a base PBF file, producing an updated sorted PBF.
///
/// Single-pass streaming batch pipeline: for each byte-budgeted batch of raw frames,
/// Phase 1 classifies blobs in parallel, Phase 2 computes inline upsert
/// assignments (O(log n) per blob), then Phase 3+4 spawns parallel rewrites and
/// streams output in file order as results arrive.
///
/// # Errors
///
/// Returns an error if the base PBF or OSC file cannot be read, the output
/// file cannot be written, or if any PBF parsing/encoding fails.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity, clippy::cast_precision_loss)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub fn merge(
    base_pbf: &Path,
    osc_file: &Path,
    output_pbf: &Path,
    opts: &MergeOptions,
    overrides: &HeaderOverrides,
) -> Result<MergeStats> {
    let MergeOptions { compression, direct_io, io_uring, force, locations_on_ways } = *opts;
    require_indexdata(base_pbf, direct_io, force,
        "base PBF has no blob-level indexdata. Without indexdata, every blob must be \
         decompressed to classify elements (significantly slower).")?;

    let mut phase_timers = PhaseTimers::new();
    let stalls = Arc::new(StallAccumulator::new());
    let classify_counters = Arc::new(ClassifyCounters::new());

    // Step 1: Parse the diff
    crate::debug::emit_marker("MERGE_DIFFPARSE_START");
    let osc_start = std::time::Instant::now();
    eprintln!("Parsing OSC diff: {}", osc_file.display());
    let diff = Arc::new(parse_osc_file(osc_file)?);
    eprintln!(
        "Diff: {} nodes, {} ways, {} relations ({} del nodes, {} del ways, {} del rels)",
        diff.node_count(), diff.way_count(), diff.relation_count(),
        diff.deleted_nodes.len(), diff.deleted_ways.len(), diff.deleted_relations.len(),
    );
    let diff_heap_bytes = diff.heap_size_estimate() as u64;
    eprintln!(
        "CompactDiffOverlay heap estimate: {:.1} MB",
        diff_heap_bytes as f64 / (1024.0 * 1024.0),
    );
    phase_timers.osc_parse = osc_start.elapsed();
    #[cfg(feature = "hotpath")]
    let mut phase_rss = PhaseRss::new();
    #[cfg(feature = "hotpath")]
    {
        phase_rss.after_osc_parse = read_rss_kb();
    }

    crate::debug::emit_marker("MERGE_DIFFPARSE_END");

    // Step 2: Pre-compute sorted ID ranges for fast overlap checking
    crate::debug::emit_marker("MERGE_DIFFRANGES_START");
    let diffranges_start = std::time::Instant::now();
    let ranges = Arc::new(DiffRanges::from_diff(&diff));
    phase_timers.diffranges = diffranges_start.elapsed();
    crate::debug::emit_marker("MERGE_DIFFRANGES_END");
    eprintln!(
        "Diff ID ranges: {} node IDs, {} way IDs, {} rel IDs",
        ranges.node_ids.len(), ranges.way_ids.len(), ranges.rel_ids.len(),
    );

    // Step 2.5: Build sparse node location index for --locations-on-ways
    // Pre-scan base PBF to fill all needed node coordinates upfront, then
    // wrap in Arc for read-only sharing across all rewrite tasks (no
    // per-batch cloning).
    let (loc_map, loc_stats) = if locations_on_ways {
        crate::debug::emit_marker("MERGE_PREFILL_START");
        let prefill_start = std::time::Instant::now();
        let mut idx = NodeLocationIndex::build_from_diff(&diff);
        let (from_base, blobs_scanned) = idx.prefill_from_base(base_pbf, direct_io)?;
        phase_timers.prefill = prefill_start.elapsed();
        crate::debug::emit_marker("MERGE_PREFILL_END");
        let missing = idx.needed_set.len() as u64;
        let total = idx.locations.len() as u64 + missing;
        let from_diff = total - from_base - missing;
        eprintln!(
            "  {from_base} from base ({blobs_scanned} blobs), {missing} not found"
        );
        let stats = LocStats { needed: total, from_diff, from_base, missing, blobs_scanned };
        (Some(Arc::new(idx.locations)), stats)
    } else {
        (None, LocStats::default())
    };

    // Step 3: Read header from base PBF (for writer setup)
    crate::debug::emit_marker("MERGE_HEADER_READ_START");
    let header_start = std::time::Instant::now();
    let header_bytes = read_header(base_pbf, direct_io, locations_on_ways, overrides)?;
    phase_timers.header_read = header_start.elapsed();
    crate::debug::emit_marker("MERGE_HEADER_READ_END");

    // Step 4: Create pipelined writer
    crate::debug::emit_marker("MERGE_WRITER_SETUP_START");
    let writer_setup_start = std::time::Instant::now();
    let mut writer = writer_from_header_bytes(
        output_pbf,
        compression,
        &header_bytes,
        direct_io,
        io_uring,
    )?;
    phase_timers.writer_setup = writer_setup_start.elapsed();
    crate::debug::emit_marker("MERGE_WRITER_SETUP_END");

    // Step 5: Spawn parallel reader (header-only schedule + pread worker pool
    // + reorder pump). Replaces the sequential reader-thread: at planet, the
    // old reader capped at ~1400 frames/s, which in turn capped classify
    // batches at ~12 blobs/batch and left rayon cores idle. See
    // notes/apply-changes-opportunities.md plan item #3.
    crate::debug::emit_marker("MERGE_LOOP_START");
    let (reader_thread, frame_rx) =
        spawn_parallel_reader(base_pbf, direct_io, Arc::clone(&stalls));

    // Open second handle for copy_file_range.
    // The main thread owns the primary FileReader; this handle provides the fd
    // for kernel-space copy (copy_file_range uses explicit offsets, thread-safe).
    #[cfg(feature = "linux-direct-io")]
    let (_copy_fd_file, input_fd, use_copy_range) = {
        let f = FileReader::buffered(base_pbf)?;
        let fd = f.raw_fd();
        (f, fd, io_uring || !direct_io)
    };
    #[cfg(not(feature = "linux-direct-io"))]
    let (_input_fd, _use_copy_range) = (0i32, false);

    let mut bb = BlockBuilder::new();
    let mut stats = MergeStats::new();
    stats.diff_heap_bytes = diff_heap_bytes;
    let mut blob_count: u64 = 0;

    let mut cursors = UpsertCursors::new();
    let mut last_type: Option<ElemKind> = None;

    let mut batch: Vec<RawBlobFrame> = Vec::with_capacity(BATCH_MAX_BLOBS);
    // Passthrough coalescing buffer: accumulates consecutive raw passthrough bytes
    // and flushes them as a single write_raw_owned (move, no copy) to the
    // pipelined writer. At ~92% passthrough (Denmark), this collapses thousands
    // of individual channel sends into far fewer.
    let mut passthrough_chunks: Vec<Vec<u8>> = Vec::new();

    let mut batch_count: u64 = 0;
    loop {
        let batch_bytes = collect_batch(&frame_rx, &ranges, &mut batch, &stalls);
        if batch.is_empty() {
            break;
        }
        batch_count += 1;

        // Phase 1: Parallel classify
        let phase1_start = std::time::Instant::now();
        let classify_results: Vec<std::result::Result<ClassifyResult, String>> = batch
            .par_iter()
            .map_init(
                Vec::new,
                |buf, frame| classify_only(frame, &ranges, buf, &classify_counters),
            )
            .collect();
        phase_timers.classify_total += phase1_start.elapsed();
        #[cfg(feature = "hotpath")]
        {
            let rss = read_rss_kb();
            if rss > phase_rss.classify_max {
                phase_rss.classify_max = rss;
            }
        }

        // Phase 2: Sequential inline upsert assignment (O(log n) per blob)
        let phase2_start = std::time::Instant::now();
        let mut slots: Vec<BatchSlot> = Vec::with_capacity(batch.len());
        let mut rewrite_jobs: Vec<RewriteJob> = Vec::new();

        for result in classify_results {
            let result = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            match result {
                ClassifyResult::Passthrough(index, has_indexdata) => {
                    slots.push(BatchSlot::Passthrough { index, has_indexdata });
                }
                ClassifyResult::FalsePositive(index, has_indexdata) => {
                    slots.push(BatchSlot::FalsePositive { index, has_indexdata });
                }
                ClassifyResult::NeedsRewrite(block, index) => {
                    // Binary search for inline upserts in blob's OSM-order range
                    let upserts = ranges.upserts(index.kind);
                    let first = crate::osm_id::blob_osm_first_key(index.min_id, index.max_id);
                    let last = crate::osm_id::blob_osm_last_key(index.min_id, index.max_id);
                    let start = upserts.partition_point(|&id| crate::osm_id::osm_id_key(id) < first);
                    let end = upserts[start..].partition_point(|&id| crate::osm_id::osm_id_key(id) <= last) + start;

                    let job_idx = rewrite_jobs.len();
                    rewrite_jobs.push(RewriteJob {
                        block,
                        kind: index.kind,
                        upsert_range: (start, end),
                    });
                    slots.push(BatchSlot::Rewrite { job_index: job_idx, index });
                }
            }
        }

        phase_timers.phase2_inline_total += phase2_start.elapsed();

        // Location index is pre-filled and immutable - just reference it.

        // Phase 3+4: Spawn parallel rewrites, then stream output in file order.
        // Each rayon task owns its RewriteJob (including PrimitiveBlock), freeing
        // memory as soon as the task completes rather than holding all blocks until
        // all rewrites finish. The main thread processes slots in order, receiving
        // rewrite results from the channel on demand.
        let phase34_start = std::time::Instant::now();

        let rewrite_count = rewrite_jobs.len();
        let (rewrite_tx, rewrite_rx) =
            mpsc::sync_channel::<(usize, std::result::Result<RewriteOutput, String>)>(
                rayon::current_num_threads().min(rewrite_count.max(1)),
            );

        let spawn_start = std::time::Instant::now();
        for (job_idx, job) in rewrite_jobs.into_iter().enumerate() {
            let tx = rewrite_tx.clone();
            let diff_clone = Arc::clone(&diff);
            let ranges_clone = Arc::clone(&ranges);
            let loc_clone = if job.kind == ElemKind::Way { loc_map.clone() } else { None };
            rayon::spawn(move || {
                let mut task_bb = BlockBuilder::new();
                let upserts = ranges_clone.upserts(job.kind);
                let inline_slice = &upserts[job.upsert_range.0..job.upsert_range.1];
                let result = rewrite_block_parallel(
                    &job.block,
                    &diff_clone,
                    &mut task_bb,
                    inline_slice,
                    job.kind,
                    loc_clone.as_deref(),
                )
                .map_err(|e| e.to_string());
                // job (PrimitiveBlock) dropped here - freed before other tasks finish
                drop(tx.send((job_idx, result)));
            });
        }
        drop(rewrite_tx); // close channel when all cloned senders are done
        phase_timers.rewrite_spawn_total += spawn_start.elapsed();

        // Streaming drain: process slots in file order, receiving rewrite results
        // from the channel as needed. Out-of-order arrivals are buffered in
        // `received` and consumed when their slot is reached.
        let mut received: Vec<Option<RewriteOutput>> =
            (0..rewrite_count).map(|_| None).collect();

        for (i, slot) in slots.iter().enumerate() {
            blob_count += 1;

            let (blob_kind, min_id, max_id) = match slot {
                BatchSlot::Passthrough { index, .. }
                | BatchSlot::FalsePositive { index, .. }
                | BatchSlot::Rewrite { index, .. } => {
                    (index.kind, index.min_id, index.max_id)
                }
            };

            // Handle type transitions: flush remaining upserts of previous type(s)
            if let Some(prev) = last_type
                && prev != blob_kind
            {
                flush_passthrough_buf(&mut passthrough_chunks, &mut writer)?;
                let lm = loc_map.as_deref();
                flush_remaining_upserts(
                    prev, blob_kind, &ranges, &diff,
                    &mut cursors, &mut bb, &mut writer, &mut stats, lm,
                )?;
            }
            last_type = Some(blob_kind);

            // Gap creates: emit upserts before this blob in OSM order
            let osm_first = crate::osm_id::blob_osm_first_id(min_id, max_id);
            let has_gap = has_gap_creates(blob_kind, osm_first, &ranges, &cursors);
            if has_gap {
                flush_passthrough_buf(&mut passthrough_chunks, &mut writer)?;
                let lm = loc_map.as_deref();
                emit_gap_creates(
                    blob_kind, osm_first, &ranges,
                    &diff, &mut cursors, &mut bb, &mut writer, &mut stats, lm,
                )?;
                flush_block(&mut bb, &mut writer)?;
            }

            match slot {
                BatchSlot::Passthrough { index, has_indexdata }
                | BatchSlot::FalsePositive { index, has_indexdata } => {
                    // Coalesce: append raw frame bytes to passthrough buffer.
                    // For indexed blobs, take the frame bytes (zero-copy move).
                    // For non-indexed blobs, reframe with indexdata first.
                    #[cfg(feature = "linux-direct-io")]
                    if use_copy_range {
                        // copy_file_range path: flush coalesced buffer first,
                        // then do kernel-space copy (can't coalesce across copy_file_range)
                        let t_flush = std::time::Instant::now();
                        flush_passthrough_buf(&mut passthrough_chunks, &mut writer)?;
                        phase_timers.passthrough_write_total += t_flush.elapsed();
                        let t_copy = std::time::Instant::now();
                        writer.write_raw_copy(
                            input_fd,
                            batch[i].file_offset,
                            batch[i].frame_bytes.len() as u64,
                        )?;
                        let copy_us = u64::try_from(t_copy.elapsed().as_micros())
                            .unwrap_or(u64::MAX);
                        stalls.writer_call_us.fetch_add(
                            copy_us,
                            std::sync::atomic::Ordering::Relaxed,
                        );
                    }
                    #[cfg(feature = "linux-direct-io")]
                    if !use_copy_range {
                        coalesce_passthrough(
                            &mut batch[i], index, *has_indexdata,
                            &mut passthrough_chunks,
                        )?;
                    }
                    #[cfg(not(feature = "linux-direct-io"))]
                    coalesce_passthrough(
                        &mut batch[i], index, *has_indexdata,
                        &mut passthrough_chunks,
                    )?;

                    if matches!(slot, BatchSlot::Passthrough { has_indexdata: true, .. }) {
                        stats.blobs_index_hit += 1;
                    } else if matches!(slot, BatchSlot::Passthrough { .. }) {
                        stats.blobs_scan_only += 1;
                    }
                    match index.kind {
                        ElemKind::Node => stats.base_nodes += index.count,
                        ElemKind::Way => stats.base_ways += index.count,
                        ElemKind::Relation => stats.base_relations += index.count,
                    }
                    stats.blobs_passthrough += 1;
                    let frame_len = batch[i].frame_bytes.len() as u64;
                    stats.bytes_passthrough += frame_len;
                    #[allow(clippy::cast_possible_truncation)]
                    stats.blob_sizes.push(frame_len as u32);
                }
                BatchSlot::Rewrite { job_index, index: _ } => {
                    // Wait for this rewrite result, buffering out-of-order arrivals.
                    // Fast path: try_recv first. Only emit WAIT_* markers and
                    // accumulate stall time when we actually fall through to a
                    // blocking recv - otherwise a run with hundreds of finished-
                    // early rayon tasks would flood the sidecar with zero-width
                    // WAIT_REWRITE_RESULT pairs.
                    let recv_start = std::time::Instant::now();
                    let mut emitted_wait = false;
                    while received[*job_index].is_none() {
                        match rewrite_rx.try_recv() {
                            Ok((idx, result)) => {
                                received[idx] = Some(
                                    result.map_err(|e| -> Box<dyn std::error::Error> {
                                        e.into()
                                    })?,
                                );
                            }
                            Err(mpsc::TryRecvError::Empty) => {
                                if !emitted_wait {
                                    crate::debug::emit_marker("WAIT_REWRITE_RESULT_START");
                                    emitted_wait = true;
                                }
                                let t_block = std::time::Instant::now();
                                let (idx, result) = rewrite_rx.recv()
                                    .map_err(|_| -> Box<dyn std::error::Error> {
                                        "rewrite channel closed unexpectedly".into()
                                    })?;
                                let block_us = u64::try_from(t_block.elapsed().as_micros())
                                    .unwrap_or(u64::MAX);
                                stalls.rewrite_recv_us.fetch_add(
                                    block_us,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                                received[idx] = Some(
                                    result.map_err(|e| -> Box<dyn std::error::Error> {
                                        e.into()
                                    })?,
                                );
                            }
                            Err(mpsc::TryRecvError::Disconnected) => {
                                return Err("rewrite channel closed unexpectedly".into());
                            }
                        }
                    }
                    if emitted_wait {
                        crate::debug::emit_marker("WAIT_REWRITE_RESULT_END");
                    }
                    phase_timers.rewrite_recv_total += recv_start.elapsed();
                    let t_flush = std::time::Instant::now();
                    flush_passthrough_buf(&mut passthrough_chunks, &mut writer)?;
                    phase_timers.passthrough_write_total += t_flush.elapsed();
                    let mut output = received[*job_index]
                        .take()
                        .ok_or("rewrite output missing")?;
                    let mut rewrite_bytes: u64 = 0;
                    let t_write = std::time::Instant::now();
                    for (block_bytes, index, tagdata) in output.blocks.drain(..) {
                        rewrite_bytes += block_bytes.len() as u64;
                        writer.write_primitive_block_owned(block_bytes, index, tagdata.as_deref())?;
                    }
                    let write_elapsed = t_write.elapsed();
                    phase_timers.output_write_total += write_elapsed;
                    let write_us = u64::try_from(write_elapsed.as_micros())
                        .unwrap_or(u64::MAX);
                    stalls.writer_call_us.fetch_add(
                        write_us,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    stats.bytes_rewritten += rewrite_bytes;
                    stats.merge_from(&output.stats);
                    stats.blobs_rewritten += 1;
                    // output dropped here - RewriteOutput freed immediately

                    // Advance cursor past blob's OSM-last (inline upserts handled by rewrite)
                    let last = crate::osm_id::blob_osm_last_key(min_id, max_id);
                    let (cursor, upserts) = cursors.get_mut(blob_kind, &ranges);
                    while *cursor < upserts.len() && crate::osm_id::osm_id_key(upserts[*cursor]) <= last {
                        *cursor += 1;
                    }
                }
            }

            #[allow(clippy::cast_precision_loss)]
            if blob_count.is_multiple_of(500) {
                eprintln!(
                    "  Blob {blob_count}: {} pass ({} idx) / {} rewrite, {} elements, batch={} ({:.1} MB est)",
                    stats.blobs_passthrough, stats.blobs_index_hit,
                    stats.blobs_rewritten, stats.total_elements(),
                    batch.len(), batch_bytes as f64 / (1024.0 * 1024.0),
                );
            }
        }

        // Flush any remaining coalesced passthrough bytes at batch boundary
        let t_flush = std::time::Instant::now();
        flush_passthrough_buf(&mut passthrough_chunks, &mut writer)?;
        phase_timers.passthrough_write_total += t_flush.elapsed();
        // phase34_start bound - leave for reference; the per-step
        // accumulators above capture the actual rewrite/output/recv costs
        // directly. Retain only the RSS probe under hotpath.
        let _ = phase34_start;
        #[cfg(feature = "hotpath")]
        {
            let rss = read_rss_kb();
            if rss > phase_rss.rewrite_max {
                phase_rss.rewrite_max = rss;
            }
            if rss > phase_rss.output_max {
                phase_rss.output_max = rss;
            }
        }
    }

    // Join reader thread (should already be done since channel is drained)
    reader_thread
        .join()
        .map_err(|_| "reader thread panicked")?
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // Trailing creates: flush remaining upserts per type.
    // When last_type is None (no blobs at all), cursors are at 0 so all types
    // are flushed in full - equivalent to the previous dedicated else-branch.
    crate::debug::emit_marker("MERGE_TRAILING_CREATES_START");
    let trailing_start = std::time::Instant::now();
    let types_to_flush = match last_type {
        None | Some(ElemKind::Node) => &[ElemKind::Node, ElemKind::Way, ElemKind::Relation][..],
        Some(ElemKind::Way) => &[ElemKind::Way, ElemKind::Relation][..],
        Some(ElemKind::Relation) => &[ElemKind::Relation][..],
    };
    for &kind in types_to_flush {
        let (cursor, upserts) = cursors.get_mut(kind, &ranges);
        while *cursor < upserts.len() {
            let lm = loc_map.as_deref();
            emit_create_for_output(upserts[*cursor], kind, &diff, &mut bb, &mut writer, &mut stats, lm)?;
            *cursor += 1;
        }
        flush_block(&mut bb, &mut writer)?;
    }
    phase_timers.trailing_creates = trailing_start.elapsed();
    crate::debug::emit_marker("MERGE_TRAILING_CREATES_END");

    crate::debug::emit_marker("MERGE_FINAL_FLUSH_START");
    let flush_start = std::time::Instant::now();
    writer.flush()?;
    phase_timers.final_flush = flush_start.elapsed();
    crate::debug::emit_marker("MERGE_FINAL_FLUSH_END");
    crate::debug::emit_marker("MERGE_LOOP_END");
    #[cfg(feature = "hotpath")]
    {
        phase_rss.after_flush = read_rss_kb();
    }
    // Populate location stats from the index (if active)
    if loc_map.is_some() {
        stats.loc_nodes_needed = loc_stats.needed;
        stats.loc_nodes_from_diff = loc_stats.from_diff;
        stats.loc_nodes_from_base = loc_stats.from_base;
        stats.loc_missing = loc_stats.missing;
        stats.loc_node_blobs_scanned = loc_stats.blobs_scanned;
    }

    stats.print_summary();

    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    {
        // Shape counters.
        crate::debug::emit_counter("merge_blobs_passthrough", stats.blobs_passthrough as i64);
        crate::debug::emit_counter("merge_blobs_rewritten", stats.blobs_rewritten as i64);
        crate::debug::emit_counter("merge_blobs_index_hit", stats.blobs_index_hit as i64);
        crate::debug::emit_counter("merge_blobs_scan_only", stats.blobs_scan_only as i64);
        crate::debug::emit_counter("merge_total_elements", stats.total_elements() as i64);
        crate::debug::emit_counter("merge_deleted", stats.deleted as i64);
        crate::debug::emit_counter("merge_diff_nodes", stats.diff_nodes as i64);
        crate::debug::emit_counter("merge_diff_ways", stats.diff_ways as i64);
        crate::debug::emit_counter("merge_diff_relations", stats.diff_relations as i64);
        crate::debug::emit_counter("merge_batches_total", batch_count as i64);
        crate::debug::emit_counter("merge_bytes_passthrough", stats.bytes_passthrough as i64);
        crate::debug::emit_counter("merge_bytes_rewritten", stats.bytes_rewritten as i64);
        crate::debug::emit_counter("merge_diff_heap_bytes", stats.diff_heap_bytes as i64);

        // Phase wall times (ms). Every entry here is an always-on accumulator;
        // the sum approximates total `merge()` wall excluding pure framework
        // overhead. Divergence between sum-of-phases and MERGE total indicates
        // unaccounted-for work and should drive new phase markers.
        crate::debug::emit_counter(
            "merge_osc_parse_ms",
            phase_timers.osc_parse.as_millis() as i64,
        );
        crate::debug::emit_counter(
            "merge_diffranges_ms",
            phase_timers.diffranges.as_millis() as i64,
        );
        crate::debug::emit_counter(
            "merge_prefill_ms",
            phase_timers.prefill.as_millis() as i64,
        );
        crate::debug::emit_counter(
            "merge_header_read_ms",
            phase_timers.header_read.as_millis() as i64,
        );
        crate::debug::emit_counter(
            "merge_writer_setup_ms",
            phase_timers.writer_setup.as_millis() as i64,
        );
        crate::debug::emit_counter(
            "merge_classify_total_ms",
            phase_timers.classify_total.as_millis() as i64,
        );
        crate::debug::emit_counter(
            "merge_phase2_inline_total_ms",
            phase_timers.phase2_inline_total.as_millis() as i64,
        );
        crate::debug::emit_counter(
            "merge_rewrite_spawn_total_ms",
            phase_timers.rewrite_spawn_total.as_millis() as i64,
        );
        crate::debug::emit_counter(
            "merge_rewrite_recv_total_ms",
            phase_timers.rewrite_recv_total.as_millis() as i64,
        );
        crate::debug::emit_counter(
            "merge_output_write_total_ms",
            phase_timers.output_write_total.as_millis() as i64,
        );
        crate::debug::emit_counter(
            "merge_passthrough_write_total_ms",
            phase_timers.passthrough_write_total.as_millis() as i64,
        );
        crate::debug::emit_counter(
            "merge_trailing_creates_ms",
            phase_timers.trailing_creates.as_millis() as i64,
        );
        crate::debug::emit_counter(
            "merge_final_flush_ms",
            phase_timers.final_flush.as_millis() as i64,
        );

        // Stall attribution - each counter names the *blocking point*. Big
        // reader_send_us and small consumer_recv_us means consumer is the
        // bottleneck; the inverse means the reader thread is. writer_call_us
        // is cumulative time in writer.write_* calls, capturing compression
        // and queue-push backpressure from the merge perspective.
        crate::debug::emit_counter(
            "merge_reader_send_wait_us",
            stalls.reader_send_us.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "merge_consumer_recv_wait_us",
            stalls.consumer_recv_us.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "merge_rewrite_recv_wait_us",
            stalls.rewrite_recv_us.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "merge_writer_call_us",
            stalls.writer_call_us.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );

        // Classify per-path split: blob counts and cumulative CPU per sub-step
        // across rayon workers. Divide the ns counters by observed parallelism
        // (merge_classify_total_ms vs decode_threads) to back out wall share.
        crate::debug::emit_counter(
            "merge_blobs_classify_fastpath",
            classify_counters.blobs_fastpath.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "merge_blobs_classify_scan_pass",
            classify_counters.blobs_scan_pass.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "merge_blobs_classify_false_positive",
            classify_counters.blobs_false_positive.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "merge_blobs_classify_rewrite",
            classify_counters.blobs_rewrite.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "merge_classify_decompress_ns",
            classify_counters.decompress_ns.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "merge_classify_scan_ns",
            classify_counters.scan_ns.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "merge_classify_parse_ns",
            classify_counters.parse_ns.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "merge_classify_precise_ns",
            classify_counters.precise_ns.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );

        // Locations-on-ways - only populated when the flag is on.
        if loc_map.is_some() {
            crate::debug::emit_counter("merge_loc_needed", stats.loc_nodes_needed as i64);
            crate::debug::emit_counter("merge_loc_from_diff", stats.loc_nodes_from_diff as i64);
            crate::debug::emit_counter("merge_loc_from_base", stats.loc_nodes_from_base as i64);
            crate::debug::emit_counter("merge_loc_missing", stats.loc_missing as i64);
            crate::debug::emit_counter(
                "merge_loc_node_blobs_scanned",
                stats.loc_node_blobs_scanned as i64,
            );
        }
    }

    #[cfg(feature = "hotpath")]
    {
        eprintln!("osc_parse_ms={}", phase_timers.osc_parse.as_millis());
        eprintln!("diffranges_ms={}", phase_timers.diffranges.as_millis());
        eprintln!("prefill_ms={}", phase_timers.prefill.as_millis());
        eprintln!("header_read_ms={}", phase_timers.header_read.as_millis());
        eprintln!("writer_setup_ms={}", phase_timers.writer_setup.as_millis());
        eprintln!("classify_total_ms={}", phase_timers.classify_total.as_millis());
        eprintln!("phase2_inline_total_ms={}", phase_timers.phase2_inline_total.as_millis());
        eprintln!("rewrite_spawn_total_ms={}", phase_timers.rewrite_spawn_total.as_millis());
        eprintln!("rewrite_recv_total_ms={}", phase_timers.rewrite_recv_total.as_millis());
        eprintln!("output_write_total_ms={}", phase_timers.output_write_total.as_millis());
        eprintln!("passthrough_write_total_ms={}", phase_timers.passthrough_write_total.as_millis());
        eprintln!("trailing_creates_ms={}", phase_timers.trailing_creates.as_millis());
        eprintln!("final_flush_ms={}", phase_timers.final_flush.as_millis());
        eprintln!("phase_rss_after_osc_kb={}", phase_rss.after_osc_parse);
        eprintln!("phase_rss_classify_max_kb={}", phase_rss.classify_max);
        eprintln!("phase_rss_rewrite_max_kb={}", phase_rss.rewrite_max);
        eprintln!("phase_rss_output_max_kb={}", phase_rss.output_max);
        eprintln!("phase_rss_after_flush_kb={}", phase_rss.after_flush);
    }

    Ok(stats)
}
