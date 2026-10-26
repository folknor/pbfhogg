//! Interned string pool used by the builder.

use std::hash::{Hash, Hasher};

use rustc_hash::{FxHashMap, FxHasher};

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
        // Offset 0 = empty string
        pool.data.push(0);
        pool
    }

    pub(super) fn intern(&mut self, s: &str) -> u32 {
        if s.is_empty() {
            return 0;
        }
        let mut hasher = FxHasher::default();
        s.hash(&mut hasher);
        self.intern_with_hash(s, hasher.finish())
    }

    fn intern_with_hash(&mut self, s: &str, hash: u64) -> u32 {
        if let Some(offset) = self.find_with_hash(s, hash) {
            return offset;
        }
        // Latch failure so merge callbacks can drain their bounded channels
        // without publishing a wrapped offset or allocating further strings.
        let Some(offset) = pool_offset(self.data.len(), s.len()) else {
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
        self.data.extend_from_slice(s.as_bytes());
        self.data.push(0);
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

    // Compare exactly `s.len()` bytes plus the terminator rather than scanning
    // for the first NUL: no per-candidate scan, and a string with an interior
    // NUL still deduplicates against its own earlier copy, as the previous
    // owned-String keys did.
    fn matches_at(&self, offset: u32, s: &str) -> bool {
        let Ok(start) = usize::try_from(offset) else {
            return false;
        };
        let Some(end) = start.checked_add(s.len()) else {
            return false;
        };
        self.data.get(start..end) == Some(s.as_bytes()) && self.data.get(end) == Some(&0)
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

fn pool_offset(current: usize, string_len: usize) -> Option<u32> {
    let end = current.checked_add(string_len)?.checked_add(1)?;
    u32::try_from(end).ok()?;
    u32::try_from(current).ok()
}

/// Read a null-terminated string from the pool by offset.
#[expect(
    clippy::expect_used,
    reason = "offsets come from this pool's own inserts of &str values, each NUL-terminated, so the read is valid UTF-8 up to a NUL"
)]
pub(super) fn read_string_from_pool(pool: &StringPool, offset: u32) -> &str {
    super::super::format::read_nul_string(&pool.data, offset)
        .expect("StringPool only contains NUL-terminated UTF-8 strings")
}

#[cfg(test)]
mod tests {
    use super::pool_offset;

    #[test]
    fn pool_limit_includes_terminator() {
        let limit = u32::MAX as usize;
        assert_eq!(pool_offset(limit - 2, 1), Some(u32::MAX - 2));
        assert_eq!(pool_offset(limit - 1, 1), None);
        assert_eq!(pool_offset(usize::MAX, 1), None);
    }
}
