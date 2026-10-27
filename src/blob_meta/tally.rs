//! What a complete walk of one decoded blob's elements saw of its IDs, for
//! comparing with the blob's indexdata claim (reference/indexdata-trust.md,
//! "Range, count and order claims refused at decode").
//!
//! A command that already visits every element of a blob it routed on the
//! claim adds each ID here (an increment and a branchless min and max) and
//! asks at the end whether the claim was exact. The kind half of the claim
//! is `PrimitiveBlock::refuse_foreign_kind`'s, so a tally holds the IDs of
//! one kind.

use super::BlobIndex;
use crate::error::{Error, Result};

/// The count and numeric ID extremes of the elements a walk has added.
#[derive(Clone, Copy, Debug)]
pub(crate) struct IdTally {
    count: u64,
    min: i64,
    max: i64,
}

impl Default for IdTally {
    fn default() -> Self {
        Self {
            count: 0,
            min: i64::MAX,
            max: i64::MIN,
        }
    }
}

impl IdTally {
    /// Add one element's ID.
    #[inline]
    pub(crate) fn add(&mut self, id: i64) {
        self.count += 1;
        self.min = self.min.min(id);
        self.max = self.max.max(id);
    }

    /// The tally of a walk that counted `count` elements with numeric ID
    /// extremes `min` and `max` itself (a reframe that tracks them for the
    /// index it writes). `min` and `max` are ignored when `count` is 0.
    pub(crate) fn of(count: u64, min: i64, max: i64) -> Self {
        if count == 0 {
            return Self::default();
        }
        Self { count, min, max }
    }

    /// Elements added so far.
    pub(crate) fn count(&self) -> u64 {
        self.count
    }

    /// Whether an ID added so far lies outside the claimed range: the claim
    /// is already false, whatever the rest of the blob holds.
    pub(crate) fn escapes(&self, claim: &BlobIndex) -> bool {
        self.count != 0 && (self.min < claim.min_id || self.max > claim.max_id)
    }

    /// Refuse `claim` unless it is exact for the walk just completed: the
    /// same count and the same numeric `min_id` and `max_id`, the rule
    /// `check --index` applies to those fields. A blob with no element is
    /// refused under any claim, since no index describes one and pbfhogg
    /// never writes one. `IndexdataMismatch`; the caller locates it.
    pub(crate) fn refuse_inexact(&self, claim: &BlobIndex) -> Result<()> {
        let exact = self.count != 0
            && self.count == claim.count
            && self.min == claim.min_id
            && self.max == claim.max_id;
        if exact {
            return Ok(());
        }
        Err(self.mismatch(claim, true))
    }

    /// The `IndexdataMismatch` naming what the claim says and what the walk
    /// found: the whole blob when `complete`, else the elements met before
    /// the walk stopped (an ID outside the claimed range refuses the claim
    /// whatever follows).
    #[cold]
    pub(crate) fn mismatch(&self, claim: &BlobIndex, complete: bool) -> Error {
        let found = if self.count == 0 {
            "no element".to_owned()
        } else {
            format!(
                "{} {} elements with ids {}..={}",
                self.count, claim.kind, self.min, self.max
            )
        };
        let seen = if complete {
            "the decoded blob holds"
        } else {
            "the decoded blob's first elements include"
        };
        crate::error::new_indexdata_mismatch(format!(
            "blob indexdata claims {} {} elements with ids {}..={} but {seen} \
             {found}, and this run places and counts the blob by its claim",
            claim.count, claim.kind, claim.min_id, claim.max_id
        ))
    }
}

/// Whether a [`ClaimWalk`] also holds the blob to the header's
/// `Sort.Type_then_ID` order, and who to name when it does not.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Order {
    /// Every element's ID must not sort below its predecessor's in canonical
    /// OSM order, else `ErrorKind::Unsorted`. The text opens the refusal,
    /// naming the command and what the order is needed for.
    Required(&'static str),
    /// The walk checks the claim only: the run does not depend on order.
    Free,
}

/// One complete walk of a decoded blob's elements, checked against the
/// blob's indexdata claim and, when asked, the header's order claim as it
/// goes.
///
/// Each element's ID goes to [`observe`](Self::observe) (an increment and a
/// branchless min and max, and under [`Order::Required`] one key compare
/// with the previous ID), and the complete walk ends in
/// [`finish`](Self::finish):
///
/// - a claim that is not exact for the walk is `IndexdataMismatch`
///   ([`IdTally::refuse_inexact`]: a blob with no element under a claim is
///   refused too), also when the walk breaks order on an ID the claim does
///   not cover, since that makes the claim the false one;
/// - otherwise an ID sorting below its predecessor is `Unsorted`.
///
/// An unindexed blob (`--force`) makes no claim and is checked for order
/// only. The claimed kind is checked apart, once per blob
/// ([`KindSet::refuse_under`](crate::element_kind::KindSet::refuse_under)),
/// so every ID a walk observes is of one kind.
pub(crate) struct ClaimWalk<'c> {
    claim: Option<&'c BlobIndex>,
    order: Order,
    tally: IdTally,
    /// The first ID observed, valid once `tally` has counted one.
    first: i64,
    previous: crate::osm_id::OsmIdKey,
}

impl<'c> ClaimWalk<'c> {
    /// A walk of a blob carrying `claim` (`None` for an unindexed blob).
    pub(crate) fn new(claim: Option<&'c BlobIndex>, order: Order) -> Self {
        Self {
            claim,
            order,
            tally: IdTally::default(),
            first: 0,
            previous: crate::osm_id::osm_id_key(0),
        }
    }

    /// The next element's ID, in iteration order.
    #[inline]
    pub(crate) fn observe(&mut self, id: i64) -> Result<()> {
        if let Order::Required(_) = self.order {
            let key = crate::osm_id::osm_id_key(id);
            // Key 0 is the least key, so the first element always passes.
            if key < self.previous {
                return Err(self.disorder(id));
            }
            self.previous = key;
        }
        if self.tally.count() == 0 {
            self.first = id;
        }
        self.tally.add(id);
        Ok(())
    }

    /// The blob's first element in iteration order, once one has been
    /// observed.
    pub(crate) fn first(&self) -> Option<i64> {
        (self.tally.count() != 0).then_some(self.first)
    }

    /// What the walk has counted so far, for a caller that holds the claim
    /// apart from the walk (a pool whose worker cannot see its position).
    pub(crate) fn tally(&self) -> IdTally {
        self.tally
    }

    /// End a complete walk: the claim, when there is one, must be exact.
    /// Returns the first element's ID, `None` for an unindexed blob with no
    /// element (an indexed one is refused).
    pub(crate) fn finish(&self) -> Result<Option<i64>> {
        if let Some(claim) = self.claim {
            self.tally.refuse_inexact(claim)?;
        }
        Ok(self.first())
    }

    #[cold]
    fn disorder(&self, id: i64) -> Error {
        if let Some(claim) = self.claim {
            let mut seen = self.tally;
            seen.add(id);
            if seen.escapes(claim) {
                return seen.mismatch(claim, false);
            }
        }
        let context = match self.order {
            Order::Required(context) => context,
            Order::Free => "",
        };
        crate::error::new_unsorted(format!(
            "{context} - a blob holds id {id} after id {}, out of the order its header \
             declares",
            crate::osm_id::osm_id_from_key(self.previous)
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::ElementKind;

    fn claim(min_id: i64, max_id: i64, count: u64) -> BlobIndex {
        BlobIndex {
            kind: ElementKind::Node,
            min_id,
            max_id,
            count,
            bbox: None,
        }
    }

    fn tally(ids: &[i64]) -> IdTally {
        let mut t = IdTally::default();
        for &id in ids {
            t.add(id);
        }
        t
    }

    /// Exact on count, min and max in either order, mixed signs included;
    /// every other claim, overstated, understated or empty, is refused.
    #[test]
    fn a_claim_is_exact_or_refused() {
        assert!(tally(&[10, 20]).refuse_inexact(&claim(10, 20, 2)).is_ok());
        assert!(tally(&[-1, -5, 1]).refuse_inexact(&claim(-5, 1, 3)).is_ok());
        for (ids, c) in [
            (&[10, 20][..], claim(5, 20, 2)),
            (&[10, 20][..], claim(10, 40, 2)),
            (&[10, 30][..], claim(10, 20, 2)),
            (&[10, 20][..], claim(10, 20, 3)),
            (&[][..], claim(0, 0, 0)),
        ] {
            let err = tally(ids).refuse_inexact(&c).expect_err("inexact claim");
            assert!(
                matches!(err.kind(), crate::ErrorKind::IndexdataMismatch { .. }),
                "{ids:?} under {c:?}: {err}"
            );
        }
    }

    #[test]
    fn escape_is_any_id_outside_the_claimed_range() {
        assert!(!tally(&[]).escapes(&claim(10, 20, 1)));
        assert!(!tally(&[10, 15]).escapes(&claim(10, 20, 3)));
        assert!(tally(&[9]).escapes(&claim(10, 20, 3)));
        assert!(tally(&[21]).escapes(&claim(10, 20, 3)));
    }

    fn walk(ids: &[i64], c: Option<&BlobIndex>, order: Order) -> Result<Option<i64>> {
        let mut w = ClaimWalk::new(c, order);
        for &id in ids {
            w.observe(id)?;
        }
        w.finish()
    }

    /// Order is refused only when required; a free walk checks the claim
    /// alone. A descent onto an ID the claim does not cover is the claim's
    /// fault, so it is the mismatch, not the disorder.
    #[test]
    fn a_walk_refuses_disorder_only_when_order_is_required() {
        let required = Order::Required("test");
        let honest = claim(1, 3, 3);
        assert_eq!(walk(&[1, 2, 3], Some(&honest), required).unwrap(), Some(1));
        let err = walk(&[2, 1, 3], Some(&honest), required).unwrap_err();
        assert!(
            matches!(err.kind(), crate::ErrorKind::Unsorted { .. }),
            "{err}"
        );
        assert_eq!(
            walk(&[2, 1, 3], Some(&honest), Order::Free).unwrap(),
            Some(2)
        );
        // Equal IDs are in order (duplicates are another concern).
        assert!(walk(&[1, 1, 3], Some(&honest), required).is_ok());
        // Canonical order: negatives by magnitude after zero.
        assert!(walk(&[-1, -2], Some(&claim(-2, -1, 2)), required).is_ok());
        let err = walk(&[2, 0, 3], Some(&honest), required).unwrap_err();
        assert!(
            matches!(err.kind(), crate::ErrorKind::IndexdataMismatch { .. }),
            "{err}"
        );
        // No claim: order only, and an empty walk is accepted.
        assert_eq!(walk(&[], None, required).unwrap(), None);
        // A claim over no element is refused.
        assert!(walk(&[], Some(&claim(0, 0, 0)), Order::Free).is_err());
    }
}
