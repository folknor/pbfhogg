//! Interned string pool used by the builder.

use rustc_hash::FxHashMap;

pub(super) struct StringPool {
    pub(super) data: Vec<u8>,
    pub(super) index: FxHashMap<String, u32>,
}

impl StringPool {
    pub(super) fn new() -> Self {
        let mut pool = Self {
            data: Vec::new(),
            index: FxHashMap::default(),
        };
        // Offset 0 = empty string
        pool.data.push(0);
        pool
    }

    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn intern(&mut self, s: &str) -> u32 {
        if s.is_empty() {
            return 0;
        }
        if let Some(&offset) = self.index.get(s) {
            return offset;
        }
        let offset = self.data.len() as u32;
        self.index.insert(s.to_owned(), offset);
        self.data.extend_from_slice(s.as_bytes());
        self.data.push(0);
        offset
    }
}

/// Read a null-terminated string from the pool by offset.
pub(super) fn read_string_from_pool(pool: &StringPool, offset: u32) -> &str {
    super::super::format::read_nul_string(&pool.data, offset)
}
