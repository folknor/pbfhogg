//! Block-local string table used by `BlockBuilder` for tag keys/values, user
//! names, and relation roles. Index 0 is always the empty string.

use std::rc::Rc;

use rustc_hash::FxHashMap;

use protohoggr::{encode_bytes_field, encode_bytes_field_always};

use crate::PrimitiveBlock;
use crate::read::wire_schema::{primitive_block, string_table};

/// Block-local string table. Index 0 is always the empty string.
///
/// ## Why FxHashMap instead of std HashMap
///
/// The `index` map uses `FxHashMap` (from `rustc-hash`) instead of the standard
/// `HashMap` with `SipHash` for rewrite throughput.
///
/// Strings include decoded PBF and OSC input, so FxHash does not provide
/// collision-attack resistance here. It is retained for the measured rewrite
/// hot path, where every tag performs a lookup. The element cap bounds normal
/// batches, not adversarial string counts: a pre-seeded table copies every
/// source slot, including unused entries. A hardened hasher would require
/// measuring rewrite CPU and allocations on representative input.
pub(super) struct StringTable {
    pub(super) strings: Vec<Rc<str>>,
    index: FxHashMap<Rc<str>, u32>,
    empty: Rc<str>,
}

impl StringTable {
    pub(super) fn new() -> Self {
        let empty: Rc<str> = Rc::from("");
        let mut st = StringTable {
            strings: Vec::with_capacity(256),
            index: FxHashMap::with_capacity_and_hasher(256, Default::default()),
            empty: Rc::clone(&empty),
        };
        // Slot zero is the dense-node end-of-tags marker, so it must not
        // be returned by add("") for an explicit empty tag key. Interning
        // an empty string deliberately creates a nonzero slot instead.
        st.strings.push(empty); // index 0 = empty string
        st
    }

    /// Insert a string and return its index, or return the existing index if already present.
    ///
    /// ## Fast path (cache hit, ~99% of calls)
    ///
    /// `self.index.get(s)` looks up the `&str` directly via the `Borrow` trait -
    /// no allocation, just FxHash + probe. At the default element cap
    /// (`super::block_builder::DEFAULT_ELEMENT_CAP`), typical blocks reuse
    /// strings across many tags, so the vast majority are cache hits.
    ///
    /// ## Slow path (cache miss, ~1% of calls)
    ///
    /// On the first occurrence of a string, allocates a single `Rc<str>` shared
    /// between the HashMap key and the Vec entry. `Rc::clone` is just a refcount
    /// bump - one heap allocation per unique string total.
    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn add(&mut self, s: &str) -> u32 {
        // Fast path: string already interned - hash-only lookup, no allocation.
        if let Some(&idx) = self.index.get(s) {
            return idx;
        }
        // Slow path: first occurrence - single Rc<str> allocation, shared
        // between the Vec and HashMap (Rc::clone is just a refcount bump).
        let next_idx = self.strings.len() as u32;
        let rc: Rc<str> = Rc::from(s);
        self.strings.push(Rc::clone(&rc));
        self.index.insert(rc, next_idx);
        next_idx
    }

    /// Pre-seed from an input block's string table, populating the index map.
    ///
    /// After pre-seeding, input index N maps to output index N (identity).
    /// Index 0 (empty string) is already present from `new()` and is skipped.
    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn pre_seed(&mut self, block: &PrimitiveBlock) {
        debug_assert_eq!(self.strings.len(), 1, "pre-seed requires a fresh table");
        let len = block.string_table_len();
        for i in 1..len {
            if let Some(s) = block.string_table_entry(i) {
                // Raw rewrite fields retain source indices. Duplicate strings
                // and empty strings outside slot zero must occupy their source
                // slots too; interning them would shift every later index.
                let rc: Rc<str> = Rc::from(s);
                self.strings.push(Rc::clone(&rc));
                self.index.entry(rc).or_insert(i as u32);
            }
        }
    }

    pub(super) fn clear(&mut self) {
        self.strings.clear();
        self.index.clear();
        // As in new(), leave zero out of the lookup map: dense tag keys
        // need a nonzero index even when their string is empty.
        self.strings.push(Rc::clone(&self.empty));
    }

    /// Encode the string table directly to wire format bytes.
    ///
    /// StringTable has one field: `repeated bytes s = 1;`
    pub(super) fn encode_to(&self, buf: &mut Vec<u8>, scratch: &mut Vec<u8>) {
        scratch.clear();
        for s in &self.strings {
            encode_bytes_field_always(scratch, string_table::STRINGS, s.as_bytes());
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
        assert_eq!(table.strings.len(), block.string_table_len());
        for (i, s) in table.strings.iter().enumerate() {
            assert_eq!(Some(s.as_ref()), block.string_table_entry(i));
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
        assert_eq!(table.strings.len(), 3);
    }
}
