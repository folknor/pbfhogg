//! Block-local string table used by `BlockBuilder` for tag keys/values, user
//! names, and relation roles. Index 0 is always the empty string.

use std::rc::Rc;

use rustc_hash::FxHashMap;

use protohoggr::{encode_bytes_field, encode_bytes_field_always};

use crate::PrimitiveBlock;

/// Block-local string table. Index 0 is always the empty string.
///
/// ## Why FxHashMap instead of std HashMap
///
/// The `index` map uses `FxHashMap` (from `rustc-hash`) instead of the standard
/// `HashMap` with `SipHash`. This is safe and beneficial here because:
///
/// **Safety:** This is a write-side-only data structure. All strings inserted
/// come from the caller's in-process data (tag keys, tag values, role strings,
/// user names) - never from untrusted PBF input. There is no risk of
/// HashDoS attacks, which is the sole reason the standard library defaults to
/// the slower SipHash-1-3 hasher.
///
/// **Performance:** FxHash is a simple, non-cryptographic hash (multiply +
/// rotate) that is substantially faster than SipHash for short strings - which
/// is exactly what OSM tag keys/values are (typically 3-30 bytes: "name",
/// "highway", "building", "residential", etc.). The string table is on the hot
/// path of PBF writing: every tag on every element does a hash lookup + possible
/// insert. In profiling, the hasher shows up as a measurable fraction of write
/// time, so switching to FxHash gives a meaningful speedup.
///
/// **Where NOT to use FxHash:** On the *read* side (e.g. if you were building a
/// lookup table from PBF data), strings come from untrusted input files that
/// could be adversarially crafted. In that context, SipHash (or ahash, which
/// also has DoS resistance) should be used to prevent O(n^2) hash collisions.
///
/// **Alternatives considered:**
/// - `ahash`: Also fast and DoS-resistant, but the DoS resistance is
///   unnecessary overhead here since we control the input. FxHash is simpler
///   and marginally faster for the short-string workload.
/// - `IndexMap`: Preserves insertion order (which we need via `self.strings`),
///   but wrapping an IndexMap would still need a fast hasher, and we already
///   maintain the ordered Vec separately. Switching would add a dependency for
///   no net benefit.
/// - Custom perfect hashing: Not viable because the string set is dynamic -
///   we do not know all strings upfront.
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
        st.strings.push(empty); // index 0 = empty string
        st
    }

    /// Insert a string and return its index, or return the existing index if already present.
    ///
    /// ## Fast path (cache hit, ~99% of calls)
    ///
    /// `self.index.get(s)` looks up the `&str` directly via the `Borrow` trait -
    /// no allocation, just FxHash + probe. This is the hot path: a typical 8000-
    /// element block has ~1200 unique strings but ~16,000+ add() calls, so the
    /// vast majority are cache hits.
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
    pub(super) fn pre_seed(&mut self, block: &PrimitiveBlock) {
        let len = block.string_table_len();
        for i in 1..len {
            if let Some(s) = block.string_table_entry(i) {
                self.add(s);
            }
        }
    }

    pub(super) fn clear(&mut self) {
        self.strings.clear();
        self.index.clear();
        self.strings.push(Rc::clone(&self.empty));
    }

    /// Encode the string table directly to wire format bytes.
    ///
    /// StringTable has one field: `repeated bytes s = 1;`
    pub(super) fn encode_to(&self, buf: &mut Vec<u8>, scratch: &mut Vec<u8>) {
        scratch.clear();
        for s in &self.strings {
            encode_bytes_field_always(scratch, 1, s.as_bytes());
        }
        encode_bytes_field(buf, 1, scratch);
    }
}
