//! Deduplicating string interner used by the OSC overlay for tag keys and
//! relation member roles - strings that repeat heavily across a diff.

use rustc_hash::FxHashMap;

/// Deduplicating string interner that maps strings to compact u32 IDs.
///
/// Tag keys and relation member roles repeat heavily across OSC diffs (e.g.
/// "name", "highway", "building" appear thousands of times). Instead of storing
/// N copies of each string, we store one copy in a flat `data` buffer and hand
/// out u32 intern IDs. This saves both memory and allocation overhead.
///
/// Intern ID 0 is reserved for the empty string.
pub(super) struct StringInterner {
    /// Flat buffer holding all interned string bytes, concatenated.
    data: Vec<u8>,
    /// Maps intern_id -> (offset, len) into `data`.
    table: Vec<(u32, u32)>,
    /// Maps string content -> intern_id for dedup lookup.
    lookup: FxHashMap<String, u32>,
}

impl StringInterner {
    pub(super) fn new() -> Self {
        let mut interner = Self {
            data: Vec::new(),
            table: Vec::new(),
            lookup: FxHashMap::default(),
        };
        // Reserve intern_id 0 for the empty string.
        interner.table.push((0, 0));
        interner.lookup.insert(String::new(), 0);
        interner
    }

    /// Intern a string, returning its unique ID. Deduplicates: if the string
    /// was already interned, returns the existing ID without allocating.
    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.lookup.get(s) {
            return id;
        }
        let offset = self.data.len() as u32;
        let len = s.len() as u32;
        self.data.extend_from_slice(s.as_bytes());
        let id = self.table.len() as u32;
        self.table.push((offset, len));
        self.lookup.insert(s.to_string(), id);
        id
    }

    /// Resolve an intern ID back to the original string.
    pub(super) fn resolve(&self, id: u32) -> &str {
        let (offset, len) = self.table[id as usize];
        let bytes = &self.data[offset as usize..(offset + len) as usize];
        std::str::from_utf8(bytes).unwrap_or("")
    }

    /// Estimate the heap memory used by this interner in bytes.
    pub(super) fn heap_size_estimate(&self) -> usize {
        let mut total = self.data.capacity();
        total += self.table.capacity() * std::mem::size_of::<(u32, u32)>();
        // FxHashMap overhead: each bucket is (String, u32) + 1 control byte.
        total += self.lookup.capacity()
            * (std::mem::size_of::<String>() + std::mem::size_of::<u32>() + 1);
        // Add heap capacity of each String key in the lookup map.
        for key in self.lookup.keys() {
            total += key.capacity();
        }
        total
    }
}
