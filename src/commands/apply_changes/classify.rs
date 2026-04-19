//! Blob classification for single-pass merge pipeline.

use bytes::Bytes;

use crate::blob::{decompress_blob_data_into, parse_primitive_block_from_bytes_owned};
use crate::blob_meta::{self, BlobIndex, ElemKind};
use crate::{Element, PrimitiveBlock};

use super::diff_ranges::DiffRanges;
use super::stats::ClassifyCounters;

use crate::read::raw_frame::RawBlobFrame;

use std::sync::atomic::Ordering;

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
/// blob's [min_id, max_id]). This function walks actual element IDs in the
/// parsed block and sorted-merges against the `DiffRanges` sorted ID vectors
/// (which already combine deletes + upserts per kind).
///
/// Sorted-merge has a per-kind cursor that only advances forward. Since block
/// elements within a kind are sorted and `DiffRanges` IDs are sorted by
/// `osm_id_cmp`, the cursor visits at most the sub-slice of diff IDs spanning
/// the block's ID range. At planet with ~8 000 elements/blob this collapses
/// ~16 000 hash lookups (measured 614 us/blob via `FxHashSet::contains`) to
/// a handful of tuple compares per element.
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
pub(super) fn block_overlaps_diff(block: &PrimitiveBlock, ranges: &DiffRanges) -> bool {
    use std::cmp::Ordering;

    let mut node_cursor: usize = 0;
    let mut way_cursor: usize = 0;
    let mut rel_cursor: usize = 0;

    for element in block.elements_skip_metadata() {
        let (cursor, sorted, id) = match &element {
            Element::DenseNode(dn) => (&mut node_cursor, &ranges.node_ids[..], dn.id()),
            Element::Node(n) => (&mut node_cursor, &ranges.node_ids[..], n.id()),
            Element::Way(w) => (&mut way_cursor, &ranges.way_ids[..], w.id()),
            Element::Relation(r) => (&mut rel_cursor, &ranges.rel_ids[..], r.id()),
        };
        while *cursor < sorted.len()
            && crate::osm_id::osm_id_cmp(sorted[*cursor], id) == Ordering::Less
        {
            *cursor += 1;
        }
        if *cursor < sorted.len() && sorted[*cursor] == id {
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
    buf: &mut Vec<u8>,
    counters: &ClassifyCounters,
) -> std::result::Result<ClassifyResult, String> {
    let has_indexdata = frame.index.is_some();

    // Fast path: use inline index from BlobHeader indexdata (no decompression).
    if let Some(ref idx) = frame.index
        && !ranges.range_overlaps(idx.kind, idx.min_id, idx.max_id)
    {
        counters.blobs_fastpath.fetch_add(1, Ordering::Relaxed);
        return Ok(ClassifyResult::Passthrough(*idx, has_indexdata));
    }

    // Slow path: decompress.
    let t_decompress = std::time::Instant::now();
    decompress_blob_data_into(frame.blob_bytes(), buf).map_err(|e| e.to_string())?;
    counters.decompress_ns.fetch_add(
        u64::try_from(t_decompress.elapsed().as_nanos()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );

    // Lightweight scan: only useful when indexdata is absent. When indexdata
    // is present and said "overlap" (we wouldn't be here otherwise), the
    // scan's tighter range would still overlap - measured 0 `scan_pass`
    // hits at planet. Skip the call entirely.
    let scan = if frame.index.is_none() {
        let t_scan = std::time::Instant::now();
        let scan = blob_meta::scan_block_ids(buf);
        counters.scan_ns.fetch_add(
            u64::try_from(t_scan.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        if let Some(ref s) = scan
            && !ranges.range_overlaps(s.kind, s.min_id, s.max_id)
        {
            counters.blobs_scan_pass.fetch_add(1, Ordering::Relaxed);
            return Ok(ClassifyResult::Passthrough(*s, has_indexdata));
        }
        scan
    } else {
        None
    };

    // Range overlaps - full parse + precise check.
    let t_parse = std::time::Instant::now();
    let raw = std::mem::take(buf);
    let block =
        parse_primitive_block_from_bytes_owned(&Bytes::from(raw)).map_err(|e| e.to_string())?;
    counters.parse_ns.fetch_add(
        u64::try_from(t_parse.elapsed().as_nanos()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );

    // Prefer scan (tighter) when we have it; fall back to frame indexdata or
    // a fabricated index from the parsed block.
    let index = scan
        .or(frame.index)
        .unwrap_or_else(|| fallback_index(&block));

    let t_precise = std::time::Instant::now();
    let overlaps = block_overlaps_diff(&block, ranges);
    counters.precise_ns.fetch_add(
        u64::try_from(t_precise.elapsed().as_nanos()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );

    if !overlaps {
        counters.blobs_false_positive.fetch_add(1, Ordering::Relaxed);
        return Ok(ClassifyResult::FalsePositive(index, has_indexdata));
    }

    counters.blobs_rewrite.fetch_add(1, Ordering::Relaxed);
    Ok(ClassifyResult::NeedsRewrite(block, index))
}
