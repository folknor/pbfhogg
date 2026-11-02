//! The run's tag counts: one table of distinct (key, value) pairs split into
//! shards by pair hash, each behind its own mutex, so every decode worker
//! merges its own blob's counts and no single thread serialises the merge.
//!
//! A shard stores its strings in a [`ChunkArena`] (fixed-size byte chunks,
//! so growth never copies and never reserves double) and each distinct pair
//! as one fixed-size [`Entry`] in a `hashbrown::HashTable`: no allocation per
//! pair, no `String` header per value, nothing to free per pair when the run
//! ends. Keys are interned per shard: a key's bytes are stored once in each
//! shard holding a pair of it, and an entry names its key by slot. The slot is
//! only a storage reference; pairs are hashed and compared by their bytes, so
//! equal keys in different shards (with unrelated slots) and equal strings at
//! different string table indices count as one text.

use std::hash::BuildHasher as _;
use std::sync::{Mutex, MutexGuard};

use hashbrown::HashTable;
use rustc_hash::FxBuildHasher;

use super::TagCount;
use crate::error::{Error, Result};
use crate::tunables::{TAGS_COUNT_ARENA_CHUNK_BYTES, TAGS_COUNT_SHARD_BITS};

/// The one hash of a (key, value) pair, for shard selection, lookup and the
/// pair tables' rehash on growth. Hashing the pair as a tuple of slices
/// writes each slice's length before its bytes, so `("ab", "c")` and
/// `("a", "bc")` hash as different pairs.
pub(super) fn pair_hash(key: &[u8], value: &[u8]) -> u64 {
    FxBuildHasher.hash_one((key, value))
}

/// The hash of a key alone, for a shard's key index.
fn key_hash(key: &[u8]) -> u64 {
    FxBuildHasher.hash_one(key)
}

fn too_long() -> Error {
    Error::internal("inspect tags: a tag string too long for the count table")
}

/// Where a string lies in a [`ChunkArena`]: chunk number, offset, length.
/// A zero-length string is a valid empty range, no sentinel.
#[derive(Clone, Copy)]
struct Place {
    chunk: u32,
    offset: u32,
    len: u32,
}

/// Byte storage in chunks of `TAGS_COUNT_ARENA_CHUNK_BYTES`, appended to and
/// never moved: a string goes in the current chunk when it fits, otherwise
/// into a fresh one; a string longer than a chunk gets a chunk of its own and
/// the current chunk stays current, so an outsized string between small ones
/// abandons no free space. Waste is each chunk's unused tail (under the
/// length of the string that did not fit) and the current chunk's unfilled
/// rest, not the up-to-half a doubling vector reserves, and growth copies
/// nothing.
#[derive(Default)]
struct ChunkArena {
    chunks: Vec<Vec<u8>>,
    /// The chunk small strings are appended to, once there is one.
    current: Option<usize>,
}

impl ChunkArena {
    fn push(&mut self, bytes: &[u8]) -> Result<Place> {
        let len = u32::try_from(bytes.len()).map_err(|_| too_long())?;
        let fits = |chunk: &Vec<u8>| chunk.capacity() - chunk.len() >= bytes.len();
        let index = if bytes.len() > TAGS_COUNT_ARENA_CHUNK_BYTES {
            self.chunks.push(Vec::with_capacity(bytes.len()));
            self.chunks.len() - 1
        } else {
            match self.current {
                Some(i) if fits(&self.chunks[i]) => i,
                _ => {
                    self.chunks
                        .push(Vec::with_capacity(TAGS_COUNT_ARENA_CHUNK_BYTES));
                    self.current = Some(self.chunks.len() - 1);
                    self.chunks.len() - 1
                }
            }
        };
        let chunk = &mut self.chunks[index];
        let offset = u32::try_from(chunk.len()).map_err(|_| too_long())?;
        chunk.extend_from_slice(bytes);
        Ok(Place {
            chunk: u32::try_from(index).map_err(|_| too_long())?,
            offset,
            len,
        })
    }

    fn get(&self, place: Place) -> &[u8] {
        let start = place.offset as usize;
        &self.chunks[place.chunk as usize][start..start + place.len as usize]
    }

    fn len(&self) -> usize {
        self.chunks.iter().map(Vec::len).sum()
    }

    fn capacity(&self) -> usize {
        self.chunks.iter().map(Vec::capacity).sum()
    }
}

/// One distinct pair of a shard: its key's slot, where its value lies, and
/// its count. 24 bytes.
#[derive(Clone, Copy)]
struct Entry {
    key: u32,
    value: Place,
    count: u64,
}

#[derive(Default)]
pub(super) struct Shard {
    arena: ChunkArena,
    /// Every interned key's place, by slot.
    keys: Vec<Place>,
    /// Slots of `keys`, keyed by the bytes they address.
    key_index: HashTable<u32>,
    entries: HashTable<Entry>,
}

impl Shard {
    fn key(&self, slot: u32) -> &[u8] {
        self.arena.get(self.keys[slot as usize])
    }

    /// The slot of `key`, interning it on first sight.
    fn intern(&mut self, key: &[u8]) -> Result<u32> {
        let hash = key_hash(key);
        let Shard {
            arena,
            keys,
            key_index,
            ..
        } = self;
        if let Some(&slot) = key_index.find(hash, |&s| arena.get(keys[s as usize]) == key) {
            return Ok(slot);
        }
        let slot = u32::try_from(keys.len()).map_err(|_| too_long())?;
        keys.push(arena.push(key)?);
        let (arena, keys) = (&*arena, &*keys);
        key_index.insert_unique(hash, slot, |&s| key_hash(arena.get(keys[s as usize])));
        Ok(slot)
    }

    /// Add `count` to the pair (`key`, `value`), whose [`pair_hash`] is
    /// `hash`, entering it on first sight. One probe of the pair table finds
    /// an existing pair, comparing key and value bytes; only a new pair looks
    /// its key up in the key index.
    pub(super) fn add(&mut self, hash: u64, key: &str, value: &str, count: u64) -> Result<()> {
        let (key, value) = (key.as_bytes(), value.as_bytes());
        let found = {
            let Shard {
                arena,
                keys,
                entries,
                ..
            } = self;
            entries.find_mut(hash, |e| {
                arena.get(e.value) == value && arena.get(keys[e.key as usize]) == key
            })
        };
        if let Some(entry) = found {
            entry.count += count;
            return Ok(());
        }
        // Key slot and value bytes exist before the entry does, so a rehash
        // on growth can resolve every entry, this one included.
        let entry = Entry {
            key: self.intern(key)?,
            value: self.arena.push(value)?,
            count,
        };
        let Shard {
            arena,
            keys,
            entries,
            ..
        } = self;
        let (arena, keys) = (&*arena, &*keys);
        entries.insert_unique(hash, entry, |e| {
            pair_hash(arena.get(keys[e.key as usize]), arena.get(e.value))
        });
        Ok(())
    }
}

/// The shards, `1 << TAGS_COUNT_SHARD_BITS` of them.
pub(super) struct CountTable {
    shards: Vec<Mutex<Shard>>,
}

/// A poisoned shard: a worker panicked while holding it, perhaps inside an
/// insertion or a table growth. Its contents are not trusted, so the run
/// fails instead of reading them (the panic itself also propagates when the
/// pool's scope joins).
fn poisoned() -> Error {
    Error::internal("inspect tags: a count shard was poisoned by a panicking worker")
}

impl CountTable {
    pub(super) fn new() -> Self {
        CountTable {
            shards: (0..1_usize << TAGS_COUNT_SHARD_BITS)
                .map(|_| Mutex::new(Shard::default()))
                .collect(),
        }
    }

    /// How many shards the table has.
    pub(super) fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// The shard of a pair with hash `hash`: the top bits of a multiplicative
    /// mix of the whole hash (the 64-bit golden-ratio constant, odd and with
    /// its bits spread). A plain slice of `hash` would pin, within each shard,
    /// the bits hashbrown reads for the bucket index (low) or the control
    /// byte (top seven).
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the shift leaves TAGS_COUNT_SHARD_BITS bits, which index the shard vector"
    )]
    pub(super) fn shard_of(hash: u64) -> usize {
        (hash.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - TAGS_COUNT_SHARD_BITS)) as usize
    }

    /// Shard `index`, locked. The caller holds one shard at a time, so no
    /// lock order is needed.
    pub(super) fn lock(&self, index: usize) -> Result<MutexGuard<'_, Shard>> {
        self.shards
            .get(index)
            .ok_or_else(|| Error::internal("inspect tags: shard index out of range"))?
            .lock()
            .map_err(|_| poisoned())
    }

    /// Every pair whose count `keep` admits, as owned rows, unsorted. Retained
    /// rows are counted before the result is reserved, so a threshold that
    /// keeps few rows reserves few; each shard is freed once read, so the
    /// rows never coexist with the whole table. Cancellation is checked per
    /// shard.
    pub(super) fn into_counts(
        self,
        keep: impl Fn(u64) -> bool,
        cancel: &crate::host::Recorder,
    ) -> Result<Vec<TagCount>> {
        let shards = self
            .shards
            .into_iter()
            .map(|shard| shard.into_inner().map_err(|_| poisoned()))
            .collect::<Result<Vec<Shard>>>()?;
        // What the table holds at its peak, for attributing the run's memory:
        // distinct pairs and interned keys (one per shard holding the key),
        // arena bytes stored and reserved, and the two hash tables' own
        // allocations.
        let sum = |f: fn(&Shard) -> usize| shards.iter().map(f).sum::<usize>();
        cancel.counter("tags_count_distinct_pairs", sum(|s| s.entries.len()));
        cancel.counter("tags_count_interned_keys", sum(|s| s.keys.len()));
        cancel.counter("tags_count_arena_len_bytes", sum(|s| s.arena.len()));
        cancel.counter("tags_count_arena_bytes", sum(|s| s.arena.capacity()));
        cancel.counter(
            "tags_count_table_bytes",
            sum(|s| {
                s.entries.allocation_size()
                    + s.key_index.allocation_size()
                    + s.keys.capacity() * size_of::<Place>()
            }),
        );
        let mut retained = 0_usize;
        for shard in &shards {
            cancel.check()?;
            retained += shard.entries.iter().filter(|e| keep(e.count)).count();
        }
        let text = |bytes: &[u8]| -> Result<String> {
            std::str::from_utf8(bytes)
                .map(str::to_owned)
                .map_err(|_| Error::internal("inspect tags: a counted string is not UTF-8"))
        };
        let mut rows = Vec::with_capacity(retained);
        for shard in shards {
            cancel.check()?;
            for &entry in shard.entries.iter().filter(|e| keep(e.count)) {
                rows.push(TagCount {
                    key: text(shard.key(entry.key))?,
                    value: text(shard.arena.get(entry.value))?,
                    count: entry.count,
                });
            }
        }
        Ok(rows)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{ChunkArena, Shard, pair_hash};
    use crate::tunables::TAGS_COUNT_ARENA_CHUNK_BYTES;

    /// An outsized string gets a chunk of its own and the current chunk
    /// keeps taking small strings; every string reads back as stored, the
    /// empty one included.
    #[test]
    fn arena_keeps_its_current_chunk_across_an_outsized_string() {
        let mut arena = ChunkArena::default();
        let big = vec![b'x'; TAGS_COUNT_ARENA_CHUNK_BYTES + 1];
        let a = arena.push(b"small").unwrap();
        let b = arena.push(&big).unwrap();
        let c = arena.push(b"").unwrap();
        let d = arena.push(b"next").unwrap();
        assert_eq!(arena.chunks.len(), 2);
        assert_eq!((a.chunk, c.chunk, d.chunk), (0, 0, 0));
        assert_eq!(b.chunk, 1);
        assert_eq!(arena.get(a), b"small");
        assert_eq!(arena.get(b), big.as_slice());
        assert_eq!(arena.get(c), b"");
        assert_eq!(arena.get(d), b"next");
        // Filling the current chunk opens the next one.
        let fill = vec![b'y'; TAGS_COUNT_ARENA_CHUNK_BYTES - 9];
        let e = arena.push(&fill).unwrap();
        assert_eq!(e.chunk, 0);
        let f = arena.push(b"spill").unwrap();
        assert_eq!(f.chunk, 2);
        assert_eq!(arena.get(e), fill.as_slice());
    }

    /// A shard counts by text across growth: many keys and values, each pair
    /// added twice, all found again, and a key's bytes interned once.
    #[test]
    fn shard_counts_by_text_across_growth() {
        let mut shard = Shard::default();
        let pairs: Vec<(String, String)> = (0..5_000)
            .map(|i| (format!("k{}", i % 7), format!("v{i}")))
            .collect();
        for _ in 0..2 {
            for (k, v) in &pairs {
                shard
                    .add(pair_hash(k.as_bytes(), v.as_bytes()), k, v, 1)
                    .unwrap();
            }
        }
        assert_eq!(shard.entries.len(), pairs.len());
        assert_eq!(shard.keys.len(), 7);
        assert!(shard.entries.iter().all(|e| e.count == 2));
        // ("ab", "c") and ("a", "bc") are different pairs.
        shard.add(pair_hash(b"ab", b"c"), "ab", "c", 1).unwrap();
        shard.add(pair_hash(b"a", b"bc"), "a", "bc", 1).unwrap();
        assert_eq!(shard.entries.len(), pairs.len() + 2);
    }
}
