//! The partition plan and the per-partition slot store of `check --locations`.
//!
//! # Partitions
//!
//! The key set `K` (node IDs with a slot) is ranked, and its rank space is cut
//! into partitions of at most `S` slots. Partition `k` holds ranks
//! `[k*S, (k+1)*S)`, which are exactly the members of the ID interval
//! `[B_k, B_k+1)`, where `B_k` is the smallest ID whose rank (members strictly
//! below it) is at least `k*S`. `B_0` is `i64::MIN` and the last partition is
//! unbounded above, so the partitions cover every `i64` and each ID belongs to
//! exactly one of them, whether or not it is in `K`.
//!
//! # Slots
//!
//! One `u64` per member of the partition, in rank order, plus two bit sets:
//! *authority* (a node record with this ID exists) and *mismatch* (some ref
//! disagreed with that record). A slot holds a [`crate::coord_pack`] word or
//! one of three sentinels, which put a biased latitude above the 1.8e9 that
//! `coord_pack::pack` can produce, so no coordinate can collide with them.
//!
//! The node phase joins every record into its slot over the lattice
//! `EMPTY < VALID(c) < CONFLICT < UNUSABLE`: two different valid coordinates
//! join to CONFLICT and any record with an unusable coordinate forces
//! UNUSABLE. A join is order independent, so the final state does not depend
//! on which worker got there first. The way phase reads node slots only after
//! the node phase has joined (the pool's thread join is the barrier), and, in
//! agreement mode, runs a second absorbing state machine on the slots of IDs
//! without a node record: `EMPTY -> INLINE(c) -> DISAGREE`, where DISAGREE
//! means the refs carried at least two different coordinates - again a fact
//! about the set of coordinates, not about their order. Counts are taken by a
//! scan of the final states, never from which CAS won.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::scan::Coord;
use crate::coord_pack::{PRESENT, pack, unpack};
use crate::idset::RankedIdSet;

/// Node records with this ID disagree about its coordinate.
const CONFLICT: u64 = PRESENT | 0x7FFF_FFF0;
/// A node record with this ID has a coordinate outside the domain.
const UNUSABLE: u64 = PRESENT | 0x7FFF_FFF1;
/// No node record; the refs carry at least two different coordinates.
const DISAGREE: u64 = PRESENT | 0x7FFF_FFF2;

/// Bits one slot costs: the word plus the two bit sets.
const BITS_PER_SLOT: u64 = 64 + 2;

/// The slot capacity a store budget in bytes buys, at least one.
pub(super) fn slots_for_budget(bytes: u64) -> u64 {
    (bytes.saturating_mul(8) / BITS_PER_SLOT).max(1)
}

/// The decoded state of one slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SlotState {
    Empty,
    Coord { lat: i32, lon: i32 },
    Conflict,
    Unusable,
    Disagree,
}

fn decode(word: u64) -> SlotState {
    match word {
        0 => SlotState::Empty,
        CONFLICT => SlotState::Conflict,
        UNUSABLE => SlotState::Unusable,
        DISAGREE => SlotState::Disagree,
        packed => match unpack(packed) {
            Some((lat, lon)) => SlotState::Coord { lat, lon },
            None => SlotState::Empty,
        },
    }
}

/// How the key set's rank space is cut into partitions.
pub(super) struct Plan {
    total: u64,
    slots_per_partition: u64,
    /// `B_1 ..= B_(P-1)`, ascending.
    bounds: Vec<i64>,
}

impl Plan {
    /// Plan partitions of at most `slots_per_partition` members over `keys`.
    /// An empty key set gets one partition and no boundary search.
    pub(super) fn new(keys: &RankedIdSet, slots_per_partition: u64) -> Self {
        let total = keys.total_count();
        let slots_per_partition = slots_per_partition.max(1);
        let partitions = total.div_ceil(slots_per_partition).max(1);
        let bounds = (1..partitions)
            .map(|k| lowest_id_with_rank_at_least(keys, k * slots_per_partition))
            .collect();
        Self {
            total,
            slots_per_partition,
            bounds,
        }
    }

    pub(super) fn partitions(&self) -> usize {
        self.bounds.len() + 1
    }

    /// Partition `k`'s ID interval: inclusive low end, exclusive high end
    /// (`None`: unbounded).
    pub(super) fn ids(&self, k: usize) -> (i64, Option<i64>) {
        let lo = if k == 0 { i64::MIN } else { self.bounds[k - 1] };
        (lo, self.bounds.get(k).copied())
    }

    /// Partition `k`'s first rank and its slot count.
    pub(super) fn ranks(&self, k: usize) -> (u64, u64) {
        let first = k as u64 * self.slots_per_partition;
        let end = (first + self.slots_per_partition).min(self.total);
        (first, end.saturating_sub(first))
    }
}

/// The smallest ID `x >= 0` whose rank is at least `target`. The caller
/// asks only for `target < total`, which `x = i64::MAX` always satisfies
/// (its rank counts every member but possibly `i64::MAX` itself).
fn lowest_id_with_rank_at_least(keys: &RankedIdSet, target: u64) -> i64 {
    let (mut lo, mut hi) = (0_i64, i64::MAX);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if keys.rank(mid) >= target {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

/// Whether `id` lies in the half-open interval `(lo, hi)` of a partition.
pub(super) fn in_partition(id: i64, (lo, hi): (i64, Option<i64>)) -> bool {
    id >= lo && hi.is_none_or(|hi| id < hi)
}

/// Whether the inclusive range `[min, max]` meets a partition's interval.
pub(super) fn range_meets(min: i64, max: i64, (lo, hi): (i64, Option<i64>)) -> bool {
    max >= lo && hi.is_none_or(|hi| min < hi)
}

fn bitset(len: usize) -> Vec<AtomicU64> {
    std::iter::repeat_with(|| AtomicU64::new(0))
        .take(len.div_ceil(64))
        .collect()
}

fn bit(set: &[AtomicU64], i: usize) -> (&AtomicU64, u64) {
    (&set[i >> 6], 1_u64 << (i & 63))
}

/// One partition's slots.
pub(super) struct Store {
    slots: Vec<AtomicU64>,
    authority: Vec<AtomicU64>,
    mismatch: Vec<AtomicU64>,
}

/// Final-state counts of one partition.
#[derive(Default, Debug, PartialEq, Eq)]
pub(super) struct Final {
    /// Slots with a node record.
    pub present: u64,
    /// Slots whose node records disagree with each other.
    pub conflicting: u64,
    /// Slots with a node record some ref disagreed with.
    pub mismatched: u64,
    /// Slots without a node record whose refs disagree (agreement mode).
    pub disagreeing: u64,
}

impl Store {
    pub(super) fn new(len: u64) -> crate::Result<Self> {
        let len = usize::try_from(len).map_err(|_| {
            crate::Error::internal("location check partition does not fit in memory")
        })?;
        Ok(Self {
            slots: std::iter::repeat_with(|| AtomicU64::new(0))
                .take(len)
                .collect(),
            authority: bitset(len),
            mismatch: bitset(len),
        })
    }

    /// Join one node record into slot `i`.
    pub(super) fn join_node(&self, i: usize, coord: Coord) {
        let (word, mask) = bit(&self.authority, i);
        word.fetch_or(mask, Relaxed);
        let record = match coord {
            // In domain by construction, so `pack` cannot refuse it; the
            // fallback keeps the join total rather than panicking.
            Coord::Valid { lat, lon } => pack(lat, lon).unwrap_or(UNUSABLE),
            Coord::Invalid { .. } => UNUSABLE,
        };
        let slot = &self.slots[i];
        let mut cur = slot.load(Relaxed);
        loop {
            let next = match cur {
                UNUSABLE => return,
                _ if record == UNUSABLE => UNUSABLE,
                CONFLICT => return,
                0 => record,
                same if same == record => return,
                _ => CONFLICT,
            };
            match slot.compare_exchange_weak(cur, next, Relaxed, Relaxed) {
                Ok(_) => return,
                Err(seen) => cur = seen,
            }
        }
    }

    pub(super) fn has_authority(&self, i: usize) -> bool {
        let (word, mask) = bit(&self.authority, i);
        word.load(Relaxed) & mask != 0
    }

    pub(super) fn state(&self, i: usize) -> SlotState {
        decode(self.slots[i].load(Relaxed))
    }

    pub(super) fn mark_mismatch(&self, i: usize) {
        let (word, mask) = bit(&self.mismatch, i);
        word.fetch_or(mask, Relaxed);
    }

    /// Record an inline coordinate for slot `i`, which has no node record.
    pub(super) fn agree(&self, i: usize, lat: i32, lon: i32) {
        let Some(coord) = pack(lat, lon) else {
            return;
        };
        let slot = &self.slots[i];
        let mut cur = slot.load(Relaxed);
        loop {
            let next = match cur {
                DISAGREE => return,
                0 => coord,
                same if same == coord => return,
                _ => DISAGREE,
            };
            match slot.compare_exchange_weak(cur, next, Relaxed, Relaxed) {
                Ok(_) => return,
                Err(seen) => cur = seen,
            }
        }
    }

    pub(super) fn len(&self) -> usize {
        self.slots.len()
    }

    /// Count the final states. Call only after every phase writing the store
    /// has joined.
    pub(super) fn finish(&self) -> Final {
        let mut out = Final::default();
        for i in 0..self.slots.len() {
            let state = self.state(i);
            if self.has_authority(i) {
                out.present += 1;
                if state == SlotState::Conflict {
                    out.conflicting += 1;
                }
                let (word, mask) = bit(&self.mismatch, i);
                if word.load(Relaxed) & mask != 0 {
                    out.mismatched += 1;
                }
            } else if state == SlotState::Disagree {
                out.disagreeing += 1;
            }
        }
        out
    }

    /// Whether slot `i` is one a node-ID detail reports: conflicting node
    /// records, or (without a node record) disagreeing refs.
    pub(super) fn reportable(&self, i: usize) -> Option<SlotState> {
        match (self.has_authority(i), self.state(i)) {
            (true, SlotState::Conflict) => Some(SlotState::Conflict),
            (false, SlotState::Disagree) => Some(SlotState::Disagree),
            _ => None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{Coord, Plan, SlotState, Store, in_partition, range_meets, slots_for_budget};
    use crate::idset::IdSet;

    fn valid(lat: i32, lon: i32) -> Coord {
        Coord::Valid { lat, lon }
    }

    const BAD: Coord = Coord::Invalid {
        lat_nano: 100_000_000_000,
        lon_nano: 0,
    };

    /// Every order of the same records reaches the same final state.
    #[test]
    fn node_join_is_order_independent() {
        let cases: [(&[Coord], SlotState); 4] = [
            (
                &[valid(1, 2), valid(1, 2)],
                SlotState::Coord { lat: 1, lon: 2 },
            ),
            (&[valid(1, 2), valid(3, 4)], SlotState::Conflict),
            (&[valid(1, 2), BAD], SlotState::Unusable),
            (&[valid(1, 2), valid(3, 4), BAD], SlotState::Unusable),
        ];
        for (records, want) in cases {
            let orders = permutations(records.len());
            assert_eq!(orders.len(), (1..=records.len()).product::<usize>());
            for order in orders {
                let store = Store::new(1).unwrap();
                for &r in &order {
                    store.join_node(0, records[r]);
                }
                assert_eq!(store.state(0), want, "{records:?} in order {order:?}");
                assert!(store.has_authority(0));
            }
        }
    }

    /// Every ordering of `0..n`.
    fn permutations(n: usize) -> Vec<Vec<usize>> {
        if n == 0 {
            return vec![Vec::new()];
        }
        let mut out = Vec::new();
        for rest in permutations(n - 1) {
            for at in 0..=rest.len() {
                let mut order = rest.clone();
                order.insert(at, n - 1);
                out.push(order);
            }
        }
        out
    }

    /// The same records joined by racing threads, many times over, end in
    /// the state a sequential join reaches.
    #[test]
    fn concurrent_node_joins_reach_the_sequential_state() {
        let cases: [(&[Coord], SlotState); 3] = [
            (
                &[valid(1, 2), valid(1, 2), valid(1, 2)],
                SlotState::Coord { lat: 1, lon: 2 },
            ),
            (
                &[valid(1, 2), valid(3, 4), valid(5, 6)],
                SlotState::Conflict,
            ),
            (&[valid(1, 2), valid(3, 4), BAD], SlotState::Unusable),
        ];
        for (records, want) in cases {
            for _ in 0..200 {
                let store = Store::new(1).unwrap();
                let barrier = std::sync::Barrier::new(records.len());
                std::thread::scope(|scope| {
                    for &record in records {
                        let (store, barrier) = (&store, &barrier);
                        scope.spawn(move || {
                            barrier.wait();
                            store.join_node(0, record);
                        });
                    }
                });
                assert_eq!(store.state(0), want, "{records:?}");
            }
        }
    }

    /// Racing inline copies of an absent node: two distinct values always
    /// end in DISAGREE, one value never does.
    #[test]
    fn concurrent_agreement_reaches_the_sequential_state() {
        for (copies, want) in [
            (
                [(5, 6), (5, 6), (5, 6)],
                SlotState::Coord { lat: 5, lon: 6 },
            ),
            ([(5, 6), (5, 6), (7, 8)], SlotState::Disagree),
        ] {
            for _ in 0..200 {
                let store = Store::new(1).unwrap();
                let barrier = std::sync::Barrier::new(copies.len());
                std::thread::scope(|scope| {
                    for (lat, lon) in copies {
                        let (store, barrier) = (&store, &barrier);
                        scope.spawn(move || {
                            barrier.wait();
                            store.agree(0, lat, lon);
                        });
                    }
                });
                assert_eq!(store.state(0), want);
            }
        }
    }

    #[test]
    fn agreement_is_absorbing() {
        let store = Store::new(2).unwrap();
        store.agree(0, 5, 6);
        store.agree(0, 5, 6);
        assert_eq!(store.state(0), SlotState::Coord { lat: 5, lon: 6 });
        store.agree(1, 5, 6);
        store.agree(1, 7, 8);
        store.agree(1, 5, 6);
        assert_eq!(store.state(1), SlotState::Disagree);
        let fin = store.finish();
        assert_eq!(fin.disagreeing, 1);
        assert_eq!(fin.present, 0);
    }

    /// Null Island is a coordinate, not an empty slot.
    #[test]
    fn null_island_is_a_coordinate() {
        let store = Store::new(1).unwrap();
        store.join_node(0, valid(0, 0));
        assert_eq!(store.state(0), SlotState::Coord { lat: 0, lon: 0 });
    }

    #[test]
    fn partitions_cover_every_member_exactly_once() {
        let mut set = IdSet::new();
        let ids = [0_i64, 5, 6, 7, 1 << 30, (1 << 30) + 1, 1 << 40, i64::MAX];
        for id in ids {
            set.set(id);
        }
        let keys = set.into_ranked();
        for per in 1..=9 {
            let plan = Plan::new(&keys, per);
            assert_eq!(plan.partitions() as u64, (ids.len() as u64).div_ceil(per));
            let mut seen = 0;
            for k in 0..plan.partitions() {
                let range = plan.ids(k);
                let (first, len) = plan.ranks(k);
                let members: Vec<i64> = ids
                    .iter()
                    .copied()
                    .filter(|&id| in_partition(id, range))
                    .collect();
                assert_eq!(members.len() as u64, len, "per {per} partition {k}");
                for (offset, id) in members.iter().enumerate() {
                    assert_eq!(keys.rank_if_set(*id), Some(first + offset as u64));
                }
                seen += members.len();
            }
            assert_eq!(seen, ids.len());
        }
    }

    #[test]
    fn an_empty_key_set_has_one_unbounded_partition() {
        let keys = IdSet::new().into_ranked();
        let plan = Plan::new(&keys, 10);
        assert_eq!(plan.partitions(), 1);
        assert_eq!(plan.ids(0), (i64::MIN, None));
        assert_eq!(plan.ranks(0), (0, 0));
    }

    #[test]
    fn ranges_meet_half_open_partitions() {
        assert!(range_meets(5, 9, (9, Some(20))));
        assert!(!range_meets(5, 8, (9, Some(20))));
        assert!(!range_meets(20, 30, (9, Some(20))));
        assert!(range_meets(i64::MIN, -1, (i64::MIN, Some(0))));
        assert!(range_meets(i64::MAX, i64::MAX, (9, None)));
    }

    #[test]
    fn budget_buys_at_least_one_slot() {
        assert_eq!(slots_for_budget(0), 1);
        assert_eq!(slots_for_budget(66), 8);
    }
}
