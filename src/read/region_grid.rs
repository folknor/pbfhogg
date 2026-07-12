//! Conservative grid index for pruning multi-extract region candidates.

const CELL_SIZE_DMD: i64 = 1_000_000;
const LON_CELLS: usize = 3600;
const LAT_CELLS: usize = 1800;
const NUM_CELLS: usize = LON_CELLS * LAT_CELLS;
// Highest valid column / row index as i64, for use as the clamp upper bound in
// the widened cell-index math below. These mirror LON_CELLS-1 / LAT_CELLS-1;
// they are literal i64 (not a usize->i64 cast) so the affine shift stays in i64
// without an extra flagged cast. Keep in lockstep with LON_CELLS / LAT_CELLS.
const LON_MAX_CELL: i64 = 3599;
const LAT_MAX_CELL: i64 = 1799;
const LON_OFFSET_DMD: i64 = 1_800_000_000;
const LAT_OFFSET_DMD: i64 = 900_000_000;
const GRID_MAX_INDEX_BYTES: u64 = 256 * 1024 * 1024;
const GRID_MAX_PAIRS: u64 = GRID_MAX_INDEX_BYTES / 4;

/// A compressed sparse row grid of region bounding-box coverage.
///
/// This is only a candidate index. Callers must always perform their normal
/// exact containment check on every returned region index.
pub(crate) struct RegionGrid {
    cell_starts: Vec<u32>,
    region_indices: Vec<u32>,
}

// Widen to i64 for the affine shift (lon + LON_OFFSET_DMD overflows i32), then
// clamp into 0..=LON_MAX_CELL before the value is ever used as an index. The
// clamped result is provably in 0..=3599, so the cast to usize can neither
// truncate nor sign-lose.
#[inline]
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn cell_lon(lon: i32) -> usize {
    let raw = (i64::from(lon) + LON_OFFSET_DMD) / CELL_SIZE_DMD;
    raw.clamp(0, LON_MAX_CELL) as usize
}

// As `cell_lon`, for latitude: the clamp confines the shifted i64 to
// 0..=LAT_MAX_CELL (<= 1799) before the cast, so usize is exact.
#[inline]
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn cell_lat(lat: i32) -> usize {
    let raw = (i64::from(lat) + LAT_OFFSET_DMD) / CELL_SIZE_DMD;
    raw.clamp(0, LAT_MAX_CELL) as usize
}

#[inline]
fn cell_of(lat: i32, lon: i32) -> usize {
    cell_lat(lat) * LON_CELLS + cell_lon(lon)
}

#[inline]
fn coverage_is_within_budget(total_pairs: u64) -> bool {
    total_pairs <= GRID_MAX_PAIRS && total_pairs <= u64::from(u32::MAX)
}

/// Test-only hook: force [`RegionGrid::build`] to return `None` so that
/// classification runs the linear scan even at/above the region threshold.
/// Lets a test drive one identical config down both the grid and the linear
/// code paths and compare their output byte-for-byte.
#[cfg(feature = "test-hooks")]
pub mod test_hooks {
    use std::sync::atomic::{AtomicBool, Ordering};

    /// When set, `RegionGrid::build` returns `None`.
    pub static FORCE_LINEAR: AtomicBool = AtomicBool::new(false);

    /// Clear the force-linear flag.
    pub fn reset() {
        FORCE_LINEAR.store(false, Ordering::Relaxed);
    }
}

#[cfg(feature = "test-hooks")]
#[inline]
fn force_linear() -> bool {
    test_hooks::FORCE_LINEAR.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(not(feature = "test-hooks"))]
#[inline]
fn force_linear() -> bool {
    false
}

impl RegionGrid {
    /// Rasterize region bounding boxes as inclusive cell rectangles.
    ///
    /// Returns `None` before allocating the grid when its coverage would exceed
    /// the bounded index budget. The caller must then retain its linear scan.
    pub(crate) fn build(region_bboxes: &[(i32, i32, i32, i32)]) -> Option<Self> {
        if force_linear() {
            return None;
        }
        let mut total_pairs = 0_u64;
        for &(min_lat, max_lat, min_lon, max_lon) in region_bboxes {
            let min_col = cell_lon(min_lon);
            let max_col = cell_lon(max_lon);
            let min_row = cell_lat(min_lat);
            let max_row = cell_lat(max_lat);
            if min_col > max_col || min_row > max_row {
                return None;
            }
            let width = (max_col - min_col + 1) as u64;
            let height = (max_row - min_row + 1) as u64;
            total_pairs = total_pairs.checked_add(width * height)?;
            if !coverage_is_within_budget(total_pairs) {
                return None;
            }
        }

        let mut cell_starts = vec![0_u32; NUM_CELLS + 1];
        for &(min_lat, max_lat, min_lon, max_lon) in region_bboxes {
            for row in cell_lat(min_lat)..=cell_lat(max_lat) {
                let row_start = row * LON_CELLS;
                for col in cell_lon(min_lon)..=cell_lon(max_lon) {
                    cell_starts[row_start + col + 1] += 1;
                }
            }
        }
        for cell in 1..=NUM_CELLS {
            cell_starts[cell] += cell_starts[cell - 1];
        }

        // total_pairs passed the coverage budget above (<= GRID_MAX_PAIRS and
        // <= u32::MAX), so it fits usize on any supported target; try_from makes
        // that explicit and cannot fail for an in-budget grid (the ? is dead for
        // valid input, matching the checked_add / try_from(region) fallbacks).
        let mut region_indices = vec![0_u32; usize::try_from(total_pairs).ok()?];
        let mut cursor = cell_starts.clone();
        for (region, &(min_lat, max_lat, min_lon, max_lon)) in region_bboxes.iter().enumerate() {
            let region = u32::try_from(region).ok()?;
            for row in cell_lat(min_lat)..=cell_lat(max_lat) {
                let row_start = row * LON_CELLS;
                for col in cell_lon(min_lon)..=cell_lon(max_lon) {
                    let cell = row_start + col;
                    let offset = cursor[cell] as usize;
                    region_indices[offset] = region;
                    cursor[cell] += 1;
                }
            }
        }

        Some(Self {
            cell_starts,
            region_indices,
        })
    }

    #[inline]
    pub(crate) fn candidates(&self, lat: i32, lon: i32) -> &[u32] {
        let cell = cell_of(lat, lon);
        let start = self.cell_starts[cell] as usize;
        let end = self.cell_starts[cell + 1] as usize;
        &self.region_indices[start..end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xs(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn cell_index_math() {
        assert_eq!(cell_of(0, 0), 3_241_800);
        // Mid-cell point (sub-0.1 degree magnitudes) rounds into the origin cell.
        assert_eq!(cell_of(123_456, 654_321), 3_241_800);
        assert_eq!(cell_of(-900_000_000, -1_800_000_000), 0);
        assert_eq!(cell_of(900_000_000, 1_800_000_000), 6_479_999);
        // Exact 0.1 degree cell edge lands in the higher-indexed cell.
        assert_eq!(cell_of(-899_000_000, -1_799_000_000), 3_601);
        // A coordinate exactly on a 0.1 degree boundary lands in the higher
        // column; one decimicrodegree below stays in the lower column.
        assert_eq!(cell_lon(500_000_000), 2300);
        assert_eq!(cell_lon(499_999_999), 2299);
    }

    #[test]
    fn region_max_lon_boundary_is_a_candidate() {
        // A point whose longitude equals a region's exact max_lon must still be
        // rasterized into that region's cell rectangle (superset property at the
        // integer-longitude boundary the pre-existing strip discrepancy lives at).
        let region = (10_000_000, 30_000_000, 100_000_000, 500_000_000);
        let grid = RegionGrid::build(&[region]).expect("single region within budget");
        assert!(grid.candidates(20_000_000, 500_000_000).contains(&0));
    }

    #[test]
    fn out_of_domain_clamps() {
        assert_eq!(cell_lon(2_000_000_000), LON_CELLS - 1);
        assert_eq!(cell_lat(1_000_000_000), LAT_CELLS - 1);
        assert_eq!(cell_lon(-2_000_000_000), 0);
        assert_eq!(cell_lat(-1_000_000_000), 0);
    }

    #[test]
    fn rasterize_exact_cell_set() {
        let bbox = (10_000_000, 30_000_000, -20_000_000, 40_000_000);
        let grid = RegionGrid::build(&[bbox]).expect("small grid coverage");
        for row in 0..LAT_CELLS {
            for col in 0..LON_CELLS {
                let expected = (cell_lat(bbox.0)..=cell_lat(bbox.1)).contains(&row)
                    && (cell_lon(bbox.2)..=cell_lon(bbox.3)).contains(&col);
                let found = grid.cell_starts[row * LON_CELLS + col]
                    != grid.cell_starts[row * LON_CELLS + col + 1];
                assert_eq!(found, expected);
            }
        }
    }

    #[test]
    fn rasterize_full_width_antimeridian() {
        let grid = RegionGrid::build(&[(0, 0, -1_800_000_000, 1_800_000_000)])
            .expect("one row is within budget");
        let row = cell_lat(0);
        for col in 0..LON_CELLS {
            assert_eq!(
                grid.region_indices[grid.cell_starts[row * LON_CELLS + col] as usize],
                0
            );
        }
        assert!(grid.candidates(100_000_000, 0).is_empty());
    }

    #[test]
    fn coverage_budget_falls_back() {
        // 16 full-world boxes = 103_680_000 pairs > the 256 MiB budget -> fall back.
        let world = (-900_000_000, 900_000_000, -1_800_000_000, 1_800_000_000);
        assert!(RegionGrid::build(&[world; 16]).is_none());
        // 16 small ~1 degree boxes are comfortably within budget -> build succeeds.
        let small = (10_000_000, 20_000_000, 10_000_000, 20_000_000);
        assert!(RegionGrid::build(&[small; 16]).is_some());
        // Budget predicate: within-budget small coverage passes, and a synthetic
        // pair total above u32::MAX is rejected by the independent overflow guard.
        assert!(coverage_is_within_budget(16 * 121));
        assert!(coverage_is_within_budget(GRID_MAX_PAIRS));
        assert!(!coverage_is_within_budget(GRID_MAX_PAIRS + 1));
        assert!(!coverage_is_within_budget(u64::from(u32::MAX) + 1));
    }

    // Test RNG: callers always pass `hi >= lo` with `lo..=hi` inside the i32
    // decimicrodegree coordinate domain, so the span (i64->u64) is non-negative,
    // `xs % span` (u64->i64) is a small positive value, and the sampled result
    // is within `lo..=hi` and thus fits i32. The narrowing casts are intentional
    // and lossless for every input the tests actually feed in.
    #[allow(
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::cast_possible_truncation
    )]
    fn rand_range(state: &mut u64, lo: i64, hi: i64) -> i32 {
        let span = (hi - lo + 1) as u64;
        (lo + (xs(state) % span) as i64) as i32
    }

    /// 24-region mix drawn from `seed`: small boxes, large boxes, disjoint
    /// strips, mutually overlapping boxes, and pinned edge cases (pole-spanning,
    /// full-width antimeridian, out-of-domain longitude).
    ///
    /// The i64->i32 casts below narrow bounds that are computed in i64 only to
    /// avoid intermediate i32 overflow; every constructed coordinate stays
    /// within the i32 decimicrodegree domain, so the casts are lossless.
    #[allow(clippy::cast_possible_truncation)]
    fn fixture_regions(seed: u64) -> Vec<(i32, i32, i32, i32)> {
        let mut s = seed;
        let mut r = Vec::with_capacity(24);
        // 0-5: small ~1 degree boxes.
        for _ in 0..6 {
            let min_lon = rand_range(&mut s, -1_790_000_000, 1_780_000_000);
            let min_lat = rand_range(&mut s, -890_000_000, 880_000_000);
            r.push((min_lat, min_lat + 10_000_000, min_lon, min_lon + 10_000_000));
        }
        // 6-11: large ~30 degree boxes.
        for _ in 0..6 {
            let min_lon = rand_range(&mut s, -1_800_000_000, 1_500_000_000);
            let min_lat = rand_range(&mut s, -900_000_000, 600_000_000);
            r.push((
                min_lat,
                min_lat + 300_000_000,
                min_lon,
                min_lon + 300_000_000,
            ));
        }
        // 12-17: six disjoint longitude strips partitioning the domain.
        for k in 0..6_i64 {
            let w = 600_000_000_i64;
            let min_lon = -1_800_000_000 + k * w;
            r.push((
                -400_000_000,
                400_000_000,
                min_lon as i32,
                (min_lon + w - 1) as i32,
            ));
        }
        // 18-20: mutually overlapping boxes around a shared center.
        for k in 0..3_i64 {
            let c = 100_000_000 + k * 20_000_000;
            r.push((
                (c - 150_000_000) as i32,
                (c + 150_000_000) as i32,
                (c - 150_000_000) as i32,
                (c + 150_000_000) as i32,
            ));
        }
        // 21: pole-spanning.
        r.push((800_000_000, 900_000_000, -200_000_000, 200_000_000));
        // 22: full-width longitude (antimeridian band).
        r.push((100_000_000, 120_000_000, -1_800_000_000, 1_800_000_000));
        // 23: out-of-domain max_lon (clamps into the last column).
        r.push((-100_000_000, 100_000_000, 1_700_000_000, 2_000_000_000));
        r
    }

    // `i as u32` narrows an enumerate index over a 24-element fixture, well
    // within u32; the cast cannot truncate.
    #[allow(clippy::cast_possible_truncation)]
    #[test]
    fn superset_matches_linear() {
        for &seed in &[1_u64, 2, 3, 5, 8, 13, 21, 34] {
            let bboxes = fixture_regions(seed);
            let grid = RegionGrid::build(&bboxes).expect("fixture is within budget");
            let mut ps = seed ^ 0x1234_5678_9ABC_DEF0;
            for _ in 0..5_000 {
                // Point range deliberately exceeds the domain so clamping is
                // exercised on both the rasterize and the query side.
                let lat = rand_range(&mut ps, -1_000_000_000, 1_000_000_000);
                let lon = rand_range(&mut ps, -2_000_000_000, 2_000_000_000);
                let linear: Vec<u32> = bboxes
                    .iter()
                    .enumerate()
                    .filter_map(|(i, b)| {
                        (lat >= b.0 && lat <= b.1 && lon >= b.2 && lon <= b.3).then_some(i as u32)
                    })
                    .collect();
                let candidates = grid.candidates(lat, lon);
                let pruned: Vec<u32> = candidates
                    .iter()
                    .copied()
                    .filter(|&i| {
                        let b = bboxes[i as usize];
                        lat >= b.0 && lat <= b.1 && lon >= b.2 && lon <= b.3
                    })
                    .collect();
                // Deciding-layer equality: grid-pruned matches == brute-force matches.
                assert_eq!(pruned, linear, "seed {seed} point ({lat},{lon})");
                // Raw superset: every real match is present among the candidates.
                for i in &linear {
                    assert!(
                        candidates.contains(i),
                        "seed {seed} point ({lat},{lon}) missing candidate {i}"
                    );
                }
            }
        }
    }
}
