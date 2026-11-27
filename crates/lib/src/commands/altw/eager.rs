//! All-node coordinate store for indexed sparse ALTW.
//!
//! A worker publishes one immutable ID-range segment per node blob. Only the
//! directory is assembled after the workers join. The coordinate slots hold
//! raw pairs, with membership kept in the segment's keys. Geographic and
//! duplicate checks happen on lookup, since only way references made those
//! checks observable in the reference-selected store.

use std::cell::OnceCell;
use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use crate::blob_meta::{BlobIndex, ClaimWalk, Order};
use crate::error::{Error, Location, Result};
use crate::scan::node::NodeTuple;
use crate::tunables::{
    ALTW_EAGER_DIRECTORY_BITS, ALTW_EAGER_STORE_BUDGET_PCT, AUTO_STORE_BUDGET_PCT,
};

type Coord = (i32, i32);

/// Charge the raw pairs against the existing page-cache budget with room
/// for a key per pair in the sparsest segments. A separate exact bound on
/// per-blob metadata keeps many tiny blobs from eating the reserved margin.
pub(super) fn fits(store_bytes: u64, node_blobs: u64, budget: u64) -> bool {
    let store = u128::from(store_bytes);
    let budget = u128::from(budget);
    let per_blob =
        size_of::<OnceLock<Segment>>() + size_of::<Segment>() + 2 * size_of::<Range<usize>>();
    let metadata = u128::from(node_blobs) * per_blob as u128;
    store * 100 <= budget * u128::from(ALTW_EAGER_STORE_BUDGET_PCT)
        && (2 * store + metadata) * 100 <= budget * u128::from(AUTO_STORE_BUDGET_PCT)
}

/// The representation of IDs in one blob, from dense to arbitrarily sparse.
enum Keys {
    Dense,
    Bitmap(Box<[RankWord]>),
    /// Repeated IDs here retain their distinct coordinate pairs in file order.
    Sorted(Box<[i64]>),
}

#[derive(Clone, Copy, Default)]
struct RankWord {
    present: u64,
    before: usize,
}

pub(super) struct Segment {
    min_id: i64,
    max_id: i64,
    blob_index: u64,
    frame_offset: u64,
    keys: Keys,
    coords: Box<[Coord]>,
}

impl Segment {
    pub(super) fn from_blob(
        tuples: &mut Vec<NodeTuple>,
        claim: &BlobIndex,
        blob_index: u64,
        frame_offset: u64,
    ) -> Result<Self> {
        let mut walk = ClaimWalk::new(Some(claim), Order::Free);
        let mut previous = i64::MIN;
        let mut descended = false;
        let mut repeated = false;
        for (position, tuple) in tuples.iter().enumerate() {
            walk.observe(tuple.id)?;
            descended |= tuple.id < previous;
            repeated |= position != 0 && tuple.id == previous;
            previous = tuple.id;
        }
        walk.finish()?;
        if descended {
            // Same-ID records keep their source order for duplicate checks.
            tuples.sort_by_key(|tuple| tuple.id);
            repeated = tuples.windows(2).any(|pair| pair[0].id == pair[1].id);
        }
        if repeated {
            tuples.dedup_by_key(|tuple| (tuple.id, tuple.lat, tuple.lon));
            repeated = tuples.windows(2).any(|pair| pair[0].id == pair[1].id);
        }
        let span = claim.max_id.abs_diff(claim.min_id);
        let keys = if repeated {
            Keys::Sorted(tuples.iter().map(|tuple| tuple.id).collect())
        } else if span == (tuples.len() - 1) as u64 {
            Keys::Dense
        } else {
            sparse_keys(tuples, claim.min_id, span)
        };
        Ok(Self {
            min_id: claim.min_id,
            max_id: claim.max_id,
            blob_index,
            frame_offset,
            keys,
            coords: tuples.iter().map(|tuple| (tuple.lat, tuple.lon)).collect(),
        })
    }

    fn records(&self, id: i64) -> &[Coord] {
        if !(self.min_id..=self.max_id).contains(&id) {
            return &[];
        }
        let offset = self.min_id.abs_diff(id);
        // Dense and bitmap segments were sized to cover this offset.
        #[allow(clippy::cast_possible_truncation)]
        let rank = match &self.keys {
            Keys::Dense => offset as usize,
            Keys::Bitmap(words) => {
                let word = &words[(offset / u64::from(u64::BITS)) as usize];
                let bit = 1 << (offset % u64::from(u64::BITS));
                if word.present & bit == 0 {
                    return &[];
                }
                word.before + (word.present & (bit - 1)).count_ones() as usize
            }
            Keys::Sorted(ids) => {
                let start = ids.partition_point(|&key| key < id);
                if ids.get(start) != Some(&id) {
                    return &[];
                }
                // Adjacent identical records were collapsed. If there is
                // a second pair, it suffices to resolve which refusal wins.
                let count = if ids.get(start + 1) == Some(&id) {
                    2
                } else {
                    1
                };
                return &self.coords[start..start + count];
            }
        };
        &self.coords[rank..rank + 1]
    }

    fn locate(&self, error: Error, source: &Arc<Path>) -> Error {
        error.locate(Location::new(
            Some(source),
            Some(self.blob_index),
            Some(crate::ByteOffset(self.frame_offset)),
        ))
    }
}

fn sparse_keys(tuples: &[NodeTuple], min_id: i64, span: u64) -> Keys {
    let word_bits = u64::from(u64::BITS);
    let words = span / word_bits + 1;
    let key_bytes = tuples.len() * size_of::<i64>();
    if words > (key_bytes / size_of::<RankWord>()) as u64 {
        return Keys::Sorted(tuples.iter().map(|tuple| tuple.id).collect());
    }
    #[allow(clippy::cast_possible_truncation)]
    let mut words = vec![RankWord::default(); words as usize];
    for tuple in tuples {
        let offset = min_id.abs_diff(tuple.id);
        #[allow(clippy::cast_possible_truncation)]
        let word = &mut words[(offset / word_bits) as usize];
        word.present |= 1 << (offset % word_bits);
    }
    let mut before = 0;
    for word in &mut words {
        word.before = before;
        before += word.present.count_ones() as usize;
    }
    Keys::Bitmap(words.into_boxed_slice())
}

pub(super) struct Index {
    segments: Vec<Segment>,
    directory: Vec<Range<usize>>,
    min_id: i64,
    max_id: i64,
    shift: u32,
}

impl Index {
    pub(super) fn finish(
        slots: Vec<OnceLock<Segment>>,
        invocation: &crate::host::Recorder,
    ) -> Result<Self> {
        let mut segments = Vec::with_capacity(slots.len());
        for slot in slots {
            segments.push(
                slot.into_inner().ok_or_else(|| {
                    Error::internal("node task did not fill its all-node segment")
                })?,
            );
        }
        segments.sort_unstable_by_key(|segment| (segment.min_id, segment.blob_index));
        let min_id = segments.first().map_or(0, |segment| segment.min_id);
        let max_id = segments
            .iter()
            .map(|segment| segment.max_id)
            .max()
            .unwrap_or(0);
        let bits = segments.len().max(1).ilog2();
        let span_bits = u64::BITS - min_id.abs_diff(max_id).leading_zeros();
        let shift = span_bits.saturating_sub(bits.min(ALTW_EAGER_DIRECTORY_BITS));
        let directory = build_directory(&segments, min_id, max_id, shift);
        let coord_bytes: usize = segments
            .iter()
            .map(|s| size_of_val(s.coords.as_ref()))
            .sum();
        invocation.counter("altw_pass1_store_bytes", coord_bytes);
        Ok(Self {
            segments,
            directory,
            min_id,
            max_id,
            shift,
        })
    }

    fn candidates(&self, id: i64) -> &[Segment] {
        if !(self.min_id..=self.max_id).contains(&id) {
            return &[];
        }
        let bucket = bucket_for(id, self.min_id, self.shift);
        &self.segments[self.directory[bucket].clone()]
    }

    fn get(&self, id: i64) -> std::result::Result<Option<Coord>, &[Segment]> {
        let candidates = self.candidates(id);
        let mut found = None;
        for segment in candidates {
            for &(lat, lon) in segment.records(id) {
                if !crate::coord_pack::in_domain(lat, lon)
                    || found.is_some_and(|first| first != (lat, lon))
                {
                    return Err(candidates);
                }
                found = Some((lat, lon));
            }
        }
        Ok(found)
    }
}

/// Diagnose the first failing node record in file order. The directory's
/// numeric order is only for lookup. As with a ranked insert, test each
/// record's domain before comparing it to the existing coordinate.
#[cold]
fn refused_node(id: i64, candidates: &[Segment], source: &Arc<Path>) -> Error {
    let records = || {
        candidates.iter().filter_map(move |segment| {
            let coords = segment.records(id);
            (!coords.is_empty()).then_some((segment, coords))
        })
    };
    let Some((_, first)) = records().min_by_key(|(segment, _)| segment.blob_index) else {
        return Error::internal("all-node lookup refused an absent node");
    };
    let original = first[0];
    let differing = records()
        .filter_map(|(segment, coords)| {
            coords
                .iter()
                .copied()
                .find(|&(lat, lon)| {
                    !crate::coord_pack::in_domain(lat, lon) || (lat, lon) != original
                })
                .map(|coords| (segment, coords))
        })
        .min_by_key(|(segment, _)| segment.blob_index);
    let Some((segment, (lat, lon))) = differing else {
        return Error::internal("all-node lookup refused identical valid coordinates");
    };
    let error = if crate::coord_pack::in_domain(lat, lon) {
        super::conflicting_node_coordinates_error(id, original, (lat, lon))
    } else {
        super::node_coordinate_domain_error(id, lat, lon)
    };
    segment.locate(error, source)
}

// The shift bounds this to the number of bits in the directory index.
#[allow(clippy::cast_possible_truncation)]
fn bucket_for(id: i64, min_id: i64, shift: u32) -> usize {
    min_id.abs_diff(id).checked_shr(shift).unwrap_or(0) as usize
}

fn build_directory(
    segments: &[Segment],
    min_id: i64,
    max_id: i64,
    shift: u32,
) -> Vec<Range<usize>> {
    let mut low = 0;
    let mut high = 0;
    (0..=bucket_for(max_id, min_id, shift))
        .map(|bucket| {
            while low < segments.len() && bucket_for(segments[low].max_id, min_id, shift) < bucket {
                low += 1;
            }
            high = high.max(low);
            while high < segments.len()
                && bucket_for(segments[high].min_id, min_id, shift) <= bucket
            {
                high += 1;
            }
            low..high
        })
        .collect()
}

/// One way blob's view of the dictionary. The shared reframe looks up
/// locations through an Option, so retain the first refusal until that
/// reframe returns. A refused blob never reaches the writer.
pub(super) struct Lookup<'a> {
    index: &'a Index,
    source: &'a Arc<Path>,
    error: OnceCell<Error>,
}

impl<'a> Lookup<'a> {
    pub(super) fn new(index: &'a Index, source: &'a Arc<Path>) -> Self {
        Self {
            index,
            source,
            error: OnceCell::new(),
        }
    }

    pub(super) fn get(&self, id: i64) -> Option<Coord> {
        match self.index.get(id) {
            Ok(coords) => coords,
            Err(candidates) => {
                self.error
                    .get_or_init(|| refused_node(id, candidates, self.source));
                None
            }
        }
    }

    pub(super) fn check(&mut self) -> Result<()> {
        self.error.take().map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(ids: impl IntoIterator<Item = i64>) -> Segment {
        let mut tuples: Vec<_> = ids
            .into_iter()
            .zip(1..)
            .map(|(id, lat)| NodeTuple { id, lat, lon: 0 })
            .collect();
        let claim = BlobIndex {
            kind: crate::ElementKind::Node,
            min_id: tuples.iter().map(|tuple| tuple.id).min().expect("nodes"),
            max_id: tuples.iter().map(|tuple| tuple.id).max().expect("nodes"),
            count: tuples.len() as u64,
            bbox: None,
        };
        Segment::from_blob(&mut tuples, &claim, 1, 2).expect("honest node claim")
    }

    #[test]
    fn eager_segment_keys_resolve_only_present_ids_across_words_and_extremes() {
        let dense = segment([-1, 0, 1]);
        assert!(matches!(dense.keys, Keys::Dense));
        assert_eq!(dense.records(0), &[(2, 0)]);
        assert!(dense.records(2).is_empty());

        let bitmap = segment((-3..200).step_by(2));
        assert!(matches!(bitmap.keys, Keys::Bitmap(_)));
        assert_eq!(bitmap.records(61), &[(33, 0)]);
        assert_eq!(bitmap.records(63), &[(34, 0)]);
        assert!(bitmap.records(62).is_empty());

        let sparse = segment([i64::MAX, i64::MIN]);
        assert!(matches!(sparse.keys, Keys::Sorted(_)));
        assert_eq!(sparse.records(i64::MIN), &[(2, 0)]);
        assert_eq!(sparse.records(i64::MAX), &[(1, 0)]);
        assert!(sparse.records(0).is_empty());
    }

    #[test]
    fn eager_budget_keeps_headroom_for_keys_and_directory() {
        assert!(fits(3_500, 1, 10_000));
        assert!(!fits(3_501, 1, 10_000));
        assert!(!fits(3_500, 100, 10_000));
        assert!(!fits(u64::MAX, u64::MAX, u64::MAX));
    }
}
