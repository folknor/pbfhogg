//! OSM element ID value semantics: canonical sort order and blob-range key
//! derivation.
//!
//! OSM IDs are signed `i64`. The canonical osmium sort order is `0` first,
//! then negative IDs by ascending absolute value (`-1, -2, -3, ...`), then
//! positive IDs (`1, 2, 3, ...`). For positive-only IDs (all production
//! PBFs), this collapses to plain `i64` comparison.
//!
//! Distinct from the membership/cardinality concerns in [`crate::idset`]:
//! this module is pure value semantics, no data structure.

/// Canonical-order sort key: `(class, magnitude)`, class 0 for zero, 1 for
/// negative, 2 for positive.
pub(crate) type OsmIdKey = (u8, u64);

/// Sort key for OSM element IDs in canonical order.
///
/// Order: 0, then negative IDs by ascending absolute value (-1, -2, -3, ...),
/// then positive IDs (1, 2, 3, ...). Matches libosmium's sort comparator.
///
/// For positive-only IDs (all production PBFs), this is equivalent to plain
/// i64 comparison - the `(2, id)` tuple compares identically to raw `id`.
///
/// The magnitude is `unsigned_abs`, so the key is injective: every distinct
/// ID gets a distinct key. A saturating negation used to map both
/// `i64::MIN` and `-i64::MAX` to the same key, and a merge-join keyed on it
/// paired two different elements as the same object.
#[inline]
pub(crate) fn osm_id_key(id: i64) -> OsmIdKey {
    if id > 0 {
        (2, id.unsigned_abs())
    } else if id == 0 {
        (0, 0)
    } else {
        (1, id.unsigned_abs())
    }
}

/// The ID a key was derived from. Inverse of [`osm_id_key`].
#[inline]
// The casts reverse `unsigned_abs`; magnitude 2^63 maps back to i64::MIN.
#[allow(clippy::cast_possible_wrap)]
pub(crate) fn osm_id_from_key(key: OsmIdKey) -> i64 {
    match key.0 {
        0 => 0,
        // Magnitude 2^63 is i64::MIN, which wrapping_neg maps to itself.
        1 => (key.1 as i64).wrapping_neg(),
        _ => key.1 as i64,
    }
}

/// Compare two OSM element IDs in canonical sort order.
#[inline]
pub(crate) fn osm_id_cmp(a: i64, b: i64) -> std::cmp::Ordering {
    osm_id_key(a).cmp(&osm_id_key(b))
}

/// OSM-order "first" key for a blob's numeric ID range.
///
/// Used by blob-level sort to determine blob ordering. Conservative for
/// mixed-sign ranges (assumes 0 is present).
#[inline]
pub(crate) fn blob_osm_first_key(min_id: i64, max_id: i64) -> OsmIdKey {
    if min_id >= 0 {
        osm_id_key(min_id)
    } else if max_id <= 0 {
        osm_id_key(max_id)
    } else {
        osm_id_key(0)
    }
}

/// OSM-order "last" key for a blob's numeric ID range.
///
/// Used by blob-level overlap detection.
#[inline]
pub(crate) fn blob_osm_last_key(min_id: i64, max_id: i64) -> OsmIdKey {
    if min_id >= 0 {
        osm_id_key(max_id)
    } else if max_id <= 0 {
        osm_id_key(min_id)
    } else {
        osm_id_key(max_id)
    }
}

/// The ID of the "first" element of a blob in OSM sort order.
///
/// For positive-only blobs, this is min_id. For negative-only blobs,
/// this is max_id (closest to 0). For mixed blobs, conservatively 0.
#[inline]
pub(crate) fn blob_osm_first_id(min_id: i64, max_id: i64) -> i64 {
    if min_id >= 0 {
        min_id
    } else if max_id <= 0 {
        max_id
    } else {
        0
    }
}

/// The ID of the "last" element of a blob in OSM sort order.
///
/// Mirror of [`blob_osm_first_id`]: for positive-only blobs this is
/// `max_id`, for negative-only blobs `min_id` (furthest from 0), and for
/// mixed-sign blobs `max_id` (the positives sort last). Kept id-valued
/// rather than key-valued so error messages can name a real OSM ID.
#[inline]
pub(crate) fn blob_osm_last_id(min_id: i64, max_id: i64) -> i64 {
    if max_id <= 0 && min_id < 0 {
        min_id
    } else {
        max_id
    }
}

/// The smallest OSM-order key of an element a blob's numeric range proves
/// present, for order checks.
///
/// Not [`blob_osm_first_key`]: that one is a lower bound on the blob's first
/// element (key 0 for a mixed-sign range, which may or may not hold 0), the
/// right direction for overlap tests and the wrong one for refusing a file.
/// An order check refuses only disorder the metadata proves, so it needs an
/// element the blob certainly holds. A range's `min_id` and `max_id` are both
/// present (indexdata ranges are exact), and for a mixed-sign range the
/// smaller of their keys is `min_id`'s, a negative: blob `[-1]` followed by
/// blob `[-2, 1]` (holding -2 and 1) is valid and passes, where the lower
/// bound would have refused it. What a mixed-sign range cannot show is a 0 or
/// a small negative inside it sorting before the previous blob's last
/// element; that disorder is invisible to metadata, like disorder within a
/// blob. Single-sign ranges are exact either way.
#[inline]
pub(crate) fn blob_osm_present_first_key(min_id: i64, max_id: i64) -> OsmIdKey {
    if min_id < 0 && max_id > 0 {
        osm_id_key(min_id)
    } else {
        blob_osm_first_key(min_id, max_id)
    }
}

/// A blob [`OrderWatermark::observe`] refused: `first_id` is the element the
/// blob holds that sorts before `previous_id`, the furthest element already
/// observed, of kind ordinal `previous_kind`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OrderViolation {
    pub(crate) previous_kind: u8,
    pub(crate) previous_id: i64,
    pub(crate) first_id: i64,
}

/// Running canonical-order bound shared by sorted input and output guards.
///
/// Refuses only disorder the ranges prove (see
/// [`blob_osm_present_first_key`]); every valid layout passes, including
/// mixed-sign ones.
#[derive(Default)]
pub(crate) struct OrderWatermark {
    last: Option<(u8, OsmIdKey)>,
}

impl OrderWatermark {
    /// Equality is allowed: duplicate IDs are a separate validation concern.
    pub(crate) fn observe(
        &mut self,
        kind: u8,
        min_id: i64,
        max_id: i64,
    ) -> Result<(), OrderViolation> {
        let first = blob_osm_present_first_key(min_id, max_id);
        let mut last = blob_osm_last_key(min_id, max_id).max(first);
        if let Some((previous_kind, previous_last)) = self.last {
            if kind < previous_kind || (kind == previous_kind && first < previous_last) {
                return Err(OrderViolation {
                    previous_kind,
                    previous_id: osm_id_from_key(previous_last),
                    first_id: osm_id_from_key(first),
                });
            }
            if kind == previous_kind {
                last = last.max(previous_last);
            }
        }
        self.last = Some((kind, last));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn violation(previous_kind: u8, previous_id: i64, first_id: i64) -> OrderViolation {
        OrderViolation {
            previous_kind,
            previous_id,
            first_id,
        }
    }

    #[test]
    fn watermark_handles_kinds_equal_bounds_and_signed_ids() {
        let mut guard = OrderWatermark::default();
        assert_eq!(guard.observe(0, -2, -1), Ok(()));
        assert_eq!(guard.observe(0, -3, -2), Ok(()));
        assert_eq!(guard.observe(0, -1, -1), Err(violation(0, -3, -1)));
        assert_eq!(guard.observe(0, 1, 30), Ok(()));
        assert_eq!(guard.observe(0, 5, 8), Err(violation(0, 30, 5)));
        assert_eq!(guard.observe(0, 30, 40), Ok(()));
        assert_eq!(guard.observe(1, 1, 2), Ok(()));
        assert_eq!(guard.observe(0, 100, 200), Err(violation(1, 2, 100)));
    }

    /// Every valid mixed-sign layout passes, and a mixed-sign range is
    /// refused exactly when an element it certainly holds sorts before the
    /// furthest element already seen.
    #[test]
    fn watermark_accepts_valid_mixed_sign_layouts() {
        // [-1] then [-2, 1] holding -2 and 1: canonical -1, -2, 1.
        let mut guard = OrderWatermark::default();
        assert_eq!(guard.observe(0, -1, -1), Ok(()));
        assert_eq!(guard.observe(0, -2, 1), Ok(()));
        assert_eq!(guard.observe(0, 2, 5), Ok(()));
        // A range ending at 0 starts at 0, exactly.
        let mut guard = OrderWatermark::default();
        assert_eq!(guard.observe(0, -5, 0), Ok(()));
        assert_eq!(guard.observe(0, -9, -6), Ok(()));
        assert_eq!(guard.observe(0, -10, 3), Ok(()));
        // Extremes: magnitude 2^63 is the furthest negative.
        let mut guard = OrderWatermark::default();
        assert_eq!(guard.observe(0, -i64::MAX, -1), Ok(()));
        assert_eq!(guard.observe(0, i64::MIN, i64::MAX), Ok(()));
        // Proven disorder: -3 is held and sorts before -5 already seen.
        let mut guard = OrderWatermark::default();
        assert_eq!(guard.observe(0, -5, -1), Ok(()));
        assert_eq!(guard.observe(0, -3, 4), Err(violation(0, -5, -3)));
        // And after positives, any mixed-sign range is disorder.
        let mut guard = OrderWatermark::default();
        assert_eq!(guard.observe(0, 1, 10), Ok(()));
        assert_eq!(guard.observe(0, -100, 20), Err(violation(0, 10, -100)));
    }

    /// Distinct IDs must never share a key, including at the extremes where
    /// a saturating negation used to fold `i64::MIN` onto `-i64::MAX`, and
    /// the key must round-trip to the ID it came from.
    #[test]
    fn osm_id_key_is_injective_and_round_trips() {
        let ids = [i64::MIN, -i64::MAX, -2, -1, 0, 1, 2, i64::MAX - 1, i64::MAX];
        for (i, &a) in ids.iter().enumerate() {
            assert_eq!(osm_id_from_key(osm_id_key(a)), a, "round trip of {a}");
            for &b in &ids[i + 1..] {
                assert_ne!(osm_id_key(a), osm_id_key(b), "{a} and {b} share a key");
            }
        }
        assert_eq!(osm_id_cmp(-1, -i64::MAX), std::cmp::Ordering::Less);
        assert_eq!(osm_id_cmp(-i64::MAX, i64::MIN), std::cmp::Ordering::Less);
        assert_eq!(osm_id_cmp(i64::MIN, 1), std::cmp::Ordering::Less);
    }

    /// The id-valued and key-valued blob-bound helpers must agree: callers
    /// mix them (an error message names the ID while the comparison uses the
    /// key), and a disagreement would put the two on different elements.
    /// Covers positive-only, negative-only, mixed-sign and degenerate spans.
    #[test]
    fn blob_bound_id_and_key_helpers_agree() {
        let spans = [
            (1_i64, 100_i64),
            (0, 0),
            (0, 100),
            (-100, -1),
            (-100, 0),
            (-100, 100),
            (5, 5),
            (-5, -5),
            (i64::MIN + 1, i64::MAX),
        ];
        for (min_id, max_id) in spans {
            assert_eq!(
                osm_id_key(blob_osm_first_id(min_id, max_id)),
                blob_osm_first_key(min_id, max_id),
                "first bound disagrees for span ({min_id}, {max_id})"
            );
            assert_eq!(
                osm_id_key(blob_osm_last_id(min_id, max_id)),
                blob_osm_last_key(min_id, max_id),
                "last bound disagrees for span ({min_id}, {max_id})"
            );
            assert!(
                blob_osm_first_key(min_id, max_id) <= blob_osm_last_key(min_id, max_id),
                "first must not sort after last for span ({min_id}, {max_id})"
            );
        }
    }
}
