//! Simple extraction strategy: one phase per kind for sorted inputs (fused to
//! one decode per blob where the ID ranges allow), two-pass fallback for
//! unsorted.

use rayon::prelude::*;

use crate::block_builder::{BlockBuilder, OwnedBlock};
use crate::cat::CleanAttrs;
use crate::read::block::{BlockBuffer, ClassificationBlock};
use crate::read::input::PinnedInput;
use crate::write::WriteOptions;
use crate::writer::PbfWriter;
use crate::{Element, ElementKind};

use super::super::{HeaderOverrides, Result, drain_batch_results, flush_local, writer_from_header};
use crate::idset::IdSet;
use crate::retention::release_scratch;

use super::common::{
    BboxInt, BlobDesc, ExtractPass2IdSets, Passthrough, build_blob_schedule_with_passthrough,
    dense_only_payload, extract_block_pass2, merge_extract_stats, pread_execute,
    relation_has_matched_member, spatial_blob_filter, warn_passthrough_demotions,
};
use super::{ExtractStats, Region};

use crate::tunables::EXTRACT_SIMPLE_BATCH_SIZE;

/// Which element kind a classification sweep is resolving.
///
/// Classification is inherently ordered - a way is matched by looking up its
/// refs in `bbox_node_ids`, and a relation by looking up its members in
/// `bbox_node_ids` and `matched_way_ids`. On a sorted file, file order
/// already supplies that order. On an unsorted one it does not, which is why
/// the unsorted fallback sweeps the file once per kind instead of deciding
/// all three in a single file-order walk.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ClassifyPhase {
    Nodes,
    Ways,
    Relations,
}

impl ClassifyPhase {
    /// The element kind this sweep resolves.
    fn kind(self) -> ElementKind {
        match self {
            ClassifyPhase::Nodes => ElementKind::Node,
            ClassifyPhase::Ways => ElementKind::Way,
            ClassifyPhase::Relations => ElementKind::Relation,
        }
    }
}

/// Classify elements in a single block for simple extract (populate ID sets).
///
/// `phase` selects which kind is resolved; elements of other kinds are
/// ignored. Iterates elements without metadata (faster) and marks matching
/// IDs:
/// - Nodes: bbox containment → set `bbox_node_ids`
/// - Ways: any ref in `bbox_node_ids` → set `matched_way_ids`
/// - Relations: matched node/way member → set `matched_relation_ids`
///
/// Skips whole blocks that hold no element of the phase's kind, read from
/// the exact kind set block validation recorded. The coarse block type label
/// names a group by its first field, so an empty dense message in front of
/// ways labels a way block as nodes and a ways-first group can carry
/// relations, and a sweep skipping on the label lost those matches.
/// A block of several kinds is walked under the same phase rule.
#[hotpath::measure]
fn classify_block_simple(
    block: &ClassificationBlock<'_>,
    phase: ClassifyPhase,
    region: &Region,
    exact_bbox: &BboxInt,
    bbox_node_ids: &mut IdSet,
    matched_way_ids: &mut IdSet,
    matched_relation_ids: &mut IdSet,
) -> crate::error::Result<()> {
    let phase_kind = phase.kind();
    if !block.has_kind(phase_kind) {
        return Ok(());
    }

    match phase {
        ClassifyPhase::Nodes => {
            for element in block.nodes() {
                let (id, lat, lon) = match element {
                    Element::DenseNode(n) => (n.id(), n.decimicro_lat()?, n.decimicro_lon()?),
                    Element::Node(n) => (n.id(), n.decimicro_lat()?, n.decimicro_lon()?),
                    _ => continue,
                };
                if region.contains_decimicro(exact_bbox, lat, lon) {
                    bbox_node_ids.insert(id);
                }
            }
        }
        ClassifyPhase::Ways => {
            for way in block.ways() {
                if way.refs().any(|id| bbox_node_ids.get(id)) {
                    matched_way_ids.insert(way.id());
                }
            }
        }
        ClassifyPhase::Relations => {
            for relation in block.relations() {
                if relation_has_matched_member(relation.members(), bbox_node_ids, matched_way_ids) {
                    matched_relation_ids.insert(relation.id());
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn extract_simple(
    input: &PinnedInput,
    output: &crate::output::BoundOutput,
    region: &Region,
    set_bounds: bool,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<ExtractStats> {
    // Check if input is sorted - if so, classify + write in a single file pass.
    // We need a quick header check without keeping the reader open. Use BlobReader
    // to read just the first blob (header).
    let is_sorted = crate::BlobReader::open_in(input, write.direct_reads, host)?
        .leading_header()?
        .to_headerblock()?
        .is_sorted();

    if is_sorted {
        return extract_simple_single_pass(
            input, output, region, set_bounds, clean, write, overrides, host,
        );
    }

    extract_simple_two_pass(
        input, output, region, set_bounds, clean, write, overrides, host,
    )
}

// Unsorted fallback: two passes (collect IDs, then write).
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn extract_simple_two_pass(
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
    host.invocation.marker("SIMPLE_UNSORTED_PASS1_START");
    let mut stats = ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_written: 0,
        ways_from_relations: 0,
        relations_written: 0,
        strategy: "simple",
    };

    let mut bbox_node_ids = IdSet::new();
    let mut matched_way_ids = IdSet::new();
    let mut matched_relation_ids = IdSet::new();

    let bbox_int = BboxInt::from_bbox(region.bbox());
    let exact_bbox = BboxInt::inside_bbox(region.bbox());
    let spatial_filter = spatial_blob_filter(&bbox_int);

    // One sweep per element kind. A single file-order sweep deciding all
    // three kinds is only sound when nodes precede ways precede relations,
    // i.e. when the file is sorted - and this is exactly the branch taken
    // when it is not. Under the old single sweep, a way whose blob appeared
    // before the blob holding its bbox nodes never matched and was simply
    // absent from the output: no error, no warning, no count. Three sweeps
    // cost two extra reads of an input that is already on the slow path, and
    // they decide from what the sweep actually saw rather than from where
    // the producer happened to put the blob.
    // Each block takes the decode buffer and drops before the next blob; the
    // buffer is not pooled, by the rule in read/decompress.rs ("Take
    // loops"). The parse scratch is reused.
    let mut decompress_buf: Vec<u8> = Vec::new();
    let (mut st_scratch, mut gr_scratch) = (Vec::new(), Vec::new());
    for phase in [
        ClassifyPhase::Nodes,
        ClassifyPhase::Ways,
        ClassifyPhase::Relations,
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
            // Even here, where elements are classified after decoding, a
            // blob claimed as nodes is skipped on its indexdata bbox
            // without a decode (reference/indexdata-trust.md).
            // An indexed blob is decoded only in the sweep of its claimed
            // kind, where the projection checks that claim
            // against the payload; the other two sweeps would only drop
            // it. Every admitted indexed blob still reaches its own sweep,
            // so the claim is checked once before pass 2 relies on it.
            // Unindexed blobs are decoded in all three sweeps.
            if let Some(idx) = blob.index()
                && (!spatial_filter.wants_index(&idx) || idx.kind != phase.kind())
            {
                continue;
            }
            let mut classify = || -> Result<()> {
                blob.decompress_into(&mut decompress_buf)?;
                let buffer = BlockBuffer::new(
                    std::mem::take(&mut decompress_buf),
                    None,
                    &mut st_scratch,
                    &mut gr_scratch,
                    blob.index().map(|idx| idx.kind),
                )
                .for_selection()?;
                release_scratch(&mut st_scratch);
                release_scratch(&mut gr_scratch);
                let block = buffer.classify()?;
                classify_block_simple(
                    &block,
                    phase,
                    region,
                    &exact_bbox,
                    &mut bbox_node_ids,
                    &mut matched_way_ids,
                    &mut matched_relation_ids,
                )
            };
            classify().map_err(|e| blob.locate_error(source.as_ref(), e))?;
        }
    }
    host.invocation.marker("SIMPLE_UNSORTED_PASS1_END");

    host.invocation.marker("SIMPLE_UNSORTED_PASS2_START");
    let outside_way_node_ids = IdSet::new();

    let mut blob_reader = crate::blob::BlobReader::open_in(input, direct_io, host)?;
    blob_reader.set_parse_indexdata(true);
    let header_blob = blob_reader.leading_header()?;
    let source = blob_reader.source().cloned();
    let header = header_blob
        .to_headerblock()
        .map_err(|e| header_blob.locate_error(source.as_ref(), e))?;
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

    let ids = ExtractPass2IdSets {
        bbox_node_ids: &bbox_node_ids,
        outside_way_node_ids: &outside_way_node_ids,
        matched_way_ids: &matched_way_ids,
        matched_relation_ids: &matched_relation_ids,
    };

    // The batch holds compressed blobs; the workers decompress and decode
    // them, so decode runs in parallel instead of ahead of each batch on
    // this thread. Two skips need no decode:
    // - a node blob whose indexdata bbox misses the region, the same test
    //   pass 1 skipped it on, so nothing in it was selected (and, as on the
    //   sorted route, a duplicate ID's occurrence there is not written);
    // - an indexed blob whose claimed kind has an empty selection set. Pass
    //   1 decoded every admitted indexed blob in its claimed kind's sweep
    //   and refused a foreign kind there, so the claim is already checked.
    //   Node output reads only `bbox_node_ids` here, since
    //   `outside_way_node_ids` is empty on this route.
    let empty_for = |kind: ElementKind| match kind {
        ElementKind::Node => bbox_node_ids.is_empty(),
        ElementKind::Way => matched_way_ids.is_empty(),
        ElementKind::Relation => matched_relation_ids.is_empty(),
    };
    let mut batch: Vec<crate::blob::Blob> = Vec::with_capacity(EXTRACT_SIMPLE_BATCH_SIZE);
    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
            continue;
        }
        if let Some(idx) = blob.index()
            && (!spatial_filter.wants_index(&idx) || empty_for(idx.kind))
        {
            continue;
        }
        batch.push(blob);
        if batch.len() >= EXTRACT_SIMPLE_BATCH_SIZE {
            process_extract_pass2_batch(
                &batch,
                source.as_ref(),
                &ids,
                clean,
                coords,
                &mut writer,
                &mut stats,
                host,
            )?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        process_extract_pass2_batch(
            &batch,
            source.as_ref(),
            &ids,
            clean,
            coords,
            &mut writer,
            &mut stats,
            host,
        )?;
    }

    writer.flush()?;
    host.invocation.marker("SIMPLE_UNSORTED_PASS2_END");
    Ok(stats)
}

/// Sorted simple extract: one header-only schedule walk, then one phase per
/// kind (nodes, ways, relations), each kind's ID set frozen before the next
/// kind reads it.
///
/// A kind whose blobs all carry indexdata ID ranges runs fused
/// (`fused::fused_pass`): each blob is read, decompressed and decoded once,
/// selected and framed on its worker, blobs whose ranges overlap (a history
/// file's versions straddling a blob cut) read together as one component.
/// A kind the planner refuses (an unindexed blob, an overlap that skips a
/// blob, too long a run) runs a classify pool that fills the kind's ID set,
/// then a write pool that rereads and writes by it.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn extract_simple_single_pass(
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
    host.invocation.marker("EXTRACT_SCAN_START");
    let mut stats = ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_written: 0,
        ways_from_relations: 0,
        relations_written: 0,
        strategy: "simple",
    };

    let bbox_int = BboxInt::from_bbox(region.bbox());
    let spatial_filter = spatial_blob_filter(&bbox_int);

    // Build schedule once, partition by element type.
    // Tag node blobs for raw passthrough if bbox region + no clean.
    let exact_bbox = BboxInt::inside_bbox(region.bbox());
    let passthrough_bbox = if matches!(region, Region::Bbox(_)) && !clean.any() {
        let inside = &exact_bbox;
        Some(crate::BlobBbox::new(
            inside.min_lat,
            inside.max_lat,
            inside.min_lon,
            inside.max_lon,
        ))
    } else {
        None
    };
    let full_schedule =
        build_blob_schedule_with_passthrough(input, passthrough_bbox.as_ref(), host)?;
    // The three schedules split on the indexdata kind, and node blobs are
    // dropped on the indexdata bbox, both claims never decoded to check: a
    // mislabelled blob's elements are missed by the phase that needs them,
    // and a blob whose bbox understates its nodes is skipped unread.
    // Deliberate: claims are believed, not verified
    // (reference/indexdata-trust.md); `check --index` is the verifier.
    let node_schedule: Vec<&BlobDesc> = full_schedule
        .iter()
        .filter(|d| {
            match d.kind {
                Some(crate::ElementKind::Node) => {
                    // Apply spatial bbox filter to skip node blobs outside extract region.
                    if let Some(ref filter_bbox) = spatial_filter.node_bbox {
                        match d.bbox {
                            Some(ref bb) => filter_bbox.intersects(bb),
                            None => true, // no bbox in indexdata - must include
                        }
                    } else {
                        true // no spatial filter configured
                    }
                }
                None => true, // no indexdata - must include
                _ => false,
            }
        })
        .collect();
    // Non-indexed blobs (kind == None) are included in all three schedules
    // because we can't determine their type without decompressing. Each phase's
    // classify closure only processes its matching element type, so elements of
    // other types are silently skipped. An unindexed blob also keeps every
    // kind off the fused pass, so it is decompressed twice per kind, a
    // classify and a write - acceptable since indexed PBFs (production
    // path) always have kind set and this path is only reachable via --force.
    let way_schedule: Vec<&BlobDesc> = full_schedule
        .iter()
        .filter(|d| matches!(d.kind, Some(crate::ElementKind::Way) | None))
        .collect();
    let relation_schedule: Vec<&BlobDesc> = full_schedule
        .iter()
        .filter(|d| matches!(d.kind, Some(crate::ElementKind::Relation) | None))
        .collect();

    // Open writer.
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

    let mut bbox_node_ids = IdSet::new();
    let mut matched_way_ids = IdSet::new();

    // Each kind runs fused (one decode per blob, selecting and framing on
    // the worker) when its schedule's ID ranges plan every ID's occurrences
    // into one component task; otherwise classify, then write (see `fused`).
    if let Some(components) = super::fused::fusable(&node_schedule) {
        host.invocation.marker("SIMPLE_NODE_FUSED_START");
        super::fused::fused_pass(
            input,
            &node_schedule,
            &components,
            &super::fused::Select::Nodes {
                region,
                exact: &exact_bbox,
            },
            clean,
            &mut writer,
            &mut stats,
            &mut bbox_node_ids,
            host,
        )?;
        host.invocation.marker("SIMPLE_NODE_FUSED_END");
    } else {
        simple_nodes_two_phase(
            input,
            &node_schedule,
            region,
            &exact_bbox,
            clean,
            &mut writer,
            &mut stats,
            &mut bbox_node_ids,
            host,
        )?;
    }

    // --- Phase 2: ways ---
    if let Some(components) = super::fused::fusable(&way_schedule) {
        host.invocation.marker("SIMPLE_WAY_FUSED_START");
        super::fused::fused_pass(
            input,
            &way_schedule,
            &components,
            &super::fused::Select::Ways {
                nodes: &bbox_node_ids,
                coords,
            },
            clean,
            &mut writer,
            &mut stats,
            &mut matched_way_ids,
            host,
        )?;
        host.invocation.marker("SIMPLE_WAY_FUSED_END");
    } else {
        simple_ways_two_phase(
            input,
            &way_schedule,
            &bbox_node_ids,
            clean,
            coords,
            &mut writer,
            &mut stats,
            &mut matched_way_ids,
            host,
        )?;
    }

    // --- Phase 3: relations ---
    if let Some(components) = super::fused::fusable(&relation_schedule) {
        host.invocation.marker("SIMPLE_REL_FUSED_START");
        let mut unused = IdSet::new();
        super::fused::fused_pass(
            input,
            &relation_schedule,
            &components,
            &super::fused::Select::Relations {
                nodes: &bbox_node_ids,
                ways: &matched_way_ids,
            },
            clean,
            &mut writer,
            &mut stats,
            &mut unused,
            host,
        )?;
        host.invocation.marker("SIMPLE_REL_FUSED_END");
    } else {
        simple_relations_two_phase(
            input,
            &relation_schedule,
            &bbox_node_ids,
            &matched_way_ids,
            clean,
            &mut writer,
            &mut stats,
            host,
        )?;
    }

    writer.flush()?;
    host.invocation.marker("EXTRACT_SCAN_END");
    Ok(stats)
}

/// The node kind's classify-then-write form, for a node schedule
/// [`fused::fusable`](super::fused::fusable) does not admit.
#[allow(clippy::too_many_arguments)]
fn simple_nodes_two_phase(
    input: &PinnedInput,
    node_schedule: &[&BlobDesc],
    region: &Region,
    exact_bbox: &BboxInt,
    clean: &CleanAttrs,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut ExtractStats,
    bbox_node_ids: &mut IdSet,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    let matched_way_ids = &IdSet::new();
    let empty_relation_ids = IdSet::new();
    let outside_way_node_ids = IdSet::new();
    // Workers pread, decompress and project node coordinates, then send
    // matching IDs to the consumer.
    // Consumer merges into bbox_node_ids. No shared mutable state in workers.
    host.invocation.marker("SIMPLE_NODE_CLASSIFY_START");
    let mut verified_nodes: Vec<Option<u64>> = vec![None; node_schedule.len()];
    {
        let classify_schedule = scheduled(node_schedule)?;
        let classify_input = crate::read::input::InputFile::open(input)?;
        let region_ref = region;
        let exact_bbox_ref = exact_bbox;
        let node_schedule_ref = node_schedule;
        // Per blob: the matching node ids, plus - for a passthrough candidate
        // - the observed node count if the blob verified (see `Passthrough`).
        // Positions index `node_schedule`, which `verified_nodes` follows.
        crate::scan::classify::parallel_project_phase(
            &classify_input,
            &classify_schedule,
            None,
            || (),
            |position, buffer, ()| {
                let buffer = buffer.for_selection()?;
                let block = buffer.classify()?;
                let mut matching = Vec::new();
                let mut scanned = 0;
                for element in block.nodes() {
                    let (id, lat, lon) = match element {
                        Element::DenseNode(n) => (n.id(), n.decimicro_lat()?, n.decimicro_lon()?),
                        Element::Node(n) => (n.id(), n.decimicro_lat()?, n.decimicro_lon()?),
                        _ => continue,
                    };
                    scanned += 1;
                    if region_ref.contains_decimicro(exact_bbox_ref, lat, lon) {
                        matching.push(id);
                    }
                }
                let verified = if node_schedule_ref[position].passthrough == Passthrough::Candidate
                    && matching.len() == scanned
                    && dense_only_payload(buffer.payload())?
                {
                    buffer.validate_passthrough()?;
                    Some(scanned as u64)
                } else {
                    None
                };
                Ok((matching, verified))
            },
            |position, (matching_ids, verified): (Vec<i64>, Option<u64>)| {
                for id in matching_ids {
                    bbox_node_ids.insert(id);
                }
                verified_nodes[position] = verified;
            },
            host,
        )?;
    }
    host.invocation.marker("SIMPLE_NODE_CLASSIFY_END");
    // bbox_node_ids frozen. Write matching nodes via pread-from-workers.
    host.invocation.marker("SIMPLE_NODE_WRITE_START");
    let node_descs = settle_passthrough_candidates(node_schedule, &verified_nodes, host);
    {
        let ids = ExtractPass2IdSets {
            bbox_node_ids: &*bbox_node_ids,
            outside_way_node_ids: &outside_way_node_ids,
            matched_way_ids,
            matched_relation_ids: &empty_relation_ids,
        };
        pread_execute(
            input,
            &node_descs,
            None,
            writer,
            stats,
            |block, bb, output| {
                let s = extract_block_pass2(
                    block,
                    &ids,
                    clean,
                    crate::owned::WayCoords::Drop,
                    Some(crate::ElementKind::Node),
                    bb,
                    output,
                )?;
                flush_local(bb, output)?;
                Ok(s)
            },
            host,
        )?;
    }

    host.invocation.marker("SIMPLE_NODE_WRITE_END");
    Ok(())
}

/// The way kind's classify-then-write form.
#[allow(clippy::too_many_arguments)]
fn simple_ways_two_phase(
    input: &PinnedInput,
    way_schedule: &[&BlobDesc],
    bbox_node_ids: &IdSet,
    clean: &CleanAttrs,
    coords: crate::owned::WayCoords,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut ExtractStats,
    matched_way_ids: &mut IdSet,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    let empty_relation_ids = IdSet::new();
    let outside_way_node_ids = IdSet::new();
    host.invocation.marker("SIMPLE_WAY_CLASSIFY_START");
    {
        let classify_schedule = scheduled(way_schedule)?;
        let classify_input = crate::read::input::InputFile::open(input)?;
        let bbox_ids_ref = bbox_node_ids;
        crate::scan::classify::parallel_project_phase(
            &classify_input,
            &classify_schedule,
            None,
            || (),
            |_, buffer, _| {
                let buffer = buffer.for_selection()?;
                let block = buffer.classify()?;
                let mut matching = Vec::new();
                for way in block.ways() {
                    if way.refs().any(|id| bbox_ids_ref.get(id)) {
                        matching.push(way.id());
                    }
                }
                Ok(matching)
            },
            |_position, matching_ids: Vec<i64>| {
                for id in matching_ids {
                    matched_way_ids.insert(id);
                }
            },
            host,
        )?;
    }
    host.invocation.marker("SIMPLE_WAY_CLASSIFY_END");
    // matched_way_ids frozen. Write matching ways via pread-from-workers.
    //
    // `bbox_node_ids` stays live, unread, through this write: its last reader
    // is the relation classify below. Moving that classify ahead of this
    // write (it needs only the two finished sets) and dropping the node set
    // here was built and measured at planet (bygg, Europe-sized box, against
    // 0973c352): peak anon 2.60 GB against 2.57 GB, no change. The freed
    // chunks stay in the allocator's arenas and the write's worker-side
    // buffers do not land in them, so the drop moves no peak; do not redo it
    // without a change to how the set's chunks are allocated.
    host.invocation.marker("SIMPLE_WAY_WRITE_START");
    let way_descs: Vec<BlobDesc> = way_schedule
        .iter()
        .map(|d| BlobDesc {
            passthrough: Passthrough::No,
            ..**d
        })
        .collect();
    {
        let ids = ExtractPass2IdSets {
            bbox_node_ids,
            outside_way_node_ids: &outside_way_node_ids,
            matched_way_ids: &*matched_way_ids,
            matched_relation_ids: &empty_relation_ids,
        };
        pread_execute(
            input,
            &way_descs,
            None,
            writer,
            stats,
            |block, bb, output| {
                let s = extract_block_pass2(
                    block,
                    &ids,
                    clean,
                    coords,
                    Some(crate::ElementKind::Way),
                    bb,
                    output,
                )?;
                flush_local(bb, output)?;
                Ok(s)
            },
            host,
        )?;
    }

    host.invocation.marker("SIMPLE_WAY_WRITE_END");
    Ok(())
}

/// The relation kind's classify-then-write form.
#[allow(clippy::too_many_arguments)]
fn simple_relations_two_phase(
    input: &PinnedInput,
    relation_schedule: &[&BlobDesc],
    bbox_node_ids: &IdSet,
    matched_way_ids: &IdSet,
    clean: &CleanAttrs,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut ExtractStats,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    let outside_way_node_ids = IdSet::new();
    host.invocation.marker("SIMPLE_REL_CLASSIFY_START");
    let mut matched_relation_ids = IdSet::new();
    {
        let rel_classify_schedule = scheduled(relation_schedule)?;
        let rel_classify_file = crate::read::input::InputFile::open(input)?;
        crate::scan::classify::parallel_project_accumulate(
            &rel_classify_file,
            &rel_classify_schedule,
            None,
            IdSet::new,
            |buffer, ids| {
                let buffer = buffer.for_selection()?;
                let block = buffer.classify()?;
                for relation in block.relations() {
                    if relation_has_matched_member(
                        relation.members(),
                        bbox_node_ids,
                        matched_way_ids,
                    ) {
                        ids.insert(relation.id());
                    }
                }
                Ok(())
            },
            |worker_ids| {
                matched_relation_ids.merge(worker_ids);
            },
            host,
        )?;
    }
    host.invocation.marker("SIMPLE_REL_CLASSIFY_END");
    host.invocation.marker("SIMPLE_REL_WRITE_START");
    let rel_descs: Vec<BlobDesc> = relation_schedule
        .iter()
        .map(|d| BlobDesc {
            passthrough: Passthrough::No,
            ..**d
        })
        .collect();
    {
        let ids = ExtractPass2IdSets {
            bbox_node_ids,
            outside_way_node_ids: &outside_way_node_ids,
            matched_way_ids,
            matched_relation_ids: &matched_relation_ids,
        };
        pread_execute(
            input,
            &rel_descs,
            None,
            writer,
            stats,
            |block, bb, output| {
                let s = extract_block_pass2(
                    block,
                    &ids,
                    clean,
                    crate::owned::WayCoords::Drop,
                    Some(crate::ElementKind::Relation),
                    bb,
                    output,
                )?;
                flush_local(bb, output)?;
                Ok(s)
            },
            host,
        )?;
    }

    host.invocation.marker("SIMPLE_REL_WRITE_END");
    Ok(())
}

/// The classify pools' schedule for `descs`, in the same order, so a pool
/// position indexes `descs`. Failures are located by file and frame offset
/// (an extract schedule carries no blob ordinal).
fn scheduled(descs: &[&BlobDesc]) -> Result<Vec<crate::scan::classify::ScheduledBlob>> {
    descs
        .iter()
        .map(|d| {
            crate::scan::classify::ScheduledBlob::at_frame(
                d.frame_offset,
                d.offset,
                d.size,
                d.kind,
                crate::scan::classify::KindClaims::Refuse,
            )
        })
        .collect()
}

/// Turn each passthrough candidate into `Verified` (with the node count the
/// classify phase observed) or back into `No`, by node-schedule position.
fn settle_passthrough_candidates(
    node_schedule: &[&BlobDesc],
    verified_nodes: &[Option<u64>],
    host: &crate::host::Entered<'_>,
) -> Vec<BlobDesc> {
    let mut verified_blobs: u64 = 0;
    let mut demoted_blobs: u64 = 0;
    let descs = node_schedule
        .iter()
        .zip(verified_nodes)
        .map(|(d, verified)| {
            let passthrough = match (d.passthrough, *verified) {
                (Passthrough::Candidate, Some(nodes)) => {
                    verified_blobs += 1;
                    Passthrough::Verified { nodes }
                }
                (Passthrough::Candidate, None) => {
                    demoted_blobs += 1;
                    Passthrough::No
                }
                (other, _) => other,
            };
            BlobDesc { passthrough, ..**d }
        })
        .collect();
    warn_passthrough_demotions(verified_blobs, demoted_blobs, host);
    descs
}

/// Decode and write a batch of compressed blobs in parallel for pass 2 of the
/// unsorted simple extract. Each worker decompresses and decodes its blob;
/// the block takes the decode buffer and drops when the blob is done
/// (read/decompress.rs, "Take loops"), while the parse scratch is kept per
/// worker. Results drain in batch order, so output stays in file order.
#[allow(clippy::too_many_arguments)]
fn process_extract_pass2_batch(
    batch: &[crate::blob::Blob],
    source: Option<&std::sync::Arc<std::path::Path>>,
    ids: &ExtractPass2IdSets<'_>,
    clean: &CleanAttrs,
    coords: crate::owned::WayCoords,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut ExtractStats,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    type BatchResult = crate::Result<(Vec<OwnedBlock>, ExtractStats)>;
    let limits = crate::commands::split::Limits::new(writer.compression());
    let results: Vec<BatchResult> = host.executor.install(|| {
        batch
            .par_iter()
            .map_init(
                || (BlockBuilder::new(), Vec::new(), Vec::new()),
                |(bb, st_scratch, gr_scratch), blob| {
                    let mut task = || -> BatchResult {
                        let mut decompress_buf: Vec<u8> = Vec::new();
                        blob.decompress_into(&mut decompress_buf)?;
                        let block = BlockBuffer::new(
                            decompress_buf,
                            None,
                            st_scratch,
                            gr_scratch,
                            blob.index().map(|idx| idx.kind),
                        )
                        .for_selection()?;
                        release_scratch(st_scratch);
                        release_scratch(gr_scratch);
                        let mut output: Vec<OwnedBlock> = Vec::new();
                        let block_stats =
                            extract_block_pass2(&block, ids, clean, coords, None, bb, &mut output)?;
                        flush_local(bb, &mut output)?;
                        crate::commands::split::fit_blocks(&mut output, &limits)?;
                        Ok((output, block_stats))
                    };
                    // Every failure of the work on one blob, the cut's
                    // refusal of a block it cannot fit included, names it.
                    task().map_err(|e| blob.locate_error(source, e))
                },
            )
            .collect()
    });

    drain_batch_results(results, writer, |s| merge_extract_stats(stats, &s), host)?;
    Ok(())
}
