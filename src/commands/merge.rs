//! PBF merge: apply an OSC diff overlay to a base PBF, producing an updated PBF.
//!
//! Single-pass streaming batch pipeline:
//!   Phase 1: Parallel classify              [rayon pool]
//!   Phase 2: Sequential inline assign       [main thread, O(log n) per blob]
//!   Phase 3+4: Parallel rewrite + streaming output [rayon pool + main thread]
//!
//! Key insight: we pass ALL upsert IDs in a blob's range to the rewrite function.
//! IDs that match base elements are modifications (handled by normal element processing);
//! IDs that don't match are creates (emitted by the cursor). This eliminates the need
//! for a separate pass to collect modification IDs and compute create lists.

use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;

use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::blob::{
    decode_blob_to_headerblock, decompress_blob_data_into,
    parse_primitive_block_from_bytes_owned, BlobKind,
};
use crate::blob_index::{self, BlobIndex, ElemKind};
use bytes::Bytes;
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::file_reader::FileReader;
use crate::file_writer::FileWriter;
use crate::osc::{parse_osc_file, CompactDiffOverlay};
use crate::writer::{Compression, PbfWriter};
use crate::{Element, PrimitiveBlock};

use super::{
    build_output_header, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, flush_local, flush_passthrough_buf, read_raw_frame,
    require_indexdata, writer_from_header_bytes, HeaderOverrides, RawBlobFrame,
};

use super::{Result, BATCH_BYTE_BUDGET, BATCH_MIN_BLOBS, BATCH_MAX_BLOBS};

const READER_CHANNEL_SIZE: usize = 128;

// ---------------------------------------------------------------------------
// Progress counters
// ---------------------------------------------------------------------------

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
    pub blobs_skip_decompress: u64,
    pub blobs_scan_only: u64,
    pub blobs_index_hit: u64,
    /// Bytes of raw passthrough frames (wire size including framing).
    pub bytes_passthrough: u64,
    /// Bytes of rewritten blocks (pre-compression protobuf size).
    pub bytes_rewritten: u64,
    /// Heap bytes used by the CompactDiffOverlay after OSC parsing.
    pub diff_heap_bytes: u64,
    /// Per-blob frame sizes in bytes for percentile computation.
    blob_sizes: Vec<u32>,
    // -- locations-on-ways stats (only populated when flag is on) --
    /// Total node IDs needed for OSC ways.
    pub loc_nodes_needed: u64,
    /// Node coordinates found in OSC (pre-seeded).
    pub loc_nodes_from_diff: u64,
    /// Node coordinates found in base PBF during merge.
    pub loc_nodes_from_base: u64,
    /// Node coordinates not found anywhere (0,0 fallback).
    pub loc_missing: u64,
    /// Passthrough node blobs decompressed for coordinate extraction.
    pub loc_node_blobs_scanned: u64,
}

impl MergeStats {
    fn new() -> Self {
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
            blobs_skip_decompress: 0,
            blobs_scan_only: 0,
            blobs_index_hit: 0,
            bytes_passthrough: 0,
            bytes_rewritten: 0,
            diff_heap_bytes: 0,
            blob_sizes: Vec::new(),
            loc_nodes_needed: 0,
            loc_nodes_from_diff: 0,
            loc_nodes_from_base: 0,
            loc_missing: 0,
            loc_node_blobs_scanned: 0,
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

    fn merge_from(&mut self, other: &MergeStats) {
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

    pub fn print_summary(&self) {
        let total_blobs =
            self.blobs_passthrough + self.blobs_rewritten + self.blobs_skip_decompress;
        eprintln!("Merge complete: {} elements written", self.total_elements());
        eprintln!(
            "  Base: {} nodes, {} ways, {} relations",
            self.base_nodes, self.base_ways, self.base_relations,
        );
        eprintln!(
            "  Diff: {} nodes, {} ways, {} relations",
            self.diff_nodes, self.diff_ways, self.diff_relations,
        );
        eprintln!("  Deleted: {}", self.deleted);
        eprintln!(
            "  Blobs: {} passthrough ({} index-hit, {} scan-only, {} skip-decompress), {} rewritten (of {total_blobs} total)",
            self.blobs_passthrough + self.blobs_skip_decompress,
            self.blobs_index_hit,
            self.blobs_scan_only,
            self.blobs_skip_decompress,
            self.blobs_rewritten,
        );
        let total_bytes = self.bytes_passthrough + self.bytes_rewritten;
        if total_bytes > 0 {
            #[allow(clippy::cast_precision_loss)]
            let rewrite_pct = (self.bytes_rewritten as f64 / total_bytes as f64) * 100.0;
            eprintln!(
                "  Bytes: {} passthrough, {} rewritten ({rewrite_pct:.1}% rewrite ratio)",
                self.bytes_passthrough, self.bytes_rewritten,
            );
        }
        if !self.blob_sizes.is_empty() {
            let mut sizes = self.blob_sizes.clone();
            let (p50, p95, p99) = percentiles_u32(&mut sizes);
            eprintln!("  Blob sizes: p50={p50}, p95={p95}, p99={p99} bytes");
        }
        if self.diff_heap_bytes > 0 {
            #[allow(clippy::cast_precision_loss)]
            let mb = self.diff_heap_bytes as f64 / (1024.0 * 1024.0);
            eprintln!("  CompactDiffOverlay heap: {mb:.1} MB");
        }
        if self.loc_nodes_needed > 0 {
            eprintln!(
                "  Locations-on-ways: {} nodes needed, {} from diff, {} from base, {} missing, {} node blobs scanned",
                self.loc_nodes_needed, self.loc_nodes_from_diff,
                self.loc_nodes_from_base, self.loc_missing,
                self.loc_node_blobs_scanned,
            );
        }
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

/// Per-phase wall time accumulation across all batches.
#[cfg(feature = "hotpath")]
struct PhaseTimers {
    osc_parse: std::time::Duration,
    classify_total: std::time::Duration,
    rewrite_total: std::time::Duration,
    output_total: std::time::Duration,
    trailing_creates: std::time::Duration,
}

#[cfg(feature = "hotpath")]
impl PhaseTimers {
    fn new() -> Self {
        Self {
            osc_parse: std::time::Duration::ZERO,
            classify_total: std::time::Duration::ZERO,
            rewrite_total: std::time::Duration::ZERO,
            output_total: std::time::Duration::ZERO,
            trailing_creates: std::time::Duration::ZERO,
        }
    }
}

/// Read current RSS in kilobytes from `/proc/self/statm`.
/// Returns 0 on failure (non-Linux, read error, parse error).
#[cfg(feature = "hotpath")]
fn read_rss_kb() -> u64 {
    let Ok(statm) = std::fs::read_to_string("/proc/self/statm") else {
        return 0;
    };
    let Some(resident_str) = statm.split_whitespace().nth(1) else {
        return 0;
    };
    let Ok(pages) = resident_str.parse::<u64>() else {
        return 0;
    };
    pages * 4 // pages × 4096 / 1024 = pages × 4
}

/// Per-phase RSS tracking (rolling max across batches, in KB).
#[cfg(feature = "hotpath")]
struct PhaseRss {
    after_osc_parse: u64,
    classify_max: u64,
    rewrite_max: u64,
    output_max: u64,
    after_flush: u64,
}

#[cfg(feature = "hotpath")]
impl PhaseRss {
    fn new() -> Self {
        Self {
            after_osc_parse: 0,
            classify_max: 0,
            rewrite_max: 0,
            output_max: 0,
            after_flush: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Sparse node location index for --locations-on-ways
// ---------------------------------------------------------------------------

/// Sparse node coordinate index for maintaining LocationsOnWays through merges.
///
/// Only contains coordinates for nodes referenced by OSC ways. Populated in two
/// stages: (1) pre-seeded from OSC node creates/modifications, (2) filled from
/// base PBF during the merge pass for nodes not in the OSC.
struct NodeLocationIndex {
    /// Coordinates indexed by node ID (decimicrodegrees).
    locations: FxHashMap<i64, (i32, i32)>,
    /// Node IDs still needed from the base PBF (not found in OSC).
    /// Sorted for range overlap checks against BlobIndex.
    needed_sorted: Vec<i64>,
    /// Same IDs as `needed_sorted` but as a set for O(1) membership tests.
    needed_set: FxHashSet<i64>,
}

impl NodeLocationIndex {
    /// Build the index from an already-parsed OSC diff.
    ///
    /// 1. Collects all node IDs referenced by OSC ways
    /// 2. Seeds coordinates from OSC nodes (creates/modifications)
    /// 3. Remaining needed IDs stored for base PBF extraction
    fn build_from_diff(diff: &CompactDiffOverlay) -> Self {
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
    fn overlaps_needed(&self, min_id: i64, max_id: i64) -> bool {
        if self.needed_sorted.is_empty() {
            return false;
        }
        // Find the first needed ID >= min_id
        let start = self.needed_sorted.partition_point(|&id| id < min_id);
        // If that ID is <= max_id, there's overlap
        start < self.needed_sorted.len() && self.needed_sorted[start] <= max_id
    }

    /// Extract needed coordinates from a decoded PrimitiveBlock.
    fn extract_from_block(&mut self, block: &PrimitiveBlock) -> u64 {
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
    fn all_found(&self) -> bool {
        self.needed_set.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Diff ID ranges for fast overlap checking
// ---------------------------------------------------------------------------

/// Pre-computed sorted ID vectors from the diff, for fast overlap checks.
///
/// `node_ids`/`way_ids`/`rel_ids` include both upserts and deletes — used
/// for range overlap checks. `node_upserts`/`way_upserts`/`rel_upserts`
/// contain only create/modify IDs (no deletes) — used for inline assignment
/// and gap create tracking.
struct DiffRanges {
    /// Sorted node IDs affected by the diff (upserts + deletes).
    node_ids: Vec<i64>,
    /// Sorted way IDs affected by the diff (upserts + deletes).
    way_ids: Vec<i64>,
    /// Sorted relation IDs affected by the diff (upserts + deletes).
    rel_ids: Vec<i64>,
    /// Sorted create/modify node IDs (no deletes). For inline assignment + gap creates.
    node_upserts: Vec<i64>,
    /// Sorted create/modify way IDs (no deletes).
    way_upserts: Vec<i64>,
    /// Sorted create/modify relation IDs (no deletes).
    rel_upserts: Vec<i64>,
}

impl DiffRanges {
    fn from_diff(diff: &CompactDiffOverlay) -> Self {
        let mut node_ids: Vec<i64> = diff
            .node_ids()
            .chain(diff.deleted_nodes.iter())
            .copied()
            .collect();
        node_ids.sort_unstable_by(|a, b| super::osm_id_cmp(*a, *b));
        node_ids.dedup();

        let mut way_ids: Vec<i64> = diff
            .way_ids()
            .chain(diff.deleted_ways.iter())
            .copied()
            .collect();
        way_ids.sort_unstable_by(|a, b| super::osm_id_cmp(*a, *b));
        way_ids.dedup();

        let mut rel_ids: Vec<i64> = diff
            .relation_ids()
            .chain(diff.deleted_relations.iter())
            .copied()
            .collect();
        rel_ids.sort_unstable_by(|a, b| super::osm_id_cmp(*a, *b));
        rel_ids.dedup();

        let mut node_upserts: Vec<i64> = diff.node_ids().copied().collect();
        node_upserts.sort_unstable_by(|a, b| super::osm_id_cmp(*a, *b));
        node_upserts.dedup();

        let mut way_upserts: Vec<i64> = diff.way_ids().copied().collect();
        way_upserts.sort_unstable_by(|a, b| super::osm_id_cmp(*a, *b));
        way_upserts.dedup();

        let mut rel_upserts: Vec<i64> = diff.relation_ids().copied().collect();
        rel_upserts.sort_unstable_by(|a, b| super::osm_id_cmp(*a, *b));
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
    /// result means the blob *might* need rewriting — it still gets a secondary
    /// check via `block_overlaps_diff` after full parsing. A false result means
    /// the blob is safe for raw passthrough (no diff IDs in its range at all).
    fn range_overlaps(&self, kind: ElemKind, min_id: i64, max_id: i64) -> bool {
        let ids = match kind {
            ElemKind::Node => &self.node_ids,
            ElemKind::Way => &self.way_ids,
            ElemKind::Relation => &self.rel_ids,
        };
        if ids.is_empty() {
            return false;
        }
        // Binary search for the first ID >= blob's OSM-first in OSM order
        let first = super::blob_osm_first_key(min_id, max_id);
        let last = super::blob_osm_last_key(min_id, max_id);
        let pos = ids.partition_point(|&id| super::osm_id_key(id) < first);
        pos < ids.len() && super::osm_id_key(ids[pos]) <= last
    }

    /// Return the sorted upsert (create/modify) IDs for a given element kind.
    fn upserts(&self, kind: ElemKind) -> &[i64] {
        match kind {
            ElemKind::Node => &self.node_upserts,
            ElemKind::Way => &self.way_upserts,
            ElemKind::Relation => &self.rel_upserts,
        }
    }


}

// osc_member_type_to_member_type removed: OscRelMember.member_type is now
// a MemberType enum directly (see osc.rs), so no string→enum conversion needed.

// ---------------------------------------------------------------------------
// Per-type upsert cursor tracking
// ---------------------------------------------------------------------------

/// Grouped per-type cursors tracking how far through each upsert vector
/// we have emitted creates. Replaces three bare `usize` variables.
struct UpsertCursors {
    node: usize,
    way: usize,
    rel: usize,
}

impl UpsertCursors {
    fn new() -> Self {
        Self { node: 0, way: 0, rel: 0 }
    }

    /// Mutable cursor + upsert slice for the given element kind.
    fn get_mut<'a>(&mut self, kind: ElemKind, ranges: &'a DiffRanges) -> (&mut usize, &'a [i64]) {
        match kind {
            ElemKind::Node => (&mut self.node, ranges.upserts(ElemKind::Node)),
            ElemKind::Way => (&mut self.way, ranges.upserts(ElemKind::Way)),
            ElemKind::Relation => (&mut self.rel, ranges.upserts(ElemKind::Relation)),
        }
    }

    /// Immutable cursor value + upsert slice for the given element kind.
    fn get<'a>(&self, kind: ElemKind, ranges: &'a DiffRanges) -> (usize, &'a [i64]) {
        match kind {
            ElemKind::Node => (self.node, ranges.upserts(ElemKind::Node)),
            ElemKind::Way => (self.way, ranges.upserts(ElemKind::Way)),
            ElemKind::Relation => (self.rel, ranges.upserts(ElemKind::Relation)),
        }
    }
}

/// Estimate a blob's in-flight memory cost for byte-budgeted batch sizing.
///
/// For indexed blobs whose ID range doesn't overlap the diff, returns just
/// the raw frame size (pure passthrough — no decompression needed).
/// For potential rewrite blobs, returns raw_size × 21 (raw + ~16× decompressed
/// + ~5× rewrite output estimate).
fn estimate_blob_cost(frame: &RawBlobFrame, ranges: &DiffRanges) -> usize {
    let raw = frame.frame_bytes.len();
    if let Some(ref idx) = frame.index
        && !ranges.range_overlaps(idx.kind, idx.min_id, idx.max_id)
    {
        return raw;
    }
    raw * 21
}

// ---------------------------------------------------------------------------
// Quick-scan: check if a block has any IDs that overlap the diff
// ---------------------------------------------------------------------------

/// Check if any element *actually in the block* has a matching ID in the diff.
/// Returns true if the block needs re-encoding, false for safe passthrough.
///
/// This is the secondary, precise overlap check. It runs after `classify_blob`
/// returned `MayOverlap` (the coarse range check found diff IDs within the
/// blob's [min_id, max_id]). This function iterates actual element IDs in the
/// parsed block and checks them against the diff's HashMap/HashSet.
///
/// **Key distinction from `range_overlaps`**: a diff with only pure creates
/// (new IDs not present in the base PBF) can cause `range_overlaps` to return
/// true (the create IDs fall within the blob's range), but this function
/// returns false (no element in the block has a matching diff ID). In that
/// case the blob is passed through raw, and the creates are emitted afterward
/// by the gap-create logic, which means they may appear out of strict ID order
/// relative to the passthrough block. This is intentional — rewriting an
/// otherwise unaffected block just to interleave pure creates would be wasted
/// work. OSM consumers handle non-strictly-sorted IDs across block boundaries.
fn block_overlaps_diff(block: &PrimitiveBlock, diff: &CompactDiffOverlay) -> bool {
    for element in block.elements_skip_metadata() {
        let dominated = match &element {
            Element::DenseNode(dn) => {
                let id = dn.id();
                diff.deleted_nodes.contains(&id) || diff.has_node(id)
            }
            Element::Node(n) => {
                let id = n.id();
                diff.deleted_nodes.contains(&id) || diff.has_node(id)
            }
            Element::Way(w) => {
                let id = w.id();
                diff.deleted_ways.contains(&id) || diff.has_way(id)
            }
            Element::Relation(r) => {
                let id = r.id();
                diff.deleted_relations.contains(&id) || diff.has_relation(id)
            }
        };
        if dominated {
            return true;
        }
    }
    false
}

use super::{
    dense_node_raw_metadata, element_raw_metadata, ensure_node_capacity, ensure_relation_capacity,
    ensure_way_capacity, flush_block,
};

// ---------------------------------------------------------------------------
// Writing OSC elements (from diff, no metadata)
// ---------------------------------------------------------------------------

fn write_osc_way(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    way: &crate::osc::CompactWayRef<'_>,
    loc_map: Option<&FxHashMap<i64, (i32, i32)>>,
    stats: &mut MergeStats,
) -> Result<()> {
    ensure_way_capacity(bb, writer)?;
    let tags: Vec<(&str, &str)> = way.tags().collect();
    let refs: Vec<i64> = way.refs().collect();
    if let Some(locs) = loc_map {
        let mut locations: Vec<(i32, i32)> = Vec::with_capacity(refs.len());
        for &node_id in &refs {
            match locs.get(&node_id) {
                Some(&loc) => locations.push(loc),
                None => {
                    stats.loc_missing += 1;
                    locations.push((0, 0));
                }
            }
        }
        bb.add_way_with_locations(way.id(), &tags, &refs, &locations, None);
    } else {
        bb.add_way(way.id(), &tags, &refs, None);
    }
    Ok(())
}

fn write_osc_relation(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    rel: &crate::osc::CompactRelationRef<'_>,
) -> Result<()> {
    ensure_relation_capacity(bb, writer)?;
    let tags: Vec<(&str, &str)> = rel.tags().collect();
    let members: Vec<MemberData<'_>> = rel
        .members()
        .map(|(mt, ref_id, role)| MemberData {
            id: crate::MemberId::from_id_and_type(ref_id, mt),
            role,
        })
        .collect();
    bb.add_relation(rel.id(), &tags, &members, None);
    Ok(())
}

// ---------------------------------------------------------------------------
// Writing base elements for parallel rewrite (local flush, no PbfWriter)
// ---------------------------------------------------------------------------

fn write_base_dense_node_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    dn: &crate::DenseNode<'_>,
    block: &PrimitiveBlock,
) -> Result<()> {
    ensure_node_capacity_local(bb, output)?;
    if !bb.is_pre_seeded() {
        flush_local(bb, output)?;
        bb.pre_seed_string_table(block);
    }
    let meta = dense_node_raw_metadata(dn);
    bb.add_node_raw(
        dn.id(),
        dn.decimicro_lat(),
        dn.decimicro_lon(),
        dn.raw_tags(),
        meta.as_ref(),
    );
    Ok(())
}

fn write_base_node_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    node: &crate::Node<'_>,
    block: &PrimitiveBlock,
) -> Result<()> {
    ensure_node_capacity_local(bb, output)?;
    if !bb.is_pre_seeded() {
        flush_local(bb, output)?;
        bb.pre_seed_string_table(block);
    }
    let meta = element_raw_metadata(&node.info());
    bb.add_node_raw(
        node.id(),
        node.decimicro_lat(),
        node.decimicro_lon(),
        node.raw_tags().map(|(k, v)| (k.cast_signed(), v.cast_signed())),
        meta.as_ref(),
    );
    Ok(())
}

fn write_base_way_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    way: &crate::Way<'_>,
    block: &PrimitiveBlock,
) -> Result<()> {
    ensure_way_capacity_local(bb, output)?;
    if !bb.is_pre_seeded() {
        flush_local(bb, output)?;
        bb.pre_seed_string_table(block);
    }
    bb.add_way_raw_bytes(
        way.id(),
        way.keys_data(),
        way.vals_data(),
        way.refs_data(),
        way.info_data(),
    );
    Ok(())
}

/// Write a surviving base way with LocationsOnWays data preserved.
///
/// Like `write_base_way_local` but also forwards raw `lat_data`/`lon_data` bytes.
fn write_base_way_local_with_locations(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    way: &crate::Way<'_>,
    block: &PrimitiveBlock,
) -> Result<()> {
    ensure_way_capacity_local(bb, output)?;
    if !bb.is_pre_seeded() {
        flush_local(bb, output)?;
        bb.pre_seed_string_table(block);
    }
    bb.add_way_raw_bytes_with_locations(
        way.id(),
        way.keys_data(),
        way.vals_data(),
        way.refs_data(),
        way.lat_data(),
        way.lon_data(),
        way.info_data(),
    );
    Ok(())
}

/// Write an OSC way with optional LocationsOnWays coordinate lookup.
fn write_osc_way_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    way: &crate::osc::CompactWayRef<'_>,
    loc_map: Option<&FxHashMap<i64, (i32, i32)>>,
    stats: &mut MergeStats,
) -> Result<()> {
    ensure_way_capacity_local(bb, output)?;
    let tags: Vec<(&str, &str)> = way.tags().collect();
    let refs: Vec<i64> = way.refs().collect();

    if let Some(locs) = loc_map {
        let mut locations: Vec<(i32, i32)> = Vec::with_capacity(refs.len());
        for &node_id in &refs {
            match locs.get(&node_id) {
                Some(&loc) => locations.push(loc),
                None => {
                    stats.loc_missing += 1;
                    locations.push((0, 0));
                }
            }
        }
        bb.add_way_with_locations(way.id(), &tags, &refs, &locations, None);
    } else {
        bb.add_way(way.id(), &tags, &refs, None);
    }
    Ok(())
}

fn write_base_relation_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    rel: &crate::Relation<'_>,
    block: &PrimitiveBlock,
) -> Result<()> {
    ensure_relation_capacity_local(bb, output)?;
    if !bb.is_pre_seeded() {
        flush_local(bb, output)?;
        bb.pre_seed_string_table(block);
    }
    bb.add_relation_raw_bytes(
        rel.id(),
        rel.keys_data(),
        rel.vals_data(),
        rel.roles_sid_data(),
        rel.memids_data(),
        rel.types_data(),
        rel.info_data(),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Header handling
// ---------------------------------------------------------------------------

#[allow(clippy::redundant_closure_for_method_calls)]
fn build_header_bytes(
    header: &crate::HeaderBlock,
    locations_on_ways: bool,
    overrides: &HeaderOverrides,
) -> Result<Vec<u8>> {
    if locations_on_ways {
        if !header.has_locations_on_ways() {
            return Err(
                "merge --locations-on-ways requires the base PBF to have LocationsOnWays. \
                 Run add-locations-to-ways first to bootstrap coordinates."
                    .into(),
            );
        }
        if !header.is_sorted() {
            return Err(
                "merge --locations-on-ways requires a sorted base PBF (Sort.Type_then_ID). \
                 All nodes must precede all ways in the file."
                    .into(),
            );
        }
        build_output_header(header, false, overrides, |hb| {
            hb.sorted().optional_feature("LocationsOnWays")
        })
    } else {
        super::warn_locations_on_ways_loss(header);
        build_output_header(header, false, overrides, |hb| hb.sorted())
    }
}

/// Read the OSMHeader blob from a base PBF and return rebuilt header bytes.
fn read_header(
    base_pbf: &Path,
    direct_io: bool,
    locations_on_ways: bool,
    overrides: &HeaderOverrides,
) -> Result<Vec<u8>> {
    let mut reader = FileReader::open(base_pbf, direct_io)?;
    let mut offset: u64 = 0;
    loop {
        match read_raw_frame(&mut reader, &mut offset)? {
            Some(frame) if frame.blob_type == BlobKind::OsmHeader => {
                let header = decode_blob_to_headerblock(frame.blob_bytes())?;
                return build_header_bytes(&header, locations_on_ways, overrides);
            }
            Some(_) => {}
            None => return Err("base PBF has no OSMHeader blob".into()),
        }
    }
}

/// Spawn a reader thread that streams raw data frames over a bounded channel.
/// Skips the OSMHeader blob and any non-OsmData blobs.
fn spawn_reader_thread(
    base_pbf: &Path,
    direct_io: bool,
) -> (
    std::thread::JoinHandle<std::result::Result<(), String>>,
    mpsc::Receiver<RawBlobFrame>,
) {
    let base_path = base_pbf.to_path_buf();
    let (frame_tx, frame_rx) = mpsc::sync_channel::<RawBlobFrame>(READER_CHANNEL_SIZE);
    let handle = std::thread::spawn(move || -> std::result::Result<(), String> {
        let mut reader = FileReader::open(&base_path, direct_io).map_err(|e| e.to_string())?;
        let mut file_offset: u64 = 0;
        let mut past_header = false;
        while let Some(frame) =
            read_raw_frame(&mut reader, &mut file_offset).map_err(|e| e.to_string())?
        {
            if frame.blob_type == BlobKind::OsmHeader {
                past_header = true;
                continue;
            }
            if !past_header || frame.blob_type != BlobKind::OsmData {
                continue;
            }
            if frame_tx.send(frame).is_err() {
                break;
            }
        }
        Ok(())
    });
    (handle, frame_rx)
}

/// Collect a byte-budgeted batch of raw frames from the reader channel.
/// Returns the estimated in-flight byte cost of the batch.
fn collect_batch(
    frame_rx: &mpsc::Receiver<RawBlobFrame>,
    ranges: &DiffRanges,
    batch: &mut Vec<RawBlobFrame>,
) -> usize {
    batch.clear();
    let mut batch_bytes: usize = 0;
    while batch.len() < BATCH_MAX_BLOBS {
        if batch.len() >= BATCH_MIN_BLOBS && batch_bytes >= BATCH_BYTE_BUDGET {
            break;
        }
        match frame_rx.try_recv() {
            Ok(frame) => {
                batch_bytes += estimate_blob_cost(&frame, ranges);
                batch.push(frame);
            }
            Err(mpsc::TryRecvError::Empty) => {
                if batch.is_empty() {
                    match frame_rx.recv() {
                        Ok(frame) => {
                            batch_bytes += estimate_blob_cost(&frame, ranges);
                            batch.push(frame);
                        }
                        Err(_) => break,
                    }
                } else {
                    break;
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => break,
        }
    }
    batch_bytes
}

// ---------------------------------------------------------------------------
// Process an affected data block (has diff overlap — re-encode)
// ---------------------------------------------------------------------------

fn element_kind(element: &Element<'_>) -> ElemKind {
    match element {
        Element::DenseNode(_) | Element::Node(_) => ElemKind::Node,
        Element::Way(_) => ElemKind::Way,
        Element::Relation(_) => ElemKind::Relation,
    }
}

// ---------------------------------------------------------------------------
// Parallel rewrite: rewrite a block without PbfWriter or CreateEmitter
// ---------------------------------------------------------------------------

/// Output from `rewrite_block_parallel`: serialized blocks + local stats.
struct RewriteOutput {
    blocks: Vec<OwnedBlock>,
    stats: MergeStats,
}

/// Emit a single create element into the local BlockBuilder.
#[allow(clippy::too_many_arguments)]
fn emit_create_local(
    id: i64,
    kind: ElemKind,
    diff: &CompactDiffOverlay,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    stats: &mut MergeStats,
    loc_map: Option<&FxHashMap<i64, (i32, i32)>>,
) -> Result<()> {
    match kind {
        ElemKind::Node => {
            if let Some(osc) = diff.get_node(id) {
                ensure_node_capacity_local(bb, output)?;
                let tags: Vec<(&str, &str)> = osc.tags().collect();
                bb.add_node(osc.id(), osc.decimicro_lat(), osc.decimicro_lon(), &tags, None);
                stats.diff_nodes += 1;
            }
        }
        ElemKind::Way => {
            if let Some(osc) = diff.get_way(id) {
                write_osc_way_local(bb, output, &osc, loc_map, stats)?;
                stats.diff_ways += 1;
            }
        }
        ElemKind::Relation => {
            if let Some(osc) = diff.get_relation(id) {
                ensure_relation_capacity_local(bb, output)?;
                let tags: Vec<(&str, &str)> = osc.tags().collect();
                let members: Vec<MemberData<'_>> = osc
                    .members()
                    .map(|(mt, ref_id, role)| MemberData {
                        id: crate::MemberId::from_id_and_type(ref_id, mt),
                        role,
                    })
                    .collect();
                bb.add_relation(osc.id(), &tags, &members, None);
                stats.diff_relations += 1;
            }
        }
    }
    Ok(())
}

/// Rewrite a block in parallel: same element-by-element logic as `rewrite_block`,
/// but flushes to local `Vec<Vec<u8>>` instead of `PbfWriter`. Interleaves
/// upserts at their sorted positions within the block — IDs that match base
/// elements are modifications (handled by normal element processing); IDs that
/// don't match are creates (emitted by the cursor).
#[allow(clippy::too_many_lines)]
#[hotpath::measure]
fn rewrite_block_parallel(
    block: &PrimitiveBlock,
    diff: &CompactDiffOverlay,
    bb: &mut BlockBuilder,
    inline_upserts: &[i64],
    kind: ElemKind,
    loc_map: Option<&FxHashMap<i64, (i32, i32)>>,
) -> Result<RewriteOutput> {
    let mut output: Vec<OwnedBlock> = Vec::new();
    let mut stats = MergeStats::new();
    let mut upsert_cursor: usize = 0;

    bb.pre_seed_string_table(block);

    for element in block.elements() {
        let elem_id = match &element {
            Element::DenseNode(dn) => dn.id(),
            Element::Node(n) => n.id(),
            Element::Way(w) => w.id(),
            Element::Relation(r) => r.id(),
        };

        // Emit creates (upsert IDs not in base block) before this element
        while upsert_cursor < inline_upserts.len() && super::osm_id_cmp(inline_upserts[upsert_cursor], elem_id).is_lt() {
            let cid = inline_upserts[upsert_cursor];
            upsert_cursor += 1;
            emit_create_local(cid, kind, diff, bb, &mut output, &mut stats, loc_map)?;
        }
        // Skip modification IDs (handled below by normal element processing)
        if upsert_cursor < inline_upserts.len() && inline_upserts[upsert_cursor] == elem_id {
            upsert_cursor += 1;
        }

        match &element {
            Element::DenseNode(dn) => {
                let id = dn.id();
                if diff.deleted_nodes.contains(&id) {
                    stats.deleted += 1;
                } else if let Some(osc) = diff.get_node(id) {
                    ensure_node_capacity_local(bb, &mut output)?;
                    let tags: Vec<(&str, &str)> = osc.tags().collect();
                    bb.add_node(osc.id(), osc.decimicro_lat(), osc.decimicro_lon(), &tags, None);

                    stats.diff_nodes += 1;
                } else {
                    write_base_dense_node_local(bb, &mut output, dn, block)?;
                    stats.base_nodes += 1;
                }
            }
            Element::Node(n) => {
                let id = n.id();
                if diff.deleted_nodes.contains(&id) {
                    stats.deleted += 1;
                } else if let Some(osc) = diff.get_node(id) {
                    ensure_node_capacity_local(bb, &mut output)?;
                    let tags: Vec<(&str, &str)> = osc.tags().collect();
                    bb.add_node(osc.id(), osc.decimicro_lat(), osc.decimicro_lon(), &tags, None);

                    stats.diff_nodes += 1;
                } else {
                    write_base_node_local(bb, &mut output, n, block)?;
                    stats.base_nodes += 1;
                }
            }
            Element::Way(w) => {
                let id = w.id();
                if diff.deleted_ways.contains(&id) {
                    stats.deleted += 1;
                } else if let Some(osc) = diff.get_way(id) {
                    write_osc_way_local(bb, &mut output, &osc, loc_map, &mut stats)?;
                    stats.diff_ways += 1;
                } else if loc_map.is_some() {
                    // Forward existing raw lat/lon data for LocationsOnWays
                    write_base_way_local_with_locations(bb, &mut output, w, block)?;
                    stats.base_ways += 1;
                } else {
                    write_base_way_local(bb, &mut output, w, block)?;
                    stats.base_ways += 1;
                }
            }
            Element::Relation(r) => {
                let id = r.id();
                if diff.deleted_relations.contains(&id) {
                    stats.deleted += 1;
                } else if let Some(osc) = diff.get_relation(id) {
                    ensure_relation_capacity_local(bb, &mut output)?;
                    let tags: Vec<(&str, &str)> = osc.tags().collect();
                    let members: Vec<MemberData<'_>> = osc
                        .members()
                        .map(|(mt, ref_id, role)| MemberData {
                            id: crate::MemberId::from_id_and_type(ref_id, mt),
                            role,
                        })
                        .collect();
                    bb.add_relation(osc.id(), &tags, &members, None);

                    stats.diff_relations += 1;
                } else {
                    write_base_relation_local(bb, &mut output, r, block)?;
                    stats.base_relations += 1;
                }
            }
        }
    }

    // Emit remaining upserts after the last element (trailing creates)
    while upsert_cursor < inline_upserts.len() {
        let cid = inline_upserts[upsert_cursor];
        upsert_cursor += 1;
        emit_create_local(cid, kind, diff, bb, &mut output, &mut stats, loc_map)?;
    }

    // Flush remaining elements in the BlockBuilder
    flush_local(bb, &mut output)?;

    Ok(RewriteOutput {
        blocks: output,
        stats,
    })
}

// ---------------------------------------------------------------------------
// Single-pass classification types
// ---------------------------------------------------------------------------

/// Classification result from Phase 1 parallel classify.
enum ClassifyResult {
    /// No diff overlap — raw passthrough.
    Passthrough(BlobIndex, bool),
    /// Range overlapped but no element affected — raw passthrough.
    FalsePositive(BlobIndex, bool),
    /// At least one element affected — needs rewrite.
    NeedsRewrite(PrimitiveBlock, BlobIndex),
}

/// Per-blob slot in the batch pipeline.
enum BatchSlot {
    Passthrough { index: BlobIndex, has_indexdata: bool },
    FalsePositive { index: BlobIndex, has_indexdata: bool },
    Rewrite { job_index: usize, index: BlobIndex },
}

/// A rewrite job for Phase 3 parallel processing.
struct RewriteJob {
    block: PrimitiveBlock,
    kind: ElemKind,
    upsert_range: (usize, usize),
}

// ---------------------------------------------------------------------------
// Phase 1: classify_only
// ---------------------------------------------------------------------------

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
fn classify_only(
    frame: &RawBlobFrame,
    ranges: &DiffRanges,
    diff: &CompactDiffOverlay,
    buf: &mut Vec<u8>,
) -> std::result::Result<ClassifyResult, String> {
    let has_indexdata = frame.index.is_some();

    // Fast path: use inline index from BlobHeader indexdata (no decompression).
    if let Some(ref idx) = frame.index
        && !ranges.range_overlaps(idx.kind, idx.min_id, idx.max_id)
    {
        return Ok(ClassifyResult::Passthrough(*idx, has_indexdata));
    }

    // Slow path: decompress + lightweight scan.
    decompress_blob_data_into(frame.blob_bytes(), buf).map_err(|e| e.to_string())?;

    let scan = if let Some(scan) = blob_index::scan_block_ids(buf) {
        if !ranges.range_overlaps(scan.kind, scan.min_id, scan.max_id) {
            return Ok(ClassifyResult::Passthrough(scan, has_indexdata));
        }
        Some(scan)
    } else {
        None
    };

    // Range overlaps — full parse + precise check.
    let raw = std::mem::take(buf);
    let block =
        parse_primitive_block_from_bytes_owned(&Bytes::from(raw)).map_err(|e| e.to_string())?;

    let index = scan.unwrap_or_else(|| fallback_index(&block));

    if !block_overlaps_diff(&block, diff) {
        return Ok(ClassifyResult::FalsePositive(index, has_indexdata));
    }

    Ok(ClassifyResult::NeedsRewrite(block, index))
}

// ---------------------------------------------------------------------------
// Gap-create emitter for Phase 4 sequential output
// ---------------------------------------------------------------------------

/// Emit a single create element via PbfWriter (for gap creates and trailing creates).
#[allow(clippy::too_many_arguments)]
fn emit_create_for_output(
    id: i64,
    kind: ElemKind,
    diff: &CompactDiffOverlay,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc_map: Option<&FxHashMap<i64, (i32, i32)>>,
) -> Result<()> {
    match kind {
        ElemKind::Node => {
            if let Some(osc) = diff.get_node(id) {
                ensure_node_capacity(bb, writer)?;
                let tags: Vec<(&str, &str)> = osc.tags().collect();
                bb.add_node(osc.id(), osc.decimicro_lat(), osc.decimicro_lon(), &tags, None);
                stats.diff_nodes += 1;
            }
        }
        ElemKind::Way => {
            if let Some(osc) = diff.get_way(id) {
                write_osc_way(bb, writer, &osc, loc_map, stats)?;
                stats.diff_ways += 1;
            }
        }
        ElemKind::Relation => {
            if let Some(osc) = diff.get_relation(id) {
                write_osc_relation(bb, writer, &osc)?;
                stats.diff_relations += 1;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Public merge function
// ---------------------------------------------------------------------------

/// Apply an OSC diff to a base PBF file, producing an updated sorted PBF.
///
/// Single-pass streaming batch pipeline: for each byte-budgeted batch of raw frames,
/// Phase 1 classifies blobs in parallel, Phase 2 computes inline upsert
/// assignments (O(log n) per blob), then Phase 3+4 spawns parallel rewrites and
/// streams output in file order as results arrive.
///
/// # Errors
///
/// Returns an error if the base PBF or OSC file cannot be read, the output
/// file cannot be written, or if any PBF parsing/encoding fails.
/// Options controlling merge I/O and compression behavior.
pub struct MergeOptions {
    pub compression: Compression,
    pub direct_io: bool,
    pub io_uring: bool,
    pub force: bool,
    pub locations_on_ways: bool,
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity, clippy::cast_precision_loss)]
#[hotpath::measure]
pub fn merge(
    base_pbf: &Path,
    osc_file: &Path,
    output_pbf: &Path,
    opts: &MergeOptions,
    overrides: &HeaderOverrides,
) -> Result<MergeStats> {
    let MergeOptions { compression, direct_io, io_uring, force, locations_on_ways } = *opts;
    require_indexdata(base_pbf, direct_io, force,
        "base PBF has no blob-level indexdata. Without indexdata, every blob must be \
         decompressed to classify elements (significantly slower).")?;

    // Step 1: Parse the diff
    crate::debug::emit_marker("MERGE_DIFFPARSE_START");
    #[cfg(feature = "hotpath")]
    let osc_start = std::time::Instant::now();
    eprintln!("Parsing OSC diff: {}", osc_file.display());
    let diff = Arc::new(parse_osc_file(osc_file)?);
    eprintln!(
        "Diff: {} nodes, {} ways, {} relations ({} del nodes, {} del ways, {} del rels)",
        diff.node_count(), diff.way_count(), diff.relation_count(),
        diff.deleted_nodes.len(), diff.deleted_ways.len(), diff.deleted_relations.len(),
    );
    let diff_heap_bytes = diff.heap_size_estimate() as u64;
    eprintln!(
        "CompactDiffOverlay heap estimate: {:.1} MB",
        diff_heap_bytes as f64 / (1024.0 * 1024.0),
    );
    #[cfg(feature = "hotpath")]
    let mut phase_timers = PhaseTimers::new();
    #[cfg(feature = "hotpath")]
    {
        phase_timers.osc_parse = osc_start.elapsed();
    }
    #[cfg(feature = "hotpath")]
    let mut phase_rss = PhaseRss::new();
    #[cfg(feature = "hotpath")]
    {
        phase_rss.after_osc_parse = read_rss_kb();
    }

    crate::debug::emit_marker("MERGE_DIFFPARSE_END");

    // Step 2: Pre-compute sorted ID ranges for fast overlap checking
    let ranges = Arc::new(DiffRanges::from_diff(&diff));
    eprintln!(
        "Diff ID ranges: {} node IDs, {} way IDs, {} rel IDs",
        ranges.node_ids.len(), ranges.way_ids.len(), ranges.rel_ids.len(),
    );

    // Step 2.5: Build sparse node location index for --locations-on-ways
    let mut loc_index = if locations_on_ways {
        let idx = NodeLocationIndex::build_from_diff(&diff);
        Some(idx)
    } else {
        None
    };

    // Step 3: Read header from base PBF (for writer setup)
    let header_bytes = read_header(base_pbf, direct_io, locations_on_ways, overrides)?;

    // Step 4: Create pipelined writer
    let mut writer = writer_from_header_bytes(
        output_pbf,
        compression,
        &header_bytes,
        direct_io,
        io_uring,
    )?;

    // Step 5: Spawn reader thread with read-ahead
    crate::debug::emit_marker("MERGE_LOOP_START");
    let (reader_thread, frame_rx) = spawn_reader_thread(base_pbf, direct_io);

    // Open second handle for copy_file_range.
    // The main thread owns the primary FileReader; this handle provides the fd
    // for kernel-space copy (copy_file_range uses explicit offsets, thread-safe).
    #[cfg(feature = "linux-direct-io")]
    let (_copy_fd_file, input_fd, use_copy_range) = {
        let f = FileReader::buffered(base_pbf)?;
        let fd = f.raw_fd();
        (f, fd, io_uring || !direct_io)
    };
    #[cfg(not(feature = "linux-direct-io"))]
    let (_input_fd, _use_copy_range) = (0i32, false);

    let mut bb = BlockBuilder::new();
    let mut stats = MergeStats::new();
    stats.diff_heap_bytes = diff_heap_bytes;
    let mut blob_count: u64 = 0;

    let mut cursors = UpsertCursors::new();
    let mut last_type: Option<ElemKind> = None;

    let mut batch: Vec<RawBlobFrame> = Vec::with_capacity(BATCH_MAX_BLOBS);
    // Passthrough coalescing buffer: accumulates consecutive raw passthrough bytes
    // and flushes them as a single write_raw_owned (move, no copy) to the
    // pipelined writer. At ~92% passthrough (Denmark), this collapses thousands
    // of individual channel sends into far fewer.
    let mut passthrough_chunks: Vec<Vec<u8>> = Vec::new();

    loop {
        let batch_bytes = collect_batch(&frame_rx, &ranges, &mut batch);
        if batch.is_empty() {
            break;
        }

        // Phase 1: Parallel classify
        #[cfg(feature = "hotpath")]
        let phase1_start = std::time::Instant::now();
        let classify_results: Vec<std::result::Result<ClassifyResult, String>> = batch
            .par_iter()
            .map_init(
                Vec::new,
                |buf, frame| classify_only(frame, &ranges, &diff, buf),
            )
            .collect();
        #[cfg(feature = "hotpath")]
        {
            phase_timers.classify_total += phase1_start.elapsed();
            let rss = read_rss_kb();
            if rss > phase_rss.classify_max {
                phase_rss.classify_max = rss;
            }
        }

        // Phase 2: Sequential inline upsert assignment (O(log n) per blob)
        let mut slots: Vec<BatchSlot> = Vec::with_capacity(batch.len());
        let mut rewrite_jobs: Vec<RewriteJob> = Vec::new();

        for result in classify_results {
            let result = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            match result {
                ClassifyResult::Passthrough(index, has_indexdata) => {
                    slots.push(BatchSlot::Passthrough { index, has_indexdata });
                }
                ClassifyResult::FalsePositive(index, has_indexdata) => {
                    slots.push(BatchSlot::FalsePositive { index, has_indexdata });
                }
                ClassifyResult::NeedsRewrite(block, index) => {
                    // Binary search for inline upserts in blob's OSM-order range
                    let upserts = ranges.upserts(index.kind);
                    let first = super::blob_osm_first_key(index.min_id, index.max_id);
                    let last = super::blob_osm_last_key(index.min_id, index.max_id);
                    let start = upserts.partition_point(|&id| super::osm_id_key(id) < first);
                    let end = upserts[start..].partition_point(|&id| super::osm_id_key(id) <= last) + start;

                    let job_idx = rewrite_jobs.len();
                    rewrite_jobs.push(RewriteJob {
                        block,
                        kind: index.kind,
                        upsert_range: (start, end),
                    });
                    slots.push(BatchSlot::Rewrite { job_index: job_idx, index });
                }
            }
        }

        // Phase 2.5: Extract node coordinates for --locations-on-ways.
        // Must complete BEFORE Phase 3 spawns way rewrites that read the index.
        if let Some(ref mut idx) = loc_index
            && !idx.all_found()
        {
                // First pass: extract from rewrite blobs (already parsed, cheap)
                for slot in &slots {
                    let blob_index = match slot {
                        BatchSlot::Passthrough { index, .. }
                        | BatchSlot::FalsePositive { index, .. }
                        | BatchSlot::Rewrite { index, .. } => index,
                    };
                    if blob_index.kind != ElemKind::Node
                        || !idx.overlaps_needed(blob_index.min_id, blob_index.max_id)
                    {
                        continue;
                    }
                    if let BatchSlot::Rewrite { job_index, .. } = slot {
                        let found = idx.extract_from_block(&rewrite_jobs[*job_index].block);
                        stats.loc_nodes_from_base += found;
                    }
                }

                // Second pass: parallel decompress+extract from passthrough node blobs
                let passthrough_scan: Vec<usize> = slots
                    .iter()
                    .enumerate()
                    .filter(|(_, slot)| {
                        let blob_index = match slot {
                            BatchSlot::Passthrough { index, .. }
                            | BatchSlot::FalsePositive { index, .. }
                            | BatchSlot::Rewrite { index, .. } => index,
                        };
                        blob_index.kind == ElemKind::Node
                            && idx.overlaps_needed(blob_index.min_id, blob_index.max_id)
                            && !matches!(slot, BatchSlot::Rewrite { .. })
                    })
                    .map(|(i, _)| i)
                    .collect();

                if !passthrough_scan.is_empty() {
                    let needed_set = &idx.needed_set;
                    // Node-only scanner: extract (id, lat, lon) without PrimitiveBlock
                    // construction. Skips string table and group_ranges allocation.
                    let extracted: Vec<Vec<(i64, (i32, i32))>> = passthrough_scan
                        .par_iter()
                        .filter_map(|&i| {
                            let mut buf = Vec::new();
                            if decompress_blob_data_into(batch[i].blob_bytes(), &mut buf).is_err()
                            {
                                return None;
                            }
                            let mut tuples = Vec::new();
                            if super::node_scanner::extract_node_tuples(&buf, &mut tuples).is_err()
                            {
                                return None;
                            }
                            let found: Vec<(i64, (i32, i32))> = tuples
                                .iter()
                                .filter(|t| needed_set.contains(&t.id))
                                .map(|t| (t.id, (t.lat, t.lon)))
                                .collect();
                            Some(found)
                        })
                        .collect();

                    for coords in extracted {
                        for (id, loc) in coords {
                            idx.locations.insert(id, loc);
                            idx.needed_set.remove(&id);
                            stats.loc_nodes_from_base += 1;
                        }
                    }
                    stats.loc_node_blobs_scanned += passthrough_scan.len() as u64;
                }
        }

        // Snapshot the location index for rewrite tasks that need it (way blobs).
        // Only clones when this batch has way rewrites and the flag is on.
        let loc_snapshot: Option<Arc<FxHashMap<i64, (i32, i32)>>> =
            if loc_index.is_some()
                && rewrite_jobs.iter().any(|j| j.kind == ElemKind::Way)
            {
                loc_index.as_ref().map(|idx| Arc::new(idx.locations.clone()))
            } else {
                None
            };

        // Phase 3+4: Spawn parallel rewrites, then stream output in file order.
        // Each rayon task owns its RewriteJob (including PrimitiveBlock), freeing
        // memory as soon as the task completes rather than holding all blocks until
        // all rewrites finish. The main thread processes slots in order, receiving
        // rewrite results from the channel on demand.
        #[cfg(feature = "hotpath")]
        let phase34_start = std::time::Instant::now();

        let rewrite_count = rewrite_jobs.len();
        let (rewrite_tx, rewrite_rx) =
            mpsc::sync_channel::<(usize, std::result::Result<RewriteOutput, String>)>(
                rayon::current_num_threads().min(rewrite_count.max(1)),
            );

        for (job_idx, job) in rewrite_jobs.into_iter().enumerate() {
            let tx = rewrite_tx.clone();
            let diff_clone = Arc::clone(&diff);
            let ranges_clone = Arc::clone(&ranges);
            let loc_clone = if job.kind == ElemKind::Way { loc_snapshot.clone() } else { None };
            rayon::spawn(move || {
                let mut task_bb = BlockBuilder::new();
                let upserts = ranges_clone.upserts(job.kind);
                let inline_slice = &upserts[job.upsert_range.0..job.upsert_range.1];
                let result = rewrite_block_parallel(
                    &job.block,
                    &diff_clone,
                    &mut task_bb,
                    inline_slice,
                    job.kind,
                    loc_clone.as_deref(),
                )
                .map_err(|e| e.to_string());
                // job (PrimitiveBlock) dropped here — freed before other tasks finish
                drop(tx.send((job_idx, result)));
            });
        }
        drop(rewrite_tx); // close channel when all cloned senders are done

        // Streaming drain: process slots in file order, receiving rewrite results
        // from the channel as needed. Out-of-order arrivals are buffered in
        // `received` and consumed when their slot is reached.
        let mut received: Vec<Option<RewriteOutput>> =
            (0..rewrite_count).map(|_| None).collect();

        for (i, slot) in slots.iter().enumerate() {
            blob_count += 1;

            let (blob_kind, min_id, max_id) = match slot {
                BatchSlot::Passthrough { index, .. }
                | BatchSlot::FalsePositive { index, .. }
                | BatchSlot::Rewrite { index, .. } => {
                    (index.kind, index.min_id, index.max_id)
                }
            };

            // Handle type transitions: flush remaining upserts of previous type(s)
            if let Some(prev) = last_type
                && prev != blob_kind
            {
                flush_passthrough_buf(&mut passthrough_chunks, &mut writer)?;
                let lm = loc_index.as_ref().map(|idx| &idx.locations);
                flush_remaining_upserts(
                    prev, blob_kind, &ranges, &diff,
                    &mut cursors, &mut bb, &mut writer, &mut stats, lm,
                )?;
            }
            last_type = Some(blob_kind);

            // Gap creates: emit upserts before this blob in OSM order
            let osm_first = super::blob_osm_first_id(min_id, max_id);
            let has_gap = has_gap_creates(blob_kind, osm_first, &ranges, &cursors);
            if has_gap {
                flush_passthrough_buf(&mut passthrough_chunks, &mut writer)?;
                let lm = loc_index.as_ref().map(|idx| &idx.locations);
                emit_gap_creates(
                    blob_kind, osm_first, &ranges,
                    &diff, &mut cursors, &mut bb, &mut writer, &mut stats, lm,
                )?;
                flush_block(&mut bb, &mut writer)?;
            }

            match slot {
                BatchSlot::Passthrough { index, has_indexdata }
                | BatchSlot::FalsePositive { index, has_indexdata } => {
                    // Coalesce: append raw frame bytes to passthrough buffer.
                    // For indexed blobs, take the frame bytes (zero-copy move).
                    // For non-indexed blobs, reframe with indexdata first.
                    #[cfg(feature = "linux-direct-io")]
                    if use_copy_range {
                        // copy_file_range path: flush coalesced buffer first,
                        // then do kernel-space copy (can't coalesce across copy_file_range)
                        flush_passthrough_buf(&mut passthrough_chunks, &mut writer)?;
                        writer.write_raw_copy(
                            input_fd,
                            batch[i].file_offset,
                            batch[i].frame_bytes.len() as u64,
                        )?;
                    }
                    #[cfg(feature = "linux-direct-io")]
                    if !use_copy_range {
                        coalesce_passthrough(
                            &mut batch[i], index, *has_indexdata,
                            &mut passthrough_chunks,
                        )?;
                    }
                    #[cfg(not(feature = "linux-direct-io"))]
                    coalesce_passthrough(
                        &mut batch[i], index, *has_indexdata,
                        &mut passthrough_chunks,
                    )?;

                    if matches!(slot, BatchSlot::Passthrough { has_indexdata: true, .. }) {
                        stats.blobs_index_hit += 1;
                    } else if matches!(slot, BatchSlot::Passthrough { .. }) {
                        stats.blobs_scan_only += 1;
                    }
                    match index.kind {
                        ElemKind::Node => stats.base_nodes += index.count,
                        ElemKind::Way => stats.base_ways += index.count,
                        ElemKind::Relation => stats.base_relations += index.count,
                    }
                    stats.blobs_passthrough += 1;
                    let frame_len = batch[i].frame_bytes.len() as u64;
                    stats.bytes_passthrough += frame_len;
                    #[allow(clippy::cast_possible_truncation)]
                    stats.blob_sizes.push(frame_len as u32);
                }
                BatchSlot::Rewrite { job_index, index: _ } => {
                    // Wait for this rewrite result, buffering out-of-order arrivals
                    while received[*job_index].is_none() {
                        let (idx, result) = rewrite_rx.recv()
                            .map_err(|_| -> Box<dyn std::error::Error> {
                                "rewrite channel closed unexpectedly".into()
                            })?;
                        received[idx] = Some(
                            result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?,
                        );
                    }
                    flush_passthrough_buf(&mut passthrough_chunks, &mut writer)?;
                    let mut output = received[*job_index]
                        .take()
                        .ok_or("rewrite output missing")?;
                    let mut rewrite_bytes: u64 = 0;
                    for (block_bytes, index, tagdata) in output.blocks.drain(..) {
                        rewrite_bytes += block_bytes.len() as u64;
                        writer.write_primitive_block_owned(block_bytes, index, tagdata.as_deref())?;
                    }
                    stats.bytes_rewritten += rewrite_bytes;
                    stats.merge_from(&output.stats);
                    stats.blobs_rewritten += 1;
                    // output dropped here — RewriteOutput freed immediately

                    // Advance cursor past blob's OSM-last (inline upserts handled by rewrite)
                    let last = super::blob_osm_last_key(min_id, max_id);
                    let (cursor, upserts) = cursors.get_mut(blob_kind, &ranges);
                    while *cursor < upserts.len() && super::osm_id_key(upserts[*cursor]) <= last {
                        *cursor += 1;
                    }
                }
            }

            #[allow(clippy::cast_precision_loss)]
            if blob_count.is_multiple_of(500) {
                eprintln!(
                    "  Blob {blob_count}: {} pass ({} idx) / {} rewrite, {} elements, batch={} ({:.1} MB est)",
                    stats.blobs_passthrough, stats.blobs_index_hit,
                    stats.blobs_rewritten, stats.total_elements(),
                    batch.len(), batch_bytes as f64 / (1024.0 * 1024.0),
                );
            }
        }

        // Flush any remaining coalesced passthrough bytes at batch boundary
        flush_passthrough_buf(&mut passthrough_chunks, &mut writer)?;
        #[cfg(feature = "hotpath")]
        {
            let elapsed = phase34_start.elapsed();
            phase_timers.rewrite_total += elapsed;
            phase_timers.output_total += elapsed;
            let rss = read_rss_kb();
            if rss > phase_rss.rewrite_max {
                phase_rss.rewrite_max = rss;
            }
            if rss > phase_rss.output_max {
                phase_rss.output_max = rss;
            }
        }
    }

    // Join reader thread (should already be done since channel is drained)
    reader_thread
        .join()
        .map_err(|_| "reader thread panicked")?
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // Trailing creates: flush remaining upserts per type.
    // When last_type is None (no blobs at all), cursors are at 0 so all types
    // are flushed in full — equivalent to the previous dedicated else-branch.
    #[cfg(feature = "hotpath")]
    let trailing_start = std::time::Instant::now();
    let types_to_flush = match last_type {
        None | Some(ElemKind::Node) => &[ElemKind::Node, ElemKind::Way, ElemKind::Relation][..],
        Some(ElemKind::Way) => &[ElemKind::Way, ElemKind::Relation][..],
        Some(ElemKind::Relation) => &[ElemKind::Relation][..],
    };
    for &kind in types_to_flush {
        let (cursor, upserts) = cursors.get_mut(kind, &ranges);
        while *cursor < upserts.len() {
            let lm = loc_index.as_ref().map(|idx| &idx.locations);
            emit_create_for_output(upserts[*cursor], kind, &diff, &mut bb, &mut writer, &mut stats, lm)?;
            *cursor += 1;
        }
        flush_block(&mut bb, &mut writer)?;
    }

    #[cfg(feature = "hotpath")]
    {
        phase_timers.trailing_creates = trailing_start.elapsed();
    }

    writer.flush()?;
    crate::debug::emit_marker("MERGE_LOOP_END");
    #[cfg(feature = "hotpath")]
    {
        phase_rss.after_flush = read_rss_kb();
    }
    // Populate location stats from the index (if active)
    if let Some(ref idx) = loc_index {
        stats.loc_nodes_needed = idx.locations.len() as u64 + idx.needed_set.len() as u64;
        stats.loc_nodes_from_diff =
            stats.loc_nodes_needed - stats.loc_nodes_from_base - idx.needed_set.len() as u64;
    }

    stats.print_summary();

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("merge_blobs_passthrough", stats.blobs_passthrough as i64);
        crate::debug::emit_counter("merge_blobs_rewritten", stats.blobs_rewritten as i64);
        crate::debug::emit_counter("merge_total_elements", stats.total_elements() as i64);
        crate::debug::emit_counter("merge_deleted", stats.deleted as i64);
        crate::debug::emit_counter("merge_diff_nodes", stats.diff_nodes as i64);
        crate::debug::emit_counter("merge_diff_ways", stats.diff_ways as i64);
        crate::debug::emit_counter("merge_diff_relations", stats.diff_relations as i64);
    }

    #[cfg(feature = "hotpath")]
    {
        eprintln!("osc_parse_ms={}", phase_timers.osc_parse.as_millis());
        eprintln!("classify_total_ms={}", phase_timers.classify_total.as_millis());
        eprintln!("rewrite_total_ms={}", phase_timers.rewrite_total.as_millis());
        eprintln!("output_total_ms={}", phase_timers.output_total.as_millis());
        eprintln!("trailing_creates_ms={}", phase_timers.trailing_creates.as_millis());
        eprintln!("phase_rss_after_osc_kb={}", phase_rss.after_osc_parse);
        eprintln!("phase_rss_classify_max_kb={}", phase_rss.classify_max);
        eprintln!("phase_rss_rewrite_max_kb={}", phase_rss.rewrite_max);
        eprintln!("phase_rss_output_max_kb={}", phase_rss.output_max);
        eprintln!("phase_rss_after_flush_kb={}", phase_rss.after_flush);
    }

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Helpers extracted from merge() to keep cognitive complexity down
// ---------------------------------------------------------------------------

/// Flush remaining upserts for the previous element type during a type
/// transition. Also handles skipped types (e.g., Node -> Relation flushes
/// all Way upserts).
#[allow(clippy::too_many_arguments)]
fn flush_remaining_upserts(
    prev: ElemKind,
    next: ElemKind,
    ranges: &DiffRanges,
    diff: &CompactDiffOverlay,
    cursors: &mut UpsertCursors,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc_map: Option<&FxHashMap<i64, (i32, i32)>>,
) -> Result<()> {
    // Flush remaining creates of the previous type
    let (cursor, upserts) = cursors.get_mut(prev, ranges);
    while *cursor < upserts.len() {
        emit_create_for_output(upserts[*cursor], prev, diff, bb, writer, stats, loc_map)?;
        *cursor += 1;
    }
    flush_block(bb, writer)?;

    // Handle skipped type: Node -> Relation (flush all Way upserts)
    if prev == ElemKind::Node && next == ElemKind::Relation {
        let (cursor, upserts) = cursors.get_mut(ElemKind::Way, ranges);
        while *cursor < upserts.len() {
            emit_create_for_output(upserts[*cursor], ElemKind::Way, diff, bb, writer, stats, loc_map)?;
            *cursor += 1;
        }
        flush_block(bb, writer)?;
    }

    Ok(())
}

/// Emit gap creates: upsert IDs of the current type that fall before a blob's min_id.
#[allow(clippy::too_many_arguments)]
fn emit_gap_creates(
    blob_kind: ElemKind,
    min_id: i64,
    ranges: &DiffRanges,
    diff: &CompactDiffOverlay,
    cursors: &mut UpsertCursors,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
    loc_map: Option<&FxHashMap<i64, (i32, i32)>>,
) -> Result<()> {
    let (cursor, upserts) = cursors.get_mut(blob_kind, ranges);
    while *cursor < upserts.len() && super::osm_id_cmp(upserts[*cursor], min_id).is_lt() {
        emit_create_for_output(upserts[*cursor], blob_kind, diff, bb, writer, stats, loc_map)?;
        *cursor += 1;
    }
    Ok(())
}

/// Append a passthrough blob's raw bytes to the coalescing buffer.
/// For indexed blobs, moves frame_bytes via std::mem::take (zero copy).
/// For non-indexed blobs, reframes with indexdata first.
fn coalesce_passthrough(
    frame: &mut RawBlobFrame,
    index: &BlobIndex,
    has_indexdata: bool,
    chunks: &mut Vec<Vec<u8>>,
) -> Result<()> {
    if has_indexdata {
        chunks.push(std::mem::take(&mut frame.frame_bytes));
    } else {
        let indexdata = index.serialize();
        let reframed = crate::write::writer::reframe_raw_with_index(
            frame.blob_bytes(),
            &indexdata,
            frame.tagdata.as_deref(),
        )?;
        chunks.push(reframed);
    }
    Ok(())
}

/// Check whether there are gap creates to emit before min_id (without mutating cursors).
fn has_gap_creates(
    blob_kind: ElemKind,
    min_id: i64,
    ranges: &DiffRanges,
    cursors: &UpsertCursors,
) -> bool {
    let (cursor, upserts) = cursors.get(blob_kind, ranges);
    cursor < upserts.len() && super::osm_id_cmp(upserts[cursor], min_id).is_lt()
}
