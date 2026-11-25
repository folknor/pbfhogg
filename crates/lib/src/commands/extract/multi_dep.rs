//! Multi-extract for the dependency strategies (complete-ways and smart) on
//! sorted input: one run over the input for all N regions, each region's
//! output equal to the single-region extract of that region.
//!
//! The single-region engines (complete.rs, smart.rs) hold one `IdSet` per
//! selection; here each selection is a shared [`Membership`] holding, per
//! ID, the mask of regions it belongs to. N per-region `IdSet`s (osmium's
//! shape) are not used: they lost on memory when measured
//! (reference/performance/multi-extract.md, "Shared region membership").
//!
//! The masks, each per region equal to the single-region set:
//!
//! 1. Node classify: `spatial`, the regions a node lies in.
//! 2. Way classify, per way record: the record's mask is the OR of its
//!    refs' spatial masks. It is ORed into `matched_way` by way ID, and the
//!    record's refs get that same record mask (less the regions each ref is
//!    already spatial in) in `way_nodes`. A version's refs therefore never
//!    reach a region only another version of the way matched, as in the
//!    single-region pass, which adds the refs of the record that matched.
//! 3. Relation classify, per record: the direct mask from its node members'
//!    spatial and way members' matched-way masks; every record, matched or
//!    not, contributes its child-to-parent edges. Under smart, a record with
//!    a direct mask completes its members ([`completion_members`], judged
//!    on that record's own first `type` tag): a member way gets
//!    `direct & !matched_way[way]` in `extra_way`, a member node gets the
//!    direct mask in `relation_nodes`.
//! 4. Parent closure: relation masks seeded with the union of the direct
//!    masks by ID, propagated from child to parent on any bit gain, so a
//!    parent already in region A still gains region B. Parent-added bits
//!    never complete anything, and inclusion never travels down.
//! 5. Smart only: one sweep of the way blobs puts the refs of every
//!    occurrence of a way with a nonzero `extra_way` mask into
//!    `relation_nodes` with that mask.
//! 6. One write pass over the full blob schedule. A node goes to
//!    `spatial | way_nodes | relation_nodes`, a way to
//!    `matched_way | extra_way`, a relation to its closed mask. Each mask
//!    holds only what an earlier one does not (the single-region sets are
//!    kept disjoint the same way), and the region's stats count by that
//!    precedence: spatial nodes in `nodes_in_bbox`, way-completion nodes in
//!    `nodes_from_ways`, the rest in `nodes_from_relations`; matched ways in
//!    `ways_written`, extra-only ways in `ways_from_relations`.
//!
//! Membership installs may arrive in any order and overlap: `BatchBuilder`
//! sorts and ORs a blob's pushes, `install` merges by page, `freeze`
//! re-encodes, so a mask built from way or relation order keyed by node or
//! way ID is the same whatever order the blobs finish in.
//!
//! Non-indexed (`--force`) input: a blob with no indexdata joins every
//! classify schedule, each phase acting only on its own kind, and the write
//! pass reads each physical blob once, as the single-region engines do.

use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::writer::{Compression, PbfWriter};
use crate::{Element, ElementKind, PrimitiveBlock};

use super::super::{
    HeaderOverrides, Result, clean_metadata, clean_partial_metadata, ensure_node_capacity_local,
    ensure_relation_capacity_local, ensure_way_capacity_local, flush_local,
};
use crate::read::input::PinnedInput;
use crate::scan::classify::ScheduledBlob;

use super::common::{
    BboxInt, BlobDesc, MatchableMember, Passthrough, matchable_members, spatial_blob_filter,
};
use super::membership::{
    BatchBuilder, Membership, PageBatch, admits, full_mask, mask_words, regions_of,
};
use super::multi::{
    RegionFraming, node_blob_regions, open_region_writers, probe_sorted_header, region_grid,
    union_bbox,
};
use super::smart::{CompletionMember, completion_members, push_parent_edges};
use super::{ExtractStats, ExtractStrategy, Region};

use crate::owned::{dense_node_metadata, element_metadata};

/// Whether `region`'s bit is set in `mask`.
#[inline]
fn has(mask: &[u64], region: usize) -> bool {
    (mask[region / 64] >> (region % 64)) & 1 == 1
}

/// `out = a & !b`, word by word; returns whether any bit is left.
#[inline]
fn and_not(a: &[u64], b: &[u64], out: &mut [u64]) -> bool {
    let mut any = 0;
    for ((o, &x), &y) in out.iter_mut().zip(a).zip(b) {
        *o = x & !y;
        any |= *o;
    }
    any != 0
}

fn empty_stats(strategy: &'static str) -> ExtractStats {
    ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_written: 0,
        ways_from_relations: 0,
        relations_written: 0,
        strategy,
    }
}

/// Run complete-ways or smart for every region in one run over a sorted
/// input. Returns `None`, before any output is written, when the input does
/// not declare itself sorted (or the probe cannot read it): the caller then
/// runs one extract per region. `strategy` must be `CompleteWays` or
/// `Smart`.
///
/// Every output is written through a buffered sync writer whatever the
/// configured backend, as the simple single pass writes; the pool workers
/// compress and frame each region's blocks (`RegionFraming`). No node blob
/// is copied raw.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cognitive_complexity
)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn try_extract_multi_dependent(
    input: &PinnedInput,
    regions: &[Region],
    outputs: &crate::output::BoundOutputSet,
    strategy: ExtractStrategy,
    set_bounds: bool,
    clean: &crate::cat::CleanAttrs,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<Option<Vec<ExtractStats>>> {
    let (smart, name) = match strategy {
        ExtractStrategy::CompleteWays => (false, "complete_ways"),
        ExtractStrategy::Smart => (true, "smart"),
        ExtractStrategy::Simple => {
            return Err(crate::error::Error::internal(
                "multi-extract: the dependency engine was handed the simple strategy",
            ));
        }
    };
    let Some(header) = probe_sorted_header(input, direct_io, host)? else {
        return Ok(None);
    };

    let n = regions.len();
    host.invocation.marker("MULTI_DEP_EXTRACT_START");
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "extract.multi.single_pass",
        "[multi-extract] single-pass: {n} regions, {name} strategy"
    );

    let bbox_ints: Vec<BboxInt> = regions
        .iter()
        .map(|region| BboxInt::from_bbox(region.bbox()))
        .collect();
    // Point membership runs against the exact boxes; `bbox_ints` are the
    // conservative ones, for blob selection only.
    let inside_bbox_ints: Vec<BboxInt> = regions
        .iter()
        .map(|region| BboxInt::inside_bbox(region.bbox()))
        .collect();
    let spatial_filter = spatial_blob_filter(&union_bbox(&bbox_ints));

    // --- Schedules, from one header-only walk ---
    host.invocation.marker("MULTI_DEP_SCHEDULE_SCAN_START");
    let mut walker = crate::read::header_walker::HeaderWalker::open(input, host)?;
    let _ = walker
        .next_header()?
        .ok_or_else(|| crate::error::new_missing_header(input.path()))?;
    let mut node_schedule: Vec<ScheduledBlob> = Vec::new();
    // Each node-schedule position's region admission
    // (`multi::node_blob_regions`), pushed beside it: the classify pool
    // takes a bare schedule and hands back the position.
    let mut node_admit: Vec<Option<Box<[u64]>>> = Vec::new();
    // Way blobs are never skipped by the spatial filter, so this one
    // schedule serves the way classify and smart's extra-way sweep.
    let mut way_schedule: Vec<ScheduledBlob> = Vec::new();
    let mut relation_schedule: Vec<ScheduledBlob> = Vec::new();
    // Every OsmData blob, unfiltered, for the write pass: the schedule the
    // single-region engines' write pass runs over, so its decode validation
    // is theirs (no kind-claim refusal there).
    let mut write_schedule: Vec<BlobDesc> = Vec::new();
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        walker.observe_filter_index(meta.index.as_ref());
        let idx = meta.index.as_ref();
        let entry = ScheduledBlob::from_meta(&meta, crate::scan::classify::KindClaims::Refuse)?;
        write_schedule.push(BlobDesc {
            frame_offset: meta.frame_start,
            frame_size: meta.frame_size,
            offset: meta.data_offset,
            size: meta.data_size,
            kind: idx.map(|i| i.kind),
            bbox: idx.and_then(|i| i.bbox),
            index: idx.copied(),
            passthrough: Passthrough::No,
        });
        // Kind and bbox are indexdata claims, believed without a decode
        // (reference/indexdata-trust.md), as in the single-region engines.
        if let Some(idx) = idx {
            if !spatial_filter.wants_index(idx) {
                continue;
            }
            match idx.kind {
                ElementKind::Node => {
                    // Classified only for the regions it is read for, as in
                    // each single extract; one between regions is skipped.
                    let admit = node_blob_regions(&bbox_ints, Some(idx));
                    if admit
                        .as_deref()
                        .is_some_and(|mask| mask.iter().all(|&word| word == 0))
                    {
                        continue;
                    }
                    node_schedule.push(entry);
                    node_admit.push(admit);
                }
                ElementKind::Way => way_schedule.push(entry),
                ElementKind::Relation => relation_schedule.push(entry),
            }
        } else {
            // Non-indexed blob (force path): every phase sees it and acts
            // on its own kind only.
            node_schedule.push(entry);
            node_admit.push(None);
            way_schedule.push(entry);
            relation_schedule.push(entry);
        }
    }
    host.invocation.marker("MULTI_DEP_SCHEDULE_SCAN_END");
    let shared_file = walker.input();
    drop(walker);

    let mw = mask_words(n);
    let all_regions = full_mask(n);

    // --- Node classify: spatial ---
    host.invocation.marker("MULTI_DEP_NODE_CLASSIFY_START");
    let mut spatial = Membership::new(n);
    let region_bboxes: Vec<(i32, i32, i32, i32)> = inside_bbox_ints
        .iter()
        .map(|bi| (bi.min_lat, bi.max_lat, bi.min_lon, bi.max_lon))
        .collect();
    let grid = region_grid(&region_bboxes, host);
    let all_bbox = regions
        .iter()
        .all(|region| matches!(region, Region::Bbox(_)));
    if all_bbox {
        let bboxes = &region_bboxes;
        let grid = &grid;
        let node_admit = &node_admit;
        crate::scan::classify::parallel_project_phase(
            &shared_file,
            &node_schedule,
            None,
            || {
                (
                    crate::read::columnar::DenseNodeColumns::new(),
                    vec![Vec::<i64>::new(); n],
                    BatchBuilder::new(n),
                )
            },
            |position, buffer, (columns, scratch, builder)| {
                let block = buffer.block()?;
                block.decode_dense_columns(columns)?;
                for v in scratch.iter_mut() {
                    v.clear();
                }
                match grid {
                    Some(grid) => {
                        columns.collect_matching_ids_multi_bbox_grid(bboxes, grid, scratch);
                    }
                    None => columns.collect_matching_ids_multi_bbox(bboxes, scratch),
                }
                // Dense columns deliberately exclude plain Node messages.
                for group in block.groups() {
                    for node in group.nodes() {
                        let (lat, lon) = super::common::plain_coords(&node)?;
                        for (region, &(min_lat, max_lat, min_lon, max_lon)) in
                            bboxes.iter().enumerate()
                        {
                            if lat >= min_lat && lat <= max_lat && lon >= min_lon && lon <= max_lon
                            {
                                scratch[region].push(node.id());
                            }
                        }
                    }
                }
                let admit = node_admit[position].as_deref();
                for (region, ids) in scratch.iter().enumerate() {
                    if !admits(admit, region) {
                        continue;
                    }
                    for &id in ids {
                        builder.push(id, region);
                    }
                }
                Ok(builder.finish())
            },
            |_seq, batch: PageBatch| spatial.install(batch),
            host,
        )?;
    } else {
        let grid = &grid;
        let inside = &inside_bbox_ints;
        let node_admit = &node_admit;
        crate::scan::classify::parallel_project_phase(
            &shared_file,
            &node_schedule,
            None,
            || BatchBuilder::new(n),
            |position, buffer, builder: &mut BatchBuilder| {
                let block = buffer.block()?;
                let admit = node_admit[position].as_deref();
                for element in block.elements_skip_metadata() {
                    let (id, lat, lon) = match &element {
                        Element::DenseNode(node) => (
                            node.id(),
                            node.decimicro_lat().ok(),
                            node.decimicro_lon().ok(),
                        ),
                        Element::Node(node) => (
                            node.id(),
                            node.decimicro_lat().ok(),
                            node.decimicro_lon().ok(),
                        ),
                        _ => continue,
                    };
                    let (lat, lon) = super::common::checked_coords(lat, lon)?;
                    match grid {
                        Some(grid) => {
                            for &region in grid.candidates(lat, lon) {
                                let region = region as usize;
                                if admits(admit, region)
                                    && regions[region].contains_decimicro(&inside[region], lat, lon)
                                {
                                    builder.push(id, region);
                                }
                            }
                        }
                        None => {
                            for region in 0..n {
                                if admits(admit, region)
                                    && regions[region].contains_decimicro(&inside[region], lat, lon)
                                {
                                    builder.push(id, region);
                                }
                            }
                        }
                    }
                }
                Ok(builder.finish())
            },
            |_seq, batch: PageBatch| spatial.install(batch),
            host,
        )?;
    }
    spatial.freeze();
    host.invocation.marker("MULTI_DEP_NODE_CLASSIFY_END");

    // --- Way classify: matched_way by ID, way_nodes per record ---
    //
    // A block walk, as the single-region way classify is: every way's refs
    // are tested against the node membership, and that, not the message
    // parse, is the phase's cost.
    host.invocation.marker("MULTI_DEP_WAY_CLASSIFY_START");
    let mut matched_way = Membership::new(n);
    let mut way_nodes = Membership::new(n);
    {
        let spatial = &spatial;
        let all_regions = &all_regions;
        crate::scan::classify::parallel_classify_phase(
            &shared_file,
            &way_schedule,
            None,
            || {
                (
                    BatchBuilder::new(n),
                    BatchBuilder::new(n),
                    vec![0u64; mw],
                    vec![0u64; mw],
                    vec![0u64; mw],
                )
            },
            |block, (ways, nodes, record, ref_mask, add)| {
                for element in block.elements_skip_metadata() {
                    let Element::Way(w) = &element else {
                        continue;
                    };
                    record.fill(0);
                    for r in w.refs() {
                        if spatial.or_into(r, record) && record.as_slice() == all_regions.as_slice()
                        {
                            break;
                        }
                    }
                    if record.iter().all(|&word| word == 0) {
                        continue;
                    }
                    ways.push_mask(w.id(), record);
                    // This record's refs join its regions, less those each
                    // ref is already spatial in (`outside_way_node_ids`'
                    // rule, per region).
                    for r in w.refs() {
                        spatial.mask_into(r, ref_mask);
                        if and_not(record, ref_mask, add) {
                            nodes.push_mask(r, add);
                        }
                    }
                }
                Ok((ways.finish(), nodes.finish()))
            },
            |_seq, (ways, nodes): (PageBatch, PageBatch)| {
                matched_way.install(ways);
                way_nodes.install(nodes);
            },
            host,
        )?;
    }
    matched_way.freeze();
    way_nodes.freeze();
    host.invocation.marker("MULTI_DEP_WAY_CLASSIFY_END");

    // --- Relation classify: direct masks, edges, smart extras ---
    //
    // Per-blob send: a worker-held accumulation of relation-driven members,
    // dispersed over the ID space, grows with the worker count (the
    // single-region phase's measured reason for the same shape).
    host.invocation.marker("MULTI_DEP_RELATION_CLASSIFY_START");
    let mut relations = RelationMasks::new(mw);
    let mut parent_edges: Vec<(i64, i64)> = Vec::new();
    let mut extra_way = Membership::new(n);
    let mut relation_nodes = Membership::new(n);
    {
        let spatial = &spatial;
        let matched_way = &matched_way;
        let way_nodes = &way_nodes;
        let all_regions = &all_regions;
        crate::scan::classify::parallel_classify_phase(
            &shared_file,
            &relation_schedule,
            None,
            || {
                (
                    BatchBuilder::new(n),
                    BatchBuilder::new(n),
                    vec![0u64; mw],
                    vec![0u64; mw],
                    vec![0u64; mw],
                )
            },
            |block, (extra_ways, extra_nodes, direct, member_mask, add)| {
                let mut ids: Vec<i64> = Vec::new();
                let mut masks: Vec<u64> = Vec::new();
                let mut edges: Vec<(i64, i64)> = Vec::new();
                for element in block.elements_skip_metadata() {
                    let Element::Relation(r) = &element else {
                        continue;
                    };
                    push_parent_edges(r, &mut edges);
                    direct.fill(0);
                    for member in matchable_members(r) {
                        let hit = match member {
                            MatchableMember::Node(id) => spatial.or_into(id, direct),
                            MatchableMember::Way(id) => matched_way.or_into(id, direct),
                        };
                        if hit && direct.as_slice() == all_regions.as_slice() {
                            break;
                        }
                    }
                    if direct.iter().all(|&word| word == 0) {
                        continue;
                    }
                    ids.push(r.id());
                    masks.extend_from_slice(direct);
                    if !smart {
                        continue;
                    }
                    for member in completion_members(r) {
                        match member {
                            CompletionMember::Way(id) => {
                                // The extra-way exclusion, per region: a way
                                // matched in a region is written there as
                                // matched.
                                matched_way.mask_into(id, member_mask);
                                if and_not(direct, member_mask, add) {
                                    extra_ways.push_mask(id, add);
                                }
                            }
                            CompletionMember::Node(id) => {
                                spatial.mask_into(id, member_mask);
                                way_nodes.or_into(id, member_mask);
                                if and_not(direct, member_mask, add) {
                                    extra_nodes.push_mask(id, add);
                                }
                            }
                        }
                    }
                }
                Ok((ids, masks, edges, extra_ways.finish(), extra_nodes.finish()))
            },
            |_seq, (ids, masks, edges, ways, nodes): RelationBlob| {
                for (id, mask) in ids.iter().zip(masks.chunks_exact(mw)) {
                    relations.or_direct(*id, mask);
                }
                parent_edges.extend(edges);
                extra_way.install(ways);
                relation_nodes.install(nodes);
            },
            host,
        )?;
    }
    extra_way.freeze();
    host.invocation.marker("MULTI_DEP_RELATION_CLASSIFY_END");

    host.invocation.marker("MULTI_DEP_RELATION_PARENTS_START");
    let relation_members = relations.close_over_parents(parent_edges, n, host);
    host.invocation.marker("MULTI_DEP_RELATION_PARENTS_END");

    // --- Smart: the nodes of extra ways, from every occurrence ---
    if smart {
        host.invocation.marker("MULTI_DEP_EXTRA_WAY_SWEEP_START");
        let spatial = &spatial;
        let way_nodes = &way_nodes;
        let extra_way = &extra_way;
        crate::scan::classify::parallel_classify_phase(
            &shared_file,
            &way_schedule,
            None,
            || {
                (
                    BatchBuilder::new(n),
                    vec![0u64; mw],
                    vec![0u64; mw],
                    vec![0u64; mw],
                )
            },
            |block, (nodes, way_mask, ref_mask, add)| {
                for element in block.elements_skip_metadata() {
                    let Element::Way(w) = &element else {
                        continue;
                    };
                    if !extra_way.mask_into(w.id(), way_mask) {
                        continue;
                    }
                    for r in w.refs() {
                        spatial.mask_into(r, ref_mask);
                        way_nodes.or_into(r, ref_mask);
                        if and_not(way_mask, ref_mask, add) {
                            nodes.push_mask(r, add);
                        }
                    }
                }
                Ok(nodes.finish())
            },
            |_seq, batch: PageBatch| relation_nodes.install(batch),
            host,
        )?;
        host.invocation.marker("MULTI_DEP_EXTRA_WAY_SWEEP_END");
    }
    relation_nodes.freeze();
    drop(shared_file);

    // --- Write pass ---
    host.invocation.marker("MULTI_DEP_WRITE_START");
    let coords = crate::owned::WayCoords::of_input(&header);
    let (mut writers, framing) = open_region_writers(
        &header,
        regions,
        outputs,
        set_bounds,
        coords,
        compression,
        overrides,
        host,
    )?;
    let sets = WriteSets {
        spatial: &spatial,
        way_nodes: &way_nodes,
        relation_nodes: &relation_nodes,
        matched_way: &matched_way,
        extra_way: &extra_way,
        relations: &relation_members,
        mw,
        clean,
        coords,
        strategy: name,
    };
    let mut stats: Vec<ExtractStats> = (0..n).map(|_| empty_stats(name)).collect();
    write_pass(
        input,
        &write_schedule,
        &sets,
        &framing,
        &mut writers,
        &mut stats,
        host,
    )?;
    for (writer, output) in writers.iter_mut().zip(outputs.iter()) {
        let display_path = output.destination();
        writer
            .flush()
            .map_err(|e| e.context(format!("failed to flush {}", display_path.display())))?;
    }
    host.invocation.marker("MULTI_DEP_WRITE_END");

    host.invocation.counter("multi_extract_region_count", n);
    host.invocation
        .counter("multi_extract_node_blobs", node_schedule.len());
    host.invocation
        .counter("multi_extract_way_blobs", way_schedule.len());
    host.invocation
        .counter("multi_extract_relation_blobs", relation_schedule.len());
    host.invocation
        .counter("multi_extract_write_blobs", write_schedule.len());
    let sum = |field: fn(&ExtractStats) -> u64| stats.iter().map(field).sum::<u64>();
    host.invocation
        .counter("multi_extract_nodes_written", sum(|s| s.nodes_in_bbox));
    host.invocation
        .counter("multi_extract_nodes_from_ways", sum(|s| s.nodes_from_ways));
    host.invocation.counter(
        "multi_extract_nodes_from_relations",
        sum(|s| s.nodes_from_relations),
    );
    host.invocation
        .counter("multi_extract_ways_written", sum(|s| s.ways_written));
    host.invocation.counter(
        "multi_extract_ways_from_relations",
        sum(|s| s.ways_from_relations),
    );
    host.invocation.counter(
        "multi_extract_relations_written",
        sum(|s| s.relations_written),
    );
    host.invocation.marker("MULTI_DEP_EXTRACT_END");
    Ok(Some(stats))
}

/// One relation blob's classify output: directly matched relation IDs,
/// their masks (`mask_words` each, in ID order), the blob's parent edges,
/// and smart's extra-way and relation-node pages.
type RelationBlob = (Vec<i64>, Vec<u64>, Vec<(i64, i64)>, PageBatch, PageBatch);

// ---------------------------------------------------------------------------
// Relation masks and the parent closure
// ---------------------------------------------------------------------------

/// Relation masks by ID while relations are classified and closed: mutable,
/// unlike a [`Membership`], because the closure adds bits to relations
/// already present. Holds only relations with a nonzero mask.
struct RelationMasks {
    mw: usize,
    index: rustc_hash::FxHashMap<i64, usize>,
    masks: Vec<u64>,
    /// Relations directly matched in some region (the closure's seeds).
    direct: usize,
}

impl RelationMasks {
    fn new(mw: usize) -> Self {
        Self {
            mw,
            index: rustc_hash::FxHashMap::default(),
            masks: Vec::new(),
            direct: 0,
        }
    }

    /// OR `mask` into `id`'s mask, inserting it when absent. Returns
    /// whether any bit was gained and whether `id` was new.
    fn or(&mut self, id: i64, mask: &[u64]) -> (bool, bool) {
        let mw = self.mw;
        let next = self.index.len();
        let at = *self.index.entry(id).or_insert(next);
        let new = at == next;
        if new {
            self.masks.resize(self.masks.len() + mw, 0);
        }
        let mut gained = false;
        for (slot, &bit) in self.masks[at * mw..(at + 1) * mw].iter_mut().zip(mask) {
            gained |= bit & !*slot != 0;
            *slot |= bit;
        }
        (gained, new)
    }

    /// A directly matched record's mask, ORed by ID (several versions of a
    /// relation share their regions).
    fn or_direct(&mut self, id: i64, mask: &[u64]) {
        let (_, new) = self.or(id, mask);
        if new {
            self.direct += 1;
        }
    }

    /// Add every parent of an included relation, recursively, region by
    /// region: osmium's `add_relation_parents` per region, the rule
    /// `smart::add_parent_relations` applies to one region. A parent gains
    /// its child's bits; on any gain it is walked again, so a parent
    /// already included in one region still carries a new region to its
    /// own parents. Self-membership and cycles end because a relation is
    /// only revisited on a gain, and a cycle no seed reaches adds nothing.
    /// Returns the closed masks as a [`Membership`].
    fn close_over_parents(
        mut self,
        mut edges: Vec<(i64, i64)>,
        n: usize,
        host: &crate::host::Entered<'_>,
    ) -> Membership {
        host.invocation
            .counter("extract_relation_parent_edges", edges.len());
        edges.sort_unstable();
        edges.dedup();
        let mw = self.mw;
        let mut work: Vec<i64> = self.index.keys().copied().collect();
        let mut child_mask = vec![0u64; mw];
        let mut added: u64 = 0;
        while let Some(child) = work.pop() {
            let Some(&at) = self.index.get(&child) else {
                continue;
            };
            child_mask.copy_from_slice(&self.masks[at * mw..(at + 1) * mw]);
            let start = edges.partition_point(|&(c, _)| c < child);
            for &(c, parent) in &edges[start..] {
                if c != child {
                    break;
                }
                let (gained, new) = self.or(parent, &child_mask);
                if new {
                    added += 1;
                }
                if gained {
                    work.push(parent);
                }
            }
        }
        host.invocation
            .counter("extract_relations_from_parents", added);
        host.invocation
            .counter("multi_extract_relations_direct", self.direct);

        let mut builder = BatchBuilder::new(n);
        for (&id, &at) in &self.index {
            builder.push_mask(id, &self.masks[at * mw..(at + 1) * mw]);
        }
        drop(self);
        let mut members = Membership::new(n);
        members.install(builder.finish());
        members.freeze();
        members
    }
}

// ---------------------------------------------------------------------------
// Write pass
// ---------------------------------------------------------------------------

/// The final masks the write pass reads, shared across the workers.
struct WriteSets<'a> {
    spatial: &'a Membership,
    way_nodes: &'a Membership,
    relation_nodes: &'a Membership,
    matched_way: &'a Membership,
    extra_way: &'a Membership,
    relations: &'a Membership,
    mw: usize,
    clean: &'a crate::cat::CleanAttrs,
    coords: crate::owned::WayCoords,
    strategy: &'static str,
}

/// One write worker's thread-local state.
struct WriteLocal {
    read_buf: crate::read::read_buf::ReadBuf,
    bbs: Vec<BlockBuilder>,
    output: Vec<Vec<OwnedBlock>>,
    pool: std::sync::Arc<crate::blob::DecompressPool>,
    st_scratch: Vec<(u32, u32)>,
    gr_scratch: Vec<(u32, u32)>,
    way: crate::owned::WayScratch,
    /// Three masks: two per-set lookups and their union.
    masks: [Vec<u64>; 3],
}

/// One write task's output: each region's framed blobs and its stats.
type RegionWrite = (Vec<Vec<Vec<u8>>>, Vec<ExtractStats>);

/// The write pass: every OsmData blob in file order, decoded once by the
/// pool workers, each element written to every region of its kind's mask
/// union, the per-region blocks framed on the workers and copied to the N
/// writers in schedule order. The shape of `common::pread_execute`, the
/// single-region engines' write pass, fanned out to N regions.
#[allow(clippy::too_many_arguments)]
fn write_pass(
    input: &PinnedInput,
    schedule: &[BlobDesc],
    sets: &WriteSets<'_>,
    framing: &RegionFraming<'_>,
    writers: &mut [PbfWriter<std::io::BufWriter<std::fs::File>>],
    stats: &mut [ExtractStats],
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    if schedule.is_empty() {
        return host.invocation.check();
    }
    let n = writers.len();
    let file = input.open()?;
    let file = &file;
    let source: std::sync::Arc<std::path::Path> = std::sync::Arc::from(input.path());
    let source = &source;
    let decode_threads = crate::host::sized_workers(
        &*host.resources,
        crate::tunables::DECODE_WORKERS_RESERVED,
        crate::tunables::DECODE_WORKERS_MIN,
        crate::tunables::DECODE_WORKERS_FALLBACK,
    );
    let mut workers = vec![(); decode_threads];
    let mut consumer =
        crate::ordered_pool::ConsumeFn(|_task, (frames, counts): RegionWrite| -> Result<()> {
            for (i, (frames, count)) in frames.into_iter().zip(&counts).enumerate() {
                super::common::merge_extract_stats(&mut stats[i], count);
                for frame in frames {
                    writers[i].write_raw_owned(frame)?;
                }
            }
            Ok(())
        });
    crate::ordered_pool::run_ordered(
        schedule.len(),
        &mut workers,
        crate::ordered_pool::PoolConfig {
            window: crate::ordered_pool::default_window(decode_threads),
            cancel: &host.invocation,
            waits: crate::ordered_pool::Waits::default(),
        },
        |_| WriteLocal {
            read_buf: crate::read::read_buf::ReadBuf::default(),
            bbs: (0..n).map(|_| BlockBuilder::new()).collect(),
            output: (0..n).map(|_| Vec::new()).collect(),
            pool: crate::blob::DecompressPool::new(&host.invocation),
            st_scratch: Vec::new(),
            gr_scratch: Vec::new(),
            way: crate::owned::WayScratch::default(),
            masks: [
                vec![0u64; sets.mw],
                vec![0u64; sets.mw],
                vec![0u64; sets.mw],
            ],
        },
        |(), local: &mut WriteLocal, task| {
            let d = &schedule[task];
            let result = (|| -> Result<RegionWrite> {
                let raw = local.read_buf.read_exact_at(file, d.offset, d.size)?;
                let mut buf = crate::blob::pool_get(Some(&local.pool), 0);
                crate::blob::decompress_blob_raw(raw, &mut buf)?;
                let block = PrimitiveBlock::from_vec_pooled_with_scratch(
                    buf,
                    &local.pool,
                    &mut local.st_scratch,
                    &mut local.gr_scratch,
                )?;
                for v in &mut local.output {
                    v.clear();
                }
                let counts = write_block(&block, sets, local)?;
                let mut frames = Vec::with_capacity(n);
                for i in 0..n {
                    flush_local(&mut local.bbs[i], &mut local.output[i])?;
                    frames.push(framing.frame_region(i, local.output[i].drain(..))?);
                }
                Ok((frames, counts))
            })()
            // Cold: only a failure builds the location.
            .map_err(|e| {
                e.locate(crate::error::Location::new(
                    Some(source),
                    None,
                    Some(crate::ByteOffset(d.frame_offset)),
                ))
            });
            // The read buffer follows its retention limit (read/read_buf.rs).
            local.read_buf.release_outlier(&host.invocation);
            result
        },
        &mut consumer,
    )
}

/// Write one decoded block's selected elements to each region's builder,
/// returning each region's counts.
#[allow(clippy::too_many_lines)]
fn write_block(
    block: &PrimitiveBlock,
    sets: &WriteSets<'_>,
    local: &mut WriteLocal,
) -> crate::Result<Vec<ExtractStats>> {
    let n = local.bbs.len();
    let mut counts: Vec<ExtractStats> = (0..n).map(|_| empty_stats(sets.strategy)).collect();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();
    let WriteLocal {
        bbs,
        output,
        way,
        masks,
        ..
    } = local;
    let [first, second, union] = masks;
    let clean = sets.clean;

    // A node's regions, with the spatial and way-completion masks kept for
    // the stats precedence. False when it is in no region.
    let node_masks = |id: i64, spatial: &mut [u64], from_way: &mut [u64], union: &mut [u64]| {
        let a = sets.spatial.mask_into(id, spatial);
        let b = sets.way_nodes.mask_into(id, from_way);
        let c = sets.relation_nodes.mask_into(id, union);
        for ((u, &s), &w) in union.iter_mut().zip(spatial.iter()).zip(from_way.iter()) {
            *u |= s | w;
        }
        a || b || c
    };
    let count_node = |counts: &mut [ExtractStats], i: usize, spatial: &[u64], from_way: &[u64]| {
        if has(spatial, i) {
            counts[i].nodes_in_bbox += 1;
        } else if has(from_way, i) {
            counts[i].nodes_from_ways += 1;
        } else {
            counts[i].nodes_from_relations += 1;
        }
    };

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                let id = dn.id();
                if !node_masks(id, first, second, union) {
                    continue;
                }
                let (lat, lon) = super::common::dense_coords(dn)?;
                let meta = clean_metadata(dense_node_metadata(dn)?, clean);
                for i in regions_of(union) {
                    ensure_node_capacity_local(&mut bbs[i], &mut output[i])?;
                    bbs[i].add_node_dense(id, lat, lon, dn.tags(), meta.as_ref());
                    count_node(&mut counts, i, first, second);
                }
            }
            Element::Node(nd) => {
                let id = nd.id();
                if !node_masks(id, first, second, union) {
                    continue;
                }
                let (lat, lon) = super::common::plain_coords(nd)?;
                let meta = clean_partial_metadata(
                    element_metadata(ElementKind::Node, id, &nd.info())?,
                    clean,
                );
                for i in regions_of(union) {
                    ensure_node_capacity_local(&mut bbs[i], &mut output[i])?;
                    bbs[i].add_node_partial(id, lat, lon, nd.tags(), meta.as_ref());
                    count_node(&mut counts, i, first, second);
                }
            }
            Element::Way(w) => {
                let id = w.id();
                let matched = sets.matched_way.mask_into(id, first);
                let extra = sets.extra_way.mask_into(id, second);
                if !matched && !extra {
                    continue;
                }
                for ((u, &m), &x) in union.iter_mut().zip(first.iter()).zip(second.iter()) {
                    *u = m | x;
                }
                way.prepare(w, sets.coords)?;
                let meta = clean_partial_metadata(
                    element_metadata(ElementKind::Way, id, &w.info())?,
                    clean,
                );
                for i in regions_of(union) {
                    ensure_way_capacity_local(&mut bbs[i], &mut output[i])?;
                    way.write(&mut bbs[i], w.tags(), meta.as_ref());
                    if has(first, i) {
                        counts[i].ways_written += 1;
                    } else {
                        counts[i].ways_from_relations += 1;
                    }
                }
            }
            Element::Relation(r) => {
                let id = r.id();
                if !sets.relations.mask_into(id, union) {
                    continue;
                }
                r.members_into(&mut members_buf)?;
                let meta = clean_partial_metadata(
                    element_metadata(ElementKind::Relation, id, &r.info())?,
                    clean,
                );
                for i in regions_of(union) {
                    ensure_relation_capacity_local(&mut bbs[i], &mut output[i])?;
                    bbs[i].add_relation_partial(id, r.tags(), &members_buf, meta.as_ref());
                    counts[i].relations_written += 1;
                }
            }
        }
    }
    Ok(counts)
}
