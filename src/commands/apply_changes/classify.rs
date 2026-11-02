//! Per-element overlap check used by the streaming worker pool's
//! precise-check stage, and the walk guard every decoded candidate's element
//! loops feed. The legacy `classify_only` (which had the fast-path /
//! scan-path / parse-path branches and was driven by the batch loop) is
//! gone - the scanner owns the fast-path now and the worker pool slow-path
//! always decompresses + parses + precise-checks.

use crate::blob_meta::BlobIndex;
use crate::error::Result;
use crate::osc::sorted::{Seek, SortedDiff};
use crate::{Element, ElementKind, PrimitiveBlock};

/// The walk every decoded candidate's element loops feed: the shared
/// [`ClaimWalk`](crate::blob_meta::ClaimWalk) under apply-changes' order
/// requirement.
///
/// Every decoded candidate ends in one complete walk before its item leaves
/// the worker: a precise-check miss walks every element, and a hit, a forced
/// rewrite and a refresh-only rewrite go through the rewrite loop, which
/// does; a way blob the refresh scan finds clean has had every way walked.
///
/// - An ID below the previous one in OSM order contradicts the header's
///   `Sort.Type_then_ID`: `Unsorted`. The rewrite interleaves inline creates
///   and the precise check advances its diff cursors on that order, so a
///   blob `[5, 3]` would write a modified 3 twice, or pass a deleted 3
///   through.
/// - A claim (indexdata) that is not exact for the walk is
///   `IndexdataMismatch`. The inline-upsert slice, the drain's gap creates
///   and cursor, and the output order guard all place the blob by its claim,
///   so an understated range duplicates a modify or writes a create out of
///   order, and an overstated one hands a neighbour's upsert to this blob as
///   well.
pub(super) type BlobWalk<'c> = crate::blob_meta::ClaimWalk<'c>;

/// A walk of a candidate blob carrying `claim` (`None` for an unindexed
/// blob), holding it to the header's order.
pub(super) fn blob_walk(claim: Option<&BlobIndex>) -> BlobWalk<'_> {
    BlobWalk::new(
        claim,
        crate::blob_meta::Order::Required(
            "apply-changes: refusing to write an unsorted PBF under a Sort.Type_then_ID header",
        ),
    )
}

/// Check if any element *actually in the block* has a matching ID in the diff.
/// Returns true if the block needs re-encoding, false for safe passthrough.
///
/// Every element visited is observed by `walk`, so a block that breaks
/// order is refused here rather than misjudged; on a `false` return every
/// element has been visited and the caller ends the walk. On `true` the walk
/// stops at the hit, and the rewrite that follows walks the block whole.
///
/// A sorted merge against the diff's upserts and deletes through
/// [`KindSeeks`]: per-kind cursors that only advance forward.
///
/// **Key distinction from `SortedDiff::range_overlaps`**: a diff with only pure
/// creates (new IDs not present in the base PBF) can cause
/// `range_overlaps` to return true (the create IDs fall within the
/// blob's range), but this function returns false (no element in the
/// block has a matching diff ID). The worker separately checks for
/// upserts inside the blob's range and rewrites it when any are present,
/// so those creates are interleaved at their sorted positions.
///
/// It reads only IDs but walks a built block on purpose, not a projection
/// on the block validator: almost every decoded candidate is a hit that
/// needs the block for its rewrite (43,671 of 43,687 at planet seq 4913,
/// bygg), so a projection would still build it, and the walk ends at the
/// first hit. The whole check is about 83 of roughly 2,500 worker
/// core-seconds there (`merge_streaming_precise_ns`), under the noise floor
/// even if a projection removed all of it.
pub(super) fn block_overlaps_diff(
    block: &PrimitiveBlock,
    diff: &SortedDiff,
    walk: &mut BlobWalk<'_>,
) -> Result<bool> {
    let mut cursors = KindSeeks::default();
    for element in block.elements_skip_metadata() {
        let (kind, id) = element_kind_id(&element);
        walk.observe(id)?;
        if cursors.find(diff, kind, id) != DiffHit::None {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The kind and ID of a decoded element.
#[inline]
pub(super) fn element_kind_id(element: &Element<'_>) -> (ElementKind, i64) {
    match element {
        Element::DenseNode(dn) => (ElementKind::Node, dn.id()),
        Element::Node(n) => (ElementKind::Node, n.id()),
        Element::Way(w) => (ElementKind::Way, w.id()),
        Element::Relation(r) => (ElementKind::Relation, r.id()),
    }
}

/// What the diff says about one base element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DiffHit {
    /// Untouched by the diff.
    None,
    /// Deleted by the diff.
    Deleted,
    /// Replaced by the diff's record at this index into the kind's upserts.
    Upserted(usize),
}

/// Per-kind forward cursors over the diff's sorted upserts and deletes,
/// for one block's elements: each kind's elements arrive in `osm_id_cmp`
/// order (the block's walk refuses a block that breaks it, before any
/// output leaves the worker), so each cursor starts by binary search at
/// the kind's first element and then only moves forward, over at most the
/// diff IDs spanning the block. An ID the block holds twice is found both
/// times ([`Seek`]). Kept per kind, not only for the blob's claimed kind, so
/// an element of another kind in the block is matched against its own
/// kind's changes.
#[derive(Default)]
pub(super) struct KindSeeks {
    upserts: [Seek; 3],
    deleted: [Seek; 3],
}

impl KindSeeks {
    #[inline]
    pub(super) fn find(&mut self, diff: &SortedDiff, kind: ElementKind, id: i64) -> DiffHit {
        let k = usize::from(kind.ordinal());
        let table = diff.kind(kind);
        if self.deleted[k].find(table.deleted(), id).is_some() {
            DiffHit::Deleted
        } else if let Some(i) = self.upserts[k].find(table.upserts(), id) {
            DiffHit::Upserted(i)
        } else {
            DiffHit::None
        }
    }
}
