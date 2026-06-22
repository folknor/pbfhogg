//! Per-element overlap check used by the streaming worker pool's
//! precise-check stage. The legacy `classify_only` (which had the
//! fast-path / scan-path / parse-path branches and was driven by the
//! batch loop) is gone - the scanner owns the fast-path now and the
//! worker pool slow-path always decompresses + parses + precise-checks.

use crate::Element;
use crate::PrimitiveBlock;

use super::diff_ranges::DiffRanges;

/// Check if any element *actually in the block* has a matching ID in the diff.
/// Returns true if the block needs re-encoding, false for safe passthrough.
///
/// Sorted-merge has a per-kind cursor that only advances forward. Since
/// block elements within a kind are sorted and `DiffRanges` IDs are
/// sorted by `osm_id_cmp`, the cursor visits at most the sub-slice of
/// diff IDs spanning the block's ID range.
///
/// **Key distinction from `range_overlaps`**: a diff with only pure
/// creates (new IDs not present in the base PBF) can cause
/// `range_overlaps` to return true (the create IDs fall within the
/// blob's range), but this function returns false (no element in the
/// block has a matching diff ID). In that case the blob is passed
/// through raw, and the creates are emitted afterward by the
/// gap-create logic.
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
