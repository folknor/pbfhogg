//! Node join keys: how a signed node ID becomes a bucket and a bucket-local
//! key, for stage 1's emission and stage 2's join alike.
//!
//! The join walks each id bucket's sorted records against the node tuples
//! of the blobs that bucket reaches, both ascending, so the key must ascend
//! with the file's canonical order (`0, -1, -2, ..., 1, 2, ...`). One key
//! cannot do that over the signed range, so node IDs fall into two families:
//!
//! - an input whose node claims hold no negative ID has one family, every
//!   ID `>= 0` keyed by itself, exactly the layout the join always had;
//! - otherwise the **negative** family is the IDs `<= 0`, keyed by their
//!   magnitude (`0, -1, -2` are keys `0, 1, 2`), and the **positive**
//!   family the IDs `> 0`, keyed by themselves.
//!
//! In a canonically ordered blob each family is then a contiguous slice
//! (IDs `<= 0` first) ascending in its key, and in a sorted file each
//! family's blobs ascend in file order. At most one node blob of a sorted
//! file holds both families (`min_id <= 0 < max_id`); the metadata scan
//! decodes it for its exact per-family spans, since its numeric range does
//! not give them ([`super::blob_meta::BoundarySplit`]).
//!
//! The families share the fixed [`NUM_BUCKETS`] id buckets (so stage 1's
//! descriptor plan is the same), split in proportion to their claimed node
//! counts. A layout whose buckets all fit the packed one-word record
//! ([`super::IdCodec`]) divides each family's key range into equal widths,
//! as before; any other runs the wide record ([`super::WideCodec`]) and
//! places each family's bucket boundaries at node-blob spans by claimed
//! node count, so IDs clustered far from zero still spread over buckets.
//! The whole plan is a function of the claims, shared by the join and by
//! auto routing's [`super::HeaderAdmission`].

use crate::error::Result;
use crate::tunables::{MAX_BUCKET_WIDTH, NUM_BUCKETS};

use super::LOCAL_ID_MASK;
use super::blob_meta::BlobMeta;

/// Which family a node ID joins in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Family {
    /// IDs at or above the layout's threshold, keyed by themselves.
    Pos,
    /// IDs below the threshold (only in the signed arm: `<= 0`), keyed by
    /// their magnitude.
    Neg,
}

impl Family {
    /// The key of `id` in this family. The caller has placed `id` here.
    #[inline]
    #[allow(clippy::cast_sign_loss)]
    pub(super) fn key(self, id: i64) -> u64 {
        match self {
            Self::Pos => id as u64,
            Self::Neg => id.unsigned_abs(),
        }
    }

    /// The ID a key of this family stands for. Cold: error text only.
    #[allow(clippy::cast_possible_wrap)]
    pub(super) fn id_of(self, key: u64) -> i64 {
        match self {
            Self::Pos => key as i64,
            // Magnitude 2^63 is i64::MIN, which wrapping_neg maps to itself.
            Self::Neg => (key as i64).wrapping_neg(),
        }
    }
}

/// What the plan reads from the node blobs' claims, one blob at a time.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct NodeClaims {
    signed: bool,
    /// Largest non-negative `max_id`, for the one-family arm.
    nonneg_max: u64,
    /// Largest positive `max_id`, and the claimed nodes of blobs holding
    /// only positive IDs, for the signed arm's positive family.
    pos_max: Option<u64>,
    pos_count: u128,
    /// Largest magnitude among IDs `<= 0`, and the claimed nodes of every
    /// other blob (a blob holding both families counts here), for the
    /// signed arm's negative family.
    neg_max: Option<u64>,
    neg_count: u128,
}

impl NodeClaims {
    /// One node blob's claim (already checked not reversed).
    pub(super) fn observe(&mut self, min_id: i64, max_id: i64, count: u64) {
        self.signed |= min_id < 0;
        if let Ok(m) = u64::try_from(max_id) {
            self.nonneg_max = self.nonneg_max.max(m);
        }
        if max_id > 0 {
            self.pos_max = Some(self.pos_max.unwrap_or(0).max(max_id.unsigned_abs()));
        }
        if min_id > 0 {
            self.pos_count += u128::from(count);
        } else {
            self.neg_max = Some(self.neg_max.unwrap_or(0).max(min_id.unsigned_abs()));
            self.neg_count += u128::from(count);
        }
    }

    /// The claims of every node blob in `metas`.
    pub(super) fn of(metas: &[BlobMeta]) -> Self {
        let mut claims = Self::default();
        for m in metas
            .iter()
            .filter(|m| matches!(m.kind, crate::ElementKind::Node))
        {
            claims.observe(m.min_id, m.max_id, m.count);
        }
        claims
    }
}

/// One family's share of the buckets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FamilyPlan {
    /// Buckets allocated (0: the family is absent).
    pub(super) buckets: usize,
    pub(super) max_key: u64,
}

/// The layout decided from the claims alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct KeyPlan {
    /// The lowest ID of the positive family: 0 in the one-family arm, 1 in
    /// the signed arm.
    pub(super) threshold: i64,
    pub(super) pos: FamilyPlan,
    pub(super) neg: FamilyPlan,
    /// Whether every bucket fits the packed one-word record.
    pub(super) compact: bool,
}

impl KeyPlan {
    pub(super) fn new(claims: &NodeClaims) -> Self {
        let n = NUM_BUCKETS;
        let (threshold, pos, neg) = if !claims.signed {
            // Today's layout, unchanged: one family over `0..=max`, even with
            // no node blob at all (every ref but 0 is then out of range).
            let pos = FamilyPlan {
                buckets: n,
                max_key: claims.nonneg_max,
            };
            (
                0,
                pos,
                FamilyPlan {
                    buckets: 0,
                    max_key: 0,
                },
            )
        } else {
            let neg_max = claims.neg_max.unwrap_or(0);
            match claims.pos_max {
                None => (
                    1,
                    FamilyPlan {
                        buckets: 0,
                        max_key: 0,
                    },
                    FamilyPlan {
                        buckets: n,
                        max_key: neg_max,
                    },
                ),
                Some(pos_max) => {
                    let total = claims.pos_count + claims.neg_count;
                    // Rounded share of n, so at most n: the narrowing
                    // cannot truncate. Counts are claims and may be zero.
                    #[allow(clippy::cast_possible_truncation)]
                    let share = (claims.neg_count * n as u128 + total / 2)
                        .checked_div(total)
                        .map_or(n / 2, |s| s as usize);
                    let neg_buckets = share.clamp(1, n - 1);
                    (
                        1,
                        FamilyPlan {
                            buckets: n - neg_buckets,
                            max_key: pos_max,
                        },
                        FamilyPlan {
                            buckets: neg_buckets,
                            max_key: neg_max,
                        },
                    )
                }
            }
        };
        let compact = [pos, neg]
            .iter()
            .all(|f| f.buckets == 0 || compact_fits(f.max_key, f.buckets));
        Self {
            threshold,
            pos,
            neg,
            compact,
        }
    }
}

/// Width of one equal-width bucket over `0..=max_key`.
fn equal_width(max_key: u64, buckets: usize) -> u64 {
    max_key.div_ceil(buckets as u64).max(1)
}

/// Largest local key an equal-width layout produces: `width - 1` in every
/// bucket but the last, which absorbs the `div_ceil` remainder.
fn equal_max_local(max_key: u64, buckets: usize) -> u64 {
    let width = equal_width(max_key, buckets);
    let last_lo = (buckets as u64 - 1).saturating_mul(width);
    (width - 1).max(max_key.saturating_sub(last_lo))
}

/// Whether `0..=max_key` over `buckets` equal buckets fits the packed
/// record: widths up to [`MAX_BUCKET_WIDTH`] and local keys within
/// [`LOCAL_ID_MASK`]. With 256 buckets that is a `max_key` of 2^38.
pub(super) fn compact_fits(max_key: u64, buckets: usize) -> bool {
    buckets > 0
        && equal_width(max_key, buckets) <= MAX_BUCKET_WIDTH
        && equal_max_local(max_key, buckets) <= u64::from(LOCAL_ID_MASK)
}

/// One family's id-bucket layout.
#[derive(Clone, Debug)]
pub(super) struct FamilyLayout {
    /// Global index of this family's first bucket.
    first: usize,
    /// Buckets in use (a wide layout may use fewer than allocated).
    buckets: usize,
    max_key: u64,
    /// Equal-width layout: the width; the last bucket ends at `max_key`.
    width: u64,
    /// Wide layout: `buckets + 1` strictly increasing boundaries, the last
    /// `max_key + 1`. Empty for an equal-width layout.
    cuts: Vec<u64>,
    /// Largest local key [`Self::locate_compact`] can return.
    max_local: u64,
}

impl FamilyLayout {
    fn absent(first: usize) -> Self {
        Self {
            first,
            buckets: 0,
            max_key: 0,
            width: 1,
            cuts: Vec::new(),
            max_local: 0,
        }
    }

    /// An equal-width layout over `0..=max_key`.
    pub(super) fn equal(first: usize, buckets: usize, max_key: u64) -> Self {
        if buckets == 0 {
            return Self::absent(first);
        }
        Self {
            first,
            buckets,
            max_key,
            width: equal_width(max_key, buckets),
            cuts: Vec::new(),
            max_local: equal_max_local(max_key, buckets),
        }
    }

    /// A cut layout of at most `allocated` buckets over `spans` (this
    /// family's, in file order, ascending): a boundary at the first span's
    /// low key, then one at a later span's low key each time the claimed
    /// nodes before it pass the next of `allocated` equal shares, and
    /// `max_key + 1`. A boundary is never placed inside a span, and each
    /// share takes the next usable span boundary, so blobs past a large one
    /// still spread over buckets.
    pub(super) fn cut(first: usize, allocated: usize, max_key: u64, spans: &[KeySpan]) -> Self {
        let Some(head) = spans.first() else {
            return Self::absent(first);
        };
        if allocated == 0 {
            return Self::absent(first);
        }
        let weight = |s: &KeySpan| u128::from(s.count.max(1));
        let total: u128 = spans.iter().map(weight).sum();
        let shares = allocated as u128;
        let mut cuts = vec![head.lo];
        let mut before: u128 = 0;
        let mut next = 1u128;
        for pair in spans.windows(2) {
            before += weight(&pair[0]);
            let lo = pair[1].lo;
            if next < shares
                && before * shares >= next * total
                && cuts.last().is_some_and(|&c| lo > c)
            {
                cuts.push(lo);
                next += 1;
            }
        }
        // Spans lie within `0..=max_key`, so the sentinel is above every
        // cut; `max_key` is at most 2^63, so it cannot overflow.
        cuts.push(max_key + 1);
        let max_local = cuts.windows(2).map(|w| w[1] - 1 - w[0]).max().unwrap_or(0);
        Self {
            first,
            buckets: cuts.len() - 1,
            max_key,
            width: 1,
            cuts,
            max_local,
        }
    }

    /// `(global bucket, local key)` for `key` in an equal-width layout.
    #[inline]
    pub(super) fn locate_compact(&self, key: u64) -> Option<(usize, u64)> {
        if self.buckets == 0 || key > self.max_key {
            return None;
        }
        // The last bucket absorbs the `div_ceil` remainder.
        #[allow(clippy::cast_possible_truncation)]
        let idx = ((key / self.width) as usize).min(self.buckets - 1);
        Some((self.first + idx, key - idx as u64 * self.width))
    }

    /// `(global bucket, local key)` for `key` in a cut layout.
    #[inline]
    pub(super) fn locate_wide(&self, key: u64) -> Option<(usize, u64)> {
        let (&lo, &end) = (self.cuts.first()?, self.cuts.last()?);
        if key < lo || key >= end {
            return None;
        }
        // A key equal to an interior cut belongs to the bucket it starts.
        let idx = self.cuts.partition_point(|&c| c <= key) - 1;
        Some((self.first + idx, key - self.cuts[idx]))
    }

    /// The key range `[lo, hi)` of local bucket `idx`, the same bounds
    /// [`Self::locate_compact`] and [`Self::locate_wide`] route by.
    fn bounds(&self, idx: usize) -> (u64, u64) {
        if self.cuts.is_empty() {
            let lo = idx as u64 * self.width;
            let hi = if idx + 1 == self.buckets {
                self.max_key + 1
            } else {
                (idx as u64 + 1) * self.width
            };
            (lo, hi)
        } else {
            (self.cuts[idx], self.cuts[idx + 1])
        }
    }

    pub(super) fn max_local(&self) -> u64 {
        self.max_local
    }

    pub(super) fn buckets(&self) -> usize {
        self.buckets
    }

    pub(super) fn max_key(&self) -> u64 {
        self.max_key
    }

    /// Width of an equal-width layout (1 for a cut layout).
    pub(super) fn width(&self) -> u64 {
        self.width
    }
}

/// The run's node-key layout: both families over the shared id buckets.
#[derive(Clone, Debug)]
pub(super) struct NodeKeyLayout {
    threshold: i64,
    pub(super) pos: FamilyLayout,
    pub(super) neg: FamilyLayout,
    wide: bool,
}

impl NodeKeyLayout {
    /// The layout `plan` describes, cut over `spans` when it is wide.
    pub(super) fn new(plan: &KeyPlan, spans: &NodeSpans) -> Self {
        let neg_first = plan.pos.buckets;
        let (pos, neg) = if plan.compact {
            (
                FamilyLayout::equal(0, plan.pos.buckets, plan.pos.max_key),
                FamilyLayout::equal(neg_first, plan.neg.buckets, plan.neg.max_key),
            )
        } else {
            (
                FamilyLayout::cut(0, plan.pos.buckets, plan.pos.max_key, &spans.pos),
                FamilyLayout::cut(neg_first, plan.neg.buckets, plan.neg.max_key, &spans.neg),
            )
        };
        Self {
            threshold: plan.threshold,
            pos,
            neg,
            wide: !plan.compact,
        }
    }

    /// The family `id` joins in, and its key there.
    #[inline]
    pub(super) fn family_key(&self, id: i64) -> (Family, u64) {
        let family = if id >= self.threshold {
            Family::Pos
        } else {
            Family::Neg
        };
        (family, family.key(id))
    }

    /// The family of every ID at or above [`Self::threshold`].
    pub(super) fn threshold(&self) -> i64 {
        self.threshold
    }

    fn family(&self, family: Family) -> &FamilyLayout {
        match family {
            Family::Pos => &self.pos,
            Family::Neg => &self.neg,
        }
    }

    /// `(global bucket, local key)` for a ref in an equal-width layout, or
    /// `None` where no node blob's claim can hold it.
    #[inline]
    pub(super) fn locate_compact(&self, id: i64) -> Option<(usize, u64)> {
        let (family, key) = self.family_key(id);
        self.family(family).locate_compact(key)
    }

    /// [`Self::locate_compact`] for a cut layout.
    #[inline]
    pub(super) fn locate_wide(&self, id: i64) -> Option<(usize, u64)> {
        let (family, key) = self.family_key(id);
        self.family(family).locate_wide(key)
    }

    /// The family and key range `[lo, hi)` of global bucket `idx`, `None`
    /// for a bucket no key routes to.
    pub(super) fn bucket(&self, idx: usize) -> Option<(Family, u64, u64)> {
        for family in [Family::Pos, Family::Neg] {
            let f = self.family(family);
            if idx >= f.first && idx < f.first + f.buckets {
                let (lo, hi) = f.bounds(idx - f.first);
                return Some((family, lo, hi));
            }
        }
        None
    }

    /// Whether the run uses the wide shard record.
    pub(super) fn wide(&self) -> bool {
        self.wide
    }

    /// Largest local key either family can produce.
    pub(super) fn max_local(&self) -> u64 {
        self.pos.max_local().max(self.neg.max_local())
    }
}

/// One node blob's key range in one family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct KeySpan {
    pub(super) lo: u64,
    pub(super) hi: u64,
    /// The family's claimed or decoded node count in the blob.
    pub(super) count: u64,
    /// Index into the node-blob mapping (node blobs in file order).
    pub(super) blob: u32,
}

/// Every node blob's spans, per family, in file order.
#[derive(Clone, Debug, Default)]
pub(super) struct NodeSpans {
    pub(super) pos: Vec<KeySpan>,
    pub(super) neg: Vec<KeySpan>,
}

impl NodeSpans {
    pub(super) fn of(&self, family: Family) -> &[KeySpan] {
        match family {
            Family::Pos => &self.pos,
            Family::Neg => &self.neg,
        }
    }

    /// The spans of the node blobs in `metas` under `plan`'s threshold.
    /// Single-family claims give exact spans from their extrema; a blob
    /// holding both families carries its decoded split.
    pub(super) fn build(metas: &[BlobMeta], plan: &KeyPlan) -> Result<Self> {
        let mut spans = Self::default();
        let nodes = metas
            .iter()
            .filter(|m| matches!(m.kind, crate::ElementKind::Node));
        for (ordinal, m) in nodes.enumerate() {
            let blob = u32::try_from(ordinal).map_err(|_| {
                crate::Error::invalid_data(format!(
                    "add-locations-to-ways, external index: the input has more than {} node \
                     blobs, more than this backend can number. The sparse index has no such \
                     limit.",
                    1u64 << 32
                ))
            })?;
            let span = |lo, hi, count| KeySpan {
                lo,
                hi,
                count,
                blob,
            };
            if m.min_id >= plan.threshold {
                let (lo, hi) = (Family::Pos.key(m.min_id), Family::Pos.key(m.max_id));
                spans.pos.push(span(lo, hi, m.count));
            } else if m.max_id < plan.threshold {
                let (lo, hi) = (Family::Neg.key(m.max_id), Family::Neg.key(m.min_id));
                spans.neg.push(span(lo, hi, m.count));
            } else {
                let Some(split) = m.split else {
                    return Err(crate::Error::internal(format!(
                        "external join: node blob {ordinal} claims ids {}..={} across both \
                         key families but was not decoded by the metadata scan",
                        m.min_id, m.max_id
                    )));
                };
                if let Some(n) = split.neg {
                    spans.neg.push(span(n.lo, n.hi, n.count));
                }
                if let Some(p) = split.pos {
                    spans.pos.push(span(p.lo, p.hi, p.count));
                }
            }
        }
        Ok(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(blobs: &[(i64, i64, u64)]) -> NodeClaims {
        let mut c = NodeClaims::default();
        for &(lo, hi, n) in blobs {
            c.observe(lo, hi, n);
        }
        c
    }

    #[test]
    fn non_negative_claims_keep_the_one_family_layout() {
        let plan = KeyPlan::new(&claims(&[(0, 10, 3), (11, 14_000_000_000, 5)]));
        assert_eq!(plan.threshold, 0);
        assert_eq!(
            plan.pos,
            FamilyPlan {
                buckets: 256,
                max_key: 14_000_000_000
            }
        );
        assert_eq!(plan.neg.buckets, 0);
        assert!(plan.compact);
        let layout = NodeKeyLayout::new(&plan, &NodeSpans::default());
        assert_eq!(layout.locate_compact(0), Some((0, 0)));
        assert_eq!(layout.locate_compact(-1), None);
        assert_eq!(layout.pos.width(), 54_687_500);
        // No node blob at all: today's layout over 0..=0.
        let empty = KeyPlan::new(&NodeClaims::default());
        assert_eq!((empty.pos.buckets, empty.pos.max_key), (256, 0));
    }

    #[test]
    fn signed_claims_split_the_buckets_by_claimed_nodes() {
        // 0 and the negatives (300 nodes, the straddling blob counted here)
        // against 100 positives.
        let plan = KeyPlan::new(&claims(&[
            (-2, 0, 100),
            (-9, -5, 100),
            (-11, 2, 100),
            (3, 99, 100),
        ]));
        assert_eq!(plan.threshold, 1);
        assert_eq!(plan.neg.buckets, 192);
        assert_eq!(plan.pos.buckets, 64);
        assert_eq!((plan.neg.max_key, plan.pos.max_key), (11, 99));
        // Negatives only, and a lone negative among many positives.
        let only = KeyPlan::new(&claims(&[(-5, -1, 5)]));
        assert_eq!((only.pos.buckets, only.neg.buckets), (0, 256));
        let lone = KeyPlan::new(&claims(&[(-1, -1, 1), (1, 1 << 20, 1 << 20)]));
        assert_eq!((lone.pos.buckets, lone.neg.buckets), (255, 1));
    }

    #[test]
    fn keys_ascend_with_canonical_order_in_each_family() {
        let plan = KeyPlan::new(&claims(&[(i64::MIN, 0, 2), (1, i64::MAX, 2)]));
        let layout = NodeKeyLayout::new(&plan, &NodeSpans::default());
        assert_eq!(layout.family_key(0), (Family::Neg, 0));
        assert_eq!(layout.family_key(-1), (Family::Neg, 1));
        assert_eq!(layout.family_key(i64::MIN), (Family::Neg, 1 << 63));
        assert_eq!(layout.family_key(1), (Family::Pos, 1));
        assert_eq!(
            layout.family_key(i64::MAX),
            (Family::Pos, i64::MAX.unsigned_abs())
        );
        assert_eq!(Family::Neg.id_of(1 << 63), i64::MIN);
        assert_eq!(Family::Neg.id_of(7), -7);
        assert!(!plan.compact, "keys past 2^38 need the wide record");
    }

    fn span(lo: u64, hi: u64, count: u64, blob: u32) -> KeySpan {
        KeySpan {
            lo,
            hi,
            count,
            blob,
        }
    }

    #[test]
    fn cuts_follow_claimed_nodes_and_never_split_a_span() {
        let spans = [
            span(10, 19, 10, 0),
            span(20, 29, 10, 1),
            span(30, 39, 10, 2),
            span(40, 49, 10, 3),
        ];
        let f = FamilyLayout::cut(5, 2, 49, &spans);
        assert_eq!(f.cuts, vec![10, 30, 50]);
        assert_eq!(f.buckets(), 2);
        assert_eq!(f.locate_wide(9), None);
        assert_eq!(f.locate_wide(10), Some((5, 0)));
        assert_eq!(f.locate_wide(29), Some((5, 19)));
        assert_eq!(f.locate_wide(30), Some((6, 0)));
        assert_eq!(f.locate_wide(49), Some((6, 19)));
        assert_eq!(f.locate_wide(50), None);
        assert_eq!(f.bounds(1), (30, 50));
        assert_eq!(f.max_local(), 19);
    }

    #[test]
    fn a_heavy_first_span_leaves_later_boundaries_usable() {
        let mut spans = vec![span(0, 1 << 40, 65_536, 0)];
        for i in 0..200u32 {
            let k = (1 << 41) + u64::from(i);
            spans.push(span(k, k, 1, i + 1));
        }
        let f = FamilyLayout::cut(0, 256, (1 << 41) + 199, &spans);
        assert!(f.buckets() > 100, "{}", f.buckets());
        // Touching spans share a boundary key; the right bucket owns it.
        let touching = [span(1, 5, 1, 0), span(5, 9, 1, 1)];
        let t = FamilyLayout::cut(0, 2, 9, &touching);
        assert_eq!(t.cuts, vec![1, 5, 10]);
        assert_eq!(t.locate_wide(5), Some((1, 0)));
        // One span, many buckets allocated: one bucket.
        let one = FamilyLayout::cut(0, 256, u64::MAX >> 1, &[span(0, u64::MAX >> 1, 9, 0)]);
        assert_eq!(one.buckets(), 1);
        assert_eq!(one.locate_wide(u64::MAX >> 1), Some((0, u64::MAX >> 1)));
        assert_eq!(FamilyLayout::cut(0, 4, 0, &[]).buckets(), 0);
    }

    #[test]
    fn the_negative_extreme_fits_the_wide_layout() {
        let spans = [span(0, 1 << 63, 2, 0)];
        let f = FamilyLayout::cut(3, 1, 1 << 63, &spans);
        assert_eq!(f.locate_wide(1 << 63), Some((3, 1 << 63)));
        assert_eq!(f.bounds(0), (0, (1 << 63) + 1));
    }

    #[test]
    fn equal_layouts_route_and_bound_alike() {
        let f = FamilyLayout::equal(128, 4, 1000);
        assert_eq!(f.width(), 250);
        assert_eq!(f.locate_compact(0), Some((128, 0)));
        assert_eq!(f.locate_compact(999), Some((131, 249)));
        assert_eq!(f.locate_compact(1000), Some((131, 250)));
        assert_eq!(f.locate_compact(1001), None);
        assert_eq!(f.bounds(3), (750, 1001));
        for key in 0..=1000u64 {
            let (b, local) = f.locate_compact(key).expect("in range");
            let (lo, hi) = f.bounds(b - 128);
            assert!(lo <= key && key < hi && lo + local == key, "{key}");
        }
        assert!(compact_fits(1 << 38, 256));
        assert!(!compact_fits((1 << 38) + 1, 256));
        assert!(
            !compact_fits(1 << 38, 255),
            "a smaller share narrows the limit"
        );
    }
}
