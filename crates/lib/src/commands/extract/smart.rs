//! Smart extraction strategy (three passes) + shared Pass 1 ID collection used by complete-ways.

use crate::block_builder::{BlockBuilder, MemberData};
use crate::cat::CleanAttrs;
use crate::read::block::{BlockBuffer, ClassificationRelation, SelectionBlock};
use crate::read::input::PinnedInput;
use crate::write::WriteOptions;
use crate::{Element, MemberId};

use super::super::{
    HeaderOverrides, Result, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, writer_from_header,
};
use crate::idset::IdSet;

use super::common::{
    BboxInt, BlobDesc, Extent, MayHold, Passthrough, WriteSlot, WriteSpan, add_dense_node,
    add_plain_node, any_may_hold, pread_write_pass, pread_write_pass_with_schedule,
    relation_has_matched_member, select_by_extent, select_write_schedule, spatial_blob_filter,
    widen,
};
use super::node_frames::{self, RetainedFrames};
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
    /// The schedules pass 1's header walk built, `None` on the unsorted
    /// fallback (whose later passes build their own, unpruned).
    pub(super) schedules: Option<Pass1Schedules>,
}

/// Schedules from pass 1's one header walk, reused by the later passes so
/// none of them walks the headers again (post-pass-1 header walks cost
/// about 16 % of Europe wall and a cold-arena-page residency cascade in
/// anon RSS, commits `d4ea760`, `0b085b1`), with what the classify phases
/// observed so the later passes read only the blobs that may hold an ID
/// they need: for a small region that is a small share of the file, where
/// they used to decode every way blob (smart's extra-way pass) and every
/// OsmData blob (the write pass).
pub(super) struct Pass1Schedules {
    /// Every way blob, the way classify's schedule.
    pub(super) way_schedule: Vec<crate::scan::classify::ScheduledBlob>,
    /// The observed extent of each `way_schedule` entry's ways.
    pub(super) way_extents: Vec<Extent>,
    /// Every OsmData blob, in frame order.
    pub(super) write_schedule: Vec<BlobDesc>,
    /// One span per `write_schedule` entry.
    pub(super) write_spans: Vec<WriteSpan>,
    retained_nodes: Option<RetainedFrames>,
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
            self.extra_way_ids.insert(id);
        }
        for id in node_ids.drain(..) {
            self.extra_node_ids.insert(id);
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
    fn push_new_members(
        &self,
        r: &ClassificationRelation<'_>,
        ways: &mut Vec<i64>,
        nodes: &mut Vec<i64>,
    ) -> Result<()> {
        for m in completion_members(r)? {
            match m {
                CompletionMember::Way(id) if !self.matched_way_ids.get(id) => ways.push(id),
                CompletionMember::Node(id)
                    if !self.bbox_node_ids.get(id) && !self.outside_way_node_ids.get(id) =>
                {
                    nodes.push(id);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// A member a directly matched smart relation record pulls into the
/// extract: a node, or a way with every one of its nodes.
pub(super) enum CompletionMember {
    Node(i64),
    Way(i64),
}

/// The members a directly matched relation record completes under smart,
/// in member order: its node and way members when the record itself is a
/// smart relation ([`is_smart_relation`], judged on this record's tags, not
/// another version's), none otherwise. Relation and unknown members are
/// never completed.
///
/// The one rule smart extract and the multi-region engine
/// (`multi_dep.rs`) share. Each applies the extra-way exclusion against
/// its own matched-way record: a way member already matched is written as
/// matched, so it is not an extra way (in the region where it matched, for
/// the multi engine) and its nodes come only from the ways' own completion.
pub(super) fn completion_members<'a>(
    r: &ClassificationRelation<'a>,
) -> Result<impl Iterator<Item = CompletionMember> + 'a> {
    let take = if is_smart_relation(r)? { usize::MAX } else { 0 };
    Ok(r.members().take(take).filter_map(|member| match member {
        MemberId::Node(id) => Some(CompletionMember::Node(id)),
        MemberId::Way(id) => Some(CompletionMember::Way(id)),
        MemberId::Relation(_) | MemberId::Unknown(_, _) => None,
    }))
}

/// Which element kind the unsorted fallback's current file sweep resolves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UnsortedPhase {
    Nodes,
    Ways,
    Relations,
}

impl UnsortedPhase {
    fn kind(self) -> crate::ElementKind {
        match self {
            Self::Nodes => crate::ElementKind::Node,
            Self::Ways => crate::ElementKind::Way,
            Self::Relations => crate::ElementKind::Relation,
        }
    }
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
    direct_io: bool,
    node_frame_options: Option<node_frames::Options<'_>>,
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
    let filter = spatial_blob_filter(&BboxInt::from_bbox(region.bbox()));
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
                // A blob claiming another kind holds nothing this sweep
                // reads (its own sweep decodes it and refuses a false
                // claim), as in the simple strategy's unsorted fallback.
                if let Some(idx) = blob.index()
                    && (idx.kind != phase.kind() || !filter.wants_index(&idx))
                {
                    continue;
                }
                let mut project = || -> Result<_> {
                    blob.decompress_into(&mut decompress_buf)?;
                    BlockBuffer::new(
                        std::mem::take(&mut decompress_buf),
                        None,
                        &mut st_scratch,
                        &mut gr_scratch,
                        blob.index().map(|idx| idx.kind),
                    )
                    .for_selection()
                };
                let buffer = project().map_err(|err| blob.locate_error(source.as_ref(), err))?;
                crate::retention::release_scratch(&mut st_scratch);
                crate::retention::release_scratch(&mut gr_scratch);
                let block = buffer
                    .classify()
                    .map_err(|err| blob.locate_error(source.as_ref(), err))?;
                if phase == UnsortedPhase::Relations {
                    let known = Pass1Known {
                        bbox_node_ids: &bbox_node_ids,
                        outside_way_node_ids: &outside_way_node_ids,
                        matched_way_ids: &matched_way_ids,
                    };
                    for r in block.relations() {
                        push_parent_edges(&r, &mut parent_edges);
                        if relation_has_matched_member(
                            r.members(),
                            &bbox_node_ids,
                            &matched_way_ids,
                        ) {
                            matched_relation_ids.insert(r.id());
                            if H::COLLECT_MEMBER_IDS {
                                known
                                    .push_new_members(&r, &mut extras_ways, &mut extras_nodes)
                                    .map_err(|err| blob.locate_error(source.as_ref(), err))?;
                                handler.merge_extras(&mut extras_ways, &mut extras_nodes);
                            }
                        }
                    }
                    continue;
                }
                if phase == UnsortedPhase::Ways {
                    for way in block.ways() {
                        if way.refs().any(|id| bbox_node_ids.get(id)) {
                            matched_way_ids.insert(way.id());
                            for id in way.refs().filter(|&id| !bbox_node_ids.get(id)) {
                                outside_way_node_ids.insert(id);
                            }
                        }
                    }
                    continue;
                }
                for element in block.nodes() {
                    let (id, lat, lon) = match element {
                        Element::DenseNode(n) => (n.id(), n.decimicro_lat()?, n.decimicro_lon()?),
                        Element::Node(n) => (n.id(), n.decimicro_lat()?, n.decimicro_lon()?),
                        _ => continue,
                    };
                    if region.contains_decimicro(&exact_bbox, lat, lon) {
                        bbox_node_ids.insert(id);
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
            schedules: None,
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
    // The spatial filter passes every way and relation blob, so
    // `way_schedule` is every way blob: the schedule smart's extra-way pass
    // cuts by observed extent.
    let mut relation_schedule: Vec<ScheduledBlob> = Vec::new();
    // Every OsmData blob, for the write pass, with where its span will
    // come from.
    let mut write_schedule: Vec<BlobDesc> = Vec::new();
    let mut write_slots: Vec<WriteSlot> = Vec::new();
    let mut node_positions = Vec::new();
    let mut all_indexed = true;
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        walker.observe_filter_index(meta.index.as_ref());
        let idx = meta.index.as_ref();
        let entry = ScheduledBlob::from_meta(&meta, crate::scan::classify::KindClaims::Refuse)?;
        // The write schedule takes every OsmData blob (no kind or spatial
        // filter, no input frame passthrough).
        let bbox = idx.and_then(|i| i.bbox);
        let kind_for_blob = idx.map(|i| i.kind);
        let before = (way_schedule.len(), relation_schedule.len());
        write_schedule.push(BlobDesc {
            frame_offset: meta.frame_start,
            frame_size: meta.frame_size,
            offset: meta.data_offset,
            size: meta.data_size,
            kind: kind_for_blob,
            bbox,
            index: idx.copied(),
            passthrough: Passthrough::No,
        });
        // Kind and bbox are indexdata claims, believed without a decode: a
        // mislabelled blob lands in the wrong per-kind schedule and a node
        // blob whose bbox understates its nodes is skipped by
        // `filter.wants_index` (reference/indexdata-trust.md).
        if let Some(idx) = idx {
            if filter.wants_index(idx) {
                match idx.kind {
                    crate::ElementKind::Node => {
                        node_schedule.push(entry);
                        if node_frame_options.is_some() {
                            node_positions.push(write_slots.len());
                        }
                    }
                    crate::ElementKind::Way => way_schedule.push(entry),
                    crate::ElementKind::Relation => relation_schedule.push(entry),
                }
            }
            write_slots.push(WriteSlot::of(
                idx,
                (before.0, way_schedule.len()),
                (before.1, relation_schedule.len()),
            )?);
        } else {
            all_indexed = false;
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
            write_slots.push(WriteSlot::Unindexed);
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
        .counter("smart_pass1_pass3_blobs", write_schedule.len());
    // Observed extents, one per way and relation classify-schedule position.
    let mut way_extents: Vec<Extent> = vec![None; way_schedule.len()];
    let mut relation_extents: Vec<Extent> = vec![None; relation_schedule.len()];

    let shared_file = walker.input();
    drop(walker);

    host.invocation.marker("PASS1_NODE_CLASSIFY_START");
    let retained_nodes = node_frames::classify_nodes(
        &shared_file,
        &node_schedule,
        &write_schedule,
        &node_positions,
        region,
        &exact_bbox,
        node_frame_options.filter(|_| all_indexed),
        &mut bbox_node_ids,
        host,
    )?;
    host.invocation.marker("PASS1_NODE_CLASSIFY_END");

    // Phase 2: Classify ways by ref intersection with bbox nodes.
    host.invocation.marker("PASS1_WAY_CLASSIFY_START");
    crate::scan::classify::parallel_project_phase(
        &shared_file,
        &way_schedule,
        None,
        || (),
        |_, buffer, _| {
            let buffer = buffer.for_selection()?;
            let block = buffer.classify()?;
            let mut way_ids = Vec::new();
            let mut node_ids = Vec::new();
            // Every occurrence, matched or not: smart's extra-way pass and
            // the write pass read this blob only if its extent may hold a
            // way they need.
            let mut extent: Extent = None;
            for w in block.ways() {
                widen(&mut extent, w.id());
                if w.refs().any(|r| bbox_node_ids.get(r)) {
                    way_ids.push(w.id());
                    node_ids.extend(w.refs().filter(|&r| !bbox_node_ids.get(r)));
                }
            }
            Ok((way_ids, node_ids, extent))
        },
        |seq, (way_ids, node_ids, extent)| {
            for id in way_ids {
                matched_way_ids.insert(id);
            }
            for id in node_ids {
                outside_way_node_ids.insert(id);
            }
            if let Some(slot) = way_extents.get_mut(seq) {
                *slot = extent;
            }
        },
        host,
    )?;
    host.invocation.marker("PASS1_WAY_CLASSIFY_END");

    // Phase 3: Classify relations by member intersection. Smart also
    // checks the type of each directly matched relation for completion.
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
    crate::scan::classify::parallel_project_phase(
        &shared_file,
        &relation_schedule,
        None,
        || (),
        |_, buffer, _| {
            let buffer = buffer.for_selection()?;
            let block = buffer.classify()?;
            let mut rel_ids = Vec::new();
            let mut extra_ways = Vec::new();
            let mut extra_nodes = Vec::new();
            let mut edges = Vec::new();
            // Every occurrence: the parent closure can include a relation
            // no record matched directly.
            let mut extent: Extent = None;
            for r in block.relations() {
                widen(&mut extent, r.id());
                push_parent_edges(&r, &mut edges);
                if relation_has_matched_member(
                    r.members(),
                    known.bbox_node_ids,
                    known.matched_way_ids,
                ) {
                    rel_ids.push(r.id());
                    if H::COLLECT_MEMBER_IDS {
                        known.push_new_members(&r, &mut extra_ways, &mut extra_nodes)?;
                    }
                }
            }
            Ok((rel_ids, extra_ways, extra_nodes, edges, extent))
        },
        |seq, (rel_ids, mut extra_ways, mut extra_nodes, edges, extent)| {
            for id in rel_ids {
                matched_relation_ids.insert(id);
            }
            handler.merge_extras(&mut extra_ways, &mut extra_nodes);
            parent_edges.extend(edges);
            if let Some(slot) = relation_extents.get_mut(seq) {
                *slot = extent;
            }
        },
        host,
    )?;
    host.invocation.marker("PASS1_RELATION_CLASSIFY_END");
    host.invocation.marker("PASS1_RELATION_PARENTS_START");
    add_parent_relations(&mut matched_relation_ids, parent_edges, host);
    host.invocation.marker("PASS1_RELATION_PARENTS_END");

    let write_spans = write_slots
        .iter()
        .map(|slot| slot.span(&way_extents, &relation_extents))
        .collect::<Result<Vec<_>>>()?;
    Ok(Pass1Result {
        bbox_node_ids,
        matched_way_ids,
        outside_way_node_ids,
        matched_relation_ids,
        schedules: Some(Pass1Schedules {
            way_schedule,
            way_extents,
            write_schedule,
            write_spans,
            retained_nodes,
        }),
    })
}

/// The write pass's schedule: the pass 1 write schedule cut to the blobs
/// that may hold an element a set selects, counted for the sidecar.
pub(super) fn write_schedule_for(
    mut schedules: Pass1Schedules,
    nodes: &[&dyn MayHold],
    ways: &[&dyn MayHold],
    relations: &[&dyn MayHold],
    host: &crate::host::Entered<'_>,
) -> Result<(Vec<BlobDesc>, Option<RetainedFrames>)> {
    let schedule = select_write_schedule(
        &schedules.write_schedule,
        &schedules.write_spans,
        nodes,
        ways,
        relations,
    )?;
    if let Some(frames) = &mut schedules.retained_nodes {
        // The first node set is the region set, already encoded in each
        // frame. A possible member of any completion set requires a reread.
        let (_, completion) = nodes.split_first().ok_or_else(|| {
            crate::Error::internal("extract: missing region node set in write schedule")
        })?;
        frames.retain(|_, frame| !any_may_hold(completion, Some(frame.extent)));
    }
    let copied = schedules
        .retained_nodes
        .as_ref()
        .map_or(0, RetainedFrames::len);
    host.invocation
        .counter("extract_write_blobs", schedule.len().saturating_sub(copied));
    Ok((schedule, schedules.retained_nodes))
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
    frame_limits: Option<node_frames::Limits>,
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
    let mut handler = SmartRelationHandler::new();
    let node_frame_options = frame_limits.map(|limits| node_frames::Options {
        limits,
        clean,
        compression,
    });
    let mut result = collect_pass1_generic(
        input,
        region,
        direct_io,
        node_frame_options,
        &mut handler,
        host,
    )?;
    let mut extra_node_ids = handler.extra_node_ids;
    host.invocation.marker("SMART_PASS1_END");
    // Brokkr stores named counters generically and reports changes in sidecar
    // comparisons; this sample is inert unless the invocation has a sink.
    crate::debug::emit_mallinfo2(&host.invocation, "MI_PASS1_END");

    // --- Pass 2: Resolve extra way node deps (parallel pread) ---
    host.invocation.marker("SMART_PASS2_START");
    // For each way in extra_way_ids (none of them matched), collect the node
    // refs pass 1 does not include into extra_node_ids. On sorted input the
    // pass reads only the way blobs whose observed extent may hold an extra
    // way (none when there are none); the unsorted fallback builds a full
    // way schedule.
    {
        host.invocation.marker("SMART_PASS2_SCHEDULE_START");
        let (way_schedule, shared_file) = match &result.schedules {
            Some(schedules) => (
                select_by_extent(
                    &schedules.way_schedule,
                    &schedules.way_extents,
                    &[&handler.extra_way_ids],
                )?,
                crate::read::input::InputFile::open(input)?,
            ),
            None => crate::scan::classify::build_classify_schedule(
                input,
                Some(crate::ElementKind::Way),
                crate::scan::classify::KindClaims::Refuse,
                host,
            )?,
        };
        host.invocation
            .counter("smart_pass2_way_blobs", way_schedule.len());
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
        host.invocation.marker("SMART_PASS2_CLASSIFY_START");
        crate::scan::classify::parallel_project_phase(
            &shared_file,
            &way_schedule,
            None,
            Vec::<i64>::new,
            |_, buffer, scratch| {
                let buffer = buffer.for_selection()?;
                let block = buffer.classify()?;
                scratch.clear();
                for way in block.ways() {
                    // Extra ways exclude the direct matches, and refs pass 1
                    // already includes stay out of the extra node IDs.
                    if extra_way_ids_ref.get(way.id()) {
                        scratch.extend(way.refs().filter(|&id| {
                            !bbox_node_ids_ref.get(id) && !outside_way_node_ids_ref.get(id)
                        }));
                    }
                }
                Ok(std::mem::take(scratch))
            },
            |_seq, refs| {
                for id in refs {
                    extra_node_ids.insert(id);
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
    let coords = crate::owned::WayCoords::of_input(&header);
    let bbox = region.bbox();
    let mut writer = writer_from_header(
        output,
        compression,
        &header,
        &super::output_facts(set_bounds, bbox, coords),
        overrides,
        backend,
        host,
    )?;

    let pass1_blob_schedule = match result.schedules.take() {
        Some(schedules) => Some(write_schedule_for(
            schedules,
            &[
                &result.bbox_node_ids,
                &result.outside_way_node_ids,
                &extra_node_ids,
            ],
            &[&result.matched_way_ids, &handler.extra_way_ids],
            &[&result.matched_relation_ids],
            host,
        )?),
        None => None,
    };

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
    // The pass 1 schedule, cut by span, on sorted input; the unsorted
    // fallback builds a full one.
    if let Some((pass1_blob_schedule, retained_nodes)) = pass1_blob_schedule {
        pread_write_pass_with_schedule(
            input,
            &pass1_blob_schedule,
            retained_nodes,
            &mut writer,
            &mut stats,
            |block, bb, output_blocks| {
                extract_block_pass3(block, &ids, clean, coords, bb, output_blocks)
            },
            host,
        )?;
    } else {
        pread_write_pass(
            input,
            &mut writer,
            &mut stats,
            |block, bb, output_blocks| {
                extract_block_pass3(block, &ids, clean, coords, bb, output_blocks)
            },
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

use super::super::clean_partial_metadata;
use crate::owned::element_metadata;

/// Process a single block for Pass 3 of smart extraction: write elements whose IDs
/// were collected in Passes 1+2. Uses thread-local BlockBuilder and output buffer.
#[hotpath::measure]
fn extract_block_pass3<O: crate::commands::BlockSink + ?Sized>(
    block: &SelectionBlock,
    ids: &ExtractPass3IdSets<'_>,
    clean: &CleanAttrs,
    coords: crate::owned::WayCoords,
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
    let mut way = crate::owned::WayScratch::default();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    let selected = |kind, id| match kind {
        crate::ElementKind::Node => {
            ids.bbox_node_ids.get(id)
                || ids.outside_way_node_ids.get(id)
                || ids.extra_node_ids.get(id)
        }
        crate::ElementKind::Way => ids.matched_way_ids.get(id) || ids.extra_way_ids.get(id),
        crate::ElementKind::Relation => ids.matched_relation_ids.get(id),
    };
    block.visit_retained(selected, |element| {
        match &element {
            Element::DenseNode(dn) => {
                let id = dn.id();
                let in_bbox = ids.bbox_node_ids.get(id);
                let from_way = ids.outside_way_node_ids.get(id);
                ensure_node_capacity_local(bb, output)?;
                add_dense_node(bb, dn, clean)?;
                if in_bbox {
                    stats.nodes_in_bbox += 1;
                } else if from_way {
                    stats.nodes_from_ways += 1;
                } else {
                    stats.nodes_from_relations += 1;
                }
            }
            Element::Node(n) => {
                let id = n.id();
                let in_bbox = ids.bbox_node_ids.get(id);
                let from_way = ids.outside_way_node_ids.get(id);
                ensure_node_capacity_local(bb, output)?;
                add_plain_node(bb, n, clean)?;
                if in_bbox {
                    stats.nodes_in_bbox += 1;
                } else if from_way {
                    stats.nodes_from_ways += 1;
                } else {
                    stats.nodes_from_relations += 1;
                }
            }
            Element::Way(w) => {
                let in_matched = ids.matched_way_ids.get(w.id());
                let in_extra = ids.extra_way_ids.get(w.id());
                ensure_way_capacity_local(bb, output)?;
                way.prepare(w, coords)?;
                let meta = clean_partial_metadata(
                    element_metadata(crate::ElementKind::Way, w.id(), &w.info())?,
                    clean,
                );
                way.write(bb, w.tags(), meta.as_ref());
                if in_extra && !in_matched {
                    stats.ways_from_relations += 1;
                } else {
                    stats.ways_written += 1;
                }
            }
            Element::Relation(r) => {
                ensure_relation_capacity_local(bb, output)?;
                r.members_into(&mut members_buf)?;
                let meta = clean_partial_metadata(
                    element_metadata(crate::ElementKind::Relation, r.id(), &r.info())?,
                    clean,
                );
                bb.add_relation_partial(r.id(), r.tags(), &members_buf, meta.as_ref());
                stats.relations_written += 1;
            }
        }
        Ok(())
    })?;
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
fn is_smart_relation(r: &ClassificationRelation<'_>) -> Result<bool> {
    Ok(matches!(
        r.tag_value("type")?,
        Some("multipolygon" | "boundary")
    ))
}

/// Record a `(member, parent)` edge for every relation-type member of `r`.
///
/// Every relation contributes, matched or not: whether a parent is in the
/// extract depends on its members' membership, which is only known once
/// every relation blob has been classified.
pub(super) fn push_parent_edges(r: &ClassificationRelation<'_>, edges: &mut Vec<(i64, i64)>) {
    let parent = r.id();
    for m in r.members() {
        if let MemberId::Relation(child) = m {
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
    let mut work: Vec<i64> = included.iter_canonical().collect();
    let mut added: u64 = 0;
    while let Some(child) = work.pop() {
        let start = edges.partition_point(|&(c, _)| c < child);
        for &(c, parent) in &edges[start..] {
            if c != child {
                break;
            }
            if included.insert_if_new(parent) {
                added += 1;
                work.push(parent);
            }
        }
    }
    host.invocation
        .counter("extract_relations_from_parents", added);
}
