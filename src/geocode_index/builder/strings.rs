//! Interned string pool used by the builder.
//!
//! The pool is `strings.bin` as published: entries in the format's
//! length-prefixed encoding (`format::read_pool_string`), so every value,
//! one holding a NUL included, reads back whole from its offset.

use std::hash::{Hash, Hasher};

use rustc_hash::{FxHashMap, FxHasher};

use super::super::format;

/// The hash the pool keys `s` by.
pub(super) fn hash_str(s: &str) -> u64 {
    let mut hasher = FxHasher::default();
    s.hash(&mut hasher);
    hasher.finish()
}

pub(super) struct StringPool {
    overflowed: bool,
    pub(super) data: Vec<u8>,
    // Index string hashes to offsets into `data`; keep bytes in one place
    // instead of owning a second String for every unique value.
    index: FxHashMap<u64, u32>,
    // Hash collisions are exceptional. Store extra offsets only for those
    // hashes and compare the bytes before reusing an offset.
    collisions: FxHashMap<u32, Vec<u32>>,
}

impl StringPool {
    pub(super) fn new() -> Self {
        let mut pool = Self {
            overflowed: false,
            data: Vec::new(),
            index: FxHashMap::default(),
            collisions: FxHashMap::default(),
        };
        // Offset 0 = empty string: the length-zero entry.
        format::push_pool_string(&mut pool.data, "", 0);
        pool
    }

    pub(super) fn intern(&mut self, s: &str) -> u32 {
        self.intern_hashed(s, hash_str(s))
    }

    /// [`Self::intern`] with `hash` = [`hash_str`]`(s)`, computed by the
    /// caller, so a parallel worker can hash and leave the serial merge only
    /// the lookup.
    pub(super) fn intern_hashed(&mut self, s: &str, hash: u64) -> u32 {
        if s.is_empty() {
            return 0;
        }
        self.intern_with_hash(s, hash)
    }

    fn intern_with_hash(&mut self, s: &str, hash: u64) -> u32 {
        if let Some(offset) = self.find_with_hash(s, hash) {
            return offset;
        }
        // Latch failure so merge callbacks can drain their bounded channels
        // without publishing a wrapped offset or allocating further strings.
        let Some((offset, len)) = pool_offset(self.data.len(), s.len()) else {
            self.overflowed = true;
            return 0;
        };
        if self.overflowed {
            return 0;
        }
        if let Some(&first_offset) = self.index.get(&hash) {
            self.collisions
                .entry(first_offset)
                .or_default()
                .push(offset);
        } else {
            self.index.insert(hash, offset);
        }
        format::push_pool_string(&mut self.data, s, len);
        offset
    }

    fn find_with_hash(&self, s: &str, hash: u64) -> Option<u32> {
        let first_offset = *self.index.get(&hash)?;
        if self.matches_at(first_offset, s) {
            return Some(first_offset);
        }
        self.collisions
            .get(&first_offset)?
            .iter()
            .copied()
            .find(|&offset| self.matches_at(offset, s))
    }

    // The entry carries its own length, so equality is of whole values:
    // interned offsets stay equal exactly when the strings are, which the
    // interpolation resolver relies on. Raw bytes are compared; every entry
    // was written from a `&str`, so no UTF-8 decode is needed per probe.
    fn matches_at(&self, offset: u32, s: &str) -> bool {
        format::pool_entry_bytes(&self.data, offset) == Some(s.as_bytes())
    }

    #[cfg(feature = "hotpath")]
    pub(super) fn unique_count(&self) -> usize {
        self.index.len() + self.collisions.values().map(Vec::len).sum::<usize>()
    }

    pub(super) fn validate(&self) -> super::Result<()> {
        if self.overflowed {
            return Err(crate::error::Error::invalid_data(
                "string pool exceeds u32 offsets; widen offsets and bump FORMAT_VERSION",
            ));
        }
        Ok(())
    }
}

/// The offset and `u32` length of a `string_len`-byte entry appended at
/// `current`, or None when the entry's offset or its last byte would not be
/// addressable by a `u32`.
fn pool_offset(current: usize, string_len: usize) -> Option<(u32, u32)> {
    let len = u32::try_from(string_len).ok()?;
    let end = current
        .checked_add(format::pool_prefix_len(len))?
        .checked_add(string_len)?;
    u32::try_from(end).ok()?;
    Some((u32::try_from(current).ok()?, len))
}

/// Read a string from the pool by offset.
#[expect(
    clippy::expect_used,
    reason = "offsets come from this pool's own inserts of &str values, each a whole length-prefixed UTF-8 entry"
)]
pub(super) fn read_string_from_pool(pool: &StringPool, offset: u32) -> &str {
    format::read_pool_string(&pool.data, offset)
        .expect("StringPool only contains length-prefixed UTF-8 strings")
}

#[cfg(test)]
mod tests {
    use super::{StringPool, pool_offset, read_string_from_pool};

    #[test]
    fn pool_limit_includes_the_length_prefix() {
        let limit = u32::MAX as usize;
        // A one-byte string takes a one-byte prefix: two bytes in all.
        assert_eq!(pool_offset(limit - 2, 1), Some((u32::MAX - 2, 1)));
        assert_eq!(pool_offset(limit - 1, 1), None);
        // A 128-byte string takes a two-byte prefix.
        assert_eq!(pool_offset(limit - 130, 128), Some((u32::MAX - 130, 128)));
        assert_eq!(pool_offset(limit - 129, 128), None);
        assert_eq!(pool_offset(usize::MAX, 1), None);
    }

    #[test]
    fn a_value_holding_a_nul_interns_and_reads_back_whole() {
        let mut pool = StringPool::new();
        let whole = pool.intern("10\0a");
        let prefix = pool.intern("10");
        assert_ne!(whole, prefix, "distinct values get distinct offsets");
        assert_eq!(pool.intern("10\0a"), whole, "a repeat deduplicates");
        assert_eq!(read_string_from_pool(&pool, whole), "10\0a");
        assert_eq!(read_string_from_pool(&pool, prefix), "10");
        assert_eq!(read_string_from_pool(&pool, 0), "");
    }
}
