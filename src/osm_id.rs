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

/// Sort key for OSM element IDs in canonical order.
///
/// Order: 0, then negative IDs by ascending absolute value (-1, -2, -3, ...),
/// then positive IDs (1, 2, 3, ...). Matches libosmium's sort comparator.
///
/// For positive-only IDs (all production PBFs), this is equivalent to plain
/// i64 comparison - the `(2, id)` tuple compares identically to raw `id`.
#[inline]
pub(crate) fn osm_id_key(id: i64) -> (u8, i64) {
    if id > 0 {
        (2, id)
    } else if id == 0 {
        (0, 0)
    } else {
        (1, id.saturating_neg())
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
pub(crate) fn blob_osm_first_key(min_id: i64, max_id: i64) -> (u8, i64) {
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
pub(crate) fn blob_osm_last_key(min_id: i64, max_id: i64) -> (u8, i64) {
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
