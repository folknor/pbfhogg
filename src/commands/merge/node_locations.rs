//! Sparse node location index for `--locations-on-ways`.

use std::path::Path;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::blob_index::ElemKind;
use crate::osc::CompactDiffOverlay;

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
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
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

    /// Check if all needed nodes have been found.
    pub(super) fn all_found(&self) -> bool {
        self.needed_set.is_empty()
    }

    /// Pre-scan the base PBF to fill all remaining needed node coordinates.
    ///
    /// Uses indexdata to skip non-node blobs and blobs whose ID ranges don't
    /// overlap needed IDs. Matching blobs use the node scanner (no
    /// PrimitiveBlock construction). Exits early once all needed IDs are found.
    /// Returns `(nodes_found, blobs_scanned)`.
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    pub(super) fn prefill_from_base(
        &mut self,
        base_pbf: &Path,
        direct_io: bool,
    ) -> super::Result<(u64, u64)> {
        if self.all_found() {
            return Ok((0, 0));
        }

        let mut reader = crate::blob::BlobReader::open(base_pbf, direct_io)?;
        reader.set_parse_indexdata(true);

        let mut buf: Vec<u8> = Vec::new();
        let mut tuples = Vec::new();
        let mut group_starts = Vec::new();
        let mut nodes_found: u64 = 0;
        let mut blobs_scanned: u64 = 0;
        let mut blobs_skipped_non_node: u64 = 0;
        let mut blobs_skipped_range: u64 = 0;
        let mut early_exit = false;

        for blob_result in &mut reader {
            let blob = blob_result?;
            if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
                continue;
            }
            if let Some(idx) = blob.index() {
                // Skip non-node blobs and node blobs outside needed range
                if idx.kind != ElemKind::Node {
                    // Sorted PBF: once past nodes, no more node blobs
                    blobs_skipped_non_node += 1;
                    break;
                }
                if !self.overlaps_needed(idx.min_id, idx.max_id) {
                    blobs_skipped_range += 1;
                    continue;
                }
            }
            blob.decompress_into(&mut buf)?;
            tuples.clear();
            group_starts.clear();
            if crate::commands::node_scanner::extract_node_tuples(
                &buf, &mut tuples, &mut group_starts,
            ).is_ok() {
                for t in &tuples {
                    if self.needed_set.remove(&t.id) {
                        self.locations.insert(t.id, (t.lat, t.lon));
                        nodes_found += 1;
                    }
                }
            }
            blobs_scanned += 1;
            if self.all_found() {
                early_exit = true;
                break;
            }
        }

        #[allow(clippy::cast_possible_wrap)]
        {
            crate::debug::emit_counter("merge_prefill_nodes_found", nodes_found as i64);
            crate::debug::emit_counter("merge_prefill_blobs_scanned", blobs_scanned as i64);
            crate::debug::emit_counter(
                "merge_prefill_blobs_skipped_non_node",
                blobs_skipped_non_node as i64,
            );
            crate::debug::emit_counter(
                "merge_prefill_blobs_skipped_range",
                blobs_skipped_range as i64,
            );
            crate::debug::emit_counter(
                "merge_prefill_early_exit",
                i64::from(early_exit),
            );
        }

        Ok((nodes_found, blobs_scanned))
    }
}
