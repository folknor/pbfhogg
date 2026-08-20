//! Per-blob tag key index: lightweight scanning and serialization.
//!
//! Stored in BlobHeader field 4 as a variable-length binary blob.
//! Used by the pipeline to skip decompression of blobs that provably
//! lack required tag keys (e.g. a blob with no `highway` key can be
//! skipped when filtering for `highway=primary`).

use protohoggr::Cursor;

/// Tag key index version.
pub(crate) const TAG_INDEX_VERSION: u8 = 0x01;

/// Largest key count the wire format can represent (`key_count` is u16 LE).
const TAG_INDEX_MAX_KEYS: usize = u16::MAX as usize;

/// Largest single key length the wire format can represent (`key_len` is u16 LE).
const TAG_INDEX_MAX_KEY_LEN: usize = u16::MAX as usize;

/// Encode a sorted key set into the tag index wire format.
///
/// Format: version (u8) + key_count (u16 LE) + repeated [key_len (u16 LE) +
/// key bytes].
///
/// Returns `None` when the key set cannot be represented faithfully - more
/// than [`TAG_INDEX_MAX_KEYS`] keys, or any key longer than
/// [`TAG_INDEX_MAX_KEY_LEN`] bytes. Both bounds used to be silent `as u16`
/// truncations at the producer, which is the worst possible failure mode for
/// this structure: a wrapped `key_len` desynchronizes `deserialize` (garbage
/// keys from that point on), and a wrapped `key_count` yields a *shorter* key
/// set that still parses cleanly, so `tags_filter` skips blobs whose keys were
/// dropped and reports a cheerful "N blobs skipped by tag index" while
/// silently omitting matches. The tag index is a pure optimization - a blob
/// with no tagdata is decoded rather than skipped - so emitting nothing is
/// always correct where emitting a lie is not.
///
/// This is the single encoder. `BlockBuilder::encode_block` builds its tagdata
/// straight from string-table indices without materializing a [`TagIndex`],
/// and routes through here rather than carrying its own copy of the format.
pub(crate) fn encode_tag_index<'a, I>(keys: I) -> Option<Vec<u8>>
where
    I: ExactSizeIterator<Item = &'a [u8]>,
{
    let count = keys.len();
    if count > TAG_INDEX_MAX_KEYS {
        return None;
    }
    let mut buf = Vec::with_capacity(3 + count * 2);
    buf.push(TAG_INDEX_VERSION);
    #[allow(clippy::cast_possible_truncation)]
    buf.extend_from_slice(&(count as u16).to_le_bytes());
    let mut written = 0_usize;
    for key in keys {
        if key.len() > TAG_INDEX_MAX_KEY_LEN {
            return None;
        }
        #[allow(clippy::cast_possible_truncation)]
        buf.extend_from_slice(&(key.len() as u16).to_le_bytes());
        buf.extend_from_slice(key);
        written += 1;
    }
    // A mismatch here would write a count that does not describe the payload.
    // `ExactSizeIterator` promises this cannot happen; refuse to emit rather
    // than trust an unsound implementation.
    if written != count {
        return None;
    }
    Some(buf)
}

/// Per-blob tag key index: the set of unique tag keys present in a blob.
#[derive(Debug, Clone)]
pub(crate) struct TagIndex {
    /// Sorted unique tag key byte strings.
    keys: Vec<Box<[u8]>>,
}

impl TagIndex {
    /// Serialize to the tag index wire format.
    ///
    /// Returns `None` when the key set exceeds what the format can represent -
    /// see [`encode_tag_index`] for why emitting nothing beats emitting a
    /// truncated index.
    #[hotpath::measure]
    pub fn serialize(&self) -> Option<Vec<u8>> {
        encode_tag_index(self.keys.iter().map(std::convert::AsRef::as_ref))
    }

    /// Deserialize from tag index bytes (BlobHeader field 4).
    ///
    /// Returns `None` if the data is invalid or has an unrecognized version.
    pub fn deserialize(data: &[u8]) -> Option<Self> {
        if data.len() < 3 {
            return None;
        }
        if data[0] != TAG_INDEX_VERSION {
            return None;
        }
        let key_count = u16::from_le_bytes(data[1..3].try_into().ok()?) as usize;
        let mut pos = 3;
        let mut keys = Vec::with_capacity(key_count);
        for _ in 0..key_count {
            if pos + 2 > data.len() {
                return None;
            }
            let key_len = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
            pos += 2;
            if pos + key_len > data.len() {
                return None;
            }
            keys.push(data[pos..pos + key_len].into());
            pos += key_len;
        }
        Some(TagIndex { keys })
    }

    /// Returns `true` if the tag index has no keys (blob contains no tagged elements).
    pub fn keys_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The sorted, deduplicated key set.
    ///
    /// Exposed for `check --index`, which diffs a stored tag index against the
    /// one it scans out of the blob itself and therefore needs the key set as
    /// a set rather than as a membership predicate.
    pub(crate) fn keys(&self) -> &[Box<[u8]>] {
        &self.keys
    }

    /// Returns `true` if any of the given keys is present in this tag index.
    pub fn has_any_key(&self, required: &[Box<[u8]>]) -> bool {
        required
            .iter()
            .any(|rk| self.keys.binary_search(rk).is_ok())
    }

    /// Returns `true` if any key in this tag index starts with any of the given prefixes.
    pub fn has_any_prefix(&self, prefixes: &[Box<[u8]>]) -> bool {
        prefixes.iter().any(|prefix| {
            let idx = self.keys.partition_point(|k| k.as_ref() < prefix.as_ref());
            idx < self.keys.len() && self.keys[idx].starts_with(prefix)
        })
    }
}

/// Parse a StringTable message into byte slices for each entry.
///
/// StringTable is PrimitiveBlock field 1. Inside, each entry is field 1 (bytes).
fn parse_string_table(raw: &[u8]) -> Option<Vec<&[u8]>> {
    let mut cur = Cursor::new(raw);
    let mut entries = Vec::new();
    while let Some((tag, wire_type)) = cur.read_tag().ok()? {
        if tag == 1 && wire_type == 2 {
            entries.push(cur.read_len_delimited().ok()?);
        } else {
            cur.skip_field(wire_type).ok()?;
        }
    }
    Some(entries)
}

/// Scan decompressed PrimitiveBlock bytes to extract the set of unique tag keys.
///
/// Walks the protobuf wire format, parsing the StringTable and tag key indices
/// from all PrimitiveGroups. Returns a `TagIndex` with sorted unique keys.
///
/// Returns `None` if the block has no groups or cannot be parsed.
#[hotpath::measure]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn scan_block_tags(raw: &[u8]) -> Option<TagIndex> {
    let mut cur = Cursor::new(raw);
    let mut string_table_data: Option<&[u8]> = None;
    let mut groups: Vec<&[u8]> = Vec::new();

    while let Some((tag, wire_type)) = cur.read_tag().ok()? {
        match (tag, wire_type) {
            (1, 2) => {
                // StringTable (field 1, length-delimited)
                string_table_data = Some(cur.read_len_delimited().ok()?);
            }
            (2, 2) => {
                // PrimitiveGroup (field 2, length-delimited)
                groups.push(cur.read_len_delimited().ok()?);
            }
            _ => {
                cur.skip_field(wire_type).ok()?;
            }
        }
    }

    let string_table = parse_string_table(string_table_data?)?;
    if groups.is_empty() {
        return None;
    }

    let mut key_indices: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for group_data in groups {
        // A group carrying more than one element field is not something this
        // scanner can index: it stops at the first element field it finds, so
        // the off-kind elements' keys would be missing from the index while
        // the index still looks complete. `tags_filter` skips whole blobs on
        // that index, so an incomplete one silently drops matches. Refuse to
        // emit a tag index at all for such a blob - no tagdata means "decode
        // this blob", which is always correct, just slower.
        if super::scan_ids::group_mixes_element_kinds(group_data) {
            return None;
        }
        scan_group_tag_keys(group_data, &mut key_indices);
    }

    // Resolve indices to string table entries
    let mut keys: Vec<Box<[u8]>> = key_indices
        .into_iter()
        .filter_map(|idx| {
            let i = idx as usize;
            if i < string_table.len() && !string_table[i].is_empty() {
                Some(string_table[i].into())
            } else {
                None
            }
        })
        .collect();
    keys.sort();
    keys.dedup();

    Some(TagIndex { keys })
}

/// Scan a PrimitiveGroup for tag key string-table indices.
fn scan_group_tag_keys(raw: &[u8], key_indices: &mut std::collections::HashSet<u32>) {
    let mut cur = Cursor::new(raw);
    while let Some((tag, wire_type)) = cur.read_tag().ok().flatten() {
        match (tag, wire_type) {
            (2, 2) => {
                // DenseNodes (field 2)
                if let Ok(data) = cur.read_len_delimited() {
                    scan_dense_node_tag_keys(data, key_indices);
                }
                return;
            }
            (1, 2) | (3, 2) | (4, 2) => {
                // Node (1), Way (3), Relation (4)
                if let Ok(msg) = cur.read_len_delimited() {
                    scan_element_tag_keys(msg, key_indices);
                }
                // Continue scanning remaining elements of same type
                scan_remaining_element_tag_keys(&mut cur, tag, key_indices);
                return;
            }
            _ => {
                if cur.skip_field(wire_type).is_err() {
                    return;
                }
            }
        }
    }
}

/// Scan DenseNodes keys_vals (field 10) for tag key indices.
///
/// Format: interleaved [key_sid, val_sid, key_sid, val_sid, ..., 0, ...]
/// where 0 separates nodes. Keys are at even positions within each node's
/// tag sequence (before the 0 delimiter).
fn scan_dense_node_tag_keys(raw: &[u8], key_indices: &mut std::collections::HashSet<u32>) {
    let mut cur = Cursor::new(raw);
    while let Some((tag, wire_type)) = cur.read_tag().ok().flatten() {
        if tag == 10 && wire_type == 2 {
            // keys_vals packed field
            if let Ok(data) = cur.read_len_delimited() {
                let mut kv_cur = Cursor::new(data);
                let mut is_key = true;
                while !kv_cur.is_empty() {
                    if let Ok(val) = kv_cur.read_varint() {
                        #[allow(clippy::cast_possible_truncation)]
                        let val = val as u32;
                        if val == 0 {
                            // Delimiter: next node's tags start
                            is_key = true;
                        } else if is_key {
                            key_indices.insert(val);
                            is_key = false;
                        } else {
                            // Value - skip
                            is_key = true;
                        }
                    } else {
                        break;
                    }
                }
            }
            return;
        }
        if cur.skip_field(wire_type).is_err() {
            return;
        }
    }
}

/// Scan a Way/Relation/Node message for tag key indices (field 2 = keys packed uint32).
fn scan_element_tag_keys(msg: &[u8], key_indices: &mut std::collections::HashSet<u32>) {
    let mut cur = Cursor::new(msg);
    while let Some((tag, wire_type)) = cur.read_tag().ok().flatten() {
        if tag == 2 && wire_type == 2 {
            // keys: packed uint32
            if let Ok(data) = cur.read_len_delimited() {
                let mut k_cur = Cursor::new(data);
                while !k_cur.is_empty() {
                    if let Ok(val) = k_cur.read_varint() {
                        #[allow(clippy::cast_possible_truncation)]
                        key_indices.insert(val as u32);
                    } else {
                        break;
                    }
                }
            }
            return;
        }
        if cur.skip_field(wire_type).is_err() {
            return;
        }
    }
}

/// Scan remaining element messages in a group after the first.
fn scan_remaining_element_tag_keys(
    cur: &mut Cursor<'_>,
    expected_tag: u32,
    key_indices: &mut std::collections::HashSet<u32>,
) {
    while let Some((tag, wire_type)) = cur.read_tag().ok().flatten() {
        if tag == expected_tag && wire_type == 2 {
            if let Ok(msg) = cur.read_len_delimited() {
                scan_element_tag_keys(msg, key_indices);
            }
        } else if cur.skip_field(wire_type).is_err() {
            return;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::blob_meta::BlobFilter;

    #[test]
    fn tag_index_roundtrip_empty() {
        let ti = TagIndex { keys: Vec::new() };
        let bytes = ti.serialize().expect("empty key set is encodable");
        let recovered = TagIndex::deserialize(&bytes).expect("should deserialize");
        assert!(recovered.keys.is_empty());
    }

    #[test]
    fn tag_index_roundtrip_single_key() {
        let ti = TagIndex {
            keys: vec![b"highway"[..].into()],
        };
        let bytes = ti.serialize().expect("one short key is encodable");
        let recovered = TagIndex::deserialize(&bytes).expect("should deserialize");
        assert_eq!(recovered.keys.len(), 1);
        assert_eq!(&*recovered.keys[0], b"highway");
    }

    #[test]
    fn tag_index_roundtrip_many_keys() {
        let ti = TagIndex {
            keys: vec![
                b"addr:city"[..].into(),
                b"amenity"[..].into(),
                b"highway"[..].into(),
                b"name"[..].into(),
            ],
        };
        let bytes = ti.serialize().expect("four short keys are encodable");
        let recovered = TagIndex::deserialize(&bytes).expect("should deserialize");
        assert_eq!(recovered.keys.len(), 4);
        assert_eq!(&*recovered.keys[0], b"addr:city");
        assert_eq!(&*recovered.keys[1], b"amenity");
        assert_eq!(&*recovered.keys[2], b"highway");
        assert_eq!(&*recovered.keys[3], b"name");
    }

    #[test]
    fn tag_index_deserialize_rejects_bad_version() {
        let mut bytes = TagIndex { keys: Vec::new() }
            .serialize()
            .expect("empty key set is encodable");
        bytes[0] = 0xFF;
        assert!(TagIndex::deserialize(&bytes).is_none());
    }

    #[test]
    fn tag_index_deserialize_rejects_truncated() {
        assert!(TagIndex::deserialize(&[0x01]).is_none());
        assert!(TagIndex::deserialize(&[]).is_none());
    }

    // --- Encoder bounds -------------------------------------------------

    #[test]
    fn encode_refuses_a_key_longer_than_the_length_field() {
        // 65536 bytes wraps `key_len as u16` to 0, after which `deserialize`
        // reads the key's own bytes as the next key_len and yields garbage.
        let huge = vec![b'k'; TAG_INDEX_MAX_KEY_LEN + 1];
        assert!(
            encode_tag_index(std::iter::once(huge.as_slice())).is_none(),
            "a key the length field cannot express must produce no index"
        );

        // Passing counterpart at the exact boundary: one byte shorter is
        // representable and must still encode and round-trip, so this is not
        // satisfied by a refuse-everything regression.
        let at_limit = vec![b'k'; TAG_INDEX_MAX_KEY_LEN];
        let bytes = encode_tag_index(std::iter::once(at_limit.as_slice()))
            .expect("a key of exactly the maximum length is representable");
        let recovered = TagIndex::deserialize(&bytes).expect("should deserialize");
        assert_eq!(recovered.keys.len(), 1);
        assert_eq!(recovered.keys[0].len(), TAG_INDEX_MAX_KEY_LEN);
    }

    #[test]
    fn encode_refuses_more_keys_than_the_count_field() {
        // 65536 keys wraps `key_count as u16` to 0. That is the dangerous
        // half: the result still parses, as an index with no keys, so
        // `tags_filter` skips the blob entirely and drops every match in it.
        let keys: Vec<Vec<u8>> = (0..=TAG_INDEX_MAX_KEYS)
            .map(|i| i.to_string().into_bytes())
            .collect();
        assert!(
            encode_tag_index(keys.iter().map(std::vec::Vec::as_slice)).is_none(),
            "a key count the count field cannot express must produce no index"
        );

        let at_limit = &keys[..TAG_INDEX_MAX_KEYS];
        let bytes = encode_tag_index(at_limit.iter().map(std::vec::Vec::as_slice))
            .expect("exactly the maximum key count is representable");
        let recovered = TagIndex::deserialize(&bytes).expect("should deserialize");
        assert_eq!(recovered.keys.len(), TAG_INDEX_MAX_KEYS);
    }

    // --- Mixed-kind groups ----------------------------------------------

    #[test]
    fn a_group_mixing_element_kinds_yields_no_tag_index() {
        // PrimitiveBlock: StringTable ["", "highway", "boundary"], one group
        // holding a Way (field 3) with key 1 and a Relation (field 4) with
        // key 2. `scan_group_tag_keys` stops after the ways, so the index
        // would claim {"highway"} and `tags_filter -e boundary=...` would skip
        // the blob and lose the relation.
        let mut st = Vec::new();
        for s in [
            b"".as_slice(),
            b"highway".as_slice(),
            b"boundary".as_slice(),
        ] {
            st.push(0x0A);
            st.push(u8::try_from(s.len()).unwrap());
            st.extend_from_slice(s);
        }
        // Way message: field 2 (keys) packed [1].
        let way = [0x12u8, 0x01, 0x01];
        // Relation message: field 2 (keys) packed [2].
        let rel = [0x12u8, 0x01, 0x02];
        let mut group = Vec::new();
        group.push(0x1A); // field 3, LEN
        group.push(u8::try_from(way.len()).unwrap());
        group.extend_from_slice(&way);
        group.push(0x22); // field 4, LEN
        group.push(u8::try_from(rel.len()).unwrap());
        group.extend_from_slice(&rel);

        let mut block = Vec::new();
        block.push(0x0A); // PrimitiveBlock field 1, StringTable
        block.push(u8::try_from(st.len()).unwrap());
        block.extend_from_slice(&st);
        block.push(0x12); // PrimitiveBlock field 2, PrimitiveGroup
        block.push(u8::try_from(group.len()).unwrap());
        block.extend_from_slice(&group);

        assert!(
            scan_block_tags(&block).is_none(),
            "a group whose off-kind elements' keys the scanner cannot see must \
             produce no tag index; an incomplete index makes tags-filter skip \
             blobs that do contain matches"
        );

        // Passing counterpart: the same block with the relation removed is a
        // clean single-kind group and must still produce {"highway"}.
        let mut clean_group = Vec::new();
        clean_group.push(0x1A);
        clean_group.push(u8::try_from(way.len()).unwrap());
        clean_group.extend_from_slice(&way);
        let mut clean_block = Vec::new();
        clean_block.push(0x0A);
        clean_block.push(u8::try_from(st.len()).unwrap());
        clean_block.extend_from_slice(&st);
        clean_block.push(0x12);
        clean_block.push(u8::try_from(clean_group.len()).unwrap());
        clean_block.extend_from_slice(&clean_group);
        let ti = scan_block_tags(&clean_block).expect("a single-kind group still indexes");
        assert!(ti.has_any_key(&[b"highway"[..].into()]));
    }

    #[test]
    fn tag_index_has_any_key() {
        let ti = TagIndex {
            keys: vec![
                b"amenity"[..].into(),
                b"highway"[..].into(),
                b"name"[..].into(),
            ],
        };
        assert!(ti.has_any_key(&[b"highway"[..].into()]));
        assert!(ti.has_any_key(&[b"building"[..].into(), b"amenity"[..].into()]));
        assert!(!ti.has_any_key(&[b"building"[..].into()]));
        assert!(!ti.has_any_key(&[]));
    }

    #[test]
    fn tag_index_has_any_prefix() {
        let ti = TagIndex {
            keys: vec![
                b"addr:city"[..].into(),
                b"addr:street"[..].into(),
                b"highway"[..].into(),
            ],
        };
        assert!(ti.has_any_prefix(&[b"addr:"[..].into()]));
        assert!(ti.has_any_prefix(&[b"high"[..].into()]));
        assert!(!ti.has_any_prefix(&[b"building"[..].into()]));
        assert!(!ti.has_any_prefix(&[]));
    }

    #[test]
    fn wants_tag_index_no_filter_passes() {
        let filter = BlobFilter::new(true, true, true);
        let ti = TagIndex {
            keys: vec![b"highway"[..].into()],
        };
        assert!(
            filter.wants_tag_index(&ti),
            "no filter configured → always pass"
        );
    }

    #[test]
    fn wants_tag_index_key_match() {
        let filter =
            BlobFilter::new(true, true, true).with_required_tag_keys(vec!["highway".to_string()]);
        let ti = TagIndex {
            keys: vec![b"highway"[..].into(), b"name"[..].into()],
        };
        assert!(filter.wants_tag_index(&ti));
    }

    #[test]
    fn wants_tag_index_key_no_match() {
        let filter =
            BlobFilter::new(true, true, true).with_required_tag_keys(vec!["amenity".to_string()]);
        let ti = TagIndex {
            keys: vec![b"highway"[..].into(), b"name"[..].into()],
        };
        assert!(!filter.wants_tag_index(&ti));
    }

    #[test]
    fn wants_tag_index_prefix_match() {
        let filter =
            BlobFilter::new(true, true, true).with_required_tag_prefixes(vec!["addr:".to_string()]);
        let ti = TagIndex {
            keys: vec![b"addr:street"[..].into(), b"name"[..].into()],
        };
        assert!(filter.wants_tag_index(&ti));
    }

    #[test]
    fn wants_tag_index_both_configured_key_matches() {
        // Both keys and prefixes configured; key matches but prefix doesn't
        let filter = BlobFilter::new(true, true, true)
            .with_required_tag_keys(vec!["highway".to_string()])
            .with_required_tag_prefixes(vec!["addr:".to_string()]);
        let ti = TagIndex {
            keys: vec![b"highway"[..].into(), b"name"[..].into()],
        };
        assert!(
            filter.wants_tag_index(&ti),
            "key match should pass even if prefix doesn't"
        );
    }

    #[test]
    fn wants_tag_index_both_configured_prefix_matches() {
        // Both keys and prefixes configured; prefix matches but key doesn't
        let filter = BlobFilter::new(true, true, true)
            .with_required_tag_keys(vec!["amenity".to_string()])
            .with_required_tag_prefixes(vec!["addr:".to_string()]);
        let ti = TagIndex {
            keys: vec![b"addr:city"[..].into(), b"name"[..].into()],
        };
        assert!(
            filter.wants_tag_index(&ti),
            "prefix match should pass even if key doesn't"
        );
    }

    #[test]
    fn wants_tag_index_both_configured_neither_matches() {
        let filter = BlobFilter::new(true, true, true)
            .with_required_tag_keys(vec!["amenity".to_string()])
            .with_required_tag_prefixes(vec!["addr:".to_string()]);
        let ti = TagIndex {
            keys: vec![b"highway"[..].into(), b"name"[..].into()],
        };
        assert!(
            !filter.wants_tag_index(&ti),
            "neither key nor prefix matches → reject"
        );
    }
}
