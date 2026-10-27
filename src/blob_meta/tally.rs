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

#[cfg(test)]
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
}
