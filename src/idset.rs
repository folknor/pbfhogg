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
pub struct IdSet {
    chunks: Vec<Option<Box<[u8; CHUNK_SIZE]>>>,
    /// Rank index for O(1) rank queries. Built on demand via `build_rank_index()`.
    /// `chunk_prefix[cid]` = total set bits in chunks 0..cid.
    /// `block_prefix[cid][block]` = set bits in chunk `cid` before block `block`.
    /// Block size = 64 bytes (512 bits, 8 u64 words). Max 7 words scanned per rank().
    rank_chunk_prefix: Option<Vec<u64>>,
    rank_block_prefix: Option<Vec<Option<Vec<u32>>>>,
}

const CHUNK_BITS: usize = 22;
const CHUNK_SIZE: usize = 1 << CHUNK_BITS;

impl IdSet {
    pub fn new() -> Self {
        Self {
            chunks: Vec::new(),
            rank_chunk_prefix: None,
            rank_block_prefix: None,
        }
    }

    /// Returns `true` if any chunk has been allocated (at least one `set` call).
    pub fn has_any(&self) -> bool {
        self.chunks.iter().any(std::option::Option::is_some)
    }

    /// Returns the number of allocated 4 MB chunks backing this set.
    pub fn allocated_chunk_count(&self) -> usize {
        self.chunks.iter().filter(|chunk| chunk.is_some()).count()
    }

    #[allow(clippy::cast_sign_loss)]
    pub fn set(&mut self, id: i64) {
        if id < 0 {
            return;
        } // Negative IDs are not valid OSM element IDs
        let id = id as u64;
        let cid = (id >> (CHUNK_BITS + 3)) as usize;
        if cid >= self.chunks.len() {
            self.chunks.resize_with(cid + 1, || None);
        }
        let chunk = self.chunks[cid].get_or_insert_with(|| Box::new([0u8; CHUNK_SIZE]));
        let offset = ((id >> 3) & ((1u64 << CHUNK_BITS) - 1)) as usize;
        chunk[offset] |= 1u8 << (id & 7);
    }

    /// Set a bit and report whether it was previously unset. Returns `true`
    /// the first time an ID is seen, `false` on every subsequent call with
    /// the same ID. Negative IDs are silently rejected (and return `false`).
    ///
    /// Matches `RoaringTreemap::insert`'s semantics for duplicate-detection
    /// callers. `verify_ids --full` uses this; general monotonic-population
    /// callers should prefer the void-return [`set`].
    #[allow(clippy::cast_sign_loss)]
    pub fn set_if_new(&mut self, id: i64) -> bool {
        if id < 0 {
            return false;
        }
        let id = id as u64;
        let cid = (id >> (CHUNK_BITS + 3)) as usize;
        if cid >= self.chunks.len() {
            self.chunks.resize_with(cid + 1, || None);
        }
        let chunk = self.chunks[cid].get_or_insert_with(|| Box::new([0u8; CHUNK_SIZE]));
        let offset = ((id >> 3) & ((1u64 << CHUNK_BITS) - 1)) as usize;
        let bit = 1u8 << (id & 7);
        let was_set = (chunk[offset] & bit) != 0;
        chunk[offset] |= bit;
        !was_set
    }

    /// Pre-allocate all chunks needed to hold IDs up to `max_id`.
    /// Call before `set_atomic` to avoid dynamic resizing during
    /// concurrent access.
    ///
    /// # Invariant for callers
    ///
    /// `max_id` is the contract between the caller and every thread that
    /// later calls `set_atomic` / `set_atomic_if_new` through `&self`. Any
    /// `id > max_id` will panic (no dynamic chunk growth is possible under
    /// shared borrow). The panic message in `set_atomic` points back at this.
    ///
    /// Common sources of overshoot seen in practice:
    ///
    /// - Corrupted or mismatched indexdata: a blob header reports a smaller
    ///   `max_id` than the element IDs that actually appear once the blob is
    ///   decoded. `renumber_external` builds its schedule from indexdata and
    ///   uses `pass1_schedule.last().max_id`.
    /// - Future planet growth past a hard-coded cap (see `verify_ids`,
    ///   `check_refs` which cap at 14e9 nodes, 1.5e9 ways, 25e6 relations).
    ///
    /// The defensive option is to verify `max_id` during the schedule scan
    /// against a sanity bound, or to widen the hard-coded caps.
    #[allow(clippy::cast_sign_loss)]
    pub fn pre_allocate(&mut self, max_id: i64) {
        if max_id < 0 {
            return;
        }
        let max_cid = (max_id as u64 >> (CHUNK_BITS + 3)) as usize;
        if max_cid >= self.chunks.len() {
            self.chunks.resize_with(max_cid + 1, || None);
        }
        for slot in &mut self.chunks {
            if slot.is_none() {
                *slot = Some(Box::new([0u8; CHUNK_SIZE]));
            }
        }
    }

    /// Atomically set a bit. Requires `pre_allocate()` to have been called
    /// with a `max_id` >= `id`. Safe for concurrent use from multiple threads
    /// via `&self` (no `&mut` needed). Uses `Relaxed` ordering - callers must
    /// synchronize (e.g. thread join) before reading via `get()` or `rank()`.
    ///
    /// # Panics
    ///
    /// Panics if `id` falls outside the pre-allocated range. This usually
    /// means the caller's `pre_allocate(max_id)` argument underestimated the
    /// highest ID that would be seen - most commonly because the value was
    /// derived from indexdata that understated the real maximum (see the
    /// `pre_allocate` doc for the full list of failure modes). The panic
    /// message includes the offending ID and the pre-allocated upper bound
    /// so it's obvious whether this is an indexdata bug, a hard-coded cap
    /// overshoot, or a missing `pre_allocate` call.
    ///
    /// Do NOT "fix" this panic by replacing it with a silent no-op or by
    /// resizing `self.chunks` here - both would be wrong. A silent no-op
    /// loses data; a resize is unsound because `&self` methods cannot
    /// safely grow the backing `Vec` while other threads hold pointers
    /// into it.
    #[allow(clippy::cast_sign_loss)]
    pub fn set_atomic(&self, id: i64) {
        let id = id as u64;
        let cid = (id >> (CHUNK_BITS + 3)) as usize;
        let chunk = self.chunk_for_atomic(cid, id, "set_atomic");
        let offset = ((id >> 3) & ((1u64 << CHUNK_BITS) - 1)) as usize;
        let bit = 1u8 << (id & 7);
        // SAFETY: AtomicU8 and u8 have identical size/alignment. The chunk
        // is pre-allocated and lives for the duration of the parallel phase.
        // Relaxed ordering is sufficient - we only need visibility after
        // the thread::scope join barrier.
        let atomic =
            unsafe { &*(std::ptr::addr_of!(chunk[offset]).cast::<std::sync::atomic::AtomicU8>()) };
        atomic.fetch_or(bit, std::sync::atomic::Ordering::Relaxed);
    }

    /// Atomic set + duplicate detection. Returns `true` the first time an
    /// ID is seen (in the happens-before sense of the atomic op), `false`
    /// if some other thread already set the same bit.
    ///
    /// Requires `pre_allocate()` to have covered `id`. Safe for concurrent
    /// use from multiple threads via `&self`. Relaxed ordering; callers must
    /// synchronize (thread join) before reading via `get()`.
    ///
    /// # Panics
    ///
    /// See [`Self::set_atomic`] - same panic contract and diagnostics.
    #[allow(clippy::cast_sign_loss)]
    pub fn set_atomic_if_new(&self, id: i64) -> bool {
        let id = id as u64;
        let cid = (id >> (CHUNK_BITS + 3)) as usize;
        let chunk = self.chunk_for_atomic(cid, id, "set_atomic_if_new");
        let offset = ((id >> 3) & ((1u64 << CHUNK_BITS) - 1)) as usize;
        let bit = 1u8 << (id & 7);
        // SAFETY: same as set_atomic. fetch_or returns the previous byte,
        // from which we extract the prior state of this specific bit.
        let atomic =
            unsafe { &*(std::ptr::addr_of!(chunk[offset]).cast::<std::sync::atomic::AtomicU8>()) };
        let prev = atomic.fetch_or(bit, std::sync::atomic::Ordering::Relaxed);
        (prev & bit) == 0
    }

    /// Shared chunk-lookup helper for `set_atomic` / `set_atomic_if_new`.
    /// Produces a detailed panic message when the caller's `pre_allocate`
    /// budget was exceeded - callers see exactly which ID fell outside the
    /// pre-allocated range, so indexdata/max-id bugs are diagnosable from
    /// a single panic line rather than an opaque "not pre-allocated".
    #[inline]
    fn chunk_for_atomic(&self, cid: usize, id: u64, fn_name: &'static str) -> &[u8; CHUNK_SIZE] {
        if cid >= self.chunks.len() {
            let max_covered = if self.chunks.is_empty() {
                "<none - pre_allocate was never called>".to_string()
            } else {
                format!(
                    "chunk {} (~id {})",
                    self.chunks.len() - 1,
                    (self.chunks.len() as u64) << (CHUNK_BITS + 3)
                )
            };
            panic!(
                "{fn_name}: id {id} lands in chunk {cid}, but pre_allocate only \
                 covers up to {max_covered}. Most likely cause: the caller's \
                 `pre_allocate(max_id)` argument was derived from indexdata that \
                 understated the real maximum ID, or from a hard-coded cap that \
                 planet growth has exceeded. See `IdSet::pre_allocate` docs."
            );
        }
        match self.chunks[cid].as_ref() {
            Some(c) => c,
            None => panic!(
                "{fn_name}: id {id} lands in chunk {cid} (within Vec length {}), \
                 but that chunk's slot is None. This means `pre_allocate()` was \
                 never called on this IdSet - the `set_atomic*` methods \
                 require explicit pre-allocation because they cannot grow under \
                 a shared `&self` borrow.",
                self.chunks.len(),
            ),
        }
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
    ///
    /// Negative-id handling: `max_id < 0` returns false (no representable
    /// match), and `min_id < 0` is clamped to 0. An `IdSet` only stores
    /// non-negative ids (negative `set` is a silent no-op), so a query
    /// `[neg, max]` is equivalent to `[0, max]` for any positive `max`.
    /// Without the clamp the unsigned cast wraps `min` past `max`, the
    /// iteration range becomes empty, and a blob whose indexdata straddles
    /// zero is silently skipped before the per-element matcher runs -
    /// dropping legitimate positive matches. See the "Negative input IDs
    /// rejected project-wide" entry in `DEVIATIONS.md` for the wider
    /// project stance and the call sites that depend on this behavior
    /// (`getid` blob-prefilter, `add-locations-to-ways --index-type external`
    /// stage 4).
    #[allow(clippy::cast_sign_loss)]
    pub fn any_in_range(&self, min_id: i64, max_id: i64) -> bool {
        if min_id > max_id || max_id < 0 {
            return false;
        }
        let min_id = min_id.max(0) as u64;
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
                let range_end = if max_id < chunk_end {
                    max_id - chunk_base
                } else {
                    ((CHUNK_SIZE as u64) << 3) - 1
                };

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

    /// Iterate over all set IDs in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = i64> + '_ {
        self.chunks.iter().enumerate().flat_map(|(cid, chunk)| {
            let chunk_base = (cid as u64) << (CHUNK_BITS + 3);
            chunk.iter().flat_map(move |data| {
                data.iter().enumerate().flat_map(move |(byte_idx, &byte)| {
                    (0..8u8).filter_map(move |bit| {
                        if byte & (1 << bit) != 0 {
                            #[allow(clippy::cast_possible_wrap)]
                            Some((chunk_base + (byte_idx as u64) * 8 + u64::from(bit)) as i64)
                        } else {
                            None
                        }
                    })
                })
            })
        })
    }

    /// Merge all set bits from `other` into `self` (union).
    pub fn merge_from(&mut self, other: &IdSet) {
        if other.chunks.len() > self.chunks.len() {
            self.chunks.resize_with(other.chunks.len(), || None);
        }
        for (cid, src) in other.chunks.iter().enumerate() {
            if let Some(src_chunk) = src {
                let dst = self.chunks[cid].get_or_insert_with(|| Box::new([0u8; CHUNK_SIZE]));
                for (d, s) in dst.iter_mut().zip(src_chunk.iter()) {
                    *d |= *s;
                }
            }
        }
    }

    /// Build the prefix-sum index for O(1) `rank()` queries.
    /// Must be called after all `set()` calls are complete.
    /// Invalidated by subsequent `set()` or `merge()` calls.
    pub fn build_rank_index(&mut self) {
        const BLOCK_BYTES: usize = 64; // 8 u64 words = 512 bits per block
        const WORDS_PER_BLOCK: usize = BLOCK_BYTES / 8;
        const BLOCKS_PER_CHUNK: usize = CHUNK_SIZE / BLOCK_BYTES;

        let mut chunk_prefix = Vec::with_capacity(self.chunks.len() + 1);
        let mut block_prefix = Vec::with_capacity(self.chunks.len());
        let mut cumulative: u64 = 0;

        for chunk in &self.chunks {
            chunk_prefix.push(cumulative);
            if let Some(data) = chunk {
                let words: &[u64] = unsafe {
                    std::slice::from_raw_parts(data.as_ptr().cast::<u64>(), CHUNK_SIZE / 8)
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
        const BLOCK_BYTES: usize = 64;
        const WORDS_PER_BLOCK: usize = BLOCK_BYTES / 8;

        let chunk_prefix = self
            .rank_chunk_prefix
            .as_ref()
            .expect("rank() called without build_rank_index()");
        let block_prefix = self
            .rank_block_prefix
            .as_ref()
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
            let words: &[u64] =
                unsafe { std::slice::from_raw_parts(chunk.as_ptr().cast::<u64>(), CHUNK_SIZE / 8) };
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

    /// Combined get + rank in a single lookup. Returns `start_id + rank(id)`
    /// if `id` is set, or `id` unchanged if not (orphan passthrough).
    /// Avoids the double chunk lookup + bounds check of separate `get()` + `rank()`.
    /// Requires `build_rank_index()`.
    #[inline]
    #[allow(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    pub fn resolve(&self, id: i64, start_id: i64) -> i64 {
        const BLOCK_BYTES: usize = 64;
        const WORDS_PER_BLOCK: usize = BLOCK_BYTES / 8;

        let uid = id as u64;
        let cid = (uid >> (CHUNK_BITS + 3)) as usize;

        // Fast path: chunk doesn't exist → orphan.
        if cid >= self.chunks.len() {
            return id;
        }
        let chunk = match &self.chunks[cid] {
            Some(c) => c,
            None => return id,
        };

        // Check if the bit is set.
        let bit_offset = (uid & (((CHUNK_SIZE as u64) << 3) - 1)) as usize;
        let target_byte = bit_offset >> 3;
        let target_bit = bit_offset & 7;
        if (chunk[target_byte] & (1u8 << target_bit)) == 0 {
            return id; // not set → orphan
        }

        // Bit is set - compute rank.
        let chunk_prefix = self
            .rank_chunk_prefix
            .as_ref()
            .expect("resolve() called without build_rank_index()");
        let block_prefix = self
            .rank_block_prefix
            .as_ref()
            .expect("resolve() called without build_rank_index()");

        let mut r = chunk_prefix[cid];
        let block_idx = target_byte / BLOCK_BYTES;

        if let Some(bp) = &block_prefix[cid] {
            r += u64::from(bp[block_idx]);
        }

        let words: &[u64] =
            unsafe { std::slice::from_raw_parts(chunk.as_ptr().cast::<u64>(), CHUNK_SIZE / 8) };
        let block_start_word = block_idx * WORDS_PER_BLOCK;
        let target_word = target_byte / 8;
        for &w in &words[block_start_word..target_word] {
            r += u64::from(w.count_ones());
        }

        let word = words[target_word];
        let bit_in_word = ((target_byte & 7) << 3) + target_bit;
        if bit_in_word > 0 {
            let mask = (1u64 << bit_in_word) - 1;
            r += u64::from((word & mask).count_ones());
        }

        start_id + r as i64
    }

    /// Combined get + rank in a single lookup. Returns `Some(rank)` if `id`
    /// is set, `None` if not. Avoids the double chunk/bit lookup of separate
    /// `get()` + `rank()` calls. Requires `build_rank_index()`.
    #[inline]
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub fn rank_if_set(&self, id: i64) -> Option<u64> {
        const BLOCK_BYTES: usize = 64;
        const WORDS_PER_BLOCK: usize = BLOCK_BYTES / 8;

        let uid = id as u64;
        let cid = (uid >> (CHUNK_BITS + 3)) as usize;

        if cid >= self.chunks.len() {
            return None;
        }
        let chunk = self.chunks[cid].as_ref()?;

        let bit_offset = (uid & (((CHUNK_SIZE as u64) << 3) - 1)) as usize;
        let target_byte = bit_offset >> 3;
        let target_bit = bit_offset & 7;
        if (chunk[target_byte] & (1u8 << target_bit)) == 0 {
            return None;
        }

        let chunk_prefix = self.rank_chunk_prefix.as_ref()?;
        let block_prefix = self.rank_block_prefix.as_ref()?;

        let mut r = chunk_prefix[cid];
        let block_idx = target_byte / BLOCK_BYTES;

        if let Some(bp) = &block_prefix[cid] {
            r += u64::from(bp[block_idx]);
        }

        let words: &[u64] =
            unsafe { std::slice::from_raw_parts(chunk.as_ptr().cast::<u64>(), CHUNK_SIZE / 8) };
        let block_start_word = block_idx * WORDS_PER_BLOCK;
        let target_word = target_byte / 8;
        for &w in &words[block_start_word..target_word] {
            r += u64::from(w.count_ones());
        }

        let word = words[target_word];
        let bit_in_word = ((target_byte & 7) << 3) + target_bit;
        if bit_in_word > 0 {
            let mask = (1u64 << bit_in_word) - 1;
            r += u64::from((word & mask).count_ones());
        }

        Some(r)
    }

    /// Count of set IDs strictly less than `id`. Safe for any `i64` -
    /// arguments below zero return 0, arguments past the highest allocated
    /// chunk return `total_count()`. Unlike `rank()`, never panics on
    /// out-of-range inputs.
    ///
    /// Requires `build_rank_index()`.
    #[allow(clippy::cast_sign_loss)]
    pub fn count_below(&self, id: i64) -> u64 {
        if id <= 0 {
            return 0;
        }
        let uid = id as u64;
        let cid = (uid >> (CHUNK_BITS + 3)) as usize;
        if cid >= self.chunks.len() {
            return self.total_count();
        }
        self.rank(id)
    }

    /// Count of set IDs in the inclusive range `[min_id, max_id]`. Returns
    /// 0 for an empty range. Safe for arguments outside the allocated chunk
    /// space (clamps via `count_below`).
    ///
    /// Used by external join's stage 1 to compute per-node-blob referenced
    /// rank ranges from the blob's indexdata `(min_id, max_id)` without
    /// decoding the blob.
    ///
    /// Requires `build_rank_index()`.
    pub fn count_in_range(&self, min_id: i64, max_id: i64) -> u64 {
        if max_id < min_id {
            return 0;
        }
        let after_max = match max_id.checked_add(1) {
            Some(v) => self.count_below(v),
            None => self.total_count(),
        };
        after_max - self.count_below(min_id)
    }

    /// Drop the rank-index prefix arrays built by `build_rank_index()`. After
    /// this call, `rank()`, `rank_if_set()`, `resolve()`, `count_below()`,
    /// `count_in_range()`, and `total_count()` all fail (panic or return
    /// `None`). The bitmap itself (chunk storage) is retained - `get()`,
    /// `set()`, and `has_any()` still work.
    ///
    /// Used by stage 2 of the external ALTW join to free ~100 MB of rank
    /// metadata once all rank consumers in stage 1 are finished.
    pub fn drop_rank_index(&mut self) {
        self.rank_chunk_prefix = None;
        self.rank_block_prefix = None;
    }

    /// Returns the total number of set IDs. Requires `build_rank_index()`.
    pub fn total_count(&self) -> u64 {
        let prefix = self
            .rank_chunk_prefix
            .as_ref()
            .expect("total_count() called without build_rank_index()");
        prefix[self.chunks.len()]
    }

    /// Merge another IdSet into this one via bitwise OR.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_basic_ids() {
        let mut s = IdSet::new();
        s.set(1);
        s.set(100);
        s.set(1_000_000);
        assert!(s.get(1));
        assert!(s.get(100));
        assert!(s.get(1_000_000));
    }

    #[test]
    fn get_returns_false_for_unset_ids() {
        let mut s = IdSet::new();
        s.set(5);
        assert!(!s.get(0));
        assert!(!s.get(4));
        assert!(!s.get(6));
        assert!(!s.get(999_999));
    }

    #[test]
    fn chunk_boundary_ids() {
        let mut s = IdSet::new();
        // Chunk size is 1 << 22 bytes = 4MB = 33_554_432 bits.
        // IDs at and around the boundary cross chunks.
        let boundary: i64 = 33_554_432;
        s.set(0);
        s.set(boundary - 1);
        s.set(boundary);
        s.set(boundary + 1);

        assert!(s.get(0));
        assert!(s.get(boundary - 1));
        assert!(s.get(boundary));
        assert!(s.get(boundary + 1));
        assert!(!s.get(boundary - 2));
        assert!(!s.get(boundary + 2));
    }

    #[test]
    fn any_in_range_hit_and_miss() {
        let mut s = IdSet::new();
        s.set(50);
        s.set(200);

        // Range that contains a set ID
        assert!(s.any_in_range(40, 60));
        assert!(s.any_in_range(50, 50));
        assert!(s.any_in_range(190, 210));

        // Range that does not contain any set ID
        // Note: any_in_range uses byte-level granularity, so we need ranges
        // that don't share a byte with any set ID. ID 50 is in byte 6 (bits 48-55),
        // ID 200 is in byte 25 (bits 200-207).
        assert!(!s.any_in_range(56, 199));
        assert!(!s.any_in_range(0, 47));
        assert!(!s.any_in_range(208, 300));

        // Inverted range returns false
        assert!(!s.any_in_range(100, 10));
    }

    #[test]
    fn any_in_range_negative_min_clamps_to_zero() {
        // Pins the negative-bound contract documented at any_in_range and in
        // DEVIATIONS.md ("Negative input IDs rejected project-wide"). An
        // IdSet only stores non-negative ids, so a straddling query must
        // inspect the [0, max_id] portion rather than silently returning
        // false (the pre-clamp bug, where the unsigned cast wrapped min
        // past max and skipped the whole range).
        let mut s = IdSet::new();
        s.set(50);

        // Straddling range: positive portion [0, 60] contains 50.
        assert!(s.any_in_range(-100, 60));
        // Straddling range with no positive match.
        assert!(!s.any_in_range(-100, 47));
        // Wholly negative range: nothing representable, return false.
        assert!(!s.any_in_range(-200, -50));
        // Both bounds zero: degenerate but valid; bit 0 not set.
        assert!(!s.any_in_range(0, 0));
        s.set(0);
        assert!(s.any_in_range(-5, 0));
    }

    #[test]
    fn merge_two_sets() {
        let mut a = IdSet::new();
        a.set(10);
        a.set(20);

        let mut b = IdSet::new();
        b.set(20);
        b.set(30);

        a.merge(b);

        assert!(a.get(10));
        assert!(a.get(20));
        assert!(a.get(30));
        assert!(!a.get(15));
    }

    #[test]
    fn rank_and_total_count() {
        let mut s = IdSet::new();
        s.set(5);
        s.set(10);
        s.set(15);
        s.set(20);
        s.build_rank_index();

        assert_eq!(s.total_count(), 4);
        // rank(id) = number of set bits before id
        assert_eq!(s.rank(5), 0);
        assert_eq!(s.rank(10), 1);
        assert_eq!(s.rank(15), 2);
        assert_eq!(s.rank(20), 3);
    }

    #[test]
    fn has_any_empty_vs_nonempty() {
        let empty = IdSet::new();
        assert!(!empty.has_any());

        let mut s = IdSet::new();
        s.set(42);
        assert!(s.has_any());
    }

    #[test]
    fn rank_if_set_parity_with_get_rank() {
        let mut s = IdSet::new();
        let ids = [0, 1, 5, 10, 15, 20, 100, 1_000_000, 33_554_432];
        for &id in &ids {
            s.set(id);
        }
        s.build_rank_index();

        // Set IDs: rank_if_set should return Some(rank) matching rank()
        for &id in &ids {
            let expected = s.rank(id);
            assert_eq!(s.rank_if_set(id), Some(expected), "rank_if_set({id})");
        }

        // Unset IDs: rank_if_set should return None
        let unset = [2, 3, 7, 11, 50, 999, 33_554_431];
        for &id in &unset {
            assert!(!s.get(id), "precondition: {id} should not be set");
            assert_eq!(s.rank_if_set(id), None, "rank_if_set({id}) for unset");
        }
    }

    #[test]
    fn count_in_range_basic() {
        let mut s = IdSet::new();
        for id in [5, 10, 15, 20, 100] {
            s.set(id);
        }
        s.build_rank_index();

        // Inclusive range semantics, exact and gap cases.
        assert_eq!(s.count_in_range(5, 20), 4);
        assert_eq!(s.count_in_range(5, 5), 1);
        assert_eq!(s.count_in_range(6, 9), 0);
        assert_eq!(s.count_in_range(0, 100), 5);
        assert_eq!(s.count_in_range(0, 4), 0);
        assert_eq!(s.count_in_range(101, 1_000), 0);

        // Inverted range.
        assert_eq!(s.count_in_range(50, 10), 0);
    }

    #[test]
    fn count_in_range_safe_past_allocated_chunks() {
        // Regression test for the external join build_node_blob_mapping
        // path: a node blob's indexdata may report (min_id, max_id) where
        // max_id sits in a chunk past the highest allocated chunk in
        // IdSet (because no referenced node has an ID that high).
        // rank() panics on chunks[cid] indexing in that case;
        // count_below() / count_in_range() must clamp to total_count.
        let mut s = IdSet::new();
        s.set(100);
        s.set(200);
        s.build_rank_index();

        // chunks.len() == 1 because both IDs fall in chunk 0.
        // Probe IDs many chunks past the end.
        let way_past_end: i64 = 1_000_000_000_000;
        assert_eq!(s.count_below(way_past_end), 2);
        assert_eq!(s.count_in_range(0, way_past_end), 2);
        assert_eq!(s.count_in_range(150, way_past_end), 1);
        assert_eq!(s.count_in_range(way_past_end / 2, way_past_end), 0);

        // i64::MAX is the saturating-add boundary in build_node_blob_mapping;
        // count_in_range must handle max_id = i64::MAX without overflow.
        assert_eq!(s.count_in_range(0, i64::MAX), 2);
    }

    #[test]
    fn count_below_negative_and_zero() {
        let mut s = IdSet::new();
        s.set(5);
        s.build_rank_index();
        assert_eq!(s.count_below(-1), 0);
        assert_eq!(s.count_below(0), 0);
        assert_eq!(s.count_below(5), 0);
        assert_eq!(s.count_below(6), 1);
    }

    #[test]
    fn drop_rank_index_frees_metadata_but_keeps_bitmap() {
        let mut s = IdSet::new();
        s.set(5);
        s.set(10);
        s.build_rank_index();
        assert_eq!(s.rank_if_set(5), Some(0));
        s.drop_rank_index();
        // Bitmap still works.
        assert!(s.get(5));
        assert!(s.get(10));
        assert!(!s.get(7));
        // Rank queries return None.
        assert_eq!(s.rank_if_set(5), None);
    }

    #[test]
    fn rank_if_set_without_rank_index_returns_none() {
        let mut s = IdSet::new();
        s.set(42);
        // No build_rank_index() - rank_if_set returns None because
        // rank prefix arrays are None.
        assert_eq!(s.rank_if_set(42), None);
    }
}
