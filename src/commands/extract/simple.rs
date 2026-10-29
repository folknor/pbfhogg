//! Simple extraction strategy: single-pass for sorted inputs, two-pass fallback for unsorted.

use rayon::prelude::*;

use crate::block_builder::{BlockBuilder, OwnedBlock};
use crate::cat::CleanAttrs;
use crate::read::input::PinnedInput;
use crate::write::WriteOptions;
use crate::writer::PbfWriter;
use crate::{BlockType, Element, PrimitiveBlock};

use super::super::{HeaderOverrides, Result, drain_batch_results, flush_local, writer_from_header};
use crate::idset::IdSet;

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

/// Classify elements in a single block for simple extract (populate ID sets).
///
/// `phase` selects which kind is resolved; elements of other kinds are
/// ignored. Iterates elements without metadata (faster) and marks matching
/// IDs:
/// - Nodes: bbox containment → set `bbox_node_ids`
/// - Ways: any ref in `bbox_node_ids` → set `matched_way_ids`
/// - Relations: matched node/way member → set `matched_relation_ids`
///
/// Uses `block_type()` (1 byte per group) to skip whole blocks that hold no
/// element of the phase's kind, keeping the sorted path's fast rejection.
/// `BlockType::Mixed` blocks are walked element by element under the same
/// phase rule, so a mixed block cannot smuggle a way past the node sweep.
#[hotpath::measure]
fn classify_block_simple(
    block: &PrimitiveBlock,
    phase: ClassifyPhase,
    region: &Region,
    bbox_int: &BboxInt,
    bbox_node_ids: &mut IdSet,
    matched_way_ids: &mut IdSet,
    matched_relation_ids: &mut IdSet,
) {
    let relevant = match block.block_type() {
        BlockType::Empty => false,
        BlockType::Mixed => true,
        BlockType::DenseNodes | BlockType::Nodes => phase == ClassifyPhase::Nodes,
        BlockType::Ways => phase == ClassifyPhase::Ways,
        BlockType::Relations => phase == ClassifyPhase::Relations,
    };
    if !relevant {
        return;
    }

    for element in block.elements_skip_metadata() {
        match (phase, &element) {
            (ClassifyPhase::Nodes, Element::DenseNode(dn))
                if region.contains_decimicro(bbox_int, dn.decimicro_lat(), dn.decimicro_lon()) =>
            {
                bbox_node_ids.set(dn.id());
            }
            (ClassifyPhase::Nodes, Element::Node(n))
                if region.contains_decimicro(bbox_int, n.decimicro_lat(), n.decimicro_lon()) =>
            {
                bbox_node_ids.set(n.id());
            }
            (ClassifyPhase::Ways, Element::Way(w)) if w.refs().any(|r| bbox_node_ids.get(r)) => {
                matched_way_ids.set(w.id());
            }
            (ClassifyPhase::Relations, Element::Relation(r))
                if relation_has_matched_member(r, bbox_node_ids, matched_way_ids) =>
            {
                matched_relation_ids.set(r.id());
            }
            _ => {}
        }
    }
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
    let mut decompress_buf: Vec<u8> = Vec::new();
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
            if let Some(idx) = blob.index()
                && !spatial_filter.wants_index(&idx)
            {
                continue;
            }
            blob.decompress_into(&mut decompress_buf)?;
            let block = PrimitiveBlock::from_vec(std::mem::take(&mut decompress_buf))?;
            refuse_blob_foreign_kind(&blob, &block, source.as_ref())?;
            classify_block_simple(
                &block,
                phase,
                region,
                &bbox_int,
                &mut bbox_node_ids,
                &mut matched_way_ids,
                &mut matched_relation_ids,
            );
        }
    }
    host.invocation.marker("SIMPLE_UNSORTED_PASS1_END");

    host.invocation.marker("SIMPLE_UNSORTED_PASS2_START");
    let all_way_node_ids = IdSet::new();

    let mut blob_reader = crate::blob::BlobReader::open_in(input, direct_io, host)?;
    blob_reader.set_parse_indexdata(true);
    let header_blob = blob_reader.leading_header()?;
    let header = header_blob.to_headerblock()?;
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

    let ids = ExtractPass2IdSets {
        bbox_node_ids: &bbox_node_ids,
        all_way_node_ids: &all_way_node_ids,
        matched_way_ids: &matched_way_ids,
        matched_relation_ids: &matched_relation_ids,
    };

    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(EXTRACT_SIMPLE_BATCH_SIZE);
    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
            continue;
        }
        blob.decompress_into(&mut decompress_buf)?;
        let block = PrimitiveBlock::from_vec(std::mem::take(&mut decompress_buf))?;
        batch.push(block);
        if batch.len() >= EXTRACT_SIMPLE_BATCH_SIZE {
            process_extract_pass2_batch(&batch, &ids, clean, &mut writer, &mut stats, host)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        process_extract_pass2_batch(&batch, &ids, clean, &mut writer, &mut stats, host)?;
    }

    writer.flush()?;
    host.invocation.marker("SIMPLE_UNSORTED_PASS2_END");
    Ok(stats)
}

/// 3-phase barrier pipeline for sorted simple extract.
///
/// Exploits the sorted PBF guarantee (nodes → ways → relations) to parallelize
/// both classification and writing. Each phase runs pread-from-workers:
///
/// Phase 1 (nodes): workers classify (bbox check, pure function) + write matches.
///   Consumer collects bbox_node_ids. No shared mutable state needed by workers.
/// Phase 2 (ways): workers check refs against frozen &bbox_node_ids + write matches.
///   Consumer collects matched_way_ids.
/// Phase 3 (relations): workers check members against frozen ID sets + write matches.
///
/// ID sets become read-only after each phase barrier. Workers share them via
/// references in thread::scope. Single file scan (schedule built once from
/// header-only pass), three execution phases.
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
    let passthrough_bbox = if matches!(region, Region::Bbox(_)) && !clean.any() {
        let inside = BboxInt::inside_bbox(region.bbox());
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
    // other types are silently skipped. This means non-indexed blobs are
    // decompressed up to 3 times - acceptable since indexed PBFs (production
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

    let mut bbox_node_ids = IdSet::new();
    let mut matched_way_ids = IdSet::new();
    let empty_relation_ids = IdSet::new(); // placeholder for node/way phases
    let all_way_node_ids = IdSet::new();

    // --- Phase 1: Classify nodes (parallel pread + scanner) ---
    // Workers pread node blobs, decompress, scan with node-only scanner,
    // check bbox (pure function), send matching IDs to consumer.
    // Consumer merges into bbox_node_ids. No shared mutable state in workers.
    host.invocation.marker("SIMPLE_NODE_CLASSIFY_START");
    let mut verified_nodes: Vec<Option<u64>> = vec![None; node_schedule.len()];
    {
        let classify_schedule = scheduled(&node_schedule)?;
        let classify_input = crate::read::input::InputFile::open(input)?;
        let region_ref = region;
        let bbox_int_ref = &bbox_int;
        let node_schedule_ref = &node_schedule;
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
                )
            },
            |position, decompressed, (tuples, group_starts)| {
                tuples.clear();
                let kinds =
                    crate::scan::node::extract_node_tuples(decompressed, tuples, group_starts)?;
                // The scanner reads only nodes, so ways or relations under a
                // node claim are ones it would never see.
                kinds.refuse_under(node_schedule_ref[position].kind)?;
                // Classification is the same for every blob, candidate or
                // not, so verification can never change which nodes are
                // selected.
                let matching: Vec<i64> = tuples
                    .iter()
                    .filter(|t| region_ref.contains_decimicro(bbox_int_ref, t.lat, t.lon))
                    .map(|t| t.id)
                    .collect();
                let verified = if node_schedule_ref[position].passthrough == Passthrough::Candidate
                {
                    verify_candidate(decompressed, tuples.len(), matching.len())?
                } else {
                    None
                };
                Ok((matching, verified))
            },
            |position, (matching_ids, verified): (Vec<i64>, Option<u64>)| {
                for id in matching_ids {
                    bbox_node_ids.set(id);
                }
                verified_nodes[position] = verified;
            },
            host,
        )?;
    }
    host.invocation.marker("SIMPLE_NODE_CLASSIFY_END");
    // bbox_node_ids frozen. Write matching nodes via pread-from-workers.
    host.invocation.marker("SIMPLE_NODE_WRITE_START");
    let node_descs = settle_passthrough_candidates(&node_schedule, &verified_nodes, host);
    {
        let ids = ExtractPass2IdSets {
            bbox_node_ids: &bbox_node_ids,
            all_way_node_ids: &all_way_node_ids,
            matched_way_ids: &matched_way_ids,
            matched_relation_ids: &empty_relation_ids,
        };
        pread_execute(
            input,
            &node_descs,
            &mut writer,
            &mut stats,
            |block, bb, output| {
                let s = extract_block_pass2(
                    block,
                    &ids,
                    clean,
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
    // --- Phase 2: Classify ways (scanner) + write ways (pread-from-workers) ---
    host.invocation.marker("SIMPLE_WAY_CLASSIFY_START");
    {
        let classify_schedule = scheduled(&way_schedule)?;
        let classify_input = crate::read::input::InputFile::open(input)?;
        let bbox_ids_ref = &bbox_node_ids;
        let way_schedule_ref = &way_schedule;
        crate::scan::classify::parallel_scan_blobs_raw(
            &classify_input,
            &classify_schedule,
            None,
            || (Vec::<i64>::new(), Vec::<(usize, usize)>::new()),
            |position, decompressed, (refs_buf, group_starts)| {
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
                kinds.refuse_under(way_schedule_ref[position].kind)?;
                Ok(matching)
            },
            |_position, matching_ids: Vec<i64>| {
                for id in matching_ids {
                    matched_way_ids.set(id);
                }
            },
            host,
        )?;
    }
    host.invocation.marker("SIMPLE_WAY_CLASSIFY_END");
    // matched_way_ids frozen. Write matching ways via pread-from-workers.
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
            bbox_node_ids: &bbox_node_ids,
            all_way_node_ids: &all_way_node_ids,
            matched_way_ids: &matched_way_ids,
            matched_relation_ids: &empty_relation_ids,
        };
        pread_execute(
            input,
            &way_descs,
            &mut writer,
            &mut stats,
            |block, bb, output| {
                let s = extract_block_pass2(
                    block,
                    &ids,
                    clean,
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
    // --- Phase 3: Classify relations + write (pread-from-workers) ---
    host.invocation.marker("SIMPLE_REL_CLASSIFY_START");
    let mut matched_relation_ids = IdSet::new();
    {
        let rel_classify_schedule = scheduled(&relation_schedule)?;
        let rel_classify_file = crate::read::input::InputFile::open(input)?;
        crate::scan::classify::parallel_classify_accumulate(
            &rel_classify_file,
            &rel_classify_schedule,
            None,
            IdSet::new,
            |block, ids| {
                for element in block.elements_skip_metadata() {
                    if let Element::Relation(r) = &element
                        && relation_has_matched_member(r, &bbox_node_ids, &matched_way_ids)
                    {
                        ids.set(r.id());
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
            bbox_node_ids: &bbox_node_ids,
            all_way_node_ids: &all_way_node_ids,
            matched_way_ids: &matched_way_ids,
            matched_relation_ids: &matched_relation_ids,
        };
        pread_execute(
            input,
            &rel_descs,
            &mut writer,
            &mut stats,
            |block, bb, output| {
                let s = extract_block_pass2(
                    block,
                    &ids,
                    clean,
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
    writer.flush()?;
    host.invocation.marker("EXTRACT_SCAN_END");
    Ok(stats)
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

/// Verify one passthrough candidate from its classify-phase decode: the
/// observed node count when every scanned node landed in the region and the
/// payload has the dense-only shape that makes the scan complete, else
/// `None`. Occurrence counts, not set sizes: a duplicate id counts every
/// time it would be copied.
fn verify_candidate(
    decompressed: &[u8],
    scanned: usize,
    matching: usize,
) -> crate::error::Result<Option<u64>> {
    if matching != scanned || !dense_only_payload(decompressed)? {
        return Ok(None);
    }
    // What the decode path would do with this blob; a block it rejects must
    // not reach the output through a raw copy instead.
    PrimitiveBlock::from_vec(decompressed.to_vec())?;
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

/// Process a batch of blocks in parallel for Pass 2 of complete-ways extraction.
fn process_extract_pass2_batch(
    batch: &[PrimitiveBlock],
    ids: &ExtractPass2IdSets<'_>,
    clean: &CleanAttrs,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut ExtractStats,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    type BatchResult = crate::Result<(Vec<OwnedBlock>, ExtractStats)>;
    let results: Vec<BatchResult> = host.executor.install(|| {
        batch
            .par_iter()
            .map_init(BlockBuilder::new, |bb, block| {
                let mut output: Vec<OwnedBlock> = Vec::new();
                let block_stats = extract_block_pass2(block, ids, clean, None, bb, &mut output)?;
                flush_local(bb, &mut output)?;
                Ok((output, block_stats))
            })
            .collect()
    });

    drain_batch_results(results, writer, |s| merge_extract_stats(stats, &s), host)?;
    Ok(())
}
