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
// Lightweight protobuf scanner: extract element type + ID range
// without full PrimitiveBlock parsing.
// ---------------------------------------------------------------------------

/// Scan decompressed PrimitiveBlock bytes to extract element type and ID range.
/// This walks the protobuf wire format manually, only reading element IDs.
/// Much cheaper than a full PrimitiveBlock parse (skips string tables,
/// coordinates, tags, metadata, etc.).
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn scan_block_ids(raw: &[u8]) -> Option<BlobIndex> {
    let mut cursor = 0;
    let mut result: Option<BlobIndex> = None;

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
fn scan_primitive_group(raw: &[u8], mut cursor: usize, end: usize) -> Option<BlobIndex> {
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
fn scan_dense_node_ids(raw: &[u8], mut cursor: usize, end: usize) -> Option<BlobIndex> {
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
) -> Option<BlobIndex> {
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

    Some(BlobIndex {
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
        1 => Some(cursor + 8), // 64-bit fixed
        2 => {
            // Length-delimited
            let (len, new_pos) = read_varint(raw, cursor)?;
            Some(new_pos + len as usize)
        }
        5 => Some(cursor + 4), // 32-bit fixed
        _ => None,             // Unknown wire type
    }
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
