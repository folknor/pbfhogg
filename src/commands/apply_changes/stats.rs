//! Merge statistics, per-phase timers, and RSS tracking.

/// Statistics from a merge operation.
pub struct MergeStats {
    pub base_nodes: u64,
    pub base_ways: u64,
    pub base_relations: u64,
    pub diff_nodes: u64,
    pub diff_ways: u64,
    pub diff_relations: u64,
    pub deleted: u64,
    pub blobs_passthrough: u64,
    pub blobs_rewritten: u64,
    pub blobs_index_hit: u64,
    /// Bytes of raw passthrough frames (wire size including framing).
    pub bytes_passthrough: u64,
    /// Bytes of rewritten blobs as written (framed, compressed).
    pub bytes_rewritten: u64,
    /// Heap bytes used by the CompactDiffOverlay after OSC parsing.
    pub diff_heap_bytes: u64,
    /// Per-blob frame sizes in bytes for exact percentile computation.
    /// Retains 4 bytes per observed blob (plus Vec spare capacity); summary
    /// temporarily clones that allocation to sort without mutating stats.
    /// A fixed-width histogram gives approximate percentiles and changes the
    /// reported values. Keep exact samples: compare this 8-byte-per-blob peak
    /// with overlay and decoded-block RSS before trading observability for a
    /// bound. An exact size-frequency map can cost more on varied frame sizes.
    pub(super) blob_sizes: Vec<u32>,
    /// The run had `--locations-on-ways`: the summary reports the refresh.
    pub(super) locations_on_ways: bool,
    // -- locations-on-ways stats (only populated when flag is on) --
    /// Unique node IDs referenced by OSC ways.
    pub loc_nodes_needed: u64,
    /// Node coordinates found in OSC (pre-seeded).
    pub loc_nodes_from_diff: u64,
    /// Node coordinates found in base PBF during merge.
    pub loc_nodes_from_base: u64,
    /// Referenced nodes resolved nowhere, including nodes the diff
    /// deletes. Non-zero fails the merge unless `--ignore-missing-nodes`
    /// asked for the (0,0) fallback.
    pub loc_missing: u64,
    /// Base node blobs decompressed for coordinate extraction (every node
    /// blob, whether it was then passed through or rewritten).
    pub loc_node_blobs_scanned: u64,
    /// Way blobs with no merge work of their own, searched for surviving
    /// base ways whose inline coordinates the OSC makes stale. Most were
    /// passed through undecoded before the search existed.
    pub loc_refresh_blobs_scanned: u64,
    /// Blobs rewritten only because they held such a way (no element of
    /// theirs in the OSC, no create in their range).
    pub loc_refresh_only_blobs: u64,
    /// Surviving base ways written with at least one inline coordinate
    /// changed to the OSC's.
    pub loc_refreshed_ways: u64,
    /// Inline coordinates changed to the OSC's, one per way-node position.
    pub loc_refreshed_refs: u64,
    /// Surviving base ways' references to nodes the OSC deletes, counted in
    /// rewritten and searched way blobs only. Way blobs passed through
    /// unsearched are not counted, and an OSC that creates or modifies no
    /// node searches none, so on a delete-only node diff (exactly where a
    /// mismatch is likeliest) this covers only blobs rewritten for other
    /// edits. Zero therefore does not show that no surviving way uses a
    /// deleted node. Such refs keep their coordinate; nonzero means the base
    /// and the OSC disagree (the OSM API does not delete a node a way still
    /// uses).
    pub loc_deleted_node_refs: u64,
}

impl MergeStats {
    pub(super) fn new() -> Self {
        Self {
            base_nodes: 0,
            base_ways: 0,
            base_relations: 0,
            diff_nodes: 0,
            diff_ways: 0,
            diff_relations: 0,
            deleted: 0,
            blobs_passthrough: 0,
            blobs_rewritten: 0,
            blobs_index_hit: 0,
            bytes_passthrough: 0,
            bytes_rewritten: 0,
            diff_heap_bytes: 0,
            blob_sizes: Vec::new(),
            locations_on_ways: false,
            loc_nodes_needed: 0,
            loc_nodes_from_diff: 0,
            loc_nodes_from_base: 0,
            loc_missing: 0,
            loc_node_blobs_scanned: 0,
            loc_refresh_blobs_scanned: 0,
            loc_refresh_only_blobs: 0,
            loc_refreshed_ways: 0,
            loc_refreshed_refs: 0,
            loc_deleted_node_refs: 0,
        }
    }

    pub fn total_elements(&self) -> u64 {
        self.base_nodes
            + self.base_ways
            + self.base_relations
            + self.diff_nodes
            + self.diff_ways
            + self.diff_relations
    }

    pub(super) fn merge_from(&mut self, other: &MergeStats) {
        self.base_nodes += other.base_nodes;
        self.base_ways += other.base_ways;
        self.base_relations += other.base_relations;
        self.diff_nodes += other.diff_nodes;
        self.diff_ways += other.diff_ways;
        self.diff_relations += other.diff_relations;
        self.deleted += other.deleted;
        self.bytes_passthrough += other.bytes_passthrough;
        self.bytes_rewritten += other.bytes_rewritten;
    }

    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        let total_blobs = self.blobs_passthrough + self.blobs_rewritten;
        writeln!(
            w,
            "Merge complete: {} elements written",
            self.total_elements()
        )?;
        writeln!(
            w,
            "  Base: {} nodes, {} ways, {} relations",
            self.base_nodes, self.base_ways, self.base_relations,
        )?;
        writeln!(
            w,
            "  Diff: {} nodes, {} ways, {} relations",
            self.diff_nodes, self.diff_ways, self.diff_relations,
        )?;
        writeln!(w, "  Deleted: {}", self.deleted)?;
        writeln!(
            w,
            "  Blobs: {} passthrough ({} index-hit), {} rewritten (of {total_blobs} total)",
            self.blobs_passthrough, self.blobs_index_hit, self.blobs_rewritten,
        )?;
        let total_bytes = self.bytes_passthrough + self.bytes_rewritten;
        if total_bytes > 0 {
            #[allow(clippy::cast_precision_loss)]
            let rewrite_pct = (self.bytes_rewritten as f64 / total_bytes as f64) * 100.0;
            writeln!(
                w,
                "  Bytes: {} passthrough, {} rewritten ({rewrite_pct:.1}% rewrite ratio)",
                self.bytes_passthrough, self.bytes_rewritten,
            )?;
        }
        if !self.blob_sizes.is_empty() {
            let mut sizes = self.blob_sizes.clone();
            let (p50, p95, p99) = percentiles_u32(&mut sizes);
            writeln!(w, "  Blob sizes: p50={p50}, p95={p95}, p99={p99} bytes")?;
        }
        if self.diff_heap_bytes > 0 {
            #[allow(clippy::cast_precision_loss)]
            let mb = self.diff_heap_bytes as f64 / (1024.0 * 1024.0);
            writeln!(w, "  CompactDiffOverlay heap: {mb:.1} MB")?;
        }
        if self.loc_nodes_needed > 0 {
            writeln!(
                w,
                "  Locations-on-ways: {} nodes needed, {} from diff, {} from base, {} missing, {} node blobs scanned",
                self.loc_nodes_needed,
                self.loc_nodes_from_diff,
                self.loc_nodes_from_base,
                self.loc_missing,
                self.loc_node_blobs_scanned,
            )?;
        }
        if self.locations_on_ways {
            writeln!(
                w,
                "  Inline locations refreshed: {} refs in {} ways, {} way blobs scanned, {} rewritten only for this, {} refs to deleted nodes kept (in rewritten or searched blobs only)",
                self.loc_refreshed_refs,
                self.loc_refreshed_ways,
                self.loc_refresh_blobs_scanned,
                self.loc_refresh_only_blobs,
                self.loc_deleted_node_refs,
            )?;
        }

        Ok(())
    }
}

/// Compute p50, p95, p99 from a mutable slice. Returns `(0, 0, 0)` if empty.
fn percentiles_u32(data: &mut [u32]) -> (u32, u32, u32) {
    if data.is_empty() {
        return (0, 0, 0);
    }
    data.sort_unstable();
    let len = data.len();
    (data[len / 2], data[len * 95 / 100], data[len * 99 / 100])
}
