//! Deduplicating string interner used by the OSC overlay for tag keys,
//! relation member roles and user names - strings that repeat heavily across
//! a diff.

use rustc_hash::FxHashMap;

/// The interned strings themselves: every string once, and the id to
/// `(offset, len)` table the arena records' ids resolve through. Read-only;
/// [`StringInterner`] owns one while parsing, and a parse that is finished
/// with interning keeps only this (`StringInterner::into_strings`).
///
/// Intern ID 0 is the empty string.
pub(super) struct StringTable {
    /// Flat buffer holding all interned strings, concatenated. A `String`
    /// rather than bytes: every `(offset, len)` in `table` spans one whole
    /// appended `&str`, so [`Self::resolve`] slices it with `str::get` (a
    /// bounds and char-boundary check) instead of re-validating UTF-8 on
    /// every tag key, member role and user name it hands out.
    data: String,
    /// Maps intern_id -> (offset, len) into `data`.
    table: Vec<(u32, u32)>,
}

/// Deduplicating string interner that maps strings to compact u32 IDs.
///
/// Tag keys and relation member roles repeat heavily across OSC diffs (e.g.
/// "name", "highway", "building" appear thousands of times). Instead of storing
/// N copies of each string, we store one copy in a flat buffer and hand out
/// u32 intern IDs. This saves both memory and allocation overhead.
///
/// The lookup map keys each string a second time; it exists only to
/// deduplicate while interning, so a finished parse drops it
/// ([`Self::into_strings`]).
pub(super) struct StringInterner {
    strings: StringTable,
    /// Maps string content -> intern_id for dedup lookup.
    lookup: FxHashMap<String, u32>,
}

/// The OSC overlay's arena and interner are written only by the overlay
/// itself, so a read that finds them inconsistent is a pbfhogg bug. Cold
/// and never inlined, so the read accessors pay one branch for the check.
#[cold]
#[inline(never)]
#[track_caller]
pub(super) fn arena_invariant_broken(what: &'static str) -> ! {
    panic!("OSC overlay arena invariant broken: {what}")
}

fn interner_overflow(what: &str) -> crate::Error {
    crate::Error::invalid_data(format!(
        "OSC string interner overflowed: more than {} {what}. The overlay cannot address \
         them without silently aliasing one string onto another",
        u32::MAX
    ))
}

impl StringInterner {
    pub(super) fn new() -> Self {
        let mut interner = Self {
            strings: StringTable {
                data: String::new(),
                table: Vec::new(),
            },
            lookup: FxHashMap::default(),
        };
        // Reserve intern_id 0 for the empty string.
        interner.strings.table.push((0, 0));
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
        let strings = &mut self.strings;
        let offset =
            u32::try_from(strings.data.len()).map_err(|_| interner_overflow("string bytes"))?;
        let len = u32::try_from(s.len()).map_err(|_| interner_overflow("a single string"))?;
        // Guard the read-side `offset + len` too: both fit u32 individually
        // yet their sum need not, and `resolve` slices with that sum.
        offset
            .checked_add(len)
            .ok_or_else(|| interner_overflow("string bytes"))?;
        let id = u32::try_from(strings.table.len())
            .map_err(|_| interner_overflow("distinct strings"))?;
        strings.data.push_str(s);
        strings.table.push((offset, len));
        self.lookup.insert(s.to_string(), id);
        Ok(id)
    }

    /// The strings interned so far, for resolving ids.
    #[inline]
    pub(super) fn strings(&self) -> &StringTable {
        &self.strings
    }

    /// The strings, without the lookup map that only interning needs.
    pub(super) fn into_strings(self) -> StringTable {
        self.strings
    }

    /// Estimate the heap memory used by this interner in bytes.
    pub(super) fn heap_size_estimate(&self) -> usize {
        let mut total = self.strings.heap_bytes();
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

impl StringTable {
    /// Resolve an intern ID back to the original string.
    ///
    /// Panics on an id this interner never handed out, or a table entry
    /// that does not span whole strings of `data`. Every id reaching here
    /// was read from an arena record that the overlay wrote with an id
    /// [`StringInterner::intern`] returned on this same interner, and the
    /// arena views can only be built by the overlay, so either failure is a
    /// pbfhogg bug (arena bytes misread, or ids crossed between interners),
    /// not a property of the input. Any fallback string would be the silent
    /// wrong answer: an empty tag key, member role or user name written to
    /// the output as if the diff had said so.
    #[inline]
    pub(super) fn resolve(&self, id: u32) -> &str {
        let Some(&(offset, len)) = self.table.get(id as usize) else {
            arena_invariant_broken("intern id was never handed out by this interner")
        };
        let start = offset as usize;
        match self.data.get(start..start + len as usize) {
            Some(s) => s,
            None => arena_invariant_broken("interner table entry does not span a whole string"),
        }
    }

    /// Bytes of backing storage held: the string buffer and the table, by
    /// capacity.
    pub(super) fn heap_bytes(&self) -> usize {
        self.data.capacity() + self.table.capacity() * std::mem::size_of::<(u32, u32)>()
    }
}
