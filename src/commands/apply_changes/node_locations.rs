//! Sparse node location index for `--locations-on-ways`.
//!
//! Now a setup-only helper: produces the OSC-pre-seeded `locations`
//! map (handed to the drain as `seeded_locations`) and the still-needed
//! `needed_set` (handed to the worker pool as a coord-extraction
//! filter). The base-PBF prefill phase no longer exists - it has been
//! fused into the streaming worker pool's node phase, where workers
//! opportunistically extract coords for `needed_set` IDs as they
//! decompress node blobs.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::osc::CompactDiffOverlay;

/// Sparse node coordinate index for maintaining LocationsOnWays through
/// merges. Built once from the OSC overlay; the drain takes ownership of
/// `locations` (consumed at the node→way barrier as the seed for the
/// merged `loc_map`) and the worker pool takes a shared `Arc` reference
/// to `needed_set` (used to filter `extract_node_tuples` output).
pub(super) struct NodeLocationIndex {
    /// Coordinates pre-seeded from OSC node creates/modifications.
    pub(super) locations: FxHashMap<i64, (i32, i32)>,
    /// Node IDs still required from the base PBF (not present in OSC).
    pub(super) needed_set: FxHashSet<i64>,
}

impl NodeLocationIndex {
    /// Build the index from an already-parsed OSC diff.
    ///
    /// 1. Collects all node IDs referenced by OSC ways
    /// 2. Seeds coordinates from OSC nodes (creates/modifications)
    /// 3. Remaining needed IDs stored for base PBF extraction in the
    ///    worker pool's node phase.
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    pub(super) fn build_from_diff(diff: &CompactDiffOverlay) -> Self {
        // Collect all node IDs referenced by OSC ways.
        let mut all_needed: FxHashSet<i64> = FxHashSet::default();
        for &way_id in diff.way_ids() {
            if let Some(way) = diff.get_way(way_id) {
                for node_id in way.refs() {
                    all_needed.insert(node_id);
                }
            }
        }

        let mut locations: FxHashMap<i64, (i32, i32)> =
            FxHashMap::with_capacity_and_hasher(all_needed.len(), Default::default());
        let mut still_needed: FxHashSet<i64> = FxHashSet::default();

        for &node_id in &all_needed {
            if let Some(node) = diff.get_node(node_id) {
                locations.insert(node_id, (node.decimicro_lat(), node.decimicro_lon()));
            } else {
                still_needed.insert(node_id);
            }
        }

        Self {
            locations,
            needed_set: still_needed,
        }
    }
}
