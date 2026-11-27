//! Rank boundaries and the state transitions shared by the compressed store
//! and the plain test oracle.
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
pub(super) const DISAGREE: u64 = PRESENT | 0x7FFF_FFF2;

/// The decoded state of one slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SlotState {
    Empty,
    Coord { lat: i32, lon: i32 },
    Conflict,
    Unusable,
    Disagree,
}

pub(super) fn decode(word: u64) -> SlotState {
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

/// The smallest ID whose rank is at least `target`, searched over every
/// `i64`. The caller asks only for `target < total`, which `x = i64::MAX`
/// always satisfies (its rank counts every member but possibly `i64::MAX`
/// itself). The midpoint is taken through the unsigned distance, which
/// `hi - lo` would overflow for `lo < 0 < hi`.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
pub(super) fn lowest_id_with_rank_at_least(keys: &RankedIdSet, target: u64) -> i64 {
    let (mut lo, mut hi) = (i64::MIN, i64::MAX);
    while lo < hi {
        let mid = lo.wrapping_add((hi.wrapping_sub(lo) as u64 / 2) as i64);
        if keys.numeric_rank(mid) >= target {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

/// Whether `id` lies in the half-open interval `[lo, hi)` of a partition.
pub(super) fn in_partition(id: i64, (lo, hi): (i64, Option<i64>)) -> bool {
    id >= lo && hi.is_none_or(|hi| id < hi)
}

/// Whether the inclusive range `[min, max]` meets a partition's interval.
pub(super) fn range_meets(min: i64, max: i64, (lo, hi): (i64, Option<i64>)) -> bool {
    max >= lo && hi.is_none_or(|hi| min < hi)
}

pub(super) fn bitset(len: usize) -> Vec<AtomicU64> {
    std::iter::repeat_with(|| AtomicU64::new(0))
        .take(len.div_ceil(64))
        .collect()
}

pub(super) fn bit(set: &[AtomicU64], i: usize) -> (&AtomicU64, u64) {
    (&set[i >> 6], 1_u64 << (i & 63))
}

/// One partition's slots in the original plain representation.
#[cfg(test)]
pub(super) struct PlainStore {
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

#[cfg(test)]
impl PlainStore {
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
        join_node(&self.slots[i], coord);
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

    pub(super) fn agree(&self, i: usize, lat: i32, lon: i32) {
        join_agreement(&self.slots[i], lat, lon);
    }

    /// Count the final states after every writer has joined.
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

    pub(super) fn reportable(&self, i: usize) -> Option<SlotState> {
        match (self.has_authority(i), self.state(i)) {
            (true, SlotState::Conflict) => Some(SlotState::Conflict),
            (false, SlotState::Disagree) => Some(SlotState::Disagree),
            _ => None,
        }
    }
}

pub(super) fn join_node(slot: &AtomicU64, coord: Coord) {
    let record = match coord {
        // In domain by construction, so `pack` cannot refuse it; the
        // fallback keeps the join total rather than panicking.
        Coord::Valid { lat, lon } => pack(lat, lon).unwrap_or(UNUSABLE),
        Coord::Invalid { .. } => UNUSABLE,
    };
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

/// Record an inline coordinate for a slot with no node record.
pub(super) fn join_agreement(slot: &AtomicU64, lat: i32, lon: i32) {
    let Some(coord) = pack(lat, lon) else {
        return;
    };
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

#[cfg(test)]
mod tests {
    use super::{
        Coord, PlainStore, SlotState, in_partition, lowest_id_with_rank_at_least, range_meets,
    };
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
                let store = PlainStore::new(1).expect("construct location store");
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

    /// Model every linearization of three joins over the finite classes:
    /// empty, two distinct coordinates, conflict, and unusable. Successful
    /// CAS operations totally order updates to a slot; a failed (including
    /// spuriously failed) CAS retries from the observed state without an
    /// effect. Thus all concurrent outcomes reduce to these serial orders.
    /// The thread smoke tests below check wiring, not interleaving coverage.
    #[test]
    fn node_join_matches_the_lattice_model() {
        let records = [valid(1, 2), valid(3, 4), BAD];
        for a in 0..3 {
            for b in 0..3 {
                for c in 0..3 {
                    let selection = [a, b, c];
                    let want = if selection.contains(&2) {
                        SlotState::Unusable
                    } else if selection.contains(&0) && selection.contains(&1) {
                        SlotState::Conflict
                    } else if a == 0 {
                        SlotState::Coord { lat: 1, lon: 2 }
                    } else {
                        SlotState::Coord { lat: 3, lon: 4 }
                    };
                    for order in permutations(3) {
                        let store = PlainStore::new(1).expect("construct location store");
                        assert_eq!(store.state(0), SlotState::Empty);
                        for index in order {
                            store.join_node(0, records[selection[index]]);
                        }
                        assert_eq!(store.state(0), want);
                        assert!(store.has_authority(0));
                    }
                }
            }
        }
    }

    #[test]
    fn agreement_matches_the_set_model() {
        for mask in 0..8 {
            for order in permutations(3) {
                let store = PlainStore::new(1).expect("construct location store");
                for index in order {
                    let lat = if mask & (1 << index) == 0 { 5 } else { 7 };
                    store.agree(0, lat, 6);
                }
                let want = match mask {
                    0 => SlotState::Coord { lat: 5, lon: 6 },
                    7 => SlotState::Coord { lat: 7, lon: 6 },
                    _ => SlotState::Disagree,
                };
                assert_eq!(store.state(0), want);
                assert!(!store.has_authority(0));
            }
        }
    }

    /// A bounded smoke test: racing records end in
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
            let store = PlainStore::new(1).expect("construct location store");
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
            let store = PlainStore::new(1).expect("construct location store");
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

    #[test]
    fn agreement_is_absorbing() {
        let store = PlainStore::new(2).expect("construct location store");
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
        let store = PlainStore::new(1).expect("construct location store");
        store.join_node(0, valid(0, 0));
        assert_eq!(store.state(0), SlotState::Coord { lat: 0, lon: 0 });
    }

    #[test]
    fn partitions_cover_every_member_exactly_once() {
        covers_each_member_once(&[0_i64, 5, 6, 7, 1 << 30, (1 << 30) + 1, 1 << 40, i64::MAX]);
    }

    /// Both signs and both extremes: the boundary search runs over all of
    /// `i64`, so a partition may start at a negative ID, at zero, or in the
    /// gap between two members of different signs.
    #[test]
    fn signed_partitions_cover_every_member_exactly_once() {
        covers_each_member_once(&[
            i64::MIN,
            i64::MIN + 1,
            -(1 << 40),
            -7,
            -1,
            0,
            1,
            7,
            i64::MAX,
        ]);
        covers_each_member_once(&[i64::MIN, -1, 0, i64::MAX]);
        covers_each_member_once(&[-9, -5, -2]);
    }

    /// Every member falls in exactly one partition of every plan, at the
    /// slot its numeric rank names. `ids` ascends.
    fn covers_each_member_once(ids: &[i64]) {
        let mut set = IdSet::new();
        for &id in ids {
            set.insert(id);
        }
        let keys = set.into_ranked();
        for per in 1..=9 {
            let total = keys.total_count();
            let mut first = 0;
            let mut lo = i64::MIN;
            let mut seen = 0;
            while first < total {
                let len = per.min(total - first);
                let end = first + len;
                let hi = (end < total).then(|| lowest_id_with_rank_at_least(&keys, end));
                let range = (lo, hi);
                let members: Vec<i64> = ids
                    .iter()
                    .copied()
                    .filter(|&id| in_partition(id, range))
                    .collect();
                assert_eq!(members.len() as u64, len, "per {per}, first rank {first}");
                for (offset, id) in members.iter().enumerate() {
                    assert_eq!(keys.numeric_rank_if_set(*id), Some(first + offset as u64));
                }
                seen += members.len();
                first = end;
                lo = hi.unwrap_or(i64::MAX);
            }
            assert_eq!(seen, ids.len());
        }
    }

    #[test]
    fn ranges_meet_half_open_partitions() {
        assert!(range_meets(5, 9, (9, Some(20))));
        assert!(!range_meets(5, 8, (9, Some(20))));
        assert!(!range_meets(20, 30, (9, Some(20))));
        assert!(range_meets(i64::MIN, -1, (i64::MIN, Some(0))));
        assert!(range_meets(i64::MAX, i64::MAX, (9, None)));
    }
}
