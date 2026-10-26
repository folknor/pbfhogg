//! `check` command: integrity checks over a PBF file.
//!
//! Four modes selected by CLI flags:
//! - `--refs`: referential integrity (way refs, relation members) via [`refs::check_refs`].
//! - `--ids --full`: ID uniqueness and ordering via [`verify_ids::verify_ids`].
//! - `--index`: blob-level sidecar metadata (`indexdata`, `tagdata`) against
//!   the blobs themselves, via [`index::check_index`]. Opt-in rather than part
//!   of the default pair, because it decompresses the whole file.
//! - `--locations`: a LocationsOnWays file's inline way-node coordinates
//!   against its node records, via [`locations::check_locations`]. Opt-in for
//!   the same reason, and because it only applies to such files.

pub mod index;
pub mod locations;
pub mod refs;
pub mod verify_ids;

/// Options shared by the ID, index, reference and location checks.
#[derive(Debug, Clone, Copy)]
pub struct CheckOptions {
    /// Maximum number of detail entries to retain in this check.
    /// Zero means unlimited. This bounds the report's detail list, not the
    /// check's working memory: exact counting and deferred reference checks
    /// retain intermediate data independently of this display limit.
    pub max_errors: usize,
    /// Read the input with `O_DIRECT` where the check supports it: the index
    /// check's reads and the ID check's front read. The reference and
    /// location checks, and the ID check's blob reads, are parallel preads of
    /// unaligned blob ranges that ignore it.
    pub direct_io: bool,
}

// ---------------------------------------------------------------------------
// ID-space allocation floors
// ---------------------------------------------------------------------------
//
// Both check modes populate an `IdSet` that cannot grow once the parallel
// phase starts, so its size has to be chosen up front. These are *floors*, not
// ceilings: callers take `max(indexdata_max_id, floor)` so allocation tracks
// the file while retaining a minimum range for small files. A file-derived
// ceiling below caps indexdata-driven allocation above those per-kind floors.

use crate::tunables::CHECK_IDS_PER_BYTE;

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
/// the per-kind minimum above. `IdSet` costs one bit per ID, so `file_len`
/// bytes of input buy `file_len * 8` IDs of range, so a 4 KB file can no
/// longer request an exabyte. That ceiling covers a full planet, whose IDs
/// are dense, but not a regional extract: an extract keeps global IDs, and
/// renumber measured a country extract's highest node ID at well over 8 per
/// input byte. For those files it is the per-kind floor that covers the real
/// ID range, so a floor that falls behind live OSM IDs makes this function
/// clamp preallocation for honest extracts. The arithmetic is shared with
/// renumber and the geocode builder via `IdSet::file_bounded_max_id`.
///
/// This is an allocation bound, not an input validity bound. Both callers
/// collect observed positive IDs refused by atomic setters and insert them
/// with `IdSet::set` (or `set_if_new`) after workers join. Only those decoded
/// IDs can allocate sparse overflow chunks; unverified indexdata cannot.
pub(super) fn id_alloc_bound(indexdata_max: i64, floor: i64, file_len: u64) -> i64 {
    crate::idset::IdSet::file_bounded_max_id(indexdata_max, file_len, CHECK_IDS_PER_BYTE).max(floor)
}

/// Insert the positive IDs a parallel phase found above its pre-allocation
/// (`ids`, drained) into `set`. Test builds count them on the run's token.
pub(super) fn insert_overflow_ids(
    set: &mut crate::idset::IdSet,
    ids: &mut Vec<i64>,
    host: &crate::host::Host,
) {
    #[cfg(feature = "test-hooks")]
    host.cancel
        .faults()
        .check_overflow_ids
        .fetch_add(ids.len() as u64, std::sync::atomic::Ordering::Relaxed);
    #[cfg(not(feature = "test-hooks"))]
    let _ = host;
    for id in ids.drain(..) {
        set.set(id);
    }
}

#[cfg(test)]
mod tests {
    use super::id_alloc_bound;
    use crate::tunables::{NODE_ID_FLOOR, RELATION_ID_FLOOR, WAY_ID_FLOOR};

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
