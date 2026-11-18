//! Signed element ID sets for O(1) membership testing, with rank queries
//! once frozen.
//!
//! [`IdSet`] and [`RankedIdSet`] hold any `i64`, negative IDs included: an
//! OSM file may carry negative element IDs (JOSM editor-local staging IDs),
//! and a set never drops one. Inside, each set is two private unsigned
//! bitmaps over a checked [`NonNegId`]: one keyed by the non-negative IDs
//! themselves, one keyed by `!id` for the negatives (`-1 -> 0`,
//! `i64::MIN -> i64::MAX`, no signed negation, so every `i64` is
//! representable). A set with no negative member never allocates for the
//! negative side, and a non-negative ID costs one sign branch more than the
//! bitmap alone.
//!
//! Every order-dependent operation names its order:
//!
//! - **numeric**: signed numeric order (`i64::MIN .. i64::MAX`), the order of
//!   indexdata `min_id` / `max_id` extrema and of numeric ID intervals;
//! - **canonical**: OSM `id_order` (`0`, then `-1, -2, ...`, then
//!   `1, 2, ...`), the order of a sorted file (`crate::osm_id`).
//!
//! On a set without negative members the two orders agree.

// The whole module is bit arithmetic between `i64` ids, `u64` keys and chunk
// ids and `u32` / `usize` in-chunk offsets. Every narrowing cast is bounded
// by construction (an offset is masked to 25 bits, a direct chunk id is below
// `DIRECT_CHUNKS`, a key is at most `i64::MAX` because it comes from a
// `NonNegId`), so the cast lints are allowed module-wide instead of on every
// function.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Bytes in one bit-array chunk (4 MiB).
const CHUNK_BYTES: usize = 1 << 22;
const WORDS_PER_CHUNK: usize = CHUNK_BYTES / std::mem::size_of::<u64>();
/// Keys per chunk = bits per bit-array chunk = 2^25.
const ID_SHIFT: u32 = 25;
const OFFSET_MASK: u64 = (1 << ID_SHIFT) - 1;

/// Chunk ids below this live in the directly indexed directory. 2^18 chunks
/// cover keys below 2^43 (~8.8e12); the directory a single high key can
/// force is therefore bounded by a few MiB instead of growing with the key.
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

// ---------------------------------------------------------------------------
// Keys and sign routing
// ---------------------------------------------------------------------------

/// A bitmap key: a value in `0..=i64::MAX`, checked when it is made. The
/// unsigned backend takes nothing else, so no raw `i64` reaches it and no
/// sign can be lost on the way in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct NonNegId(u64);

impl NonNegId {
    /// `id` as a key, or `None` for a negative `id`.
    #[inline]
    fn new(id: i64) -> Option<Self> {
        (id >= 0).then_some(Self(id as u64))
    }

    /// `(chunk id, in-chunk offset)`.
    #[inline]
    fn split(self) -> (u64, u32) {
        (self.0 >> ID_SHIFT, (self.0 & OFFSET_MASK) as u32)
    }

    /// The key at chunk `cid`, offset `off`. `cid` and `off` come from a
    /// stored member, so the key is at most `i64::MAX`.
    #[inline]
    fn join(cid: u64, off: u32) -> Self {
        Self((cid << ID_SHIFT) + u64::from(off))
    }
}

/// Which bitmap an ID lives in, and its key there.
#[derive(Clone, Copy)]
enum Family {
    /// A non-negative ID, keyed by itself.
    NonNeg(NonNegId),
    /// A negative ID, keyed by `!id`: `-1 -> 0`, `i64::MIN -> i64::MAX`.
    /// Ascending keys are descending numeric IDs and ascending canonical
    /// ones.
    Neg(NonNegId),
}

/// The one sign decision every signed operation makes.
#[inline]
fn family(id: i64) -> Family {
    if id >= 0 {
        Family::NonNeg(NonNegId(id as u64))
    } else {
        Family::Neg(NonNegId((!id) as u64))
    }
}

/// The negative ID behind a negative-family key.
#[inline]
fn neg_id(key: NonNegId) -> i64 {
    !(key.0 as i64)
}

/// A zeroed bit array. Allocated with `alloc_zeroed` semantics so a chunk this
/// large comes from fresh zero pages and is resident only as far as it is
/// written, rather than being touched end to end up front.
fn new_dense() -> Box<[AtomicU64]> {
    let words = Box::<[AtomicU64]>::new_zeroed_slice(WORDS_PER_CHUNK);
    // SAFETY: an all-zero bit pattern is a valid `AtomicU64` (value 0).
    unsafe { words.assume_init() }
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

// ---------------------------------------------------------------------------
// Chunks
// ---------------------------------------------------------------------------

/// One 2^25-key chunk.
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
        self.offsets_from(0)
    }

    /// Member offsets at or above `from`, ascending, starting at `from`'s
    /// word rather than the chunk's first.
    fn offsets_from(&self, from: u32) -> ChunkOffsets<'_> {
        match self {
            Chunk::Dense(words) => {
                let word = (from >> 6) as usize;
                ChunkOffsets::Dense {
                    words: &words[..],
                    next: word + 1,
                    cur: words[word].load(Relaxed) & (u64::MAX << (from & 63)),
                    cur_base: (word as u32) * 64,
                }
            }
            Chunk::Sparse(offsets) => {
                let i = offsets.partition_point(|&x| x < from);
                ChunkOffsets::Sparse(offsets[i..].iter())
            }
        }
    }

    /// Member offsets at or below `to`, descending, starting at `to`'s word
    /// rather than the chunk's last.
    fn offsets_rev_to(&self, to: u32) -> ChunkOffsetsRev<'_> {
        match self {
            Chunk::Dense(words) => {
                let word = (to >> 6) as usize;
                let bit = to & 63;
                let mask = if bit == 63 {
                    u64::MAX
                } else {
                    (1u64 << (bit + 1)) - 1
                };
                ChunkOffsetsRev::Dense {
                    words: &words[..],
                    end: word,
                    cur: words[word].load(Relaxed) & mask,
                    cur_base: (word as u32) * 64,
                }
            }
            Chunk::Sparse(offsets) => {
                let i = offsets.partition_point(|&x| x <= to);
                ChunkOffsetsRev::Sparse(offsets[..i].iter().rev())
            }
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
                // loop-ends: consumes a set bit or one word per pass, ending at
                // the last word; the load reads the bitset, nothing waits
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

/// Descending in-chunk offsets of one chunk's members.
enum ChunkOffsetsRev<'a> {
    Dense {
        words: &'a [AtomicU64],
        /// One past the index of the next word to load.
        end: usize,
        /// Unvisited set bits of the current word.
        cur: u64,
        /// Bit offset of the current word.
        cur_base: u32,
    },
    Sparse(std::iter::Rev<std::slice::Iter<'a, u32>>),
}

impl Iterator for ChunkOffsetsRev<'_> {
    type Item = u32;

    #[inline]
    fn next(&mut self) -> Option<u32> {
        match self {
            ChunkOffsetsRev::Dense {
                words,
                end,
                cur,
                cur_base,
                // loop-ends: consumes a set bit or one word per pass, ending at
                // the first word; the load reads the bitset, nothing waits
            } => loop {
                if *cur != 0 {
                    let top = 63 - cur.leading_zeros();
                    *cur &= !(1u64 << top);
                    return Some(*cur_base + top);
                }
                if *end == 0 {
                    return None;
                }
                *end -= 1;
                *cur = words[*end].load(Relaxed);
                *cur_base = (*end as u32) * 64;
            },
            ChunkOffsetsRev::Sparse(it) => it.next().copied(),
        }
    }
}

/// Prefix-sum index behind one bitmap's ranks. Built and dropped as one
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
    /// total, so an empty bitmap has `[0]`.
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

/// Rank of member offset `off` in the present chunk `chunk` (chunk id `cid`,
/// found at `slot`): keys strictly below it.
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

// ---------------------------------------------------------------------------
// The unsigned backend
// ---------------------------------------------------------------------------

/// Chunked sparse bitmap over [`NonNegId`] keys, the private backend of both
/// families of an [`IdSet`].
///
/// Mirrors osmium's `IdSetDense`: the key space is cut into chunks of 2^25
/// keys, and a populated chunk is normally a 4 MiB bit array. Lookup and
/// insertion on such a chunk are 3 instructions (chunk index + word offset +
/// bitmask), with no hashing or sorting overhead.
///
/// - **Directory.** Chunks below [`DIRECT_CHUNKS`] (keys below 2^43) live in
///   a directly indexed `Vec`, so real data never pays more than an index
///   for the chunk lookup. Chunks at or above it live in an ordered map, so
///   the directory does not grow with the largest key ever inserted: one key
///   near `i64::MAX` used to resize the `Vec` to ~2^38 entries (about 4 TB)
///   and abort the process.
/// - **Chunks.** A chunk starts as a sorted array of in-chunk offsets and is
///   promoted to the 4 MiB bit array when it would exceed [`SPARSE_MAX`]
///   members. Heavily populated chunks use the bit-array form, while an
///   isolated key does not cost a whole 4 MiB chunk.
struct Bitmap {
    /// Chunks with chunk id below `DIRECT_CHUNKS`, indexed by chunk id.
    chunks: Vec<Option<Chunk>>,
    /// Chunks with chunk id at or above `DIRECT_CHUNKS`, keyed by chunk id.
    /// Every key here is above every key in `chunks`.
    overflow: BTreeMap<u64, Chunk>,
}

impl Bitmap {
    const fn new() -> Self {
        Self {
            chunks: Vec::new(),
            overflow: BTreeMap::new(),
        }
    }

    fn has_any(&self) -> bool {
        self.chunks.iter().any(Option::is_some) || !self.overflow.is_empty()
    }

    fn allocated_chunk_count(&self) -> usize {
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

    #[inline]
    fn insert(&mut self, key: NonNegId) {
        let (cid, off) = key.split();
        // Fast path: an existing bit-array chunk in the direct directory,
        // the steady state for real OSM data. A plain OR, as the bitmap-only
        // set had; routing every call through `insert_if_new`'s chunk-kind
        // match measured ~1.1 ns/ref slower in altw pass 0's serial union.
        // An overflow chunk id is past `chunks.len()`, so `get_mut` misses.
        if let Some(Some(Chunk::Dense(words))) = self.chunks.get_mut(cid as usize) {
            *words[(off >> 6) as usize].get_mut() |= 1u64 << (off & 63);
            return;
        }
        self.chunk_entry(cid).insert(off);
    }

    #[inline]
    fn insert_if_new(&mut self, key: NonNegId) -> bool {
        let (cid, off) = key.split();
        self.chunk_entry(cid).insert(off)
    }

    /// Bit-array chunks for every key in `0..=max`; see
    /// [`IdSet::pre_allocate`].
    fn pre_allocate(&mut self, max: NonNegId) {
        let (max_cid, _) = max.split();
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

    /// The bit-array chunk an atomic setter may write for `key`, or `None`:
    /// no chunk, or a chunk that only holds array contents (the atomic
    /// setters cannot grow or convert a chunk under `&self`).
    #[inline]
    fn dense_for_atomic(&self, key: NonNegId) -> Option<(&[AtomicU64], u32)> {
        let (cid, off) = key.split();
        match self.chunk(cid)? {
            (_, Chunk::Dense(words)) => Some((&words[..], off)),
            (_, Chunk::Sparse(_)) => None,
        }
    }

    /// The highest allocated chunk id, for the atomic setters' panic message.
    fn last_chunk(&self) -> Option<u64> {
        self.overflow.keys().next_back().copied().or_else(|| {
            self.chunks
                .iter()
                .rposition(Option::is_some)
                .map(|i| i as u64)
        })
    }

    #[inline]
    fn get(&self, key: NonNegId) -> bool {
        let (cid, off) = key.split();
        self.chunk(cid)
            .is_some_and(|(_, chunk)| chunk.contains(off))
    }

    /// Whether any key in the inclusive range `[min, max]` is a member.
    fn any_in(&self, min: NonNegId, max: NonNegId) -> bool {
        if min > max {
            return false;
        }
        let (min_cid, min_off) = min.split();
        let (max_cid, max_off) = max.split();
        // Clip the query to one chunk's offsets. Offsets are within-chunk,
        // so no bound is ever formed as `key + 1`.
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

    /// Members at or above `start`, ascending. Chunks wholly below `start`
    /// are skipped without being walked.
    fn iter_from(&self, start: NonNegId) -> impl Iterator<Item = NonNegId> + '_ {
        let (first_cid, first_off) = start.split();
        let direct_skip = usize::try_from(first_cid).unwrap_or(usize::MAX);
        let direct = self
            .chunks
            .iter()
            .enumerate()
            .skip(direct_skip)
            .filter_map(|(cid, chunk)| chunk.as_ref().map(|chunk| (cid as u64, chunk)));
        let overflow = self
            .overflow
            .range(first_cid.max(DIRECT_CHUNKS)..)
            .map(|(&cid, chunk)| (cid, chunk));
        direct.chain(overflow).flat_map(move |(cid, chunk)| {
            let from = if cid == first_cid { first_off } else { 0 };
            chunk
                .offsets_from(from)
                .map(move |off| NonNegId::join(cid, off))
        })
    }

    /// Members at or below `last`, descending. Chunks wholly above `last`
    /// are skipped without being walked.
    fn iter_rev_to(&self, last: NonNegId) -> impl Iterator<Item = NonNegId> + '_ {
        let (last_cid, last_off) = last.split();
        let overflow = self
            .overflow
            .range(DIRECT_CHUNKS..=last_cid.max(DIRECT_CHUNKS))
            .rev()
            .filter(move |&(&cid, _)| cid <= last_cid)
            .map(|(&cid, chunk)| (cid, chunk));
        let direct_end = usize::try_from(last_cid.saturating_add(1))
            .unwrap_or(usize::MAX)
            .min(self.chunks.len());
        let direct = self.chunks[..direct_end]
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(cid, chunk)| chunk.as_ref().map(|chunk| (cid as u64, chunk)));
        overflow.chain(direct).flat_map(move |(cid, chunk)| {
            let to = if cid == last_cid {
                last_off
            } else {
                OFFSET_MASK as u32
            };
            chunk
                .offsets_rev_to(to)
                .map(move |off| NonNegId::join(cid, off))
        })
    }

    /// An upper bound, in bytes, on this bitmap plus its rank index: the
    /// direct directory and its per-entry rank vectors (allocated across the
    /// whole directory, empty entries included), and per allocated chunk a
    /// 4 MiB bit array (an array chunk is at most `SPARSE_MAX` `u32`s, far
    /// less), its block prefixes and an overflow-map entry.
    fn ranked_bytes_upper_bound(&self) -> u64 {
        use std::mem::size_of;
        let per_entry =
            size_of::<Option<Chunk>>() + size_of::<u64>() + size_of::<Option<Vec<u32>>>();
        let per_chunk = CHUNK_BYTES + BLOCKS_PER_CHUNK * size_of::<u32>() + 128;
        let directory = (self.chunks.len() + 1) as u64 * per_entry as u64;
        directory + self.allocated_chunk_count() as u64 * per_chunk as u64
    }

    /// The bytes this bitmap holds now, and with `ranked` also the rank index
    /// [`Self::rank_index`] would build for it, priced by what each chunk
    /// is: a bit array its 4 MiB (and its block prefixes when ranked), an
    /// array chunk the capacity of its offsets, plus the direct directory
    /// and an entry per overflow chunk. Unlike
    /// [`Self::ranked_bytes_upper_bound`], which charges every chunk as a
    /// bit array, it does not grow a thinly spread set into gigabytes.
    fn footprint_bytes(&self, ranked: bool) -> u64 {
        use std::mem::size_of;
        let rank_entry = if ranked {
            size_of::<u64>() + size_of::<Option<Vec<u32>>>()
        } else {
            0
        };
        let direct_entry = size_of::<Option<Chunk>>() + rank_entry;
        let overflow_entry = size_of::<u64>() + size_of::<Chunk>() + 64 + rank_entry;
        let chunk_bytes = |chunk: &Chunk| -> u64 {
            match chunk {
                Chunk::Dense(_) => {
                    let prefixes = if ranked {
                        BLOCKS_PER_CHUNK * size_of::<u32>()
                    } else {
                        0
                    };
                    (CHUNK_BYTES + prefixes) as u64
                }
                Chunk::Sparse(offsets) => (offsets.capacity() * size_of::<u32>()) as u64,
            }
        };
        let directory = (self.chunks.len() + 1) as u64 * direct_entry as u64
            + self.overflow.len() as u64 * overflow_entry as u64;
        directory
            + self
                .chunks
                .iter()
                .flatten()
                .chain(self.overflow.values())
                .map(chunk_bytes)
                .sum::<u64>()
    }

    fn merge_from(&mut self, other: &Bitmap) {
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

    fn merge(&mut self, other: Bitmap) {
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

    /// The prefix-sum index of this bitmap's current members.
    fn rank_index(&self) -> RankIndex {
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

        RankIndex {
            chunk_prefix,
            block_prefix,
            ov_keys,
            ov_prefix,
            ov_block,
        }
    }

    /// Members strictly below `key`, defined for every key.
    fn rank(&self, idx: &RankIndex, key: NonNegId) -> u64 {
        let (cid, off) = key.split();
        if cid < DIRECT_CHUNKS {
            let i = cid as usize;
            if i >= self.chunks.len() {
                // Past every direct chunk; every overflow key is larger.
                return idx.chunk_prefix[self.chunks.len()];
            }
            match &self.chunks[i] {
                Some(chunk) => rank_in_chunk(idx, Slot::Direct(i), cid, chunk, off),
                None => idx.chunk_prefix[i],
            }
        } else {
            let i = idx.ov_keys.partition_point(|&k| k < cid);
            let mut r = idx.ov_prefix[i];
            if idx.ov_keys.get(i) == Some(&cid)
                && let Some(chunk) = self.overflow.get(&cid)
            {
                r += chunk.rank_within(off, idx.ov_block[i].as_deref());
            }
            r
        }
    }

    /// `Some(rank)` for a member, `None` otherwise, in one chunk lookup.
    #[inline]
    fn rank_if_set(&self, idx: &RankIndex, key: NonNegId) -> Option<u64> {
        let (cid, off) = key.split();
        let (slot, chunk) = self.chunk(cid)?;
        if !chunk.contains(off) {
            return None;
        }
        Some(rank_in_chunk(idx, slot, cid, chunk, off))
    }

    fn len(&self) -> u64 {
        self.chunks
            .iter()
            .flatten()
            .chain(self.overflow.values())
            .map(Chunk::count)
            .sum()
    }

    fn is_empty(&self) -> bool {
        self.chunks
            .iter()
            .flatten()
            .chain(self.overflow.values())
            .all(|chunk| chunk.offsets().next().is_none())
    }
}

// ---------------------------------------------------------------------------
// The signed set
// ---------------------------------------------------------------------------

/// A set of element IDs, any `i64` (see the module docs).
///
/// # Memory
///
/// For a set populated by `insert` / `insert_if_new` and merges of such sets,
/// memory is bounded by the number of distinct IDs, not by their spread or
/// their magnitude. Sparse chunks store offsets and promote to bit arrays
/// after [`SPARSE_MAX`] members; each family's direct directory is bounded by
/// [`DIRECT_CHUNKS`]. A promotion temporarily holds both representations. A
/// set with no negative member allocates nothing for the negative family.
///
/// The exception is [`Self::pre_allocate`], which creates empty bit-array
/// chunks for a whole non-negative ID range on request; its argument is the
/// caller's responsibility (see its docs), and merging a pre-allocated set
/// carries those chunks along.
///
/// # Rank queries
///
/// An `IdSet` answers membership only. Rank queries live on
/// [`RankedIdSet`], produced by [`Self::into_ranked`] once population is
/// complete; a rank query on a set that is still being built does not
/// compile.
pub struct IdSet {
    /// Non-negative IDs, keyed by themselves.
    nonneg: Bitmap,
    /// Negative IDs, keyed by `!id`.
    neg: Bitmap,
}

impl Default for IdSet {
    fn default() -> Self {
        Self::new()
    }
}

impl IdSet {
    pub const fn new() -> Self {
        Self {
            nonneg: Bitmap::new(),
            neg: Bitmap::new(),
        }
    }

    /// Returns `true` if any chunk has been allocated (at least one insert,
    /// or a `pre_allocate`).
    pub fn has_any(&self) -> bool {
        self.nonneg.has_any() || self.neg.has_any()
    }

    /// Returns the number of allocated chunks backing this set, in either
    /// form (sorted array or 4 MiB bit array), over both families.
    pub fn allocated_chunk_count(&self) -> usize {
        self.nonneg.allocated_chunk_count() + self.neg.allocated_chunk_count()
    }

    /// Insert `id`, any `i64`, without an allocation proportional to it.
    ///
    /// Perf note: do not call this per element inside a dense classify
    /// hot loop - the chunk lookup is random access and measured 29x
    /// slower than `Vec::push` there. Accumulate matches in a `Vec<i64>`
    /// and drain into the set afterwards; direct `insert()` is fine on
    /// sparse paths. See `reference/performance/classify.md`
    /// "IdSetDense accumulation".
    #[inline]
    pub fn insert(&mut self, id: i64) {
        match family(id) {
            Family::NonNeg(key) => self.nonneg.insert(key),
            Family::Neg(key) => self.insert_negative(key),
        }
    }

    /// The negative arm of [`Self::insert`], out of line so the non-negative
    /// arm stays a tight inlined OR in the hot loops that call `insert`.
    #[inline(never)]
    fn insert_negative(&mut self, key: NonNegId) {
        self.neg.insert(key);
    }

    /// Insert `id` and report whether it was absent: `true` the first time
    /// an ID is seen, `false` on every later call with the same ID.
    ///
    /// Duplicate-detection callers use this; monotonic-population callers
    /// should prefer the void-return [`Self::insert`].
    #[inline]
    pub fn insert_if_new(&mut self, id: i64) -> bool {
        match family(id) {
            Family::NonNeg(key) => self.nonneg.insert_if_new(key),
            Family::Neg(key) => self.neg.insert_if_new(key),
        }
    }

    /// Pre-allocate bit-array chunks for every non-negative ID in
    /// `0..=max_id`, so the atomic setters can write them through `&self`.
    /// A negative `max_id` allocates nothing, and negative IDs never become
    /// atomically writable: an atomic collector spills them (see
    /// [`Self::try_insert_atomic`]). Chunks that already hold array contents
    /// are promoted in place, keeping their members. A chunk is the unit of
    /// coverage, so IDs above `max_id` up to the end of its chunk become
    /// writable too; callers that need an exact cutoff apply it themselves.
    ///
    /// This eagerly allocates 4 MiB per 2^25 IDs of range, so `max_id` sizes
    /// the allocation directly: a caller deriving it from untrusted input
    /// (indexdata) must bound it first with [`IdSet::file_bounded_max_id`].
    ///
    /// # Invariant for callers
    ///
    /// `max_id` is the contract between the caller and every thread that
    /// later calls `insert_atomic` / `insert_atomic_if_new` through `&self`.
    /// An ID outside the covered chunks panics there (no dynamic chunk growth
    /// is possible under shared borrow). The panic message points back here.
    ///
    /// Common sources of overshoot seen in practice:
    ///
    /// - Corrupted or mismatched indexdata: a blob header reports a smaller
    ///   `max_id` than the element IDs that actually appear once the blob is
    ///   decoded. Callers that size from indexdata must route IDs above
    ///   their bound elsewhere; renumber collects them per worker and
    ///   inserts them with `insert` after its parallel pass.
    /// - Future planet growth past a hard-coded cap.
    ///
    /// The defensive option is to verify `max_id` during the schedule scan
    /// against a sanity bound, or to use the `try_` setters.
    pub fn pre_allocate(&mut self, max_id: i64) {
        if let Some(max) = NonNegId::new(max_id) {
            self.nonneg.pre_allocate(max);
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
    /// observed IDs above it after their parallel pass, so their factor
    /// trades serial overflow work against memory; `check` also adds a
    /// per-kind allocation floor. A negative claim is returned unchanged, and
    /// `pre_allocate` treats it as "nothing to allocate".
    pub(crate) fn file_bounded_max_id(claimed_max: i64, file_len: u64, ids_per_byte: u64) -> i64 {
        let by_size = i64::try_from(file_len.saturating_mul(ids_per_byte)).unwrap_or(i64::MAX);
        claimed_max.min(by_size)
    }

    /// Atomically insert `id`. Requires `pre_allocate()` to have covered
    /// `id`. Safe for concurrent use from multiple threads via `&self` (no
    /// `&mut` needed). Uses `Relaxed` ordering - callers must synchronize
    /// (e.g. thread join) before reading via `get()` or ranking via
    /// `into_ranked()`.
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
    /// an unrouted negative ID, or a missing `pre_allocate` call.
    ///
    /// Do NOT "fix" this panic by replacing it with a silent no-op or by
    /// growing the set here - both would be wrong. A silent no-op loses data;
    /// growing is unsound because `&self` methods cannot safely grow the
    /// backing storage while other threads hold pointers into it. A caller
    /// that can meet an uncovered ID uses [`Self::try_insert_atomic`] and
    /// spills.
    pub fn insert_atomic(&self, id: i64) {
        let (words, off) = self.chunk_for_atomic(id, "insert_atomic");
        words[(off >> 6) as usize].fetch_or(1u64 << (off & 63), Relaxed);
    }

    /// Atomic insert + duplicate detection. Returns `true` the first time an
    /// ID is seen (in the happens-before sense of the atomic op), `false`
    /// if some other thread already set the same bit.
    ///
    /// Requires `pre_allocate()` to have covered `id`. Safe for concurrent
    /// use from multiple threads via `&self`. Relaxed ordering: a `get()`
    /// racing the setters is a relaxed load of the same word, so a `true`
    /// from it is final but a `false` may miss a bit another thread is
    /// setting; a caller that needs every set bit synchronizes (thread join)
    /// before reading.
    ///
    /// # Panics
    ///
    /// See [`Self::insert_atomic`] - same panic contract and diagnostics.
    pub fn insert_atomic_if_new(&self, id: i64) -> bool {
        let (words, off) = self.chunk_for_atomic(id, "insert_atomic_if_new");
        let bit = 1u64 << (off & 63);
        let prev = words[(off >> 6) as usize].fetch_or(bit, Relaxed);
        (prev & bit) == 0
    }

    /// The atomic-or-spill insert for collectors of raw IDs.
    ///
    /// Inserts `id` atomically and returns `true` (whether or not it was
    /// already present) when a pre-allocated chunk covers it. Returns
    /// `false` - having recorded nothing - for an ID no atomic setter can
    /// write: a negative ID, or one outside the pre-allocated chunks. The
    /// caller then spills the ID (a per-worker list) and inserts it with
    /// [`Self::insert`] once the workers have joined, which records any
    /// `i64`.
    ///
    /// For callers processing untrusted input an uncovered ID is a property
    /// of the *file*, not a bug in the caller, so it has to be a return value
    /// rather than fatal: a panic on attacker-controlled input is a
    /// denial-of-service surface, and in `check`'s case it aborts the very
    /// diagnosis the user asked for. Callers that genuinely control the ID
    /// range keep using [`Self::insert_atomic`], whose panic catches a real
    /// caller bug.
    pub fn try_insert_atomic(&self, id: i64) -> bool {
        let Some((words, off)) = self.dense_for_atomic(id) else {
            return false;
        };
        words[(off >> 6) as usize].fetch_or(1u64 << (off & 63), Relaxed);
        true
    }

    /// [`Self::try_insert_atomic`] with duplicate detection.
    ///
    /// `None` means the ID could not be recorded at all - negative, or outside
    /// the pre-allocated chunks - and the caller spills it. `Some(true)` /
    /// `Some(false)` carry the usual first-seen / already-seen answer.
    pub fn try_insert_atomic_if_new(&self, id: i64) -> Option<bool> {
        let (words, off) = self.dense_for_atomic(id)?;
        let bit = 1u64 << (off & 63);
        let prev = words[(off >> 6) as usize].fetch_or(bit, Relaxed);
        Some((prev & bit) == 0)
    }

    /// The bit-array chunk an atomic setter may write for `id`, or `None`
    /// for a negative ID or an uncovered one. A negative ID is legal input,
    /// so landing here is an input property, not a corrupt-state signal -
    /// which is precisely why the `try_` callers get `None` rather than a
    /// panic.
    #[inline]
    fn dense_for_atomic(&self, id: i64) -> Option<(&[AtomicU64], u32)> {
        self.nonneg.dense_for_atomic(NonNegId::new(id)?)
    }

    /// Panicking chunk lookup for `insert_atomic` / `insert_atomic_if_new`,
    /// with a message that says which of the failure modes this is.
    #[inline]
    fn chunk_for_atomic(&self, id: i64, fn_name: &'static str) -> (&[AtomicU64], u32) {
        if let Some(found) = self.dense_for_atomic(id) {
            return found;
        }
        if id < 0 {
            panic!(
                "{fn_name}: negative id {id}. Atomic inserts cover only the \
                 pre-allocated non-negative range; a collector that can meet \
                 a negative ID uses `try_insert_atomic` and spills it to \
                 `insert`."
            );
        }
        let max_covered = match self.nonneg.last_chunk() {
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
        match family(id) {
            Family::NonNeg(key) => self.nonneg.get(key),
            Family::Neg(key) => self.neg.get(key),
        }
    }

    /// Whether any member lies in the inclusive signed numeric range
    /// `[min_id, max_id]` (the shape of an indexdata blob's extrema); `false`
    /// for `min_id > max_id`. A range straddling zero asks both families, so
    /// a blob holding negative and non-negative IDs is never skipped for
    /// either. Chunk-level for chunks wholly inside the range, exact within
    /// a chunk.
    pub fn any_in_numeric_range(&self, min_id: i64, max_id: i64) -> bool {
        if min_id > max_id {
            return false;
        }
        if min_id < 0 {
            // Negative IDs `[min_id, min(max_id, -1)]` are keys
            // `[!min(max_id, -1), !min_id]`.
            let hi = max_id.min(-1);
            if self
                .neg
                .any_in(NonNegId((!hi) as u64), NonNegId((!min_id) as u64))
            {
                return true;
            }
        }
        max_id >= 0
            && self
                .nonneg
                .any_in(NonNegId(min_id.max(0) as u64), NonNegId(max_id as u64))
    }

    /// The members at or above `start`, in ascending signed numeric order.
    /// Chunks wholly below `start` are skipped without being walked.
    pub fn iter_numeric_from(&self, start: i64) -> impl Iterator<Item = i64> + '_ {
        // Negative members at or above a negative `start` are keys at or
        // below `!start`, walked downwards.
        let neg_last = (start < 0).then_some(NonNegId((!start) as u64));
        let negatives = neg_last
            .into_iter()
            .flat_map(|last| self.neg.iter_rev_to(last))
            .map(neg_id);
        let nonnegatives = self
            .nonneg
            .iter_from(NonNegId(start.max(0) as u64))
            .map(|key| key.0 as i64);
        negatives.chain(nonnegatives)
    }

    /// Every member in canonical order (`0`, then `-1, -2, ...`, then
    /// `1, 2, ...`): the order a sorted file holds them in.
    pub fn iter_canonical(&self) -> impl Iterator<Item = i64> + '_ {
        let zero = self.nonneg.get(NonNegId(0)).then_some(0_i64);
        let negatives = self.neg.iter_from(NonNegId(0)).map(neg_id);
        let positives = self.nonneg.iter_from(NonNegId(1)).map(|key| key.0 as i64);
        zero.into_iter().chain(negatives).chain(positives)
    }

    /// An upper bound, in bytes, on this set plus the rank index
    /// [`into_ranked`](Self::into_ranked) would build for it (see the
    /// backend's bound). A family with no allocated chunk is charged
    /// nothing, because `into_ranked` builds no index for it.
    pub(crate) fn ranked_bytes_upper_bound(&self) -> u64 {
        let neg = if self.neg.has_any() {
            self.neg.ranked_bytes_upper_bound()
        } else {
            0
        };
        self.nonneg.ranked_bytes_upper_bound() + neg
    }

    /// The bytes the negative family holds, with `ranked` also the rank
    /// index [`into_ranked`](Self::into_ranked) would build for it, priced
    /// by what its chunks are (see the backend's footprint). Zero when the
    /// set holds no negative ID: an empty family allocates and is charged
    /// nothing.
    pub(crate) fn negative_footprint_bytes(&self, ranked: bool) -> u64 {
        if self.neg.has_any() {
            self.neg.footprint_bytes(ranked)
        } else {
            0
        }
    }

    /// A key naming the backend chunk `id` lives in, over both families
    /// (two IDs share it only if they share a family and a chunk). Two sets
    /// built over disjoint chunk keys [`merge`](Self::merge) by moving
    /// chunks, with no copy, which is what lets a caller shard one set's
    /// construction across threads by chunk.
    pub(crate) fn chunk_of(id: i64) -> u64 {
        match family(id) {
            Family::NonNeg(key) => key.split().0 << 1,
            Family::Neg(key) => (key.split().0 << 1) | 1,
        }
    }

    /// Merge all members of `other` into `self` (union), borrowing `other`.
    pub fn merge_from(&mut self, other: &IdSet) {
        self.nonneg.merge_from(&other.nonneg);
        self.neg.merge_from(&other.neg);
    }

    /// Merge another set into this one (union), consuming it.
    ///
    /// For non-overlapping chunks (common in sorted PBFs where each rayon thread
    /// processes a contiguous ID range), chunks are moved with zero copying.
    /// For overlapping chunks, the members are unioned.
    pub fn merge(&mut self, other: Self) {
        self.nonneg.merge(other.nonneg);
        self.neg.merge(other.neg);
    }

    /// Freeze the set and build the prefix-sum index behind its rank queries
    /// (O(1) on each family's direct range, O(log n) above it). No index is
    /// built for a family with no allocated chunk.
    ///
    /// Taking `self` by value is the point: no insert or merge can follow,
    /// and a set populated through the atomic setters can only get here once
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
        let nonneg_rank = self.nonneg.rank_index();
        let neg_rank = self.neg.has_any().then(|| self.neg.rank_index());
        let neg_total = neg_rank.as_ref().map_or(0, RankIndex::total);
        let zero = u64::from(self.nonneg.get(NonNegId(0)));
        RankedIdSet {
            set: self,
            nonneg_rank,
            neg_rank,
            neg_total,
            zero,
        }
    }

    /// Number of members. Walks every allocated chunk (a popcount over each
    /// bit array), so it costs in proportion to the allocation, not the
    /// members; a set that will be ranked anyway has the count for free in
    /// [`RankedIdSet::total_count`].
    pub fn len(&self) -> u64 {
        self.nonneg.len() + self.neg.len()
    }

    /// True if no ID is a member. Unlike [`Self::has_any`], an allocated
    /// chunk with no members (a `pre_allocate`d range) does not count.
    pub fn is_empty(&self) -> bool {
        self.nonneg.is_empty() && self.neg.is_empty()
    }
}

/// A completed [`IdSet`] plus its rank index.
///
/// Two rank orders are offered, each named (see the module docs): numeric
/// ranks are positions in signed numeric order, canonical ranks positions in
/// canonical order. Both are bijections from the members onto
/// `0..total_count()`, and they agree on every positive ID.
///
/// Both fields are private and no accessor hands out the inner set, not even
/// as `&IdSet`: the atomic setters write through a shared reference, so any
/// path to `&IdSet` would let members change under a built index and leave
/// its prefixes stale. Membership is forwarded explicitly via [`Self::get`].
pub struct RankedIdSet {
    set: IdSet,
    nonneg_rank: RankIndex,
    /// `None` when the set has no negative family chunk.
    neg_rank: Option<RankIndex>,
    /// Negative members.
    neg_total: u64,
    /// 1 when `0` is a member, else 0.
    zero: u64,
}

impl RankedIdSet {
    #[inline]
    pub fn get(&self, id: i64) -> bool {
        self.set.get(id)
    }

    /// The number of members numerically below `id`, for every `i64`,
    /// whether or not `id` is a member: `0` at `i64::MIN`, `total_count()`
    /// past the highest member.
    pub fn numeric_rank(&self, id: i64) -> u64 {
        match family(id) {
            Family::NonNeg(key) => self.neg_total + self.set.nonneg.rank(&self.nonneg_rank, key),
            Family::Neg(key) => match &self.neg_rank {
                // Members below a negative ID are the negative keys above
                // its key.
                Some(idx) => {
                    self.neg_total - self.set.neg.rank(idx, key) - u64::from(self.set.neg.get(key))
                }
                None => 0,
            },
        }
    }

    /// `Some(numeric_rank(id))` if `id` is a member, `None` if not, in a
    /// single chunk lookup.
    #[inline]
    pub fn numeric_rank_if_set(&self, id: i64) -> Option<u64> {
        match family(id) {
            Family::NonNeg(key) => self
                .set
                .nonneg
                .rank_if_set(&self.nonneg_rank, key)
                .map(|r| self.neg_total + r),
            Family::Neg(key) => self
                .negative_rank_if_set(key)
                .map(|r| self.neg_total - 1 - r),
        }
    }

    /// The member's position in canonical order (`0`, then `-1, -2, ...`,
    /// then `1, 2, ...`), or `None` if `id` is not a member, in a single
    /// chunk lookup. For a positive ID it equals the numeric rank.
    #[inline]
    pub fn canonical_rank_if_set(&self, id: i64) -> Option<u64> {
        match family(id) {
            Family::NonNeg(NonNegId(0)) => (self.zero == 1).then_some(0),
            Family::NonNeg(key) => self
                .set
                .nonneg
                .rank_if_set(&self.nonneg_rank, key)
                .map(|r| self.neg_total + r),
            Family::Neg(key) => self.negative_rank_if_set(key).map(|r| self.zero + r),
        }
    }

    /// The negative-family key rank of a member, or `None`.
    #[inline]
    fn negative_rank_if_set(&self, key: NonNegId) -> Option<u64> {
        self.set.neg.rank_if_set(self.neg_rank.as_ref()?, key)
    }

    /// Returns the total number of members.
    pub fn total_count(&self) -> u64 {
        self.neg_total + self.nonneg_rank.total()
    }

    /// The members at or above `start`, ascending in signed numeric order.
    /// The first one yielded has numeric rank `self.numeric_rank(start)`,
    /// and each next one the rank after it.
    pub fn iter_numeric_from(&self, start: i64) -> impl Iterator<Item = i64> + '_ {
        self.set.iter_numeric_from(start)
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
    use std::collections::BTreeSet;

    /// Canonical order (`crate::osm_id`'s `id_order`) as a sort key.
    fn canonical_key(id: i64) -> (u8, u64) {
        match id {
            0 => (0, 0),
            i if i < 0 => (1, i.unsigned_abs()),
            i => (2, i as u64),
        }
    }

    /// `iter_numeric_from` skips whole chunks below `start`, filters the
    /// rest of its chunk, and pairs with `numeric_rank`: the first ID it
    /// yields has rank `numeric_rank(start)`.
    #[test]
    fn iter_numeric_from_starts_at_the_first_member_not_below_start() {
        let mut set = IdSet::new();
        let ids = [
            i64::MIN,
            -(1 << 44),
            -(1 << 25) - 1,
            -3,
            -1,
            0,
            3,
            1 << 25,
            (1 << 25) + 9,
            1 << 44,
            (1 << 44) + 1,
        ];
        for id in ids {
            set.insert(id);
        }
        let ranked = set.into_ranked();
        for start in [
            i64::MIN,
            i64::MIN + 1,
            -(1 << 44),
            -(1 << 30),
            -4,
            -3,
            -2,
            -1,
            0,
            1,
            4,
            1 << 25,
            (1 << 25) + 1,
            1 << 40,
            (1 << 44) + 1,
            i64::MAX,
        ] {
            let want: Vec<i64> = ids.iter().copied().filter(|&id| id >= start).collect();
            assert_eq!(
                ranked.iter_numeric_from(start).collect::<Vec<_>>(),
                want,
                "start {start}"
            );
            assert_eq!(
                ranked.numeric_rank(start),
                (ids.len() - want.len()) as u64,
                "start {start}"
            );
        }
    }

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
        let mut s = IdSet::new();
        s.pre_allocate(-1);
        assert!(!s.has_any(), "a negative claim allocates nothing");
    }

    // --- Atomic setters ------------------------------------------------

    #[test]
    fn try_atomic_setters_report_uncovered_ids_for_spilling() {
        let mut s = IdSet::new();
        s.pre_allocate(1_000);

        // In range: normal behaviour, and the bit really lands.
        assert_eq!(s.try_insert_atomic_if_new(500), Some(true));
        assert_eq!(
            s.try_insert_atomic_if_new(500),
            Some(false),
            "the second insert of the same id must report already-seen, not \
             a fresh insert - otherwise duplicate detection is vacuous"
        );
        assert!(s.get(500));
        assert!(s.try_insert_atomic(600));
        assert!(
            s.try_insert_atomic(600),
            "already present still reports recorded"
        );
        assert!(s.get(600));
        // Coverage is per chunk: past `max_id` in the same chunk is writable.
        assert!(s.try_insert_atomic(2_000));

        // Above the pre-allocated chunks: reported, not fatal.
        let far = 1_i64 << 40;
        assert_eq!(s.try_insert_atomic_if_new(far), None);
        assert!(!s.try_insert_atomic(far));

        // Negative IDs are never atomically writable; the caller spills them
        // and `insert` records them.
        assert_eq!(s.try_insert_atomic_if_new(-1), None);
        assert!(!s.try_insert_atomic(-1));
        assert!(!s.get(-1));
        s.insert(-1);
        s.insert(far);
        assert!(s.get(-1) && s.get(far));
    }

    #[test]
    #[should_panic(expected = "negative id -7")]
    fn insert_atomic_names_a_negative_id_instead_of_blaming_pre_allocate() {
        let mut s = IdSet::new();
        s.pre_allocate(1_000);
        s.insert_atomic(-7);
    }

    #[test]
    #[should_panic(expected = "which pre_allocate did not cover")]
    fn insert_atomic_past_the_pre_allocated_range_still_panics() {
        let mut s = IdSet::new();
        s.pre_allocate(1_000);
        s.insert_atomic(1_i64 << 40);
    }

    #[test]
    fn atomic_setters_need_pre_allocate_not_a_prior_insert() {
        // A chunk populated by `insert` alone is an array chunk, which the
        // atomic setters cannot write under `&self`. The documented contract
        // was always "requires pre_allocate"; this pins the consequence.
        let mut s = IdSet::new();
        s.insert(42);
        assert!(!s.try_insert_atomic(43));
        // pre_allocate promotes it in place and keeps its member.
        s.pre_allocate(100);
        assert!(s.get(42));
        assert!(s.try_insert_atomic(43));
        assert!(s.get(43));
    }

    #[test]
    fn insert_get_basic_ids_of_both_signs() {
        let mut s = IdSet::new();
        for id in [1, 100, 1_000_000, -1, -100, -1_000_000, 0] {
            s.insert(id);
        }
        for id in [1, 100, 1_000_000, -1, -100, -1_000_000, 0] {
            assert!(s.get(id), "{id}");
        }
        for id in [2, -2, 99, -99, 7, -7] {
            assert!(!s.get(id), "{id}");
        }
    }

    /// A negative ID and its magnitude twin are different members.
    #[test]
    fn magnitude_twins_are_distinct() {
        let mut s = IdSet::new();
        s.insert(-7);
        assert!(s.get(-7));
        assert!(!s.get(7));
        assert!(s.insert_if_new(7));
        assert!(!s.insert_if_new(-7));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn chunk_boundary_ids() {
        let mut s = IdSet::new();
        // A chunk covers 2^25 = 33_554_432 IDs.
        let boundary: i64 = 33_554_432;
        for id in [0, boundary - 1, boundary, boundary + 1] {
            s.insert(id);
            s.insert(-id - 1);
        }
        for id in [0, boundary - 1, boundary, boundary + 1] {
            assert!(s.get(id));
            assert!(s.get(-id - 1));
        }
        assert!(!s.get(boundary - 2));
        assert!(!s.get(boundary + 2));
        assert!(!s.get(-boundary + 1));
        assert!(!s.get(-boundary - 3));
    }

    #[test]
    fn extreme_ids_cost_no_allocation_proportional_to_the_id() {
        // One ID near i64::MAX used to resize the chunk directory to
        // ~2^38 entries and abort. It now lands in one overflow array chunk;
        // `i64::MIN` is key `i64::MAX` in the negative family.
        let mut s = IdSet::new();
        for id in [
            i64::MAX,
            i64::MAX - 1,
            1_i64 << 50,
            7,
            i64::MIN,
            i64::MIN + 1,
        ] {
            s.insert(id);
        }
        assert!(s.get(i64::MAX));
        assert!(s.get(i64::MAX - 1));
        assert!(s.get(1_i64 << 50));
        assert!(s.get(i64::MIN));
        assert!(s.get(i64::MIN + 1));
        assert!(!s.get(i64::MAX - 2));
        assert!(!s.get(i64::MIN + 2));
        assert_eq!(s.nonneg.chunks.len(), 1, "direct directory must not grow");
        assert!(s.neg.chunks.is_empty(), "nor the negative one");
        assert_eq!(s.nonneg.overflow.len(), 2);
        assert_eq!(s.neg.overflow.len(), 1);
        assert!(
            s.nonneg
                .overflow
                .values()
                .chain(s.neg.overflow.values())
                .all(|c| matches!(c, Chunk::Sparse(_))),
            "an isolated id must not cost a 4 MiB bit array"
        );
        assert_eq!(
            s.iter_numeric_from(i64::MIN).collect::<Vec<_>>(),
            vec![
                i64::MIN,
                i64::MIN + 1,
                7,
                1_i64 << 50,
                i64::MAX - 1,
                i64::MAX
            ]
        );
        assert!(s.any_in_numeric_range(i64::MAX, i64::MAX));
        assert!(s.any_in_numeric_range(i64::MIN, i64::MIN));
        let s = s.into_ranked();
        assert_eq!(s.total_count(), 6);
        assert_eq!(s.numeric_rank(i64::MAX), 5);
        assert_eq!(s.numeric_rank(i64::MIN), 0);
        assert_eq!(s.numeric_rank_if_set(i64::MAX - 1), Some(4));
        assert_eq!(s.numeric_rank_if_set(i64::MIN), Some(0));
        // Canonical: -(MIN+1), MIN (by magnitude), then 7, 2^50, MAX-1, MAX.
        assert_eq!(s.canonical_rank_if_set(i64::MIN + 1), Some(0));
        assert_eq!(s.canonical_rank_if_set(i64::MIN), Some(1));
        assert_eq!(s.canonical_rank_if_set(7), Some(2));
    }

    #[test]
    fn promotion_at_exactly_sparse_max_plus_one() {
        let mut s = IdSet::new();
        // Descending, so the append fast path never applies.
        for k in (0..SPARSE_MAX as i64).rev() {
            assert!(s.insert_if_new(k * 3));
        }
        assert!(matches!(s.nonneg.chunks[0], Some(Chunk::Sparse(_))));
        // A duplicate at the boundary must not promote.
        assert!(!s.insert_if_new(0));
        assert!(matches!(s.nonneg.chunks[0], Some(Chunk::Sparse(_))));
        // Member SPARSE_MAX + 1 promotes and keeps every earlier member.
        assert!(s.insert_if_new(1));
        assert!(matches!(s.nonneg.chunks[0], Some(Chunk::Dense(_))));
        for k in 0..SPARSE_MAX as i64 {
            assert!(s.get(k * 3));
        }
        assert!(s.get(1));
        assert!(!s.get(2));
        let s = s.into_ranked();
        assert_eq!(s.total_count(), SPARSE_MAX as u64 + 1);
        assert_eq!(s.numeric_rank(3), 2); // 0 and 1 are below 3
    }

    #[test]
    fn negative_promotion_at_exactly_sparse_max_plus_one() {
        let mut s = IdSet::new();
        for k in 1..=SPARSE_MAX as i64 {
            assert!(s.insert_if_new(-k));
        }
        assert!(matches!(s.neg.chunks[0], Some(Chunk::Sparse(_))));
        assert!(s.nonneg.chunks.is_empty(), "no non-negative allocation");
        assert!(s.insert_if_new(-(SPARSE_MAX as i64) - 1));
        assert!(matches!(s.neg.chunks[0], Some(Chunk::Dense(_))));
        let s = s.into_ranked();
        assert_eq!(s.total_count(), SPARSE_MAX as u64 + 1);
        assert_eq!(s.numeric_rank_if_set(-1), Some(SPARSE_MAX as u64));
        assert_eq!(s.canonical_rank_if_set(-1), Some(0));
    }

    #[test]
    fn ranks_are_total_over_every_i64() {
        // `rank` used to index `chunk_prefix[cid]` unchecked and panicked
        // past the last chunk or on a negative id.
        let mut s = IdSet::new();
        s.insert(5);
        s.insert(10);
        let s = s.into_ranked();
        assert_eq!(s.numeric_rank(-1), 0);
        assert_eq!(s.numeric_rank(i64::MIN), 0);
        assert_eq!(s.numeric_rank(0), 0);
        assert_eq!(s.numeric_rank(6), 1);
        assert_eq!(s.numeric_rank(1_000_000_000_000), 2);
        assert_eq!(s.numeric_rank(i64::MAX), 2);

        let empty = IdSet::new().into_ranked();
        assert_eq!(empty.total_count(), 0);
        assert_eq!(empty.numeric_rank(i64::MAX), 0);
        assert_eq!(empty.numeric_rank(i64::MIN), 0);
        assert_eq!(empty.numeric_rank(12_345), 0);
        assert_eq!(empty.numeric_rank_if_set(-1), None);
        assert_eq!(empty.canonical_rank_if_set(0), None);

        // Endpoints with both families: {-2, -1, 0, 3}.
        let mut s = IdSet::new();
        for id in [-2, -1, 0, 3] {
            s.insert(id);
        }
        let s = s.into_ranked();
        assert_eq!(s.numeric_rank(i64::MIN), 0);
        assert_eq!(s.numeric_rank(-3), 0);
        assert_eq!(s.numeric_rank(-2), 0);
        assert_eq!(s.numeric_rank(-1), 1);
        assert_eq!(s.numeric_rank(0), 2);
        assert_eq!(s.numeric_rank(1), 3);
        assert_eq!(s.numeric_rank(i64::MAX), 4);
        assert_eq!(
            [-2, -1, 0, 3].map(|id| s.canonical_rank_if_set(id)),
            [Some(2), Some(1), Some(0), Some(3)]
        );
    }

    #[test]
    fn any_in_numeric_range_hit_and_miss() {
        let mut s = IdSet::new();
        s.insert(50);
        s.insert(200);
        s.insert(-50);

        assert!(s.any_in_numeric_range(40, 60));
        assert!(s.any_in_numeric_range(50, 50));
        assert!(s.any_in_numeric_range(190, 210));
        assert!(s.any_in_numeric_range(-60, -40));
        assert!(s.any_in_numeric_range(-50, -50));

        // Exact, not word-granular.
        assert!(!s.any_in_numeric_range(56, 199));
        assert!(!s.any_in_numeric_range(0, 47));
        assert!(!s.any_in_numeric_range(208, 300));
        assert!(!s.any_in_numeric_range(-49, 47));
        assert!(!s.any_in_numeric_range(-200, -51));

        // Inverted range returns false
        assert!(!s.any_in_numeric_range(100, 10));
        assert!(!s.any_in_numeric_range(-10, -100));
    }

    #[test]
    fn any_in_numeric_range_on_a_bit_array_chunk_is_exact() {
        let mut s = IdSet::new();
        s.pre_allocate(0);
        s.insert(50);
        s.insert(200);
        assert!(matches!(s.nonneg.chunks[0], Some(Chunk::Dense(_))));
        assert!(s.any_in_numeric_range(40, 60));
        assert!(s.any_in_numeric_range(50, 50));
        assert!(!s.any_in_numeric_range(56, 199));
        assert!(!s.any_in_numeric_range(0, 47));
        assert!(!s.any_in_numeric_range(208, 300));
        assert!(!s.any_in_numeric_range(51, 63));
        assert!(s.any_in_numeric_range(0, i64::MAX));
        assert!(s.any_in_numeric_range(i64::MIN, i64::MAX));
    }

    /// A blob whose extrema straddle zero is asked of both families: the
    /// non-negative portion is never lost (the old unsigned cast wrapped
    /// `min` past `max`), and the negative portion is now asked too.
    #[test]
    fn any_in_numeric_range_straddling_zero_asks_both_families() {
        let mut s = IdSet::new();
        s.insert(50);
        assert!(s.any_in_numeric_range(-100, 60));
        assert!(!s.any_in_numeric_range(-100, 47));
        assert!(!s.any_in_numeric_range(-200, -50));
        assert!(!s.any_in_numeric_range(0, 0));
        s.insert(0);
        assert!(s.any_in_numeric_range(-5, 0));
        let mut s = IdSet::new();
        s.insert(-80);
        assert!(s.any_in_numeric_range(-100, 60));
        assert!(!s.any_in_numeric_range(-79, 60));
        assert!(!s.any_in_numeric_range(0, i64::MAX));
    }

    #[test]
    fn iter_canonical_is_id_order() {
        let mut s = IdSet::new();
        let ids = [9, -9, 0, 2, -2, i64::MIN, i64::MAX, -(1 << 44), 1 << 44];
        for id in ids {
            s.insert(id);
        }
        let mut want = ids.to_vec();
        want.sort_by_key(|&id| canonical_key(id));
        assert_eq!(s.iter_canonical().collect::<Vec<_>>(), want);
    }

    #[test]
    fn merge_two_sets() {
        let mut a = IdSet::new();
        a.insert(10);
        a.insert(20);
        a.insert(-10);

        let mut b = IdSet::new();
        b.insert(20);
        b.insert(30);
        b.insert(-30);

        a.merge(b);

        for id in [10, 20, 30, -10, -30] {
            assert!(a.get(id), "{id}");
        }
        assert!(!a.get(15));
        assert!(!a.get(-20));
    }

    #[test]
    fn rank_and_total_count() {
        let mut s = IdSet::new();
        s.insert(5);
        s.insert(10);
        s.insert(15);
        s.insert(20);
        let s = s.into_ranked();

        assert_eq!(s.total_count(), 4);
        // rank(id) = number of members before id
        assert_eq!(s.numeric_rank(5), 0);
        assert_eq!(s.numeric_rank(10), 1);
        assert_eq!(s.numeric_rank(15), 2);
        assert_eq!(s.numeric_rank(20), 3);
    }

    #[test]
    fn has_any_empty_vs_nonempty() {
        let empty = IdSet::new();
        assert!(!empty.has_any());

        let mut s = IdSet::new();
        s.insert(42);
        assert!(s.has_any());
        let mut s = IdSet::new();
        s.insert(-42);
        assert!(s.has_any());
    }

    /// The negative footprint is zero without a negative member, and prices
    /// a thinly spread negative family by what it holds: one ID in each of
    /// 1024 overflow chunks costs kilobytes, not 1024 bit arrays.
    #[test]
    fn negative_footprint_prices_what_the_family_holds() {
        let mut s = IdSet::new();
        assert_eq!(s.negative_footprint_bytes(true), 0);
        s.insert(0);
        s.insert(i64::MAX);
        assert_eq!(s.negative_footprint_bytes(true), 0);
        for i in 0..1024_i64 {
            s.insert(i64::MIN + (i << 30) * 64);
        }
        let ranked = s.negative_footprint_bytes(true);
        assert!(ranked > 0);
        assert!(ranked < 1 << 20, "{ranked} bytes for 1024 thin chunks");
        assert!(s.negative_footprint_bytes(false) < ranked);
        let mut dense = IdSet::new();
        for id in -2000..0 {
            dense.insert(id);
        }
        assert!(dense.negative_footprint_bytes(false) >= CHUNK_BYTES as u64);
    }

    /// An empty negative family costs nothing in the ranked bound, so a set
    /// of non-negative IDs is charged exactly what its bitmap alone is.
    #[test]
    fn ranked_bound_charges_nothing_for_an_empty_negative_family() {
        let mut s = IdSet::new();
        s.insert(7);
        assert_eq!(
            s.ranked_bytes_upper_bound(),
            s.nonneg.ranked_bytes_upper_bound()
        );
        s.insert(-7);
        assert!(s.ranked_bytes_upper_bound() > s.nonneg.ranked_bytes_upper_bound());
        let ranked = s.into_ranked();
        assert!(ranked.neg_rank.is_some());
        assert!(IdSet::new().into_ranked().neg_rank.is_none());
    }

    /// Chunk keys separate the families, so `0` and `-1` (both key 0 in
    /// their family) are not grouped.
    #[test]
    fn chunk_of_separates_families() {
        assert_ne!(IdSet::chunk_of(0), IdSet::chunk_of(-1));
        assert_eq!(IdSet::chunk_of(0), IdSet::chunk_of(1));
        assert_eq!(IdSet::chunk_of(-1), IdSet::chunk_of(-2));
        assert_ne!(IdSet::chunk_of(1 << 25), IdSet::chunk_of(0));
        assert_ne!(IdSet::chunk_of(i64::MIN), IdSet::chunk_of(i64::MAX));
    }

    /// `len` and `is_empty` count members, not allocation: a pre-allocated
    /// range is allocated (`has_any`) but empty. Covers a bit-array chunk, an
    /// array chunk and an overflow-map chunk above the direct directory, in
    /// both families.
    #[test]
    fn len_and_is_empty_count_members_not_chunks() {
        let mut s = IdSet::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);

        s.pre_allocate(3 << ID_SHIFT);
        assert!(s.has_any(), "pre_allocate allocates chunks");
        assert!(s.is_empty(), "but sets no member");
        assert_eq!(s.len(), 0);

        let overflow_id = i64::try_from(DIRECT_CHUNKS << ID_SHIFT).expect("fits") + 5;
        let array_id = 10_i64 << ID_SHIFT;
        for id in [7, 7, 1 << ID_SHIFT, array_id, overflow_id, -overflow_id, -7] {
            s.insert(id);
            assert!(!s.is_empty());
        }
        assert_eq!(s.len(), 6, "the repeated 7 counts once");
    }

    #[test]
    fn rank_if_set_parity_with_numeric_rank() {
        let mut s = IdSet::new();
        let ids = [
            0,
            1,
            5,
            10,
            15,
            20,
            100,
            1_000_000,
            33_554_432,
            -1,
            -33_554_433,
        ];
        for &id in &ids {
            s.insert(id);
        }
        let s = s.into_ranked();

        for &id in &ids {
            let expected = s.numeric_rank(id);
            assert_eq!(
                s.numeric_rank_if_set(id),
                Some(expected),
                "rank_if_set({id})"
            );
        }

        let unset = [2, 3, 7, 11, 50, 999, 33_554_431, -2, -33_554_432];
        for &id in &unset {
            assert!(!s.get(id), "precondition: {id} should not be set");
            assert_eq!(
                s.numeric_rank_if_set(id),
                None,
                "rank_if_set({id}) for unset"
            );
            assert_eq!(s.canonical_rank_if_set(id), None);
        }
    }

    #[test]
    fn re_ranking_after_mutation_sees_the_new_members() {
        let mut s = IdSet::new();
        s.insert(10);
        let ranked = s.into_ranked();
        assert_eq!(ranked.numeric_rank_if_set(10), Some(0));
        let mut s = ranked.into_inner();
        s.insert(5);
        let mut borrowed = IdSet::new();
        borrowed.insert(3);
        borrowed.insert(-3);
        s.merge_from(&borrowed);
        let mut owned = IdSet::new();
        owned.insert(1);
        s.merge(owned);
        let ranked = s.into_ranked();
        assert_eq!(ranked.numeric_rank_if_set(10), Some(4));
        assert_eq!(ranked.total_count(), 5);
    }

    #[test]
    fn atomic_storage_is_u64_aligned_and_parallel_fill_survives_rank_build() {
        let mut s = IdSet::new();
        s.pre_allocate(10_000);
        let Some(Chunk::Dense(words)) = s.nonneg.chunks[0].as_ref() else {
            panic!("pre_allocate must create a bit-array chunk");
        };
        assert_eq!((words.as_ptr() as usize) % std::mem::align_of::<u64>(), 0);
        std::thread::scope(|scope| {
            for offset in 0..8 {
                let shared = &s;
                scope.spawn(move || {
                    for id in (offset..10_000).step_by(8) {
                        shared.insert_atomic(id);
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
    /// the direct/overflow boundary, the extremes and zero, in both signs.
    fn interesting_id(rng: &mut Rng) -> i64 {
        let chunk = 1_i64 << ID_SHIFT;
        let overflow_base = (DIRECT_CHUNKS as i64) << ID_SHIFT;
        let jitter = rng.below(64) as i64;
        let magnitude = match rng.below(8) {
            0 => rng.below(4_000) as i64,
            1 => chunk - 32 + jitter,
            2 => 3 * chunk - 32 + jitter,
            3 => overflow_base - 32 + jitter,
            4 => overflow_base + 7 * chunk + jitter,
            5 => overflow_base + 3 * chunk - 32 + jitter,
            6 => i64::MAX - jitter,
            _ => (rng.below(1 << 20) as i64) * 977,
        };
        // `!magnitude` reaches `i64::MIN` from `i64::MAX` without overflow.
        if rng.below(2) == 0 {
            magnitude
        } else {
            !magnitude
        }
    }

    /// Check `s` against `reference`, then hand it back unranked so the
    /// caller can keep mutating it.
    fn assert_matches_reference(s: IdSet, reference: &BTreeSet<i64>) -> IdSet {
        let numeric: Vec<i64> = reference.iter().copied().collect();
        let mut canonical = numeric.clone();
        canonical.sort_by_key(|&id| canonical_key(id));
        assert_eq!(
            s.iter_numeric_from(i64::MIN).collect::<Vec<_>>(),
            numeric,
            "numeric order and contents"
        );
        assert_eq!(
            s.iter_canonical().collect::<Vec<_>>(),
            canonical,
            "canonical order and contents"
        );
        assert_eq!(s.len(), reference.len() as u64);
        assert_eq!(s.is_empty(), reference.is_empty());
        let s = s.into_ranked();
        assert_eq!(s.total_count(), reference.len() as u64);

        let below_of = |p: i64| numeric.partition_point(|&x| x < p) as u64;
        let mut rng = Rng(0xDEAD_BEEF);
        let mut probes: Vec<i64> = numeric.clone();
        probes.extend((0..400).map(|_| interesting_id(&mut rng)));
        probes.extend([i64::MIN, -1, 0, 1, i64::MAX]);
        for &p in &probes {
            let below = below_of(p);
            let present = reference.contains(&p);
            assert_eq!(s.get(p), present, "get({p})");
            assert_eq!(s.numeric_rank(p), below, "numeric_rank({p})");
            assert_eq!(
                s.numeric_rank_if_set(p),
                present.then_some(below),
                "numeric_rank_if_set({p})"
            );
            let canonical_rank =
                canonical.partition_point(|&x| canonical_key(x) < canonical_key(p));
            assert_eq!(
                s.canonical_rank_if_set(p),
                present.then_some(canonical_rank as u64),
                "canonical_rank_if_set({p})"
            );
            let from: Vec<i64> = s.iter_numeric_from(p).take(3).collect();
            let want: Vec<i64> = reference.range(p..).take(3).copied().collect();
            assert_eq!(from, want, "iter_numeric_from({p})");
        }
        let s = s.into_inner();
        for _ in 0..400 {
            let a = interesting_id(&mut rng);
            let b = interesting_id(&mut rng);
            let (lo, hi) = (a.min(b), a.max(b));
            let hit = reference.range(lo..=hi).next().is_some();
            assert_eq!(
                s.any_in_numeric_range(lo, hi),
                hit,
                "any_in_numeric_range({lo}, {hi})"
            );
        }
        s
    }

    /// Push one direct and one overflow chunk of each family over the
    /// promotion threshold.
    fn fill_past_promotion(s: &mut IdSet, reference: &mut BTreeSet<i64>) {
        let overflow_base = (DIRECT_CHUNKS as i64) << ID_SHIFT;
        for k in 0..(SPARSE_MAX as i64 + 10) {
            for id in [
                5 * k,
                overflow_base + 11 * k,
                !(7 * k),
                !(overflow_base + 13 * k),
            ] {
                s.insert(id);
                reference.insert(id);
            }
        }
    }

    /// A second set of random ids, mirrored into the reference.
    fn random_other(rng: &mut Rng, reference: &mut BTreeSet<i64>) -> IdSet {
        let mut other = IdSet::new();
        for _ in 0..1_500 {
            let id = interesting_id(rng);
            other.insert(id);
            reference.insert(id);
        }
        other
    }

    #[test]
    fn differential_against_btreeset() {
        let mut rng = Rng(0x1234_5678_9ABC_DEF1);
        let mut s = IdSet::new();
        let mut reference = BTreeSet::new();

        for round in 0..6 {
            for _ in 0..3_000 {
                let id = interesting_id(&mut rng);
                let fresh = s.insert_if_new(id);
                assert_eq!(fresh, reference.insert(id), "insert_if_new({id})");
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
            dup.insert(id);
        }
        s.merge_from(&dup);
        s.merge(dup);
        assert_matches_reference(s, &reference);
    }

    #[test]
    fn sparse_into_dense_merges_both_directions() {
        let mut dense = IdSet::new();
        for k in 0..(SPARSE_MAX as i64 + 1) {
            dense.insert(2 * k);
        }
        let mut sparse = IdSet::new();
        sparse.insert(1);
        sparse.insert(3);

        // Array destination, bit-array source, owned: takes the allocation.
        let mut a = IdSet::new();
        a.insert(1);
        a.insert(3);
        let mut dense_copy = IdSet::new();
        dense_copy.merge_from(&dense);
        a.merge(dense_copy);
        assert!(matches!(a.nonneg.chunks[0], Some(Chunk::Dense(_))));
        // Bit-array destination, array source.
        dense.merge_from(&sparse);
        assert_eq!(
            a.iter_canonical().collect::<Vec<_>>(),
            dense.iter_canonical().collect::<Vec<_>>()
        );
        // Two array chunks whose deduplicated union exceeds SPARSE_MAX.
        let mut x = IdSet::new();
        let mut y = IdSet::new();
        for k in 0..SPARSE_MAX as i64 {
            x.insert(2 * k);
        }
        for k in 0..(SPARSE_MAX as i64 - 1) {
            y.insert(2 * k + 1);
        }
        y.insert(0); // duplicate of x's first member; y stays an array chunk
        assert!(matches!(y.nonneg.chunks[0], Some(Chunk::Sparse(_))));
        x.merge_from(&y);
        assert!(matches!(x.nonneg.chunks[0], Some(Chunk::Dense(_))));
        assert_eq!(x.into_ranked().total_count(), 2 * SPARSE_MAX as u64 - 1);
    }
}
