//! Multi-extract: single-pass multi-region extraction for sorted inputs (simple strategy).

use std::path::Path;

use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::writer::{Compression, PbfWriter};
use crate::{Element, PrimitiveBlock};

use super::super::{
    HeaderOverrides, Result, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, flush_local,
};
use crate::idset::IdSet;
use crate::read::region_grid::RegionGrid;

use super::common::{
    BboxInt, Passthrough, dense_only_payload, relation_has_matched_member, spatial_blob_filter,
    warn_passthrough_demotions,
};
use super::{ExtractStats, Region};

use crate::owned::{dense_node_metadata, element_metadata};

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
    input: &Path,
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
        w.write_header(&header_bytes).map_err(|e| {
            e.context(format!(
                "failed to write header to {}",
                display_path.display()
            ))
        })?;
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
    crate::debug::emit_marker("MULTI_SCHEDULE_SCAN_START");
    let mut walker = crate::read::header_walker::HeaderWalker::open(input, &host.diagnostics)?;
    let _ = walker
        .next_header()?
        .ok_or_else(|| crate::error::new_missing_header(input))?;

    let mut node_schedule: Vec<(usize, u64, usize)> = Vec::new();
    let mut way_schedule: Vec<(usize, u64, usize)> = Vec::new();
    let mut relation_schedule: Vec<(usize, u64, usize)> = Vec::new();
    // Per-node-blob passthrough metadata.
    let mut node_blob_info: Vec<NodeBlobInfo> = Vec::new();
    let mut seq: usize = 0;
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        walker.observe_filter_index(meta.index.as_ref());
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
                    node_schedule.push((seq, meta.data_offset, meta.data_size));
                }
                crate::ElementKind::Way => {
                    way_schedule.push((seq, meta.data_offset, meta.data_size));
                }
                crate::ElementKind::Relation => {
                    relation_schedule.push((seq, meta.data_offset, meta.data_size));
                }
            }
        } else {
            // No indexdata - include in all schedules (conservative).
            node_blob_info.push(NodeBlobInfo {
                passthrough: Passthrough::No,
                frame_offset: meta.frame_start,
                frame_size: 0,
            });
            node_schedule.push((seq, meta.data_offset, meta.data_size));
            way_schedule.push((seq, meta.data_offset, meta.data_size));
            relation_schedule.push((seq, meta.data_offset, meta.data_size));
        }
        seq += 1;
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

    let shared_file = std::sync::Arc::clone(walker.shared_file());
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
         -> Vec<Vec<i64>> {
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
            scratch.iter_mut().map(std::mem::take).collect::<Vec<_>>()
        };
        let worker_init = || {
            (
                crate::read::columnar::DenseNodeColumns::new(),
                vec![Vec::<i64>::new(); n],
            )
        };

        // Passthrough candidates are classified from the raw bytes, so the
        // structural check in `dense_only_payload` can see them. Both
        // subsets carry the node-schedule position as their sequence number;
        // the write phase below keeps the original node_schedule order.
        let (candidate_schedule, other_schedule): (Vec<_>, Vec<_>) = node_schedule
            .iter()
            .enumerate()
            .map(|(pos, &(_seq, offset, size))| (pos, offset, size))
            .partition(|&(pos, _, _)| node_blob_info[pos].passthrough == Passthrough::Candidate);

        crate::scan::classify::parallel_classify_phase(
            &shared_file,
            &other_schedule,
            None,
            worker_init,
            |block, (columns, scratch)| classify_columns(block, columns, scratch),
            |_pos, region_ids: Vec<Vec<i64>>| merge_region_ids(&mut bbox_node_ids, region_ids),
            &*host.resources,
        )?;
        crate::scan::classify::parallel_scan_blobs_raw(
            &shared_file,
            &candidate_schedule,
            None,
            worker_init,
            |bytes, (columns, scratch)| {
                let eligible = dense_only_payload(bytes)?;
                // What the decode path would do with this blob; a block it
                // rejects must not be copied raw.
                let block = PrimitiveBlock::from_vec(bytes.to_vec())?;
                let region_ids = classify_columns(&block, columns, scratch);
                // Occurrence counts, not set sizes: a duplicate id counts
                // every time it is copied.
                let observed = columns.len();
                let verified = (eligible && region_ids.iter().all(|ids| ids.len() == observed))
                    .then_some(observed as u64);
                Ok((region_ids, verified))
            },
            |pos, (region_ids, verified): (Vec<Vec<i64>>, Option<u64>)| {
                merge_region_ids(&mut bbox_node_ids, region_ids);
                verified_nodes[pos] = verified;
            },
            &*host.resources,
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
                scratch.iter_mut().map(std::mem::take).collect::<Vec<_>>()
            },
            |_seq, region_ids: Vec<Vec<i64>>| merge_region_ids(&mut bbox_node_ids, region_ids),
            &*host.resources,
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
                        let meta = dense_node_metadata(dn);
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
                        let meta = element_metadata(&nd.info());
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
            scratch.iter_mut().map(std::mem::take).collect::<Vec<_>>()
        },
        |_seq, region_ids| {
            for (i, ids) in region_ids.into_iter().enumerate() {
                for id in ids {
                    matched_way_ids[i].set(id);
                }
            }
        },
        &*host.resources,
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
                    let meta = element_metadata(&w.info());
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
        },
        |region_ids| {
            for (i, worker_set) in region_ids.into_iter().enumerate() {
                matched_relation_ids[i].merge(worker_set);
            }
        },
        &*host.resources,
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
                    members_buf.clear();
                    members_buf.extend(r.members().map(|m| MemberData {
                        id: m.id,
                        role: m.role().unwrap_or(""),
                    }));
                    let meta = element_metadata(&r.info());
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
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("multi_extract_region_count", n as i64);
        crate::debug::emit_counter("multi_extract_node_blobs", node_schedule.len() as i64);
        crate::debug::emit_counter("multi_extract_way_blobs", way_schedule.len() as i64);
        crate::debug::emit_counter(
            "multi_extract_relation_blobs",
            relation_schedule.len() as i64,
        );
        let total_nodes: u64 = stats.iter().map(|s| s.nodes_in_bbox).sum();
        let total_ways: u64 = stats.iter().map(|s| s.ways_written).sum();
        let total_relations: u64 = stats.iter().map(|s| s.relations_written).sum();
        crate::debug::emit_counter("multi_extract_nodes_written", total_nodes as i64);
        crate::debug::emit_counter("multi_extract_ways_written", total_ways as i64);
        crate::debug::emit_counter("multi_extract_relations_written", total_relations as i64);
    }
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
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[(usize, u64, usize)],
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
    use std::os::unix::fs::FileExt as _;

    if schedule.is_empty() {
        return Ok(());
    }

    // Blobs verified as fully contained in ALL N regions skip decode
    // entirely - write raw frame to all N writers. Other blobs, including any
    // unsettled candidate, go through parallel decode workers.
    let mut decode_items: Vec<(usize, u64, usize)> = Vec::new();
    let mut passthrough_items: Vec<(usize, u64, usize, u64)> = Vec::new();
    for (local_seq, ((_global_seq, data_offset, data_size), info)) in
        schedule.iter().zip(blob_info.iter()).enumerate()
    {
        if let Passthrough::Verified { nodes } = info.passthrough {
            passthrough_items.push((local_seq, info.frame_offset, info.frame_size, nodes));
        } else {
            decode_items.push((local_seq, *data_offset, *data_size));
        }
    }

    if !passthrough_items.is_empty() {
        let pt = passthrough_items.len();
        let dc = decode_items.len();
        crate::host::diag!(
            host.diagnostics,
            Progress,
            "extract.multi.node_blobs",
            "  node blobs: {pt} passthrough, {dc} decoded"
        );
    }

    // If everything is passthrough, skip the thread scope entirely.
    if decode_items.is_empty() {
        let mut frame_buf: Vec<u8> = Vec::new();
        for &(_, frame_offset, frame_size, count) in &passthrough_items {
            frame_buf.resize(frame_size, 0);
            shared_file.read_exact_at(&mut frame_buf, frame_offset)?;
            for i in 0..n {
                writers[i].write_raw(&frame_buf)?;
                stats[i].nodes_in_bbox += count;
            }
        }
        return Ok(());
    }

    let decode_threads = crate::host::sized_workers(
        &*host.resources,
        crate::tunables::DECODE_WORKERS_RESERVED,
        crate::tunables::DECODE_WORKERS_MIN,
        crate::tunables::DECODE_WORKERS_FALLBACK,
    );

    type WorkerResult = crate::error::Result<(Vec<Vec<OwnedBlock>>, Vec<u64>)>;

    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<(usize, u64, usize)>(
        crate::tunables::POOL_DESCRIPTOR_CHANNEL_DEPTH,
    );
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<(usize, MultiNodeCI)>(
        crate::tunables::POOL_RESULT_CHANNEL_DEPTH,
    );

    std::thread::scope(|scope| -> Result<()> {
        scope.spawn(move || {
            for item in decode_items {
                if desc_tx.send(item).is_err() {
                    break;
                }
            }
        });

        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let tx = result_tx.clone();
            let file = std::sync::Arc::clone(shared_file);
            let block_fn_ref = &block_fn;
            scope.spawn(move || {
                let mut read_buf: Vec<u8> = Vec::new();
                let mut bbs: Vec<BlockBuilder> = (0..n).map(|_| BlockBuilder::new()).collect();
                let mut output: Vec<Vec<OwnedBlock>> = (0..n).map(|_| Vec::new()).collect();
                let worker_pool = crate::blob::DecompressPool::new();
                let mut st_scratch: Vec<(u32, u32)> = Vec::new();
                let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
                let mut i64_scratch: Vec<i64> = Vec::new();

                loop {
                    let (s, data_offset, data_size) = {
                        let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match guard.recv() {
                            Ok(d) => d,
                            Err(_) => break,
                        }
                    };

                    let r: WorkerResult = (|| {
                        read_buf.resize(data_size, 0);
                        file.read_exact_at(&mut read_buf, data_offset)?;
                        let mut buf = crate::blob::pool_get(
                            Some(&worker_pool),
                            data_size * crate::tunables::DECOMPRESS_EXPANSION_GUESS,
                        );
                        crate::blob::decompress_blob_raw(&read_buf, &mut buf)?;
                        let block = PrimitiveBlock::from_vec_pooled_with_scratch(
                            buf,
                            &worker_pool,
                            &mut st_scratch,
                            &mut gr_scratch,
                        )?;
                        for v in &mut output {
                            v.clear();
                        }
                        let counts = block_fn_ref(&block, &mut bbs, &mut output, &mut i64_scratch)?;
                        for i in 0..n {
                            flush_local(&mut bbs[i], &mut output[i])?;
                        }
                        let taken: Vec<Vec<OwnedBlock>> =
                            output.iter_mut().map(std::mem::take).collect();
                        Ok((taken, counts))
                    })();
                    if tx.send((s, MultiNodeCI::Decoded(r))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(desc_rx);
        drop(result_tx);

        // Pre-insert passthrough items into the reorder buffer.
        let mut reorder: crate::reorder_buffer::ReorderBuffer<MultiNodeCI> =
            crate::reorder_buffer::ReorderBuffer::with_capacity(
                crate::tunables::POOL_REORDER_CAPACITY,
            );
        for &(local_seq, frame_offset, frame_size, count) in &passthrough_items {
            reorder.push(
                local_seq,
                MultiNodeCI::Passthrough(frame_offset, frame_size, count),
            );
        }

        let mut frame_buf: Vec<u8> = Vec::new();
        for (s, item) in result_rx {
            reorder.push(s, item);
            while let Some(ci) = reorder.pop_ready() {
                write_consumer_item(ci, n, shared_file, &mut frame_buf, writers, stats)?;
            }
        }
        while let Some(ci) = reorder.pop_ready() {
            write_consumer_item(ci, n, shared_file, &mut frame_buf, writers, stats)?;
        }
        Ok(())
    })?;

    Ok(())
}

/// Write one consumer item (decoded or passthrough) to N writers.
fn write_consumer_item(
    item: MultiNodeCI,
    n: usize,
    shared_file: &std::sync::Arc<std::fs::File>,
    frame_buf: &mut Vec<u8>,
    writers: &mut [PbfWriter<std::io::BufWriter<std::fs::File>>],
    stats: &mut [ExtractStats],
) -> Result<()> {
    use std::os::unix::fs::FileExt as _;
    match item {
        MultiNodeCI::Decoded(r) => {
            let (region_blocks, counts) = r?;
            for (i, blocks) in region_blocks.into_iter().enumerate() {
                stats[i].nodes_in_bbox += counts[i];
                for OwnedBlock {
                    bytes: block_bytes,
                    index,
                    tagdata,
                    way_members,
                } in blocks
                {
                    writers[i].write_primitive_block_owned(
                        block_bytes,
                        index,
                        tagdata.as_deref(),
                        way_members.as_deref(),
                    )?;
                }
            }
        }
        MultiNodeCI::Passthrough(frame_offset, frame_size, count) => {
            frame_buf.resize(frame_size, 0);
            shared_file.read_exact_at(frame_buf, frame_offset)?;
            for i in 0..n {
                writers[i].write_raw(frame_buf)?;
                stats[i].nodes_in_bbox += count;
            }
        }
    }
    Ok(())
}

enum MultiNodeCI {
    Decoded(crate::error::Result<(Vec<Vec<OwnedBlock>>, Vec<u64>)>),
    Passthrough(u64, usize, u64),
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
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[(usize, u64, usize)],
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
    use std::os::unix::fs::FileExt as _;

    if schedule.is_empty() {
        return Ok(());
    }

    let decode_threads = crate::host::sized_workers(
        &*host.resources,
        crate::tunables::DECODE_WORKERS_RESERVED,
        crate::tunables::DECODE_WORKERS_MIN,
        crate::tunables::DECODE_WORKERS_FALLBACK,
    );

    // Worker result: per-region OwnedBlocks + per-region counts.
    type WorkerResult = crate::error::Result<(Vec<Vec<OwnedBlock>>, Vec<u64>)>;

    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<(usize, u64, usize)>(
        crate::tunables::POOL_DESCRIPTOR_CHANNEL_DEPTH,
    );
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<(usize, WorkerResult)>(
        crate::tunables::POOL_RESULT_CHANNEL_DEPTH,
    );

    std::thread::scope(|scope| -> Result<()> {
        // Dispatcher: feed schedule items to workers with local sequence index.
        scope.spawn(move || {
            for (local_seq, &(_global_seq, data_offset, data_size)) in schedule.iter().enumerate() {
                if desc_tx.send((local_seq, data_offset, data_size)).is_err() {
                    break;
                }
            }
        });

        // Workers: pread → decompress → PrimitiveBlock → classify N regions → N × OwnedBlocks.
        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let tx = result_tx.clone();
            let file = std::sync::Arc::clone(shared_file);
            let block_fn_ref = &block_fn;
            scope.spawn(move || {
                let mut read_buf: Vec<u8> = Vec::new();
                let mut bbs: Vec<BlockBuilder> = (0..n).map(|_| BlockBuilder::new()).collect();
                let mut output: Vec<Vec<OwnedBlock>> = (0..n).map(|_| Vec::new()).collect();
                let worker_pool = crate::blob::DecompressPool::new();
                let mut st_scratch: Vec<(u32, u32)> = Vec::new();
                let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
                let mut i64_scratch: Vec<i64> = Vec::new();

                loop {
                    let (s, data_offset, data_size) = {
                        let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match guard.recv() {
                            Ok(d) => d,
                            Err(_) => break,
                        }
                    };

                    let r: WorkerResult = (|| {
                        read_buf.resize(data_size, 0);
                        file.read_exact_at(&mut read_buf, data_offset)?;
                        let mut buf = crate::blob::pool_get(
                            Some(&worker_pool),
                            data_size * crate::tunables::DECOMPRESS_EXPANSION_GUESS,
                        );
                        crate::blob::decompress_blob_raw(&read_buf, &mut buf)?;
                        let block = PrimitiveBlock::from_vec_pooled_with_scratch(
                            buf,
                            &worker_pool,
                            &mut st_scratch,
                            &mut gr_scratch,
                        )?;
                        for v in &mut output {
                            v.clear();
                        }
                        let counts = block_fn_ref(&block, &mut bbs, &mut output, &mut i64_scratch)?;
                        // Flush remaining elements in each BlockBuilder.
                        for i in 0..n {
                            flush_local(&mut bbs[i], &mut output[i])?;
                        }
                        let taken: Vec<Vec<OwnedBlock>> =
                            output.iter_mut().map(std::mem::take).collect();
                        Ok((taken, counts))
                    })();
                    if tx.send((s, r)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(desc_rx);
        drop(result_tx);

        // Consumer: receive N-region results in order, write to N writers.
        let mut reorder: crate::reorder_buffer::ReorderBuffer<WorkerResult> =
            crate::reorder_buffer::ReorderBuffer::with_capacity(
                crate::tunables::POOL_REORDER_CAPACITY,
            );

        for (s, item) in result_rx {
            reorder.push(s, item);

            while let Some(result) = reorder.pop_ready() {
                let (region_blocks, counts) = result?;
                for (i, blocks) in region_blocks.into_iter().enumerate() {
                    *stat_field(&mut stats[i]) += counts[i];
                    for OwnedBlock {
                        bytes: block_bytes,
                        index,
                        tagdata,
                        way_members,
                    } in blocks
                    {
                        writers[i].write_primitive_block_owned(
                            block_bytes,
                            index,
                            tagdata.as_deref(),
                            way_members.as_deref(),
                        )?;
                    }
                }
            }
        }
        // Drain remaining.
        while let Some(result) = reorder.pop_ready() {
            let (region_blocks, counts) = result?;
            for (i, blocks) in region_blocks.into_iter().enumerate() {
                *stat_field(&mut stats[i]) += counts[i];
                for OwnedBlock {
                    bytes: block_bytes,
                    index,
                    tagdata,
                    way_members,
                } in blocks
                {
                    writers[i].write_primitive_block_owned(
                        block_bytes,
                        index,
                        tagdata.as_deref(),
                        way_members.as_deref(),
                    )?;
                }
            }
        }
        Ok(())
    })?;

    Ok(())
}
