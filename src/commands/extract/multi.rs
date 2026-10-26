//! Multi-extract: single-pass multi-region extraction for sorted inputs (simple strategy).

use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::writer::{Compression, PbfWriter};
use crate::{Element, ElementKind, PrimitiveBlock};

use super::super::{
    HeaderOverrides, Result, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, flush_local,
};
use crate::idset::IdSet;
use crate::read::input::{InputFile, PinnedInput};
use crate::read::region_grid::RegionGrid;
use crate::scan::classify::ScheduledBlob;

use super::common::{
    BboxInt, Passthrough, dense_only_payload, relation_has_matched_member, spatial_blob_filter,
    warn_passthrough_demotions,
};
use super::{ExtractStats, Region};

use crate::owned::{dense_node_metadata, element_metadata, relation_members_into};

/// Try single-pass multi-extract: read the PBF once, classify each element
/// against all N regions, write to N output files. Returns `None` to fall
/// back to sequential if the input isn't sorted.
///
/// Simple strategy only (no per-region way/relation ID tracking beyond what
/// fits in memory). Uses sync-mode PbfWriters (no per-writer thread).
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cognitive_complexity
)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn try_extract_multi_single_pass(
    input: &PinnedInput,
    regions: &[Region],
    outputs: &crate::output::OutputSet,
    set_bounds: bool,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<Option<Vec<ExtractStats>>> {
    use std::io::BufWriter;

    // Check if input is sorted.
    let header = {
        let mut br = crate::BlobReader::open_in(input, direct_io, host)?;
        match br.next() {
            Some(Ok(blob)) => match blob.decode()? {
                crate::blob::BlobDecode::OsmHeader(h) => {
                    if !h.is_sorted() {
                        return Ok(None); // fall back to sequential
                    }
                    h
                }
                _ => return Ok(None),
            },
            Some(Err(e)) => {
                // Declining here keeps the historical fallback (the
                // sequential passes then fail on their own read), but the
                // first error must not vanish: name it before falling back.
                crate::host::diag!(
                    host.diagnostics,
                    Note,
                    "extract.multi.probe_read_error",
                    "Multi-extract single-pass probe could not read the first blob: {}",
                    e.report()
                );
                crate::debug::emit_counter("multi_extract_probe_read_error", 1);
                return Ok(None);
            }
            None => return Ok(None),
        }
    };

    let n = regions.len();
    crate::debug::emit_marker("MULTI_EXTRACT_START");
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "extract.multi.single_pass",
        "[multi-extract] single-pass: {n} regions, simple strategy"
    );

    // Precompute per-region integer bboxes.
    let bbox_ints: Vec<BboxInt> = regions
        .iter()
        .map(|region| BboxInt::from_bbox(region.bbox()))
        .collect();
    let inside_bbox_ints: Vec<BboxInt> = regions
        .iter()
        .map(|region| BboxInt::inside_bbox(region.bbox()))
        .collect();

    // Union bbox for blob-level spatial skip.
    let union_bbox = BboxInt {
        min_lon: bbox_ints
            .iter()
            .map(|b| b.min_lon)
            .min()
            .unwrap_or(i32::MIN),
        min_lat: bbox_ints
            .iter()
            .map(|b| b.min_lat)
            .min()
            .unwrap_or(i32::MIN),
        max_lon: bbox_ints
            .iter()
            .map(|b| b.max_lon)
            .max()
            .unwrap_or(i32::MAX),
        max_lat: bbox_ints
            .iter()
            .map(|b| b.max_lat)
            .max()
            .unwrap_or(i32::MAX),
    };
    let spatial_filter = spatial_blob_filter(&union_bbox);

    // Open N sync-mode writers. Each owns a 256 KiB buffer; publication
    // syncs each staged output after the final flush. All N outputs are compressed serially on
    // the consumer thread. Switching to the parallel writer was tried and
    // reverted on 2026-08-20: two interleaved Denmark 5-region pairs came out
    // with opposite signs (parallel/sync 1200/1100 ms, then 501/1101 ms), which
    // is inconclusive, not evidence of a speedup. Do not re-attempt without a
    // stable matched measurement at a larger scale - the single-region writer's
    // shape is not known to transfer to N concurrent outputs.
    let mut writers: Vec<PbfWriter<BufWriter<std::fs::File>>> = Vec::with_capacity(n);
    for (region, output) in regions.iter().zip(outputs.iter()) {
        let display_path = output.destination();
        let bbox = region.bbox();
        let header_bytes = super::super::output_header::build_one(
            &header,
            &super::output_facts(set_bounds, bbox),
            overrides,
            host,
        )?
        .bytes;
        let completion = output.begin_write()?;
        let mut w = PbfWriter::to_staged_file(output.path(), compression)
            .map_err(|e| e.context(format!("failed to create {}", display_path.display())))?
            .with_completion(completion);
        // The writer names its output in any error it returns.
        w.write_header(&header_bytes)?;
        writers.push(w);
    }

    // Three persistent membership sets per region; regions may overlap, so
    // sharing one set would lose which outputs need an object. These are lazy
    // IdSets, not pre-allocated full-ID-space bitmaps. Budget 4 MiB per dense
    // 2^25-ID chunk per region and kind, plus sparse chunks and directories.
    // For dense coverage through maximum IDs Mn/Mw/Mr, the bitmap component is
    // N * 4 MiB * sum(floor(Mkind / 2^25) + 1). Overlap can multiply this by
    // all N regions; the region-count limit is not a memory limit. Worker
    // classification results, relation sets and output builders add to this.
    // No up-front refusal uses this worst case: disjoint small regions can
    // touch far fewer chunks, and header ID ranges cannot predict membership.
    // A useful admission budget needs observed chunk occupancy and a caller's
    // memory policy, not a machine-RAM guess inside this library. Preserve the
    // measured per-blob Vec accumulation: direct hot-loop set insertion and
    // whole-worker dense accumulation have already lost on time and memory,
    // respectively (reference/performance-history.md).
    // Per-region ID sets and stats.
    let mut bbox_node_ids: Vec<IdSet> = (0..n).map(|_| IdSet::new()).collect();
    let mut matched_way_ids: Vec<IdSet> = (0..n).map(|_| IdSet::new()).collect();
    let mut matched_relation_ids: Vec<IdSet> = (0..n).map(|_| IdSet::new()).collect();
    let mut stats: Vec<ExtractStats> = (0..n)
        .map(|_| ExtractStats {
            nodes_in_bbox: 0,
            nodes_from_ways: 0,
            nodes_from_relations: 0,
            ways_written: 0,
            ways_from_relations: 0,
            relations_written: 0,
            strategy: "simple",
        })
        .collect();

    // Build schedules by element type for parallel classification.
    // Walk via the pread-only HeaderWalker so blob bodies stay out of the
    // page cache during the scan - the per-kind classification passes open
    // fresh fds and pread only the blobs they need.
    //
    // This second open is deliberate too: the probe above is the sole judge
    // of the leading header (it decides the decline contract, honouring
    // direct I/O), and the walk wants its own FADV_RANDOM descriptor. The walk
    // starts at the beginning and consumes the leading frame without
    // re-judging it; the walker would accept a data blob there, so it is
    // not a second missing-header check.
    crate::debug::emit_marker("MULTI_SCHEDULE_SCAN_START");
    let mut walker = crate::read::header_walker::HeaderWalker::open(input, host)?;
    let _ = walker
        .next_header()?
        .ok_or_else(|| crate::error::new_missing_header(input.path()))?;

    let mut node_schedule: Vec<ScheduledBlob> = Vec::new();
    let mut way_schedule: Vec<ScheduledBlob> = Vec::new();
    let mut relation_schedule: Vec<ScheduledBlob> = Vec::new();
    // Per-node-blob passthrough metadata.
    let mut node_blob_info: Vec<NodeBlobInfo> = Vec::new();
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        walker.observe_filter_index(meta.index.as_ref());
        let entry = ScheduledBlob::from_meta(&meta)?;
        // Both decisions below are taken on indexdata claims, never decoded
        // to check: a blob claimed as nodes whose bbox misses every region
        // is skipped unread, and each blob joins the one per-kind schedule
        // its kind names, so a mislabelled blob's elements are missed or
        // dropped. Deliberate: claims are believed, not verified
        // (reference/indexdata-trust.md); `check --index` is the verifier.
        if let Some(idx) = meta.index.as_ref() {
            if !spatial_filter.wants_index(idx) {
                continue;
            }
            match idx.kind {
                crate::ElementKind::Node => {
                    // Raw passthrough is only sound for bbox regions - polygon
                    // regions can exclude nodes inside the bbox but outside the
                    // polygon boundary or inside holes - and only when the
                    // blob goes to every region. This nominates on the
                    // indexdata bbox claim; the node classify phase verifies.
                    let candidate = idx.bbox.as_ref().is_some_and(|blob_bbox| {
                        inside_bbox_ints
                            .iter()
                            .zip(regions.iter())
                            .all(|(bi, region)| {
                                matches!(region, Region::Bbox(_))
                                    && crate::BlobBbox::new(
                                        bi.min_lat, bi.max_lat, bi.min_lon, bi.max_lon,
                                    )
                                    .contains(blob_bbox)
                            })
                    });
                    node_blob_info.push(NodeBlobInfo {
                        passthrough: if candidate {
                            Passthrough::Candidate
                        } else {
                            Passthrough::No
                        },
                        frame_offset: meta.frame_start,
                        frame_size: meta.frame_size,
                    });
                    node_schedule.push(entry);
                }
                crate::ElementKind::Way => way_schedule.push(entry),
                crate::ElementKind::Relation => relation_schedule.push(entry),
            }
        } else {
            // No indexdata - include in all schedules (conservative).
            node_blob_info.push(NodeBlobInfo {
                passthrough: Passthrough::No,
                frame_offset: meta.frame_start,
                frame_size: 0,
            });
            node_schedule.push(entry);
            way_schedule.push(entry);
            relation_schedule.push(entry);
        }
    }
    crate::debug::emit_marker("MULTI_SCHEDULE_SCAN_END");

    // DO NOT add per-region partial raw passthrough here (decode once,
    // write raw to contained regions, route elements to non-contained
    // regions). It was a "high impact at planet scale" item on the
    // multi-extract optimization plan for a while, resting on the
    // heuristic that region-contained blobs would cluster under
    // contiguous-bbox regions.
    //
    // Measured end-to-end on 2026-04-20 via a shadow counter that did
    // the full per-region bbox-containment classification per blob
    // without actually passing anything through raw. Shadow added in
    // the 2026-04-19 landing, reverted here. Planet 5-region
    // `--config --simple` at commit 57b01f9, UUID dad573cb:
    //
    //   0 / 32,835 node blobs would have qualified for partial
    //   passthrough. Zero. All 32,835 blobs (10.4 B elements) landed
    //   in the `none_contained` bucket - no region bbox contains any
    //   whole blob. The existing all-N-contained fast path also fired
    //   0 times.
    //
    // The math is hostile structurally, not a property of this region
    // set: PBF node blobs are ID-sorted, and OSM IDs are chronological
    // rather than geographic, so nodes in a single 8,000-element blob
    // scatter across the planet. Each blob's geographic bbox is
    // ~planet-wide, so it cannot be contained in a sub-planet region
    // under any bbox subdivision. Geography-sorted PBFs (Hilbert
    // curve over lat/lon) would flip this, but they break
    // Sort.Type_then_ID and are not what we ingest.
    //
    // The existing all-N-contained raw-passthrough path in
    // multi_extract_pread_write_nodes is kept for its real but narrow
    // niche: N=1 extracts and fully-overlapping regions. Don't remove
    // it on the "fires 0 times at planet 5-region" evidence - the
    // planet multi-region config is the hardest case, not the only
    // one.
    //
    // Sister pin: src/commands/tags_filter/mod.rs pass-2 worker (same
    // precedent: shadow measurement produced zero, reverted, leave the
    // pin as the block against re-attempting).

    let shared_file = walker.input();
    drop(walker);

    // Phase 1: Parallel node classification → N bbox_node_ids.
    // For all-bbox regions, use columnar decode (batch IDs/lats/lons into
    // contiguous arrays) with single-pass multi-region classification.
    // Polygon regions fall back to element-by-element iteration.
    let all_bbox = regions
        .iter()
        .all(|region| matches!(region, Region::Bbox(_)));
    use crate::tunables::GRID_REGION_THRESHOLD;
    let region_bboxes: Vec<(i32, i32, i32, i32)> = inside_bbox_ints
        .iter()
        .map(|bi| (bi.min_lat, bi.max_lat, bi.min_lon, bi.max_lon))
        .collect();
    let grid = if n >= GRID_REGION_THRESHOLD {
        // A test can decline the grid to drive the same config down the
        // linear scan and compare output byte for byte.
        #[cfg(feature = "test-hooks")]
        let grid = if host.cancel.faults().forces_linear_region_grid() {
            None
        } else {
            RegionGrid::build(&region_bboxes)
        };
        #[cfg(not(feature = "test-hooks"))]
        let grid = RegionGrid::build(&region_bboxes);
        if grid.is_none() {
            // Output is unchanged (the linear classifier tests every
            // region), but node classification cost grows with N.
            crate::host::diag!(
                host.diagnostics,
                Note,
                "extract.multi.grid_fallback",
                "Multi-extract region grid declined {n} regions (coverage over budget or an empty interior); classifying nodes against every region"
            );
            crate::debug::emit_counter("multi_extract_grid_fallback", 1);
        }
        grid
    } else {
        None
    };
    crate::debug::emit_marker("MULTI_NODE_CLASSIFY_START");
    // Observed node count per node-schedule position, for passthrough
    // candidates that verified. See `Passthrough`.
    let mut verified_nodes: Vec<Option<u64>> = vec![None; node_schedule.len()];
    if all_bbox {
        let bboxes = region_bboxes;
        // The one classifier for every node blob, candidate or not, so that
        // verification can never change which nodes are selected.
        let classify_columns = |block: &PrimitiveBlock,
                                columns: &mut crate::read::columnar::DenseNodeColumns,
                                scratch: &mut Vec<Vec<i64>>|
         -> crate::Result<Vec<Vec<i64>>> {
            block.decode_dense_columns(columns);
            for v in scratch.iter_mut() {
                v.clear();
            }
            match &grid {
                Some(grid) => {
                    columns.collect_matching_ids_multi_bbox_grid(&bboxes, grid, scratch);
                }
                None => columns.collect_matching_ids_multi_bbox(&bboxes, scratch),
            }
            // Dense columns deliberately exclude plain Node messages. Visit
            // only those messages here; keep dense classification batched.
            for group in block.groups() {
                for node in group.nodes() {
                    let lat = node.decimicro_lat();
                    let lon = node.decimicro_lon();
                    for (region, &(min_lat, max_lat, min_lon, max_lon)) in bboxes.iter().enumerate()
                    {
                        if lat >= min_lat && lat <= max_lat && lon >= min_lon && lon <= max_lon {
                            scratch[region].push(node.id());
                        }
                    }
                }
            }
            Ok(scratch.iter_mut().map(std::mem::take).collect::<Vec<_>>())
        };
        let worker_init = || {
            (
                crate::read::columnar::DenseNodeColumns::new(),
                vec![Vec::<i64>::new(); n],
            )
        };

        // Passthrough candidates are classified from the raw bytes, so the
        // structural check in `dense_only_payload` can see them. The pools
        // report positions within each subset; `candidate_positions` maps a
        // candidate's back to its node-schedule position, which the write
        // phase below keys on.
        let (candidate_positions, other_positions): (Vec<usize>, Vec<usize>) = (0..node_schedule
            .len())
            .partition(|&pos| node_blob_info[pos].passthrough == Passthrough::Candidate);
        let candidate_schedule: Vec<ScheduledBlob> = candidate_positions
            .iter()
            .map(|&pos| node_schedule[pos])
            .collect();
        let other_schedule: Vec<ScheduledBlob> = other_positions
            .iter()
            .map(|&pos| node_schedule[pos])
            .collect();

        crate::scan::classify::parallel_classify_phase(
            &shared_file,
            &other_schedule,
            None,
            worker_init,
            |block, (columns, scratch)| classify_columns(block, columns, scratch),
            |_pos, region_ids: Vec<Vec<i64>>| merge_region_ids(&mut bbox_node_ids, region_ids),
            host,
        )?;
        crate::scan::classify::parallel_scan_blobs_raw(
            &shared_file,
            &candidate_schedule,
            None,
            worker_init,
            |_position, bytes, (columns, scratch)| {
                let eligible = dense_only_payload(bytes)?;
                // What the decode path would do with this blob; a block it
                // rejects must not be copied raw.
                let block = PrimitiveBlock::from_vec(bytes.to_vec())?;
                let region_ids = classify_columns(&block, columns, scratch)?;
                // Occurrence counts, not set sizes: a duplicate id counts
                // every time it is copied.
                let observed = columns.len();
                let verified = (eligible && region_ids.iter().all(|ids| ids.len() == observed))
                    .then_some(observed as u64);
                Ok((region_ids, verified))
            },
            |pos, (region_ids, verified): (Vec<Vec<i64>>, Option<u64>)| {
                merge_region_ids(&mut bbox_node_ids, region_ids);
                verified_nodes[candidate_positions[pos]] = verified;
            },
            host,
        )?;
    } else {
        crate::scan::classify::parallel_classify_phase(
            &shared_file,
            &node_schedule,
            None,
            || vec![Vec::<i64>::new(); n],
            |block, scratch| {
                for v in scratch.iter_mut() {
                    v.clear();
                }
                for element in block.elements_skip_metadata() {
                    let (id, lat, lon) = match &element {
                        Element::DenseNode(node) => {
                            (node.id(), node.decimicro_lat(), node.decimicro_lon())
                        }
                        Element::Node(node) => {
                            (node.id(), node.decimicro_lat(), node.decimicro_lon())
                        }
                        _ => continue,
                    };
                    match &grid {
                        Some(grid) => {
                            for &region in grid.candidates(lat, lon) {
                                let region = region as usize;
                                if regions[region].contains_decimicro(&bbox_ints[region], lat, lon)
                                {
                                    scratch[region].push(id);
                                }
                            }
                        }
                        None => {
                            for region in 0..n {
                                if regions[region].contains_decimicro(&bbox_ints[region], lat, lon)
                                {
                                    scratch[region].push(id);
                                }
                            }
                        }
                    }
                }
                Ok(scratch.iter_mut().map(std::mem::take).collect::<Vec<_>>())
            },
            |_seq, region_ids: Vec<Vec<i64>>| merge_region_ids(&mut bbox_node_ids, region_ids),
            host,
        )?;
    }
    crate::debug::emit_marker("MULTI_NODE_CLASSIFY_END");

    // Settle every candidate: verified ones pass through counted by what
    // classify observed, the rest are decoded like any other blob.
    let mut verified_blobs: u64 = 0;
    let mut demoted_blobs: u64 = 0;
    for (info, verified) in node_blob_info.iter_mut().zip(&verified_nodes) {
        if info.passthrough == Passthrough::Candidate {
            info.passthrough = match *verified {
                Some(nodes) => {
                    verified_blobs += 1;
                    Passthrough::Verified { nodes }
                }
                None => {
                    demoted_blobs += 1;
                    Passthrough::No
                }
            };
        }
    }
    warn_passthrough_demotions(verified_blobs, demoted_blobs, host);

    // Phase 1 write: parallel decode with raw passthrough for fully-contained node blobs.
    crate::debug::emit_marker("MULTI_NODE_WRITE_START");
    multi_extract_pread_write_nodes(
        &shared_file,
        &node_schedule,
        &node_blob_info,
        n,
        |block, bbs, output, _scratch| {
            let mut counts = vec![0u64; n];
            for element in block.elements() {
                match &element {
                    // The `any` prefilter is a pure fast path over the same
                    // sets the inner loop tests: it keeps metadata decode off
                    // the overwhelmingly common no-region-matches node.
                    Element::DenseNode(dn) if bbox_node_ids.iter().any(|s| s.get(dn.id())) => {
                        let id = dn.id();
                        let meta = dense_node_metadata(dn)?;
                        for i in 0..n {
                            if bbox_node_ids[i].get(id) {
                                ensure_node_capacity_local(&mut bbs[i], &mut output[i])?;
                                bbs[i].add_node(
                                    id,
                                    dn.decimicro_lat(),
                                    dn.decimicro_lon(),
                                    dn.tags(),
                                    meta.as_ref(),
                                );
                                counts[i] += 1;
                            }
                        }
                    }
                    Element::Node(nd) if bbox_node_ids.iter().any(|s| s.get(nd.id())) => {
                        let id = nd.id();
                        let meta = element_metadata(ElementKind::Node, id, &nd.info())?;
                        for i in 0..n {
                            if bbox_node_ids[i].get(id) {
                                ensure_node_capacity_local(&mut bbs[i], &mut output[i])?;
                                bbs[i].add_node(
                                    id,
                                    nd.decimicro_lat(),
                                    nd.decimicro_lon(),
                                    nd.tags(),
                                    meta.as_ref(),
                                );
                                counts[i] += 1;
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(counts)
        },
        &mut writers,
        &mut stats,
        host,
    )?;
    crate::debug::emit_marker("MULTI_NODE_WRITE_END");

    // Phase 2: Parallel way classification → N matched_way_ids.
    //
    // Per-worker scratch `Vec<Vec<i64>>` is cleared (not dropped) between
    // blocks so the inner `Vec<i64>` capacity amortizes across the ~N blobs
    // each worker processes - the same pattern used by the node classify
    // phase above (see `|| vec![Vec::<i64>::new(); n]` there). Pre-
    // instrumentation Japan 5-region bench showed this phase at 892 ms
    // with the prior `|| ()` + per-block `vec![Vec::new(); n]` allocation.
    crate::debug::emit_marker("MULTI_WAY_CLASSIFY_START");
    crate::scan::classify::parallel_classify_phase(
        &shared_file,
        &way_schedule,
        None,
        || vec![Vec::<i64>::new(); n],
        |block, scratch: &mut Vec<Vec<i64>>| {
            for v in scratch.iter_mut() {
                v.clear();
            }
            let mut region_mask = vec![false; n];
            for element in block.elements_skip_metadata() {
                if let Element::Way(w) = &element {
                    region_mask.fill(false);
                    let mut remaining = n;
                    for node_id in w.refs() {
                        for i in 0..n {
                            if !region_mask[i] && bbox_node_ids[i].get(node_id) {
                                region_mask[i] = true;
                                remaining -= 1;
                            }
                        }
                        if remaining == 0 {
                            break;
                        }
                    }
                    for (i, &matched) in region_mask.iter().enumerate() {
                        if matched {
                            scratch[i].push(w.id());
                        }
                    }
                }
            }
            Ok(scratch.iter_mut().map(std::mem::take).collect::<Vec<_>>())
        },
        |_seq, region_ids| {
            for (i, ids) in region_ids.into_iter().enumerate() {
                for id in ids {
                    matched_way_ids[i].set(id);
                }
            }
        },
        host,
    )?;
    crate::debug::emit_marker("MULTI_WAY_CLASSIFY_END");

    // Phase 2 write: parallel decode, write matching ways to N writers.
    crate::debug::emit_marker("MULTI_WAY_WRITE_START");
    multi_extract_pread_write(
        &shared_file,
        &way_schedule,
        n,
        |block, bbs, output, scratch| {
            let mut counts = vec![0u64; n];
            for element in block.elements() {
                if let Element::Way(w) = &element {
                    let wid = w.id();
                    if !matched_way_ids.iter().any(|s| s.get(wid)) {
                        continue;
                    }
                    scratch.clear();
                    scratch.extend(w.refs());
                    let meta = element_metadata(ElementKind::Way, wid, &w.info())?;
                    for i in 0..n {
                        if matched_way_ids[i].get(wid) {
                            ensure_way_capacity_local(&mut bbs[i], &mut output[i])?;
                            bbs[i].add_way(wid, w.tags(), scratch, meta.as_ref());
                            counts[i] += 1;
                        }
                    }
                }
            }
            Ok(counts)
        },
        &mut writers,
        &mut stats,
        |s| &mut s.ways_written,
        host,
    )?;
    crate::debug::emit_marker("MULTI_WAY_WRITE_END");

    // Phase 3: Parallel relation classification → N matched_relation_ids.
    crate::debug::emit_marker("MULTI_REL_CLASSIFY_START");
    crate::scan::classify::parallel_classify_accumulate(
        &shared_file,
        &relation_schedule,
        None,
        || (0..n).map(|_| IdSet::new()).collect::<Vec<_>>(),
        |block, region_ids| {
            for element in block.elements_skip_metadata() {
                if let Element::Relation(r) = &element {
                    for i in 0..n {
                        if relation_has_matched_member(r, &bbox_node_ids[i], &matched_way_ids[i]) {
                            region_ids[i].set(r.id());
                        }
                    }
                }
            }
            Ok(())
        },
        |region_ids| {
            for (i, worker_set) in region_ids.into_iter().enumerate() {
                matched_relation_ids[i].merge(worker_set);
            }
        },
        host,
    )?;
    crate::debug::emit_marker("MULTI_REL_CLASSIFY_END");

    // Phase 3 write: parallel decode, write matching relations to N writers.
    crate::debug::emit_marker("MULTI_REL_WRITE_START");
    multi_extract_pread_write(
        &shared_file,
        &relation_schedule,
        n,
        |block, bbs, output, _scratch| {
            let mut counts = vec![0u64; n];
            let mut members_buf: Vec<MemberData<'_>> = Vec::new();
            for element in block.elements() {
                if let Element::Relation(r) = &element {
                    let rid = r.id();
                    if !matched_relation_ids.iter().any(|s| s.get(rid)) {
                        continue;
                    }
                    relation_members_into(r, &mut members_buf)?;
                    let meta = element_metadata(ElementKind::Relation, rid, &r.info())?;
                    for i in 0..n {
                        if matched_relation_ids[i].get(rid) {
                            ensure_relation_capacity_local(&mut bbs[i], &mut output[i])?;
                            bbs[i].add_relation(rid, r.tags(), &members_buf, meta.as_ref());
                            counts[i] += 1;
                        }
                    }
                }
            }
            Ok(counts)
        },
        &mut writers,
        &mut stats,
        |s| &mut s.relations_written,
        host,
    )?;
    crate::debug::emit_marker("MULTI_REL_WRITE_END");

    // Flush all writers (workers already flushed their BlockBuilders per blob).
    for (writer, output) in writers.iter_mut().zip(outputs.iter()) {
        let display_path = output.destination();
        writer
            .flush()
            .map_err(|e| e.context(format!("failed to flush {}", display_path.display())))?;
    }

    // Counters: schedule sizes + cross-region totals. Parallels the single-region
    // `extract()` wrapper at the bottom of `pub fn extract` - the single-pass
    // path bypasses that wrapper, so without this block `brokkr sidecar
    // --counters` is empty for multi-extract runs.
    crate::debug::emit_counter("multi_extract_region_count", n);
    crate::debug::emit_counter("multi_extract_node_blobs", node_schedule.len());
    crate::debug::emit_counter("multi_extract_way_blobs", way_schedule.len());
    crate::debug::emit_counter("multi_extract_relation_blobs", relation_schedule.len());
    let total_nodes: u64 = stats.iter().map(|s| s.nodes_in_bbox).sum();
    let total_ways: u64 = stats.iter().map(|s| s.ways_written).sum();
    let total_relations: u64 = stats.iter().map(|s| s.relations_written).sum();
    crate::debug::emit_counter("multi_extract_nodes_written", total_nodes);
    crate::debug::emit_counter("multi_extract_ways_written", total_ways);
    crate::debug::emit_counter("multi_extract_relations_written", total_relations);
    crate::debug::emit_marker("MULTI_EXTRACT_END");

    Ok(Some(stats))
}

// ---------------------------------------------------------------------------
// Parallel batch infrastructure
// ---------------------------------------------------------------------------

/// Per-node-blob passthrough metadata, parallel to the node schedule.
struct NodeBlobInfo {
    /// `Candidate` when the indexdata bbox claims containment in every
    /// region; settled to `Verified` or `No` after node classify.
    passthrough: Passthrough,
    frame_offset: u64,
    frame_size: usize,
}

/// Merge one blob's per-region matching node ids into the region id sets.
fn merge_region_ids(bbox_node_ids: &mut [IdSet], region_ids: Vec<Vec<i64>>) {
    for (set, ids) in bbox_node_ids.iter_mut().zip(region_ids) {
        for id in ids {
            set.set(id);
        }
    }
}

/// Node write phase with raw passthrough for verified blobs.
///
/// Blobs verified as fully contained in ALL N regions are written as raw
/// frames to all N writers without decode, counted by the node count
/// classify observed. Other blobs go through parallel decode workers.
/// Both streams are interleaved in sequence order via ReorderBuffer.
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
fn multi_extract_pread_write_nodes<F>(
    input: &InputFile,
    schedule: &[ScheduledBlob],
    blob_info: &[NodeBlobInfo],
    n: usize,
    block_fn: F,
    writers: &mut [PbfWriter<std::io::BufWriter<std::fs::File>>],
    stats: &mut [ExtractStats],
    host: &crate::host::Host,
) -> Result<()>
where
    F: Fn(
            &PrimitiveBlock,
            &mut Vec<BlockBuilder>,
            &mut Vec<Vec<OwnedBlock>>,
            &mut Vec<i64>,
        ) -> crate::Result<Vec<u64>>
        + Send
        + Sync,
{
    // Blobs verified as fully contained in ALL N regions skip decode
    // entirely - the consumer writes their raw frame to all N writers
    // between the decoded ones. Other blobs, including any unsettled
    // candidate, go through the parallel decode workers.
    let len = blob_info.len().min(schedule.len());
    let decode_at: Vec<usize> = blob_info[..len]
        .iter()
        .enumerate()
        .filter(|(_, info)| !matches!(info.passthrough, Passthrough::Verified { .. }))
        .map(|(position, _)| position)
        .collect();

    let pt = len - decode_at.len();
    if pt > 0 {
        let dc = decode_at.len();
        crate::host::diag!(
            host.diagnostics,
            Progress,
            "extract.multi.node_blobs",
            "  node blobs: {pt} passthrough, {dc} decoded"
        );
    }

    let decode_threads = crate::host::sized_workers(
        &*host.resources,
        crate::tunables::DECODE_WORKERS_RESERVED,
        crate::tunables::DECODE_WORKERS_MIN,
        crate::tunables::DECODE_WORKERS_FALLBACK,
    );
    let decode_at = &decode_at;
    let mut consumer = MultiNodeConsumer {
        interleaved: crate::ordered_pool::Interleaved::new(decode_at, len),
        blob_info,
        input,
        frame_buf: Vec::new(),
        writers,
        stats,
        cancel: &host.cancel,
    };
    let mut workers = vec![(); decode_threads];
    crate::ordered_pool::run_ordered(
        consumer.interleaved.decode_count(),
        &mut workers,
        crate::ordered_pool::PoolConfig {
            window: crate::ordered_pool::default_window(decode_threads),
            cancel: &host.cancel,
            waits: crate::ordered_pool::Waits::default(),
        },
        |_| MultiLocal::new(n),
        |(), local: &mut MultiLocal, ordinal| {
            let entry = &schedule[decode_at[ordinal]];
            let block = local.decode(input, entry)?;
            local
                .classify(&block, n, &block_fn)
                .map_err(|e| input.locate_blob(entry, e))
        },
        &mut consumer,
    )?;
    for position in consumer.interleaved.rest() {
        consumer.passthrough(position)?;
    }
    Ok(())
}

/// One multi-extract worker's thread-local state: per-region builders and
/// output, plus the decode scratch.
struct MultiLocal {
    read_buf: Vec<u8>,
    bbs: Vec<BlockBuilder>,
    output: Vec<Vec<OwnedBlock>>,
    pool: std::sync::Arc<crate::blob::DecompressPool>,
    st_scratch: Vec<(u32, u32)>,
    gr_scratch: Vec<(u32, u32)>,
    i64_scratch: Vec<i64>,
}

impl MultiLocal {
    fn new(n: usize) -> Self {
        Self {
            read_buf: Vec::new(),
            bbs: (0..n).map(|_| BlockBuilder::new()).collect(),
            output: (0..n).map(|_| Vec::new()).collect(),
            pool: crate::blob::DecompressPool::new(),
            st_scratch: Vec::new(),
            gr_scratch: Vec::new(),
            i64_scratch: Vec::new(),
        }
    }

    /// pread, decompress and parse `entry`, located with the input on failure.
    fn decode(&mut self, input: &InputFile, entry: &ScheduledBlob) -> Result<PrimitiveBlock> {
        use std::os::unix::fs::FileExt as _;
        (|| {
            self.read_buf.resize(entry.data_size(), 0);
            input
                .file()
                .read_exact_at(&mut self.read_buf, entry.data_offset())?;
            let mut buf = crate::blob::pool_get(
                Some(&self.pool),
                entry.data_size() * crate::tunables::DECOMPRESS_EXPANSION_GUESS,
            );
            crate::blob::decompress_blob_raw(&self.read_buf, &mut buf)?;
            PrimitiveBlock::from_vec_pooled_with_scratch(
                buf,
                &self.pool,
                &mut self.st_scratch,
                &mut self.gr_scratch,
            )
        })()
        .map_err(|e| input.locate_blob(entry, e))
    }

    /// Classify `block` against the N regions and take each region's blocks.
    fn classify<F>(
        &mut self,
        block: &PrimitiveBlock,
        n: usize,
        block_fn: &F,
    ) -> crate::Result<(Vec<Vec<OwnedBlock>>, Vec<u64>)>
    where
        F: Fn(
            &PrimitiveBlock,
            &mut Vec<BlockBuilder>,
            &mut Vec<Vec<OwnedBlock>>,
            &mut Vec<i64>,
        ) -> crate::Result<Vec<u64>>,
    {
        for v in &mut self.output {
            v.clear();
        }
        let counts = block_fn(
            block,
            &mut self.bbs,
            &mut self.output,
            &mut self.i64_scratch,
        )?;
        for i in 0..n {
            flush_local(&mut self.bbs[i], &mut self.output[i])?;
        }
        Ok((self.output.iter_mut().map(std::mem::take).collect(), counts))
    }
}

/// Write one decoded blob's per-region blocks to the N writers, adding each
/// region's count through `stat_field`.
fn write_regions(
    (region_blocks, counts): (Vec<Vec<OwnedBlock>>, Vec<u64>),
    writers: &mut [PbfWriter<std::io::BufWriter<std::fs::File>>],
    stats: &mut [ExtractStats],
    stat_field: fn(&mut ExtractStats) -> &mut u64,
) -> Result<()> {
    for (i, blocks) in region_blocks.into_iter().enumerate() {
        *stat_field(&mut stats[i]) += counts[i];
        for block in blocks {
            writers[i].write_primitive_block_owned(block)?;
        }
    }
    Ok(())
}

/// The node pass's consumer: decoded per-region blocks and verified
/// passthrough frames, merged in schedule order.
///
/// A raw frame keeps the input's BlobHeader, sidecars included: verified
/// passthrough proved region membership, not that the header is true, so a
/// false `count` or ID range carries into every region's output (see the
/// matching note in `common::pread_execute`).
struct MultiNodeConsumer<'a> {
    interleaved: crate::ordered_pool::Interleaved<'a>,
    blob_info: &'a [NodeBlobInfo],
    input: &'a InputFile,
    frame_buf: Vec<u8>,
    writers: &'a mut [PbfWriter<std::io::BufWriter<std::fs::File>>],
    stats: &'a mut [ExtractStats],
    cancel: &'a crate::host::Cancel,
}

impl MultiNodeConsumer<'_> {
    /// Copy the verified raw frame at schedule position `position` to all N
    /// writers.
    fn passthrough(&mut self, position: usize) -> Result<()> {
        use std::os::unix::fs::FileExt as _;
        self.cancel.check()?;
        let info = &self.blob_info[position];
        let Passthrough::Verified { nodes } = info.passthrough else {
            return Err(crate::error::Error::internal(
                "multi-extract: a decode blob was scheduled as passthrough",
            ));
        };
        self.frame_buf.resize(info.frame_size, 0);
        self.input
            .file()
            .read_exact_at(&mut self.frame_buf, info.frame_offset)
            .map_err(|e| self.input.locate_at(info.frame_offset, e.into()))?;
        for (writer, stats) in self.writers.iter_mut().zip(self.stats.iter_mut()) {
            writer.write_raw(&self.frame_buf)?;
            stats.nodes_in_bbox += nodes;
        }
        Ok(())
    }
}

impl crate::ordered_pool::OrderedConsumer<(Vec<Vec<OwnedBlock>>, Vec<u64>)>
    for MultiNodeConsumer<'_>
{
    fn before_item(&mut self, ordinal: usize) -> crate::Result<()> {
        for position in self.interleaved.before(ordinal) {
            self.passthrough(position)?;
        }
        Ok(())
    }

    fn consume(
        &mut self,
        _ordinal: usize,
        item: (Vec<Vec<OwnedBlock>>, Vec<u64>),
    ) -> crate::Result<()> {
        write_regions(item, self.writers, self.stats, |s| &mut s.nodes_in_bbox)
    }
}

/// Multi-region pread-from-workers write pass.
///
/// Workers pread blob data, decompress, parse into PrimitiveBlock, then call
/// the provided closure to classify elements against N regions and produce
/// N × Vec<OwnedBlock>. The consumer writes each region's blocks to its
/// writer in sequence order.
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
fn multi_extract_pread_write<F>(
    input: &InputFile,
    schedule: &[ScheduledBlob],
    n: usize,
    block_fn: F,
    writers: &mut [PbfWriter<std::io::BufWriter<std::fs::File>>],
    stats: &mut [ExtractStats],
    stat_field: fn(&mut ExtractStats) -> &mut u64,
    host: &crate::host::Host,
) -> Result<()>
where
    F: Fn(
            &PrimitiveBlock,
            &mut Vec<BlockBuilder>,
            &mut Vec<Vec<OwnedBlock>>,
            &mut Vec<i64>,
        ) -> crate::Result<Vec<u64>>
        + Send
        + Sync,
{
    // The ordered classify pool: per-region results in schedule order,
    // bounded by its window. Each worker keeps its per-region builders,
    // output vectors and ID scratch across blobs, built on its own thread.
    crate::scan::classify::parallel_classify_phase_ordered(
        input,
        schedule,
        None,
        None,
        || MultiLocal::new(n),
        |block, local: &mut MultiLocal| local.classify(block, n, &block_fn),
        |_position, item| write_regions(item, writers, stats, stat_field),
        host,
    )
}
