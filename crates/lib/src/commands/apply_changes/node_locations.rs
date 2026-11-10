//! Sparse node location index for `--locations-on-ways`.
//!
//! Now a setup-only helper: produces the OSC-pre-seeded `locations`
//! map (handed to the drain as `DrainLocations::seeded`) and the
//! still-needed `needed_set` (handed to the worker pool as
//! `WorkerLocations::needed`, a coord-extraction filter). The base-PBF prefill phase no longer exists - it has been
//! fused into the streaming worker pool's node phase, where workers
//! opportunistically extract coords for `needed_set` IDs as they
//! decompress node blobs.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::ElementKind;
use crate::osc::sorted::{Seek, SortedDiff};

/// Sparse node coordinate index for maintaining LocationsOnWays through
/// merges. Built once from the OSC overlay; the drain takes ownership of
/// `locations` (consumed at the node→way barrier as the seed for the
/// merged `loc_map`) and the worker pool takes a shared `Arc` reference
/// to `needed_set` (it keeps only the coordinates `extract_node_coords`
/// reports for IDs in the set).
pub(super) struct NodeLocationIndex {
    /// Coordinates pre-seeded from OSC node creates/modifications.
    pub(super) locations: FxHashMap<i64, (i32, i32)>,
    /// Node IDs still required from the base PBF (not present in OSC,
    /// not deleted by it).
    pub(super) needed_set: FxHashSet<i64>,
    /// Unique node IDs referenced by OSC ways. At least
    /// `locations.len() + needed_set.len()`: a referenced node the diff
    /// deletes is in neither map but still counts here, so it is reported
    /// as unresolved rather than silently satisfied.
    pub(super) total_referenced: u64,
}

impl NodeLocationIndex {
    /// Build the index from an already-parsed OSC diff.
    ///
    /// 1. Collects all node IDs referenced by OSC ways
    /// 2. Seeds coordinates from OSC nodes (creates/modifications)
    /// 3. Remaining needed IDs stored for base PBF extraction in the
    ///    worker pool's node phase.
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    pub(super) fn build_from_diff(diff: &SortedDiff) -> Self {
        // Every node ID referenced by a surviving OSC way, sorted and
        // unique, so it can be joined against the diff's sorted node lists.
        let way_count = diff.upserts(ElementKind::Way).len();
        let mut all_needed: Vec<i64> = Vec::new();
        for i in 0..way_count {
            all_needed.extend(diff.way(i).refs());
        }
        all_needed.sort_unstable_by(|a, b| crate::osm_id::osm_id_cmp(*a, *b));
        all_needed.dedup();

        let mut locations: FxHashMap<i64, (i32, i32)> =
            FxHashMap::with_capacity_and_hasher(all_needed.len(), Default::default());
        let mut still_needed: FxHashSet<i64> = FxHashSet::default();

        // Load-bearing: the node upserts are both created AND modified OSC
        // nodes. Way refs pointing to modified base nodes need the fresh
        // coords; a narrower list (creates only) would silently propagate
        // stale base-PBF coords to the rewritten ways.
        //
        // A node the diff deletes is not taken from the base either: it is
        // absent from the merged output, so an OSC way referencing it has
        // an unresolved reference. Fetching its pre-deletion base coords
        // would satisfy the missing-node guard with a node that no longer
        // exists.
        let upserts = diff.upserts(ElementKind::Node);
        let deleted = diff.deleted(ElementKind::Node);
        let mut in_upserts = Seek::default();
        let mut in_deleted = Seek::default();
        for &node_id in &all_needed {
            if let Some(i) = in_upserts.find(upserts, node_id) {
                let node = diff.node(i);
                locations.insert(node_id, (node.decimicro_lat(), node.decimicro_lon()));
            } else if in_deleted.find(deleted, node_id).is_none() {
                still_needed.insert(node_id);
            }
        }

        Self {
            locations,
            needed_set: still_needed,
            total_referenced: all_needed.len() as u64,
        }
    }
}
