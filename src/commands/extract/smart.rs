//! Smart extraction strategy (three passes) + shared Pass 1 ID collection used by complete-ways.

use crate::block_builder::{BlockBuilder, MemberData};
use crate::cat::CleanAttrs;
use crate::read::input::PinnedInput;
use crate::write::WriteOptions;
use crate::{Element, MemberId, PrimitiveBlock};

use super::super::{
    HeaderOverrides, Result, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, writer_from_header,
};
use crate::idset::IdSet;

use super::common::{
    BboxInt, BlobDesc, Passthrough, pread_write_pass, pread_write_pass_with_schedule,
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
    /// The node refs of matched ways that are not in `bbox_node_ids`. The
    /// nodes a writer includes are the union of the two, and every writer
    /// tests the region set first, so holding a ref in both bought nothing:
    /// at a large region nearly every way ref is in the region, and the full
    /// way-node set was a second node-ID-space-wide set (about 2.2 GB at
    /// planet for a Europe-sized box) alive from the way classify to the end.
    pub(super) outside_way_node_ids: IdSet,
    pub(super) matched_relation_ids: IdSet,
    /// Unfiltered way blob schedule built during PASS1's manual scan, plumbed
    /// out so smart PASS2 can reuse it instead of calling
    /// `build_classify_schedule` again. Saves ~16% wall on Europe by avoiding
    /// a second 19-second header scan. Empty for the unsorted-fallback path
    /// (smart PASS2 falls back to `build_classify_schedule` in that case).
    pub(super) way_schedule: Vec<crate::scan::classify::ScheduledBlob>,
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
    /// Whether pass 1 collects extra way/node member IDs from smart
    /// relations (multipolygon/boundary). Compile-time constant - the compiler
    /// eliminates the dead branch in `CompleteRelationHandler`.
    const COLLECT_MEMBER_IDS: bool;

    /// Take the member IDs [`Pass1Known::push_new_members`] collected,
    /// leaving both vectors empty.
    fn merge_extras(&mut self, way_ids: &mut Vec<i64>, node_ids: &mut Vec<i64>);
}

pub(super) struct CompleteRelationHandler;

impl RelationHandler for CompleteRelationHandler {
    const COLLECT_MEMBER_IDS: bool = false;

    fn merge_extras(&mut self, way_ids: &mut Vec<i64>, node_ids: &mut Vec<i64>) {
        way_ids.clear();
        node_ids.clear();
    }
}

/// Smart's relation-driven extras. Both sets are kept disjoint from what
/// pass 1 already includes (`extra_way_ids` from `matched_way_ids`,
/// `extra_node_ids` from the region and outside-way node sets): pass 3 tests
/// those first, so an ID in both changed nothing but the set's size.
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

    fn merge_extras(&mut self, way_ids: &mut Vec<i64>, node_ids: &mut Vec<i64>) {
        for id in way_ids.drain(..) {
            self.extra_way_ids.set(id);
        }
        for id in node_ids.drain(..) {
            self.extra_node_ids.set(id);
        }
    }
}

/// The pass 1 sets that are final by the time relations are classified,
/// which a smart relation's members are filtered against.
struct Pass1Known<'a> {
    bbox_node_ids: &'a IdSet,
    outside_way_node_ids: &'a IdSet,
    matched_way_ids: &'a IdSet,
}

impl Pass1Known<'_> {
    /// Push the members of smart relation `r` that pass 1 does not already
    /// include.
    fn push_new_members(&self, r: &crate::Relation, ways: &mut Vec<i64>, nodes: &mut Vec<i64>) {
        for m in r.members() {
            match m.id {
                MemberId::Way(id) if !self.matched_way_ids.get(id) => ways.push(id),
                MemberId::Node(id)
                    if !self.bbox_node_ids.get(id) && !self.outside_way_node_ids.get(id) =>
                {
                    nodes.push(id);
                }
                _ => {}
            }
        }
    }
}

/// Which element kind the unsorted fallback's current file sweep resolves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UnsortedPhase {
    Nodes,
    Ways,
    Relations,
}

/// Collect pass 1 ID sets with strategy-specific relation handling.
///
/// On a sorted file, three parallel classify phases; on an unsorted one,
/// three sequential `BlobReader` sweeps. Collects:
/// - `bbox_node_ids`: nodes within the bounding box
/// - `matched_way_ids`: ways referencing at least one bbox node
/// - `outside_way_node_ids`: node refs of matched ways outside the region
/// - `matched_relation_ids`: relations with a member node in the region or a
///   member way matched, then every relation reaching one of those through
///   relation-type members, recursively (see [`add_parent_relations`])
///
/// The `handler` controls additional per-relation processing (e.g. smart
/// strategy collects extra way/node IDs from multipolygon/boundary relations).
#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
#[hotpath::measure]
pub(super) fn collect_pass1_generic<H: RelationHandler>(
    input: &PinnedInput,
    region: &Region,
    bbox_int: &BboxInt,
    direct_io: bool,
    handler: &mut H,
    host: &crate::host::Entered<'_>,
) -> Result<Pass1Result> {
    let mut bbox_node_ids = IdSet::new();
    let mut matched_way_ids = IdSet::new();
    let mut outside_way_node_ids = IdSet::new();
    let mut matched_relation_ids = IdSet::new();
    // (member relation, parent relation) for every relation-type member of
    // every relation, matched or not: the parent closure below walks it.
    let mut parent_edges: Vec<(i64, i64)> = Vec::new();
    // Point membership runs against the exact box; `bbox_int` is the
    // conservative one, for blob selection only.
    let exact_bbox = BboxInt::inside_bbox(region.bbox());

    // Read only for the header's sort flag, which picks the branch below.
    let mut blob_reader = crate::blob::BlobReader::open_in(input, direct_io, host)?;
    blob_reader.set_parse_indexdata(true);
    let header_blob = blob_reader.leading_header()?;
    let is_sorted = header_blob.to_headerblock()?.is_sorted();
    let filter = spatial_blob_filter(bbox_int);
    let mut decompress_buf: Vec<u8> = Vec::new();

    if !is_sorted {
        drop(blob_reader);
        let (mut st_scratch, mut gr_scratch) = (Vec::new(), Vec::new());
        let (mut extras_ways, mut extras_nodes) = (Vec::new(), Vec::new());
        // One sweep per element kind, for the same reason the simple
        // strategy's unsorted fallback does it: matching a way needs
        // `bbox_node_ids` already complete, and matching a relation needs
        // `matched_way_ids` already complete. A single file-order sweep only
        // supplies that ordering on a sorted file, and this branch is the one
        // taken when the file is not sorted - so any way stored before its
        // nodes, or relation stored before its members, was silently dropped
        // from the extract.
        for phase in [
            UnsortedPhase::Nodes,
            UnsortedPhase::Ways,
            UnsortedPhase::Relations,
        ] {
            let mut blob_reader = crate::blob::BlobReader::open_in(input, direct_io, host)?;
            blob_reader.set_parse_indexdata(true);
            blob_reader.leading_header()?;
            let source = blob_reader.source().cloned();
            for blob_result in &mut blob_reader {
                let blob = blob_result?;
                if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
                    continue;
                }
                if let Some(idx) = blob.index()
                    && !filter.wants_index(&idx)
                {
                    continue;
                }
                blob.decompress_into(&mut decompress_buf)?;
                // The block takes the decode buffer and drops before the
                // next blob, unpooled by the rule in read/decompress.rs
                // ("Take loops"); the parse scratch is reused.
                let block = PrimitiveBlock::from_vec_with_scratch(
                    std::mem::take(&mut decompress_buf),
                    &mut st_scratch,
                    &mut gr_scratch,
                )?;
                crate::commands::release_scratch(&mut st_scratch);
                crate::commands::release_scratch(&mut gr_scratch);
                super::common::refuse_blob_foreign_kind(&blob, &block, source.as_ref())?;
                for element in block.elements_skip_metadata() {
                    match (phase, &element) {
                        (UnsortedPhase::Nodes, Element::DenseNode(dn))
                            if region.contains_decimicro(
                                &exact_bbox,
                                dn.decimicro_lat(),
                                dn.decimicro_lon(),
                            ) =>
                        {
                            bbox_node_ids.set(dn.id());
                        }
                        (UnsortedPhase::Nodes, Element::Node(n))
                            if region.contains_decimicro(
                                &exact_bbox,
                                n.decimicro_lat(),
                                n.decimicro_lon(),
                            ) =>
                        {
                            bbox_node_ids.set(n.id());
                        }
                        (UnsortedPhase::Ways, Element::Way(w))
                            if w.refs().any(|r| bbox_node_ids.get(r)) =>
                        {
                            matched_way_ids.set(w.id());
                            for r in w.refs().filter(|&r| !bbox_node_ids.get(r)) {
                                outside_way_node_ids.set(r);
                            }
                        }
                        (UnsortedPhase::Relations, Element::Relation(r)) => {
                            push_parent_edges(r, &mut parent_edges);
                            if relation_has_matched_member(r, &bbox_node_ids, &matched_way_ids) {
                                matched_relation_ids.set(r.id());
                                if H::COLLECT_MEMBER_IDS && is_smart_relation(r) {
                                    let known = Pass1Known {
                                        bbox_node_ids: &bbox_node_ids,
                                        outside_way_node_ids: &outside_way_node_ids,
                                        matched_way_ids: &matched_way_ids,
                                    };
                                    known.push_new_members(r, &mut extras_ways, &mut extras_nodes);
                                    handler.merge_extras(&mut extras_ways, &mut extras_nodes);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        add_parent_relations(&mut matched_relation_ids, parent_edges, host);
        return Ok(Pass1Result {
            bbox_node_ids,
            matched_way_ids,
            outside_way_node_ids,
            matched_relation_ids,
            way_schedule: Vec::new(),
            pass3_blob_schedule: Vec::new(),
        });
    }

    // Sorted path: parallel three-phase classification via pread-from-workers.
    // Phase 1: nodes (bbox check) → bbox_node_ids
    // Phase 2: ways (ref check against bbox_node_ids) → matched_way_ids + outside_way_node_ids
    // Phase 3: relations (member check) → matched_relation_ids + handler extras
    drop(blob_reader);
    drop(decompress_buf);

    // Build per-type schedules from header-only scan via pread-only HeaderWalker.
    host.invocation.marker("SMART_PASS1_SCHEDULE_SCAN_START");
    let mut walker = crate::read::header_walker::HeaderWalker::open(input, host)?;
    let _ = walker
        .next_header()?
        .ok_or_else(|| crate::error::new_missing_header(input.path()))?;

    use crate::scan::classify::ScheduledBlob;
    let mut node_schedule: Vec<ScheduledBlob> = Vec::new();
    let mut way_schedule: Vec<ScheduledBlob> = Vec::new();
    let mut relation_schedule: Vec<ScheduledBlob> = Vec::new();
    // Unfiltered way schedule (no spatial filter): used by smart PASS2 to
    // find ways referenced by relations regardless of spatial location.
    // Returned via Pass1Result so smart PASS2 can skip its own
    // build_classify_schedule call entirely, saving ~16% wall on Europe
    // (one fewer 19-second header scan after PASS1's parallel work).
    let mut full_way_schedule: Vec<ScheduledBlob> = Vec::new();
    // Full BlobDesc schedule for all OsmData blobs, returned via Pass1Result
    // for smart/complete PASS3 to reuse. Eliminates the third post-PASS1
    // header scan (build_blob_schedule) and its associated cold-arena-page
    // residency cascade.
    let mut pass3_blob_schedule: Vec<BlobDesc> = Vec::new();
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        walker.observe_filter_index(meta.index.as_ref());
        let idx = meta.index.as_ref();
        let entry = ScheduledBlob::from_meta(&meta, crate::scan::classify::KindClaims::Refuse)?;
        // Build pass3_blob_schedule unconditionally for all OsmData blobs.
        // Mirrors build_blob_schedule's behavior (no kind/spatial filter,
        // no passthrough since smart/complete don't use the passthrough
        // optimization).
        let bbox = idx.and_then(|i| i.bbox);
        let kind_for_blob = idx.map(|i| i.kind);
        pass3_blob_schedule.push(BlobDesc {
            frame_offset: meta.frame_start,
            frame_size: meta.frame_size,
            offset: meta.data_offset,
            size: meta.data_size,
            kind: kind_for_blob,
            bbox,
            passthrough: Passthrough::No,
        });
        // Kind and bbox are indexdata claims, believed without a decode: a
        // mislabelled blob lands in the wrong per-kind schedule and a node
        // blob whose bbox understates its nodes is skipped by
        // `filter.wants_index` (reference/indexdata-trust.md).
        if let Some(idx) = idx {
            // Build full_way_schedule unconditionally for way blobs.
            if matches!(idx.kind, crate::ElementKind::Way) {
                full_way_schedule.push(entry);
            }
            if !filter.wants_index(idx) {
                continue;
            }
            match idx.kind {
                crate::ElementKind::Node => node_schedule.push(entry),
                crate::ElementKind::Way => way_schedule.push(entry),
                crate::ElementKind::Relation => relation_schedule.push(entry),
            }
        } else {
            // Non-indexed blob (force-flag path). Without indexdata we cannot
            // determine the blob's kind or bbox up front, so replicate into
            // every per-kind schedule; classify closures below already
            // kind-filter via `match element`, so blobs of the wrong kind
            // no-op at the element level. The same replication repack and
            // degrade ask for through `scan::classify::KindSchedules::
            // into_replicated`, and `multi::try_extract_multi_single_pass`
            // does.
            node_schedule.push(entry);
            way_schedule.push(entry);
            relation_schedule.push(entry);
            full_way_schedule.push(entry);
        }
    }
    host.invocation.marker("SMART_PASS1_SCHEDULE_SCAN_END");

    host.invocation
        .counter("smart_pass1_node_blobs", node_schedule.len());
    host.invocation
        .counter("smart_pass1_way_blobs", way_schedule.len());
    host.invocation
        .counter("smart_pass1_relation_blobs", relation_schedule.len());
    host.invocation
        .counter("smart_pass1_full_way_blobs", full_way_schedule.len());
    host.invocation
        .counter("smart_pass1_pass3_blobs", pass3_blob_schedule.len());

    let shared_file = walker.input();
    drop(walker);

    // Phase 1: Classify nodes by region containment.
    // For bbox-only regions, use columnar decode (batch IDs/lats/lons into
    // contiguous arrays) for cache-friendly classification. Polygon regions
    // fall back to element-by-element iteration.
    host.invocation.marker("PASS1_NODE_CLASSIFY_START");
    let use_columnar = matches!(region, Region::Bbox(_));
    // The columnar classifier decides point membership, not blob candidacy,
    // so it runs against the exact box (`exact_bbox`). `bbox_int` is
    // deliberately widened and would admit nodes up to one decimicrodegree
    // outside the requested bbox.
    let exact_bbox = &exact_bbox;
    crate::scan::classify::parallel_classify_phase(
        &shared_file,
        &node_schedule,
        None,
        || {
            (
                crate::read::columnar::DenseNodeColumns::new(),
                Vec::<i64>::new(),
            )
        },
        |block, (columns, ids)| {
            ids.clear();
            if use_columnar {
                block.decode_dense_columns(columns);
                columns.collect_matching_ids_bbox(
                    exact_bbox.min_lat,
                    exact_bbox.max_lat,
                    exact_bbox.min_lon,
                    exact_bbox.max_lon,
                    ids,
                );
                // Dense columns deliberately exclude plain Node messages, so
                // they are classified here; without this a plain node in the
                // region, and every way and relation it would have matched,
                // was missing from a sorted bbox extract.
                for group in block.groups() {
                    for node in group.nodes() {
                        if exact_bbox.contains(node.decimicro_lat(), node.decimicro_lon()) {
                            ids.push(node.id());
                        }
                    }
                }
            } else {
                for element in block.elements_skip_metadata() {
                    match &element {
                        Element::DenseNode(dn)
                            if region.contains_decimicro(
                                exact_bbox,
                                dn.decimicro_lat(),
                                dn.decimicro_lon(),
                            ) =>
                        {
                            ids.push(dn.id());
                        }
                        Element::Node(n)
                            if region.contains_decimicro(
                                exact_bbox,
                                n.decimicro_lat(),
                                n.decimicro_lon(),
                            ) =>
                        {
                            ids.push(n.id());
                        }
                        _ => {}
                    }
                }
            }
            Ok(std::mem::take(ids))
        },
        |_seq, ids| {
            for id in ids {
                bbox_node_ids.set(id);
            }
        },
        host,
    )?;
    host.invocation.marker("PASS1_NODE_CLASSIFY_END");

    // Phase 2: Classify ways by ref intersection with bbox nodes.
    //
    // Deliberately a block walk, not a projection (shared by smart and
    // complete). Reading each blob through `read::project_ids::WaySpans`
    // was built and measured at planet (Europe bbox, smart): this phase
    // stayed at 16.6-16.9 s, since every way's refs are scanned against the
    // node set and that, not the message re-parse a projection removes, is
    // its cost (reference/performance/inspect.md, "inspect --nodes reads through a
    // projection").
    host.invocation.marker("PASS1_WAY_CLASSIFY_START");
    crate::scan::classify::parallel_classify_phase(
        &shared_file,
        &way_schedule,
        None,
        || (),
        |block, _s| {
            let mut way_ids = Vec::new();
            let mut node_ids = Vec::new();
            for element in block.elements_skip_metadata() {
                if let Element::Way(w) = &element
                    && w.refs().any(|r| bbox_node_ids.get(r))
                {
                    way_ids.push(w.id());
                    node_ids.extend(w.refs().filter(|&r| !bbox_node_ids.get(r)));
                }
            }
            Ok((way_ids, node_ids))
        },
        |_seq, (way_ids, node_ids)| {
            for id in way_ids {
                matched_way_ids.set(id);
            }
            for id in node_ids {
                outside_way_node_ids.set(id);
            }
        },
        host,
    )?;
    host.invocation.marker("PASS1_WAY_CLASSIFY_END");

    // Phase 3: Classify relations by member intersection.
    //
    // A block walk on purpose: the whole phase is about 1 s at planet
    // (0.9-1.2 s, bygg), so the re-parse a projection would remove sits
    // under the noise floor of a 130 s run; smart also reads the relation
    // tags (`is_smart_relation`).
    host.invocation.marker("PASS1_RELATION_CLASSIFY_START");
    // Per-blob send, not per-worker accumulation: a worker-held `IdSet` of
    // smart member IDs promoted its own dense chunks for members dispersed
    // over the ID space, so the phase's memory scaled with the worker count
    // (planet, Europe-sized box: 7.8 GB peak against 4.5 GB either side).
    // Workers only read `bbox_node_ids`, `outside_way_node_ids` and
    // `matched_way_ids`, all final here, while the consumer writes the
    // relation set, the handler's extras and the edge list.
    let known = Pass1Known {
        bbox_node_ids: &bbox_node_ids,
        outside_way_node_ids: &outside_way_node_ids,
        matched_way_ids: &matched_way_ids,
    };
    let known = &known;
    crate::scan::classify::parallel_classify_phase(
        &shared_file,
        &relation_schedule,
        None,
        || (),
        |block, _s| {
            let mut rel_ids = Vec::new();
            let mut extra_ways = Vec::new();
            let mut extra_nodes = Vec::new();
            let mut edges = Vec::new();
            for element in block.elements_skip_metadata() {
                let Element::Relation(r) = &element else {
                    continue;
                };
                push_parent_edges(r, &mut edges);
                if relation_has_matched_member(r, known.bbox_node_ids, known.matched_way_ids) {
                    rel_ids.push(r.id());
                    if H::COLLECT_MEMBER_IDS && is_smart_relation(r) {
                        known.push_new_members(r, &mut extra_ways, &mut extra_nodes);
                    }
                }
            }
            Ok((rel_ids, extra_ways, extra_nodes, edges))
        },
        |_seq, (rel_ids, mut extra_ways, mut extra_nodes, edges)| {
            for id in rel_ids {
                matched_relation_ids.set(id);
            }
            handler.merge_extras(&mut extra_ways, &mut extra_nodes);
            parent_edges.extend(edges);
        },
        host,
    )?;
    host.invocation.marker("PASS1_RELATION_CLASSIFY_END");
    host.invocation.marker("PASS1_RELATION_PARENTS_START");
    add_parent_relations(&mut matched_relation_ids, parent_edges, host);
    host.invocation.marker("PASS1_RELATION_PARENTS_END");

    Ok(Pass1Result {
        bbox_node_ids,
        matched_way_ids,
        outside_way_node_ids,
        matched_relation_ids,
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
    input: &PinnedInput,
    output: &crate::output::BoundOutput,
    region: &Region,
    set_bounds: bool,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<ExtractStats> {
    let WriteOptions {
        compression,
        backend,
        direct_reads: direct_io,
        ..
    } = *write;
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
    host.invocation.marker("SMART_PASS1_START");
    let bbox_int = BboxInt::from_bbox(region.bbox());
    let mut handler = SmartRelationHandler::new();
    let mut result =
        collect_pass1_generic(input, region, &bbox_int, direct_io, &mut handler, host)?;
    let mut extra_node_ids = handler.extra_node_ids;
    host.invocation.marker("SMART_PASS1_END");
    // Brokkr stores named counters generically and reports changes in sidecar
    // comparisons; this sample is inert unless the invocation has a sink.
    crate::debug::emit_mallinfo2(&host.invocation, "MI_PASS1_END");

    // --- Pass 2: Resolve extra way node deps (parallel pread) ---
    host.invocation.marker("SMART_PASS2_START");
    // For each way in extra_way_ids (none of them matched), collect the node
    // refs pass 1 does not include into extra_node_ids.
    // Reuses PASS1's full_way_schedule (built during the manual header scan
    // in collect_pass1_generic) to avoid a second 19-second
    // build_classify_schedule call after PASS1's parallel work. Saves ~16%
    // wall on Europe extract-smart. Falls back to build_classify_schedule
    // for the unsorted-fallback path where PASS1 doesn't build a schedule.
    {
        host.invocation.marker("SMART_PASS2_SCHEDULE_START");
        let pass1_way_schedule = std::mem::take(&mut result.way_schedule);
        let (way_schedule, shared_file) = if pass1_way_schedule.is_empty() {
            crate::scan::classify::build_classify_schedule(
                input,
                Some(crate::ElementKind::Way),
                crate::scan::classify::KindClaims::Refuse,
                host,
            )?
        } else {
            (
                pass1_way_schedule,
                crate::read::input::InputFile::open(input)?,
            )
        };
        host.invocation.marker("SMART_PASS2_SCHEDULE_END");

        let extra_way_ids_ref = &handler.extra_way_ids;
        let bbox_node_ids_ref = &result.bbox_node_ids;
        let outside_way_node_ids_ref = &result.outside_way_node_ids;
        // Per-blob send (not accumulate): extra_way_ids is relation-driven and
        // can be wide + globally dispersed. Per-worker IdSet accumulate over
        // an unbounded node-ID set is the unsafe shape the helper doc warns
        // against, regardless of whether this specific workload triggers the
        // worst case. The fix improves PASS2 wall by ~23%; the planet-scale
        // memory peak it was originally framed as fixing turned out to be
        // elsewhere (cold-arena-page residency cascade, addressed by the
        // PASS1 schedule-reuse commits `d4ea760` and `0b085b1`).
        // Deliberately a block walk. On `WaySpans` restricted to
        // `extra_way_ids` this phase went from 10.1 s to 8.7 s at planet,
        // but the command moved by under 1 % (131.9 s against 131.5 s
        // median), noise for a 130 s run (reference/performance/inspect.md,
        // "inspect --nodes reads through a projection").
        host.invocation.marker("SMART_PASS2_CLASSIFY_START");
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
                        // `extra_way_ids` holds no matched way (see
                        // `SmartRelationHandler`), and refs pass 1 already
                        // includes stay out of `extra_node_ids`.
                        if extra_way_ids_ref.get(wid) {
                            scratch.extend(w.refs().filter(|&r| {
                                !bbox_node_ids_ref.get(r) && !outside_way_node_ids_ref.get(r)
                            }));
                        }
                    }
                }
                Ok(std::mem::take(scratch))
            },
            |_seq, refs| {
                for id in refs {
                    extra_node_ids.set(id);
                }
            },
            host,
        )?;
        host.invocation.marker("SMART_PASS2_CLASSIFY_END");
    }

    host.invocation.marker("SMART_PASS2_END");

    // --- Pass 3: Write matching elements via pread-from-workers ---
    host.invocation.marker("SMART_PASS3_START");
    host.invocation.marker("SMART_PASS3_SETUP_START");

    let mut header_reader = crate::blob::BlobReader::open_in(input, direct_io, host)?;
    let header_blob = header_reader.leading_header()?;
    let header = header_blob.to_headerblock()?;
    drop(header_reader);
    let bbox = region.bbox();
    let mut writer = writer_from_header(
        output,
        compression,
        &header,
        &super::output_facts(set_bounds, bbox),
        overrides,
        backend,
        host,
    )?;

    // Take the pre-built blob schedule BEFORE creating `ids`, since `ids`
    // holds immutable borrows of `result` and we need a brief mutable borrow
    // here to mem::take the schedule.
    let pass1_blob_schedule = std::mem::take(&mut result.pass3_blob_schedule);

    let ids = ExtractPass3IdSets {
        bbox_node_ids: &result.bbox_node_ids,
        outside_way_node_ids: &result.outside_way_node_ids,
        extra_node_ids: &extra_node_ids,
        matched_way_ids: &result.matched_way_ids,
        extra_way_ids: &handler.extra_way_ids,
        matched_relation_ids: &result.matched_relation_ids,
    };

    host.invocation.marker("SMART_PASS3_SETUP_END");
    host.invocation.marker("SMART_PASS3_WRITE_START");
    // Reuse PASS1's pre-built blob schedule if available, falling back to
    // build_blob_schedule for the unsorted-fallback path. Avoids the third
    // post-PASS1 header scan and its cold-arena-page residency cascade.
    if pass1_blob_schedule.is_empty() {
        pread_write_pass(
            input,
            &mut writer,
            &mut stats,
            |block, bb, output_blocks| extract_block_pass3(block, &ids, clean, bb, output_blocks),
            host,
        )?;
    } else {
        pread_write_pass_with_schedule(
            input,
            &pass1_blob_schedule,
            &mut writer,
            &mut stats,
            |block, bb, output_blocks| extract_block_pass3(block, &ids, clean, bb, output_blocks),
            host,
        )?;
    }
    host.invocation.marker("SMART_PASS3_WRITE_END");

    host.invocation.marker("SMART_PASS3_END");
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Smart Pass 3: Parallel helpers
// ---------------------------------------------------------------------------

/// Read-only ID sets for Pass 3 of smart strategy, shared across rayon threads.
struct ExtractPass3IdSets<'a> {
    bbox_node_ids: &'a IdSet,
    outside_way_node_ids: &'a IdSet,
    extra_node_ids: &'a IdSet,
    matched_way_ids: &'a IdSet,
    extra_way_ids: &'a IdSet,
    matched_relation_ids: &'a IdSet,
}

use super::super::clean_metadata;
use crate::owned::{dense_node_metadata, element_metadata, relation_members_into};

/// Process a single block for Pass 3 of smart extraction: write elements whose IDs
/// were collected in Passes 1+2. Uses thread-local BlockBuilder and output buffer.
#[hotpath::measure]
fn extract_block_pass3<O: crate::commands::BlockSink + ?Sized>(
    block: &PrimitiveBlock,
    ids: &ExtractPass3IdSets<'_>,
    clean: &CleanAttrs,
    bb: &mut BlockBuilder,
    output: &mut O,
) -> crate::Result<ExtractStats> {
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
                let from_way = ids.outside_way_node_ids.get(id);
                let from_rel = ids.extra_node_ids.get(id);
                if in_bbox || from_way || from_rel {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = clean_metadata(dense_node_metadata(dn)?, clean);
                    bb.add_node(
                        dn.id(),
                        dn.decimicro_lat(),
                        dn.decimicro_lon(),
                        dn.tags(),
                        meta.as_ref(),
                    );
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
                let from_way = ids.outside_way_node_ids.get(id);
                let from_rel = ids.extra_node_ids.get(id);
                if in_bbox || from_way || from_rel {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = clean_metadata(
                        element_metadata(crate::ElementKind::Node, n.id(), &n.info())?,
                        clean,
                    );
                    bb.add_node(
                        n.id(),
                        n.decimicro_lat(),
                        n.decimicro_lon(),
                        n.tags(),
                        meta.as_ref(),
                    );
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
                    let meta = clean_metadata(
                        element_metadata(crate::ElementKind::Way, w.id(), &w.info())?,
                        clean,
                    );
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
                    relation_members_into(r, &mut members_buf)?;
                    let meta = clean_metadata(
                        element_metadata(crate::ElementKind::Relation, r.id(), &r.info())?,
                        clean,
                    );
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

/// Returns true if the relation's `type` tag is `multipolygon` or `boundary`.
///
/// These are the relation types whose direct node and way members the smart
/// strategy pulls in, with every node of those ways. The first `type` key
/// decides, as osmium's `tags()["type"]` does, so a relation carrying the
/// key twice is judged by the same value under both tools.
///
/// osmium smart completes `multipolygon` alone unless given
/// `-S types=multipolygon,boundary`. pbfhogg completes boundaries too, a
/// recorded deviation (DEVIATIONS.md, "extract: smart completes boundary
/// relations"): an administrative area cut at the extract edge is as broken
/// a polygon as a multipolygon is, and `build-geocode-index` and `export`
/// both assemble `type=boundary` relations as areas.
fn is_smart_relation(r: &crate::Relation) -> bool {
    r.tags()
        .find(|&(k, _)| k == "type")
        .is_some_and(|(_, v)| v == "multipolygon" || v == "boundary")
}

/// Record a `(member, parent)` edge for every relation-type member of `r`.
///
/// Every relation contributes, matched or not: whether a parent is in the
/// extract depends on its members' membership, which is only known once
/// every relation blob has been classified.
fn push_parent_edges(r: &crate::Relation, edges: &mut Vec<(i64, i64)>) {
    let parent = r.id();
    for m in r.members() {
        if let MemberId::Relation(child) = m.id {
            edges.push((child, parent));
        }
    }
}

/// Add every relation that has an already-included relation as a member,
/// recursively to any depth: osmium extract's complete_ways and smart rule
/// (`add_relation_parents`). A parent included this way pulls in nothing
/// else - not its other members, and under smart it is not completed even
/// when it is a multipolygon or boundary - exactly as in osmium.
///
/// A worklist seeded with every directly matched relation, over the edges
/// sorted by member, so each relation is expanded once whatever the depth;
/// cycles and self-membership end because a relation is only pushed when it
/// is newly added.
fn add_parent_relations(
    included: &mut IdSet,
    mut edges: Vec<(i64, i64)>,
    host: &crate::host::Entered<'_>,
) {
    host.invocation
        .counter("extract_relation_parent_edges", edges.len());
    if edges.is_empty() {
        host.invocation
            .counter("extract_relations_from_parents", 0_u64);
        return;
    }
    edges.sort_unstable();
    edges.dedup();
    let mut work: Vec<i64> = included.iter().collect();
    let mut added: u64 = 0;
    while let Some(child) = work.pop() {
        let start = edges.partition_point(|&(c, _)| c < child);
        for &(c, parent) in &edges[start..] {
            if c != child {
                break;
            }
            if included.set_if_new(parent) {
                added += 1;
                work.push(parent);
            }
        }
    }
    host.invocation
        .counter("extract_relations_from_parents", added);
}
