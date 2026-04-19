//! Diff ID ranges for fast overlap checking and per-type upsert cursor tracking.

use crate::blob_meta::ElemKind;
use crate::osc::CompactDiffOverlay;

/// Pre-computed sorted ID vectors from the diff, for fast overlap checks.
///
/// `node_ids`/`way_ids`/`rel_ids` include both upserts and deletes - used
/// for range overlap checks. `node_upserts`/`way_upserts`/`rel_upserts`
/// contain only create/modify IDs (no deletes) - used for inline assignment
/// and gap create tracking.
pub(super) struct DiffRanges {
    /// Sorted node IDs affected by the diff (upserts + deletes).
    pub(super) node_ids: Vec<i64>,
    /// Sorted way IDs affected by the diff (upserts + deletes).
    pub(super) way_ids: Vec<i64>,
    /// Sorted relation IDs affected by the diff (upserts + deletes).
    pub(super) rel_ids: Vec<i64>,
    /// Sorted create/modify node IDs (no deletes). For inline assignment + gap creates.
    node_upserts: Vec<i64>,
    /// Sorted create/modify way IDs (no deletes).
    way_upserts: Vec<i64>,
    /// Sorted create/modify relation IDs (no deletes).
    rel_upserts: Vec<i64>,
}

impl DiffRanges {
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    pub(super) fn from_diff(diff: &CompactDiffOverlay) -> Self {
        let mut node_ids: Vec<i64> = diff
            .node_ids()
            .chain(diff.deleted_nodes.iter())
            .copied()
            .collect();
        node_ids.sort_unstable_by(|a, b| super::super::osm_id_cmp(*a, *b));
        node_ids.dedup();

        let mut way_ids: Vec<i64> = diff
            .way_ids()
            .chain(diff.deleted_ways.iter())
            .copied()
            .collect();
        way_ids.sort_unstable_by(|a, b| super::super::osm_id_cmp(*a, *b));
        way_ids.dedup();

        let mut rel_ids: Vec<i64> = diff
            .relation_ids()
            .chain(diff.deleted_relations.iter())
            .copied()
            .collect();
        rel_ids.sort_unstable_by(|a, b| super::super::osm_id_cmp(*a, *b));
        rel_ids.dedup();

        let mut node_upserts: Vec<i64> = diff.node_ids().copied().collect();
        node_upserts.sort_unstable_by(|a, b| super::super::osm_id_cmp(*a, *b));
        node_upserts.dedup();

        let mut way_upserts: Vec<i64> = diff.way_ids().copied().collect();
        way_upserts.sort_unstable_by(|a, b| super::super::osm_id_cmp(*a, *b));
        way_upserts.dedup();

        let mut rel_upserts: Vec<i64> = diff.relation_ids().copied().collect();
        rel_upserts.sort_unstable_by(|a, b| super::super::osm_id_cmp(*a, *b));
        rel_upserts.dedup();

        Self {
            node_ids,
            way_ids,
            rel_ids,
            node_upserts,
            way_upserts,
            rel_upserts,
        }
    }

    /// Check if any affected ID of the given type falls within [min_id, max_id].
    ///
    /// This is a coarse range check used during blob classification. A true
    /// result means the blob *might* need rewriting - it still gets a secondary
    /// check via `block_overlaps_diff` after full parsing. A false result means
    /// the blob is safe for raw passthrough (no diff IDs in its range at all).
    pub(super) fn range_overlaps(&self, kind: ElemKind, min_id: i64, max_id: i64) -> bool {
        let ids = match kind {
            ElemKind::Node => &self.node_ids,
            ElemKind::Way => &self.way_ids,
            ElemKind::Relation => &self.rel_ids,
        };
        if ids.is_empty() {
            return false;
        }
        // Binary search for the first ID >= blob's OSM-first in OSM order
        let first = super::super::blob_osm_first_key(min_id, max_id);
        let last = super::super::blob_osm_last_key(min_id, max_id);
        let pos = ids.partition_point(|&id| super::super::osm_id_key(id) < first);
        pos < ids.len() && super::super::osm_id_key(ids[pos]) <= last
    }

    /// Return the sorted upsert (create/modify) IDs for a given element kind.
    pub(super) fn upserts(&self, kind: ElemKind) -> &[i64] {
        match kind {
            ElemKind::Node => &self.node_upserts,
            ElemKind::Way => &self.way_upserts,
            ElemKind::Relation => &self.rel_upserts,
        }
    }
}

// osc_member_type_to_member_type removed: OscRelMember.member_type is now
// a MemberType enum directly (see osc.rs), so no string→enum conversion needed.

/// Grouped per-type cursors tracking how far through each upsert vector
/// we have emitted creates. Replaces three bare `usize` variables.
pub(super) struct UpsertCursors {
    node: usize,
    way: usize,
    rel: usize,
}

impl UpsertCursors {
    pub(super) fn new() -> Self {
        Self { node: 0, way: 0, rel: 0 }
    }

    /// Mutable cursor + upsert slice for the given element kind.
    pub(super) fn get_mut<'a>(&mut self, kind: ElemKind, ranges: &'a DiffRanges) -> (&mut usize, &'a [i64]) {
        match kind {
            ElemKind::Node => (&mut self.node, ranges.upserts(ElemKind::Node)),
            ElemKind::Way => (&mut self.way, ranges.upserts(ElemKind::Way)),
            ElemKind::Relation => (&mut self.rel, ranges.upserts(ElemKind::Relation)),
        }
    }

    /// Immutable cursor value + upsert slice for the given element kind.
    pub(super) fn get<'a>(&self, kind: ElemKind, ranges: &'a DiffRanges) -> (usize, &'a [i64]) {
        match kind {
            ElemKind::Node => (self.node, ranges.upserts(ElemKind::Node)),
            ElemKind::Way => (self.way, ranges.upserts(ElemKind::Way)),
            ElemKind::Relation => (self.rel, ranges.upserts(ElemKind::Relation)),
        }
    }
}
