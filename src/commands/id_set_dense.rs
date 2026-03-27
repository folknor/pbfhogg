//! Chunked sparse bitset for O(1) element ID membership testing.
//!
//! Shared across commands that need planet-scale ID sets (extract, tags_filter).

/// Chunked sparse bitset for O(1) element ID membership testing.
///
/// Mirrors osmium's `IdSetDense`: a vector of on-demand 4MB byte-array chunks,
/// each covering 33M IDs via bit-level addressing. Lookup and insertion are
/// 3 instructions (chunk index + byte offset + bitmask), with no hashing or
/// sorting overhead.
///
/// Memory: 1 bit per ID present in each allocated chunk, 4MB per chunk, zero
/// for empty ranges. For Denmark's 52M nodes: 2 chunks = 8MB. For planet
/// (12B node IDs): ~364 chunks = 1.5GB.
pub(crate) struct IdSetDense {
    chunks: Vec<Option<Box<[u8; CHUNK_SIZE]>>>,
}

const CHUNK_BITS: usize = 22;
const CHUNK_SIZE: usize = 1 << CHUNK_BITS;

impl IdSetDense {
    pub fn new() -> Self {
        Self { chunks: Vec::new() }
    }

    #[allow(clippy::cast_sign_loss)]
    pub fn set(&mut self, id: i64) {
        let id = id as u64;
        let cid = (id >> (CHUNK_BITS + 3)) as usize;
        if cid >= self.chunks.len() {
            self.chunks.resize_with(cid + 1, || None);
        }
        let chunk = self.chunks[cid].get_or_insert_with(|| Box::new([0u8; CHUNK_SIZE]));
        let offset = ((id >> 3) & ((1u64 << CHUNK_BITS) - 1)) as usize;
        chunk[offset] |= 1u8 << (id & 7);
    }

    #[allow(clippy::cast_sign_loss)]
    pub fn get(&self, id: i64) -> bool {
        let id = id as u64;
        let cid = (id >> (CHUNK_BITS + 3)) as usize;
        if cid >= self.chunks.len() {
            return false;
        }
        match &self.chunks[cid] {
            None => false,
            Some(chunk) => {
                let offset = ((id >> 3) & ((1u64 << CHUNK_BITS) - 1)) as usize;
                (chunk[offset] & (1u8 << (id & 7))) != 0
            }
        }
    }

    /// Check if any ID in the range [min_id, max_id] is set.
    /// Uses chunk-level granularity for IDs outside a single chunk boundary,
    /// and bit-level for IDs within a chunk. Fast for the common case where
    /// the range spans 1-2 chunks.
    #[allow(clippy::cast_sign_loss)]
    pub fn any_in_range(&self, min_id: i64, max_id: i64) -> bool {
        if min_id > max_id {
            return false;
        }
        let min_id = min_id as u64;
        let max_id = max_id as u64;
        let min_chunk = (min_id >> (CHUNK_BITS + 3)) as usize;
        let max_chunk = (max_id >> (CHUNK_BITS + 3)) as usize;

        for cid in min_chunk..=max_chunk {
            if cid >= self.chunks.len() {
                return false;
            }
            if let Some(chunk) = &self.chunks[cid] {
                // Determine the bit range within this chunk.
                let chunk_base = (cid as u64) << (CHUNK_BITS + 3);
                let range_start = min_id.saturating_sub(chunk_base);
                let chunk_end = chunk_base + ((CHUNK_SIZE as u64) << 3);
                let range_end = if max_id < chunk_end { max_id - chunk_base } else { ((CHUNK_SIZE as u64) << 3) - 1 };

                // Check byte range for any set bits.
                let start_byte = (range_start >> 3) as usize;
                let end_byte = ((range_end >> 3) as usize).min(CHUNK_SIZE - 1);
                for byte in &chunk[start_byte..=end_byte] {
                    if *byte != 0 {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Merge another IdSetDense into this one via bitwise OR.
    ///
    /// For non-overlapping chunks (common in sorted PBFs where each rayon thread
    /// processes a contiguous ID range), chunks are moved with zero copying.
    /// For overlapping chunks, byte-level OR is applied.
    #[allow(dead_code)]
    pub fn merge(&mut self, other: Self) {
        if other.chunks.len() > self.chunks.len() {
            self.chunks.resize_with(other.chunks.len(), || None);
        }
        for (i, other_chunk) in other.chunks.into_iter().enumerate() {
            if let Some(oc) = other_chunk {
                match &mut self.chunks[i] {
                    Some(sc) => {
                        for (a, b) in sc.iter_mut().zip(oc.iter()) {
                            *a |= *b;
                        }
                    }
                    slot @ None => {
                        *slot = Some(oc);
                    }
                }
            }
        }
    }
}
