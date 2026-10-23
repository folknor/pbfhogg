//! The version-precedence rule both OSC consumers enforce.
//!
//! `apply-changes` (through `CompactDiffOverlay`) and
//! `merge-changes --simplify` both keep the last change per object by
//! input position, so both refuse an incoming change that would move an
//! object backwards. They must answer that question identically: a rule
//! that differs between them means the same input is refused by one and
//! accepted by the other, which is the defect this type exists to stop.
//!
//! The rule: an incoming change that claims a version is admitted unless
//! that version is lower than the highest version ever claimed for the
//! object, deletes included. A change that claims no version is
//! admitted unconditionally and neither raises nor resets the floor;
//! there is nothing to compare, and forgetting the floor because of it
//! would let the next stale change through.
//!
//! An equal version is admitted, and the later change wins by position.
//! Version alone cannot order changes that share a version number, and
//! real change streams carry them routinely: regional extract diffs
//! (Geofabrik's, and `diff --format osc` without `--increment-version`)
//! delete an object leaving the region at the version it already had, so
//! `modify v12` in one diff and `delete v12` in the next is a normal
//! sequence, as are a repeated boundary change and an object re-entering
//! the region unchanged. No comparison of the content either consumer
//! retains can tell those apart from a conflict, so the gate does not
//! try: it protects numerical version precedence, and leaves ordering
//! within one version to input order.
//!
//! The two consumers store their history differently, so this type
//! carries the comparison and the refusal text, not the storage.
//! `merge-changes` keeps one `VersionFloor` beside each deduped change.
//! The overlay builds one per check from what it already holds: the live
//! record's version plus a per-id floor map, into which, while the gate
//! is on, every transition that would lose a version folds it first - a
//! delete dropping the live record, and an unversioned create or modify
//! overwriting it. Sharing
//! the comparison alone would not make the two agree; both histories
//! have to be the highest claim ever made, and
//! `tests/osc_parser_parity.rs` holds them to that on the same input.

/// The highest version claimed so far for one object, or none when
/// nothing seen for it ever claimed one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VersionFloor(Option<i32>);

impl VersionFloor {
    /// A floor built from the highest version a consumer already holds.
    #[inline]
    pub(crate) fn new(highest: Option<i32>) -> Self {
        Self(highest)
    }

    /// Whether a change claiming `claim` may follow this floor: refused
    /// only when the claim is lower than the floor. Returns the floor it
    /// fell below on refusal.
    #[inline]
    pub(crate) fn admit(self, claim: Option<i32>) -> Result<(), i32> {
        match (claim, self.0) {
            (Some(claim), Some(floor)) if claim < floor => Err(floor),
            _ => Ok(()),
        }
    }

    /// Fold an admitted change's claim into the floor. An unversioned
    /// change leaves it as it was.
    #[inline]
    pub(crate) fn raise(&mut self, claim: Option<i32>) {
        if let Some(claim) = claim {
            self.0 = Some(self.0.map_or(claim, |floor| floor.max(claim)));
        }
    }
}

// Both consumers refuse with `ErrorKind::VersionRegression`, so the same
// input fails with the same explanation whichever command read it; how a
// caller opts out is the front end's advice to give.

#[cfg(test)]
mod tests {
    use super::VersionFloor;

    #[test]
    fn unversioned_changes_neither_raise_nor_reset() {
        let mut floor = VersionFloor::default();
        assert_eq!(floor.admit(Some(0)), Ok(()));
        floor.raise(Some(9));
        assert_eq!(floor.admit(None), Ok(()));
        floor.raise(None);
        assert_eq!(floor.admit(Some(8)), Err(9));
        assert_eq!(floor.admit(Some(9)), Ok(()));
        assert_eq!(floor.admit(Some(10)), Ok(()));
    }

    /// An equal claim is admitted and leaves the floor where it was, so
    /// a lower claim after it is still refused against the same floor.
    #[test]
    fn equal_claims_are_admitted_without_moving_the_floor() {
        let mut floor = VersionFloor::default();
        floor.raise(Some(12));
        assert_eq!(floor.admit(Some(12)), Ok(()));
        floor.raise(Some(12));
        assert_eq!(floor, VersionFloor::new(Some(12)));
        assert_eq!(floor.admit(Some(11)), Err(12));
    }

    #[test]
    fn zero_and_negative_claims_are_claims() {
        let mut floor = VersionFloor::default();
        floor.raise(Some(-3));
        assert_eq!(floor.admit(Some(-3)), Ok(()));
        assert_eq!(floor.admit(Some(-4)), Err(-3));
        assert_eq!(floor.admit(Some(0)), Ok(()));
        floor.raise(Some(0));
        assert_eq!(floor.admit(Some(0)), Ok(()));
        assert_eq!(floor.admit(Some(-1)), Err(0));
    }
}
