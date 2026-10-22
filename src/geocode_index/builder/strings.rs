//! Interned string pool used by the builder.

use rustc_hash::FxHashMap;

pub(super) struct StringPool {
    overflowed: bool,
    pub(super) data: Vec<u8>,
    pub(super) index: FxHashMap<String, u32>,
}

impl StringPool {
    pub(super) fn new() -> Self {
        let mut pool = Self {
            overflowed: false,
            data: Vec::new(),
            index: FxHashMap::default(),
        };
        // Offset 0 = empty string
        pool.data.push(0);
        pool
    }

    pub(super) fn intern(&mut self, s: &str) -> u32 {
        if s.is_empty() {
            return 0;
        }
        if let Some(&offset) = self.index.get(s) {
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
        self.index.insert(s.to_owned(), offset);
        self.data.extend_from_slice(s.as_bytes());
        self.data.push(0);
        offset
    }

    pub(super) fn validate(&self) -> super::Result<()> {
        if self.overflowed {
            return Err(
                "string pool exceeds u32 offsets; widen offsets and bump FORMAT_VERSION".into(),
            );
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
