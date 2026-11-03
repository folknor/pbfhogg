//! Block-local string table used by `BlockBuilder` for tag keys/values, user
//! names, and relation roles. Index 0 is always the empty string.

use std::hash::BuildHasher;

use hashbrown::HashTable;
use rustc_hash::FxBuildHasher;

use protohoggr::encode_bytes_field_always;

use crate::PrimitiveBlock;
use crate::read::wire_schema::{primitive_block, string_table};

/// Block-local string table. Index 0 is always the empty string.
///
/// ## Storage: one arena, no allocation per string
///
/// Every string's bytes are appended to one `bytes` arena and addressed by
/// a slot, `(start, end)` offsets into it (usize, so no narrowing can alias
/// two slots however much a caller feeds one block). The dedupe `index`
/// holds slot numbers keyed by the bytes they address; lookup hashes the
/// candidate's bytes and compares against the arena. [`clear`](Self::clear)
/// truncates all three and keeps their capacity, so a builder reused across
/// blocks interns with no allocator traffic after warmup. This replaced an
/// `Rc<str>` per unique string, freed in bulk at every block end, which put
/// every encoding thread on the allocator's slow path thousands of times per
/// block (the arena locks a capped arena count turns into contention).
///
/// One hash function ([`hash`]) serves lookup, insertion and the table's
/// rehash on growth, which recovers each slot's bytes from the arena: a slot
/// number alone carries no hash.
///
/// ## Why FxHash
///
/// The index hashes with FxHash (from `rustc-hash`) rather than SipHash, for
/// rewrite throughput.
///
/// Strings include decoded PBF and OSC input, so FxHash does not provide
/// collision-attack resistance here. It is retained for the measured rewrite
/// hot path, where every tag performs a lookup. The element cap bounds normal
/// batches, not adversarial string counts: a pre-seeded table copies every
/// source slot, including unused entries. A hardened hasher would require
/// measuring rewrite CPU and allocations on representative input.
/// This is a throughput policy, not an input-safety guarantee: the reader's
/// blob byte limit bounds storage but does not prevent collision-heavy work.
/// Retain the existing hasher rather than impose a new hashing cost on every
/// tag lookup to harden pre-seeding alone. Replacing it is a separate policy
/// choice covering both `add` and `pre_seed`; an element-count bound or slot
/// deduplication is not a substitute (deduplication breaks raw sid identity).
pub(super) struct StringTable {
    bytes: Vec<u8>,
    slots: Vec<(usize, usize)>,
    index: HashTable<u32>,
}

/// The one hash of a string's bytes, for every index operation.
fn hash(bytes: &[u8]) -> u64 {
    FxBuildHasher.hash_one(bytes)
}

impl StringTable {
    pub(super) fn new() -> Self {
        let mut slots = Vec::with_capacity(256);
        // Slot zero is the dense-node end-of-tags marker, so it must not
        // be returned by add("") for an explicit empty tag key. Interning
        // an empty string deliberately creates a nonzero slot instead, and
        // slot zero is never entered in the index.
        slots.push((0, 0));
        StringTable {
            bytes: Vec::with_capacity(4096),
            slots,
            index: HashTable::with_capacity(256),
        }
    }

    /// Number of slots, slot zero included.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.slots.len()
    }

    /// The bytes of slot `idx`. Panics on an index past [`len`](Self::len),
    /// as indexing the old slot vector did.
    pub(super) fn get(&self, idx: usize) -> &[u8] {
        let (start, end) = self.slots[idx];
        &self.bytes[start..end]
    }

    /// Insert a string and return its index, or return the existing index if already present.
    ///
    /// A hit hashes the string and compares it against the arena: no
    /// allocation. A miss appends the bytes and a slot, then enters the
    /// slot in the index (appended first, so a rehash on growth can resolve
    /// it).
    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn add(&mut self, s: &str) -> u32 {
        let key = s.as_bytes();
        let h = hash(key);
        let (bytes, slots) = (&self.bytes, &self.slots);
        let slot_bytes = |i: u32| {
            let (start, end) = slots[i as usize];
            &bytes[start..end]
        };
        if let Some(&idx) = self.index.find(h, |&i| slot_bytes(i) == key) {
            return idx;
        }
        let next_idx = self.slots.len() as u32;
        self.push_slot(key);
        self.insert_index(h, next_idx);
        next_idx
    }

    fn push_slot(&mut self, key: &[u8]) {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(key);
        self.slots.push((start, self.bytes.len()));
    }

    fn insert_index(&mut self, h: u64, idx: u32) {
        let (bytes, slots) = (&self.bytes, &self.slots);
        self.index.insert_unique(h, idx, |&i| {
            let (start, end) = slots[i as usize];
            hash(&bytes[start..end])
        });
    }

    /// Pre-seed from an input block's string table, populating the index map.
    ///
    /// After pre-seeding, input index N maps to output index N (identity).
    /// Index 0 (empty string) is already present from `new()` and is skipped;
    /// the caller refuses a source whose slot zero is anything else
    /// (`BlockBuilder::pre_seed_string_table`). A table that is not fresh is
    /// cleared first, so the copy always starts at slot one.
    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn pre_seed(&mut self, block: &PrimitiveBlock) {
        if self.slots.len() != 1 {
            self.clear();
        }
        let len = block.string_table_len();
        for i in 1..len {
            if let Some(s) = block.string_table_entry(i) {
                // Raw rewrite fields retain source indices. Duplicate strings
                // and empty strings outside slot zero must occupy their source
                // slots too; interning them would shift every later index.
                // Only a string's first slot enters the index.
                let key = s.as_bytes();
                let h = hash(key);
                let (bytes, slots) = (&self.bytes, &self.slots);
                let present = self
                    .index
                    .find(h, |&j| {
                        let (start, end) = slots[j as usize];
                        &bytes[start..end] == key
                    })
                    .is_some();
                let idx = self.slots.len() as u32;
                self.push_slot(key);
                if !present {
                    self.insert_index(h, idx);
                }
            }
        }
    }

    pub(super) fn clear(&mut self) {
        self.bytes.clear();
        self.slots.clear();
        self.index.clear();
        // As in new(), leave zero out of the lookup map: dense tag keys
        // need a nonzero index even when their string is empty.
        self.slots.push((0, 0));
    }

    /// After a [`clear`](Self::clear): start over from a fresh table when the
    /// retained arena, slot vector or index has grown past `limit` bytes, so
    /// a builder kept across blocks does not pin one outlier block's table.
    /// The index's figure is an estimate (buckets times entry size, ignoring
    /// control bytes and slack), so that bound is approximate.
    pub(super) fn release_outliers(&mut self, limit: usize) {
        let slots_bytes = self.slots.capacity() * std::mem::size_of::<(usize, usize)>();
        let index_bytes = self.index.capacity() * std::mem::size_of::<u32>();
        if self.bytes.capacity() > limit || slots_bytes > limit || index_bytes > limit {
            *self = Self::new();
        }
    }

    /// Encode the string table as `PrimitiveBlock` field 1, appended to `buf`.
    ///
    /// StringTable has one field: `repeated bytes s = 1;`, one entry per
    /// slot, slot zero, duplicate and empty slots included (raw sid identity
    /// depends on every one). The body's length is computed from the slots
    /// first, so the entries are written once, straight into `buf`, behind
    /// their length prefix. Staging the body in a scratch buffer and copying
    /// it would keep a second retained buffer the size of the largest table
    /// the builder met: rewritten blocks keep their input block's whole
    /// table, and at planet that staging buffer was the largest retained
    /// buffer of apply-changes' rewrite workers.
    ///
    /// The body is never empty (slot zero alone takes two bytes), so the
    /// field is always written, as the skip-if-empty encoder this replaced
    /// always wrote it.
    pub(super) fn encode_to(&self, buf: &mut Vec<u8>) -> crate::error::Result<()> {
        use super::encode::len_field_size;
        let overflow = || crate::error::Error::internal("string table length overflows usize");
        let mut body_len = 0usize;
        for &(start, end) in &self.slots {
            body_len = len_field_size(end - start)
                .and_then(|n| body_len.checked_add(n))
                .ok_or_else(overflow)?;
        }
        let total = len_field_size(body_len).ok_or_else(overflow)?;
        buf.reserve(total);
        protohoggr::encode_tag(buf, primitive_block::STRING_TABLE, protohoggr::WIRE_LEN);
        protohoggr::encode_varint(buf, body_len as u64);
        let body_start = buf.len();
        for &(start, end) in &self.slots {
            encode_bytes_field_always(buf, string_table::STRINGS, &self.bytes[start..end]);
        }
        // The length prefix went out before the body, so a body of another
        // length is a corrupt block. Refused here, the encode fails and the
        // block is never handed out.
        let written = buf.len() - body_start;
        if written != body_len {
            return Err(crate::error::Error::internal(format!(
                "string table body is {written} bytes but its length prefix says {body_len}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protohoggr::encode_bytes_field;

    /// The staged encoding `encode_to` replaced: the body built in a scratch
    /// buffer, then copied behind its length.
    fn staged(table: &StringTable) -> Vec<u8> {
        let mut body = Vec::new();
        for i in 0..table.len() {
            encode_bytes_field_always(&mut body, string_table::STRINGS, table.get(i));
        }
        let mut out = b"prefix".to_vec();
        encode_bytes_field(&mut out, primitive_block::STRING_TABLE, &body);
        out
    }

    /// The direct encoding is byte-identical to the staged one across the
    /// varint width transitions of an entry's length (127/128, 16383/16384)
    /// and of the body's (the table grows through 128 and 16384 bytes), with
    /// duplicate and empty pre-seeded slots kept, and appends after what the
    /// buffer already holds.
    #[test]
    fn direct_encoding_matches_the_staged_one_across_length_widths() {
        let long = |n: usize| "x".repeat(n);
        let mut strings = Vec::new();
        for s in ["", "key", "", "key"] {
            encode_bytes_field_always(&mut strings, string_table::STRINGS, s.as_bytes());
        }
        let mut bytes = Vec::new();
        encode_bytes_field(&mut bytes, primitive_block::STRING_TABLE, &strings);
        let block = PrimitiveBlock::from_vec(bytes).expect("valid string table");
        let mut table = StringTable::new();
        table.pre_seed(&block);
        let check = |table: &StringTable| {
            let mut out = b"prefix".to_vec();
            table.encode_to(&mut out).expect("encodes");
            assert_eq!(out, staged(table));
        };
        check(&table);
        for n in [
            1, 126, 127, 128, 129, 16_382, 16_383, 16_384, 16_385, 300_000,
        ] {
            table.add(&long(n));
            check(&table);
        }
        let mut fresh = StringTable::new();
        check(&fresh);
        for i in 0..40 {
            fresh.add(&format!("s{i}"));
            check(&fresh);
        }
    }

    #[test]
    fn pre_seed_preserves_duplicate_and_empty_slots() {
        let mut strings = Vec::new();
        for s in ["", "key", "value", "key", "", "tail"] {
            encode_bytes_field_always(&mut strings, string_table::STRINGS, s.as_bytes());
        }
        let mut bytes = Vec::new();
        encode_bytes_field(&mut bytes, primitive_block::STRING_TABLE, &strings);
        let block = PrimitiveBlock::from_vec(bytes).expect("valid string table");
        let mut table = StringTable::new();
        table.pre_seed(&block);
        assert_eq!(table.len(), block.string_table_len());
        for i in 0..table.len() {
            assert_eq!(
                Some(table.get(i)),
                block.string_table_entry(i).map(str::as_bytes)
            );
        }
        assert_eq!(table.add("key"), 1);
        assert_eq!(table.add(""), 4);
        assert_eq!(table.add("tail"), 5);
        assert_eq!(table.add("new"), 6);
    }

    #[test]
    fn explicit_empty_string_has_a_nonzero_slot_after_new_and_clear() {
        let mut table = StringTable::new();
        assert_eq!(table.add(""), 1);
        assert_eq!(table.add(""), 1);
        table.add("old");
        table.clear();
        assert_eq!(table.add(""), 1);
        assert_eq!(table.add("new"), 2);
        assert_eq!(table.len(), 3);
    }

    /// The index survives many growths with every slot still found by
    /// content (the rehash recovers each slot's bytes from the arena), and
    /// a cleared table interns again from slot one.
    #[test]
    fn lookups_survive_index_growth_and_clear() {
        let mut table = StringTable::new();
        let words: Vec<String> = (0..20_000).map(|i| format!("w{i}")).collect();
        for (n, w) in words.iter().enumerate() {
            assert_eq!(table.add(w) as usize, n + 1);
        }
        for (n, w) in words.iter().enumerate() {
            assert_eq!(
                table.add(w) as usize,
                n + 1,
                "{w} must be found after growth"
            );
            assert_eq!(table.get(n + 1), w.as_bytes());
        }
        table.clear();
        assert_eq!(table.len(), 1);
        assert_eq!(table.add("w7"), 1);
        assert_eq!(table.add("w8"), 2);
        assert_eq!(table.add("w7"), 1);
    }
}
