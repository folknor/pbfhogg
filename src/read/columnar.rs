//! Columnar dense node decode - batch decode IDs, lats, lons into contiguous
//! arrays for cache-friendly classification passes.
//!
//! The PBF wire format already stores dense nodes as three parallel packed
//! delta-encoded arrays (IDs, lats, lons). This module decodes them into
//! contiguous `Vec<i64>` / `Vec<i32>` slices that classification closures
//! can operate on directly, enabling autovectorization and better cache
//! utilization vs element-by-element iteration.

use super::region_grid::RegionGrid;
use super::wire::{PackedSint64Iter, WireDenseNodes};

/// Decoded dense node columns - contiguous arrays of IDs and coordinates.
///
/// All three arrays have the same length (`count`). Coordinates are in
/// decimicrodegrees (10^-7 degrees), matching `DenseNode::decimicro_lat/lon`.
pub(crate) struct DenseNodeColumns {
    /// Absolute node IDs (delta-decoded).
    pub ids: Vec<i64>,
    /// Latitude in decimicrodegrees.
    pub lats: Vec<i32>,
    /// Longitude in decimicrodegrees.
    pub lons: Vec<i32>,
}

impl DenseNodeColumns {
    /// Create empty columns.
    pub fn new() -> Self {
        Self {
            ids: Vec::new(),
            lats: Vec::new(),
            lons: Vec::new(),
        }
    }

    /// Number of nodes.
    ///
    /// The three columns are decoded by three independent loops, so their
    /// lengths are only equal because the wire data said so. Reporting
    /// `ids.len()` made every `for i in 0..len { lats[i]; lons[i] }` below a
    /// panic waiting for a dense group with a truncated (or absent) `lat`
    /// array - reachable from untrusted input via `extract --strategy smart`
    /// and `multi-extract`. Taking the minimum makes positional indexing
    /// total: no length can be out of range for any column.
    ///
    /// This is the panic-proofing, not the correctness guard. A dense group
    /// whose arrays disagree is a hard error at block construction
    /// (`validate_groups_strict` in `block.rs`), so a short column never
    /// reaches here and this `min` never actually truncates anything.
    #[inline]
    pub fn len(&self) -> usize {
        self.ids.len().min(self.lats.len()).min(self.lons.len())
    }

    /// Clear all columns for reuse.
    pub fn clear(&mut self) {
        self.ids.clear();
        self.lats.clear();
        self.lons.clear();
    }

    /// Batch-decode dense nodes from wire format, appending to columnar arrays.
    ///
    /// Delta-decodes IDs, lats, lons and converts coordinates to
    /// decimicrodegrees using the block's granularity and offsets.
    ///
    /// **Does not clear** - call `clear()` before the first group if needed.
    /// This allows multiple dense groups in a single block to be appended.
    ///
    /// The three arrays are decoded independently and their lengths are only
    /// equal because the wire data said so; [`Self::len`] takes the minimum so
    /// that a disagreement cannot become an out-of-bounds index downstream,
    /// and `validate_groups_strict` in `block.rs` rejects the disagreement
    /// outright at block construction.
    #[allow(clippy::cast_possible_truncation)]
    pub fn decode_append(
        &mut self,
        dense: &WireDenseNodes<'_>,
        granularity: i32,
        lat_offset: i64,
        lon_offset: i64,
    ) {
        let gran = i64::from(granularity);

        // Reconstruct signed deltas modulo 2^64, as the dense iterator and
        // writer do. This keeps columnar and element reads identical.
        // Decode IDs (delta-encoded sint64).
        let mut cumulative_id: i64 = 0;
        for delta in PackedSint64Iter::new(dense.id_data) {
            cumulative_id = cumulative_id.wrapping_add(delta);
            self.ids.push(cumulative_id);
        }

        // One scaling for every granularity, the element accessors' own:
        // nanodegrees = offset + granularity * cumulative modulo 2^64, then
        // / 100 truncating toward zero and narrowed by `as i32`. Scaling and
        // offsets wrap rather than being validated (see "What is
        // deliberately not checked" on block construction in `block.rs`),
        // and because this is the accessors' arithmetic step for step, a
        // node classifies at the same coordinate here as in every reader
        // built on the accessors, even when the true value is past `i64`.
        //
        // There is deliberately no granularity-100 shortcut
        // (`cumulative + offset / 100`, skipping the multiply and divide).
        // One existed; it wrapped at a different point from the accessors,
        // so on a block whose true nanodegree value overflows `i64` it put
        // the node somewhere else than a simple extract does, and a matched
        // planet A/B of extract --strategy smart and multi-extract found
        // its removal inside noise (reference/performance.md, "Columnar
        // granularity-100 shortcut").
        let mut cumulative_lat: i64 = 0;
        for delta in PackedSint64Iter::new(dense.lat_data) {
            cumulative_lat = cumulative_lat.wrapping_add(delta);
            let nano = lat_offset.wrapping_add(gran.wrapping_mul(cumulative_lat));
            self.lats.push((nano / 100) as i32);
        }

        let mut cumulative_lon: i64 = 0;
        for delta in PackedSint64Iter::new(dense.lon_data) {
            cumulative_lon = cumulative_lon.wrapping_add(delta);
            let nano = lon_offset.wrapping_add(gran.wrapping_mul(cumulative_lon));
            self.lons.push((nano / 100) as i32);
        }
    }

    /// Classify nodes against N bounding boxes in a single pass, collecting
    /// matching IDs into per-region output Vecs.
    ///
    /// Each bbox is `(min_lat, max_lat, min_lon, max_lon)` in decimicrodegrees.
    /// `out` must have length >= `bboxes.len()`. Single pass over the lat/lon
    /// arrays with N bbox tests per node.
    #[inline]
    pub fn collect_matching_ids_multi_bbox(
        &self,
        bboxes: &[(i32, i32, i32, i32)],
        out: &mut [Vec<i64>],
    ) {
        let n = self.len();
        let lats = &self.lats;
        let lons = &self.lons;
        let ids = &self.ids;

        for i in 0..n {
            let lat = lats[i];
            let lon = lons[i];
            let id = ids[i];
            for (j, &(min_lat, max_lat, min_lon, max_lon)) in bboxes.iter().enumerate() {
                let hit = (lat >= min_lat) as u8
                    & (lat <= max_lat) as u8
                    & (lon >= min_lon) as u8
                    & (lon <= max_lon) as u8;
                if hit != 0 {
                    out[j].push(id);
                }
            }
        }
    }

    /// Grid-pruned counterpart to [`Self::collect_matching_ids_multi_bbox`].
    /// Candidate regions are still checked against their exact integer bbox.
    #[inline]
    pub(crate) fn collect_matching_ids_multi_bbox_grid(
        &self,
        bboxes: &[(i32, i32, i32, i32)],
        grid: &RegionGrid,
        out: &mut [Vec<i64>],
    ) {
        for i in 0..self.len() {
            let lat = self.lats[i];
            let lon = self.lons[i];
            let id = self.ids[i];
            for &region in grid.candidates(lat, lon) {
                let region = region as usize;
                let (min_lat, max_lat, min_lon, max_lon) = bboxes[region];
                let hit = (lat >= min_lat) as u8
                    & (lat <= max_lat) as u8
                    & (lon >= min_lon) as u8
                    & (lon <= max_lon) as u8;
                if hit != 0 {
                    out[region].push(id);
                }
            }
        }
    }

    /// Classify nodes against a bounding box, collecting matching IDs
    /// into a caller-provided Vec (scratch reuse).
    ///
    /// Uses branchless bitwise AND for the 4 comparisons to enable
    /// autovectorization. The comparison loop is a pure function over
    /// contiguous i32 arrays - no data-dependent branches.
    #[inline]
    pub fn collect_matching_ids_bbox(
        &self,
        min_lat: i32,
        max_lat: i32,
        min_lon: i32,
        max_lon: i32,
        out: &mut Vec<i64>,
    ) {
        let n = self.len();
        let lats = &self.lats;
        let lons = &self.lons;
        let ids = &self.ids;

        for i in 0..n {
            // Branchless: bitwise AND instead of short-circuit &&.
            // All four comparisons execute unconditionally - enables
            // autovectorization (no data-dependent branches).
            let hit = (lats[i] >= min_lat) as u8
                & (lats[i] <= max_lat) as u8
                & (lons[i] >= min_lon) as u8
                & (lons[i] <= max_lon) as u8;
            if hit != 0 {
                out.push(ids[i]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::region_grid::RegionGrid;

    #[test]
    fn columnar_deltas_wrap_in_both_coordinate_paths() {
        // Zigzag(MIN), zigzag(-1): absolute values MIN, MAX.
        let deltas = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1, 1];
        let dense = WireDenseNodes {
            id_data: &deltas,
            lat_data: &deltas,
            lon_data: &deltas,
            keys_vals_data: &[],
            info_data: None,
        };
        for granularity in [100, 0] {
            let mut columns = DenseNodeColumns::new();
            columns.decode_append(&dense, granularity, 0, 0);
            assert_eq!(columns.ids, [i64::MIN, i64::MAX]);
            let coords = if granularity == 100 { [0, -1] } else { [0, 0] };
            assert_eq!(columns.lats, coords);
            assert_eq!(columns.lons, coords);
        }
    }

    #[test]
    fn columnar_scaling_adds_offset_before_narrowing() {
        let coordinate = [0xfe, 0xff, 0xff, 0xff, 0x0f]; // zigzag(i32::MAX)
        let dense = WireDenseNodes {
            id_data: &[2],
            lat_data: &coordinate,
            lon_data: &coordinate,
            keys_vals_data: &[],
            info_data: None,
        };
        let mut columns = DenseNodeColumns::new();
        columns.decode_append(&dense, 100, 100, 100);
        assert_eq!(columns.lats, [i32::MIN]);
        assert_eq!(columns.lons, [i32::MIN]);
    }

    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    // The `next(..) as i32` narrowings intentionally fold the u64 RNG stream
    // into a signed coordinate (later reduced mod ~2.1e9), and the i64->i32
    // longitude narrowing stays within the i32 decimicrodegree domain by
    // construction; all casts are lossless for the inputs exercised here.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    #[test]
    fn columnar_grid_parity() {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut columns = DenseNodeColumns::new();
        for id in 0..2_000_i64 {
            columns.ids.push(id);
            columns
                .lats
                .push((next(&mut state) as i32).wrapping_rem(2_100_000_001));
            columns
                .lons
                .push((next(&mut state) as i32).wrapping_rem(2_100_000_001));
        }
        let bboxes: Vec<_> = (0..24_i32)
            .map(|i| {
                let min_lat = -900_000_000 + i * 70_000_000;
                let min_lon = (-1_800_000_000_i64 + i64::from(i) * 140_000_000) as i32;
                (
                    min_lat,
                    min_lat + 120_000_000,
                    min_lon,
                    min_lon + 240_000_000,
                )
            })
            .collect();
        let grid = RegionGrid::build(&bboxes).expect("fixture is within budget");
        let mut linear = vec![Vec::new(); bboxes.len()];
        let mut pruned = vec![Vec::new(); bboxes.len()];
        columns.collect_matching_ids_multi_bbox(&bboxes, &mut linear);
        columns.collect_matching_ids_multi_bbox_grid(&bboxes, &grid, &mut pruned);
        assert_eq!(pruned, linear);
    }

    /// Columns whose `lat`/`lon` arrays are shorter than `ids` must not panic
    /// any classification pass.
    ///
    /// This is the shape a dense group with a truncated (or absent) packed
    /// `lat` field decodes to: three independent decode loops, three different
    /// lengths. Every pass here indexes `lats[i]` / `lons[i]` with `i` drawn
    /// from `len()`, so while `len()` reported `ids.len()` all three were an
    /// index-out-of-bounds panic on untrusted input, reachable from
    /// `extract --strategy smart` and `multi-extract`.
    #[test]
    fn short_coordinate_columns_do_not_panic_any_pass() {
        let mut columns = DenseNodeColumns::new();
        for id in 0..64_i64 {
            columns.ids.push(id);
        }
        // Coordinates for only the first 10 nodes, and one column shorter
        // still - `len()` must respect the shortest of the three.
        for i in 0..10_i32 {
            columns.lats.push(i * 1_000_000);
        }
        for i in 0..7_i32 {
            columns.lons.push(i * 1_000_000);
        }
        // The passes run FIRST, before any assertion about `len()` itself: a
        // structural `assert_eq!(columns.len(), 7)` up here would trip before
        // the indexing loops ever executed, so a regression would surface as a
        // failed length assertion rather than as the panic this test exists to
        // prevent - and a reader would never learn whether the panic was back.
        let bboxes = [(-900_000_000, 900_000_000, -1_800_000_000, 1_800_000_000)];
        let grid = RegionGrid::build(&bboxes).expect("fixture is within budget");

        let mut out = vec![Vec::new()];
        columns.collect_matching_ids_multi_bbox(&bboxes, &mut out);
        assert_eq!(out[0].len(), 7);

        let mut pruned = vec![Vec::new()];
        columns.collect_matching_ids_multi_bbox_grid(&bboxes, &grid, &mut pruned);
        assert_eq!(pruned[0].len(), 7);

        let mut single = Vec::new();
        columns.collect_matching_ids_bbox(
            -900_000_000,
            900_000_000,
            -1_800_000_000,
            1_800_000_000,
            &mut single,
        );
        assert_eq!(single.len(), 7);

        assert_eq!(columns.len(), 7, "len() must be the shortest column");
    }
}
