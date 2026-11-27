//! Simple extraction strategy: one phase per kind for sorted inputs (fused to
//! one decode per blob where the ID ranges allow), two-pass fallback for
//! unsorted.

use rayon::prelude::*;

use crate::block_builder::{BlockBuilder, OwnedBlock};
use crate::cat::CleanAttrs;
use crate::read::input::PinnedInput;
use crate::write::WriteOptions;
use crate::writer::PbfWriter;
use crate::{Element, ElementKind, PrimitiveBlock};

use super::super::{HeaderOverrides, Result, drain_batch_results, flush_local, writer_from_header};
use crate::idset::IdSet;
use crate::retention::release_scratch;

use super::common::{
    BboxInt, BlobDesc, ExtractPass2IdSets, Passthrough, build_blob_schedule_with_passthrough,
    dense_only_payload, extract_block_pass2, merge_extract_stats, pread_execute,
    refuse_blob_foreign_kind, relation_has_matched_member, spatial_blob_filter,
    warn_passthrough_demotions,
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
/// the exact kind set block validation recorded
/// ([`PrimitiveBlock::element_kinds`]), never from `block_type()`: that label
/// names a group by its first field, so an empty dense message in front of
/// ways labels a way block as nodes and a ways-first group can carry
/// relations, and a sweep skipping on the label lost those matches.
/// A block of several kinds is walked under the same phase rule.
#[hotpath::measure]
fn classify_block_simple(
    block: &PrimitiveBlock,
    phase: ClassifyPhase,
    region: &Region,
    exact_bbox: &BboxInt,
    bbox_node_ids: &mut IdSet,
    matched_way_ids: &mut IdSet,
    matched_relation_ids: &mut IdSet,
) -> crate::error::Result<()> {
    let phase_kind = phase.kind();
    if !block.element_kinds().has(phase_kind) {
        return Ok(());
    }

    for element in block.elements_skip_metadata() {
        match (phase, &element) {
            (ClassifyPhase::Nodes, Element::DenseNode(dn))
                if region.contains_node(
                    exact_bbox,
                    dn.decimicro_lat().ok(),
                    dn.decimicro_lon().ok(),
                )? =>
            {
                bbox_node_ids.insert(dn.id());
            }
            (ClassifyPhase::Nodes, Element::Node(n))
                if region.contains_node(
                    exact_bbox,
                    n.decimicro_lat().ok(),
                    n.decimicro_lon().ok(),
                )? =>
            {
                bbox_node_ids.insert(n.id());
            }
            (ClassifyPhase::Ways, Element::Way(w)) if w.refs().any(|r| bbox_node_ids.get(r)) => {
                matched_way_ids.insert(w.id());
            }
            (ClassifyPhase::Relations, Element::Relation(r))
                if relation_has_matched_member(r, bbox_node_ids, matched_way_ids) =>
            {
                matched_relation_ids.insert(r.id());
            }
            _ => {}
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
            // kind, where `refuse_blob_foreign_kind` checks that claim
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
                let block = PrimitiveBlock::from_vec_with_scratch(
                    std::mem::take(&mut decompress_buf),
                    &mut st_scratch,
                    &mut gr_scratch,
                )?;
                release_scratch(&mut st_scratch);
                release_scratch(&mut gr_scratch);
                refuse_blob_foreign_kind(&blob, &block, source.as_ref())?;
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
    // Workers pread node blobs, decompress, scan with node-only scanner,
    // check bbox (pure function), send matching IDs to consumer.
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
        crate::scan::classify::parallel_scan_blobs_raw(
            &classify_input,
            &classify_schedule,
            None,
            || {
                (
                    Vec::<crate::scan::node::NodeTuple>::new(),
                    Vec::<(usize, usize)>::new(),
                    ValidateScratch::default(),
                )
            },
            |position, decompressed, (tuples, group_starts, validate), gate| {
                tuples.clear();
                let kinds =
                    crate::scan::node::extract_node_tuples(decompressed, tuples, group_starts)?;
                // The scanner reads only nodes, so ways or relations under a
                // node claim are ones it would never see.
                let admitted = gate.admit(kinds)?;
                // Classification is the same for every blob, candidate or
                // not, so verification can never change which nodes are
                // selected.
                let matching: Vec<i64> = tuples
                    .iter()
                    .filter(|t| region_ref.contains_decimicro(exact_bbox_ref, t.lat, t.lon))
                    .map(|t| t.id)
                    .collect();
                let verified = if node_schedule_ref[position].passthrough == Passthrough::Candidate
                {
                    verify_candidate(decompressed, tuples.len(), matching.len(), validate)?
                } else {
                    None
                };
                Ok((admitted, (matching, verified)))
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
        crate::scan::classify::parallel_scan_blobs_raw(
            &classify_input,
            &classify_schedule,
            None,
            || (Vec::<i64>::new(), Vec::<(usize, usize)>::new()),
            |_position, decompressed, (refs_buf, group_starts), gate| {
                let mut matching: Vec<i64> = Vec::new();
                let kinds = crate::scan::way::scan_way_refs(
                    decompressed,
                    refs_buf,
                    group_starts,
                    |way_id, refs| {
                        if refs.iter().any(|&r| bbox_ids_ref.get(r)) {
                            matching.push(way_id);
                        }
                    },
                )?;
                // The scanner reads only ways, so nodes or relations under a
                // way claim are ones it would never see.
                Ok((gate.admit(kinds)?, matching))
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
    // The classify is a block walk on purpose: about 0.3 s at planet, so
    // the re-parse a projection would remove sits under the command's
    // noise floor.
    host.invocation.marker("SIMPLE_REL_CLASSIFY_START");
    let mut matched_relation_ids = IdSet::new();
    {
        let rel_classify_schedule = scheduled(relation_schedule)?;
        let rel_classify_file = crate::read::input::InputFile::open(input)?;
        crate::scan::classify::parallel_classify_accumulate(
            &rel_classify_file,
            &rel_classify_schedule,
            None,
            IdSet::new,
            |block, ids| {
                for element in block.elements_skip_metadata() {
                    if let Element::Relation(r) = &element
                        && relation_has_matched_member(r, bbox_node_ids, matched_way_ids)
                    {
                        ids.insert(r.id());
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

/// String-table and group-range scratch for a candidate's validation.
type ValidateScratch = (Vec<(u32, u32)>, Vec<(u32, u32)>);

/// Verify one passthrough candidate from its classify-phase decode: the
/// observed node count when every scanned node landed in the region and the
/// payload has the dense-only shape that makes the scan complete, else
/// `None`. Occurrence counts, not set sizes: a duplicate id counts every
/// time it would be copied. `scratch` is the worker's string-table and
/// group-range scratch for the validation, reused across blobs.
fn verify_candidate(
    decompressed: &[u8],
    scanned: usize,
    matching: usize,
    scratch: &mut ValidateScratch,
) -> crate::error::Result<Option<u64>> {
    if matching != scanned || !dense_only_payload(decompressed)? {
        return Ok(None);
    }
    // What the decode path would do with this blob; a block it rejects must
    // not reach the output through a raw copy instead. The decoder's check,
    // run on the payload where it lies: only the verdict is needed. No claim:
    // the node scanner has refused under it, and a dense-only payload holds
    // nodes alone. It replaced a copy into a dropped block; that copy ran
    // only for nominated candidates, none on the planet bbox, and the same
    // copy in check --locations measured within noise at planet
    // (reference/performance/check.md, "Payloads validated where they lie").
    let (st, groups) = scratch;
    crate::read::block::validate_in_place(decompressed, st, groups, None, |_, _| {
        crate::read::block::NoProjection
    })?;
    Ok(Some(scanned as u64))
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
                        let block = PrimitiveBlock::from_vec_with_scratch(
                            decompress_buf,
                            st_scratch,
                            gr_scratch,
                        )?;
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
