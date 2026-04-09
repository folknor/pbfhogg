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
    /// Rank index for O(1) rank queries. Built on demand via `build_rank_index()`.
    /// `chunk_prefix[cid]` = total set bits in chunks 0..cid.
    /// `block_prefix[cid][block]` = set bits in chunk `cid` before block `block`.
    /// Block size = 256 bytes (2048 bits, 32 u64 words). Max 32 words scanned per rank().
    rank_chunk_prefix: Option<Vec<u64>>,
    rank_block_prefix: Option<Vec<Option<Vec<u32>>>>,
}

const CHUNK_BITS: usize = 22;
const CHUNK_SIZE: usize = 1 << CHUNK_BITS;

impl IdSetDense {
    pub fn new() -> Self {
        Self { chunks: Vec::new(), rank_chunk_prefix: None, rank_block_prefix: None }
    }

    /// Returns `true` if any chunk has been allocated (at least one `set` call).
    pub fn has_any(&self) -> bool {
        self.chunks.iter().any(std::option::Option::is_some)
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

    /// Build the prefix-sum index for O(1) `rank()` queries.
    /// Must be called after all `set()` calls are complete.
    /// Invalidated by subsequent `set()` or `merge()` calls.
    pub fn build_rank_index(&mut self) {
        const BLOCK_BYTES: usize = 256; // 32 u64 words = 2048 bits per block
        const WORDS_PER_BLOCK: usize = BLOCK_BYTES / 8;
        const BLOCKS_PER_CHUNK: usize = CHUNK_SIZE / BLOCK_BYTES;

        let mut chunk_prefix = Vec::with_capacity(self.chunks.len() + 1);
        let mut block_prefix = Vec::with_capacity(self.chunks.len());
        let mut cumulative: u64 = 0;

        for chunk in &self.chunks {
            chunk_prefix.push(cumulative);
            if let Some(data) = chunk {
                let words: &[u64] = unsafe {
                    std::slice::from_raw_parts(
                        data.as_ptr().cast::<u64>(),
                        CHUNK_SIZE / 8,
                    )
                };
                let mut bp = Vec::with_capacity(BLOCKS_PER_CHUNK);
                let mut within_chunk: u32 = 0;
                for block_idx in 0..BLOCKS_PER_CHUNK {
                    bp.push(within_chunk);
                    let start = block_idx * WORDS_PER_BLOCK;
                    for &w in &words[start..start + WORDS_PER_BLOCK] {
                        within_chunk += w.count_ones();
                    }
                }
                cumulative += u64::from(within_chunk);
                block_prefix.push(Some(bp));
            } else {
                block_prefix.push(None);
            }
        }
        chunk_prefix.push(cumulative); // sentinel for total count

        self.rank_chunk_prefix = Some(chunk_prefix);
        self.rank_block_prefix = Some(block_prefix);
    }

    /// Returns the rank (0-based position among all set IDs) of `id`.
    /// Requires `build_rank_index()` to have been called first.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub fn rank(&self, id: i64) -> u64 {
        const BLOCK_BYTES: usize = 256;
        const WORDS_PER_BLOCK: usize = BLOCK_BYTES / 8;

        let chunk_prefix = self.rank_chunk_prefix.as_ref()
            .expect("rank() called without build_rank_index()");
        let block_prefix = self.rank_block_prefix.as_ref()
            .expect("rank() called without build_rank_index()");

        let id = id as u64;
        let cid = (id >> (CHUNK_BITS + 3)) as usize;
        let mut r = chunk_prefix[cid];

        if let Some(chunk) = &self.chunks[cid] {
            let bit_offset = (id & (((CHUNK_SIZE as u64) << 3) - 1)) as usize;
            let target_byte = bit_offset >> 3;
            let target_bit = bit_offset & 7;
            let block_idx = target_byte / BLOCK_BYTES;

            // Add pre-computed block prefix sum.
            if let Some(bp) = &block_prefix[cid] {
                r += u64::from(bp[block_idx]);
            }

            // Scan only the remaining words within the block (max 31 words).
            let words: &[u64] = unsafe {
                std::slice::from_raw_parts(
                    chunk.as_ptr().cast::<u64>(),
                    CHUNK_SIZE / 8,
                )
            };
            let block_start_word = block_idx * WORDS_PER_BLOCK;
            let target_word = target_byte / 8;
            for &w in &words[block_start_word..target_word] {
                r += u64::from(w.count_ones());
            }

            // Count bits in the partial word up to (but not including) target bit.
            let word = words[target_word];
            let bit_in_word = ((target_byte & 7) << 3) + target_bit;
            if bit_in_word > 0 {
                let mask = (1u64 << bit_in_word) - 1;
                r += u64::from((word & mask).count_ones());
            }
        }

        r
    }

    /// Returns the total number of set IDs. Requires `build_rank_index()`.
    pub fn total_count(&self) -> u64 {
        let prefix = self.rank_chunk_prefix.as_ref()
            .expect("total_count() called without build_rank_index()");
        prefix[self.chunks.len()]
    }

    /// Merge another IdSetDense into this one via bitwise OR.
    ///
    /// For non-overlapping chunks (common in sorted PBFs where each rayon thread
    /// processes a contiguous ID range), chunks are moved with zero copying.
    /// For overlapping chunks, byte-level OR is applied.
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
