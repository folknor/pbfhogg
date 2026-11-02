//! Block-local string table used by `BlockBuilder` for tag keys/values, user
//! names, and relation roles. Index 0 is always the empty string.

use std::hash::BuildHasher;

use hashbrown::HashTable;
use rustc_hash::FxBuildHasher;

use protohoggr::{encode_bytes_field, encode_bytes_field_always};

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
    /// Index 0 (empty string) is already present from `new()` and is skipped.
    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn pre_seed(&mut self, block: &PrimitiveBlock) {
        debug_assert_eq!(self.slots.len(), 1, "pre-seed requires a fresh table");
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

    /// Encode the string table directly to wire format bytes.
    ///
    /// StringTable has one field: `repeated bytes s = 1;`
    pub(super) fn encode_to(&self, buf: &mut Vec<u8>, scratch: &mut Vec<u8>) {
        scratch.clear();
        for &(start, end) in &self.slots {
            encode_bytes_field_always(scratch, string_table::STRINGS, &self.bytes[start..end]);
        }
        encode_bytes_field(buf, primitive_block::STRING_TABLE, scratch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
