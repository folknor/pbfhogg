//! Chunked sparse bitset for O(1) element ID membership testing.
//!
//! Shared sparse bitset for command ID membership.

// The whole module is bit arithmetic between `i64` ids, `u64` chunk ids and
// `u32` / `usize` in-chunk offsets. Every narrowing cast is bounded by
// construction (an offset is masked to 25 bits, a direct chunk id is below
// `DIRECT_CHUNKS`, an id is non-negative before it is reinterpreted), so the
// cast lints are allowed module-wide instead of on every function.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Chunked sparse bitset for O(1) element ID membership testing.
///
/// Mirrors osmium's `IdSetDense`: the ID space is cut into chunks of 2^25 IDs,
/// and a populated chunk is normally a 4 MiB bit array. Lookup and insertion
/// on such a chunk are 3 instructions (chunk index + word offset + bitmask),
/// with no hashing or sorting overhead.
///
/// # Representation
///
/// - **Directory.** Chunks below [`DIRECT_CHUNKS`] (IDs below 2^43) live in a
///   directly indexed `Vec`, so real data never pays more than an index for
///   the chunk lookup.
///   Chunks at or above it live in an ordered map. The directory therefore
///   no longer grows with the largest ID ever inserted: one way ref near
///   `i64::MAX` used to resize the `Vec` to ~2^38 entries (about 4 TB) and
///   abort the process.
/// - **Chunks.** A chunk starts as a sorted array of in-chunk offsets and is
///   promoted to the 4 MiB bit array when it would exceed [`SPARSE_MAX`]
///   members. Heavily populated chunks use the bit-array form, while an
///   isolated ID does not cost a whole 4 MiB chunk.
///
/// # Memory
///
/// For a set populated by `set` / `set_if_new` and merges of such sets,
/// memory is bounded by the number of distinct IDs, not by their spread or
/// their maximum. Sparse chunks store offsets and promote to bit arrays after
/// [`SPARSE_MAX`] members; the direct directory is bounded by
/// [`DIRECT_CHUNKS`]. A promotion temporarily holds both representations.
///
/// The exception is [`Self::pre_allocate`], which creates empty bit-array
/// chunks for a whole ID range on request; its argument is the caller's
/// responsibility (see its docs), and merging a pre-allocated set carries those
/// chunks along.
///
/// # Rank queries
///
/// An `IdSet` answers membership only. Rank queries live on
/// [`RankedIdSet`], produced by [`Self::into_ranked`] once population is
/// complete; a rank query on a set that is still being built does not
/// compile.
pub struct IdSet {
    /// Chunks with chunk id below `DIRECT_CHUNKS`, indexed by chunk id.
    chunks: Vec<Option<Chunk>>,
    /// Chunks with chunk id at or above `DIRECT_CHUNKS`, keyed by chunk id.
    /// Every ID here is above every ID in `chunks`.
    overflow: BTreeMap<u64, Chunk>,
}

/// A completed [`IdSet`] plus its rank index, for O(1) (direct range) /
/// O(log n) (overflow range) rank queries.
///
/// Both fields are private and no accessor hands out the inner set, not even
/// as `&IdSet`: the atomic setters write through a shared reference, so any
/// path to `&IdSet` would let members change under a built index and leave
/// its prefixes stale. Membership is forwarded explicitly via [`Self::get`].
pub struct RankedIdSet {
    set: IdSet,
    rank: RankIndex,
}

/// Bytes in one bit-array chunk (4 MiB).
const CHUNK_BYTES: usize = 1 << 22;
const WORDS_PER_CHUNK: usize = CHUNK_BYTES / std::mem::size_of::<u64>();
/// IDs per chunk = bits per bit-array chunk = 2^25.
const ID_SHIFT: u32 = 25;
const OFFSET_MASK: u64 = (1 << ID_SHIFT) - 1;

/// Chunk ids below this live in the directly indexed directory. 2^18 chunks
/// cover IDs below 2^43 (~8.8e12); the directory a single high ID can force
/// is therefore bounded by a few MiB instead of growing with the ID.
const DIRECT_CHUNKS: u64 = 1 << 18;

/// Largest member count of an array chunk. The insertion of member
/// `SPARSE_MAX + 1` promotes the chunk to a bit array. Inserting in random
/// order can move up to roughly `SPARSE_MAX^2` bytes per chunk before
/// promotion, so the threshold trades insertion work against memory.
const SPARSE_MAX: usize = 1024;

/// Rank block: 64 bytes = 8 words = 512 bits. At most 7 words scanned per
/// within-chunk rank.
const WORDS_PER_BLOCK: usize = 8;
const BITS_PER_BLOCK: u32 = 512;
const BLOCKS_PER_CHUNK: usize = WORDS_PER_CHUNK / WORDS_PER_BLOCK;

/// Split a non-negative ID into `(chunk id, in-chunk offset)`. `None` for a
/// negative ID: an `IdSet` stores only non-negative IDs.
#[inline]
fn split(id: i64) -> Option<(u64, u32)> {
    if id < 0 {
        return None;
    }
    let id = id as u64;
    Some((id >> ID_SHIFT, (id & OFFSET_MASK) as u32))
}

fn new_dense() -> Box<[AtomicU64]> {
    std::iter::repeat_with(|| AtomicU64::new(0))
        .take(WORDS_PER_CHUNK)
        .collect()
}

#[inline]
fn dense_insert(words: &mut [AtomicU64], off: u32) -> bool {
    let bit = 1u64 << (off & 63);
    let word = words[(off >> 6) as usize].get_mut();
    let was_set = (*word & bit) != 0;
    *word |= bit;
    !was_set
}

fn dense_from_offsets(offsets: &[u32]) -> Box<[AtomicU64]> {
    let mut words = new_dense();
    for &off in offsets {
        dense_insert(&mut words, off);
    }
    words
}

/// Union of two sorted, deduplicated offset arrays.
fn union_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

/// One 2^25-ID chunk.
enum Chunk {
    /// 4 MiB bit array; the only form the atomic setters can write.
    Dense(Box<[AtomicU64]>),
    /// Sorted, deduplicated in-chunk offsets; at most `SPARSE_MAX` of them.
    Sparse(Vec<u32>),
}

impl Chunk {
    fn new_sparse() -> Self {
        Chunk::Sparse(Vec::new())
    }

    #[inline]
    fn contains(&self, off: u32) -> bool {
        match self {
            Chunk::Dense(words) => {
                (words[(off >> 6) as usize].load(Relaxed) & (1u64 << (off & 63))) != 0
            }
            Chunk::Sparse(offsets) => offsets.binary_search(&off).is_ok(),
        }
    }

    /// Insert `off`; `true` if it was not already a member.
    #[inline]
    fn insert(&mut self, off: u32) -> bool {
        match self {
            Chunk::Dense(words) => dense_insert(words, off),
            Chunk::Sparse(offsets) => {
                // Append fast path: ascending population (sorted input) never
                // searches.
                if offsets.last().is_none_or(|&last| last < off) {
                    if offsets.len() < SPARSE_MAX {
                        offsets.push(off);
                        return true;
                    }
                } else {
                    match offsets.binary_search(&off) {
                        Ok(_) => return false,
                        Err(pos) if offsets.len() < SPARSE_MAX => {
                            offsets.insert(pos, off);
                            return true;
                        }
                        Err(_) => {}
                    }
                }
                // A new member that would exceed SPARSE_MAX: promote. The
                // membership test above ran first, so a duplicate never
                // promotes.
                let mut words = dense_from_offsets(offsets);
                dense_insert(&mut words, off);
                *self = Chunk::Dense(words);
                true
            }
        }
    }

    /// Turn an array chunk into a bit array (no-op on a bit array).
    fn promote(&mut self) {
        if let Chunk::Sparse(offsets) = self {
            let words = dense_from_offsets(offsets);
            *self = Chunk::Dense(words);
        }
    }

    fn clone_chunk(&self) -> Self {
        match self {
            Chunk::Dense(words) => Chunk::Dense(
                words
                    .iter()
                    .map(|w| AtomicU64::new(w.load(Relaxed)))
                    .collect(),
            ),
            Chunk::Sparse(offsets) => Chunk::Sparse(offsets.clone()),
        }
    }

    /// Union `src` into `self`, borrowing `src`.
    fn union_ref(&mut self, src: &Chunk) {
        // An array destination meeting a bit-array source becomes a bit
        // array first; after this, an array destination implies an array
        // source.
        if matches!(src, Chunk::Dense(_)) {
            self.promote();
        }
        match self {
            Chunk::Dense(dst) => match src {
                Chunk::Dense(s) => or_words(dst, s),
                Chunk::Sparse(s) => {
                    for &off in s {
                        dense_insert(dst, off);
                    }
                }
            },
            Chunk::Sparse(dst) => match src {
                Chunk::Sparse(s) => {
                    let merged = union_sorted(dst, s);
                    *self = if merged.len() > SPARSE_MAX {
                        Chunk::Dense(dense_from_offsets(&merged))
                    } else {
                        Chunk::Sparse(merged)
                    };
                }
                // Not reachable: a bit-array source promoted `self` above.
                Chunk::Dense(_) => {}
            },
        }
    }

    /// Union `src` into `self`, consuming `src`. An array destination takes
    /// a bit-array source's allocation whole instead of copying 4 MiB.
    fn union_owned(&mut self, src: Chunk) {
        match src {
            Chunk::Dense(mut s) if matches!(self, Chunk::Sparse(_)) => {
                if let Chunk::Sparse(dst) = self {
                    for &off in dst.iter() {
                        dense_insert(&mut s, off);
                    }
                }
                *self = Chunk::Dense(s);
            }
            src => self.union_ref(&src),
        }
    }

    #[cfg(test)]
    fn count(&self) -> u64 {
        match self {
            Chunk::Dense(words) => words
                .iter()
                .map(|w| u64::from(w.load(Relaxed).count_ones()))
                .sum(),
            Chunk::Sparse(offsets) => offsets.len() as u64,
        }
    }

    /// Per-block prefix counts for a bit array (`None` for an array chunk,
    /// whose rank is a binary search) and the chunk's member count.
    fn rank_blocks(&self) -> (Option<Vec<u32>>, u64) {
        match self {
            Chunk::Dense(words) => {
                let mut prefix = Vec::with_capacity(BLOCKS_PER_CHUNK);
                let mut within: u32 = 0;
                for block in words.as_chunks::<WORDS_PER_BLOCK>().0 {
                    prefix.push(within);
                    for w in block {
                        within += w.load(Relaxed).count_ones();
                    }
                }
                (Some(prefix), u64::from(within))
            }
            Chunk::Sparse(offsets) => (None, offsets.len() as u64),
        }
    }

    /// Members of this chunk strictly below `off`. `blocks` is this chunk's
    /// entry from the rank index.
    #[inline]
    fn rank_within(&self, off: u32, blocks: Option<&[u32]>) -> u64 {
        match self {
            Chunk::Dense(words) => {
                let block_idx = (off / BITS_PER_BLOCK) as usize;
                let mut r = blocks.map_or(0, |bp| u64::from(bp[block_idx]));
                let target_word = (off >> 6) as usize;
                for w in &words[block_idx * WORDS_PER_BLOCK..target_word] {
                    r += u64::from(w.load(Relaxed).count_ones());
                }
                let bit = off & 63;
                if bit > 0 {
                    let mask = (1u64 << bit) - 1;
                    r += u64::from((words[target_word].load(Relaxed) & mask).count_ones());
                }
                r
            }
            Chunk::Sparse(offsets) => offsets.partition_point(|&x| x < off) as u64,
        }
    }

    /// Whether any member lies in the inclusive offset range `[lo, hi]`.
    fn any_in(&self, lo: u32, hi: u32) -> bool {
        match self {
            Chunk::Dense(words) => {
                let start_word = (lo >> 6) as usize;
                let end_word = (hi >> 6) as usize;
                for (i, word) in words[start_word..=end_word].iter().enumerate() {
                    let idx = start_word + i;
                    let mut value = word.load(Relaxed);
                    if idx == start_word {
                        value &= u64::MAX << (lo & 63);
                    }
                    if idx == end_word && (hi & 63) != 63 {
                        value &= (1u64 << ((hi & 63) + 1)) - 1;
                    }
                    if value != 0 {
                        return true;
                    }
                }
                false
            }
            Chunk::Sparse(offsets) => {
                let i = offsets.partition_point(|&x| x < lo);
                i < offsets.len() && offsets[i] <= hi
            }
        }
    }

    fn offsets(&self) -> ChunkOffsets<'_> {
        match self {
            Chunk::Dense(words) => ChunkOffsets::Dense {
                words: &words[..],
                next: 0,
                cur: 0,
                cur_base: 0,
            },
            Chunk::Sparse(offsets) => ChunkOffsets::Sparse(offsets.iter()),
        }
    }
}

/// OR `src`'s words into `dst`.
fn or_words(dst: &mut [AtomicU64], src: &[AtomicU64]) {
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d.get_mut() |= s.load(Relaxed);
    }
}

/// Ascending in-chunk offsets of one chunk's members.
enum ChunkOffsets<'a> {
    Dense {
        words: &'a [AtomicU64],
        /// Index of the next word to load.
        next: usize,
        /// Unvisited set bits of the current word.
        cur: u64,
        /// Bit offset of the current word.
        cur_base: u32,
    },
    Sparse(std::slice::Iter<'a, u32>),
}

impl Iterator for ChunkOffsets<'_> {
    type Item = u32;

    #[inline]
    fn next(&mut self) -> Option<u32> {
        match self {
            ChunkOffsets::Dense {
                words,
                next,
                cur,
                cur_base,
            } => loop {
                if *cur != 0 {
                    let tz = cur.trailing_zeros();
                    *cur &= *cur - 1;
                    return Some(*cur_base + tz);
                }
                if *next >= words.len() {
                    return None;
                }
                *cur = words[*next].load(Relaxed);
                *cur_base = (*next as u32) * 64;
                *next += 1;
            },
            ChunkOffsets::Sparse(it) => it.next().copied(),
        }
    }
}

/// Prefix-sum index behind `rank()` and friends. Built and dropped as one
/// unit, so no component can outlive the chunks it describes.
struct RankIndex {
    /// `chunk_prefix[cid]` = members in direct chunks `0..cid`; the last
    /// entry (index `chunks.len()`) is the direct total.
    chunk_prefix: Vec<u64>,
    /// Per direct chunk: block prefixes for a bit array, `None` otherwise.
    block_prefix: Vec<Option<Vec<u32>>>,
    /// Overflow chunk ids, ascending.
    ov_keys: Vec<u64>,
    /// `ov_prefix[i]` = direct total + members in overflow chunks before
    /// `ov_keys[i]`. One longer than `ov_keys`; the last entry is the grand
    /// total, so an empty set has `[0]`.
    ov_prefix: Vec<u64>,
    /// Per overflow chunk, parallel to `ov_keys`.
    ov_block: Vec<Option<Vec<u32>>>,
}

impl RankIndex {
    fn total(&self) -> u64 {
        self.ov_prefix.last().copied().unwrap_or(0)
    }
}

/// Where a present chunk lives, for rank lookups.
#[derive(Clone, Copy)]
enum Slot {
    Direct(usize),
    Overflow,
}

impl IdSet {
    pub fn new() -> Self {
        Self {
            chunks: Vec::new(),
            overflow: BTreeMap::new(),
        }
    }

    /// Returns `true` if any chunk has been allocated (at least one `set`
    /// call, or a `pre_allocate`).
    pub fn has_any(&self) -> bool {
        self.chunks.iter().any(Option::is_some) || !self.overflow.is_empty()
    }

    /// Returns the number of allocated chunks backing this set, in either
    /// form (sorted array or 4 MiB bit array).
    pub fn allocated_chunk_count(&self) -> usize {
        self.chunks.iter().filter(|chunk| chunk.is_some()).count() + self.overflow.len()
    }

    #[inline]
    fn chunk(&self, cid: u64) -> Option<(Slot, &Chunk)> {
        if cid < DIRECT_CHUNKS {
            let i = cid as usize;
            self.chunks
                .get(i)?
                .as_ref()
                .map(|chunk| (Slot::Direct(i), chunk))
        } else {
            self.overflow.get(&cid).map(|chunk| (Slot::Overflow, chunk))
        }
    }

    #[inline]
    fn chunk_entry(&mut self, cid: u64) -> &mut Chunk {
        if cid < DIRECT_CHUNKS {
            let i = cid as usize;
            if i >= self.chunks.len() {
                self.chunks.resize_with(i + 1, || None);
            }
            self.chunks[i].get_or_insert_with(Chunk::new_sparse)
        } else {
            self.overflow.entry(cid).or_insert_with(Chunk::new_sparse)
        }
    }

    /// Perf note: do not call this per element inside a dense classify
    /// hot loop - the chunk lookup is random access and measured 29x
    /// slower than `Vec::push` there. Accumulate matches in a `Vec<i64>`
    /// and drain into the set afterwards; direct `set()` is fine on
    /// sparse paths. See `reference/performance-history.md`
    /// "IdSetDense accumulation".
    ///
    /// Negative IDs are a silent no-op: they are not valid OSM element IDs
    /// here (see "Negative input IDs rejected project-wide" in
    /// `DEVIATIONS.md`). Any non-negative ID is accepted, up to `i64::MAX`,
    /// without an allocation proportional to the ID.
    #[inline]
    pub fn set(&mut self, id: i64) {
        let Some((cid, off)) = split(id) else {
            return;
        };
        // Fast path: an existing bit-array chunk in the direct directory,
        // the steady state for real OSM data. A plain OR, as the bitmap-only
        // set had; routing every call through `set_if_new`'s chunk-kind
        // match measured ~1.1 ns/ref slower in altw pass 0's serial union.
        // An overflow chunk id is past `chunks.len()`, so `get_mut` misses.
        if let Some(Some(Chunk::Dense(words))) = self.chunks.get_mut(cid as usize) {
            *words[(off >> 6) as usize].get_mut() |= 1u64 << (off & 63);
            return;
        }
        self.chunk_entry(cid).insert(off);
    }

    /// Set a bit and report whether it was previously unset. Returns `true`
    /// the first time an ID is seen, `false` on every subsequent call with
    /// the same ID. Negative IDs are silently rejected (and return `false`).
    ///
    /// Matches `RoaringTreemap::insert`'s semantics for duplicate-detection
    /// callers. `verify_ids --full` uses this; general monotonic-population
    /// callers should prefer the void-return [`set`].
    #[inline]
    pub fn set_if_new(&mut self, id: i64) -> bool {
        let Some((cid, off)) = split(id) else {
            return false;
        };
        self.chunk_entry(cid).insert(off)
    }

    /// Pre-allocate bit-array chunks for every ID in `0..=max_id`.
    /// Call before `set_atomic` to avoid dynamic resizing during
    /// concurrent access. Chunks that already hold array contents from
    /// `set` are promoted in place, keeping their members. Chunks above
    /// `max_id` are left as they are (the atomic setters cannot write an
    /// array chunk there; nothing relies on that).
    ///
    /// This eagerly allocates 4 MiB per 2^25 IDs of range, so `max_id` sizes
    /// the allocation directly: a caller deriving it from untrusted input
    /// (indexdata) must bound it first with [`IdSet::file_bounded_max_id`].
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
    ///   decoded. Callers that size from indexdata must route IDs above
    ///   their bound elsewhere; renumber collects them per worker and
    ///   inserts them with `set` after its parallel pass.
    /// - Future planet growth past a hard-coded cap.
    ///
    /// The defensive option is to verify `max_id` during the schedule scan
    /// against a sanity bound, or to use the `_checked` setters.
    pub fn pre_allocate(&mut self, max_id: i64) {
        let Some((max_cid, _)) = split(max_id) else {
            return;
        };
        let direct_end = max_cid.min(DIRECT_CHUNKS - 1) as usize;
        if direct_end >= self.chunks.len() {
            self.chunks.resize_with(direct_end + 1, || None);
        }
        for slot in &mut self.chunks[..=direct_end] {
            match slot {
                Some(chunk) => chunk.promote(),
                None => *slot = Some(Chunk::Dense(new_dense())),
            }
        }
        for cid in DIRECT_CHUNKS..=max_cid {
            match self.overflow.entry(cid) {
                Entry::Occupied(mut e) => e.get_mut().promote(),
                Entry::Vacant(e) => {
                    e.insert(Chunk::Dense(new_dense()));
                }
            }
        }
    }

    /// The `pre_allocate` argument for a highest ID claimed by unverified
    /// input (indexdata): the smaller of `claimed_max` and
    /// `file_len * ids_per_byte`, saturating at `i64::MAX`.
    ///
    /// A claim of `i64::MAX` from a 4 KB file would otherwise ask
    /// `pre_allocate` for an exabyte before a single blob is read
    /// (reference/indexdata-trust.md, "Allocation from claims"). At one bit
    /// per ID the bitmap stays within `ids_per_byte / 8` times the input's own size,
    /// plus one chunk of rounding.
    ///
    /// `ids_per_byte` is the caller's, because what happens to an ID above
    /// the bound differs: renumber, the geocode builder and `check` insert
    /// observed positive IDs after their parallel pass, so their factor trades
    /// serial overflow work against memory; `check` also adds a per-kind
    /// allocation floor. A negative claim is returned unchanged, and
    /// `pre_allocate` treats it as "nothing to allocate".
    pub(crate) fn file_bounded_max_id(claimed_max: i64, file_len: u64, ids_per_byte: u64) -> i64 {
        let by_size = i64::try_from(file_len.saturating_mul(ids_per_byte)).unwrap_or(i64::MAX);
        claimed_max.min(by_size)
    }

    /// Atomically set a bit. Requires `pre_allocate()` to have been called
    /// with a `max_id` >= `id`. Safe for concurrent use from multiple threads
    /// via `&self` (no `&mut` needed). Uses `Relaxed` ordering - callers must
    /// synchronize (e.g. thread join) before reading via `get()` or ranking
    /// via `into_ranked()`.
    ///
    /// # Panics
    ///
    /// Panics if `id` is negative or falls outside the pre-allocated range.
    /// The out-of-range case usually means the caller's `pre_allocate(max_id)`
    /// argument underestimated the highest ID that would be seen - most
    /// commonly because the value was derived from indexdata that understated
    /// the real maximum (see the `pre_allocate` doc for the full list of
    /// failure modes). The panic message includes the offending ID so it's
    /// obvious whether this is an indexdata bug, a hard-coded cap overshoot,
    /// an unfiltered negative ID, or a missing `pre_allocate` call.
    ///
    /// Do NOT "fix" this panic by replacing it with a silent no-op or by
    /// growing the set here - both would be wrong. A silent no-op loses data;
    /// growing is unsound because `&self` methods cannot safely grow the
    /// backing storage while other threads hold pointers into it.
    pub fn set_atomic(&self, id: i64) {
        let (words, off) = self.chunk_for_atomic(id, "set_atomic");
        words[(off >> 6) as usize].fetch_or(1u64 << (off & 63), Relaxed);
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
    pub fn set_atomic_if_new(&self, id: i64) -> bool {
        let (words, off) = self.chunk_for_atomic(id, "set_atomic_if_new");
        let bit = 1u64 << (off & 63);
        let prev = words[(off >> 6) as usize].fetch_or(bit, Relaxed);
        (prev & bit) == 0
    }

    /// Non-panicking counterpart of [`Self::set_atomic`].
    ///
    /// Returns `false` - having set nothing - when `id` is negative or falls
    /// outside the pre-allocated range, instead of panicking. For callers
    /// processing untrusted input, an out-of-range ID is a property of the
    /// *file*, not a bug in the caller, so it has to be reportable rather than
    /// fatal: a panic on attacker-controlled input is a denial-of-service
    /// surface, and in `check`'s case it aborts the very diagnosis the user
    /// asked for.
    ///
    /// Callers that genuinely control the ID range (their `pre_allocate`
    /// argument provably covers every ID they will pass) should keep using
    /// `set_atomic`, whose panic catches a real caller bug.
    pub fn set_atomic_checked(&self, id: i64) -> bool {
        let Some((words, off)) = self.dense_for_atomic(id) else {
            return false;
        };
        words[(off >> 6) as usize].fetch_or(1u64 << (off & 63), Relaxed);
        true
    }

    /// Non-panicking counterpart of [`Self::set_atomic_if_new`].
    ///
    /// `None` means the ID could not be recorded at all - negative, or outside
    /// the pre-allocated range. `Some(true)` / `Some(false)` carry the usual
    /// first-seen / already-seen answer. See [`Self::set_atomic_checked`] for
    /// why the out-of-range case is a return value here rather than a panic.
    pub fn set_atomic_if_new_checked(&self, id: i64) -> Option<bool> {
        let (words, off) = self.dense_for_atomic(id)?;
        let bit = 1u64 << (off & 63);
        let prev = words[(off >> 6) as usize].fetch_or(bit, Relaxed);
        Some((prev & bit) == 0)
    }

    /// The bit-array chunk an atomic setter may write for `id`, or `None`.
    ///
    /// `None` covers a negative ID, a chunk that does not exist, and a chunk
    /// that only holds array contents from `set` (the atomic setters cannot
    /// grow or convert a chunk under `&self`; `pre_allocate` is what makes a
    /// range writable). Negative IDs are legal in an OSM file (JOSM
    /// editor-local staging IDs), so landing here is an input property, not a
    /// corrupt-state signal - which is precisely why the checked callers get
    /// `None` rather than a panic.
    #[inline]
    fn dense_for_atomic(&self, id: i64) -> Option<(&[AtomicU64], u32)> {
        let (cid, off) = split(id)?;
        match self.chunk(cid)? {
            (_, Chunk::Dense(words)) => Some((&words[..], off)),
            (_, Chunk::Sparse(_)) => None,
        }
    }

    /// Panicking chunk lookup for `set_atomic` / `set_atomic_if_new`, with a
    /// message that says which of the failure modes this is.
    #[inline]
    fn chunk_for_atomic(&self, id: i64, fn_name: &'static str) -> (&[AtomicU64], u32) {
        if let Some(found) = self.dense_for_atomic(id) {
            return found;
        }
        if id < 0 {
            panic!(
                "{fn_name}: negative id {id}. An IdSet stores only non-negative \
                 IDs; the caller must reject or filter negative IDs before an \
                 atomic insert, or use the `_checked` variant."
            );
        }
        let last_cid = self.overflow.keys().next_back().copied().or_else(|| {
            self.chunks
                .iter()
                .rposition(Option::is_some)
                .map(|i| i as u64)
        });
        let max_covered = match last_cid {
            None => "<none - pre_allocate was never called>".to_string(),
            Some(cid) => format!(
                "chunk {cid} (ids below {})",
                (u128::from(cid) + 1) << ID_SHIFT
            ),
        };
        panic!(
            "{fn_name}: id {id} lands in chunk {}, which pre_allocate did not \
             cover (allocated chunks reach {max_covered}). Most likely cause: \
             the caller's `pre_allocate(max_id)` argument was derived from \
             indexdata that understated the real maximum ID, or from a \
             hard-coded cap that planet growth has exceeded. See \
             `IdSet::pre_allocate` docs.",
            (id as u64) >> ID_SHIFT,
        );
    }

    #[inline]
    pub fn get(&self, id: i64) -> bool {
        let Some((cid, off)) = split(id) else {
            return false;
        };
        self.chunk(cid)
            .is_some_and(|(_, chunk)| chunk.contains(off))
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
    pub fn any_in_range(&self, min_id: i64, max_id: i64) -> bool {
        if min_id > max_id || max_id < 0 {
            return false;
        }
        let Some((min_cid, min_off)) = split(min_id.max(0)) else {
            return false;
        };
        let Some((max_cid, max_off)) = split(max_id) else {
            return false;
        };
        // Clip the query to one chunk's offsets. Offsets are within-chunk,
        // so no bound is ever formed as `id + 1`.
        let clip = |cid: u64| -> (u32, u32) {
            let lo = if cid == min_cid { min_off } else { 0 };
            let hi = if cid == max_cid {
                max_off
            } else {
                OFFSET_MASK as u32
            };
            (lo, hi)
        };

        if min_cid < DIRECT_CHUNKS && !self.chunks.is_empty() {
            let last = max_cid.min(self.chunks.len() as u64 - 1);
            for cid in min_cid..=last {
                if let Some(chunk) = &self.chunks[cid as usize] {
                    let (lo, hi) = clip(cid);
                    if chunk.any_in(lo, hi) {
                        return true;
                    }
                }
            }
        }
        if max_cid >= DIRECT_CHUNKS {
            for (&cid, chunk) in self.overflow.range(min_cid.max(DIRECT_CHUNKS)..=max_cid) {
                let (lo, hi) = clip(cid);
                if chunk.any_in(lo, hi) {
                    return true;
                }
            }
        }
        false
    }

    /// Iterate over all set IDs in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = i64> + '_ {
        let direct = self
            .chunks
            .iter()
            .enumerate()
            .filter_map(|(cid, chunk)| chunk.as_ref().map(|chunk| (cid as u64, chunk)));
        let overflow = self.overflow.iter().map(|(&cid, chunk)| (cid, chunk));
        direct.chain(overflow).flat_map(|(cid, chunk)| {
            let base = cid << ID_SHIFT;
            chunk
                .offsets()
                .map(move |off| (base + u64::from(off)) as i64)
        })
    }

    /// Merge all set bits from `other` into `self` (union).
    pub fn merge_from(&mut self, other: &IdSet) {
        if other.chunks.len() > self.chunks.len() {
            self.chunks.resize_with(other.chunks.len(), || None);
        }
        for (slot, src) in self.chunks.iter_mut().zip(other.chunks.iter()) {
            if let Some(src) = src {
                match slot {
                    Some(dst) => dst.union_ref(src),
                    None => *slot = Some(src.clone_chunk()),
                }
            }
        }
        for (&cid, src) in &other.overflow {
            match self.overflow.entry(cid) {
                Entry::Occupied(mut e) => e.get_mut().union_ref(src),
                Entry::Vacant(e) => {
                    e.insert(src.clone_chunk());
                }
            }
        }
    }

    /// Merge another IdSet into this one (union).
    ///
    /// For non-overlapping chunks (common in sorted PBFs where each rayon thread
    /// processes a contiguous ID range), chunks are moved with zero copying.
    /// For overlapping chunks, the members are unioned.
    pub fn merge(&mut self, other: Self) {
        if other.chunks.len() > self.chunks.len() {
            self.chunks.resize_with(other.chunks.len(), || None);
        }
        for (slot, src) in self.chunks.iter_mut().zip(other.chunks) {
            if let Some(src) = src {
                match slot {
                    Some(dst) => dst.union_owned(src),
                    // The reason the owned form exists: an absent
                    // destination chunk takes the source allocation whole,
                    // with no 4 MB copy and no word-by-word OR.
                    None => *slot = Some(src),
                }
            }
        }
        for (cid, src) in other.overflow {
            match self.overflow.entry(cid) {
                Entry::Occupied(mut e) => e.get_mut().union_owned(src),
                Entry::Vacant(e) => {
                    e.insert(src);
                }
            }
        }
    }

    /// Freeze the set and build the prefix-sum index behind its rank queries
    /// (O(1) on the direct range, O(log n) above it).
    ///
    /// Taking `self` by value is the point: no `set` or merge can follow, and
    /// a set populated through the atomic setters can only get here once
    /// every borrow the writer threads held has ended. The chunks are moved,
    /// not copied; the index is built while they are borrowed, so peak
    /// memory is the set plus the index.
    ///
    /// The build has no cancellation checkpoint: callers check only before
    /// and after it, so a tripped token waits for the pass to finish. A check
    /// inside would be per chunk of a tight in-memory loop; if a stop ever
    /// proves too slow, chunk the build and check per chunk, with a matched
    /// A/B showing the untripped check costs nothing.
    pub fn into_ranked(self) -> RankedIdSet {
        let mut chunk_prefix = Vec::with_capacity(self.chunks.len() + 1);
        let mut block_prefix = Vec::with_capacity(self.chunks.len());
        let mut cumulative: u64 = 0;
        for chunk in &self.chunks {
            chunk_prefix.push(cumulative);
            match chunk {
                Some(chunk) => {
                    let (blocks, count) = chunk.rank_blocks();
                    block_prefix.push(blocks);
                    cumulative += count;
                }
                None => block_prefix.push(None),
            }
        }
        chunk_prefix.push(cumulative);

        let mut ov_keys = Vec::with_capacity(self.overflow.len());
        let mut ov_prefix = Vec::with_capacity(self.overflow.len() + 1);
        let mut ov_block = Vec::with_capacity(self.overflow.len());
        for (&cid, chunk) in &self.overflow {
            ov_keys.push(cid);
            ov_prefix.push(cumulative);
            let (blocks, count) = chunk.rank_blocks();
            ov_block.push(blocks);
            cumulative += count;
        }
        ov_prefix.push(cumulative);

        RankedIdSet {
            set: self,
            rank: RankIndex {
                chunk_prefix,
                block_prefix,
                ov_keys,
                ov_prefix,
                ov_block,
            },
        }
    }

    /// Total number of set IDs without a rank index (walks every chunk).
    #[cfg(test)]
    fn count_slow(&self) -> u64 {
        self.chunks
            .iter()
            .flatten()
            .chain(self.overflow.values())
            .map(Chunk::count)
            .sum()
    }
}

/// Rank of member offset `off` in the present chunk `chunk` (chunk id `cid`,
/// found at `slot`): set IDs strictly below it.
#[inline]
fn rank_in_chunk(idx: &RankIndex, slot: Slot, cid: u64, chunk: &Chunk, off: u32) -> u64 {
    match slot {
        Slot::Direct(i) => {
            idx.chunk_prefix[i] + chunk.rank_within(off, idx.block_prefix[i].as_deref())
        }
        Slot::Overflow => {
            let i = idx.ov_keys.partition_point(|&k| k < cid);
            idx.ov_prefix[i] + chunk.rank_within(off, idx.ov_block[i].as_deref())
        }
    }
}

impl RankedIdSet {
    #[inline]
    pub fn get(&self, id: i64) -> bool {
        self.set.get(id)
    }

    /// Returns the rank (0-based position among all set IDs) of `id`: the
    /// number of set IDs strictly below it. Defined for every `i64`, whether
    /// or not `id` is set: arguments at or below zero return 0, arguments
    /// past the highest chunk return `total_count()`.
    pub fn rank(&self, id: i64) -> u64 {
        if id <= 0 {
            return 0;
        }
        let idx = &self.rank;
        let Some((cid, off)) = split(id) else {
            return 0;
        };
        let chunks = &self.set.chunks;
        if cid < DIRECT_CHUNKS {
            let i = cid as usize;
            if i >= chunks.len() {
                // Past every direct chunk; every overflow ID is larger.
                return idx.chunk_prefix[chunks.len()];
            }
            match &chunks[i] {
                Some(chunk) => rank_in_chunk(idx, Slot::Direct(i), cid, chunk, off),
                None => idx.chunk_prefix[i],
            }
        } else {
            let i = idx.ov_keys.partition_point(|&k| k < cid);
            let mut r = idx.ov_prefix[i];
            if idx.ov_keys.get(i) == Some(&cid)
                && let Some(chunk) = self.set.overflow.get(&cid)
            {
                r += chunk.rank_within(off, idx.ov_block[i].as_deref());
            }
            r
        }
    }

    /// Combined get + rank in a single lookup. Returns `Some(rank)` if `id`
    /// is set, `None` if not. Avoids the double chunk lookup of separate
    /// `get()` + `rank()` calls.
    #[inline]
    pub fn rank_if_set(&self, id: i64) -> Option<u64> {
        let (cid, off) = split(id)?;
        let (slot, chunk) = self.set.chunk(cid)?;
        if !chunk.contains(off) {
            return None;
        }
        Some(rank_in_chunk(&self.rank, slot, cid, chunk, off))
    }

    /// Returns the total number of set IDs.
    pub fn total_count(&self) -> u64 {
        self.rank.total()
    }

    /// Discard the rank index and hand the set back for further mutation.
    /// Consuming, so no ranked view of the set can outlive the change.
    #[cfg(test)]
    fn into_inner(self) -> IdSet {
        self.set
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A false indexdata claim from a small file cannot size the bitmap; an
    /// honest planet-scale claim passes untouched; nothing overflows.
    #[test]
    fn file_bounded_max_id_caps_a_claim_by_the_input_size() {
        assert_eq!(IdSet::file_bounded_max_id(i64::MAX, 4096, 64), 4096 * 64);
        assert_eq!(IdSet::file_bounded_max_id(i64::MAX, 4096, 8), 4096 * 8);
        let planet_bytes = 80_u64 * 1024 * 1024 * 1024;
        assert_eq!(
            IdSet::file_bounded_max_id(60_000_000_000, planet_bytes, 8),
            60_000_000_000
        );
        assert_eq!(IdSet::file_bounded_max_id(i64::MAX, u64::MAX, 64), i64::MAX);
        assert_eq!(IdSet::file_bounded_max_id(-1, 4096, 64), -1);
    }

    // --- Non-panicking atomic setters -----------------------------------

    #[test]
    fn checked_atomic_setters_report_out_of_range_instead_of_panicking() {
        let mut s = IdSet::new();
        s.pre_allocate(1_000);

        // In range: normal behaviour, and the bit really lands.
        assert_eq!(s.set_atomic_if_new_checked(500), Some(true));
        assert_eq!(
            s.set_atomic_if_new_checked(500),
            Some(false),
            "the second insert of the same id must report already-seen, not \
             a fresh insert - otherwise duplicate detection is vacuous"
        );
        assert!(s.get(500));
        assert!(s.set_atomic_checked(600));
        assert!(s.get(600));

        // Above the pre-allocated range: reported, not fatal. This is the
        // assertion that isolates the new behaviour - `set_atomic` /
        // `set_atomic_if_new` panic on exactly this input, which on an
        // untrusted file aborts the very check meant to diagnose it.
        let far = 1_i64 << 40;
        assert_eq!(s.set_atomic_if_new_checked(far), None);
        assert!(!s.set_atomic_checked(far));

        // Negative IDs are legal OSM (JOSM editor-local staging IDs) and must
        // not panic either.
        assert_eq!(s.set_atomic_if_new_checked(-1), None);
        assert!(!s.set_atomic_checked(-1));
        assert!(!s.get(-1));
    }

    #[test]
    #[should_panic(expected = "negative id -7")]
    fn set_atomic_names_a_negative_id_instead_of_blaming_pre_allocate() {
        let mut s = IdSet::new();
        s.pre_allocate(1_000);
        s.set_atomic(-7);
    }

    #[test]
    #[should_panic(expected = "which pre_allocate did not cover")]
    fn set_atomic_past_the_pre_allocated_range_still_panics() {
        let mut s = IdSet::new();
        s.pre_allocate(1_000);
        s.set_atomic(1_i64 << 40);
    }

    #[test]
    fn atomic_setters_need_pre_allocate_not_a_prior_set() {
        // A chunk populated by `set` alone is an array chunk, which the
        // atomic setters cannot write under `&self`. The documented contract
        // was always "requires pre_allocate"; this pins the consequence.
        let mut s = IdSet::new();
        s.set(42);
        assert!(!s.set_atomic_checked(43));
        // pre_allocate promotes it in place and keeps its member.
        s.pre_allocate(100);
        assert!(s.get(42));
        assert!(s.set_atomic_checked(43));
        assert!(s.get(43));
    }

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
        // A chunk covers 2^25 = 33_554_432 IDs.
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
    fn extreme_ids_cost_no_allocation_proportional_to_the_id() {
        // One ID near i64::MAX used to resize the chunk directory to
        // ~2^38 entries and abort. It now lands in one overflow array chunk.
        let mut s = IdSet::new();
        s.set(i64::MAX);
        s.set(i64::MAX - 1);
        s.set(1_i64 << 50);
        s.set(7);
        assert!(s.get(i64::MAX));
        assert!(s.get(i64::MAX - 1));
        assert!(s.get(1_i64 << 50));
        assert!(!s.get(i64::MAX - 2));
        assert_eq!(s.chunks.len(), 1, "direct directory must not grow");
        assert_eq!(s.overflow.len(), 2);
        assert!(
            s.overflow.values().all(|c| matches!(c, Chunk::Sparse(_))),
            "an isolated id must not cost a 4 MiB bit array"
        );
        assert_eq!(
            s.iter().collect::<Vec<_>>(),
            vec![7, 1_i64 << 50, i64::MAX - 1, i64::MAX]
        );
        assert!(s.any_in_range(i64::MAX, i64::MAX));
        let s = s.into_ranked();
        assert_eq!(s.total_count(), 4);
        assert_eq!(s.rank(i64::MAX), 3);
        assert_eq!(s.rank_if_set(i64::MAX - 1), Some(2));
    }

    #[test]
    fn promotion_at_exactly_sparse_max_plus_one() {
        let mut s = IdSet::new();
        // Descending, so the append fast path never applies.
        for k in (0..SPARSE_MAX as i64).rev() {
            assert!(s.set_if_new(k * 3));
        }
        assert!(matches!(s.chunks[0], Some(Chunk::Sparse(_))));
        // A duplicate at the boundary must not promote.
        assert!(!s.set_if_new(0));
        assert!(matches!(s.chunks[0], Some(Chunk::Sparse(_))));
        // Member SPARSE_MAX + 1 promotes and keeps every earlier member.
        assert!(s.set_if_new(1));
        assert!(matches!(s.chunks[0], Some(Chunk::Dense(_))));
        for k in 0..SPARSE_MAX as i64 {
            assert!(s.get(k * 3));
        }
        assert!(s.get(1));
        assert!(!s.get(2));
        let s = s.into_ranked();
        assert_eq!(s.total_count(), SPARSE_MAX as u64 + 1);
        assert_eq!(s.rank(3), 2); // 0 and 1 are below 3
    }

    #[test]
    fn rank_is_total_over_every_i64() {
        // `rank` used to index `chunk_prefix[cid]` unchecked and panicked
        // past the last chunk or on a negative id.
        let mut s = IdSet::new();
        s.set(5);
        s.set(10);
        let s = s.into_ranked();
        assert_eq!(s.rank(-1), 0);
        assert_eq!(s.rank(i64::MIN), 0);
        assert_eq!(s.rank(0), 0);
        assert_eq!(s.rank(6), 1);
        assert_eq!(s.rank(1_000_000_000_000), 2);
        assert_eq!(s.rank(i64::MAX), 2);

        let empty = IdSet::new().into_ranked();
        assert_eq!(empty.total_count(), 0);
        assert_eq!(empty.rank(i64::MAX), 0);
        assert_eq!(empty.rank(12_345), 0);
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

        // Range that does not contain any set ID. Exact, not word-granular.
        assert!(!s.any_in_range(56, 199));
        assert!(!s.any_in_range(0, 47));
        assert!(!s.any_in_range(208, 300));

        // Inverted range returns false
        assert!(!s.any_in_range(100, 10));
    }

    #[test]
    fn any_in_range_on_a_bit_array_chunk_is_exact() {
        let mut s = IdSet::new();
        s.pre_allocate(0);
        s.set(50);
        s.set(200);
        assert!(matches!(s.chunks[0], Some(Chunk::Dense(_))));
        assert!(s.any_in_range(40, 60));
        assert!(s.any_in_range(50, 50));
        assert!(!s.any_in_range(56, 199));
        assert!(!s.any_in_range(0, 47));
        assert!(!s.any_in_range(208, 300));
        assert!(!s.any_in_range(51, 63));
        assert!(s.any_in_range(0, i64::MAX));
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
    fn set_ignores_negative_ids() {
        let mut s = IdSet::new();
        s.set(-1);
        s.set(i64::MIN);
        assert!(!s.set_if_new(-5));
        assert!(!s.has_any());
        assert!(!s.get(-1));
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
        let s = s.into_ranked();

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
        let s = s.into_ranked();

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
    fn re_ranking_after_mutation_sees_the_new_members() {
        let mut s = IdSet::new();
        s.set(10);
        let ranked = s.into_ranked();
        assert_eq!(ranked.rank_if_set(10), Some(0));
        let mut s = ranked.into_inner();
        s.set(5);
        let mut borrowed = IdSet::new();
        borrowed.set(3);
        s.merge_from(&borrowed);
        let mut owned = IdSet::new();
        owned.set(1);
        s.merge(owned);
        let ranked = s.into_ranked();
        assert_eq!(ranked.rank_if_set(10), Some(3));
        assert_eq!(ranked.total_count(), 4);
    }

    #[test]
    fn atomic_storage_is_u64_aligned_and_parallel_fill_survives_rank_build() {
        let mut s = IdSet::new();
        s.pre_allocate(10_000);
        let Some(Chunk::Dense(words)) = s.chunks[0].as_ref() else {
            panic!("pre_allocate must create a bit-array chunk");
        };
        assert_eq!((words.as_ptr() as usize) % std::mem::align_of::<u64>(), 0);
        std::thread::scope(|scope| {
            for offset in 0..8 {
                let shared = &s;
                scope.spawn(move || {
                    for id in (offset..10_000).step_by(8) {
                        shared.set_atomic(id);
                    }
                });
            }
        });
        assert_eq!(s.into_ranked().total_count(), 10_000);
    }

    // --- Differential test against an ordered reference set --------------

    /// Deterministic xorshift64* so the op sequence is reproducible.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// IDs concentrated where the representation has seams: chunk edges,
    /// the direct/overflow boundary, `i64::MAX`, and negatives.
    fn interesting_id(rng: &mut Rng) -> i64 {
        let chunk = 1_i64 << ID_SHIFT;
        let overflow_base = (DIRECT_CHUNKS as i64) << ID_SHIFT;
        let jitter = rng.below(64) as i64;
        match rng.below(9) {
            0 => rng.below(4_000) as i64,
            1 => chunk - 32 + jitter,
            2 => 3 * chunk - 32 + jitter,
            3 => overflow_base - 32 + jitter,
            4 => overflow_base + 7 * chunk + jitter,
            5 => overflow_base + 3 * chunk - 32 + jitter,
            6 => i64::MAX - jitter,
            7 => -1 - jitter,
            _ => (rng.below(1 << 20) as i64) * 977,
        }
    }

    /// Check `s` against `reference`, then hand it back unranked so the
    /// caller can keep mutating it.
    fn assert_matches_reference(s: IdSet, reference: &std::collections::BTreeSet<i64>) -> IdSet {
        assert_eq!(
            s.iter().collect::<Vec<_>>(),
            reference.iter().copied().collect::<Vec<_>>(),
            "iter order and contents"
        );
        assert_eq!(s.count_slow(), reference.len() as u64);
        let s = s.into_ranked();
        assert_eq!(s.total_count(), reference.len() as u64);

        let sorted: Vec<i64> = reference.iter().copied().collect();
        let below_of = |p: i64| sorted.partition_point(|&x| x < p) as u64;
        let mut rng = Rng(0xDEAD_BEEF);
        let mut probes: Vec<i64> = sorted.clone();
        probes.extend((0..400).map(|_| interesting_id(&mut rng)));
        probes.extend([i64::MIN, -1, 0, 1, i64::MAX]);
        for &p in &probes {
            let below = below_of(p);
            assert_eq!(s.get(p), reference.contains(&p), "get({p})");
            assert_eq!(s.rank(p), below, "rank({p})");
            let expected_rank = reference.contains(&p).then_some(below);
            assert_eq!(s.rank_if_set(p), expected_rank, "rank_if_set({p})");
        }
        let s = s.into_inner();
        for _ in 0..400 {
            let a = interesting_id(&mut rng);
            let b = interesting_id(&mut rng);
            let (lo, hi) = (a.min(b), a.max(b));
            let hit = reference.range(lo..=hi).next().is_some();
            assert_eq!(s.any_in_range(lo, hi), hit, "any_in_range({lo}, {hi})");
        }
        s
    }

    /// Push one direct and one overflow chunk over the promotion threshold.
    fn fill_past_promotion(s: &mut IdSet, reference: &mut std::collections::BTreeSet<i64>) {
        let overflow_base = (DIRECT_CHUNKS as i64) << ID_SHIFT;
        for k in 0..(SPARSE_MAX as i64 + 10) {
            for id in [5 * k, overflow_base + 11 * k] {
                s.set(id);
                reference.insert(id);
            }
        }
    }

    /// A second set of random ids, mirrored into the reference.
    fn random_other(rng: &mut Rng, reference: &mut std::collections::BTreeSet<i64>) -> IdSet {
        let mut other = IdSet::new();
        for _ in 0..1_500 {
            let id = interesting_id(rng);
            other.set(id);
            if id >= 0 {
                reference.insert(id);
            }
        }
        other
    }

    #[test]
    fn differential_against_btreeset() {
        use std::collections::BTreeSet;

        let mut rng = Rng(0x1234_5678_9ABC_DEF1);
        let mut s = IdSet::new();
        let mut reference = BTreeSet::new();

        for round in 0..6 {
            for _ in 0..3_000 {
                let id = interesting_id(&mut rng);
                let fresh = s.set_if_new(id);
                assert_eq!(fresh, id >= 0 && reference.insert(id), "set_if_new({id})");
            }
            if round == 2 {
                fill_past_promotion(&mut s, &mut reference);
            }
            // Merge in another set, alternating owned and borrowed forms.
            let other = random_other(&mut rng, &mut reference);
            if round % 2 == 0 {
                s.merge(other);
            } else {
                s.merge_from(&other);
            }
            s = assert_matches_reference(s, &reference);
        }

        // Duplicate-only merges change nothing.
        let mut dup = IdSet::new();
        for &id in reference.iter().take(5_000) {
            dup.set(id);
        }
        s.merge_from(&dup);
        s.merge(dup);
        assert_matches_reference(s, &reference);
    }

    #[test]
    fn sparse_into_dense_merges_both_directions() {
        let mut dense = IdSet::new();
        for k in 0..(SPARSE_MAX as i64 + 1) {
            dense.set(2 * k);
        }
        let mut sparse = IdSet::new();
        sparse.set(1);
        sparse.set(3);

        // Array destination, bit-array source, owned: takes the allocation.
        let mut a = IdSet::new();
        a.set(1);
        a.set(3);
        let mut dense_copy = IdSet::new();
        dense_copy.merge_from(&dense);
        a.merge(dense_copy);
        assert!(matches!(a.chunks[0], Some(Chunk::Dense(_))));
        // Bit-array destination, array source.
        dense.merge_from(&sparse);
        assert_eq!(
            a.iter().collect::<Vec<_>>(),
            dense.iter().collect::<Vec<_>>()
        );
        // Two array chunks whose deduplicated union exceeds SPARSE_MAX.
        let mut x = IdSet::new();
        let mut y = IdSet::new();
        for k in 0..SPARSE_MAX as i64 {
            x.set(2 * k);
        }
        for k in 0..(SPARSE_MAX as i64 - 1) {
            y.set(2 * k + 1);
        }
        y.set(0); // duplicate of x's first member; y stays an array chunk
        assert!(matches!(y.chunks[0], Some(Chunk::Sparse(_))));
        x.merge_from(&y);
        assert!(matches!(x.chunks[0], Some(Chunk::Dense(_))));
        assert_eq!(x.into_ranked().total_count(), 2 * SPARSE_MAX as u64 - 1);
    }
}
