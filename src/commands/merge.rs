// PBF merge: apply an OSC diff overlay to a base PBF, producing an updated PBF.
//
// Optimization: lightweight protobuf scanning extracts element type + ID range
// from decompressed bytes without full parsing. Blocks outside the diff's ID
// range are passed through without parsing. Once all element types are past
// their max affected ID, remaining blobs skip decompression entirely.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

use rayon::prelude::*;

use crate::blob::{
    decode_blob_to_headerblock, decompress_blob_data, parse_blob_header,
    parse_primitive_block_from_bytes,
};
use crate::block_builder::{BlockBuilder, MemberData, Metadata};
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
            "  Blobs: {} passthrough ({} scan-only, {} skip-decompress), {} rewritten (of {total_blobs} total)",
            self.blobs_passthrough + self.blobs_skip_decompress,
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

// ---------------------------------------------------------------------------
// Lightweight protobuf scanner: extract element type + ID range
// without full PrimitiveBlock parsing.
// ---------------------------------------------------------------------------

/// Result of a lightweight blob scan.
struct BlobScan {
    kind: ElemKind,
    min_id: i64,
    max_id: i64,
    count: u64,
}

/// Scan decompressed PrimitiveBlock bytes to extract element type and ID range.
/// This walks the protobuf wire format manually, only reading element IDs.
/// Much cheaper than a full PrimitiveBlock parse (skips string tables,
/// coordinates, tags, metadata, etc.).
#[allow(clippy::cast_possible_truncation)]
fn scan_block_ids(raw: &[u8]) -> Option<BlobScan> {
    let mut cursor = 0;
    let mut result: Option<BlobScan> = None;

    while cursor < raw.len() {
        let (tag, wire_type, new_pos) = read_tag(raw, cursor)?;
        cursor = new_pos;

        if tag == 2 && wire_type == 2 {
            // PrimitiveGroup (field 2, length-delimited)
            let (group_len, new_pos) = read_varint(raw, cursor)?;
            let group_end = new_pos + group_len as usize;
            cursor = new_pos;

            if let Some(scan) = scan_primitive_group(raw, cursor, group_end) {
                result = Some(match result {
                    None => scan,
                    Some(mut prev) => {
                        prev.min_id = prev.min_id.min(scan.min_id);
                        prev.max_id = prev.max_id.max(scan.max_id);
                        prev.count += scan.count;
                        prev
                    }
                });
            }
            cursor = group_end;
        } else {
            // Skip other fields (StringTable, granularity, offsets, etc.)
            cursor = skip_field(raw, wire_type, cursor)?;
        }
    }
    result
}

/// Scan a PrimitiveGroup submessage for element type + IDs.
#[allow(clippy::cast_possible_truncation)]
fn scan_primitive_group(raw: &[u8], mut cursor: usize, end: usize) -> Option<BlobScan> {
    while cursor < end {
        let (tag, wire_type, new_pos) = read_tag(raw, cursor)?;
        cursor = new_pos;

        match (tag, wire_type) {
            (2, 2) => {
                // DenseNodes (field 2, length-delimited)
                let (len, new_pos) = read_varint(raw, cursor)?;
                let dense_end = new_pos + len as usize;
                cursor = new_pos;
                return scan_dense_node_ids(raw, cursor, dense_end);
            }
            (3, 2) => {
                // Way (field 3, length-delimited)
                let (len, new_pos) = read_varint(raw, cursor)?;
                let msg_end = new_pos + len as usize;
                cursor = new_pos;
                // Scan this and remaining ways
                return scan_repeated_element_ids(raw, cursor, msg_end, end, 3, ElemKind::Way);
            }
            (4, 2) => {
                // Relation (field 4, length-delimited)
                let (len, new_pos) = read_varint(raw, cursor)?;
                let msg_end = new_pos + len as usize;
                cursor = new_pos;
                return scan_repeated_element_ids(
                    raw,
                    cursor,
                    msg_end,
                    end,
                    4,
                    ElemKind::Relation,
                );
            }
            (1, 2) => {
                // Node (field 1, length-delimited) — rare, non-dense
                let (len, new_pos) = read_varint(raw, cursor)?;
                let msg_end = new_pos + len as usize;
                cursor = new_pos;
                return scan_repeated_element_ids(raw, cursor, msg_end, end, 1, ElemKind::Node);
            }
            _ => {
                cursor = skip_field(raw, wire_type, cursor)?;
            }
        }
    }
    None
}

/// Scan DenseNodes to extract min/max IDs and count.
/// DenseNodes stores IDs as packed delta-encoded sint64 in field 1.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
fn scan_dense_node_ids(raw: &[u8], mut cursor: usize, end: usize) -> Option<BlobScan> {
    while cursor < end {
        let (tag, wire_type, new_pos) = read_tag(raw, cursor)?;
        cursor = new_pos;

        if tag == 1 && wire_type == 2 {
            // Packed sint64 IDs
            let (len, new_pos) = read_varint(raw, cursor)?;
            let ids_end = new_pos + len as usize;
            cursor = new_pos;

            let mut min_id = i64::MAX;
            let mut max_id = i64::MIN;
            let mut current_id: i64 = 0;
            let mut count: u64 = 0;

            while cursor < ids_end {
                let (raw_val, new_pos) = read_varint(raw, cursor)?;
                cursor = new_pos;
                // Zigzag decode: sint64
                let delta = ((raw_val >> 1) as i64) ^ -((raw_val & 1) as i64);
                current_id += delta;
                if count == 0 {
                    min_id = current_id;
                }
                max_id = current_id;
                count += 1;
            }

            if count > 0 {
                return Some(BlobScan {
                    kind: ElemKind::Node,
                    min_id,
                    max_id,
                    count,
                });
            }
            return None;
        }
        cursor = skip_field(raw, wire_type, cursor)?;
    }
    None
}

/// Scan repeated Way/Relation/Node messages to extract min/max IDs.
/// We already have the first message boundaries; scan through the group
/// to find additional messages of the same field tag.
#[allow(clippy::cast_possible_truncation)]
fn scan_repeated_element_ids(
    raw: &[u8],
    first_msg_start: usize,
    first_msg_end: usize,
    group_end: usize,
    expected_tag: u32,
    kind: ElemKind,
) -> Option<BlobScan> {
    // Extract ID from the first message
    let first_id = extract_element_id(raw, first_msg_start, first_msg_end)?;
    let mut min_id = first_id;
    let mut max_id = first_id;
    let mut count: u64 = 1;
    let mut last_id = first_id;

    // Scan remaining messages in the group
    let mut cursor = first_msg_end;
    while cursor < group_end {
        let (tag, wire_type, new_pos) = read_tag(raw, cursor)?;
        cursor = new_pos;

        if tag == expected_tag && wire_type == 2 {
            let (len, new_pos) = read_varint(raw, cursor)?;
            let msg_end = new_pos + len as usize;
            cursor = new_pos;

            if let Some(id) = extract_element_id(raw, cursor, msg_end) {
                min_id = min_id.min(id);
                max_id = max_id.max(id);
                last_id = id;
                count += 1;
            }
            cursor = msg_end;
        } else {
            cursor = skip_field(raw, wire_type, cursor)?;
        }
    }

    // For sorted PBFs, last_id == max_id, but be safe
    max_id = max_id.max(last_id);

    Some(BlobScan {
        kind,
        min_id,
        max_id,
        count,
    })
}

/// Extract the element ID (field 1, varint/int64) from a Node/Way/Relation message.
#[allow(clippy::cast_possible_wrap)]
fn extract_element_id(raw: &[u8], mut cursor: usize, end: usize) -> Option<i64> {
    while cursor < end {
        let (tag, wire_type, new_pos) = read_tag(raw, cursor)?;
        cursor = new_pos;
        if tag == 1 && wire_type == 0 {
            let (val, _) = read_varint(raw, cursor)?;
            return Some(val as i64);
        }
        cursor = skip_field(raw, wire_type, cursor)?;
    }
    None
}

// ---------------------------------------------------------------------------
// Protobuf wire format helpers
// ---------------------------------------------------------------------------

/// Read a varint from the buffer. Returns (value, new_cursor).
fn read_varint(raw: &[u8], mut cursor: usize) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;
    loop {
        if cursor >= raw.len() {
            return None;
        }
        let byte = raw[cursor];
        cursor += 1;
        result |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((result, cursor));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

/// Read a field tag. Returns (field_number, wire_type, new_cursor).
fn read_tag(raw: &[u8], cursor: usize) -> Option<(u32, u32, usize)> {
    let (val, new_pos) = read_varint(raw, cursor)?;
    #[allow(clippy::cast_possible_truncation)]
    let wire_type = (val & 0x07) as u32;
    #[allow(clippy::cast_possible_truncation)]
    let field_number = (val >> 3) as u32;
    Some((field_number, wire_type, new_pos))
}

/// Skip a field value based on wire type. Returns new cursor position.
#[allow(clippy::cast_possible_truncation)]
fn skip_field(raw: &[u8], wire_type: u32, mut cursor: usize) -> Option<usize> {
    match wire_type {
        0 => {
            // Varint — skip bytes until MSB is 0
            loop {
                if cursor >= raw.len() {
                    return None;
                }
                let byte = raw[cursor];
                cursor += 1;
                if byte & 0x80 == 0 {
                    return Some(cursor);
                }
            }
        }
        1 => Some(cursor + 8),  // 64-bit fixed
        2 => {
            // Length-delimited
            let (len, new_pos) = read_varint(raw, cursor)?;
            Some(new_pos + len as usize)
        }
        5 => Some(cursor + 4),  // 32-bit fixed
        _ => None,              // Unknown wire type
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

fn osc_member_type_to_member_type(s: &str) -> crate::MemberType {
    match s {
        "way" => crate::MemberType::Way,
        "relation" => crate::MemberType::Relation,
        _ => crate::MemberType::Node,
    }
}

// ---------------------------------------------------------------------------
// Low-level blob frame reader (preserves raw bytes for passthrough)
// ---------------------------------------------------------------------------

/// A raw blob frame: the complete `[4-byte len][BlobHeader][Blob]` bytes,
/// plus the parsed header type string and the raw Blob protobuf bytes.
struct RawBlobFrame {
    /// Complete framed bytes suitable for write_raw().
    frame_bytes: Vec<u8>,
    /// Blob type: "OSMHeader", "OSMData", etc.
    blob_type: String,
    /// The raw Blob protobuf message bytes (for selective decoding).
    blob_bytes: Vec<u8>,
}

/// Read the next raw blob frame from the reader.
/// Returns None at EOF.
fn read_raw_frame<R: Read>(reader: &mut R) -> MergeResult<Option<RawBlobFrame>> {
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

    // Parse just type + datasize
    let (blob_type, data_size) = parse_blob_header(&header_bytes)?;

    // Read Blob bytes
    let mut blob_bytes = vec![0u8; data_size];
    reader.read_exact(&mut blob_bytes)?;

    // Assemble the complete frame
    let frame_len = 4 + header_len + data_size;
    let mut frame_bytes = Vec::with_capacity(frame_len);
    frame_bytes.extend_from_slice(&len_buf);
    frame_bytes.extend_from_slice(&header_bytes);
    frame_bytes.extend_from_slice(&blob_bytes);

    Ok(Some(RawBlobFrame {
        frame_bytes,
        blob_type,
        blob_bytes,
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

// ---------------------------------------------------------------------------
// Block flushing helpers
// ---------------------------------------------------------------------------

fn flush_block(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<std::io::BufWriter<File>>,
) -> MergeResult<()> {
    if let Some(bytes) = bb.take()? {
        writer.write_primitive_block(&bytes)?;
    }
    Ok(())
}

fn ensure_node_capacity(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<std::io::BufWriter<File>>,
) -> MergeResult<()> {
    if !bb.can_add_node() {
        flush_block(bb, writer)?;
    }
    Ok(())
}

fn ensure_way_capacity(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<std::io::BufWriter<File>>,
) -> MergeResult<()> {
    if !bb.can_add_way() {
        flush_block(bb, writer)?;
    }
    Ok(())
}

fn ensure_relation_capacity(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<std::io::BufWriter<File>>,
) -> MergeResult<()> {
    if !bb.can_add_relation() {
        flush_block(bb, writer)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Writing OSC elements (from diff, no metadata)
// ---------------------------------------------------------------------------

fn write_osc_way(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<std::io::BufWriter<File>>,
    way: &OscWay,
) -> MergeResult<()> {
    ensure_way_capacity(bb, writer)?;
    let tags: Vec<(&str, &str)> = way.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    bb.add_way(way.id, &tags, &way.node_refs, None);
    Ok(())
}

fn write_osc_relation(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<std::io::BufWriter<File>>,
    rel: &OscRelation,
) -> MergeResult<()> {
    ensure_relation_capacity(bb, writer)?;
    let tags: Vec<(&str, &str)> = rel.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let members: Vec<MemberData<'_>> = rel
        .members
        .iter()
        .map(|m: &OscRelMember| {
            let mt = osc_member_type_to_member_type(&m.member_type);
            MemberData {
                id: crate::MemberId::from_id_and_type(m.ref_id, mt),
                role: &m.role,
            }
        })
        .collect();
    bb.add_relation(rel.id, &tags, &members, None);
    Ok(())
}

// ---------------------------------------------------------------------------
// Writing base elements (with metadata passthrough)
// ---------------------------------------------------------------------------

fn write_base_dense_node(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<std::io::BufWriter<File>>,
    dn: &crate::DenseNode<'_>,
) -> MergeResult<()> {
    ensure_node_capacity(bb, writer)?;
    let tags: Vec<(&str, &str)> = dn.tags().collect();
    let meta = dn.info().and_then(|info| {
        let user = info.user().ok()?;
        Some(Metadata {
            version: info.version(),
            timestamp: info.milli_timestamp() / 1000,
            changeset: info.changeset(),
            uid: info.uid(),
            user,
            visible: info.visible(),
        })
    });
    bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), &tags, meta.as_ref());
    Ok(())
}

fn write_base_way(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<std::io::BufWriter<File>>,
    way: &crate::Way<'_>,
) -> MergeResult<()> {
    ensure_way_capacity(bb, writer)?;
    let tags: Vec<(&str, &str)> = way.tags().collect();
    let refs: Vec<i64> = way.refs().collect();
    let info = way.info();
    let meta = info.version().map(|v| Metadata {
        version: v,
        timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
        changeset: info.changeset().unwrap_or(0),
        uid: info.uid().unwrap_or(0),
        user: info.user().and_then(Result::ok).unwrap_or(""),
        visible: info.visible(),
    });
    bb.add_way(way.id(), &tags, &refs, meta.as_ref());
    Ok(())
}

fn write_base_relation(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<std::io::BufWriter<File>>,
    rel: &crate::Relation<'_>,
) -> MergeResult<()> {
    ensure_relation_capacity(bb, writer)?;
    let tags: Vec<(&str, &str)> = rel.tags().collect();
    let members: Vec<MemberData<'_>> = rel
        .members()
        .map(|m| MemberData {
            id: m.id,
            role: m.role().unwrap_or(""),
        })
        .collect();
    let info = rel.info();
    let meta = info.version().map(|v| Metadata {
        version: v,
        timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
        changeset: info.changeset().unwrap_or(0),
        uid: info.uid().unwrap_or(0),
        user: info.user().and_then(Result::ok).unwrap_or(""),
        visible: info.visible(),
    });
    bb.add_relation(rel.id(), &tags, &members, meta.as_ref());
    Ok(())
}

// ---------------------------------------------------------------------------
// Header handling
// ---------------------------------------------------------------------------

fn write_header(
    header: &crate::HeaderBlock,
    writer: &mut PbfWriter<std::io::BufWriter<File>>,
) -> MergeResult<()> {
    let bbox = header.bbox().map(|b| (b.left, b.bottom, b.right, b.top));
    let header_bytes = crate::block_builder::build_header(
        bbox,
        header.osmosis_replication_timestamp(),
        header.osmosis_replication_sequence_number(),
        header.osmosis_replication_base_url(),
        &[],
    )?;
    writer.write_header(&header_bytes)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Process an affected data block (has diff overlap — re-encode)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ElemKind {
    Node,
    Way,
    Relation,
}

fn element_kind(element: &Element<'_>) -> ElemKind {
    match element {
        Element::DenseNode(_) | Element::Node(_) => ElemKind::Node,
        Element::Way(_) => ElemKind::Way,
        Element::Relation(_) => ElemKind::Relation,
    }
}

struct RewriteContext<'a> {
    diff: &'a DiffOverlay,
    emitted_nodes: &'a mut HashSet<i64>,
    emitted_ways: &'a mut HashSet<i64>,
    emitted_relations: &'a mut HashSet<i64>,
    stats: &'a mut MergeStats,
    current_kind: &'a mut Option<ElemKind>,
    create_emitter: &'a mut CreateEmitter,
}

fn rewrite_block(
    block: &PrimitiveBlock,
    ctx: &mut RewriteContext<'_>,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<std::io::BufWriter<File>>,
) -> MergeResult<()> {
    for element in block.elements() {
        let kind = element_kind(&element);
        if let Some(prev) = *ctx.current_kind
            && prev != kind
        {
            flush_block(bb, writer)?;
        }
        *ctx.current_kind = Some(kind);

        // Emit any new creates that sort before this element (sorted output)
        let elem_id = match &element {
            Element::DenseNode(dn) => dn.id(),
            Element::Node(n) => n.id(),
            Element::Way(w) => w.id(),
            Element::Relation(r) => r.id(),
        };
        ctx.create_emitter.emit_before(
            kind, elem_id, ctx.diff,
            ctx.emitted_nodes, ctx.emitted_ways, ctx.emitted_relations,
            bb, writer, ctx.stats,
        )?;

        rewrite_element(&element, ctx, bb, writer)?;
    }
    Ok(())
}

fn rewrite_element(
    element: &Element<'_>,
    ctx: &mut RewriteContext<'_>,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<std::io::BufWriter<File>>,
) -> MergeResult<()> {
    match element {
        Element::DenseNode(dn) => rewrite_dense_node(dn, ctx, bb, writer),
        Element::Node(n) => rewrite_node(n, ctx, bb, writer),
        Element::Way(w) => rewrite_way(w, ctx, bb, writer),
        Element::Relation(r) => rewrite_relation(r, ctx, bb, writer),
    }
}

fn rewrite_dense_node(
    dn: &crate::DenseNode<'_>,
    ctx: &mut RewriteContext<'_>,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<std::io::BufWriter<File>>,
) -> MergeResult<()> {
    let id = dn.id();
    if ctx.diff.deleted_nodes.contains(&id) {
        ctx.stats.deleted += 1;
        return Ok(());
    }
    if let Some(osc) = ctx.diff.nodes.get(&id) {
        ensure_node_capacity(bb, writer)?;
        let tags: Vec<(&str, &str)> = osc.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        bb.add_node(osc.id, to_decimicro(osc.lat), to_decimicro(osc.lon), &tags, None);
        ctx.emitted_nodes.insert(id);
        ctx.stats.diff_nodes += 1;
    } else {
        write_base_dense_node(bb, writer, dn)?;
        ctx.stats.base_nodes += 1;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn rewrite_node(
    node: &crate::Node<'_>,
    ctx: &mut RewriteContext<'_>,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<std::io::BufWriter<File>>,
) -> MergeResult<()> {
    let id = node.id();
    if ctx.diff.deleted_nodes.contains(&id) {
        ctx.stats.deleted += 1;
        return Ok(());
    }
    if let Some(osc) = ctx.diff.nodes.get(&id) {
        ensure_node_capacity(bb, writer)?;
        let tags: Vec<(&str, &str)> = osc.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        bb.add_node(osc.id, to_decimicro(osc.lat), to_decimicro(osc.lon), &tags, None);
        ctx.emitted_nodes.insert(id);
        ctx.stats.diff_nodes += 1;
    } else {
        ensure_node_capacity(bb, writer)?;
        let tags: Vec<(&str, &str)> = node.tags().collect();
        let info = node.info();
        let meta = info.version().map(|v| Metadata {
            version: v,
            timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
            changeset: info.changeset().unwrap_or(0),
            uid: info.uid().unwrap_or(0),
            user: info.user().and_then(Result::ok).unwrap_or(""),
            visible: info.visible(),
        });
        bb.add_node(node.id(), node.decimicro_lat(), node.decimicro_lon(), &tags, meta.as_ref());
        ctx.stats.base_nodes += 1;
    }
    Ok(())
}

fn rewrite_way(
    way: &crate::Way<'_>,
    ctx: &mut RewriteContext<'_>,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<std::io::BufWriter<File>>,
) -> MergeResult<()> {
    let id = way.id();
    if ctx.diff.deleted_ways.contains(&id) {
        ctx.stats.deleted += 1;
        return Ok(());
    }
    if let Some(osc) = ctx.diff.ways.get(&id) {
        write_osc_way(bb, writer, osc)?;
        ctx.emitted_ways.insert(id);
        ctx.stats.diff_ways += 1;
    } else {
        write_base_way(bb, writer, way)?;
        ctx.stats.base_ways += 1;
    }
    Ok(())
}

fn rewrite_relation(
    rel: &crate::Relation<'_>,
    ctx: &mut RewriteContext<'_>,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<std::io::BufWriter<File>>,
) -> MergeResult<()> {
    let id = rel.id();
    if ctx.diff.deleted_relations.contains(&id) {
        ctx.stats.deleted += 1;
        return Ok(());
    }
    if let Some(osc) = ctx.diff.relations.get(&id) {
        write_osc_relation(bb, writer, osc)?;
        ctx.emitted_relations.insert(id);
        ctx.stats.diff_relations += 1;
    } else {
        write_base_relation(bb, writer, rel)?;
        ctx.stats.base_relations += 1;
    }
    Ok(())
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
        writer: &mut PbfWriter<std::io::BufWriter<File>>,
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
        writer: &mut PbfWriter<std::io::BufWriter<File>>,
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
        writer: &mut PbfWriter<std::io::BufWriter<File>>,
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
pub fn merge(base_pbf: &Path, osc_file: &Path, output_pbf: &Path) -> MergeResult<MergeStats> {
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

    // Step 2: Open reader (low-level) and writer
    let file = File::open(base_pbf)?;
    let mut reader = BufReader::new(file);
    let mut writer = PbfWriter::to_path(output_pbf, Compression::default())?;

    let mut bb = BlockBuilder::new();
    let mut emitted_nodes: HashSet<i64> = HashSet::new();
    let mut emitted_ways: HashSet<i64> = HashSet::new();
    let mut emitted_relations: HashSet<i64> = HashSet::new();
    let mut stats = MergeStats::new();
    let mut current_kind: Option<ElemKind> = None;
    let mut blob_count: u64 = 0;
    let mut create_emitter = CreateEmitter::from_diff(&diff);

    // Step 3: Read and process blobs in parallel batches
    let mut batch: Vec<RawBlobFrame> = Vec::with_capacity(BATCH_SIZE);

    loop {
        // Read next batch of data blob frames
        batch.clear();
        while batch.len() < BATCH_SIZE {
            match read_raw_frame(&mut reader)? {
                Some(frame) if frame.blob_type == "OSMHeader" => {
                    let header = decode_blob_to_headerblock(&frame.blob_bytes)?;
                    write_header(&header, &mut writer)?;
                }
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
            for frame in &batch {
                writer.write_raw(&frame.frame_bytes)?;
                stats.blobs_skip_decompress += 1;
            }
            blob_count += batch_len;
            continue;
        }

        // Parallel decompress + classify
        let classified: Vec<Result<BlobClassified, String>> = batch
            .par_iter()
            .map(|frame| classify_blob(frame, &ranges))
            .collect();

        // Sequential processing: write passthrough frames, rewrite overlapping blocks
        for (i, result) in classified.into_iter().enumerate() {
            let class = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            blob_count += 1;

            match class {
                BlobClassified::Passthrough(scan) => {
                    // Coarse check found no diff IDs in this blob's range.
                    // Safe to write the raw compressed frame without any parsing.
                    // Creates with ID < min_id are emitted first; creates with
                    // IDs *within* this blob's range are deferred to later.
                    skip_state.update(scan.kind, scan.max_id, &ranges);
                    create_emitter.emit_before(
                        scan.kind, scan.min_id, &diff,
                        &emitted_nodes, &emitted_ways, &emitted_relations,
                        &mut bb, &mut writer, &mut stats,
                    )?;
                    flush_block(&mut bb, &mut writer)?;
                    writer.write_raw(&batch[i].frame_bytes)?;
                    match scan.kind {
                        ElemKind::Node => stats.base_nodes += scan.count,
                        ElemKind::Way => stats.base_ways += scan.count,
                        ElemKind::Relation => stats.base_relations += scan.count,
                    }
                    stats.blobs_passthrough += 1;
                    stats.blobs_scan_only += 1;
                }
                BlobClassified::MayOverlap(raw) | BlobClassified::Fallback(raw) => {
                    let block = parse_primitive_block_from_bytes(&raw)?;

                    // Emit new creates that sort before this block's first element
                    if let Some(first) = block.elements().next() {
                        let kind = element_kind(&first);
                        let first_id = match &first {
                            Element::DenseNode(dn) => dn.id(),
                            Element::Node(n) => n.id(),
                            Element::Way(w) => w.id(),
                            Element::Relation(r) => r.id(),
                        };
                        create_emitter.emit_before(
                            kind, first_id, &diff,
                            &emitted_nodes, &emitted_ways, &emitted_relations,
                            &mut bb, &mut writer, &mut stats,
                        )?;
                    }

                    if block_overlaps_diff(&block, &diff) {
                        // Precise check: at least one element in this block
                        // has a matching diff ID. Rewrite element-by-element.
                        flush_block(&mut bb, &mut writer)?;
                        let mut ctx = RewriteContext {
                            diff: &diff,
                            emitted_nodes: &mut emitted_nodes,
                            emitted_ways: &mut emitted_ways,
                            emitted_relations: &mut emitted_relations,
                            stats: &mut stats,
                            current_kind: &mut current_kind,
                            create_emitter: &mut create_emitter,
                        };
                        rewrite_block(&block, &mut ctx, &mut bb, &mut writer)?;
                        stats.blobs_rewritten += 1;
                    } else {
                        // Precise check: no element in this block is affected.
                        // The coarse range check was a false positive — e.g.
                        // the diff only has pure creates with IDs within this
                        // blob's range, but no modifies or deletes of existing
                        // elements. Pass through the raw frame. The creates
                        // will be emitted by CreateEmitter when the next blob
                        // arrives or during flush_all at EOF.
                        flush_block(&mut bb, &mut writer)?;
                        writer.write_raw(&batch[i].frame_bytes)?;
                        count_block_elements(&block, &mut stats);
                        stats.blobs_passthrough += 1;
                    }
                }
            }

            if blob_count.is_multiple_of(500) {
                eprintln!(
                    "  Blob {blob_count}: {} pass / {} rewrite / {} skip-decompress, {} elements",
                    stats.blobs_passthrough, stats.blobs_rewritten,
                    stats.blobs_skip_decompress, stats.total_elements(),
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

/// Result of parallel blob classification.
///
/// The classification pipeline has two stages:
///
/// 1. **Coarse** (`classify_blob`): decompress, scan element type + ID range,
///    check `DiffRanges::range_overlaps`. Fast — no full protobuf parse.
///    Produces `Passthrough` or `MayOverlap`.
///
/// 2. **Precise** (main loop): for `MayOverlap` blobs, do a full parse and
///    check `block_overlaps_diff` — tests each actual element ID against the
///    diff. If no actual element is affected, the blob is passed through raw
///    (even though the coarse check flagged it). This happens when the diff
///    only contains pure creates with IDs in the blob's range.
enum BlobClassified {
    /// No overlap — passthrough the raw frame directly.
    Passthrough(BlobScan),
    /// Coarse range overlaps diff — decompressed bytes ready for full parse.
    /// May still be passed through if the precise check finds no actual overlap.
    MayOverlap(Vec<u8>),
    /// Decompression or scan failed — decompressed bytes for fallback.
    Fallback(Vec<u8>),
}

/// Classify a blob in parallel: decompress + scan + range check.
/// Returns `Result<_, String>` instead of `MergeResult` so it's Send for rayon.
fn classify_blob(frame: &RawBlobFrame, ranges: &DiffRanges) -> Result<BlobClassified, String> {
    let raw = decompress_blob_data(&frame.blob_bytes).map_err(|e| e.to_string())?;

    if let Some(scan) = scan_block_ids(&raw) {
        if !ranges.range_overlaps(scan.kind, scan.min_id, scan.max_id) {
            return Ok(BlobClassified::Passthrough(scan));
        }
        // Range might overlap — need full parse
        Ok(BlobClassified::MayOverlap(raw))
    } else {
        Ok(BlobClassified::Fallback(raw))
    }
}

/// Count elements in a block for stats without doing any processing.
fn count_block_elements(block: &PrimitiveBlock, stats: &mut MergeStats) {
    for element in block.elements() {
        match element {
            Element::DenseNode(_) | Element::Node(_) => stats.base_nodes += 1,
            Element::Way(_) => stats.base_ways += 1,
            Element::Relation(_) => stats.base_relations += 1,
        }
    }
}
