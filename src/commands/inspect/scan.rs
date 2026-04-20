//! PBF scanning: index-only fast path and full-decode fallback. Produces an
//! `InspectReport` that the report/json modules later render.

use std::path::Path;

use crate::read::raw_frame::read_raw_frame;
use super::super::Result;
use super::types::{
    classify_block, is_standard_ordering, update_extended_for_element, BlockAccum, BlockInfo,
    BlockKind, HeaderMeta, InspectReport, OrderingSegment, ScanState,
};
use crate::blob::{decode_blob_to_headerblock, decompress_blob_data_into, BlobKind};
use crate::blob_meta::ElemKind;
use crate::file_reader::FileReader;
use crate::read::header_walker::HeaderWalker;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[hotpath::measure]
pub fn inspect(
    path: &Path,
    show_blocks: bool,
    show_id_ranges: bool,
    show_locations: bool,
    extended: bool,
    direct_io: bool,
) -> Result<InspectReport> {
    // Index-only fast path: skip decompression when all blobs have indexdata.
    // --locations and --extended require per-element data, so they need full decode.
    if !show_locations
        && !extended
        && let Some(report) =
            try_index_only_scan(path, show_blocks, show_id_ranges, direct_io)?
    {
        return Ok(report);
    }

    full_decode_scan(path, show_blocks, show_id_ranges, show_locations, extended, direct_io)
}

// ---------------------------------------------------------------------------
// Index-only scan: reads frame headers, skips blob data entirely
// ---------------------------------------------------------------------------

/// Attempt an index-only scan. Returns `None` if any OsmData blob lacks indexdata,
/// signalling the caller to fall back to full decode.
///
/// Uses the shared pread-only `HeaderWalker` primitive: headers are read via
/// small per-blob `pread`s and blob data payloads are skipped by advancing
/// the file offset, not by pulling bytes through a buffered reader. Avoids
/// the `BufReader::seek_relative` amplification that pulled ~40-50% of file
/// size into the page cache on cold-cache planet runs (21 s / 36 GB read
/// before the migration on a half-cached planet).
///
/// `direct_io` is intentionally ignored on the fast path - `HeaderWalker`
/// sets `posix_fadvise(POSIX_FADV_RANDOM)` on the fd, and O_DIRECT's
/// page-alignment requirements are incompatible with the tiny header
/// reads. The full-decode fallback still honours `direct_io`.
fn try_index_only_scan(
    path: &Path,
    show_blocks: bool,
    show_id_ranges: bool,
    _direct_io: bool,
) -> Result<Option<InspectReport>> {
    let meta = std::fs::metadata(path)?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    let mut walker = HeaderWalker::open(path)?;
    let mut header_meta = HeaderMeta::default();
    let mut data_buf: Vec<u8> = Vec::new();

    let mut accum = BlockAccum::new(show_blocks);
    let mut block_number = 0u32;
    let mut state = ScanState::new(show_id_ranges, false, false);
    let mut total_data_blobs = 0u64;

    while let Some(header) = walker.next_header()? {
        match header.blob_type {
            BlobKind::OsmHeader => {
                walker.pread_data(header.data_offset, header.data_size, &mut data_buf)?;
                let block = decode_blob_to_headerblock(&data_buf)?;
                header_meta = extract_header_metadata(&block);
            }
            BlobKind::OsmData => {
                total_data_blobs += 1;
                let Some(index) = header.index else {
                    return Ok(None); // fallback to full decode
                };
                block_number += 1;
                accumulate_from_index(
                    &index,
                    header.data_size,
                    header.frame_size,
                    block_number,
                    &mut state,
                    &mut accum,
                );
            }
            BlobKind::Unknown(_) => {
                // Already skipped by the walker's offset advance.
            }
        }
    }

    Ok(Some(InspectReport {
        file_name,
        file_size: meta.len(),
        header_meta,
        is_indexed: true,
        total_blocks: total_data_blobs,
        accum,
        state,
    }))
}

/// Update accumulators from a single blob's index metadata (no decompression).
fn accumulate_from_index(
    index: &crate::blob_meta::BlobIndex,
    data_size: usize,
    frame_size: usize,
    block_number: u32,
    state: &mut ScanState,
    accum: &mut BlockAccum,
) {
    let kind = BlockKind::from_elem_kind(index.kind);

    // Element counts
    match index.kind {
        ElemKind::Node => state.node_count += index.count,
        ElemKind::Way => state.way_count += index.count,
        ElemKind::Relation => state.relation_count += index.count,
    }

    // ID ranges (inter-blob monotonicity)
    let ids = match index.kind {
        ElemKind::Node => &mut state.node_ids,
        ElemKind::Way => &mut state.way_ids,
        ElemKind::Relation => &mut state.relation_ids,
    };
    if let Some(ids) = ids {
        ids.update_from_blob(index.min_id, index.max_id, index.count);
    }

    // Per-type stats
    let stats = match kind {
        BlockKind::Nodes => &mut accum.node_type,
        BlockKind::Ways => &mut accum.way_type,
        BlockKind::Relations => &mut accum.relation_type,
        BlockKind::Mixed => &mut accum.mixed_type,
    };
    stats.block_count += 1;
    stats.frame_bytes += frame_size as u64;
    stats.element_count += index.count;

    // Ordering segments
    if let Some(last) = accum.segments.last_mut().filter(|s| s.kind == kind) {
        last.last_block = block_number;
    } else {
        accum.segments.push(OrderingSegment {
            kind,
            first_block: block_number,
            last_block: block_number,
        });
    }

    // Per-block detail
    if let Some(ref mut infos) = accum.block_infos {
        infos.push(BlockInfo {
            number: block_number,
            kind,
            elements: index.count,
            compressed: data_size,
            raw: None,
        });
    }
}

/// Extract header metadata fields from a parsed `HeaderBlock`.
pub(super) fn extract_header_metadata(header: &crate::HeaderBlock) -> HeaderMeta {
    HeaderMeta {
        writing_program: header.writing_program().map(String::from),
        required_features: header
            .required_features()
            .iter()
            .map(ToString::to_string)
            .collect(),
        optional_features: header
            .optional_features()
            .iter()
            .map(ToString::to_string)
            .collect(),
        bbox: header.bbox().map(|bb| (bb.left, bb.bottom, bb.right, bb.top)),
        replication_timestamp: header.osmosis_replication_timestamp(),
        replication_sequence: header.osmosis_replication_sequence_number(),
        replication_url: header.osmosis_replication_base_url().map(String::from),
    }
}

// ---------------------------------------------------------------------------
// Full decode scan (original path)
// ---------------------------------------------------------------------------

fn full_decode_scan(
    path: &Path,
    show_blocks: bool,
    show_id_ranges: bool,
    show_locations: bool,
    extended: bool,
    direct_io: bool,
) -> Result<InspectReport> {
    let meta = std::fs::metadata(path)?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    let mut reader = FileReader::open(path, direct_io)?;
    let mut offset = 0u64;
    let mut decompress_buf = Vec::new();
    let mut header_meta = HeaderMeta::default();

    // Indexdata tracking
    let mut indexed_blobs = 0u64;
    let mut total_data_blobs = 0u64;

    let mut accum = BlockAccum::new(show_blocks);
    let mut block_number = 0u32;
    let mut state = ScanState::new(show_id_ranges, show_locations, extended);
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    while let Some(frame) = read_raw_frame(&mut reader, &mut offset)? {
        match frame.blob_type {
            BlobKind::OsmHeader => {
                let header = decode_blob_to_headerblock(frame.blob_bytes())?;
                header_meta = extract_header_metadata(&header);
            }
            BlobKind::OsmData => {
                total_data_blobs += 1;
                if frame.index.is_some() {
                    indexed_blobs += 1;
                }
                block_number += 1;
                scan_data_blob(&frame, &mut decompress_buf, &mut st_scratch, &mut gr_scratch, &mut state, block_number, &mut accum)?;
            }
            BlobKind::Unknown(_) => {}
        }
    }

    let is_indexed = total_data_blobs > 0 && indexed_blobs == total_data_blobs;

    // Compute objects_ordered from ordering segments + ID monotonicity.
    if let Some(ref mut ext) = state.extended {
        let type_ordered = is_standard_ordering(&accum.segments);
        let ids_monotonic = state
            .node_ids
            .as_ref()
            .is_none_or(|r| !r.has_data() || r.monotonic)
            && state
                .way_ids
                .as_ref()
                .is_none_or(|r| !r.has_data() || r.monotonic)
            && state
                .relation_ids
                .as_ref()
                .is_none_or(|r| !r.has_data() || r.monotonic);
        ext.objects_ordered = type_ordered && ids_monotonic;
    }

    Ok(InspectReport {
        file_name,
        file_size: meta.len(),
        header_meta,
        is_indexed,
        total_blocks: total_data_blobs,
        accum,
        state,
    })
}

/// Decompress, parse, and scan one OsmData blob. Updates all accumulators.
fn scan_data_blob(
    frame: &crate::read::raw_frame::RawBlobFrame,
    decompress_buf: &mut Vec<u8>,
    st_scratch: &mut Vec<(u32, u32)>,
    gr_scratch: &mut Vec<(u32, u32)>,
    state: &mut ScanState,
    block_number: u32,
    accum: &mut BlockAccum,
) -> Result<()> {
    let frame_size = frame.frame_bytes.len();
    let compressed_size = frame.blob_bytes().len();

    decompress_blob_data_into(frame.blob_bytes(), decompress_buf)?;
    let raw_size = decompress_buf.len();
    let block = crate::block::PrimitiveBlock::new_with_scratch(
        bytes::Bytes::copy_from_slice(decompress_buf),
        st_scratch,
        gr_scratch,
    )?;

    let mut has_nodes = false;
    let mut has_ways = false;
    let mut has_relations = false;
    let mut block_elements = 0u64;

    let need_metadata = state.extended.is_some();
    if need_metadata {
        for element in block.elements() {
            block_elements += 1;
            let (n, w, r) = state.process_element(&element);
            has_nodes |= n;
            has_ways |= w;
            has_relations |= r;
            if let Some(ref mut ext) = state.extended {
                update_extended_for_element(ext, &element);
            }
        }
    } else {
        for element in block.elements_skip_metadata() {
            block_elements += 1;
            let (n, w, r) = state.process_element(&element);
            has_nodes |= n;
            has_ways |= w;
            has_relations |= r;
        }
    }

    let kind = classify_block(has_nodes, has_ways, has_relations);

    // Update per-type stats
    let stats = match kind {
        BlockKind::Nodes => &mut accum.node_type,
        BlockKind::Ways => &mut accum.way_type,
        BlockKind::Relations => &mut accum.relation_type,
        BlockKind::Mixed => &mut accum.mixed_type,
    };
    stats.block_count += 1;
    stats.frame_bytes += frame_size as u64;
    stats.element_count += block_elements;

    // Update ordering segments
    if let Some(last) = accum.segments.last_mut().filter(|s| s.kind == kind) {
        last.last_block = block_number;
    } else {
        accum.segments.push(OrderingSegment {
            kind,
            first_block: block_number,
            last_block: block_number,
        });
    }

    // Per-block detail
    if let Some(ref mut infos) = accum.block_infos {
        infos.push(BlockInfo {
            number: block_number,
            kind,
            elements: block_elements,
            compressed: compressed_size,
            raw: Some(raw_size),
        });
    }

    Ok(())
}
