//! Simple extraction strategy: single-pass for sorted inputs, two-pass fallback for unsorted.

use std::path::Path;

use rayon::prelude::*;

use crate::block_builder::{BlockBuilder, OwnedBlock};
use crate::cat::CleanAttrs;
use crate::writer::{Compression, PbfWriter};
use crate::{BlockType, Element, PrimitiveBlock};

use super::super::{Result, BATCH_SIZE,
    drain_batch_results, flush_local, writer_from_header, HeaderOverrides,
};
use super::super::id_set_dense::IdSetDense;

use super::common::{
    BboxInt, BlobDesc, ExtractPass2IdSets, build_blob_schedule_with_passthrough,
    extract_block_pass2, merge_extract_stats, pread_execute, relation_has_matched_member,
    spatial_blob_filter,
};
use super::{ExtractStats, Region};

/// Classify elements in a single block for simple extract (populate ID sets).
///
/// Iterates elements without metadata (faster) and marks matching IDs:
/// - Nodes: bbox containment → set `bbox_node_ids`
/// - Ways: any ref in `bbox_node_ids` → set `matched_way_ids`
/// - Relations: matched node/way member → set `matched_relation_ids`
///
/// Returns `true` if any element in the block matched (the block should be
/// included in the write batch). Returns `false` if the block is empty for
/// this extract - callers can skip it to avoid parsing elements with full
/// metadata in the write path.
///
/// Uses `block_type()` (1 byte per group) to branch by type phase,
/// eliminating dead match arms in the hot inner loop for sorted PBFs.
#[hotpath::measure]
fn classify_block_simple(
    block: &PrimitiveBlock,
    region: &Region,
    bbox_int: &BboxInt,
    bbox_node_ids: &mut IdSetDense,
    matched_way_ids: &mut IdSetDense,
    matched_relation_ids: &mut IdSetDense,
) -> bool {
    let mut matched = false;
    match block.block_type() {
        BlockType::DenseNodes | BlockType::Nodes => {
            for element in block.elements_skip_metadata() {
                match &element {
                    Element::DenseNode(dn)
                        if region.contains_decimicro(bbox_int, dn.decimicro_lat(), dn.decimicro_lon()) =>
                    {
                        bbox_node_ids.set(dn.id());
                        matched = true;
                    }
                    Element::Node(n)
                        if region.contains_decimicro(bbox_int, n.decimicro_lat(), n.decimicro_lon()) =>
                    {
                        bbox_node_ids.set(n.id());
                        matched = true;
                    }
                    _ => {}
                }
            }
        }
        BlockType::Ways => {
            for element in block.elements_skip_metadata() {
                if let Element::Way(w) = &element
                    && w.refs().any(|r| bbox_node_ids.get(r))
                {
                    matched_way_ids.set(w.id());
                    matched = true;
                }
            }
        }
        BlockType::Relations => {
            for element in block.elements_skip_metadata() {
                if let Element::Relation(r) = &element
                    && relation_has_matched_member(r, bbox_node_ids, matched_way_ids)
                {
                    matched_relation_ids.set(r.id());
                    matched = true;
                }
            }
        }
        BlockType::Empty => {
            // Empty blocks have no elements - skip.
        }
        BlockType::Mixed => {
            // Fallback for mixed blocks - check all element types.
            for element in block.elements_skip_metadata() {
                match &element {
                    Element::DenseNode(dn)
                        if region.contains_decimicro(bbox_int, dn.decimicro_lat(), dn.decimicro_lon()) =>
                    {
                        bbox_node_ids.set(dn.id());
                        matched = true;
                    }
                    Element::Node(n)
                        if region.contains_decimicro(bbox_int, n.decimicro_lat(), n.decimicro_lon()) =>
                    {
                        bbox_node_ids.set(n.id());
                        matched = true;
                    }
                    Element::Way(w) if w.refs().any(|r| bbox_node_ids.get(r)) => {
                        matched_way_ids.set(w.id());
                        matched = true;
                    }
                    Element::Relation(r)
                        if relation_has_matched_member(r, bbox_node_ids, matched_way_ids) =>
                    {
                        matched_relation_ids.set(r.id());
                        matched = true;
                    }
                    _ => {}
                }
            }
        }
    }
    matched
}

#[allow(clippy::too_many_arguments)]
pub(super) fn extract_simple(input: &Path, output: &Path, region: &Region, set_bounds: bool, clean: &CleanAttrs, compression: Compression, direct_io: bool, overrides: &HeaderOverrides) -> Result<ExtractStats> {
    // Check if input is sorted - if so, classify + write in a single file pass.
    // We need a quick header check without keeping the reader open. Use BlobReader
    // to read just the first blob (header).
    let is_sorted = {
        let mut br = crate::BlobReader::open(input, direct_io)?;
        match br.next() {
            Some(Ok(blob)) => match blob.decode()? {
                crate::blob::BlobDecode::OsmHeader(h) => {
                    super::super::warn_locations_on_ways_loss(&h);
                    h.is_sorted()
                }
                _ => false,
            },
            _ => false,
        }
    };

    if is_sorted {
        return extract_simple_single_pass(input, output, region, set_bounds, clean, compression, direct_io, overrides);
    }

    // --- Unsorted fallback: two passes (collect IDs, then write) ---
    crate::debug::emit_marker("SIMPLE_UNSORTED_PASS1_START");
    let mut stats = ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_written: 0,
        ways_from_relations: 0,
        relations_written: 0,
        strategy: "simple",
    };

    let mut bbox_node_ids = IdSetDense::new();
    let mut matched_way_ids = IdSetDense::new();
    let mut matched_relation_ids = IdSetDense::new();

    let bbox_int = BboxInt::from_bbox(region.bbox());
    let spatial_filter = spatial_blob_filter(&bbox_int);

    let mut blob_reader = crate::blob::BlobReader::open(input, direct_io)?;
    blob_reader.set_parse_indexdata(true);
    blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let mut decompress_buf: Vec<u8> = Vec::new();

    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) { continue; }
        if let Some(idx) = blob.index() {
            if !spatial_filter.wants_index(&idx) { continue; }
        }
        blob.decompress_into(&mut decompress_buf)?;
        let block = PrimitiveBlock::from_vec(std::mem::take(&mut decompress_buf))?;
        classify_block_simple(
            &block, region, &bbox_int,
            &mut bbox_node_ids, &mut matched_way_ids, &mut matched_relation_ids,
        );
    }
    crate::debug::emit_marker("SIMPLE_UNSORTED_PASS1_END");

    crate::debug::emit_marker("SIMPLE_UNSORTED_PASS2_START");
    let all_way_node_ids = IdSetDense::new();

    let mut blob_reader = crate::blob::BlobReader::open(input, direct_io)?;
    blob_reader.set_parse_indexdata(true);
    let header_blob = blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let header = header_blob.to_headerblock()?;
    let bbox = region.bbox();
    let mut writer = writer_from_header(output, compression, &header, false, overrides, |hb| {
        let hb = if set_bounds {
            hb.bbox(bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat)
        } else {
            hb
        };
        hb.sorted()
    }, direct_io, false)?;

    let ids = ExtractPass2IdSets {
        bbox_node_ids: &bbox_node_ids,
        all_way_node_ids: &all_way_node_ids,
        matched_way_ids: &matched_way_ids,
        matched_relation_ids: &matched_relation_ids,
    };

    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);
    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) { continue; }
        blob.decompress_into(&mut decompress_buf)?;
        let block = PrimitiveBlock::from_vec(std::mem::take(&mut decompress_buf))?;
        batch.push(block);
        if batch.len() >= BATCH_SIZE {
            process_extract_pass2_batch(&batch, &ids, clean, &mut writer, &mut stats)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        process_extract_pass2_batch(&batch, &ids, clean, &mut writer, &mut stats)?;
    }

    writer.flush()?;
    crate::debug::emit_marker("SIMPLE_UNSORTED_PASS2_END");
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
    input: &Path,
    output: &Path,
    region: &Region,
    set_bounds: bool,
    clean: &CleanAttrs,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<ExtractStats> {
    crate::debug::emit_marker("EXTRACT_SCAN_START");
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
        Some(crate::BlobBbox::new(
            bbox_int.min_lat, bbox_int.max_lat, bbox_int.min_lon, bbox_int.max_lon,
        ))
    } else {
        None
    };
    let full_schedule = build_blob_schedule_with_passthrough(input, passthrough_bbox.as_ref())?;
    let node_schedule: Vec<&BlobDesc> = full_schedule.iter()
        .filter(|d| {
            match d.kind {
                Some(crate::blob_index::ElemKind::Node) => {
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
    let way_schedule: Vec<&BlobDesc> = full_schedule.iter()
        .filter(|d| matches!(d.kind, Some(crate::blob_index::ElemKind::Way) | None))
        .collect();
    let relation_schedule: Vec<&BlobDesc> = full_schedule.iter()
        .filter(|d| matches!(d.kind, Some(crate::blob_index::ElemKind::Relation) | None))
        .collect();

    // Open writer.
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

    let mut bbox_node_ids = IdSetDense::new();
    let mut matched_way_ids = IdSetDense::new();
    let empty_relation_ids = IdSetDense::new(); // placeholder for node/way phases
    let all_way_node_ids = IdSetDense::new();

    // --- Phase 1: Classify nodes (parallel pread + scanner) ---
    // Workers pread node blobs, decompress, scan with node-only scanner,
    // check bbox (pure function), send matching IDs to consumer.
    // Consumer merges into bbox_node_ids. No shared mutable state in workers.
    crate::debug::emit_marker("SIMPLE_NODE_CLASSIFY_START");
    {
        use std::os::unix::fs::FileExt as _;

        // node_schedule already filtered to node blobs.

        let classify_file = std::sync::Arc::new(
            std::fs::File::open(input)
                .map_err(|e| format!("failed to open {}: {e}", input.display()))?
        );

        let decode_threads = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(2).max(1))
            .unwrap_or(4);

        type ClassifyResult = (usize, crate::error::Result<Vec<i64>>);
        let (cls_tx, cls_rx) = std::sync::mpsc::sync_channel::<(usize, u64, usize)>(16);
        let cls_rx = std::sync::Arc::new(std::sync::Mutex::new(cls_rx));
        let (ids_tx, ids_rx) = std::sync::mpsc::sync_channel::<ClassifyResult>(32);

        std::thread::scope(|scope| -> Result<()> {
            // Dispatcher: send node blob descriptors.
            let descs: Vec<(usize, u64, usize)> = node_schedule.iter()
                .enumerate()
                .map(|(i, d)| (i, d.offset, d.size))
                .collect();
            scope.spawn(move || {
                for item in descs {
                    if cls_tx.send(item).is_err() { break; }
                }
            });

            // Workers: pread → decompress → node scanner → bbox check → Vec<i64>.
            let region_ref = region;
            let bbox_int_ref = &bbox_int;
            for _ in 0..decode_threads {
                let rx = std::sync::Arc::clone(&cls_rx);
                let tx = ids_tx.clone();
                let file = std::sync::Arc::clone(&classify_file);
                scope.spawn(move || {
                    let mut read_buf: Vec<u8> = Vec::new();
                    let mut decompress_buf: Vec<u8> = Vec::new();
                    let mut tuples: Vec<super::super::node_scanner::NodeTuple> = Vec::new();
                    let mut group_starts: Vec<(usize, usize)> = Vec::new();

                    loop {
                        let (seq, data_offset, data_size) = {
                            let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            match guard.recv() {
                                Ok(d) => d,
                                Err(_) => break,
                            }
                        };

                        let r: crate::error::Result<Vec<i64>> = (|| {
                            read_buf.resize(data_size, 0);
                            file.read_exact_at(&mut read_buf, data_offset)
                                .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                            crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)?;
                            tuples.clear();
                            super::super::node_scanner::extract_node_tuples(&decompress_buf, &mut tuples, &mut group_starts)
                                .map_err(|e| crate::error::new_error(
                                    crate::error::ErrorKind::Io(std::io::Error::other(e.to_string()))
                                ))?;
                            let matching: Vec<i64> = tuples.iter()
                                .filter(|t| region_ref.contains_decimicro(bbox_int_ref, t.lat, t.lon))
                                .map(|t| t.id)
                                .collect();
                            Ok(matching)
                        })();
                        if tx.send((seq, r)).is_err() { break; }
                    }
                });
            }
            drop(cls_rx);
            drop(ids_tx);

            // Consumer: merge matching IDs into bbox_node_ids.
            for (_seq, result) in ids_rx {
                let matching_ids = result?;
                for id in matching_ids {
                    bbox_node_ids.set(id);
                }
            }
            Ok(())
        })?;
    }
    crate::debug::emit_marker("SIMPLE_NODE_CLASSIFY_END");
    // bbox_node_ids frozen. Write matching nodes via pread-from-workers.
    crate::debug::emit_marker("SIMPLE_NODE_WRITE_START");
    let node_descs: Vec<BlobDesc> = node_schedule.iter().map(|d| **d).collect();
    {
        let ids = ExtractPass2IdSets {
            bbox_node_ids: &bbox_node_ids,
            all_way_node_ids: &all_way_node_ids,
            matched_way_ids: &matched_way_ids,
            matched_relation_ids: &empty_relation_ids,
        };
        pread_execute(input, &node_descs, &mut writer, &mut stats, |block, bb, output| {
            let s = extract_block_pass2(block, &ids, clean, bb, output)?;
            flush_local(bb, output)?;
            Ok(s)
        })?;
    }

    crate::debug::emit_marker("SIMPLE_NODE_WRITE_END");
    // --- Phase 2: Classify ways (scanner) + write ways (pread-from-workers) ---
    crate::debug::emit_marker("SIMPLE_WAY_CLASSIFY_START");
    {
        use std::os::unix::fs::FileExt as _;

        let classify_file = std::sync::Arc::new(
            std::fs::File::open(input)
                .map_err(|e| format!("failed to open {}: {e}", input.display()))?
        );
        let decode_threads = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(2).max(1))
            .unwrap_or(4);

        type WayClassifyResult = (usize, crate::error::Result<Vec<i64>>);
        let (cls_tx, cls_rx) = std::sync::mpsc::sync_channel::<(usize, u64, usize)>(16);
        let cls_rx = std::sync::Arc::new(std::sync::Mutex::new(cls_rx));
        let (ids_tx, ids_rx) = std::sync::mpsc::sync_channel::<WayClassifyResult>(32);

        std::thread::scope(|scope| -> Result<()> {
            let descs: Vec<(usize, u64, usize)> = way_schedule.iter()
                .enumerate()
                .map(|(i, d)| (i, d.offset, d.size))
                .collect();
            scope.spawn(move || {
                for item in descs {
                    if cls_tx.send(item).is_err() { break; }
                }
            });

            // Workers: pread → decompress → way-ref scanner → check bbox_node_ids → Vec<i64>.
            let bbox_ids_ref = &bbox_node_ids;
            for _ in 0..decode_threads {
                let rx = std::sync::Arc::clone(&cls_rx);
                let tx = ids_tx.clone();
                let file = std::sync::Arc::clone(&classify_file);
                scope.spawn(move || {
                    let mut read_buf: Vec<u8> = Vec::new();
                    let mut decompress_buf: Vec<u8> = Vec::new();
                    let mut refs_buf: Vec<i64> = Vec::new();
                    let mut group_starts: Vec<(usize, usize)> = Vec::new();

                    loop {
                        let (seq, data_offset, data_size) = {
                            let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            match guard.recv() {
                                Ok(d) => d,
                                Err(_) => break,
                            }
                        };

                        let r: crate::error::Result<Vec<i64>> = (|| {
                            read_buf.resize(data_size, 0);
                            file.read_exact_at(&mut read_buf, data_offset)
                                .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                            crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)?;
                            let mut matching: Vec<i64> = Vec::new();
                            super::super::way_scanner::scan_way_refs(&decompress_buf, &mut refs_buf, &mut group_starts, |way_id, refs| {
                                if refs.iter().any(|&r| bbox_ids_ref.get(r)) {
                                    matching.push(way_id);
                                }
                            }).map_err(|e| crate::error::new_error(
                                crate::error::ErrorKind::Io(std::io::Error::other(e.to_string()))
                            ))?;
                            Ok(matching)
                        })();
                        if tx.send((seq, r)).is_err() { break; }
                    }
                });
            }
            drop(cls_rx);
            drop(ids_tx);

            for (_seq, result) in ids_rx {
                let matching_ids = result?;
                for id in matching_ids {
                    matched_way_ids.set(id);
                }
            }
            Ok(())
        })?;
    }
    crate::debug::emit_marker("SIMPLE_WAY_CLASSIFY_END");
    // matched_way_ids frozen. Write matching ways via pread-from-workers.
    crate::debug::emit_marker("SIMPLE_WAY_WRITE_START");
    let way_descs: Vec<BlobDesc> = way_schedule.iter()
        .map(|d| BlobDesc { raw_passthrough: false, ..**d })
        .collect();
    {
        let ids = ExtractPass2IdSets {
            bbox_node_ids: &bbox_node_ids,
            all_way_node_ids: &all_way_node_ids,
            matched_way_ids: &matched_way_ids,
            matched_relation_ids: &empty_relation_ids,
        };
        pread_execute(input, &way_descs, &mut writer, &mut stats, |block, bb, output| {
            let s = extract_block_pass2(block, &ids, clean, bb, output)?;
            flush_local(bb, output)?;
            Ok(s)
        })?;
    }

    crate::debug::emit_marker("SIMPLE_WAY_WRITE_END");
    // --- Phase 3: Classify relations + write (pread-from-workers) ---
    crate::debug::emit_marker("SIMPLE_REL_CLASSIFY_START");
    let mut matched_relation_ids = IdSetDense::new();
    {
        let (rel_classify_schedule, rel_classify_file) = super::super::build_classify_schedule(
            input, Some(crate::blob_index::ElemKind::Relation),
        )?;
        super::super::parallel_classify_accumulate(
            &rel_classify_file,
            &rel_classify_schedule,
            IdSetDense::new,
            |block, ids| {
                for element in block.elements_skip_metadata() {
                    if let Element::Relation(r) = &element {
                        if relation_has_matched_member(r, &bbox_node_ids, &matched_way_ids) {
                            ids.set(r.id());
                        }
                    }
                }
            },
            |worker_ids| {
                matched_relation_ids.merge(worker_ids);
            },
        )?;
    }
    crate::debug::emit_marker("SIMPLE_REL_CLASSIFY_END");
    crate::debug::emit_marker("SIMPLE_REL_WRITE_START");
    let rel_descs: Vec<BlobDesc> = relation_schedule.iter()
        .map(|d| BlobDesc { raw_passthrough: false, ..**d })
        .collect();
    {
        let ids = ExtractPass2IdSets {
            bbox_node_ids: &bbox_node_ids,
            all_way_node_ids: &all_way_node_ids,
            matched_way_ids: &matched_way_ids,
            matched_relation_ids: &matched_relation_ids,
        };
        pread_execute(input, &rel_descs, &mut writer, &mut stats, |block, bb, output| {
            let s = extract_block_pass2(block, &ids, clean, bb, output)?;
            flush_local(bb, output)?;
            Ok(s)
        })?;
    }

    crate::debug::emit_marker("SIMPLE_REL_WRITE_END");
    writer.flush()?;
    crate::debug::emit_marker("EXTRACT_SCAN_END");
    Ok(stats)
}

/// Process a batch of blocks in parallel for Pass 2 of complete-ways extraction.
fn process_extract_pass2_batch(
    batch: &[PrimitiveBlock],
    ids: &ExtractPass2IdSets<'_>,
    clean: &CleanAttrs,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut ExtractStats,
) -> Result<()> {
    type BatchResult = std::result::Result<(Vec<OwnedBlock>, ExtractStats), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            BlockBuilder::new,
            |bb, block| {
                let mut output: Vec<OwnedBlock> = Vec::new();
                let block_stats = extract_block_pass2(block, ids, clean, bb, &mut output)?;
                flush_local(bb, &mut output)?;
                Ok((output, block_stats))
            },
        )
        .collect();

    drain_batch_results(results, writer, |s| merge_extract_stats(stats, &s))?;
    Ok(())
}
