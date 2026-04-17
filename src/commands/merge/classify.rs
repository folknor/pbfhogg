//! Blob classification for single-pass merge pipeline.

use bytes::Bytes;

use crate::blob::{decompress_blob_data_into, parse_primitive_block_from_bytes_owned};
use crate::blob_index::{self, BlobIndex, ElemKind};
use crate::osc::CompactDiffOverlay;
use crate::{Element, PrimitiveBlock};

use super::diff_ranges::DiffRanges;

use crate::commands::RawBlobFrame;

/// Estimate a blob's in-flight memory cost for byte-budgeted batch sizing.
///
/// For indexed blobs whose ID range doesn't overlap the diff, returns just
/// the raw frame size (pure passthrough - no decompression needed).
/// For potential rewrite blobs, returns raw_size × 21 (raw + ~16× decompressed
/// + ~5× rewrite output estimate).
pub(super) fn estimate_blob_cost(frame: &RawBlobFrame, ranges: &DiffRanges) -> usize {
    let raw = frame.frame_bytes.len();
    if let Some(ref idx) = frame.index
        && !ranges.range_overlaps(idx.kind, idx.min_id, idx.max_id)
    {
        return raw;
    }
    raw * 21
}

/// Check if any element *actually in the block* has a matching ID in the diff.
/// Returns true if the block needs re-encoding, false for safe passthrough.
///
/// This is the secondary, precise overlap check. It runs after `classify_blob`
/// returned `MayOverlap` (the coarse range check found diff IDs within the
/// blob's [min_id, max_id]). This function iterates actual element IDs in the
/// parsed block and checks them against the diff's HashMap/HashSet.
///
/// **Key distinction from `range_overlaps`**: a diff with only pure creates
/// (new IDs not present in the base PBF) can cause `range_overlaps` to return
/// true (the create IDs fall within the blob's range), but this function
/// returns false (no element in the block has a matching diff ID). In that
/// case the blob is passed through raw, and the creates are emitted afterward
/// by the gap-create logic, which means they may appear out of strict ID order
/// relative to the passthrough block. This is intentional - rewriting an
/// otherwise unaffected block just to interleave pure creates would be wasted
/// work. OSM consumers handle non-strictly-sorted IDs across block boundaries.
pub(super) fn block_overlaps_diff(block: &PrimitiveBlock, diff: &CompactDiffOverlay) -> bool {
    for element in block.elements_skip_metadata() {
        let dominated = match &element {
            Element::DenseNode(dn) => {
                let id = dn.id();
                diff.deleted_nodes.contains(&id) || diff.has_node(id)
            }
            Element::Node(n) => {
                let id = n.id();
                diff.deleted_nodes.contains(&id) || diff.has_node(id)
            }
            Element::Way(w) => {
                let id = w.id();
                diff.deleted_ways.contains(&id) || diff.has_way(id)
            }
            Element::Relation(r) => {
                let id = r.id();
                diff.deleted_relations.contains(&id) || diff.has_relation(id)
            }
        };
        if dominated {
            return true;
        }
    }
    false
}

pub(super) fn element_kind(element: &Element<'_>) -> ElemKind {
    match element {
        Element::DenseNode(_) | Element::Node(_) => ElemKind::Node,
        Element::Way(_) => ElemKind::Way,
        Element::Relation(_) => ElemKind::Relation,
    }
}

/// Classification result from Phase 1 parallel classify.
pub(super) enum ClassifyResult {
    /// No diff overlap - raw passthrough.
    Passthrough(BlobIndex, bool),
    /// Range overlapped but no element affected - raw passthrough.
    FalsePositive(BlobIndex, bool),
    /// At least one element affected - needs rewrite.
    NeedsRewrite(PrimitiveBlock, BlobIndex),
}

/// Per-blob slot in the batch pipeline.
pub(super) enum BatchSlot {
    Passthrough { index: BlobIndex, has_indexdata: bool },
    FalsePositive { index: BlobIndex, has_indexdata: bool },
    Rewrite { job_index: usize, index: BlobIndex },
}

/// A rewrite job for Phase 3 parallel processing.
pub(super) struct RewriteJob {
    pub(super) block: PrimitiveBlock,
    pub(super) kind: ElemKind,
    pub(super) upsert_range: (usize, usize),
}

/// Fallback BlobIndex when scan_block_ids didn't produce one (shouldn't
/// happen in practice, but handles edge cases like empty blocks).
fn fallback_index(block: &PrimitiveBlock) -> BlobIndex {
    match block.elements().next() {
        Some(ref elem) => BlobIndex {
            kind: element_kind(elem),
            min_id: 0,
            max_id: 0,
            count: 0,
            bbox: None,
        },
        None => BlobIndex {
            kind: ElemKind::Node,
            min_id: 0,
            max_id: 0,
            count: 0,
            bbox: None,
        },
    }
}

/// Classify a blob for single-pass merge. Returns whether the blob can be
/// passed through raw or needs rewriting.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn classify_only(
    frame: &RawBlobFrame,
    ranges: &DiffRanges,
    diff: &CompactDiffOverlay,
    buf: &mut Vec<u8>,
) -> std::result::Result<ClassifyResult, String> {
    let has_indexdata = frame.index.is_some();

    // Fast path: use inline index from BlobHeader indexdata (no decompression).
    if let Some(ref idx) = frame.index
        && !ranges.range_overlaps(idx.kind, idx.min_id, idx.max_id)
    {
        return Ok(ClassifyResult::Passthrough(*idx, has_indexdata));
    }

    // Slow path: decompress + lightweight scan.
    decompress_blob_data_into(frame.blob_bytes(), buf).map_err(|e| e.to_string())?;

    let scan = if let Some(scan) = blob_index::scan_block_ids(buf) {
        if !ranges.range_overlaps(scan.kind, scan.min_id, scan.max_id) {
            return Ok(ClassifyResult::Passthrough(scan, has_indexdata));
        }
        Some(scan)
    } else {
        None
    };

    // Range overlaps - full parse + precise check.
    let raw = std::mem::take(buf);
    let block =
        parse_primitive_block_from_bytes_owned(&Bytes::from(raw)).map_err(|e| e.to_string())?;

    let index = scan.unwrap_or_else(|| fallback_index(&block));

    if !block_overlaps_diff(&block, diff) {
        return Ok(ClassifyResult::FalsePositive(index, has_indexdata));
    }

    Ok(ClassifyResult::NeedsRewrite(block, index))
}
