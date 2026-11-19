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

/// A coordinate in decimicrodegrees from its absolute wire value (a dense
/// group's cumulative delta sum, or a plain node's `lat` / `lon`): the
/// element accessors' arithmetic step for step (`nano_lat` then
/// `decimicro_lat`), nanodegrees = offset + granularity * value modulo
/// 2^64, then / 100 truncating toward zero and narrowed by `as i32`.
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn decimicro(offset: i64, granularity: i64, value: i64) -> i32 {
    (offset.wrapping_add(granularity.wrapping_mul(value)) / 100) as i32
}

/// Refuse an axis of a dense group unless the cumulative values at both of
/// its ends scale to an `i32` decimicrodegree; see
/// [`DenseNodeColumns::decode_append`] for why the ends decide the group.
#[inline]
fn representable(offset: i64, granularity: i64, min: i64, max: i64) -> crate::error::Result<()> {
    use crate::scan::node::{scale_to_decimicro, unrepresentable_coordinate_err};
    if scale_to_decimicro(offset, granularity, min).is_some()
        && scale_to_decimicro(offset, granularity, max).is_some()
    {
        Ok(())
    } else {
        Err(unrepresentable_coordinate_err())
    }
}

/// A dense group's coordinates in decimicrodegrees, in node order, decoded
/// from its validated `lat` and `lon` arrays as [`DenseNodeColumns::decode_append`]
/// and the dense element iterator decode them. The validator holds the two
/// arrays to one length; the zip would stop at the shorter otherwise.
pub(crate) fn dense_decimicro_coords<'a>(
    lat_data: &'a [u8],
    lon_data: &'a [u8],
    granularity: i32,
    lat_offset: i64,
    lon_offset: i64,
) -> impl Iterator<Item = (i32, i32)> + 'a {
    let gran = i64::from(granularity);
    let (mut lat, mut lon) = (0_i64, 0_i64);
    PackedSint64Iter::new(lat_data)
        .zip(PackedSint64Iter::new(lon_data))
        .map(move |(dlat, dlon)| {
            lat = lat.wrapping_add(dlat);
            lon = lon.wrapping_add(dlon);
            (
                decimicro(lat_offset, gran, lat),
                decimicro(lon_offset, gran, lon),
            )
        })
}

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
    /// (`validate_block` in `block.rs`), so a short column never
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
    /// and `validate_block` in `block.rs` rejects the disagreement
    /// outright at block construction.
    ///
    /// The coordinates are checked, exactly as the node scanner's
    /// `scale_to_decimicro` checks them, and a group holding a coordinate
    /// whose exact value does not fit an `i32` decimicrodegree is refused
    /// with the scanner's wire error (the columns are then partial and must
    /// not be used). The check costs no per-node branch: the loops track the
    /// minimum and maximum cumulative value of each axis, and only those two
    /// endpoints are scaled exactly. `offset + granularity * raw` is monotone
    /// in `raw`, so if both endpoints are representable every node of the
    /// group is, its exact nanodegree value then fits `i64`, and the wrapping
    /// arithmetic below already produced it, cancelling intermediates
    /// included. If an endpoint is not representable, that node is not.
    pub fn decode_append(
        &mut self,
        dense: &WireDenseNodes<'_>,
        granularity: i32,
        lat_offset: i64,
        lon_offset: i64,
    ) -> crate::error::Result<()> {
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
        // / 100 truncating toward zero and narrowed by `as i32`, then the
        // group is proved representable from its endpoints (doc above), so
        // every accepted node classifies at the coordinate the exact node
        // scanner gives it and that the checked accessors give it, and a
        // group the scanner would refuse is refused here too.
        //
        // There is deliberately no granularity-100 shortcut
        // (`cumulative + offset / 100`, skipping the multiply and divide).
        // One existed; it wrapped at a different point from the accessors,
        // so on a block whose true nanodegree value overflows `i64` it
        // disagreed with the accessors, and a matched
        // planet A/B of extract --strategy smart and multi-extract found
        // its removal inside noise (reference/performance/read-pipeline.md, "Columnar
        // granularity-100 shortcut").
        let lats_before = self.lats.len();
        let (mut lat_min, mut lat_max) = (i64::MAX, i64::MIN);
        let mut cumulative_lat: i64 = 0;
        for delta in PackedSint64Iter::new(dense.lat_data) {
            cumulative_lat = cumulative_lat.wrapping_add(delta);
            lat_min = lat_min.min(cumulative_lat);
            lat_max = lat_max.max(cumulative_lat);
            self.lats.push(decimicro(lat_offset, gran, cumulative_lat));
        }
        if self.lats.len() > lats_before {
            representable(lat_offset, gran, lat_min, lat_max)?;
        }

        let lons_before = self.lons.len();
        let (mut lon_min, mut lon_max) = (i64::MAX, i64::MIN);
        let mut cumulative_lon: i64 = 0;
        for delta in PackedSint64Iter::new(dense.lon_data) {
            cumulative_lon = cumulative_lon.wrapping_add(delta);
            lon_min = lon_min.min(cumulative_lon);
            lon_max = lon_max.max(cumulative_lon);
            self.lons.push(decimicro(lon_offset, gran, cumulative_lon));
        }
        if self.lons.len() > lons_before {
            representable(lon_offset, gran, lon_min, lon_max)?;
        }
        Ok(())
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
        // Granularity 0 makes every coordinate the offset, so the wrapped
        // cumulative values are representable and only the deltas wrap.
        let mut columns = DenseNodeColumns::new();
        columns
            .decode_append(&dense, 0, 0, 0)
            .expect("granularity 0 is representable");
        assert_eq!(columns.ids, [i64::MIN, i64::MAX]);
        assert_eq!(columns.lats, [0, 0]);
        assert_eq!(columns.lons, [0, 0]);
        // At granularity 100 the same values are far past i32 and refused,
        // where the wrapping arithmetic alone gave 0 and -1.
        let mut columns = DenseNodeColumns::new();
        assert!(columns.decode_append(&dense, 100, 0, 0).is_err());
    }

    /// The group check matches `scale_to_decimicro` on every node: an
    /// unrepresentable value anywhere in the group (first, middle or last,
    /// either axis, either granularity sign) refuses the group, the last 99
    /// nanodegrees past each `i32` edge are accepted, and a cancelling
    /// encoding whose product overflows `i64` is accepted at the scanner's
    /// value.
    #[test]
    fn columnar_coordinates_are_checked_like_the_scanner() {
        use crate::scan::node::scale_to_decimicro;
        fn zz(v: i64) -> u64 {
            ((v << 1) ^ (v >> 63)).cast_unsigned()
        }
        fn packed_deltas(values: &[i64]) -> Vec<u8> {
            let mut out = Vec::new();
            let mut prev = 0_i64;
            for &v in values {
                let mut z = zz(v.wrapping_sub(prev));
                prev = v;
                while z >= 0x80 {
                    #[allow(clippy::cast_possible_truncation)]
                    out.push((z as u8) | 0x80);
                    z >>= 7;
                }
                #[allow(clippy::cast_possible_truncation)]
                out.push(z as u8);
            }
            out
        }
        let edge_hi = i64::from(i32::MAX) * 100 + 99;
        let edge_lo = i64::from(i32::MIN) * 100 - 99;
        let cases: &[(i32, i64, &[i64])] = &[
            (1, 0, &[0, edge_hi, edge_lo]),
            (1, 0, &[0, edge_hi + 1]),
            (1, 0, &[edge_lo - 1, 0]),
            (100, 0, &[5, 4_294_967_296, 7]),
            (100, 429_496_729_600, &[0]),
            (-100, 0, &[3, -2_147_483_648, 2]),
            (-100, 0, &[3, -2_147_483_647, 2]),
            (100, -9_223_372_036_854_775_800, &[92_233_720_368_547_759]),
            (100, 9_223_372_036_854_775_799, &[-92_233_720_368_547_759]),
            (1000, 500, &[-35, 12, 900_000]),
        ];
        for &(granularity, offset, values) in cases {
            let gran = i64::from(granularity);
            let expected: Option<Vec<i32>> = values
                .iter()
                .map(|&v| scale_to_decimicro(offset, gran, v))
                .collect();
            let ids = packed_deltas(
                &(1..=values.len())
                    .map(|i| i64::try_from(i).expect("small"))
                    .collect::<Vec<_>>(),
            );
            let coords = packed_deltas(values);
            let zero = packed_deltas(&vec![0; values.len()]);
            for (lat_data, lon_data, axis) in [(&coords, &zero, "lat"), (&zero, &coords, "lon")] {
                let dense = WireDenseNodes {
                    id_data: &ids,
                    lat_data,
                    lon_data,
                    keys_vals_data: &[],
                    info_data: None,
                };
                let (lat_offset, lon_offset) = if axis == "lat" {
                    (offset, 0)
                } else {
                    (0, offset)
                };
                let mut columns = DenseNodeColumns::new();
                let got = columns.decode_append(&dense, granularity, lat_offset, lon_offset);
                match &expected {
                    Some(expected) => {
                        assert!(got.is_ok(), "{axis} {granularity} {offset} {values:?}");
                        let column = if axis == "lat" {
                            &columns.lats
                        } else {
                            &columns.lons
                        };
                        assert_eq!(column, expected, "{axis} {granularity} {offset} {values:?}");
                    }
                    None => assert!(got.is_err(), "{axis} {granularity} {offset} {values:?}"),
                }
            }
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
        // 100 * i32::MAX + 100 nanodegrees is one decimicrodegree past
        // i32::MAX: the offset is applied before the narrowing, which refuses
        // it rather than wrapping it to i32::MIN.
        let mut columns = DenseNodeColumns::new();
        assert!(columns.decode_append(&dense, 100, 100, 100).is_err());
        let mut columns = DenseNodeColumns::new();
        columns
            .decode_append(&dense, 100, 0, 0)
            .expect("i32::MAX is representable");
        assert_eq!(columns.lats, [i32::MAX]);
        assert_eq!(columns.lons, [i32::MAX]);
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
