//! Sparse node location index for `--locations-on-ways`.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::osc::CompactDiffOverlay;
use crate::{Element, PrimitiveBlock};

/// Sparse node coordinate index for maintaining LocationsOnWays through merges.
///
/// Only contains coordinates for nodes referenced by OSC ways. Populated in two
/// stages: (1) pre-seeded from OSC node creates/modifications, (2) filled from
/// base PBF during the merge pass for nodes not in the OSC.
pub(super) struct NodeLocationIndex {
    /// Coordinates indexed by node ID (decimicrodegrees).
    pub(super) locations: FxHashMap<i64, (i32, i32)>,
    /// Node IDs still needed from the base PBF (not found in OSC).
    /// Sorted for range overlap checks against BlobIndex.
    pub(super) needed_sorted: Vec<i64>,
    /// Same IDs as `needed_sorted` but as a set for O(1) membership tests.
    pub(super) needed_set: FxHashSet<i64>,
}

impl NodeLocationIndex {
    /// Build the index from an already-parsed OSC diff.
    ///
    /// 1. Collects all node IDs referenced by OSC ways
    /// 2. Seeds coordinates from OSC nodes (creates/modifications)
    /// 3. Remaining needed IDs stored for base PBF extraction
    pub(super) fn build_from_diff(diff: &CompactDiffOverlay) -> Self {
        // Collect all node IDs referenced by OSC ways
        let mut all_needed: FxHashSet<i64> = FxHashSet::default();
        for &way_id in diff.way_ids() {
            if let Some(way) = diff.get_way(way_id) {
                for node_id in way.refs() {
                    all_needed.insert(node_id);
                }
            }
        }

        // Seed from OSC nodes
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

        let mut needed_sorted: Vec<i64> = still_needed.iter().copied().collect();
        needed_sorted.sort_unstable();

        let seeded = locations.len() as u64;
        let remaining = still_needed.len() as u64;
        let total = all_needed.len() as u64;
        eprintln!(
            "Locations-on-ways: {total} node IDs needed, {seeded} from diff, {remaining} from base"
        );

        Self {
            locations,
            needed_sorted,
            needed_set: still_needed,
        }
    }

    /// Check if a blob's ID range overlaps any still-needed node IDs.
    pub(super) fn overlaps_needed(&self, min_id: i64, max_id: i64) -> bool {
        if self.needed_sorted.is_empty() {
            return false;
        }
        // Find the first needed ID >= min_id
        let start = self.needed_sorted.partition_point(|&id| id < min_id);
        // If that ID is <= max_id, there's overlap
        start < self.needed_sorted.len() && self.needed_sorted[start] <= max_id
    }

    /// Extract needed coordinates from a decoded PrimitiveBlock.
    pub(super) fn extract_from_block(&mut self, block: &PrimitiveBlock) -> u64 {
        let mut found: u64 = 0;
        for element in block.elements_skip_metadata() {
            match &element {
                Element::DenseNode(dn) => {
                    if self.needed_set.contains(&dn.id()) {
                        self.locations
                            .insert(dn.id(), (dn.decimicro_lat(), dn.decimicro_lon()));
                        self.needed_set.remove(&dn.id());
                        found += 1;
                    }
                }
                Element::Node(n) => {
                    if self.needed_set.contains(&n.id()) {
                        self.locations
                            .insert(n.id(), (n.decimicro_lat(), n.decimicro_lon()));
                        self.needed_set.remove(&n.id());
                        found += 1;
                    }
                }
                Element::Way(_) | Element::Relation(_) => {}
            }
        }
        found
    }

    /// Check if all needed nodes have been found.
    pub(super) fn all_found(&self) -> bool {
        self.needed_set.is_empty()
    }
}
