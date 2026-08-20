//! `check` command: integrity checks over a PBF file.
//!
//! Two modes selected by CLI flags:
//! - `--refs`: referential integrity (way refs, relation members) via [`refs::check_refs`].
//! - `--ids --full`: ID uniqueness and ordering via [`verify_ids::verify_ids`].

pub mod refs;
pub mod verify_ids;

// ---------------------------------------------------------------------------
// ID-space allocation floors
// ---------------------------------------------------------------------------
//
// Both check modes populate an `IdSet` that cannot grow once the parallel
// phase starts, so its size has to be chosen up front. These are *floors*, not
// ceilings: callers take `max(indexdata_max_id, floor)` so the allocation
// tracks the file, and treat an ID beyond that as a reportable input property
// rather than a panic. A bare hard-coded ceiling was wrong on its own terms -
// live OSM way IDs are already ~1.4e9 against the old 1.5e9 cap, so the
// passage of time alone would have turned `check` into a crash.
//
// The floors keep small files from allocating a tiny bitset that a
// slightly-stale index would immediately overshoot; they are approximately the
// live OSM ID space rounded up.

/// Node ID-space floor (~14e9; live OSM is ~13e9).
pub(super) const NODE_ID_FLOOR: i64 = 14_000_000_000;

/// Way ID-space floor (~2e9; live OSM is ~1.4e9).
pub(super) const WAY_ID_FLOOR: i64 = 2_000_000_000;

/// Relation ID-space floor (~25e6; live OSM is ~18e6).
pub(super) const RELATION_ID_FLOOR: i64 = 25_000_000;

/// Choose the `IdSet::pre_allocate` argument for one element kind.
///
/// Sizing straight from `indexdata_max` was the point of dropping the
/// hard-coded ceilings, but indexdata is not verified anywhere, so the number
/// is attacker-controlled: a blob header claiming `max_id = i64::MAX` asks
/// `pre_allocate` for a chunk vector covering 2^63 IDs, and it populates every
/// chunk eagerly. That aborts or OOMs the process *before* the checked
/// setters, the very fix that made an overstated ID reportable, ever run.
/// Guarding understated metadata while trusting overstated metadata just moves
/// the failure one layer up.
///
/// The bound is derived from the input file rather than reinstated as a
/// constant, so it still tracks the file the way the original fix intended:
/// **the ID bitset may never exceed the size of the input file**, floored at
/// the live-OSM ID space above. `IdSet` costs one bit per ID, so `file_len`
/// bytes of input buy `file_len * 8` IDs of range. That is generous by orders
/// of magnitude against any real PBF (a PBF spends several bytes per element
/// even packed and compressed, and IDs are dense), so no honest file is ever
/// clamped, while a 4 KB file can no longer request an exabyte.
///
/// Clamping is safe rather than lossy: an ID above the chosen bound is not
/// recorded, and both callers already treat that as a reportable input
/// property (`IdViolation::IdOutOfRange` in `verify_ids`, an over-reported
/// missing ref in `check_refs`) instead of a panic.
pub(super) fn id_alloc_bound(indexdata_max: i64, floor: i64, file_len: u64) -> i64 {
    let ceiling = i64::try_from(file_len.saturating_mul(8))
        .unwrap_or(i64::MAX)
        .max(floor);
    indexdata_max.clamp(0, ceiling).max(floor)
}

#[cfg(test)]
mod tests {
    use super::{NODE_ID_FLOOR, RELATION_ID_FLOOR, WAY_ID_FLOOR, id_alloc_bound};

    #[test]
    fn an_overstated_indexdata_max_id_cannot_request_an_unbounded_allocation() {
        // The denial-of-service shape: a 4 KB file whose blob indexdata claims
        // the node ID space runs to i64::MAX. Unclamped, `pre_allocate` would
        // build and populate a chunk vector covering 2^63 IDs - roughly an
        // exabyte of bitset - and die on capacity overflow or the OOM killer
        // before a single element was read.
        assert_eq!(
            id_alloc_bound(i64::MAX, NODE_ID_FLOOR, 4096),
            NODE_ID_FLOOR,
            "an indexdata max_id far beyond what a 4 KB file could contain \
             must be clamped to the floor, not honoured"
        );

        // Passing counterpart, and the reason the bound is derived from the
        // file instead of being another hard-coded ceiling: a real
        // planet-scale max above every floor must survive untouched. A fix
        // that simply reinstated a constant cap fails here.
        let planet_bytes = 80_u64 * 1024 * 1024 * 1024;
        assert_eq!(
            id_alloc_bound(60_000_000_000, NODE_ID_FLOOR, planet_bytes),
            60_000_000_000,
            "a node max_id well past the floor is exactly what the floors were \
             dropped for; an 80 GB file must be allowed to declare it"
        );

        // The original bug this sizing existed to fix, still fixed: a file
        // reporting no usable max (or one under the floor) gets the floor.
        assert_eq!(id_alloc_bound(0, WAY_ID_FLOOR, 4096), WAY_ID_FLOOR);
        assert_eq!(
            id_alloc_bound(-1, RELATION_ID_FLOOR, 4096),
            RELATION_ID_FLOOR
        );
    }

    #[test]
    fn the_ceiling_scales_with_the_file_rather_than_being_fixed() {
        // Same overstated claim, two file sizes: the larger file earns a
        // larger allowance. This is what distinguishes the chosen policy from
        // a constant, and it fails if the ceiling stops depending on file_len.
        let small = id_alloc_bound(i64::MAX, 0, 1_000_000);
        let large = id_alloc_bound(i64::MAX, 0, 1_000_000_000);
        assert_eq!(small, 8_000_000);
        assert_eq!(large, 8_000_000_000);

        // And the saturating conversion holds for an absurd file length
        // rather than overflowing.
        assert_eq!(id_alloc_bound(i64::MAX, 0, u64::MAX), i64::MAX);
    }
}
