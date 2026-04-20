//! Smart extraction strategy (three passes) + shared Pass 1 ID collection used by complete-ways.

use std::path::Path;

use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::cat::CleanAttrs;
use crate::writer::Compression;
use crate::{Element, MemberId, PrimitiveBlock};

use super::super::{Result, writer_from_header, HeaderOverrides,
    ensure_node_capacity_local, ensure_way_capacity_local, ensure_relation_capacity_local,
};
use crate::idset::IdSet;

use super::common::{
    BboxInt, BlobDesc, pread_write_pass, pread_write_pass_with_schedule,
    relation_has_matched_member, spatial_blob_filter,
};
use super::{ExtractStats, Region};

// ---------------------------------------------------------------------------
// Pass 1: Generic ID collection with pluggable relation handling
// ---------------------------------------------------------------------------

/// Output of pass 1 ID collection, shared between complete-ways and smart strategies.
pub(super) struct Pass1Result {
    pub(super) bbox_node_ids: IdSet,
    pub(super) matched_way_ids: IdSet,
    pub(super) all_way_node_ids: IdSet,
    pub(super) matched_relation_ids: IdSet,
    /// Unfiltered way blob schedule built during PASS1's manual scan, plumbed
    /// out so smart PASS2 can reuse it instead of calling
    /// `build_classify_schedule` again. Saves ~16% wall on Europe by avoiding
    /// a second 19-second header scan. Empty for the unsorted-fallback path
    /// (smart PASS2 falls back to `build_classify_schedule` in that case).
    pub(super) way_schedule: Vec<(usize, u64, usize)>,
    /// Full BlobDesc schedule for all OsmData blobs, built during PASS1's
    /// manual scan and used by smart/complete PASS3 instead of calling
    /// `build_blob_schedule` again. Eliminates a third post-PASS1 header scan
    /// (~28 seconds on Europe). Empty for the unsorted-fallback path.
    /// Post-PASS1 header scans trigger a cold-arena-page residency cascade
    /// that doesn't show up in glibc's accounting but does show up in
    /// anon RSS, so avoiding the rescan also helps peak memory at scale.
    pub(super) pass3_blob_schedule: Vec<BlobDesc>,
}

/// Strategy-specific relation handling for pass 1.
///
/// Implementations control what happens after a relation is matched:
/// - `CompleteRelationHandler`: no-op (just collects relation IDs)
/// - `SmartRelationHandler`: additionally collects way/node member IDs from
///   multipolygon/boundary relations
pub(super) trait RelationHandler {
    /// Whether workers should collect extra way/node member IDs from smart
    /// relations (multipolygon/boundary). Compile-time constant - the compiler
    /// eliminates the dead branch in `CompleteRelationHandler`.
    const COLLECT_MEMBER_IDS: bool;

    /// Process a single matched relation in the unsorted/mixed fallback path.
    /// Called after the relation ID has already been added to `matched_relation_ids`.
    fn handle_relation(&mut self, r: &crate::Relation);

    /// Merge extra way/node IDs from parallel workers (sorted path phase 3).
    fn merge_worker_extras(&mut self, extra_way_ids: IdSet, extra_node_ids: IdSet);
}

pub(super) struct CompleteRelationHandler;

impl RelationHandler for CompleteRelationHandler {
    const COLLECT_MEMBER_IDS: bool = false;

    fn handle_relation(&mut self, _r: &crate::Relation) {}

    fn merge_worker_extras(&mut self, _extra_way_ids: IdSet, _extra_node_ids: IdSet) {}
}

struct SmartRelationHandler {
    extra_way_ids: IdSet,
    extra_node_ids: IdSet,
}

impl SmartRelationHandler {
    fn new() -> Self {
        Self {
            extra_way_ids: IdSet::new(),
            extra_node_ids: IdSet::new(),
        }
    }
}

impl RelationHandler for SmartRelationHandler {
    const COLLECT_MEMBER_IDS: bool = true;

    fn handle_relation(&mut self, r: &crate::Relation) {
        if is_smart_relation(r) {
            for m in r.members() {
                match m.id {
                    MemberId::Way(id) => self.extra_way_ids.set(id),
                    MemberId::Node(id) => self.extra_node_ids.set(id),
                    MemberId::Relation(_) | MemberId::Unknown(_, _) => {}
                }
            }
        }
    }

    fn merge_worker_extras(&mut self, extra_way_ids: IdSet, extra_node_ids: IdSet) {
        self.extra_way_ids.merge(extra_way_ids);
        self.extra_node_ids.merge(extra_node_ids);
    }
}

/// Collect pass 1 ID sets with strategy-specific relation handling.
///
/// Reads all elements via sequential BlobReader + DecompressPool, collecting:
/// - `bbox_node_ids`: nodes within the bounding box
/// - `matched_way_ids`: ways referencing at least one bbox node
/// - `all_way_node_ids`: all node refs from matched ways (for pass 2)
/// - `matched_relation_ids`: relations with matched node/way members
///
/// The `handler` controls additional per-relation processing (e.g. smart
/// strategy collects extra way/node IDs from multipolygon/boundary relations).
#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
#[hotpath::measure]
pub(super) fn collect_pass1_generic<H: RelationHandler>(
    input: &Path,
    region: &Region,
    bbox_int: &BboxInt,
    direct_io: bool,
    handler: &mut H,
) -> Result<Pass1Result> {
    let mut bbox_node_ids = IdSet::new();
    let mut matched_way_ids = IdSet::new();
    let mut all_way_node_ids = IdSet::new();
    let mut matched_relation_ids = IdSet::new();

    // Sequential reader to avoid PrimitiveBlock cross-thread retention OOM.
    let mut blob_reader = crate::blob::BlobReader::open(input, direct_io)?;
    blob_reader.set_parse_indexdata(true);
    let header_blob = blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let is_sorted = header_blob.to_headerblock()?.is_sorted();
    let filter = spatial_blob_filter(bbox_int);
    let mut decompress_buf: Vec<u8> = Vec::new();

    if !is_sorted {
        for blob_result in &mut blob_reader {
            let blob = blob_result?;
            if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) { continue; }
            if let Some(idx) = blob.index() {
                if !filter.wants_index(&idx) { continue; }
            }
            blob.decompress_into(&mut decompress_buf)?;
            let block = PrimitiveBlock::from_vec(std::mem::take(&mut decompress_buf))?;
            for element in block.elements_skip_metadata() {
                match &element {
                    Element::DenseNode(dn)
                        if region.contains_decimicro(bbox_int, dn.decimicro_lat(), dn.decimicro_lon()) =>
                    {
                        bbox_node_ids.set(dn.id());
                    }
                    Element::Node(n)
                        if region.contains_decimicro(bbox_int, n.decimicro_lat(), n.decimicro_lon()) =>
                    {
                        bbox_node_ids.set(n.id());
                    }
                    Element::Way(w)
                        if w.refs().any(|r| bbox_node_ids.get(r)) =>
                    {
                        matched_way_ids.set(w.id());
                        for r in w.refs() {
                            all_way_node_ids.set(r);
                        }
                    }
                    Element::Relation(r)
                        if relation_has_matched_member(r, &bbox_node_ids, &matched_way_ids) =>
                    {
                        matched_relation_ids.set(r.id());
                        handler.handle_relation(r);
                    }
                    _ => {}
                }
            }
        }
        return Ok(Pass1Result {
            bbox_node_ids, matched_way_ids, all_way_node_ids, matched_relation_ids,
            way_schedule: Vec::new(),
            pass3_blob_schedule: Vec::new(),
        });
    }

    // Sorted path: parallel three-phase classification via pread-from-workers.
    // Phase 1: nodes (bbox check) → bbox_node_ids
    // Phase 2: ways (ref check against bbox_node_ids) → matched_way_ids + all_way_node_ids
    // Phase 3: relations (member check) → matched_relation_ids + handler extras
    drop(blob_reader);
    drop(decompress_buf);

    // Build per-type schedules from header-only scan.
    crate::debug::emit_marker("SMART_PASS1_SCHEDULE_SCAN_START");
    let mut scanner = crate::blob::BlobReader::seekable_from_path(input)?;
    scanner.set_parse_indexdata(true);
    scanner.next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

    let mut node_schedule: Vec<(usize, u64, usize)> = Vec::new();
    let mut way_schedule: Vec<(usize, u64, usize)> = Vec::new();
    let mut relation_schedule: Vec<(usize, u64, usize)> = Vec::new();
    // Unfiltered way schedule (no spatial filter): used by smart PASS2 to
    // find ways referenced by relations regardless of spatial location.
    // Returned via Pass1Result so smart PASS2 can skip its own
    // build_classify_schedule call entirely, saving ~16% wall on Europe
    // (one fewer 19-second header scan after PASS1's parallel work).
    let mut full_way_schedule: Vec<(usize, u64, usize)> = Vec::new();
    // Full BlobDesc schedule for all OsmData blobs, returned via Pass1Result
    // for smart/complete PASS3 to reuse. Eliminates the third post-PASS1
    // header scan (build_blob_schedule) and its associated cold-arena-page
    // residency cascade.
    let mut pass3_blob_schedule: Vec<BlobDesc> = Vec::new();
    let mut seq: usize = 0;
    while let Some(result_item) = scanner.next_header_with_data_offset() {
        let (hdr, frame_offset, data_offset, data_size) = result_item?;
        if !matches!(hdr.blob_type(), crate::blob::BlobType::OsmData) { continue; }
        let idx = hdr.index();
        // Build pass3_blob_schedule unconditionally for all OsmData blobs.
        // Mirrors build_blob_schedule's behavior (no kind/spatial filter,
        // raw_passthrough=false since smart/complete don't use the
        // passthrough optimization).
        let bbox = idx.as_ref().and_then(|i| i.bbox);
        let count = idx.as_ref().map_or(0, |i| i.count);
        let kind_for_blob = idx.as_ref().map(|i| i.kind);
        #[allow(clippy::cast_possible_truncation)]
        let frame_size = (data_offset - frame_offset) as usize + data_size;
        pass3_blob_schedule.push(BlobDesc {
            frame_offset,
            frame_size,
            offset: data_offset,
            size: data_size,
            kind: kind_for_blob,
            bbox,
            count,
            raw_passthrough: false,
        });
        if let Some(idx) = idx {
            // Build full_way_schedule unconditionally for way blobs.
            if matches!(idx.kind, crate::blob_meta::ElemKind::Way) {
                full_way_schedule.push((seq, data_offset, data_size));
            }
            if !filter.wants_index(&idx) { continue; }
            match idx.kind {
                crate::blob_meta::ElemKind::Node => node_schedule.push((seq, data_offset, data_size)),
                crate::blob_meta::ElemKind::Way => way_schedule.push((seq, data_offset, data_size)),
                crate::blob_meta::ElemKind::Relation => relation_schedule.push((seq, data_offset, data_size)),
            }
        }
        seq += 1;
    }
    drop(scanner);
    crate::debug::emit_marker("SMART_PASS1_SCHEDULE_SCAN_END");

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("smart_pass1_node_blobs", node_schedule.len() as i64);
        crate::debug::emit_counter("smart_pass1_way_blobs", way_schedule.len() as i64);
        crate::debug::emit_counter("smart_pass1_relation_blobs", relation_schedule.len() as i64);
        crate::debug::emit_counter("smart_pass1_full_way_blobs", full_way_schedule.len() as i64);
        crate::debug::emit_counter("smart_pass1_pass3_blobs", pass3_blob_schedule.len() as i64);
    }

    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?
    );

    // Phase 1: Classify nodes by region containment.
    // For bbox-only regions, use columnar decode (batch IDs/lats/lons into
    // contiguous arrays) for cache-friendly classification. Polygon regions
    // fall back to element-by-element iteration.
    crate::debug::emit_marker("PASS1_NODE_CLASSIFY_START");
    let use_columnar = matches!(region, Region::Bbox(_));
    crate::scan::classify::parallel_classify_phase(
        &shared_file,
        &node_schedule,
        None,
        || (crate::read::columnar::DenseNodeColumns::new(), Vec::<i64>::new()),
        |block, (columns, ids)| {
            ids.clear();
            if use_columnar {
                block.decode_dense_columns(columns);
                columns.collect_matching_ids_bbox(
                    bbox_int.min_lat, bbox_int.max_lat,
                    bbox_int.min_lon, bbox_int.max_lon,
                    ids,
                );
            } else {
                for element in block.elements_skip_metadata() {
                    match &element {
                        Element::DenseNode(dn)
                            if region.contains_decimicro(bbox_int, dn.decimicro_lat(), dn.decimicro_lon()) =>
                        {
                            ids.push(dn.id());
                        }
                        Element::Node(n)
                            if region.contains_decimicro(bbox_int, n.decimicro_lat(), n.decimicro_lon()) =>
                        {
                            ids.push(n.id());
                        }
                        _ => {}
                    }
                }
            }
            std::mem::take(ids)
        },
        |_seq, ids| {
            for id in ids { bbox_node_ids.set(id); }
        },
    )?;
    crate::debug::emit_marker("PASS1_NODE_CLASSIFY_END");

    // Phase 2: Classify ways by ref intersection with bbox nodes.
    crate::debug::emit_marker("PASS1_WAY_CLASSIFY_START");
    crate::scan::classify::parallel_classify_phase(
        &shared_file,
        &way_schedule,
        None,
        || (),
        |block, _s| {
            let mut way_ids = Vec::new();
            let mut node_ids = Vec::new();
            for element in block.elements_skip_metadata() {
                if let Element::Way(w) = &element {
                    if w.refs().any(|r| bbox_node_ids.get(r)) {
                        way_ids.push(w.id());
                        node_ids.extend(w.refs());
                    }
                }
            }
            (way_ids, node_ids)
        },
        |_seq, (way_ids, node_ids)| {
            for id in way_ids { matched_way_ids.set(id); }
            for id in node_ids { all_way_node_ids.set(id); }
        },
    )?;
    crate::debug::emit_marker("PASS1_WAY_CLASSIFY_END");

    // Phase 3: Classify relations by member intersection.
    crate::debug::emit_marker("PASS1_RELATION_CLASSIFY_START");
    let collect_member_ids = H::COLLECT_MEMBER_IDS;
    crate::scan::classify::parallel_classify_accumulate(
        &shared_file,
        &relation_schedule,
        None,
        || (IdSet::new(), IdSet::new(), IdSet::new()),
        |block, (rel_ids, extra_way_ids, extra_node_ids)| {
            for element in block.elements_skip_metadata() {
                if let Element::Relation(r) = &element {
                    if relation_has_matched_member(r, &bbox_node_ids, &matched_way_ids) {
                        rel_ids.set(r.id());
                        if collect_member_ids && is_smart_relation(r) {
                            for m in r.members() {
                                match m.id {
                                    MemberId::Way(id) => extra_way_ids.set(id),
                                    MemberId::Node(id) => extra_node_ids.set(id),
                                    MemberId::Relation(_) | MemberId::Unknown(_, _) => {}
                                }
                            }
                        }
                    }
                }
            }
        },
        |(worker_rel_ids, worker_extra_way_ids, worker_extra_node_ids)| {
            matched_relation_ids.merge(worker_rel_ids);
            handler.merge_worker_extras(worker_extra_way_ids, worker_extra_node_ids);
        },
    )?;
    crate::debug::emit_marker("PASS1_RELATION_CLASSIFY_END");

    Ok(Pass1Result {
        bbox_node_ids, matched_way_ids, all_way_node_ids, matched_relation_ids,
        way_schedule: full_way_schedule,
        pass3_blob_schedule,
    })
}

// ---------------------------------------------------------------------------
// Smart strategy (three passes)
// ---------------------------------------------------------------------------

#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) fn extract_smart(
    input: &Path,
    output: &Path,
    region: &Region,
    set_bounds: bool,
    clean: &CleanAttrs,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<ExtractStats> {
    let mut stats = ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_written: 0,
        ways_from_relations: 0,
        relations_written: 0,
        strategy: "smart",
    };

    // --- Pass 1: Collect matches + smart relation deps ---
    crate::debug::emit_marker("SMART_PASS1_START");
    let bbox_int = BboxInt::from_bbox(region.bbox());
    let mut handler = SmartRelationHandler::new();
    let mut result = collect_pass1_generic(input, region, &bbox_int, direct_io, &mut handler)?;
    let mut extra_node_ids = handler.extra_node_ids;
    crate::debug::emit_marker("SMART_PASS1_END");
    crate::debug::emit_mallinfo2("MI_PASS1_END");

    // --- Pass 2: Resolve extra way node deps (parallel pread) ---
    crate::debug::emit_marker("SMART_PASS2_START");
    // For each way in extra_way_ids not already in matched_way_ids,
    // collect all node refs into extra_node_ids.
    // Reuses PASS1's full_way_schedule (built during the manual header scan
    // in collect_pass1_generic) to avoid a second 19-second
    // build_classify_schedule call after PASS1's parallel work. Saves ~16%
    // wall on Europe extract-smart. Falls back to build_classify_schedule
    // for the unsorted-fallback path where PASS1 doesn't build a schedule.
    {
    crate::debug::emit_marker("SMART_PASS2_SCHEDULE_START");
    let pass1_way_schedule = std::mem::take(&mut result.way_schedule);
    let (way_schedule, shared_file) = if pass1_way_schedule.is_empty() {
        crate::scan::classify::build_classify_schedule(input, Some(crate::blob_meta::ElemKind::Way))?
    } else {
        let shared_file = std::sync::Arc::new(
            std::fs::File::open(input)
                .map_err(|e| format!("failed to open {}: {e}", input.display()))?
        );
        (pass1_way_schedule, shared_file)
    };
    crate::debug::emit_marker("SMART_PASS2_SCHEDULE_END");

    let extra_way_ids_ref = &handler.extra_way_ids;
    let matched_way_ids_ref = &result.matched_way_ids;
    // Per-blob send (not accumulate): extra_way_ids is relation-driven and
    // can be wide + globally dispersed. Per-worker IdSet accumulate over
    // an unbounded node-ID set is the unsafe shape the helper doc warns
    // against, regardless of whether this specific workload triggers the
    // worst case. The fix improves PASS2 wall by ~23%; the planet-scale
    // memory peak it was originally framed as fixing turned out to be
    // elsewhere (cold-arena-page residency cascade, addressed by the
    // PASS1 schedule-reuse commits `d4ea760` and `0b085b1`).
    crate::debug::emit_marker("SMART_PASS2_CLASSIFY_START");
    crate::scan::classify::parallel_classify_phase(
        &shared_file,
        &way_schedule,
        None,
        Vec::<i64>::new,
        |block, scratch| {
            scratch.clear();
            for element in block.elements_skip_metadata() {
                if let Element::Way(w) = &element {
                    let wid = w.id();
                    if extra_way_ids_ref.get(wid) && !matched_way_ids_ref.get(wid) {
                        for r in w.refs() { scratch.push(r); }
                    }
                }
            }
            std::mem::take(scratch)
        },
        |_seq, refs| {
            for id in refs { extra_node_ids.set(id); }
        },
    )?;
    crate::debug::emit_marker("SMART_PASS2_CLASSIFY_END");
    }

    crate::debug::emit_marker("SMART_PASS2_END");

    // --- Pass 3: Write matching elements via pread-from-workers ---
    crate::debug::emit_marker("SMART_PASS3_START");
    crate::debug::emit_marker("SMART_PASS3_SETUP_START");

    let mut header_reader = crate::blob::BlobReader::open(input, direct_io)?;
    let header_blob = header_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let header = header_blob.to_headerblock()?;
    drop(header_reader);
    super::super::warn_locations_on_ways_loss(&header);
    let bbox = region.bbox();
    let mut writer = writer_from_header(output, compression, &header, false, overrides, |hb| {
        let hb = if set_bounds {
            hb.bbox(bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat)
        } else {
            hb
        };
        hb.sorted()
    }, direct_io, false)?;

    // Take the pre-built blob schedule BEFORE creating `ids`, since `ids`
    // holds immutable borrows of `result` and we need a brief mutable borrow
    // here to mem::take the schedule.
    let pass1_blob_schedule = std::mem::take(&mut result.pass3_blob_schedule);

    let ids = ExtractPass3IdSets {
        bbox_node_ids: &result.bbox_node_ids,
        all_way_node_ids: &result.all_way_node_ids,
        extra_node_ids: &extra_node_ids,
        matched_way_ids: &result.matched_way_ids,
        extra_way_ids: &handler.extra_way_ids,
        matched_relation_ids: &result.matched_relation_ids,
    };

    crate::debug::emit_marker("SMART_PASS3_SETUP_END");
    crate::debug::emit_marker("SMART_PASS3_WRITE_START");
    // Reuse PASS1's pre-built blob schedule if available, falling back to
    // build_blob_schedule for the unsorted-fallback path. Avoids the third
    // post-PASS1 header scan and its cold-arena-page residency cascade.
    if pass1_blob_schedule.is_empty() {
        pread_write_pass(input, &mut writer, &mut stats, |block, bb, output_blocks| {
            extract_block_pass3(block, &ids, clean, bb, output_blocks)
        })?;
    } else {
        pread_write_pass_with_schedule(input, &pass1_blob_schedule, &mut writer, &mut stats, |block, bb, output_blocks| {
            extract_block_pass3(block, &ids, clean, bb, output_blocks)
        })?;
    }
    crate::debug::emit_marker("SMART_PASS3_WRITE_END");

    crate::debug::emit_marker("SMART_PASS3_END");
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Smart Pass 3: Parallel helpers
// ---------------------------------------------------------------------------

/// Read-only ID sets for Pass 3 of smart strategy, shared across rayon threads.
struct ExtractPass3IdSets<'a> {
    bbox_node_ids: &'a IdSet,
    all_way_node_ids: &'a IdSet,
    extra_node_ids: &'a IdSet,
    matched_way_ids: &'a IdSet,
    extra_way_ids: &'a IdSet,
    matched_relation_ids: &'a IdSet,
}

use super::super::clean_metadata;
use crate::owned::{dense_node_metadata, element_metadata};

/// Process a single block for Pass 3 of smart extraction: write elements whose IDs
/// were collected in Passes 1+2. Uses thread-local BlockBuilder and output buffer.
#[hotpath::measure]
fn extract_block_pass3(
    block: &PrimitiveBlock,
    ids: &ExtractPass3IdSets<'_>,
    clean: &CleanAttrs,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> std::result::Result<ExtractStats, String> {
    let mut stats = ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_written: 0,
        ways_from_relations: 0,
        relations_written: 0,
        strategy: "",
    };
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                let id = dn.id();
                let in_bbox = ids.bbox_node_ids.get(id);
                let from_way = ids.all_way_node_ids.get(id);
                let from_rel = ids.extra_node_ids.get(id);
                if in_bbox || from_way || from_rel {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = clean_metadata(dense_node_metadata(dn), clean);
                    bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), dn.tags(), meta.as_ref());
                    if in_bbox {
                        stats.nodes_in_bbox += 1;
                    } else if from_way {
                        stats.nodes_from_ways += 1;
                    } else {
                        stats.nodes_from_relations += 1;
                    }
                }
            }
            Element::Node(n) => {
                let id = n.id();
                let in_bbox = ids.bbox_node_ids.get(id);
                let from_way = ids.all_way_node_ids.get(id);
                let from_rel = ids.extra_node_ids.get(id);
                if in_bbox || from_way || from_rel {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = clean_metadata(element_metadata(&n.info()), clean);
                    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), n.tags(), meta.as_ref());
                    if in_bbox {
                        stats.nodes_in_bbox += 1;
                    } else if from_way {
                        stats.nodes_from_ways += 1;
                    } else {
                        stats.nodes_from_relations += 1;
                    }
                }
            }
            Element::Way(w) => {
                let in_matched = ids.matched_way_ids.get(w.id());
                let in_extra = ids.extra_way_ids.get(w.id());
                if in_matched || in_extra {
                    ensure_way_capacity_local(bb, output)?;
                    refs_buf.clear();
                    refs_buf.extend(w.refs());
                    let meta = clean_metadata(element_metadata(&w.info()), clean);
                    bb.add_way(w.id(), w.tags(), &refs_buf, meta.as_ref());
                    if in_extra && !in_matched {
                        stats.ways_from_relations += 1;
                    } else {
                        stats.ways_written += 1;
                    }
                }
            }
            Element::Relation(r) => {
                if ids.matched_relation_ids.get(r.id()) {
                    ensure_relation_capacity_local(bb, output)?;
                    members_buf.clear();
                    members_buf.extend(r.members().map(|m| MemberData {
                        id: m.id,
                        role: m.role().unwrap_or(""),
                    }));
                    let meta = clean_metadata(element_metadata(&r.info()), clean);
                    bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
                    stats.relations_written += 1;
                }
            }
        }
    }
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Relation type check
// ---------------------------------------------------------------------------

/// Returns true if the relation has a `type=multipolygon` or `type=boundary` tag.
///
/// These are the relation types whose way members should be fully included
/// in the smart extraction strategy, along with all nodes those ways reference.
fn is_smart_relation(r: &crate::Relation) -> bool {
    r.tags().any(|(k, v)| k == "type" && (v == "multipolygon" || v == "boundary"))
}
