//! PBF merge: apply an OSC diff overlay to a base PBF, producing an updated PBF.
//!
//! Single-pass 4-phase batch pipeline:
//!   Phase 1: Parallel classify       [rayon pool]
//!   Phase 2: Sequential inline assign [main thread, O(log n) per blob]
//!   Phase 3: Parallel rewrite        [rayon pool]
//!   Phase 4: Sequential output       [main thread]
//!
//! Key insight: we pass ALL upsert IDs in a blob's range to the rewrite function.
//! IDs that match base elements are modifications (handled by normal element processing);
//! IDs that don't match are creates (emitted by the cursor). This eliminates the need
//! for a separate pass to collect modification IDs and compute create lists.

use std::io::{self, Read};
use std::path::Path;
use std::sync::mpsc;

use rayon::prelude::*;

use crate::blob::{
    decode_blob_to_headerblock, decompress_blob_data_into, parse_blob_header_with_index,
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

type MergeResult<T> = Result<T, Box<dyn std::error::Error>>;

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
        node_ids.sort_unstable();
        node_ids.dedup();

        let mut way_ids: Vec<i64> = diff
            .way_ids()
            .chain(diff.deleted_ways.iter())
            .copied()
            .collect();
        way_ids.sort_unstable();
        way_ids.dedup();

        let mut rel_ids: Vec<i64> = diff
            .relation_ids()
            .chain(diff.deleted_relations.iter())
            .copied()
            .collect();
        rel_ids.sort_unstable();
        rel_ids.dedup();

        let mut node_upserts: Vec<i64> = diff.node_ids().copied().collect();
        node_upserts.sort_unstable();
        node_upserts.dedup();

        let mut way_upserts: Vec<i64> = diff.way_ids().copied().collect();
        way_upserts.sort_unstable();
        way_upserts.dedup();

        let mut rel_upserts: Vec<i64> = diff.relation_ids().copied().collect();
        rel_upserts.sort_unstable();
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
        // Binary search for the first ID >= min_id
        let pos = ids.partition_point(|&id| id < min_id);
        pos < ids.len() && ids[pos] <= max_id
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

/// Estimate a blob's in-flight memory cost for byte-budgeted batch sizing.
///
/// For indexed blobs whose ID range doesn't overlap the diff, returns just
/// the raw frame size (pure passthrough — no decompression needed).
/// For potential rewrite blobs, returns raw_size × 21 (raw + ~16× decompressed
/// + ~5× rewrite output estimate).
fn estimate_blob_cost(frame: &RawBlobFrame, ranges: &DiffRanges) -> usize {
    let raw = frame.frame_bytes.len();
    if let Some(ref idx) = frame.index {
        if !ranges.range_overlaps(idx.kind, idx.min_id, idx.max_id) {
            return raw;
        }
    }
    raw * 21
}

// ---------------------------------------------------------------------------
// Low-level blob frame reader (preserves raw bytes for passthrough)
// ---------------------------------------------------------------------------

/// A raw blob frame: the complete `[4-byte len][BlobHeader][Blob]` bytes,
/// plus the parsed header type string.
///
/// The Blob protobuf bytes are a suffix of `frame_bytes` starting at
/// `blob_offset`, eliminating a separate ~55 KB allocation per blob.
struct RawBlobFrame {
    /// Complete framed bytes suitable for write_raw().
    frame_bytes: Vec<u8>,
    blob_type: BlobKind,
    /// Byte offset within `frame_bytes` where the Blob protobuf starts.
    blob_offset: usize,
    /// Blob-level index from BlobHeader indexdata, if present.
    /// When available, classify_blob can skip decompression entirely.
    index: Option<BlobIndex>,
    /// Per-blob tag key data from BlobHeader field 4, if present.
    /// Preserved during passthrough so tag metadata survives merges.
    tagdata: Option<Box<[u8]>>,
    /// Byte offset of this frame in the input file (for copy_file_range).
    #[cfg_attr(not(feature = "linux-direct-io"), allow(dead_code))]
    file_offset: u64,
}

impl RawBlobFrame {
    /// The raw Blob protobuf message bytes (for selective decoding).
    fn blob_bytes(&self) -> &[u8] {
        &self.frame_bytes[self.blob_offset..]
    }
}

/// Read the next raw blob frame from the reader.
/// Returns None at EOF. Updates `file_offset` to track position for copy_file_range.
#[hotpath::measure]
fn read_raw_frame<R: Read>(
    reader: &mut R,
    file_offset: &mut u64,
) -> MergeResult<Option<RawBlobFrame>> {
    let frame_start = *file_offset;

    // Read 4-byte header length
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let header_len = u32::from_be_bytes(len_buf) as usize;

    // Read BlobHeader bytes
    let mut header_bytes = vec![0u8; header_len];
    reader.read_exact(&mut header_bytes)?;

    // Parse type + datasize + optional indexdata + optional tagdata
    let (blob_type, data_size, raw_index, tagdata) = parse_blob_header_with_index(&header_bytes)?;
    let index = raw_index.and_then(|ref data| BlobIndex::deserialize(data));

    // Assemble the complete frame, reading blob data directly into it.
    // This avoids a separate ~55 KB blob_bytes allocation per blob.
    let blob_offset = 4 + header_len;
    let frame_len = blob_offset + data_size;
    *file_offset += frame_len as u64;
    let mut frame_bytes = vec![0u8; frame_len];
    frame_bytes[..4].copy_from_slice(&len_buf);
    frame_bytes[4..blob_offset].copy_from_slice(&header_bytes);
    reader.read_exact(&mut frame_bytes[blob_offset..])?;

    Ok(Some(RawBlobFrame {
        frame_bytes,
        blob_type,
        blob_offset,
        index,
        tagdata,
        file_offset: frame_start,
    }))
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
    for element in block.elements() {
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

use super::{dense_node_raw_metadata, element_raw_metadata, flush_block};

// ---------------------------------------------------------------------------
// Block flushing helpers
// ---------------------------------------------------------------------------

fn ensure_node_capacity(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> MergeResult<()> {
    if !bb.can_add_node() {
        flush_block(bb, writer)?;
    }
    Ok(())
}

fn ensure_way_capacity(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> MergeResult<()> {
    if !bb.can_add_way() {
        flush_block(bb, writer)?;
    }
    Ok(())
}

fn ensure_relation_capacity(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> MergeResult<()> {
    if !bb.can_add_relation() {
        flush_block(bb, writer)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Local flush helpers for parallel rewrite (no PbfWriter)
// ---------------------------------------------------------------------------

fn flush_local(bb: &mut BlockBuilder, output: &mut Vec<OwnedBlock>) -> MergeResult<()> {
    if let Some(triple) = bb.take_owned()? {
        output.push(triple);
    }
    Ok(())
}

fn ensure_node_capacity_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> MergeResult<()> {
    if !bb.can_add_node() {
        flush_local(bb, output)?;
    }
    Ok(())
}

fn ensure_way_capacity_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> MergeResult<()> {
    if !bb.can_add_way() {
        flush_local(bb, output)?;
    }
    Ok(())
}

fn ensure_relation_capacity_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> MergeResult<()> {
    if !bb.can_add_relation() {
        flush_local(bb, output)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Writing OSC elements (from diff, no metadata)
// ---------------------------------------------------------------------------

fn write_osc_way(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    way: &crate::osc::CompactWayRef<'_>,
) -> MergeResult<()> {
    ensure_way_capacity(bb, writer)?;
    let tags: Vec<(&str, &str)> = way.tags().collect();
    let refs: Vec<i64> = way.refs().collect();
    bb.add_way(way.id(), &tags, &refs, None);
    Ok(())
}

fn write_osc_relation(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    rel: &crate::osc::CompactRelationRef<'_>,
) -> MergeResult<()> {
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
) -> MergeResult<()> {
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
) -> MergeResult<()> {
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
) -> MergeResult<()> {
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

fn write_base_relation_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    rel: &crate::Relation<'_>,
    block: &PrimitiveBlock,
) -> MergeResult<()> {
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

fn build_header_bytes(header: &crate::HeaderBlock) -> MergeResult<Vec<u8>> {
    Ok(crate::block_builder::HeaderBuilder::from_header(header)
        .sorted()
        .build()?)
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
fn emit_create_local(
    id: i64,
    kind: ElemKind,
    diff: &CompactDiffOverlay,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    stats: &mut MergeStats,
) -> MergeResult<()> {
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
                ensure_way_capacity_local(bb, output)?;
                let tags: Vec<(&str, &str)> = osc.tags().collect();
                let refs: Vec<i64> = osc.refs().collect();
                bb.add_way(osc.id(), &tags, &refs, None);
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
) -> MergeResult<RewriteOutput> {
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
        while upsert_cursor < inline_upserts.len() && inline_upserts[upsert_cursor] < elem_id {
            let cid = inline_upserts[upsert_cursor];
            upsert_cursor += 1;
            emit_create_local(cid, kind, diff, bb, &mut output, &mut stats)?;
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
                    ensure_way_capacity_local(bb, &mut output)?;
                    let tags: Vec<(&str, &str)> = osc.tags().collect();
                    let refs: Vec<i64> = osc.refs().collect();
                    bb.add_way(osc.id(), &tags, &refs, None);

                    stats.diff_ways += 1;
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
        emit_create_local(cid, kind, diff, bb, &mut output, &mut stats)?;
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
    inline_upserts: Vec<i64>,
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
) -> Result<ClassifyResult, String> {
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
fn emit_create_for_output(
    id: i64,
    kind: ElemKind,
    diff: &CompactDiffOverlay,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
) -> MergeResult<()> {
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
                write_osc_way(bb, writer, &osc)?;
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
/// Single-pass 4-phase batch pipeline: for each byte-budgeted batch of raw frames,
/// Phase 1 classifies blobs in parallel, Phase 2 computes inline upsert
/// assignments (O(log n) per blob), Phase 3 rewrites affected blobs in parallel,
/// and Phase 4 emits output sequentially.
///
/// # Errors
///
/// Returns an error if the base PBF or OSC file cannot be read, the output
/// file cannot be written, or if any PBF parsing/encoding fails.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity, clippy::cast_precision_loss)]
#[hotpath::measure]
pub fn merge(
    base_pbf: &Path,
    osc_file: &Path,
    output_pbf: &Path,
    compression: Compression,
    direct_io: bool,
    io_uring: bool,
    sqpoll: bool,
) -> MergeResult<MergeStats> {
    // Step 1: Parse the diff
    #[cfg(feature = "hotpath")]
    let osc_start = std::time::Instant::now();
    eprintln!("Parsing OSC diff: {}", osc_file.display());
    let diff = parse_osc_file(osc_file)?;
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

    // Step 2: Pre-compute sorted ID ranges for fast overlap checking
    let ranges = DiffRanges::from_diff(&diff);
    eprintln!(
        "Diff ID ranges: {} node IDs, {} way IDs, {} rel IDs",
        ranges.node_ids.len(), ranges.way_ids.len(), ranges.rel_ids.len(),
    );

    // Step 3: Read header from base PBF (for writer setup)
    let header_bytes = {
        let mut reader = FileReader::open(base_pbf, direct_io)?;
        let mut offset: u64 = 0;
        loop {
            match read_raw_frame(&mut reader, &mut offset)? {
                Some(frame) if frame.blob_type == BlobKind::OsmHeader => {
                    let header = decode_blob_to_headerblock(frame.blob_bytes())?;
                    break build_header_bytes(&header)?;
                }
                Some(_) => {}
                None => return Err("base PBF has no OSMHeader blob".into()),
            }
        }
    };

    // Step 4: Create pipelined writer
    let mut writer = if io_uring {
        #[cfg(feature = "linux-io-uring")]
        {
            PbfWriter::to_path_pipelined_uring(output_pbf, compression, &header_bytes, sqpoll)?
        }
        #[cfg(not(feature = "linux-io-uring"))]
        {
            let _ = sqpoll;
            return Err("--io-uring requires the linux-io-uring feature".into());
        }
    } else if direct_io {
        #[cfg(feature = "linux-direct-io")]
        {
            PbfWriter::to_path_pipelined_direct(
                output_pbf,
                compression,
                &header_bytes,
            )?
        }
        #[cfg(not(feature = "linux-direct-io"))]
        {
            return Err("--direct-io requires the linux-direct-io feature".into());
        }
    } else {
        PbfWriter::to_path_pipelined(output_pbf, compression, &header_bytes)?
    };

    // Step 5: Spawn reader thread with read-ahead
    // Decouples read I/O from processing — while the main thread runs
    // classify/rewrite/output on the current batch, the reader thread
    // pre-fills the next batch.
    const BATCH_BYTE_BUDGET: usize = 128 * 1024 * 1024;
    const BATCH_MIN_BLOBS: usize = 8;
    const BATCH_MAX_BLOBS: usize = 128;
    const READER_CHANNEL_SIZE: usize = 128;
    let base_path = base_pbf.to_path_buf();
    let (frame_tx, frame_rx) = mpsc::sync_channel::<RawBlobFrame>(READER_CHANNEL_SIZE);
    let reader_thread = std::thread::spawn(move || -> Result<(), String> {
        let mut reader = FileReader::open(&base_path, direct_io).map_err(|e| e.to_string())?;
        let mut file_offset: u64 = 0;
        let mut past_header = false;
        while let Some(frame) = read_raw_frame(&mut reader, &mut file_offset).map_err(|e| e.to_string())? {
            if frame.blob_type == BlobKind::OsmHeader {
                past_header = true;
                continue;
            }
            if !past_header || frame.blob_type != BlobKind::OsmData {
                continue;
            }
            if frame_tx.send(frame).is_err() {
                break; // receiver dropped
            }
        }
        Ok(())
    });

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

    // Per-type cursors on upsert vectors for gap create tracking
    let mut node_upsert_cursor: usize = 0;
    let mut way_upsert_cursor: usize = 0;
    let mut rel_upsert_cursor: usize = 0;
    let mut last_type: Option<ElemKind> = None;

    let mut batch: Vec<RawBlobFrame> = Vec::with_capacity(BATCH_MAX_BLOBS);
    // Passthrough coalescing buffer: accumulates consecutive raw passthrough bytes
    // and flushes them as a single write_raw_owned (move, no copy) to the
    // pipelined writer. At ~92% passthrough (Denmark), this collapses thousands
    // of individual channel sends into far fewer.
    let mut passthrough_buf: Vec<u8> = Vec::new();

    loop {
        // Receive batch from reader thread (byte-budgeted)
        batch.clear();
        let mut batch_bytes: usize = 0;
        while batch.len() < BATCH_MAX_BLOBS {
            if batch.len() >= BATCH_MIN_BLOBS && batch_bytes >= BATCH_BYTE_BUDGET {
                break;
            }
            match frame_rx.try_recv() {
                Ok(frame) => {
                    batch_bytes += estimate_blob_cost(&frame, &ranges);
                    batch.push(frame);
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if batch.is_empty() {
                        // Nothing yet — block for the first frame
                        match frame_rx.recv() {
                            Ok(frame) => {
                                batch_bytes += estimate_blob_cost(&frame, &ranges);
                                batch.push(frame);
                            }
                            Err(_) => break, // reader done
                        }
                    } else {
                        break; // partial batch, proceed
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        if batch.is_empty() {
            break;
        }

        // Phase 1: Parallel classify
        #[cfg(feature = "hotpath")]
        let phase1_start = std::time::Instant::now();
        let classify_results: Vec<Result<ClassifyResult, String>> = batch
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
                    // Binary search for inline upserts in [min_id, max_id]
                    let upserts = ranges.upserts(index.kind);
                    let start = upserts.partition_point(|&id| id < index.min_id);
                    let end = upserts[start..].partition_point(|&id| id <= index.max_id) + start;
                    let inline_upserts = upserts[start..end].to_vec();

                    let job_idx = rewrite_jobs.len();
                    rewrite_jobs.push(RewriteJob {
                        block,
                        kind: index.kind,
                        inline_upserts,
                    });
                    slots.push(BatchSlot::Rewrite { job_index: job_idx, index });
                }
            }
        }

        // Phase 3: Parallel rewrite
        #[cfg(feature = "hotpath")]
        let phase3_start = std::time::Instant::now();
        let rewrite_results: Vec<Result<RewriteOutput, String>> = rewrite_jobs
            .par_iter()
            .map_init(
                BlockBuilder::new,
                |thread_bb, job| {
                    rewrite_block_parallel(
                        &job.block,
                        &diff,
                        thread_bb,
                        &job.inline_upserts,
                        job.kind,
                    )
                    .map_err(|e| e.to_string())
                },
            )
            .collect();

        #[cfg(feature = "hotpath")]
        {
            phase_timers.rewrite_total += phase3_start.elapsed();
            let rss = read_rss_kb();
            if rss > phase_rss.rewrite_max {
                phase_rss.rewrite_max = rss;
            }
        }

        // Unwrap rewrite results
        let mut rewrite_outputs: Vec<RewriteOutput> = Vec::with_capacity(rewrite_results.len());
        for result in rewrite_results {
            rewrite_outputs.push(
                result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?,
            );
        }

        // Phase 4: Sequential output with passthrough coalescing
        #[cfg(feature = "hotpath")]
        let phase4_start = std::time::Instant::now();
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
                flush_passthrough_buf(&mut passthrough_buf, &mut writer)?;
                flush_remaining_upserts(
                    prev, blob_kind, &ranges, &diff,
                    &mut node_upsert_cursor, &mut way_upsert_cursor, &mut rel_upsert_cursor,
                    &mut bb, &mut writer, &mut stats,
                )?;
            }
            last_type = Some(blob_kind);

            // Gap creates: emit upserts with ID < this blob's min_id
            let has_gap = has_gap_creates(
                blob_kind, min_id, &ranges,
                &node_upsert_cursor, &way_upsert_cursor, &rel_upsert_cursor,
            );
            if has_gap {
                flush_passthrough_buf(&mut passthrough_buf, &mut writer)?;
                emit_gap_creates(
                    blob_kind, min_id, &ranges,
                    &diff, &mut node_upsert_cursor, &mut way_upsert_cursor,
                    &mut rel_upsert_cursor, &mut bb, &mut writer, &mut stats,
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
                        flush_passthrough_buf(&mut passthrough_buf, &mut writer)?;
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
                            &mut passthrough_buf,
                        )?;
                    }
                    #[cfg(not(feature = "linux-direct-io"))]
                    coalesce_passthrough(
                        &mut batch[i], index, *has_indexdata,
                        &mut passthrough_buf,
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
                    flush_passthrough_buf(&mut passthrough_buf, &mut writer)?;
                    let output = &mut rewrite_outputs[*job_index];
                    let mut rewrite_bytes: u64 = 0;
                    for (block_bytes, index, tagdata) in output.blocks.drain(..) {
                        rewrite_bytes += block_bytes.len() as u64;
                        writer.write_primitive_block_owned(block_bytes, index, tagdata.as_deref())?;
                    }
                    stats.bytes_rewritten += rewrite_bytes;
                    stats.merge_from(&output.stats);
                    stats.blobs_rewritten += 1;

                    // Advance cursor past max_id (inline upserts handled by rewrite)
                    let (cursor, upserts) = match blob_kind {
                        ElemKind::Node => (&mut node_upsert_cursor, ranges.upserts(ElemKind::Node)),
                        ElemKind::Way => (&mut way_upsert_cursor, ranges.upserts(ElemKind::Way)),
                        ElemKind::Relation => (&mut rel_upsert_cursor, ranges.upserts(ElemKind::Relation)),
                    };
                    while *cursor < upserts.len() && upserts[*cursor] <= max_id {
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
        flush_passthrough_buf(&mut passthrough_buf, &mut writer)?;
        #[cfg(feature = "hotpath")]
        {
            phase_timers.output_total += phase4_start.elapsed();
            let rss = read_rss_kb();
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

    // Trailing creates: flush remaining upserts per type
    #[cfg(feature = "hotpath")]
    let trailing_start = std::time::Instant::now();
    if let Some(prev) = last_type {
        let types_to_flush = match prev {
            ElemKind::Node => &[ElemKind::Node, ElemKind::Way, ElemKind::Relation][..],
            ElemKind::Way => &[ElemKind::Way, ElemKind::Relation][..],
            ElemKind::Relation => &[ElemKind::Relation][..],
        };
        for &kind in types_to_flush {
            let (cursor, upserts) = match kind {
                ElemKind::Node => (&mut node_upsert_cursor, ranges.upserts(ElemKind::Node)),
                ElemKind::Way => (&mut way_upsert_cursor, ranges.upserts(ElemKind::Way)),
                ElemKind::Relation => (&mut rel_upsert_cursor, ranges.upserts(ElemKind::Relation)),
            };
            while *cursor < upserts.len() {
                emit_create_for_output(upserts[*cursor], kind, &diff, &mut bb, &mut writer, &mut stats)?;
                *cursor += 1;
            }
            flush_block(&mut bb, &mut writer)?;
        }
    } else {
        // No blobs at all — emit all creates
        for kind in [ElemKind::Node, ElemKind::Way, ElemKind::Relation] {
            for &id in ranges.upserts(kind) {
                emit_create_for_output(id, kind, &diff, &mut bb, &mut writer, &mut stats)?;
            }
            flush_block(&mut bb, &mut writer)?;
        }
    }

    #[cfg(feature = "hotpath")]
    {
        phase_timers.trailing_creates = trailing_start.elapsed();
    }

    writer.flush()?;
    #[cfg(feature = "hotpath")]
    {
        phase_rss.after_flush = read_rss_kb();
    }
    stats.print_summary();

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
    node_cursor: &mut usize,
    way_cursor: &mut usize,
    rel_cursor: &mut usize,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
) -> MergeResult<()> {
    // Flush remaining creates of the previous type
    let (cursor, upserts) = match prev {
        ElemKind::Node => (&mut *node_cursor, ranges.upserts(ElemKind::Node)),
        ElemKind::Way => (&mut *way_cursor, ranges.upserts(ElemKind::Way)),
        ElemKind::Relation => (&mut *rel_cursor, ranges.upserts(ElemKind::Relation)),
    };
    while *cursor < upserts.len() {
        emit_create_for_output(upserts[*cursor], prev, diff, bb, writer, stats)?;
        *cursor += 1;
    }
    flush_block(bb, writer)?;

    // Handle skipped type: Node -> Relation (flush all Way upserts)
    if prev == ElemKind::Node && next == ElemKind::Relation {
        let way_upserts = ranges.upserts(ElemKind::Way);
        while *way_cursor < way_upserts.len() {
            emit_create_for_output(way_upserts[*way_cursor], ElemKind::Way, diff, bb, writer, stats)?;
            *way_cursor += 1;
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
    node_cursor: &mut usize,
    way_cursor: &mut usize,
    rel_cursor: &mut usize,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    stats: &mut MergeStats,
) -> MergeResult<()> {
    let (cursor, upserts) = match blob_kind {
        ElemKind::Node => (&mut *node_cursor, ranges.upserts(ElemKind::Node)),
        ElemKind::Way => (&mut *way_cursor, ranges.upserts(ElemKind::Way)),
        ElemKind::Relation => (&mut *rel_cursor, ranges.upserts(ElemKind::Relation)),
    };
    while *cursor < upserts.len() && upserts[*cursor] < min_id {
        emit_create_for_output(upserts[*cursor], blob_kind, diff, bb, writer, stats)?;
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
    buf: &mut Vec<u8>,
) -> MergeResult<()> {
    if has_indexdata {
        let bytes = std::mem::take(&mut frame.frame_bytes);
        buf.extend_from_slice(&bytes);
    } else {
        let indexdata = index.serialize();
        let reframed = crate::write::writer::reframe_raw_with_index(
            frame.blob_bytes(),
            &indexdata,
            frame.tagdata.as_deref(),
        )?;
        buf.extend_from_slice(&reframed);
    }
    Ok(())
}

/// Flush coalesced passthrough bytes as a single write_raw_owned (move, no copy).
fn flush_passthrough_buf(
    buf: &mut Vec<u8>,
    writer: &mut PbfWriter<FileWriter>,
) -> MergeResult<()> {
    if !buf.is_empty() {
        writer.write_raw_owned(std::mem::take(buf))?;
    }
    Ok(())
}

/// Check whether there are gap creates to emit before min_id (without mutating cursors).
fn has_gap_creates(
    blob_kind: ElemKind,
    min_id: i64,
    ranges: &DiffRanges,
    node_cursor: &usize,
    way_cursor: &usize,
    rel_cursor: &usize,
) -> bool {
    let (cursor, upserts) = match blob_kind {
        ElemKind::Node => (*node_cursor, ranges.upserts(ElemKind::Node)),
        ElemKind::Way => (*way_cursor, ranges.upserts(ElemKind::Way)),
        ElemKind::Relation => (*rel_cursor, ranges.upserts(ElemKind::Relation)),
    };
    cursor < upserts.len() && upserts[cursor] < min_id
}
