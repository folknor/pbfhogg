//! PBF merge: apply an OSC diff overlay to a base PBF, producing an updated PBF.
//!
//! Optimization: lightweight protobuf scanning extracts element type + ID range
//! from decompressed bytes without full parsing. Blocks outside the diff's ID
//! range are passed through without parsing. Once all element types are past
//! their max affected ID, remaining blobs skip decompression entirely.

use std::collections::HashSet;
use std::io::{self, Read};
use std::path::Path;

use rayon::prelude::*;

use crate::blob::{
    decode_blob_to_headerblock, decompress_blob_data_into, parse_blob_header_with_index,
    parse_primitive_block_from_bytes_owned,
};
use crate::blob_index::{self, BlobIndex, ElemKind};
use bytes::Bytes;
use crate::block_builder::{BlockBuilder, MemberData};
use crate::file_reader::FileReader;
use crate::file_writer::FileWriter;
use crate::osc::{parse_osc_file, DiffOverlay, OscRelMember, OscRelation, OscWay};
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
    }
}

// ---------------------------------------------------------------------------
// Coordinate conversion
// ---------------------------------------------------------------------------

#[allow(clippy::cast_possible_truncation)]
fn to_decimicro(deg: f64) -> i32 {
    (deg * 1e7).round() as i32
}

// ---------------------------------------------------------------------------
// Diff ID ranges for fast overlap checking
// ---------------------------------------------------------------------------

/// Pre-computed sorted ID vectors from the diff, for fast overlap checks.
///
/// These IDs include both upserts (creates + modifies) and deletes. They are
/// used to determine whether a blob's ID range overlaps the diff at all.
///
/// **Important nuance**: `range_overlaps` can return true even when no element
/// *in the base PBF* is affected — e.g. if the diff only contains pure creates
/// with IDs that fall within a blob's [min_id, max_id] range. In that case,
/// `classify_blob` returns `MayOverlap`, but the secondary check
/// `block_overlaps_diff` (which tests actual element IDs in the block against
/// the diff) returns false, and the blob is passed through raw. The creates
/// are then emitted after the passthrough blob by `CreateEmitter`, which means
/// they may appear out of strict ID order relative to the passthrough block.
/// This is intentional — see the comment on `block_overlaps_diff` for details.
struct DiffRanges {
    /// Sorted node IDs affected by the diff (upserts + deletes).
    node_ids: Vec<i64>,
    /// Sorted way IDs affected by the diff (upserts + deletes).
    way_ids: Vec<i64>,
    /// Sorted relation IDs affected by the diff (upserts + deletes).
    rel_ids: Vec<i64>,
}

impl DiffRanges {
    fn from_diff(diff: &DiffOverlay) -> Self {
        let mut node_ids: Vec<i64> = diff
            .nodes
            .keys()
            .chain(diff.deleted_nodes.iter())
            .copied()
            .collect();
        node_ids.sort_unstable();
        node_ids.dedup();

        let mut way_ids: Vec<i64> = diff
            .ways
            .keys()
            .chain(diff.deleted_ways.iter())
            .copied()
            .collect();
        way_ids.sort_unstable();
        way_ids.dedup();

        let mut rel_ids: Vec<i64> = diff
            .relations
            .keys()
            .chain(diff.deleted_relations.iter())
            .copied()
            .collect();
        rel_ids.sort_unstable();
        rel_ids.dedup();

        Self {
            node_ids,
            way_ids,
            rel_ids,
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

    /// Max affected ID for a type, or None if the diff has no IDs of that type.
    fn max_id(&self, kind: ElemKind) -> Option<i64> {
        match kind {
            ElemKind::Node => self.node_ids.last().copied(),
            ElemKind::Way => self.way_ids.last().copied(),
            ElemKind::Relation => self.rel_ids.last().copied(),
        }
    }
}

/// State for tracking whether we've passed the max affected ID for each type.
struct SkipState {
    node_done: bool,
    way_done: bool,
    rel_done: bool,
}

impl SkipState {
    fn new(ranges: &DiffRanges) -> Self {
        Self {
            node_done: ranges.node_ids.is_empty(),
            way_done: ranges.way_ids.is_empty(),
            rel_done: ranges.rel_ids.is_empty(),
        }
    }

    // wontfix(name-is-has-bool): private, reads naturally as "if progress.all_done()"
    fn all_done(&self) -> bool {
        self.node_done && self.way_done && self.rel_done
    }

    fn update(&mut self, kind: ElemKind, max_id_in_block: i64, ranges: &DiffRanges) {
        if let Some(max_affected) = ranges.max_id(kind)
            && max_id_in_block > max_affected
        {
            match kind {
                ElemKind::Node => self.node_done = true,
                ElemKind::Way => self.way_done = true,
                ElemKind::Relation => self.rel_done = true,
            }
        }
    }
}

// osc_member_type_to_member_type removed: OscRelMember.member_type is now
// a MemberType enum directly (see osc.rs), so no string→enum conversion needed.

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
    /// Blob type: "OSMHeader", "OSMData", etc.
    blob_type: String,
    /// Byte offset within `frame_bytes` where the Blob protobuf starts.
    blob_offset: usize,
    /// Blob-level index from BlobHeader indexdata, if present.
    /// When available, classify_blob can skip decompression entirely.
    index: Option<BlobIndex>,
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

    // Parse type + datasize + optional indexdata
    let (blob_type, data_size, raw_index) = parse_blob_header_with_index(&header_bytes)?;
    let index = raw_index.and_then(|data| BlobIndex::deserialize(&data));

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
/// by `CreateEmitter`. This means creates that fall within a passthrough
/// blob's ID range will appear after it in the output, not interleaved at
/// their exact sorted position. This is intentional — rewriting an otherwise
/// unaffected block just to interleave pure creates would be wasted work.
/// OSM consumers handle non-strictly-sorted IDs across block boundaries.
fn block_overlaps_diff(block: &PrimitiveBlock, diff: &DiffOverlay) -> bool {
    for element in block.elements() {
        let dominated = match &element {
            Element::DenseNode(dn) => {
                let id = dn.id();
                diff.deleted_nodes.contains(&id) || diff.nodes.contains_key(&id)
            }
            Element::Node(n) => {
                let id = n.id();
                diff.deleted_nodes.contains(&id) || diff.nodes.contains_key(&id)
            }
            Element::Way(w) => {
                let id = w.id();
                diff.deleted_ways.contains(&id) || diff.ways.contains_key(&id)
            }
            Element::Relation(r) => {
                let id = r.id();
                diff.deleted_relations.contains(&id) || diff.relations.contains_key(&id)
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

fn flush_local(bb: &mut BlockBuilder, output: &mut Vec<Vec<u8>>) -> MergeResult<()> {
    if let Some(bytes) = bb.take()? {
        output.push(bytes.to_vec());
    }
    Ok(())
}

fn ensure_node_capacity_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<Vec<u8>>,
) -> MergeResult<()> {
    if !bb.can_add_node() {
        flush_local(bb, output)?;
    }
    Ok(())
}

fn ensure_way_capacity_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<Vec<u8>>,
) -> MergeResult<()> {
    if !bb.can_add_way() {
        flush_local(bb, output)?;
    }
    Ok(())
}

fn ensure_relation_capacity_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<Vec<u8>>,
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
    way: &OscWay,
) -> MergeResult<()> {
    ensure_way_capacity(bb, writer)?;
    let tags: Vec<(&str, &str)> = way.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    bb.add_way(way.id, &tags, &way.node_refs, None);
    Ok(())
}

fn write_osc_relation(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
    rel: &OscRelation,
) -> MergeResult<()> {
    ensure_relation_capacity(bb, writer)?;
    let tags: Vec<(&str, &str)> = rel.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let members: Vec<MemberData<'_>> = rel
        .members
        .iter()
        .map(|m: &OscRelMember| MemberData {
            id: crate::MemberId::from_id_and_type(m.ref_id, m.member_type),
            role: &m.role,
        })
        .collect();
    bb.add_relation(rel.id, &tags, &members, None);
    Ok(())
}

// ---------------------------------------------------------------------------
// Writing base elements for parallel rewrite (local flush, no PbfWriter)
// ---------------------------------------------------------------------------

fn write_base_dense_node_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<Vec<u8>>,
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
    output: &mut Vec<Vec<u8>>,
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
    output: &mut Vec<Vec<u8>>,
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
    output: &mut Vec<Vec<u8>>,
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
    blocks: Vec<Vec<u8>>,
    stats: MergeStats,
}

/// Emit a single create element into the local BlockBuilder.
fn emit_create_local(
    id: i64,
    kind: ElemKind,
    diff: &DiffOverlay,
    bb: &mut BlockBuilder,
    output: &mut Vec<Vec<u8>>,
    stats: &mut MergeStats,
) -> MergeResult<()> {
    match kind {
        ElemKind::Node => {
            if let Some(osc) = diff.nodes.get(&id) {
                ensure_node_capacity_local(bb, output)?;
                let tags: Vec<(&str, &str)> =
                    osc.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                bb.add_node(osc.id, to_decimicro(osc.lat), to_decimicro(osc.lon), &tags, None);
                stats.diff_nodes += 1;
            }
        }
        ElemKind::Way => {
            if let Some(osc) = diff.ways.get(&id) {
                ensure_way_capacity_local(bb, output)?;
                let tags: Vec<(&str, &str)> =
                    osc.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                bb.add_way(osc.id, &tags, &osc.node_refs, None);
                stats.diff_ways += 1;
            }
        }
        ElemKind::Relation => {
            if let Some(osc) = diff.relations.get(&id) {
                ensure_relation_capacity_local(bb, output)?;
                let tags: Vec<(&str, &str)> =
                    osc.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                let members: Vec<MemberData<'_>> = osc
                    .members
                    .iter()
                    .map(|m: &OscRelMember| MemberData {
                        id: crate::MemberId::from_id_and_type(m.ref_id, m.member_type),
                        role: &m.role,
                    })
                    .collect();
                bb.add_relation(osc.id, &tags, &members, None);
                stats.diff_relations += 1;
            }
        }
    }
    Ok(())
}

/// Rewrite a block in parallel: same element-by-element logic as `rewrite_block`,
/// but flushes to local `Vec<Vec<u8>>` instead of `PbfWriter`. Interleaves
/// pre-assigned creates at their sorted positions within the block.
#[allow(clippy::too_many_lines)]
#[hotpath::measure]
fn rewrite_block_parallel(
    block: &PrimitiveBlock,
    diff: &DiffOverlay,
    bb: &mut BlockBuilder,
    create_ids: &[i64],
    kind: ElemKind,
) -> MergeResult<RewriteOutput> {
    let mut output: Vec<Vec<u8>> = Vec::new();
    let mut stats = MergeStats::new();
    let mut create_cursor: usize = 0;

    bb.pre_seed_string_table(block);

    for element in block.elements() {
        let elem_id = match &element {
            Element::DenseNode(dn) => dn.id(),
            Element::Node(n) => n.id(),
            Element::Way(w) => w.id(),
            Element::Relation(r) => r.id(),
        };

        // Emit pre-assigned creates with ID < this element's ID
        while create_cursor < create_ids.len() && create_ids[create_cursor] < elem_id {
            let cid = create_ids[create_cursor];
            create_cursor += 1;
            emit_create_local(cid, kind, diff, bb, &mut output, &mut stats)?;
        }

        match &element {
            Element::DenseNode(dn) => {
                let id = dn.id();
                if diff.deleted_nodes.contains(&id) {
                    stats.deleted += 1;
                } else if let Some(osc) = diff.nodes.get(&id) {
                    ensure_node_capacity_local(bb, &mut output)?;
                    let tags: Vec<(&str, &str)> =
                        osc.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                    bb.add_node(
                        osc.id,
                        to_decimicro(osc.lat),
                        to_decimicro(osc.lon),
                        &tags,
                        None,
                    );

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
                } else if let Some(osc) = diff.nodes.get(&id) {
                    ensure_node_capacity_local(bb, &mut output)?;
                    let tags: Vec<(&str, &str)> =
                        osc.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                    bb.add_node(
                        osc.id,
                        to_decimicro(osc.lat),
                        to_decimicro(osc.lon),
                        &tags,
                        None,
                    );

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
                } else if let Some(osc) = diff.ways.get(&id) {
                    ensure_way_capacity_local(bb, &mut output)?;
                    let tags: Vec<(&str, &str)> =
                        osc.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                    bb.add_way(osc.id, &tags, &osc.node_refs, None);

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
                } else if let Some(osc) = diff.relations.get(&id) {
                    ensure_relation_capacity_local(bb, &mut output)?;
                    let tags: Vec<(&str, &str)> =
                        osc.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                    let members: Vec<MemberData<'_>> = osc
                        .members
                        .iter()
                        .map(|m: &OscRelMember| MemberData {
                            id: crate::MemberId::from_id_and_type(m.ref_id, m.member_type),
                            role: &m.role,
                        })
                        .collect();
                    bb.add_relation(osc.id, &tags, &members, None);

                    stats.diff_relations += 1;
                } else {
                    write_base_relation_local(bb, &mut output, r, block)?;
                    stats.base_relations += 1;
                }
            }
        }
    }

    // Emit remaining pre-assigned creates after the last element
    while create_cursor < create_ids.len() {
        let cid = create_ids[create_cursor];
        create_cursor += 1;
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
// Sorted output: emit new creates at the correct sorted position
// ---------------------------------------------------------------------------

/// Tracks sorted diff element IDs per type with cursors, emitting new creates
/// (elements in the diff but not in the base) at the correct sorted position
/// in the output stream.
///
/// `emit_before(kind, min_id)` emits all creates with ID < min_id for the
/// given type. This is called before writing each blob (passthrough or
/// rewritten). For passthrough blobs, min_id is the blob's scanned min_id,
/// so creates with smaller IDs are placed before the blob. Creates with IDs
/// *within* a passthrough blob's range are deferred — they get emitted when
/// the next blob arrives (with a higher min_id) or during `flush_all` at EOF.
/// This means they appear after the passthrough blob, not interleaved within
/// it. This out-of-order placement is an accepted trade-off for avoiding
/// unnecessary block rewrites. See `block_overlaps_diff` for details.
struct CreateEmitter {
    node_ids: Vec<i64>,
    way_ids: Vec<i64>,
    rel_ids: Vec<i64>,
    node_cursor: usize,
    way_cursor: usize,
    rel_cursor: usize,
    last_kind: Option<ElemKind>,
}

impl CreateEmitter {
    fn from_diff(diff: &DiffOverlay) -> Self {
        let mut node_ids: Vec<i64> = diff.nodes.keys().copied().collect();
        node_ids.sort_unstable();
        let mut way_ids: Vec<i64> = diff.ways.keys().copied().collect();
        way_ids.sort_unstable();
        let mut rel_ids: Vec<i64> = diff.relations.keys().copied().collect();
        rel_ids.sort_unstable();
        Self {
            node_ids,
            way_ids,
            rel_ids,
            node_cursor: 0,
            way_cursor: 0,
            rel_cursor: 0,
            last_kind: None,
        }
    }

    /// Emit new creates with ID < `min_id` for the given element type.
    ///
    /// Called before writing each blob (passthrough or rewritten). For
    /// passthrough blobs, `min_id` is the blob's scanned minimum ID. This
    /// means creates with IDs *within* the passthrough blob's range (>= min_id
    /// and <= max_id) are not emitted here — they will be emitted when the
    /// next blob arrives with a higher min_id, or during `flush_all` at EOF.
    /// This can place them after the passthrough blob in the output, which is
    /// an accepted trade-off (see `block_overlaps_diff` comment).
    ///
    /// Also handles type transitions: when switching from e.g. Node to Way,
    /// flushes all remaining node creates before starting way creates.
    #[allow(clippy::too_many_arguments, clippy::cognitive_complexity)]
    fn emit_before(
        &mut self,
        kind: ElemKind,
        min_id: i64,
        diff: &DiffOverlay,
        emitted_nodes: &HashSet<i64>,
        emitted_ways: &HashSet<i64>,
        emitted_relations: &HashSet<i64>,
        bb: &mut BlockBuilder,
        writer: &mut PbfWriter<FileWriter>,
        stats: &mut MergeStats,
    ) -> MergeResult<()> {
        // Handle type transitions: flush remaining creates for previous types
        if let Some(prev) = self.last_kind
            && prev != kind
        {
            self.flush_remaining_type(prev, diff, emitted_nodes, emitted_ways,
                emitted_relations, bb, writer, stats)?;
            // Handle skipped types (e.g., Node → Relation skipping Way)
            if prev == ElemKind::Node && kind == ElemKind::Relation {
                self.flush_remaining_type(ElemKind::Way, diff, emitted_nodes,
                    emitted_ways, emitted_relations, bb, writer, stats)?;
            }
        }
        self.last_kind = Some(kind);

        // Emit creates of this type with ID < min_id
        match kind {
            ElemKind::Node => {
                while self.node_cursor < self.node_ids.len()
                    && self.node_ids[self.node_cursor] < min_id
                {
                    let id = self.node_ids[self.node_cursor];
                    self.node_cursor += 1;
                    if !emitted_nodes.contains(&id)
                        && let Some(osc) = diff.nodes.get(&id)
                    {
                        ensure_node_capacity(bb, writer)?;
                        let tags: Vec<(&str, &str)> =
                            osc.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                        bb.add_node(osc.id, to_decimicro(osc.lat), to_decimicro(osc.lon),
                            &tags, None);
                        stats.diff_nodes += 1;
                    }
                }
            }
            ElemKind::Way => {
                while self.way_cursor < self.way_ids.len()
                    && self.way_ids[self.way_cursor] < min_id
                {
                    let id = self.way_ids[self.way_cursor];
                    self.way_cursor += 1;
                    if !emitted_ways.contains(&id)
                        && let Some(osc) = diff.ways.get(&id)
                    {
                        write_osc_way(bb, writer, osc)?;
                        stats.diff_ways += 1;
                    }
                }
            }
            ElemKind::Relation => {
                while self.rel_cursor < self.rel_ids.len()
                    && self.rel_ids[self.rel_cursor] < min_id
                {
                    let id = self.rel_ids[self.rel_cursor];
                    self.rel_cursor += 1;
                    if !emitted_relations.contains(&id)
                        && let Some(osc) = diff.relations.get(&id)
                    {
                        write_osc_relation(bb, writer, osc)?;
                        stats.diff_relations += 1;
                    }
                }
            }
        }
        Ok(())
    }

    /// Flush all remaining creates for a type (at type transition or end of file).
    #[allow(clippy::too_many_arguments)]
    fn flush_remaining_type(
        &mut self,
        kind: ElemKind,
        diff: &DiffOverlay,
        emitted_nodes: &HashSet<i64>,
        emitted_ways: &HashSet<i64>,
        emitted_relations: &HashSet<i64>,
        bb: &mut BlockBuilder,
        writer: &mut PbfWriter<FileWriter>,
        stats: &mut MergeStats,
    ) -> MergeResult<()> {
        // Use i64::MAX as min_id to flush everything remaining
        self.emit_before(
            kind, i64::MAX, diff, emitted_nodes, emitted_ways, emitted_relations,
            bb, writer, stats,
        )?;
        flush_block(bb, writer)?;
        Ok(())
    }

    /// Flush all remaining creates for all types (end of file).
    #[allow(clippy::too_many_arguments)]
    fn flush_all(
        &mut self,
        diff: &DiffOverlay,
        emitted_nodes: &HashSet<i64>,
        emitted_ways: &HashSet<i64>,
        emitted_relations: &HashSet<i64>,
        bb: &mut BlockBuilder,
        writer: &mut PbfWriter<FileWriter>,
        stats: &mut MergeStats,
    ) -> MergeResult<()> {
        self.flush_remaining_type(ElemKind::Node, diff, emitted_nodes, emitted_ways,
            emitted_relations, bb, writer, stats)?;
        self.flush_remaining_type(ElemKind::Way, diff, emitted_nodes, emitted_ways,
            emitted_relations, bb, writer, stats)?;
        self.flush_remaining_type(ElemKind::Relation, diff, emitted_nodes, emitted_ways,
            emitted_relations, bb, writer, stats)?;
        Ok(())
    }

    /// Get sorted diff IDs in `[first_id, last_id]` for the given kind,
    /// excluding IDs in the emitted set (which are modifications already
    /// written during rewrite). The returned IDs are creates to interleave.
    fn creates_in_range(
        &self,
        kind: ElemKind,
        first_id: i64,
        last_id: i64,
        emitted: &HashSet<i64>,
    ) -> Vec<i64> {
        let ids = match kind {
            ElemKind::Node => &self.node_ids,
            ElemKind::Way => &self.way_ids,
            ElemKind::Relation => &self.rel_ids,
        };
        let start = ids.partition_point(|&id| id < first_id);
        let end = ids.partition_point(|&id| id <= last_id);
        ids[start..end]
            .iter()
            .copied()
            .filter(|id| !emitted.contains(id))
            .collect()
    }
}


// ---------------------------------------------------------------------------
// Passthrough helper
// ---------------------------------------------------------------------------

/// Write a passthrough blob, using `copy_file_range` when available.
///
/// Takes ownership of the frame's `frame_bytes` to move them into the
/// pipeline channel without copying (~55 KB saved per blob).
#[allow(unused_variables)]
fn write_passthrough(
    writer: &mut PbfWriter<FileWriter>,
    frame: &mut RawBlobFrame,
    input_fd: i32,
    use_copy_range: bool,
) -> io::Result<()> {
    #[cfg(feature = "linux-direct-io")]
    if use_copy_range {
        return writer.write_raw_copy(input_fd, frame.file_offset, frame.frame_bytes.len() as u64);
    }
    writer.write_raw_owned(std::mem::take(&mut frame.frame_bytes))
}

// ---------------------------------------------------------------------------
// Public merge function
// ---------------------------------------------------------------------------

/// Apply an OSC diff to a base PBF file, producing an updated sorted PBF.
///
/// Returns merge statistics on success.
///
/// # Errors
///
/// Returns an error if the base PBF or OSC file cannot be read, the output
/// file cannot be written, or if any PBF parsing/encoding fails.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
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
    eprintln!("Parsing OSC diff: {}", osc_file.display());
    let diff = parse_osc_file(osc_file)?;
    eprintln!(
        "Diff: {} nodes, {} ways, {} relations ({} del nodes, {} del ways, {} del rels)",
        diff.nodes.len(), diff.ways.len(), diff.relations.len(),
        diff.deleted_nodes.len(), diff.deleted_ways.len(), diff.deleted_relations.len(),
    );

    // Pre-compute sorted ID ranges for fast overlap checking
    let ranges = DiffRanges::from_diff(&diff);
    let mut skip_state = SkipState::new(&ranges);
    eprintln!(
        "Diff ID ranges: {} node IDs, {} way IDs, {} rel IDs",
        ranges.node_ids.len(), ranges.way_ids.len(), ranges.rel_ids.len(),
    );

    // Step 2: Open reader, read header, create pipelined writer
    let mut reader = FileReader::open(base_pbf, direct_io)?;
    let mut file_offset: u64 = 0;

    // Read the header blob first — needed to construct the pipelined writer.
    let header_bytes = loop {
        match read_raw_frame(&mut reader, &mut file_offset)? {
            Some(frame) if frame.blob_type == "OSMHeader" => {
                let header = decode_blob_to_headerblock(frame.blob_bytes())?;
                break build_header_bytes(&header)?;
            }
            Some(_) => {} // skip unknown blob types before header
            None => {
                return Err("base PBF has no OSMHeader blob".into());
            }
        }
    };
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

    // copy_file_range: get input fd and decide whether to use kernel-space copy.
    // O_DIRECT output is incompatible with copy_file_range (bypasses DirectWriter).
    // io_uring output handles CopyRange via pread + ring write (no copy_file_range).
    #[cfg(feature = "linux-direct-io")]
    let (input_fd, use_copy_range) = (reader.raw_fd(), io_uring || !direct_io);
    #[cfg(not(feature = "linux-direct-io"))]
    let (input_fd, use_copy_range) = (0i32, false);

    let mut bb = BlockBuilder::new();
    let mut emitted_nodes: HashSet<i64> = HashSet::new();
    let mut emitted_ways: HashSet<i64> = HashSet::new();
    let mut emitted_relations: HashSet<i64> = HashSet::new();
    let mut stats = MergeStats::new();
    let mut blob_count: u64 = 0;
    let mut create_emitter = CreateEmitter::from_diff(&diff);

    // Step 3: Read and process blobs in parallel batches
    let mut batch: Vec<RawBlobFrame> = Vec::with_capacity(BATCH_SIZE);

    loop {
        // Read next batch of data blob frames
        batch.clear();
        while batch.len() < BATCH_SIZE {
            match read_raw_frame(&mut reader, &mut file_offset)? {
                Some(frame) if frame.blob_type == "OSMData" => {
                    batch.push(frame);
                }
                Some(_) => {} // skip unknown blob types
                None => break,
            }
        }
        if batch.is_empty() {
            break;
        }

        let batch_len = batch.len() as u64;

        // If all element types are past their max affected ID, passthrough entire batch
        if skip_state.all_done() {
            flush_block(&mut bb, &mut writer)?;
            for frame in &mut batch {
                write_passthrough(&mut writer, frame, input_fd, use_copy_range)?;
                stats.blobs_skip_decompress += 1;
            }
            blob_count += batch_len;
            continue;
        }

        // Phase 1: Parallel classify — decompress + scan + parse overlapping blobs
        let classify_results: Vec<Result<ClassifyResult, String>> = batch
            .par_iter()
            .map_init(
                Vec::new,
                |buf, frame| classify_only(frame, &ranges, &diff, buf),
            )
            .collect();

        // Phase 2: Sequential pre-compute — collect modifications, assign creates
        let mut slots: Vec<BatchSlot> = Vec::with_capacity(classify_results.len());
        let mut rewrite_jobs: Vec<(&PrimitiveBlock, ElemKind, Vec<i64>)> = Vec::new();

        for result in &classify_results {
            let cr = result.as_ref().map_err(|e| -> Box<dyn std::error::Error> {
                e.clone().into()
            })?;
            match cr {
                ClassifyResult::Passthrough(idx) => {
                    slots.push(BatchSlot::Passthrough(*idx));
                }
                ClassifyResult::FalsePositive(idx) => {
                    slots.push(BatchSlot::FalsePositive(*idx));
                }
                ClassifyResult::NeedsRewrite { block, kind, first_id, last_id } => {
                    // Collect modification IDs into emitted sets
                    collect_modifications(
                        block, &diff,
                        &mut emitted_nodes, &mut emitted_ways, &mut emitted_relations,
                    );

                    // Compute create IDs for this blob's range (not in emitted set)
                    let emitted = match kind {
                        ElemKind::Node => &emitted_nodes,
                        ElemKind::Way => &emitted_ways,
                        ElemKind::Relation => &emitted_relations,
                    };
                    let creates = create_emitter.creates_in_range(
                        *kind, *first_id, *last_id, emitted,
                    );

                    // Add creates to emitted set to prevent double-emission
                    let emitted_mut = match kind {
                        ElemKind::Node => &mut emitted_nodes,
                        ElemKind::Way => &mut emitted_ways,
                        ElemKind::Relation => &mut emitted_relations,
                    };
                    for &id in &creates {
                        emitted_mut.insert(id);
                    }

                    let job_idx = rewrite_jobs.len();
                    rewrite_jobs.push((block, *kind, creates));
                    slots.push(BatchSlot::Rewrite {
                        job_index: job_idx,
                        kind: *kind,
                        first_id: *first_id,
                        last_id: *last_id,
                    });
                }
            }
        }

        // Phase 3: Parallel rewrite — rewrite overlapping blobs with pre-assigned creates
        let rewrite_results: Vec<Result<RewriteOutput, String>> = rewrite_jobs
            .par_iter()
            .map_init(
                BlockBuilder::new,
                |thread_bb, (block, kind, creates)| {
                    rewrite_block_parallel(block, &diff, thread_bb, creates, *kind)
                        .map_err(|e| e.to_string())
                },
            )
            .collect();

        // Propagate errors and unwrap results
        let mut rewrite_outputs: Vec<Option<RewriteOutput>> =
            Vec::with_capacity(rewrite_results.len());
        for result in rewrite_results {
            rewrite_outputs.push(Some(
                result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?,
            ));
        }

        // Phase 4: Sequential output — emit creates + write pre-serialized blocks
        for (i, slot) in slots.into_iter().enumerate() {
            blob_count += 1;

            match slot {
                BatchSlot::Passthrough(scan) => {
                    skip_state.update(scan.kind, scan.max_id, &ranges);
                    create_emitter.emit_before(
                        scan.kind, scan.min_id, &diff,
                        &emitted_nodes, &emitted_ways, &emitted_relations,
                        &mut bb, &mut writer, &mut stats,
                    )?;
                    flush_block(&mut bb, &mut writer)?;
                    if batch[i].index.is_some() {
                        write_passthrough(
                            &mut writer, &mut batch[i], input_fd, use_copy_range,
                        )?;
                        stats.blobs_index_hit += 1;
                    } else {
                        let indexdata = scan.serialize();
                        let reframed = crate::write::writer::reframe_raw_with_index(
                            batch[i].blob_bytes(),
                            &indexdata,
                        )?;
                        writer.write_raw(&reframed)?;
                        stats.blobs_scan_only += 1;
                    }
                    match scan.kind {
                        ElemKind::Node => stats.base_nodes += scan.count,
                        ElemKind::Way => stats.base_ways += scan.count,
                        ElemKind::Relation => stats.base_relations += scan.count,
                    }
                    stats.blobs_passthrough += 1;
                }
                BatchSlot::FalsePositive(scan) => {
                    skip_state.update(scan.kind, scan.max_id, &ranges);
                    create_emitter.emit_before(
                        scan.kind, scan.min_id, &diff,
                        &emitted_nodes, &emitted_ways, &emitted_relations,
                        &mut bb, &mut writer, &mut stats,
                    )?;
                    flush_block(&mut bb, &mut writer)?;
                    write_passthrough(
                        &mut writer, &mut batch[i], input_fd, use_copy_range,
                    )?;
                    match scan.kind {
                        ElemKind::Node => stats.base_nodes += scan.count,
                        ElemKind::Way => stats.base_ways += scan.count,
                        ElemKind::Relation => stats.base_relations += scan.count,
                    }
                    stats.blobs_passthrough += 1;
                }
                BatchSlot::Rewrite { job_index, kind, first_id, last_id } => {
                    skip_state.update(kind, last_id, &ranges);
                    create_emitter.emit_before(
                        kind, first_id, &diff,
                        &emitted_nodes, &emitted_ways, &emitted_relations,
                        &mut bb, &mut writer, &mut stats,
                    )?;
                    flush_block(&mut bb, &mut writer)?;
                    let output = rewrite_outputs[job_index]
                        .take()
                        .expect("rewrite result already consumed");
                    for block_bytes in &output.blocks {
                        writer.write_primitive_block(block_bytes)?;
                    }
                    stats.merge_from(&output.stats);
                    stats.blobs_rewritten += 1;
                }
            }

            if blob_count.is_multiple_of(500) {
                eprintln!(
                    "  Blob {blob_count}: {} pass ({} idx) / {} rewrite / {} skip, {} elements",
                    stats.blobs_passthrough, stats.blobs_index_hit,
                    stats.blobs_rewritten, stats.blobs_skip_decompress,
                    stats.total_elements(),
                );
            }
        }
    }

    // Step 4: Flush remaining block from rewrite processing
    flush_block(&mut bb, &mut writer)?;

    // Step 5: Flush remaining new creates at correct sorted positions
    create_emitter.flush_all(
        &diff, &emitted_nodes, &emitted_ways, &emitted_relations,
        &mut bb, &mut writer, &mut stats,
    )?;

    writer.flush()?;
    stats.print_summary();
    Ok(stats)
}

/// Batch size for parallel blob processing.
const BATCH_SIZE: usize = 64;

/// Result of parallel classification: parsed block for overlapping blobs,
/// or a scan index for passthrough blobs.
enum ClassifyResult {
    /// No overlap — passthrough raw frame.
    Passthrough(BlobIndex),
    /// Coarse overlap but no actual affected elements — passthrough raw frame.
    FalsePositive(BlobIndex),
    /// Block overlaps diff — parsed and ready for parallel rewrite.
    NeedsRewrite {
        block: PrimitiveBlock,
        kind: ElemKind,
        first_id: i64,
        last_id: i64,
    },
}

/// Parallel classify: decompress + scan + overlap check + parse overlapping blobs.
/// Returns `ClassifyResult` with parsed block for overlap blobs, ready for the
/// sequential pre-computation step that assigns creates before parallel rewrite.
#[allow(clippy::too_many_lines)]
fn classify_only(
    frame: &RawBlobFrame,
    ranges: &DiffRanges,
    diff: &DiffOverlay,
    buf: &mut Vec<u8>,
) -> Result<ClassifyResult, String> {
    // Fast path: use inline index from BlobHeader indexdata (no decompression).
    if let Some(ref idx) = frame.index
        && !ranges.range_overlaps(idx.kind, idx.min_id, idx.max_id)
    {
        return Ok(ClassifyResult::Passthrough(*idx));
    }

    // Slow path: decompress + lightweight scan.
    decompress_blob_data_into(frame.blob_bytes(), buf).map_err(|e| e.to_string())?;

    let scan = if let Some(scan) = blob_index::scan_block_ids(buf) {
        if !ranges.range_overlaps(scan.kind, scan.min_id, scan.max_id) {
            return Ok(ClassifyResult::Passthrough(scan));
        }
        Some(scan)
    } else {
        None
    };

    // Range overlaps — full parse + precise check.
    let raw = std::mem::take(buf);
    let block =
        parse_primitive_block_from_bytes_owned(&Bytes::from(raw)).map_err(|e| e.to_string())?;

    if !block_overlaps_diff(&block, diff) {
        let idx = scan.unwrap_or_else(|| {
            BlobIndex {
                kind: element_kind(&block.elements().next().expect("empty block in merge")),
                min_id: 0,
                max_id: 0,
                count: 0,
            }
        });
        return Ok(ClassifyResult::FalsePositive(idx));
    }

    // Precise overlap confirmed — extract kind and ID range for pre-computation.
    let mut first_id = i64::MAX;
    let mut last_id = i64::MIN;
    let mut kind = ElemKind::Node;
    for element in block.elements() {
        let id = match &element {
            Element::DenseNode(dn) => dn.id(),
            Element::Node(n) => n.id(),
            Element::Way(w) => w.id(),
            Element::Relation(r) => r.id(),
        };
        if id < first_id {
            first_id = id;
            kind = element_kind(&element);
        }
        if id > last_id {
            last_id = id;
        }
    }
    if first_id == i64::MAX {
        // Empty block — treat as false positive.
        return Ok(ClassifyResult::FalsePositive(scan.unwrap_or(BlobIndex {
            kind: ElemKind::Node,
            min_id: 0,
            max_id: 0,
            count: 0,
        })));
    }

    Ok(ClassifyResult::NeedsRewrite { block, kind, first_id, last_id })
}

/// Collect modification IDs from a NeedsRewrite block into the emitted sets.
/// A modification is a base element whose ID exists in the diff overlay
/// (the diff version replaces the base version during rewrite). These IDs
/// must be in the emitted sets so `CreateEmitter` doesn't re-emit them.
fn collect_modifications(
    block: &PrimitiveBlock,
    diff: &DiffOverlay,
    emitted_nodes: &mut HashSet<i64>,
    emitted_ways: &mut HashSet<i64>,
    emitted_relations: &mut HashSet<i64>,
) {
    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                let id = dn.id();
                if diff.nodes.contains_key(&id) {
                    emitted_nodes.insert(id);
                }
            }
            Element::Node(n) => {
                let id = n.id();
                if diff.nodes.contains_key(&id) {
                    emitted_nodes.insert(id);
                }
            }
            Element::Way(w) => {
                let id = w.id();
                if diff.ways.contains_key(&id) {
                    emitted_ways.insert(id);
                }
            }
            Element::Relation(r) => {
                let id = r.id();
                if diff.relations.contains_key(&id) {
                    emitted_relations.insert(id);
                }
            }
        }
    }
}

/// Batch processing slot — tracks per-blob state for the 4-phase parallel
/// merge pipeline. Each slot maps to a blob in the batch by position.
enum BatchSlot {
    /// No overlap — passthrough raw frame.
    Passthrough(BlobIndex),
    /// Coarse overlap but no actual affected elements — passthrough raw frame.
    FalsePositive(BlobIndex),
    /// Block overlaps diff — parallel rewrite completed, output ready.
    Rewrite {
        job_index: usize,
        kind: ElemKind,
        first_id: i64,
        last_id: i64,
    },
}
