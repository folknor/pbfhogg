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

fn interner_overflow(what: &str) -> Box<dyn std::error::Error> {
    format!(
        "OSC string interner overflowed: more than {} {what}. The overlay cannot address \
         them without silently aliasing one string onto another",
        u32::MAX
    )
    .into()
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
    ///
    /// Errors rather than truncating when the flat buffer, the string, or
    /// the id space would overflow `u32`. All three are `u32` in the table
    /// layout; an `as u32` on any of them hands back an id whose `(offset,
    /// len)` names some earlier string's bytes, which then silently
    /// substitutes one tag key, member role or user name for another.
    /// Widening the table instead would double its memory for a limit that
    /// only distinct strings can reach, so the cliff is refused, not moved.
    pub(super) fn intern(&mut self, s: &str) -> super::ParseResult<u32> {
        if let Some(&id) = self.lookup.get(s) {
            return Ok(id);
        }
        let offset =
            u32::try_from(self.data.len()).map_err(|_| interner_overflow("string bytes"))?;
        let len = u32::try_from(s.len()).map_err(|_| interner_overflow("a single string"))?;
        // Guard the read-side `offset + len` too: both fit u32 individually
        // yet their sum need not, and `resolve` slices with that sum.
        offset
            .checked_add(len)
            .ok_or_else(|| interner_overflow("string bytes"))?;
        let id =
            u32::try_from(self.table.len()).map_err(|_| interner_overflow("distinct strings"))?;
        self.data.extend_from_slice(s.as_bytes());
        self.table.push((offset, len));
        self.lookup.insert(s.to_string(), id);
        Ok(id)
    }

    /// Resolve an intern ID back to the original string.
    ///
    /// Returns `""` for an id this interner never handed out. Every id
    /// reaching here was produced by [`Self::intern`] on this same
    /// interner, so an out-of-range id means the arena bytes were
    /// misinterpreted; answering with the empty string keeps a read-side
    /// accessor from panicking mid-iteration.
    pub(super) fn resolve(&self, id: u32) -> &str {
        let Some(&(offset, len)) = self.table.get(id as usize) else {
            return "";
        };
        let start = offset as usize;
        let end = start + len as usize;
        match self.data.get(start..end) {
            Some(bytes) => std::str::from_utf8(bytes).unwrap_or(""),
            None => "",
        }
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
