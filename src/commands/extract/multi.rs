//! Multi-extract: single-pass multi-region extraction for sorted inputs (simple strategy).

use std::path::Path;

use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::writer::{Compression, PbfWriter};
use crate::{Element, PrimitiveBlock};

use super::super::{Result,
    flush_local, ensure_node_capacity_local, ensure_way_capacity_local,
    ensure_relation_capacity_local, HeaderOverrides,
};
use crate::idset::IdSet;

use super::common::{BboxInt, relation_has_matched_member, spatial_blob_filter};
use super::{ExtractSlot, ExtractStats, Region};

use crate::owned::{dense_node_metadata, element_metadata};

/// Try single-pass multi-extract: read the PBF once, classify each element
/// against all N regions, write to N output files. Returns `None` to fall
/// back to sequential if the input isn't sorted.
///
/// Simple strategy only (no per-region way/relation ID tracking beyond what
/// fits in memory). Uses sync-mode PbfWriters (no per-writer thread).
#[allow(clippy::too_many_arguments, clippy::too_many_lines, clippy::cognitive_complexity)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn try_extract_multi_single_pass(
    input: &Path,
    slots: &[ExtractSlot],
    set_bounds: bool,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<Option<Vec<ExtractStats>>> {
    use std::io::BufWriter;

    // Check if input is sorted.
    let header = {
        let mut br = crate::BlobReader::open(input, direct_io)?;
        match br.next() {
            Some(Ok(blob)) => match blob.decode()? {
                crate::blob::BlobDecode::OsmHeader(h) => {
                    super::super::warn_locations_on_ways_loss(&h);
                    if !h.is_sorted() {
                        return Ok(None); // fall back to sequential
                    }
                    h
                }
                _ => return Ok(None),
            },
            _ => return Ok(None),
        }
    };

    let n = slots.len();
    crate::debug::emit_marker("MULTI_EXTRACT_START");
    eprintln!("[multi-extract] single-pass: {n} regions, simple strategy");

    // Precompute per-region integer bboxes.
    let bbox_ints: Vec<BboxInt> = slots.iter()
        .map(|s| BboxInt::from_bbox(s.region.bbox()))
        .collect();

    // Union bbox for blob-level spatial skip.
    let union_bbox = BboxInt {
        min_lon: bbox_ints.iter().map(|b| b.min_lon).min().unwrap_or(i32::MIN),
        min_lat: bbox_ints.iter().map(|b| b.min_lat).min().unwrap_or(i32::MIN),
        max_lon: bbox_ints.iter().map(|b| b.max_lon).max().unwrap_or(i32::MAX),
        max_lat: bbox_ints.iter().map(|b| b.max_lat).max().unwrap_or(i32::MAX),
    };
    let spatial_filter = spatial_blob_filter(&union_bbox);

    // Open N sync-mode writers.
    let mut writers: Vec<PbfWriter<BufWriter<std::fs::File>>> = Vec::with_capacity(n);
    for slot in slots {
        let bbox = slot.region.bbox();
        let header_bytes = super::super::build_output_header(&header, true, overrides, |hb| {
            let hb = if set_bounds {
                hb.bbox(bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat)
            } else {
                hb
            };
            hb.sorted()
        })?;
        let file = BufWriter::new(
            std::fs::File::create(&slot.output)
                .map_err(|e| format!("failed to create {}: {e}", slot.output.display()))?
        );
        let mut w = PbfWriter::new(file, compression);
        w.write_header(&header_bytes)
            .map_err(|e| format!("failed to write header to {}: {e}", slot.output.display()))?;
        writers.push(w);
    }

    // Per-region ID sets and stats.
    let mut bbox_node_ids: Vec<IdSet> = (0..n).map(|_| IdSet::new()).collect();
    let mut matched_way_ids: Vec<IdSet> = (0..n).map(|_| IdSet::new()).collect();
    let mut matched_relation_ids: Vec<IdSet> = (0..n).map(|_| IdSet::new()).collect();
    let mut stats: Vec<ExtractStats> = (0..n).map(|_| ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_written: 0,
        ways_from_relations: 0,
        relations_written: 0,
        strategy: "simple",
    }).collect();

    // Build schedules by element type for parallel classification.
    // Walk via the pread-only HeaderWalker so blob bodies stay out of the
    // page cache during the scan - the per-kind classification passes open
    // fresh fds and pread only the blobs they need.
    crate::debug::emit_marker("MULTI_SCHEDULE_SCAN_START");
    let mut walker = crate::read::header_walker::HeaderWalker::open(input)?;
    let _ = walker
        .next_header()?
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))?;

    let mut node_schedule: Vec<(usize, u64, usize)> = Vec::new();
    let mut way_schedule: Vec<(usize, u64, usize)> = Vec::new();
    let mut relation_schedule: Vec<(usize, u64, usize)> = Vec::new();
    // Per-node-blob passthrough metadata.
    let mut node_blob_info: Vec<NodeBlobInfo> = Vec::new();
    let mut seq: usize = 0;
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) { continue; }
        if let Some(idx) = meta.index.as_ref() {
            if !spatial_filter.wants_index(idx) { continue; }
            match idx.kind {
                crate::blob_meta::ElemKind::Node => {
                    // Raw passthrough is only sound for bbox regions - polygon
                    // regions can exclude nodes inside the bbox but outside the
                    // polygon boundary or inside holes.
                    let mut contained_in: Vec<usize> = Vec::new();
                    if let Some(ref blob_bbox) = idx.bbox {
                        for (i, (bi, slot)) in bbox_ints.iter().zip(slots.iter()).enumerate() {
                            if matches!(slot.region, Region::Bbox(_)) {
                                let region_bbox = crate::BlobBbox::new(bi.min_lat, bi.max_lat, bi.min_lon, bi.max_lon);
                                if region_bbox.contains(blob_bbox) {
                                    contained_in.push(i);
                                }
                            }
                        }
                    }
                    node_blob_info.push(NodeBlobInfo {
                        contained_in,
                        frame_offset: meta.frame_start,
                        frame_size: meta.frame_size,
                        count: idx.count,
                    });
                    node_schedule.push((seq, meta.data_offset, meta.data_size));
                }
                crate::blob_meta::ElemKind::Way => way_schedule.push((seq, meta.data_offset, meta.data_size)),
                crate::blob_meta::ElemKind::Relation => relation_schedule.push((seq, meta.data_offset, meta.data_size)),
            }
        } else {
            // No indexdata - include in all schedules (conservative).
            node_blob_info.push(NodeBlobInfo {
                contained_in: Vec::new(),
                frame_offset: meta.frame_start,
                frame_size: 0,
                count: 0,
            });
            node_schedule.push((seq, meta.data_offset, meta.data_size));
            way_schedule.push((seq, meta.data_offset, meta.data_size));
            relation_schedule.push((seq, meta.data_offset, meta.data_size));
        }
        seq += 1;
    }
    crate::debug::emit_marker("MULTI_SCHEDULE_SCAN_END");

    // Shadow counters: node-blob raw-passthrough eligibility per region.
    // Precedent: tags-filter pass-2 shadow counters in commit a5c6854
    // (reverted in 0ef4107 after producing the go/no-go decision).
    // Rationale in notes/multi-extract-optimization.md item #5: NODE_WRITE
    // is 52% of Europe wall under the current all-N-or-nothing
    // passthrough gate. These counters quantify the some-but-not-all
    // partial-contained population that would unlock with partial
    // passthrough. Blobs with no indexdata fall into `none_contained`
    // because we can't prove containment without decoding - matching the
    // production code's conservative "include in all schedules" path.
    emit_node_passthrough_shadow_counters(&node_blob_info, n);

    let shared_file = std::sync::Arc::clone(walker.shared_file());
    drop(walker);

    // Phase 1: Parallel node classification → N bbox_node_ids.
    // For all-bbox regions, use columnar decode (batch IDs/lats/lons into
    // contiguous arrays) with single-pass multi-region classification.
    // Polygon regions fall back to element-by-element iteration.
    let all_bbox = slots.iter().all(|s| matches!(s.region, Region::Bbox(_)));
    crate::debug::emit_marker("MULTI_NODE_CLASSIFY_START");
    if all_bbox {
        let bboxes: Vec<(i32, i32, i32, i32)> = bbox_ints.iter()
            .map(|bi| (bi.min_lat, bi.max_lat, bi.min_lon, bi.max_lon))
            .collect();
        crate::scan::classify::parallel_classify_phase(
            &shared_file,
            &node_schedule,
            None,
            || (crate::read::columnar::DenseNodeColumns::new(), vec![Vec::<i64>::new(); n]),
            |block, (columns, scratch)| {
                block.decode_dense_columns(columns);
                for v in scratch.iter_mut() { v.clear(); }
                columns.collect_matching_ids_multi_bbox(&bboxes, scratch);
                scratch.iter_mut().map(std::mem::take).collect::<Vec<_>>()
            },
            |_seq, region_ids: Vec<Vec<i64>>| {
                for (i, ids) in region_ids.into_iter().enumerate() {
                    for id in ids { bbox_node_ids[i].set(id); }
                }
            },
        )?;
    } else {
        crate::scan::classify::parallel_classify_phase(
            &shared_file,
            &node_schedule,
            None,
            || vec![Vec::<i64>::new(); n],
            |block, scratch| {
                for v in scratch.iter_mut() { v.clear(); }
                for element in block.elements_skip_metadata() {
                    match &element {
                        Element::DenseNode(dn) => {
                            let lat = dn.decimicro_lat();
                            let lon = dn.decimicro_lon();
                            for i in 0..n {
                                if slots[i].region.contains_decimicro(&bbox_ints[i], lat, lon) {
                                    scratch[i].push(dn.id());
                                }
                            }
                        }
                        Element::Node(nd) => {
                            let lat = nd.decimicro_lat();
                            let lon = nd.decimicro_lon();
                            for i in 0..n {
                                if slots[i].region.contains_decimicro(&bbox_ints[i], lat, lon) {
                                    scratch[i].push(nd.id());
                                }
                            }
                        }
                        _ => {}
                    }
                }
                scratch.iter_mut().map(std::mem::take).collect::<Vec<_>>()
            },
            |_seq, region_ids: Vec<Vec<i64>>| {
                for (i, ids) in region_ids.into_iter().enumerate() {
                    for id in ids { bbox_node_ids[i].set(id); }
                }
            },
        )?;
    }
    crate::debug::emit_marker("MULTI_NODE_CLASSIFY_END");

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
                    Element::DenseNode(dn) if bbox_node_ids.iter().any(|s| s.get(dn.id())) => {
                        let id = dn.id();
                        let meta = dense_node_metadata(dn);
                        for i in 0..n {
                            if bbox_node_ids[i].get(id) {
                                ensure_node_capacity_local(&mut bbs[i], &mut output[i])?;
                                bbs[i].add_node(id, dn.decimicro_lat(), dn.decimicro_lon(), dn.tags(), meta.as_ref());
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
                                bbs[i].add_node(id, nd.decimicro_lat(), nd.decimicro_lon(), nd.tags(), meta.as_ref());
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
            for v in scratch.iter_mut() { v.clear(); }
            for element in block.elements_skip_metadata() {
                if let Element::Way(w) = &element {
                    for i in 0..n {
                        if w.refs().any(|r| bbox_node_ids[i].get(r)) {
                            scratch[i].push(w.id());
                        }
                    }
                }
            }
            scratch.iter_mut().map(std::mem::take).collect::<Vec<_>>()
        },
        |_seq, region_ids| {
            for (i, ids) in region_ids.into_iter().enumerate() {
                for id in ids { matched_way_ids[i].set(id); }
            }
        },
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
                    if !matched_way_ids.iter().any(|s| s.get(wid)) { continue; }
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
                    if !matched_relation_ids.iter().any(|s| s.get(rid)) { continue; }
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
    )?;
    crate::debug::emit_marker("MULTI_REL_WRITE_END");

    // Flush all writers (workers already flushed their BlockBuilders per blob).
    for (i, slot) in slots.iter().enumerate() {
        writers[i].flush()
            .map_err(|e| format!("failed to flush {}: {e}", slot.output.display()))?;
    }

    // Print per-region stats.
    for (i, slot) in slots.iter().enumerate() {
        let s = &stats[i];
        let total = s.nodes_in_bbox + s.ways_written + s.relations_written;
        eprintln!(
            "  [{}] {}: {} elements ({} nodes, {} ways, {} relations)",
            i + 1,
            slot.output.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
            total, s.nodes_in_bbox, s.ways_written, s.relations_written,
        );
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
        crate::debug::emit_counter("multi_extract_relation_blobs", relation_schedule.len() as i64);
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

/// Node write phase with raw passthrough for fully-contained blobs.
///
/// Blobs fully contained in ALL N regions are written as raw frames to all
/// N writers without decode. Other blobs go through parallel decode workers.
/// Both streams are interleaved in sequence order via ReorderBuffer.
#[allow(clippy::too_many_lines)]
/// Per-node-blob passthrough metadata: (contained_regions, frame_offset, frame_size, count).
struct NodeBlobInfo {
    contained_in: Vec<usize>,
    frame_offset: u64,
    frame_size: usize,
    count: u64,
}

/// Emit `multiextract_node_shadow_*` counters summarising how node blobs
/// partition across the current all-N-or-nothing passthrough gate plus
/// per-region raw-passthrough eligibility. See the call site in
/// `try_extract_multi_single_pass` for the rationale.
#[allow(clippy::cast_possible_wrap)]
fn emit_node_passthrough_shadow_counters(node_blob_info: &[NodeBlobInfo], n: usize) {
    let total_blobs = node_blob_info.len();
    let mut total_elements: u64 = 0;
    let mut blobs_all_n: i64 = 0;
    let mut elements_all_n: u64 = 0;
    let mut blobs_partial: i64 = 0;
    let mut elements_partial: u64 = 0;
    let mut blobs_none: i64 = 0;
    let mut elements_none: u64 = 0;
    let mut per_region_blobs = vec![0i64; n];
    let mut per_region_elements = vec![0u64; n];

    for info in node_blob_info {
        total_elements += info.count;
        let k = info.contained_in.len();
        if n > 0 && k == n {
            blobs_all_n += 1;
            elements_all_n += info.count;
        } else if k > 0 {
            blobs_partial += 1;
            elements_partial += info.count;
        } else {
            blobs_none += 1;
            elements_none += info.count;
        }
        for &i in &info.contained_in {
            per_region_blobs[i] += 1;
            per_region_elements[i] += info.count;
        }
    }

    crate::debug::emit_counter("multiextract_node_shadow_blobs_total", total_blobs as i64);
    crate::debug::emit_counter("multiextract_node_shadow_elements_total", total_elements as i64);
    crate::debug::emit_counter("multiextract_node_shadow_blobs_all_n_contained", blobs_all_n);
    crate::debug::emit_counter("multiextract_node_shadow_elements_all_n_contained", elements_all_n as i64);
    crate::debug::emit_counter("multiextract_node_shadow_blobs_partial_contained", blobs_partial);
    crate::debug::emit_counter("multiextract_node_shadow_elements_partial_contained", elements_partial as i64);
    crate::debug::emit_counter("multiextract_node_shadow_blobs_none_contained", blobs_none);
    crate::debug::emit_counter("multiextract_node_shadow_elements_none_contained", elements_none as i64);
    for i in 0..n {
        crate::debug::emit_counter(
            &format!("multiextract_node_shadow_region_{i}_blobs"),
            per_region_blobs[i],
        );
        crate::debug::emit_counter(
            &format!("multiextract_node_shadow_region_{i}_elements"),
            per_region_elements[i] as i64,
        );
    }
}

#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn multi_extract_pread_write_nodes<F>(
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[(usize, u64, usize)],
    blob_info: &[NodeBlobInfo],
    n: usize,
    block_fn: F,
    writers: &mut [PbfWriter<std::io::BufWriter<std::fs::File>>],
    stats: &mut [ExtractStats],
) -> Result<()>
where
    F: Fn(&PrimitiveBlock, &mut Vec<BlockBuilder>, &mut Vec<Vec<OwnedBlock>>, &mut Vec<i64>)
        -> std::result::Result<Vec<u64>, String> + Send + Sync,
{
    use std::os::unix::fs::FileExt as _;

    if schedule.is_empty() { return Ok(()); }

    // Blobs fully contained in ALL N regions skip decode entirely - write raw
    // frame to all N writers. Other blobs go through parallel decode workers.
    let mut decode_items: Vec<(usize, u64, usize)> = Vec::new();
    let mut passthrough_items: Vec<(usize, u64, usize, u64)> = Vec::new();
    for (local_seq, ((_global_seq, data_offset, data_size), info)) in
        schedule.iter().zip(blob_info.iter()).enumerate()
    {
        if info.contained_in.len() == n {
            passthrough_items.push((local_seq, info.frame_offset, info.frame_size, info.count));
        } else {
            decode_items.push((local_seq, *data_offset, *data_size));
        }
    }

    if !passthrough_items.is_empty() {
        let pt = passthrough_items.len();
        let dc = decode_items.len();
        eprintln!("  node blobs: {pt} passthrough, {dc} decoded");
    }

    // If everything is passthrough, skip the thread scope entirely.
    if decode_items.is_empty() {
        let mut frame_buf: Vec<u8> = Vec::new();
        for &(_, frame_offset, frame_size, count) in &passthrough_items {
            frame_buf.resize(frame_size, 0);
            shared_file.read_exact_at(&mut frame_buf, frame_offset)
                .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
            for i in 0..n {
                writers[i].write_raw(&frame_buf)?;
                stats[i].nodes_in_bbox += count;
            }
        }
        return Ok(());
    }

    let decode_threads = std::thread::available_parallelism()
        .map(|t| t.get().saturating_sub(2).max(1))
        .unwrap_or(4);

    type WorkerResult = crate::error::Result<(Vec<Vec<OwnedBlock>>, Vec<u64>)>;

    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<(usize, u64, usize)>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<(usize, MultiNodeCI)>(32);

    std::thread::scope(|scope| -> Result<()> {
        scope.spawn(move || {
            for item in decode_items {
                if desc_tx.send(item).is_err() { break; }
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
                        file.read_exact_at(&mut read_buf, data_offset)
                            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                        let mut buf = crate::blob::pool_get_pub(&worker_pool, data_size * 4);
                        crate::blob::decompress_blob_raw(&read_buf, &mut buf)?;
                        let block = PrimitiveBlock::from_vec_pooled_with_scratch(
                            buf, &worker_pool, &mut st_scratch, &mut gr_scratch,
                        )?;
                        for v in &mut output { v.clear(); }
                        let counts = block_fn_ref(&block, &mut bbs, &mut output, &mut i64_scratch)
                            .map_err(|e| crate::error::new_error(
                                crate::error::ErrorKind::Io(std::io::Error::other(e))
                            ))?;
                        for i in 0..n {
                            flush_local(&mut bbs[i], &mut output[i]).map_err(|e| {
                                crate::error::new_error(
                                    crate::error::ErrorKind::Io(std::io::Error::other(e))
                                )
                            })?;
                        }
                        let taken: Vec<Vec<OwnedBlock>> = output.iter_mut()
                            .map(std::mem::take)
                            .collect();
                        Ok((taken, counts))
                    })();
                    if tx.send((s, MultiNodeCI::Decoded(r))).is_err() { break; }
                }
            });
        }
        drop(desc_rx);
        drop(result_tx);

        // Pre-insert passthrough items into the reorder buffer.
        let mut reorder: crate::reorder_buffer::ReorderBuffer<MultiNodeCI> =
            crate::reorder_buffer::ReorderBuffer::with_capacity(32);
        for &(local_seq, frame_offset, frame_size, count) in &passthrough_items {
            reorder.push(local_seq, MultiNodeCI::Passthrough(frame_offset, frame_size, count));
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
                for (block_bytes, index, tagdata) in blocks {
                    writers[i].write_primitive_block_owned(block_bytes, index, tagdata.as_deref())?;
                }
            }
        }
        MultiNodeCI::Passthrough(frame_offset, frame_size, count) => {
            frame_buf.resize(frame_size, 0);
            shared_file.read_exact_at(frame_buf, frame_offset)
                .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
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
fn multi_extract_pread_write<F>(
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[(usize, u64, usize)],
    n: usize,
    block_fn: F,
    writers: &mut [PbfWriter<std::io::BufWriter<std::fs::File>>],
    stats: &mut [ExtractStats],
    stat_field: fn(&mut ExtractStats) -> &mut u64,
) -> Result<()>
where
    F: Fn(&PrimitiveBlock, &mut Vec<BlockBuilder>, &mut Vec<Vec<OwnedBlock>>, &mut Vec<i64>)
        -> std::result::Result<Vec<u64>, String> + Send + Sync,
{
    use std::os::unix::fs::FileExt as _;

    if schedule.is_empty() { return Ok(()); }

    let decode_threads = std::thread::available_parallelism()
        .map(|t| t.get().saturating_sub(2).max(1))
        .unwrap_or(4);

    // Worker result: per-region OwnedBlocks + per-region counts.
    type WorkerResult = crate::error::Result<(Vec<Vec<OwnedBlock>>, Vec<u64>)>;

    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<(usize, u64, usize)>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<(usize, WorkerResult)>(32);

    std::thread::scope(|scope| -> Result<()> {
        // Dispatcher: feed schedule items to workers with local sequence index.
        scope.spawn(move || {
            for (local_seq, &(_global_seq, data_offset, data_size)) in schedule.iter().enumerate() {
                if desc_tx.send((local_seq, data_offset, data_size)).is_err() { break; }
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
                        file.read_exact_at(&mut read_buf, data_offset)
                            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                        let mut buf = crate::blob::pool_get_pub(&worker_pool, data_size * 4);
                        crate::blob::decompress_blob_raw(&read_buf, &mut buf)?;
                        let block = PrimitiveBlock::from_vec_pooled_with_scratch(
                            buf, &worker_pool, &mut st_scratch, &mut gr_scratch,
                        )?;
                        for v in &mut output { v.clear(); }
                        let counts = block_fn_ref(&block, &mut bbs, &mut output, &mut i64_scratch)
                            .map_err(|e| crate::error::new_error(
                                crate::error::ErrorKind::Io(std::io::Error::other(e))
                            ))?;
                        // Flush remaining elements in each BlockBuilder.
                        for i in 0..n {
                            flush_local(&mut bbs[i], &mut output[i]).map_err(|e| {
                                crate::error::new_error(
                                    crate::error::ErrorKind::Io(std::io::Error::other(e))
                                )
                            })?;
                        }
                        let taken: Vec<Vec<OwnedBlock>> = output.iter_mut()
                            .map(std::mem::take)
                            .collect();
                        Ok((taken, counts))
                    })();
                    if tx.send((s, r)).is_err() { break; }
                }
            });
        }
        drop(desc_rx);
        drop(result_tx);

        // Consumer: receive N-region results in order, write to N writers.
        let mut reorder: crate::reorder_buffer::ReorderBuffer<WorkerResult> =
            crate::reorder_buffer::ReorderBuffer::with_capacity(32);

        for (s, item) in result_rx {
            reorder.push(s, item);

            while let Some(result) = reorder.pop_ready() {
                let (region_blocks, counts) = result?;
                for (i, blocks) in region_blocks.into_iter().enumerate() {
                    *stat_field(&mut stats[i]) += counts[i];
                    for (block_bytes, index, tagdata) in blocks {
                        writers[i].write_primitive_block_owned(block_bytes, index, tagdata.as_deref())?;
                    }
                }
            }
        }
        // Drain remaining.
        while let Some(result) = reorder.pop_ready() {
            let (region_blocks, counts) = result?;
            for (i, blocks) in region_blocks.into_iter().enumerate() {
                *stat_field(&mut stats[i]) += counts[i];
                for (block_bytes, index, tagdata) in blocks {
                    writers[i].write_primitive_block_owned(block_bytes, index, tagdata.as_deref())?;
                }
            }
        }
        Ok(())
    })?;

    Ok(())
}
