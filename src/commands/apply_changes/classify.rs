//! Per-element overlap check used by the streaming worker pool's
//! precise-check stage, and the walk guard every decoded candidate's element
//! loops feed. The legacy `classify_only` (which had the fast-path /
//! scan-path / parse-path branches and was driven by the batch loop) is
//! gone - the scanner owns the fast-path now and the worker pool slow-path
//! always decompresses + parses + precise-checks.

use crate::Element;
use crate::PrimitiveBlock;
use crate::blob_meta::{BlobIndex, IdTally};
use crate::error::{Error, Result};
use crate::osm_id::{OsmIdKey, osm_id_key};

use super::diff_ranges::DiffRanges;

/// What one walk of a decoded candidate's elements has seen, checked
/// against the blob's claim and the header's order claim as it goes.
///
/// Every decoded candidate ends in one complete walk before its item leaves
/// the worker: a precise-check miss walks every element, and a hit, a forced
/// rewrite and a refresh-only rewrite go through the rewrite loop, which
/// does; a way blob the refresh scan finds clean has had every way walked.
/// Each loop hands each element's ID to [`observe`](Self::observe) (one
/// key compare against the previous ID, an increment and a branchless min
/// and max), and the complete one ends in [`finish`](Self::finish).
///
/// - An ID below the previous one in OSM order contradicts the header's
///   `Sort.Type_then_ID`: `Unsorted`. The rewrite interleaves inline creates
///   and the precise check advances its diff cursors on that order, so a
///   blob `[5, 3]` would write a modified 3 twice, or pass a deleted 3
///   through.
/// - A claim (indexdata) that is not exact for the walk is
///   `IndexdataMismatch`, also when the walk breaks order on an ID the claim
///   does not cover. The inline-upsert slice, the drain's gap creates and
///   cursor, and the output order guard all place the blob by its claim, so
///   an understated range duplicates a modify or writes a create out of
///   order, and an overstated one hands a neighbour's upsert to this blob as
///   well.
///
/// An unindexed blob (`--force`) makes no claim and is checked for order
/// only. The claimed kind is checked separately, once per blob
/// (`PrimitiveBlock::refuse_foreign_kind`, or the refresh scan's group
/// fields), so every ID observed is of one kind.
pub(super) struct BlobWalk<'c> {
    claim: Option<&'c BlobIndex>,
    tally: IdTally,
    /// The first ID observed, valid once `tally` has counted one.
    first: i64,
    previous: OsmIdKey,
}

impl<'c> BlobWalk<'c> {
    /// A walk of a blob carrying `claim` (`None` for an unindexed blob).
    pub(super) fn new(claim: Option<&'c BlobIndex>) -> Self {
        Self {
            claim,
            tally: IdTally::default(),
            first: 0,
            previous: osm_id_key(0),
        }
    }

    /// The next element's ID, in iteration order.
    #[inline]
    pub(super) fn observe(&mut self, id: i64) -> Result<()> {
        let key = osm_id_key(id);
        // Key 0 is the least key, so the first element always passes.
        if key < self.previous {
            return Err(self.disorder(id));
        }
        if self.tally.count() == 0 {
            self.first = id;
        }
        self.previous = key;
        self.tally.add(id);
        Ok(())
    }

    /// The blob's first element in iteration order, once one has been
    /// observed.
    pub(super) fn first(&self) -> Option<i64> {
        (self.tally.count() != 0).then_some(self.first)
    }

    /// End a complete walk: the claim, when there is one, must be exact.
    /// Returns the first element's ID, `None` for an unindexed blob with no
    /// element (an indexed one is refused).
    pub(super) fn finish(&self) -> Result<Option<i64>> {
        if let Some(claim) = self.claim {
            self.tally.refuse_inexact(claim)?;
        }
        Ok(self.first())
    }

    #[cold]
    fn disorder(&self, id: i64) -> Error {
        if let Some(claim) = self.claim {
            let mut seen = self.tally;
            seen.add(id);
            if seen.escapes(claim) {
                return seen.mismatch(claim, false);
            }
        }
        crate::error::new_unsorted(format!(
            "apply-changes: refusing to write an unsorted PBF under a Sort.Type_then_ID \
             header - a base blob holds id {id} after id {}, out of the order its \
             header declares",
            crate::osm_id::osm_id_from_key(self.previous)
        ))
    }
}

/// Check if any element *actually in the block* has a matching ID in the diff.
/// Returns true if the block needs re-encoding, false for safe passthrough.
///
/// Every element visited is observed by `walk`, so a block that breaks
/// order is refused here rather than misjudged; on a `false` return every
/// element has been visited and the caller ends the walk. On `true` the walk
/// stops at the hit, and the rewrite that follows walks the block whole.
///
/// Sorted-merge has a per-kind cursor that only advances forward. Since
/// block elements within a kind are sorted (`walk` refuses a block that is
/// not) and `DiffRanges` IDs are sorted by `osm_id_cmp`, each cursor starts
/// by binary search at the first diff ID not before the kind's first
/// element, and then visits at most the sub-slice of diff IDs spanning the
/// block's ID range.
///
/// **Key distinction from `range_overlaps`**: a diff with only pure
/// creates (new IDs not present in the base PBF) can cause
/// `range_overlaps` to return true (the create IDs fall within the
/// blob's range), but this function returns false (no element in the
/// block has a matching diff ID). The worker separately checks for
/// upserts inside the blob's range and rewrites it when any are present,
/// so those creates are interleaved at their sorted positions.
pub(super) fn block_overlaps_diff(
    block: &PrimitiveBlock,
    ranges: &DiffRanges,
    walk: &mut BlobWalk<'_>,
) -> Result<bool> {
    use std::cmp::Ordering;

    // `None` until the kind's first element seeds it by binary search;
    // starting at 0 walked the whole diff prefix below every blob.
    let mut node_cursor: Option<usize> = None;
    let mut way_cursor: Option<usize> = None;
    let mut rel_cursor: Option<usize> = None;

    for element in block.elements_skip_metadata() {
        let (cursor, sorted, id) = match &element {
            Element::DenseNode(dn) => (&mut node_cursor, &ranges.node_ids[..], dn.id()),
            Element::Node(n) => (&mut node_cursor, &ranges.node_ids[..], n.id()),
            Element::Way(w) => (&mut way_cursor, &ranges.way_ids[..], w.id()),
            Element::Relation(r) => (&mut rel_cursor, &ranges.rel_ids[..], r.id()),
        };
        walk.observe(id)?;
        let cursor = cursor.get_or_insert_with(|| {
            sorted.partition_point(|&x| crate::osm_id::osm_id_cmp(x, id) == Ordering::Less)
        });
        while *cursor < sorted.len()
            && crate::osm_id::osm_id_cmp(sorted[*cursor], id) == Ordering::Less
        {
            *cursor += 1;
        }
        if *cursor < sorted.len() && sorted[*cursor] == id {
            return Ok(true);
        }
    }
    Ok(false)
}
