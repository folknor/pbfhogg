//! The version-precedence rule both OSC consumers enforce.
//!
//! `apply-changes` (through `CompactDiffOverlay`) and
//! `merge-changes --simplify` both keep the last change per object by
//! input position, so both refuse an incoming change that would move an
//! object backwards. They must answer that question identically: a rule
//! that differs between them means the same input is refused by one and
//! accepted by the other, which is the defect this type exists to stop.
//!
//! The rule: an incoming change that claims a version is admitted only if
//! that version is strictly greater than the highest version ever claimed
//! for the object, deletes included. A change that claims no version is
//! admitted unconditionally and neither raises nor resets the floor;
//! there is nothing to compare, and forgetting the floor because of it
//! would let the next stale change through.
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

    /// Whether a change claiming `claim` may follow this floor. Returns
    /// the floor it failed to advance past on refusal.
    #[inline]
    pub(crate) fn admit(self, claim: Option<i32>) -> Result<(), i32> {
        match (claim, self.0) {
            (Some(claim), Some(floor)) if claim <= floor => Err(floor),
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

/// The refusal both consumers report, so the same input fails with the
/// same explanation whichever command read it. `consumer` names who
/// refused; `remedy` is how that consumer's caller opts out.
pub(crate) fn regression_message(
    consumer: &str,
    kind: &str,
    id: i64,
    claim: i32,
    floor: i32,
    remedy: &str,
) -> String {
    format!(
        "{consumer}: {kind} {id} carries version {claim}, which does not advance past \
         version {floor} already claimed for it in the changes read so far (deletes \
         included). Accepting it would let a repeated or older version replace the object \
         and report success - the usual cause is applying the same diff twice, or OSC \
         inputs listed out of sequence order. Remove the repeated changes or correct their \
         order, or {remedy} if replaying backwards is genuinely intended."
    )
}

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
        assert_eq!(floor.admit(Some(9)), Err(9));
        assert_eq!(floor.admit(Some(8)), Err(9));
        assert_eq!(floor.admit(Some(10)), Ok(()));
    }

    #[test]
    fn zero_and_negative_claims_are_claims() {
        let mut floor = VersionFloor::default();
        floor.raise(Some(-3));
        assert_eq!(floor.admit(Some(-3)), Err(-3));
        assert_eq!(floor.admit(Some(0)), Ok(()));
        floor.raise(Some(0));
        assert_eq!(floor.admit(Some(0)), Err(0));
    }
}
