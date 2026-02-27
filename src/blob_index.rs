//! Blob-level index: lightweight element type + ID range scanning and serialization.
//!
//! Used by the write path to embed per-blob metadata in the BlobHeader's `indexdata`
//! field, and by the merge read path to classify blobs without decompression.

/// Element type stored in a blob index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElemKind {
    Node,
    Way,
    Relation,
}

/// Blob-level index: element type, ID range, and element count.
///
/// Produced by [`scan_block_ids`] from decompressed PrimitiveBlock bytes,
/// or deserialized from BlobHeader `indexdata`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BlobIndex {
    pub kind: ElemKind,
    pub min_id: i64,
    pub max_id: i64,
    pub count: u64,
}

/// Indexdata wire format version. Bump if the format changes.
const INDEX_VERSION: u8 = 0x01;

/// Serialized index size: 1 version + 1 type + 8 min_id + 8 max_id + 8 count = 26 bytes.
const INDEX_SIZE: usize = 26;

impl BlobIndex {
    /// Serialize to the 26-byte indexdata format.
    pub fn serialize(&self) -> [u8; INDEX_SIZE] {
        let mut buf = [0u8; INDEX_SIZE];
        buf[0] = INDEX_VERSION;
        buf[1] = match self.kind {
            ElemKind::Node => 0,
            ElemKind::Way => 1,
            ElemKind::Relation => 2,
        };
        buf[2..10].copy_from_slice(&self.min_id.to_le_bytes());
        buf[10..18].copy_from_slice(&self.max_id.to_le_bytes());
        buf[18..26].copy_from_slice(&self.count.to_le_bytes());
        buf
    }

    /// Deserialize from indexdata bytes. Returns `None` if the data is
    /// invalid, too short, or has an unrecognized version.
    pub fn deserialize(data: &[u8]) -> Option<Self> {
        if data.len() < INDEX_SIZE || data[0] != INDEX_VERSION {
            return None;
        }
        let kind = match data[1] {
            0 => ElemKind::Node,
            1 => ElemKind::Way,
            2 => ElemKind::Relation,
            _ => return None,
        };
        let min_id = i64::from_le_bytes(data[2..10].try_into().ok()?);
        let max_id = i64::from_le_bytes(data[10..18].try_into().ok()?);
        let count = u64::from_le_bytes(data[18..26].try_into().ok()?);
        Some(BlobIndex {
            kind,
            min_id,
            max_id,
            count,
        })
    }
}

// ---------------------------------------------------------------------------
// Blob-type filter
// ---------------------------------------------------------------------------

/// Filter for skipping blobs by element type during pipelined reads.
///
/// When a `BlobFilter` is set on an [`ElementReader`](crate::ElementReader),
/// the pipeline skips decompressing blobs whose element type (from indexdata)
/// does not match the filter. Files without indexdata are unaffected — all
/// blobs pass through.
///
/// # Example
/// ```no_run
/// use pbfhogg::{ElementReader, BlobFilter};
///
/// let reader = ElementReader::from_path("data.osm.pbf")?;
/// let reader = reader.with_blob_filter(BlobFilter::only_ways());
/// // Only way blobs are decompressed; node and relation blobs are skipped.
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy)]
pub struct BlobFilter {
    pub(crate) want_nodes: bool,
    pub(crate) want_ways: bool,
    pub(crate) want_relations: bool,
}

impl BlobFilter {
    /// Create a filter that accepts only the specified element types.
    pub fn new(want_nodes: bool, want_ways: bool, want_relations: bool) -> Self {
        Self { want_nodes, want_ways, want_relations }
    }

    /// Filter that accepts only node blobs.
    pub fn only_nodes() -> Self {
        Self { want_nodes: true, want_ways: false, want_relations: false }
    }

    /// Filter that accepts only way blobs.
    pub fn only_ways() -> Self {
        Self { want_nodes: false, want_ways: true, want_relations: false }
    }

    /// Filter that accepts only relation blobs.
    pub fn only_relations() -> Self {
        Self { want_nodes: false, want_ways: false, want_relations: true }
    }

    /// Returns true if the filter accepts blobs of the given element kind.
    pub(crate) fn wants(&self, kind: ElemKind) -> bool {
        match kind {
            ElemKind::Node => self.want_nodes,
            ElemKind::Way => self.want_ways,
            ElemKind::Relation => self.want_relations,
        }
    }
}

// ---------------------------------------------------------------------------
// Lightweight protobuf scanner: extract element type + ID range
// without full PrimitiveBlock parsing.
// Uses Cursor from read::wire for varint/tag/skip primitives.
// ---------------------------------------------------------------------------

use crate::read::wire::{zigzag_decode_64, Cursor};

/// Scan decompressed PrimitiveBlock bytes to extract element type and ID range.
/// This walks the protobuf wire format manually, only reading element IDs.
/// Much cheaper than a full PrimitiveBlock parse (skips string tables,
/// coordinates, tags, metadata, etc.).
pub(crate) fn scan_block_ids(raw: &[u8]) -> Option<BlobIndex> {
    let mut cur = Cursor::new(raw);
    let mut result: Option<BlobIndex> = None;

    while let Some((tag, wire_type)) = cur.read_tag().ok()? {
        if tag == 2 && wire_type == 2 {
            // PrimitiveGroup (field 2, length-delimited)
            let group_data = cur.read_len_delimited().ok()?;
            if let Some(scan) = scan_primitive_group(group_data) {
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
        } else {
            cur.skip_field(wire_type).ok()?;
        }
    }
    result
}

/// Scan a PrimitiveGroup submessage for element type + IDs.
fn scan_primitive_group(raw: &[u8]) -> Option<BlobIndex> {
    let mut cur = Cursor::new(raw);

    while let Some((tag, wire_type)) = cur.read_tag().ok()? {
        match (tag, wire_type) {
            (2, 2) => {
                // DenseNodes (field 2, length-delimited)
                let data = cur.read_len_delimited().ok()?;
                return scan_dense_node_ids(data);
            }
            (3, 2) => {
                // Way (field 3, length-delimited)
                let first_msg = cur.read_len_delimited().ok()?;
                return scan_repeated_element_ids(first_msg, &mut cur, 3, ElemKind::Way);
            }
            (4, 2) => {
                // Relation (field 4, length-delimited)
                let first_msg = cur.read_len_delimited().ok()?;
                return scan_repeated_element_ids(first_msg, &mut cur, 4, ElemKind::Relation);
            }
            (1, 2) => {
                // Node (field 1, length-delimited) — rare, non-dense
                let first_msg = cur.read_len_delimited().ok()?;
                return scan_repeated_element_ids(first_msg, &mut cur, 1, ElemKind::Node);
            }
            _ => {
                cur.skip_field(wire_type).ok()?;
            }
        }
    }
    None
}

/// Scan DenseNodes to extract min/max IDs and count.
/// DenseNodes stores IDs as packed delta-encoded sint64 in field 1.
fn scan_dense_node_ids(raw: &[u8]) -> Option<BlobIndex> {
    let mut cur = Cursor::new(raw);

    while let Some((tag, wire_type)) = cur.read_tag().ok()? {
        if tag == 1 && wire_type == 2 {
            // Packed sint64 IDs
            let ids_data = cur.read_len_delimited().ok()?;
            let mut id_cur = Cursor::new(ids_data);

            let mut min_id = i64::MAX;
            let mut max_id = i64::MIN;
            let mut current_id: i64 = 0;
            let mut count: u64 = 0;

            while !id_cur.is_empty() {
                let raw_val = id_cur.read_varint().ok()?;
                let delta = zigzag_decode_64(raw_val);
                current_id += delta;
                min_id = min_id.min(current_id);
                max_id = max_id.max(current_id);
                count += 1;
            }

            if count > 0 {
                return Some(BlobIndex {
                    kind: ElemKind::Node,
                    min_id,
                    max_id,
                    count,
                });
            }
            return None;
        }
        cur.skip_field(wire_type).ok()?;
    }
    None
}

/// Scan repeated Way/Relation/Node messages to extract min/max IDs.
/// `first_msg` is the first message body; `rest` is positioned after it
/// in the parent group for scanning remaining messages.
fn scan_repeated_element_ids(
    first_msg: &[u8],
    rest: &mut Cursor<'_>,
    expected_tag: u32,
    kind: ElemKind,
) -> Option<BlobIndex> {
    let first_id = extract_element_id(first_msg)?;
    let mut min_id = first_id;
    let mut max_id = first_id;
    let mut count: u64 = 1;
    let mut last_id = first_id;

    // Scan remaining messages in the group
    while let Some((tag, wire_type)) = rest.read_tag().ok()? {
        if tag == expected_tag && wire_type == 2 {
            let msg = rest.read_len_delimited().ok()?;
            if let Some(id) = extract_element_id(msg) {
                min_id = min_id.min(id);
                max_id = max_id.max(id);
                last_id = id;
                count += 1;
            }
        } else {
            rest.skip_field(wire_type).ok()?;
        }
    }

    // For sorted PBFs, last_id == max_id, but be safe
    max_id = max_id.max(last_id);

    Some(BlobIndex {
        kind,
        min_id,
        max_id,
        count,
    })
}

/// Extract the element ID (field 1, varint/int64) from a Node/Way/Relation message.
#[allow(clippy::cast_possible_wrap)]
fn extract_element_id(msg: &[u8]) -> Option<i64> {
    let mut cur = Cursor::new(msg);
    while let Some((tag, wire_type)) = cur.read_tag().ok()? {
        if tag == 1 && wire_type == 0 {
            return Some(cur.read_varint().ok()? as i64);
        }
        cur.skip_field(wire_type).ok()?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_serialize_deserialize() {
        let index = BlobIndex {
            kind: ElemKind::Way,
            min_id: 100,
            max_id: 9999,
            count: 42,
        };
        let bytes = index.serialize();
        let recovered = BlobIndex::deserialize(&bytes).expect("deserialize should succeed");
        assert_eq!(recovered.kind, ElemKind::Way);
        assert_eq!(recovered.min_id, 100);
        assert_eq!(recovered.max_id, 9999);
        assert_eq!(recovered.count, 42);
    }

    #[test]
    fn deserialize_rejects_bad_version() {
        let mut bytes = BlobIndex {
            kind: ElemKind::Node,
            min_id: 0,
            max_id: 0,
            count: 0,
        }
        .serialize();
        bytes[0] = 0xFF;
        assert!(BlobIndex::deserialize(&bytes).is_none());
    }

    #[test]
    fn deserialize_rejects_short_data() {
        assert!(BlobIndex::deserialize(&[0x01, 0x00]).is_none());
    }

    #[test]
    fn deserialize_rejects_bad_type() {
        let mut bytes = BlobIndex {
            kind: ElemKind::Node,
            min_id: 0,
            max_id: 0,
            count: 0,
        }
        .serialize();
        bytes[1] = 5; // invalid element type
        assert!(BlobIndex::deserialize(&bytes).is_none());
    }

    #[test]
    fn roundtrip_negative_ids() {
        let index = BlobIndex {
            kind: ElemKind::Node,
            min_id: -100,
            max_id: -1,
            count: 100,
        };
        let bytes = index.serialize();
        let recovered = BlobIndex::deserialize(&bytes).expect("deserialize should succeed");
        assert_eq!(recovered.min_id, -100);
        assert_eq!(recovered.max_id, -1);
    }
}
