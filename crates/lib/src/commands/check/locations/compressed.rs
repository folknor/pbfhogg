//! A partition of immutable rank-indexed node coordinates, with separate
//! atomic state for the way workers. Each block packs unsigned latitude
//! and longitude deltas from its minima. A block with special node states
//! or no packing savings escapes to the original words. Absent ranks are
//! represented by a mask and take no space in either coordinate payload.
//! In agreement mode only, absent ranks also have a dense array of exact
//! atomic slots for the first inline coordinate or the disagreement sentinel.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::tunables::CHECK_LOCATIONS_CHUNK_SLOTS;

use super::store::{DISAGREE, Final, SlotState, bit, bitset, decode, join_agreement};

/// Ranks per encoded block. Each rank uses one bit of the `u32` absence
/// mask, and the encoder's scratch array and rank lookup use this width.
const BLOCK_SLOTS: usize = 32;

const _: () = assert!(
    CHECK_LOCATIONS_CHUNK_SLOTS.is_power_of_two()
        && CHECK_LOCATIONS_CHUNK_SLOTS < u16::MAX as usize
);
const _: () = assert!(BLOCK_SLOTS.is_power_of_two() && BLOCK_SLOTS <= u32::BITS as usize);

const WORD_BITS: usize = u64::BITS as usize;
const RAW: u8 = 64;

struct Block {
    base_lat: i32,
    base_lon: i32,
    offset: u16,
    lat_bits: u8,
    /// Sum of the axis widths, or RAW for the exact escape.
    width: u8,
    absent: u32,
}

impl Block {
    #[allow(clippy::cast_possible_truncation)]
    fn encode(words: &[AtomicU64], data: &mut Vec<u64>, conflicting: &mut u64) -> Self {
        // Each previous block used at most one word per rank, and the
        // chunk maximum is strictly below u16::MAX.
        let offset = data.len() as u16;
        let mut coords = [(0, 0); BLOCK_SLOTS];
        let mut count = 0;
        let mut absent = 0;
        let mut special = false;
        let (mut min_lat, mut min_lon) = (i32::MAX, i32::MAX);
        let (mut max_lat, mut max_lon) = (i32::MIN, i32::MIN);
        for (i, word) in words.iter().enumerate() {
            match decode(word.load(Relaxed)) {
                SlotState::Empty => absent |= 1_u32 << i,
                SlotState::Coord { lat, lon } => {
                    coords[count] = (lat, lon);
                    count += 1;
                    min_lat = min_lat.min(lat);
                    min_lon = min_lon.min(lon);
                    max_lat = max_lat.max(lat);
                    max_lon = max_lon.max(lon);
                }
                state => {
                    special = true;
                    *conflicting += u64::from(state == SlotState::Conflict);
                }
            }
        }
        let mut block = Self {
            base_lat: min_lat,
            base_lon: min_lon,
            offset,
            lat_bits: 0,
            width: 0,
            absent,
        };
        if count == 0 && !special {
            return block;
        }
        let lat_bits = u32::BITS - max_lat.abs_diff(min_lat).leading_zeros();
        let lon_bits = u32::BITS - max_lon.abs_diff(min_lon).leading_zeros();
        let width = lat_bits + lon_bits;
        if special || (count * width as usize).div_ceil(WORD_BITS) >= count {
            block.width = RAW;
            data.extend(
                words
                    .iter()
                    .map(|word| word.load(Relaxed))
                    .filter(|&word| word != 0),
            );
        } else {
            block.lat_bits = lat_bits as u8;
            block.width = width as u8;
            block.pack(&coords[..count], data);
        }
        block
    }

    fn pack(&self, coords: &[(i32, i32)], data: &mut Vec<u64>) {
        let width = usize::from(self.width);
        if width == 0 {
            return;
        }
        let mut pending = 0;
        let mut used = 0;
        for &(lat, lon) in coords {
            let delta = u64::from(lat.abs_diff(self.base_lat))
                | (u64::from(lon.abs_diff(self.base_lon)) << self.lat_bits);
            pending |= delta << used;
            used += width;
            if used >= WORD_BITS {
                data.push(pending);
                used -= WORD_BITS;
                pending = delta >> (width - used);
            }
        }
        if used > 0 {
            data.push(pending);
        }
    }

    #[inline]
    fn preceding_absent(&self, i: usize) -> usize {
        (self.absent & ((1_u32 << i) - 1)).count_ones() as usize
    }

    #[inline]
    fn is_absent(&self, i: usize) -> bool {
        self.absent & (1_u32 << i) != 0
    }

    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    fn node_state(&self, i: usize, data: &[u64]) -> Option<SlotState> {
        if self.is_absent(i) {
            return None;
        }
        let index = i - self.preceding_absent(i);
        let offset = usize::from(self.offset);
        let width = usize::from(self.width);
        if self.width == RAW {
            return Some(decode(data[offset + index]));
        }
        let mut delta = 0;
        if width != 0 {
            let bit = index * width;
            let start = offset + bit / WORD_BITS;
            let shift = bit % WORD_BITS;
            delta = data[start] >> shift;
            if shift + width > WORD_BITS {
                delta |= data[start + 1] << (WORD_BITS - shift);
            }
            delta &= (1_u64 << width) - 1;
        }
        let lat = (delta & ((1_u64 << self.lat_bits) - 1)) as u32;
        let lon = (delta >> self.lat_bits) as u32;
        Some(SlotState::Coord {
            lat: self.base_lat.wrapping_add_unsigned(lat),
            lon: self.base_lon.wrapping_add_unsigned(lon),
        })
    }
}

pub(super) struct Chunk {
    blocks: Box<[Block]>,
    data: Box<[u64]>,
    /// Offsets into agreement, one per block when the array is needed.
    absent_offsets: Box<[u16]>,
    agreement: Box<[AtomicU64]>,
    mismatch: Box<[AtomicU64]>,
    len: usize,
    present: u64,
    conflicting: u64,
}

impl Chunk {
    /// Select the granularity at which the planner can cut a partition. A
    /// full chunk fits even in the raw case, except for the one-slot floor
    /// when a caller explicitly sets a very small budget.
    pub(super) fn slots_for_budget(budget: u64, agreement: bool) -> usize {
        let mut len = CHECK_LOCATIONS_CHUNK_SLOTS;
        while len > 1 && Self::max_bytes(len, agreement) > budget {
            len /= 2;
        }
        len
    }

    fn max_bytes(len: usize, agreement: bool) -> u64 {
        let blocks = len.div_ceil(BLOCK_SLOTS);
        // Each rank takes at most one coordinate word across both payloads.
        let bytes = len * size_of::<u64>()
            + blocks * (size_of::<Block>() + usize::from(agreement) * size_of::<u16>())
            + len.div_ceil(WORD_BITS) * size_of::<AtomicU64>();
        bytes as u64 + Store::DIRECTORY_BYTES_PER_CHUNK
    }

    pub(super) fn encode(words: &[AtomicU64], agreement: bool) -> Self {
        assert!(!words.is_empty() && words.len() <= CHECK_LOCATIONS_CHUNK_SLOTS);
        let mut data = Vec::new();
        let mut conflicting = 0;
        let mut absent = 0;
        let blocks: Box<[_]> = words
            .chunks(BLOCK_SLOTS)
            .map(|words| {
                let block = Block::encode(words, &mut data, &mut conflicting);
                absent += block.absent.count_ones() as usize;
                block
            })
            .collect();
        let (absent_offsets, agreement) = if agreement && absent != 0 {
            let mut offset = 0;
            let offsets = blocks
                .iter()
                .map(|block| {
                    let start = offset;
                    // At most one agreement word per rank, so the offset
                    // fits even in a chunk whose ranks are all absent.
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        offset += block.absent.count_ones() as u16;
                    }
                    start
                })
                .collect();
            let slots = std::iter::repeat_with(|| AtomicU64::new(0))
                .take(absent)
                .collect();
            (offsets, slots)
        } else {
            (Box::default(), Box::default())
        };
        Self {
            blocks,
            data: data.into_boxed_slice(),
            absent_offsets,
            agreement,
            mismatch: bitset(words.len()).into_boxed_slice(),
            len: words.len(),
            present: (words.len() - absent) as u64,
            conflicting,
        }
    }

    fn bytes(&self) -> u64 {
        (size_of_val(&*self.blocks)
            + size_of_val(&*self.data)
            + size_of_val(&*self.absent_offsets)
            + size_of_val(&*self.agreement)
            + size_of_val(&*self.mismatch)) as u64
            + Store::DIRECTORY_BYTES_PER_CHUNK
    }

    #[inline]
    fn node_state(&self, i: usize) -> Option<SlotState> {
        self.blocks[i / BLOCK_SLOTS].node_state(i % BLOCK_SLOTS, &self.data)
    }

    fn agreement_slot(&self, i: usize) -> &AtomicU64 {
        let block = i / BLOCK_SLOTS;
        &self.agreement[usize::from(self.absent_offsets[block])
            + self.blocks[block].preceding_absent(i % BLOCK_SLOTS)]
    }

    fn reportable(&self, i: usize) -> Option<SlotState> {
        let block = &self.blocks[i / BLOCK_SLOTS];
        let local = i % BLOCK_SLOTS;
        if block.is_absent(local) {
            if !self.agreement.is_empty() && self.agreement_slot(i).load(Relaxed) == DISAGREE {
                return Some(SlotState::Disagree);
            }
        } else if block.width == RAW
            && block.node_state(local, &self.data) == Some(SlotState::Conflict)
        {
            return Some(SlotState::Conflict);
        }
        None
    }
}

pub(super) struct Store {
    chunks: Vec<Chunk>,
    chunk_shift: u32,
    chunk_mask: usize,
    len: usize,
    bytes: u64,
}

impl Store {
    // The directory doubles from one entry, so at most two entries per
    // retained chunk are charged, in addition to the exact boxed slices.
    const DIRECTORY_BYTES_PER_CHUNK: u64 = 2 * size_of::<Chunk>() as u64;

    pub(super) fn new(chunk_slots: usize) -> Self {
        assert!(chunk_slots.is_power_of_two());
        Self {
            chunks: Vec::new(),
            chunk_shift: chunk_slots.trailing_zeros(),
            chunk_mask: chunk_slots - 1,
            len: 0,
            bytes: 0,
        }
    }

    pub(super) fn fits(&self, chunk: &Chunk, budget: u64) -> bool {
        self.chunks.is_empty() || self.bytes.saturating_add(chunk.bytes()) <= budget
    }

    pub(super) fn push(&mut self, chunk: Chunk) -> crate::Result<()> {
        assert_eq!(
            self.len & self.chunk_mask,
            0,
            "only the last chunk can be short"
        );
        assert!(chunk.len != 0 && chunk.len <= self.chunk_mask + 1);
        self.len = self.len.checked_add(chunk.len).ok_or_else(|| {
            crate::Error::internal("location check partition does not fit in memory")
        })?;
        self.bytes += chunk.bytes();
        if self.chunks.len() == self.chunks.capacity() {
            self.chunks.reserve_exact(self.chunks.len().max(1));
        }
        self.chunks.push(chunk);
        assert!(self.chunks.capacity() <= self.chunks.len() * 2);
        Ok(())
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    pub(super) fn budgeted_bytes(&self) -> u64 {
        self.bytes
    }

    #[inline]
    fn locate(&self, i: usize) -> (&Chunk, usize) {
        (&self.chunks[i >> self.chunk_shift], i & self.chunk_mask)
    }

    #[inline]
    pub(super) fn node_state(&self, i: usize) -> Option<SlotState> {
        let (chunk, i) = self.locate(i);
        chunk.node_state(i)
    }

    pub(super) fn mark_mismatch(&self, i: usize) {
        let (chunk, i) = self.locate(i);
        let (word, mask) = bit(&chunk.mismatch, i);
        word.fetch_or(mask, Relaxed);
    }

    pub(super) fn agree(&self, i: usize, lat: i32, lon: i32) {
        let (chunk, i) = self.locate(i);
        join_agreement(chunk.agreement_slot(i), lat, lon);
    }

    /// Read the final states after every way worker has joined.
    pub(super) fn finish(&self) -> Final {
        let mut out = Final::default();
        for chunk in &self.chunks {
            out.present += chunk.present;
            out.conflicting += chunk.conflicting;
            out.mismatched += chunk
                .mismatch
                .iter()
                .map(|word| u64::from(word.load(Relaxed).count_ones()))
                .sum::<u64>();
            out.disagreeing += chunk
                .agreement
                .iter()
                .filter(|slot| slot.load(Relaxed) == DISAGREE)
                .count() as u64;
        }
        out
    }

    pub(super) fn reportable(&self, i: usize) -> Option<SlotState> {
        let (chunk, i) = self.locate(i);
        chunk.reportable(i)
    }
}

#[cfg(test)]
mod tests {
    use super::{BLOCK_SLOTS, Chunk, RAW, SlotState, Store, decode};
    use crate::commands::check::locations::scan::Coord;
    use crate::commands::check::locations::store::{PlainStore, join_node};
    use crate::idset::IdSet;
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    const BAD: Coord = Coord::Invalid {
        lat_nano: 90_000_000_001,
        lon_nano: 0,
    };

    fn valid(lat: i32, lon: i32) -> Coord {
        Coord::Valid { lat, lon }
    }

    fn coordinate(block: usize, i: i32) -> Option<Coord> {
        match block {
            0 => Some(valid(550_000_000 + i, -120_000_000 + 13 * i)),
            1 => Some(valid(
                if i % 2 == 0 {
                    900_000_000
                } else {
                    -900_000_000
                },
                if i % 4 < 2 {
                    -1_800_000_000 + i
                } else {
                    1_800_000_000 - i + 2
                },
            )),
            2 if i % 5 != 0 => Some(valid(
                -10,
                if i % 2 == 0 {
                    1_800_000_000
                } else {
                    -1_800_000_000
                },
            )),
            3 if i % 3 != 0 => Some(valid(i, -i)),
            5 if i % 2 != 0 => Some(valid(0, 0)),
            6 => Some(valid(i * 137, 180)),
            7 => Some(valid(
                scattered(i, 94_917_583, 900_000_000),
                scattered(i, 381_934_517, 1_800_000_000),
            )),
            8 => Some(valid(-i, i)),
            _ => None,
        }
    }

    fn scattered(i: i32, step: i64, limit: i64) -> i32 {
        i32::try_from(i64::from(i) * step % (2 * limit + 1) - limit)
            .expect("generated coordinate is within the limit")
    }

    #[test]
    fn generated_ranks_match_the_plain_store_before_and_after_way_checks() {
        let chunk_slots = 2 * BLOCK_SLOTS;
        let len = 8 * BLOCK_SLOTS + 3;
        let ids: Vec<i64> = (0..len)
            .map(|i| 3 * i64::try_from(i).expect("fixture ID offset fits i64") - 300)
            .collect();
        let mut keys = IdSet::new();
        for &id in &ids {
            keys.insert(id);
        }
        let keys = keys.into_ranked();

        let plain = PlainStore::new(len as u64).expect("construct oracle");
        let words: Vec<_> = std::iter::repeat_with(|| AtomicU64::new(0))
            .take(len)
            .collect();
        for (i, word) in words.iter().enumerate() {
            if let Some(coord) = coordinate(
                i / BLOCK_SLOTS,
                i32::try_from(i % BLOCK_SLOTS).expect("block offset fits i32"),
            ) {
                join_node(word, coord);
                plain.join_node(i, coord);
            }
        }
        // Identical duplicates, a conflict, and both visit orders of
        // unusable versus conflicting valid records.
        let base = 3 * BLOCK_SLOTS;
        for (i, records) in [
            (base + 1, [valid(1, -1), valid(1, -1)]),
            (base + 2, [valid(8, 8), valid(8, 8)]),
            (base + 4, [valid(8, 8), BAD]),
            (base + 5, [BAD, valid(8, 8)]),
        ] {
            for record in records {
                join_node(&words[i], record);
                plain.join_node(i, record);
            }
        }
        let mut packed = Store::new(chunk_slots);
        for words in words.chunks(chunk_slots) {
            packed
                .push(Chunk::encode(words, true))
                .expect("append chunk");
        }
        assert_eq!(packed.len(), len);
        assert_eq!(packed.chunks[0].blocks[0].width, 14);
        assert_eq!(packed.chunks[0].blocks[1].width, RAW);
        assert_eq!(packed.chunks[1].blocks[0].width, 32);
        assert_eq!(packed.chunks[1].blocks[1].width, RAW);
        assert!(packed.chunks[2].data.is_empty());

        for id in ids[0] - 1..=ids[len - 1] + 1 {
            let rank = keys.numeric_rank_if_set(id);
            let slot = rank.map(|r| usize::try_from(r).expect("fixture rank fits usize"));
            assert_eq!(
                slot.and_then(|i| packed.node_state(i)),
                slot.filter(|&i| plain.has_authority(i))
                    .map(|i| plain.state(i)),
                "node ID {id}",
            );
            let Some(i) = slot else { continue };
            if plain.has_authority(i) {
                if i % 5 == 0 && matches!(plain.state(i), SlotState::Coord { .. }) {
                    plain.mark_mismatch(i);
                    packed.mark_mismatch(i);
                    packed.mark_mismatch(i);
                }
            } else {
                compare_agreement(&plain, &packed, i);
            }
            assert_eq!(packed.reportable(i), plain.reportable(i), "node ID {id}");
        }
        assert_eq!(packed.finish(), plain.finish());
    }

    fn compare_agreement(plain: &PlainStore, packed: &Store, i: usize) {
        let lon = if i % 2 == 0 {
            1_800_000_000
        } else {
            -1_800_000_000
        };
        for inline in [lon, lon, if i % 3 == 0 { -lon } else { lon }] {
            plain.agree(i, 0, inline);
            packed.agree(i, 0, inline);
            let (chunk, local) = packed.locate(i);
            assert_eq!(
                decode(chunk.agreement_slot(local).load(Relaxed)),
                plain.state(i),
                "agreement at rank {i}",
            );
        }
        assert_eq!(packed.node_state(i), None);
    }

    #[test]
    fn budgeting_uses_encoded_bytes_and_agreement_allocation() {
        let chunk_slots = 2 * BLOCK_SLOTS;
        let build = |block, agreement| {
            let words: Vec<_> = (0..chunk_slots)
                .map(|i| {
                    let word = AtomicU64::new(0);
                    if let Some(coord) = coordinate(
                        block,
                        i32::try_from(i % BLOCK_SLOTS).expect("block offset fits i32"),
                    ) {
                        join_node(&word, coord);
                    }
                    word
                })
                .collect();
            Chunk::encode(&words, agreement)
        };
        let packed = build(0, true);
        let raw = build(1, true);
        let absent = build(4, true);
        let absent_no_agreement = build(4, false);
        assert!(packed.bytes() < raw.bytes());
        assert!(packed.bytes() < absent.bytes());
        assert!(absent_no_agreement.bytes() < packed.bytes());
        assert!(absent_no_agreement.agreement.is_empty());
        assert_eq!(absent_no_agreement.node_state(0), None);
        for chunk in [&packed, &raw, &absent] {
            assert!(chunk.bytes() <= Chunk::max_bytes(chunk_slots, true));
        }

        let budget = packed.bytes() * 2;
        let mut store = Store::new(chunk_slots);
        assert!(store.fits(&packed, budget));
        store.push(packed).expect("append chunk");
        assert!(store.fits(&build(0, true), budget));
        assert!(!store.fits(&raw, budget));
        assert!(!store.fits(&absent, budget));
        assert_eq!(Chunk::slots_for_budget(0, true), 1);
        assert!(Store::new(1).fits(&Chunk::encode(&[AtomicU64::new(0)], true), 0));
    }
}
