//! Shared geometry primitives for spatial operations.
//!
//! Extracted from `commands/extract.rs` and extended with distance functions,
//! polygon simplification, and ring assembly for the geocode index builder.

use std::f64::consts::PI;

#[cfg(feature = "commands")]
use crate::coord_fmt::deg_to_e7;
use crate::coord_fmt::e7_to_deg;

/// A geographic bounding box in WGS84 degrees.
pub struct Bbox {
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
}

impl Bbox {
    /// Construct a finite, ordered bounding box within the WGS84 domain.
    pub fn try_new(min_lon: f64, min_lat: f64, max_lon: f64, max_lat: f64) -> crate::Result<Self> {
        use crate::error::Error;
        // Domain checks precede ordering: ordered values outside WGS84 are still
        // invalid, even when an integer conversion could clamp them into range.
        // The caller supplied these numbers, so a bad one is an invalid argument.
        for (name, value) in [("min_lat", min_lat), ("max_lat", max_lat)] {
            crate::coord_pack::validate_lat(name, value).map_err(Error::invalid_argument_from)?;
        }
        for (name, value) in [("min_lon", min_lon), ("max_lon", max_lon)] {
            crate::coord_pack::validate_lon(name, value).map_err(Error::invalid_argument_from)?;
        }
        if min_lon >= max_lon {
            return Err(Error::invalid_argument(format!(
                "min_lon ({min_lon}) must be less than max_lon ({max_lon})"
            )));
        }
        if min_lat >= max_lat {
            return Err(Error::invalid_argument(format!(
                "min_lat ({min_lat}) must be less than max_lat ({max_lat})"
            )));
        }
        Ok(Self {
            min_lon,
            min_lat,
            max_lon,
            max_lat,
        })
    }

    /// Returns `true` if the point (lat, lon) in degrees falls within this bbox.
    #[cfg(feature = "commands")]
    pub(crate) fn contains(&self, lat: f64, lon: f64) -> bool {
        lat >= self.min_lat && lat <= self.max_lat && lon >= self.min_lon && lon <= self.max_lon
    }
}

// Spatial membership deliberately uses multiplication, matching reader geometry.
// The text/OSC conversion divides for byte compatibility; sharing that function
// would change which boundary nodes extraction includes.
#[cfg(feature = "commands")]
pub(crate) fn to_degrees(value: i32) -> f64 {
    e7_to_deg(f64::from(value))
}

/// Smallest decimicrodegree value whose degree form is `>= limit`.
///
/// The guess comes from scaling, then walks until the *forward* conversion
/// satisfies the comparison in both directions, so the result is exact
/// regardless of which way the two conversions round.
#[allow(clippy::cast_possible_truncation)]
#[cfg(feature = "commands")]
fn ceil_e7(limit: f64) -> i32 {
    let mut v = deg_to_e7(limit).ceil().clamp(-2.1e9, 2.1e9) as i32;
    while to_degrees(v) < limit {
        match v.checked_add(1) {
            Some(next) => v = next,
            None => break,
        }
    }
    while let Some(prev) = v.checked_sub(1) {
        if to_degrees(prev) >= limit {
            v = prev;
        } else {
            break;
        }
    }
    v
}

/// Largest decimicrodegree value whose degree form is `<= limit`.
#[allow(clippy::cast_possible_truncation)]
#[cfg(feature = "commands")]
fn floor_e7(limit: f64) -> i32 {
    let mut v = deg_to_e7(limit).floor().clamp(-2.1e9, 2.1e9) as i32;
    while to_degrees(v) > limit {
        match v.checked_sub(1) {
            Some(prev) => v = prev,
            None => break,
        }
    }
    while let Some(next) = v.checked_add(1) {
        if to_degrees(next) <= limit {
            v = next;
        } else {
            break;
        }
    }
    v
}

/// Integer bounding box in decimicrodegrees.
#[cfg(feature = "commands")]
pub(crate) struct BboxInt {
    pub(crate) min_lon: i32,
    pub(crate) min_lat: i32,
    pub(crate) max_lon: i32,
    pub(crate) max_lat: i32,
}

#[cfg(feature = "commands")]
impl BboxInt {
    /// Conservative integer bounds: a strict superset of [`Self::inside_bbox`],
    /// widened by one decimicrodegree on each side. Use for blob-level
    /// candidate selection and as a polygon prefilter, where a false positive
    /// costs work and a false negative loses data. Never for point membership.
    pub(crate) fn from_bbox(bbox: &Bbox) -> Self {
        let inside = Self::inside_bbox(bbox);
        Self {
            min_lon: inside.min_lon.saturating_sub(1),
            min_lat: inside.min_lat.saturating_sub(1),
            max_lon: inside.max_lon.saturating_add(1),
            max_lat: inside.max_lat.saturating_add(1),
        }
    }

    /// Exact integer bounds: `contains` on this box agrees, point for point,
    /// with `bbox.contains(lat as f64 * 1e-7, lon as f64 * 1e-7)`. Use for
    /// point membership, columnar classification, and for proving that a raw
    /// blob can be passed through untouched.
    pub(crate) fn inside_bbox(bbox: &Bbox) -> Self {
        Self {
            min_lon: ceil_e7(bbox.min_lon),
            min_lat: ceil_e7(bbox.min_lat),
            max_lon: floor_e7(bbox.max_lon),
            max_lat: floor_e7(bbox.max_lat),
        }
    }

    /// Return whether a decimicrodegree point is inside the box.
    pub(crate) fn contains(&self, lat: i32, lon: i32) -> bool {
        lat >= self.min_lat && lat <= self.max_lat && lon >= self.min_lon && lon <= self.max_lon
    }
}

/// Mean Earth radius in meters (WGS84 volumetric mean).
pub const EARTH_RADIUS_M: f64 = 6_371_008.8;

// ---------------------------------------------------------------------------
// Point-in-polygon (ray casting)
// ---------------------------------------------------------------------------

/// Ray-casting point-in-polygon test.
///
/// Coordinates are `(x, y)` = `(longitude, latitude)` in degrees.
/// Each crossing toggles membership, so accepted self-intersecting rings use
/// the even-odd interpretation. This matches osmium extract's polygon test;
/// this function does not decide whether a ring is geometrically simple.
/// Returns `false` for rings with fewer than 3 vertices.
pub fn point_in_ring(px: f64, py: f64, ring: &[(f64, f64)]) -> bool {
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = ring[i];
        let (xj, yj) = ring[j];
        if ((yi > py) != (yj > py)) && (px < (xj - xi) * (py - yi) / (yj - yi) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Point-in-ring that handles rings crossing the antimeridian (±180° longitude).
pub fn point_in_ring_with_antimeridian(px: f64, py: f64, ring: &[(f64, f64)]) -> bool {
    if !ring_crosses_antimeridian(ring) {
        return point_in_ring(px, py, ring);
    }
    let unwrapped = unwrap_ring_longitudes(ring);
    point_in_ring(px, py, &unwrapped)
        || point_in_ring(px + 360.0, py, &unwrapped)
        || point_in_ring(px - 360.0, py, &unwrapped)
}

/// Test if a point is inside a polygon with holes.
///
/// `exterior` is the outer ring (must contain the point).
/// `holes` are inner rings (must NOT contain the point).
/// Coordinates are `(longitude, latitude)` in degrees.
pub fn point_in_polygon(
    px: f64,
    py: f64,
    exterior: &[(f64, f64)],
    holes: &[&[(f64, f64)]],
) -> bool {
    point_in_polygon_rings(px, py, exterior, holes.iter().copied())
}

/// The one implementation behind [`point_in_polygon`], taking holes as any
/// iterator of borrowed slices so callers holding `Vec<Vec<_>>` holes need
/// no temporary `Vec<&[_]>`. Crate-private so the public surface keeps one
/// name for the operation.
pub(crate) fn point_in_polygon_rings<'a>(
    px: f64,
    py: f64,
    exterior: &[(f64, f64)],
    holes: impl IntoIterator<Item = &'a [(f64, f64)]>,
) -> bool {
    if !point_in_ring_with_antimeridian(px, py, exterior) {
        return false;
    }
    !holes
        .into_iter()
        .any(|hole| point_in_ring_with_antimeridian(px, py, hole))
}

/// Detect whether any ring segment crosses the antimeridian.
pub fn ring_crosses_antimeridian(ring: &[(f64, f64)]) -> bool {
    if ring.len() < 2 {
        return false;
    }
    ring.windows(2)
        .any(|segment| (segment[1].0 - segment[0].0).abs() > 180.0)
        || (ring[ring.len() - 1].0 - ring[0].0).abs() > 180.0
}

/// Unwrap longitudes into a continuous sequence to avoid the ±180° discontinuity.
fn unwrap_ring_longitudes(ring: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut out = Vec::with_capacity(ring.len());
    if ring.is_empty() {
        return out;
    }

    let (first_lon, first_lat) = ring[0];
    out.push((first_lon, first_lat));
    let mut prev_unwrapped_lon = first_lon;

    for &(lon, lat) in &ring[1..] {
        let mut adjusted = lon;
        while adjusted - prev_unwrapped_lon > 180.0 {
            adjusted -= 360.0;
        }
        while adjusted - prev_unwrapped_lon < -180.0 {
            adjusted += 360.0;
        }
        out.push((adjusted, lat));
        prev_unwrapped_lon = adjusted;
    }

    out
}

// ---------------------------------------------------------------------------
// Distance calculations (cos projection approximation)
// ---------------------------------------------------------------------------

/// Approximate squared distance between two points using the cos projection formula.
///
/// Input coordinates are in radians. `cos_lat` is `cos(query_latitude)`,
/// precomputed once per query. Returns radians². Multiply by `EARTH_RADIUS_M²`
/// for meters².
#[inline]
pub fn approx_distance_sq(
    lat1_rad: f64,
    lon1_rad: f64,
    lat2_rad: f64,
    lon2_rad: f64,
    cos_lat: f64,
) -> f64 {
    let dlat = lat2_rad - lat1_rad;
    let dlon = wrapped_longitude_delta(lon2_rad - lon1_rad);
    dlat * dlat + dlon * dlon * cos_lat * cos_lat
}

/// Normalize a longitude delta in radians to the shortest path across the
/// antimeridian. Inputs produced from legal longitudes are at most 2 PI apart,
/// but `rem_euclid` also keeps this total for larger finite inputs.
#[inline]
fn wrapped_longitude_delta(delta: f64) -> f64 {
    (delta + PI).rem_euclid(2.0 * PI) - PI
}

/// One full turn of longitude in decimicrodegrees (10⁻⁷ degrees).
pub const LON_FULL_TURN_E7: i64 = 3_600_000_000;

/// Shortest-path longitude delta in decimicrodegrees.
///
/// The radian twin of this is [`point_to_segment_distance_sq`]'s internal
/// unwrapping. Any caller that interpolates along a segment in the E7 domain
/// must unwrap with this first, otherwise a segment spanning ±180° is
/// interpolated the long way around the globe.
#[inline]
pub fn wrapped_longitude_delta_e7(delta_e7: i64) -> i64 {
    (delta_e7 + LON_FULL_TURN_E7 / 2).rem_euclid(LON_FULL_TURN_E7) - LON_FULL_TURN_E7 / 2
}

/// Fold an unwrapped longitude in decimicrodegrees back into (-180°, +180°].
///
/// Exactly ±180° is reported as +180° (`1_800_000_000`), matching the
/// convention that the antimeridian is the eastern edge of the domain.
#[inline]
pub fn normalize_longitude_e7(lon_e7: i64) -> i64 {
    let folded = wrapped_longitude_delta_e7(lon_e7);
    if folded == -LON_FULL_TURN_E7 / 2 {
        LON_FULL_TURN_E7 / 2
    } else {
        folded
    }
}

/// Convert a distance in meters to radians² for comparison with [`approx_distance_sq`].
#[inline]
pub fn meters_to_radians_sq(meters: f64) -> f64 {
    let r = meters / EARTH_RADIUS_M;
    r * r
}

/// Convert decimicrodegrees (i32, 10⁻⁷ degrees) to radians.
#[inline]
pub fn e7_to_rad(e7: i32) -> f64 {
    e7_to_deg(e7 as f64) * PI / 180.0
}

/// Closest point on a line segment to a query point (cos projection).
///
/// All coordinates are in radians. `cos_lat` is precomputed.
/// Returns `(t, distance_sq)` where `t ∈ [0, 1]` is the parameter along the
/// segment and `distance_sq` is in radians².
#[inline]
pub fn point_to_segment_distance_sq(
    px: f64,
    py: f64,
    ax: f64,
    ay: f64,
    bx: f64,
    by: f64,
    cos_lat: f64,
) -> (f64, f64) {
    // Work in a longitude frame unwrapped around A. Without this, a short
    // segment crossing +/-180 degrees is treated as almost a full world wide.
    let bx = ax + wrapped_longitude_delta(bx - ax);
    let px = ax + wrapped_longitude_delta(px - ax);
    let dx = (bx - ax) * cos_lat;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;

    let t = if len_sq < crate::tunables::DEGENERATE_SEGMENT_EPSILON {
        0.0
    } else {
        let dot = ((px - ax) * cos_lat) * dx + (py - ay) * dy;
        dot / len_sq
    };
    let t = t.clamp(0.0, 1.0);

    let closest_x = ax + t * (bx - ax);
    let closest_y = ay + t * (by - ay);
    let dist_sq = approx_distance_sq(py, px, closest_y, closest_x, cos_lat);
    (t, dist_sq)
}

// ---------------------------------------------------------------------------
// Douglas-Peucker simplification
// ---------------------------------------------------------------------------

/// Douglas-Peucker polyline simplification with a vertex cap.
///
/// If the ring already has ≤ `max_vertices` vertices, it is returned unchanged.
/// Otherwise, binary search for an epsilon that yields ≤ `max_vertices` vertices.
/// Coordinates are `(x, y)` (longitude, latitude) in degrees.
pub fn simplify_ring(ring: &[(f64, f64)], max_vertices: usize) -> Vec<(f64, f64)> {
    if ring.len() <= max_vertices {
        return ring.to_vec();
    }
    if ring.len() < 2 {
        return ring.to_vec();
    }

    // Compute the bounding box diagonal as the upper bound for epsilon.
    // This ensures hi is always large enough to reduce to 2 vertices.
    let mut min_x = f64::MAX;
    let mut max_x = f64::MIN;
    let mut min_y = f64::MAX;
    let mut max_y = f64::MIN;
    for &(x, y) in ring {
        if x < min_x {
            min_x = x;
        }
        if x > max_x {
            max_x = x;
        }
        if y < min_y {
            min_y = y;
        }
        if y > max_y {
            max_y = y;
        }
    }
    let diag = ((max_x - min_x).powi(2) + (max_y - min_y).powi(2)).sqrt();

    // Binary search for epsilon
    let mut lo = 0.0_f64;
    let mut hi = diag;

    for _ in 0..20 {
        let epsilon = (lo + hi) / 2.0;
        let count = dp_count(ring, epsilon);
        if count > max_vertices {
            lo = epsilon;
        } else {
            hi = epsilon;
        }
    }

    // Use `hi` (which always gives <= max_vertices)
    let mut keep = vec![false; ring.len()];
    keep[0] = true;
    if let Some(last) = ring.len().checked_sub(1) {
        keep[last] = true;
    }
    dp_mark(ring, 0, ring.len() - 1, hi, &mut keep);

    ring.iter()
        .zip(keep.iter())
        .filter(|(_, k)| **k)
        .map(|(&pt, _)| pt)
        .collect()
}

/// Count how many vertices would be kept at a given epsilon (allocation-free).
fn dp_count(ring: &[(f64, f64)], epsilon: f64) -> usize {
    if ring.len() <= 2 {
        return ring.len();
    }
    let mut count = 2;
    let mut stack = vec![(0, ring.len() - 1)];
    while let Some((start, end)) = stack.pop() {
        if let Some(max_idx) = dp_split(ring, start, end, epsilon * epsilon) {
            count += 1;
            stack.push((start, max_idx));
            stack.push((max_idx, end));
        }
    }
    count
}

/// Return the most significant interior vertex when it exceeds `epsilon_sq`.
fn dp_split(pts: &[(f64, f64)], start: usize, end: usize, epsilon_sq: f64) -> Option<usize> {
    if end <= start + 1 {
        return None;
    }
    let (ax, ay) = pts[start];
    let (bx, by) = pts[end];
    let dx = bx - ax;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;

    let mut max_dist = 0.0_f64;
    let mut max_idx = start + 1;
    for (i, &(px, py)) in pts.iter().enumerate().skip(start + 1).take(end - start - 1) {
        let rx = px - ax;
        let ry = py - ay;
        let dist = if len_sq < crate::tunables::DEGENERATE_SEGMENT_EPSILON {
            rx * rx + ry * ry
        } else {
            let t = ((rx * dx + ry * dy) / len_sq).clamp(0.0, 1.0);
            let proj_x = t * dx - rx;
            let proj_y = t * dy - ry;
            proj_x * proj_x + proj_y * proj_y
        };
        if dist > max_dist {
            max_dist = dist;
            max_idx = i;
        }
    }
    (max_dist > epsilon_sq).then_some(max_idx)
}

/// Mark vertices to keep using an explicit stack.
fn dp_mark(pts: &[(f64, f64)], start: usize, end: usize, epsilon: f64, keep: &mut [bool]) {
    let mut stack = vec![(start, end)];
    while let Some((range_start, range_end)) = stack.pop() {
        if let Some(max_idx) = dp_split(pts, range_start, range_end, epsilon * epsilon) {
            keep[max_idx] = true;
            stack.push((range_start, max_idx));
            stack.push((max_idx, range_end));
        }
    }
}

// ---------------------------------------------------------------------------
// Ring assembly from way segments
// ---------------------------------------------------------------------------

/// Assemble closed rings from a set of way segments.
///
/// Each segment is a slice of `(lat_e7, lon_e7)` coordinate pairs (i32
/// decimicrodegrees). Segments are joined by matching endpoints. Returns
/// closed rings as vectors of coordinate pairs. Unclosed chains are dropped.
///
/// Ambiguous endpoint graphs are explored with an explicit backtracking stack,
/// so a dead-end branch neither loses its segments nor hides a valid ring.
///
/// The search is unbounded: on a relation where many segments share
/// endpoints, full backtracking can take time exponential in the number of
/// branching endpoints. Callers fed untrusted data (the geocode builder) use
/// the crate's bounded variant, which runs this same search under a step
/// budget and gives up on the whole input rather than returning a different
/// ring set.
pub fn assemble_rings(segments: &[&[(i32, i32)]]) -> Vec<Vec<(i32, i32)>> {
    // No budget, so the search cannot report exhaustion.
    assemble_rings_search(segments, None).unwrap_or_default()
}

#[cfg(feature = "commands")]
use crate::tunables::RING_SEARCH_EXTRA_STEPS;

/// Step budget for [`assemble_rings_bounded`] over `segment_count` segments.
///
/// When every endpoint is shared by at most two segment ends there is never
/// a choice, so each start places at most `segment_count - 1` segments and
/// the whole call fewer than `segment_count^2`; such inputs always finish
/// within budget. Only branching endpoints (touching rings, duplicate
/// members, grids) can spend more, and they get [`RING_SEARCH_EXTRA_STEPS`]
/// of headroom. That turns the exponential worst case into a quadratic one
/// without changing the result for any input that finishes within budget.
// libosmium bounds its assembler differently (100 split locations, recursion
// depth 20), but its depth counts candidate ring joins in a different
// algorithm. Applied to this search, where depth is the number of member ways
// in one ring, a depth of 20 would refuse ordinary country boundaries.
#[cfg(feature = "commands")]
pub(crate) fn ring_search_budget(segment_count: usize) -> u64 {
    let count = u64::try_from(segment_count).unwrap_or(u64::MAX);
    count
        .saturating_mul(count)
        .saturating_add(RING_SEARCH_EXTRA_STEPS)
}

/// The bounded ring search ran out of steps. No rings are returned: a partial
/// ring set would silently differ from what the full search assembles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RingSearchBudgetExceeded;

/// [`assemble_rings`] under [`ring_search_budget`]. Within budget the result
/// is identical to the unbounded search, ring for ring and vertex for vertex.
#[cfg(feature = "commands")]
pub(crate) fn assemble_rings_bounded(
    segments: &[&[(i32, i32)]],
) -> Result<Vec<Vec<(i32, i32)>>, RingSearchBudgetExceeded> {
    assemble_rings_search(segments, Some(ring_search_budget(segments.len())))
}

/// Shared search. `budget` caps the number of segment placements across all
/// starts; `None` is unbounded. The search order is the contract: bounded and
/// unbounded callers must assemble the same rings whenever the bound holds.
fn assemble_rings_search(
    segments: &[&[(i32, i32)]],
    budget: Option<u64>,
) -> Result<Vec<Vec<(i32, i32)>>, RingSearchBudgetExceeded> {
    if segments.is_empty() {
        return Ok(Vec::new());
    }
    let mut steps: u64 = 0;

    // Build endpoint map: quantized endpoint -> list of segment indices
    let mut endpoint_map: std::collections::HashMap<(i32, i32), Vec<usize>> =
        std::collections::HashMap::new();
    // A one-point segment stays in the search: it is a zero-length link that
    // can be placed (and start a ring) at its point, exactly as before the
    // search was bounded. Dropping it would rotate or reorder output rings.
    for (idx, seg) in segments.iter().enumerate() {
        if seg.is_empty() {
            continue;
        }
        let start = seg[0];
        let end = seg[seg.len() - 1];
        endpoint_map.entry(start).or_default().push(idx);
        endpoint_map.entry(end).or_default().push(idx);
    }

    let mut used = vec![false; segments.len()];
    let mut rings = Vec::new();

    for start_idx in 0..segments.len() {
        if used[start_idx] || segments[start_idx].is_empty() {
            continue;
        }

        used[start_idx] = true;
        let mut ring: Vec<(i32, i32)> = segments[start_idx].to_vec();
        let ring_start = ring[0];

        // Full backtracking, deliberately: pruning revisited endpoints would
        // still find a ring whenever one exists, but not always the same
        // ring (a figure-eight trail through a touching point becomes two
        // rings, or a different partition), which changes geocode admin
        // polygons. The step budget, not pruning, bounds the work.
        // Candidate lists are borrowed from the endpoint map, not cloned.
        struct SearchFrame<'a> {
            candidates: &'a [usize],
            next_candidate: usize,
            chosen_segment: Option<usize>,
            ring_len_before: usize,
        }
        let candidates = endpoint_map
            .get(ring.last().expect("nonempty ring"))
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut search = vec![SearchFrame {
            candidates,
            next_candidate: 0,
            chosen_segment: None,
            ring_len_before: ring.len(),
        }];

        while !search.is_empty() {
            let current_end = ring[ring.len() - 1];

            // Ring is closed
            if ring.len() > 2 && current_end == ring_start {
                break;
            }

            let frame = search.last_mut().expect("search is nonempty");
            let next = frame.candidates[frame.next_candidate..]
                .iter()
                .position(|&idx| !used[idx])
                .map(|offset| frame.next_candidate + offset);
            let Some(candidate_pos) = next else {
                let exhausted = search.pop().expect("search is nonempty");
                if let Some(segment_idx) = exhausted.chosen_segment {
                    used[segment_idx] = false;
                    ring.truncate(exhausted.ring_len_before);
                }
                continue;
            };
            frame.next_candidate = candidate_pos + 1;
            let next_idx = frame.candidates[candidate_pos];

            // One step per placement attempt. Checked before the placement,
            // so an input that needs exactly `budget` steps still finishes.
            if let Some(budget) = budget
                && steps >= budget
            {
                return Err(RingSearchBudgetExceeded);
            }
            steps += 1;

            used[next_idx] = true;
            let ring_len_before = ring.len();
            let seg = segments[next_idx];
            let seg_start = seg[0];
            let seg_end = seg[seg.len() - 1];

            if seg_start == current_end {
                // Append forward (skip first point, it's the shared endpoint)
                ring.extend_from_slice(&seg[1..]);
            } else if seg_end == current_end {
                // Append reversed (skip last point, it's the shared endpoint)
                for &pt in seg[..seg.len() - 1].iter().rev() {
                    ring.push(pt);
                }
            } else {
                used[next_idx] = false;
                continue;
            }
            let candidates = endpoint_map
                .get(ring.last().expect("nonempty ring"))
                .map(Vec::as_slice)
                .unwrap_or_default();
            search.push(SearchFrame {
                candidates,
                next_candidate: 0,
                chosen_segment: Some(next_idx),
                ring_len_before,
            });
        }

        // Only keep closed rings
        if ring.len() > 2 && ring[ring.len() - 1] == ring_start {
            rings.push(ring);
        } else {
            used[start_idx] = false;
        }
    }

    Ok(rings)
}

/// Compute the signed area of a ring in decimicrodegree² units.
///
/// Tuples are `(lat_e7, lon_e7)`, matching [`assemble_rings`]. Positive means
/// counter-clockwise on the map (exterior), negative means clockwise (hole).
/// Uses a ring-local origin to avoid cancellation of world-scale coordinates.
pub fn signed_area(ring: &[(i32, i32)]) -> f64 {
    if ring.len() < 3 {
        return 0.0;
    }
    let mut area = 0.0_f64;
    let origin_lat = f64::from(ring[0].0);
    let origin_lon = f64::from(ring[0].1);
    let n = ring.len();
    for i in 0..n {
        let j = (i + 1) % n;
        let (xi, yi) = (
            f64::from(ring[i].1) - origin_lon,
            f64::from(ring[i].0) - origin_lat,
        );
        let (xj, yj) = (
            f64::from(ring[j].1) - origin_lon,
            f64::from(ring[j].0) - origin_lat,
        );
        area += xi * yj;
        area -= xj * yi;
    }
    area / 2.0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- point_in_ring -------------------------------------------------------

    #[test]
    fn point_in_square() {
        let square = vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)];
        assert!(point_in_ring(0.5, 0.5, &square));
        assert!(!point_in_ring(2.0, 0.5, &square));
        assert!(!point_in_ring(0.5, 2.0, &square));
        assert!(!point_in_ring(-0.5, 0.5, &square));
    }

    #[test]
    fn point_in_triangle() {
        let triangle = vec![(0.0, 0.0), (4.0, 0.0), (2.0, 3.0), (0.0, 0.0)];
        assert!(point_in_ring(2.0, 1.0, &triangle));
        assert!(!point_in_ring(0.0, 3.0, &triangle));
        assert!(!point_in_ring(5.0, 1.0, &triangle));
    }

    #[test]
    fn point_in_concave() {
        let l_shape = vec![
            (0.0, 0.0),
            (2.0, 0.0),
            (2.0, 1.0),
            (1.0, 1.0),
            (1.0, 2.0),
            (0.0, 2.0),
            (0.0, 0.0),
        ];
        assert!(point_in_ring(1.5, 0.5, &l_shape));
        assert!(point_in_ring(0.5, 1.5, &l_shape));
        assert!(!point_in_ring(1.5, 1.5, &l_shape));
        assert!(!point_in_ring(3.0, 1.0, &l_shape));
    }

    #[test]
    fn point_in_ring_degenerate() {
        assert!(!point_in_ring(0.0, 0.0, &[]));
        assert!(!point_in_ring(0.0, 0.0, &[(0.0, 0.0), (1.0, 1.0)]));
    }

    #[test]
    fn point_in_ring_antimeridian() {
        let ring = vec![
            (179.0, 10.0),
            (-179.0, 10.0),
            (-179.0, 12.0),
            (179.0, 12.0),
            (179.0, 10.0),
        ];
        assert!(point_in_ring_with_antimeridian(179.5, 11.0, &ring));
        assert!(point_in_ring_with_antimeridian(-179.5, 11.0, &ring));
        assert!(!point_in_ring_with_antimeridian(0.0, 11.0, &ring));
    }

    /// The guard added for the implicit closing edge needs a fixture where
    /// only that edge crosses. The original fixture stepped -179 -> -170 ->
    /// 179, whose SECOND window is already a 349-degree jump, so the
    /// pre-existing `windows(2)` scan answered true and the new clause was
    /// never reached: it passed identically with the fix reverted.
    ///
    /// Here every explicit step walks through 0 and stays under 180. Only
    /// the closing segment, from 179 back to -179, crosses.
    #[test]
    fn unclosed_ring_checks_implicit_closing_segment_for_antimeridian() {
        let ring = vec![
            (-179.0, 0.0),
            (-90.0, 1.0),
            (0.0, 2.0),
            (90.0, 3.0),
            (179.0, 4.0),
        ];
        assert!(
            ring.windows(2)
                .all(|s: &[(f64, f64)]| (s[1].0 - s[0].0).abs() <= 180.0),
            "fixture is only meaningful while no explicit segment crosses"
        );
        assert!(ring_crosses_antimeridian(&ring));
    }

    /// A ring that crosses nothing must stay false, closing edge included -
    /// the clause must not be a blanket "first and last differ".
    #[test]
    fn ring_without_any_crossing_stays_false_including_the_closing_edge() {
        let ring = vec![(10.0, 0.0), (20.0, 1.0), (30.0, 2.0), (15.0, 3.0)];
        assert!(!ring_crosses_antimeridian(&ring));
    }

    // -- point_in_polygon (with holes) ---------------------------------------

    #[test]
    fn polygon_with_hole() {
        // Outer square (0,0)-(10,10), hole (3,3)-(7,7)
        let exterior = vec![
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
            (0.0, 0.0),
        ];
        let hole = vec![(3.0, 3.0), (7.0, 3.0), (7.0, 7.0), (3.0, 7.0), (3.0, 3.0)];
        // Inside exterior, outside hole
        assert!(point_in_polygon(1.0, 1.0, &exterior, &[&hole]));
        // Inside hole
        assert!(!point_in_polygon(5.0, 5.0, &exterior, &[&hole]));
        // Outside exterior
        assert!(!point_in_polygon(11.0, 5.0, &exterior, &[&hole]));
    }

    // -- distance calculations -----------------------------------------------

    #[test]
    fn distance_same_point() {
        let d = approx_distance_sq(1.0, 2.0, 1.0, 2.0, 1.0_f64.cos());
        assert!(d < 1e-20);
    }

    #[test]
    fn distance_uses_short_antimeridian_delta() {
        let lat = 10.0_f64.to_radians();
        let west = 179.9_f64.to_radians();
        let east = (-179.9_f64).to_radians();
        let actual = approx_distance_sq(lat, west, lat, east, lat.cos()).sqrt();
        let expected = 0.2_f64.to_radians() * lat.cos();
        assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
    }

    /// The E7 helpers are the domain twin of the radian unwrapping above.
    /// They exist because `point_to_segment_distance_sq` returns a `t` in an
    /// unwrapped frame, and any caller interpolating raw E7 coordinates with
    /// that `t` silently takes the long way around the globe.
    #[test]
    fn e7_longitude_helpers_take_the_short_arc() {
        // 179.9E to 179.9W is 0.2 degrees east, not 359.8 west.
        assert_eq!(
            wrapped_longitude_delta_e7(-1_799_000_000 - 1_799_000_000),
            2_000_000
        );
        // And the reverse.
        assert_eq!(
            wrapped_longitude_delta_e7(1_799_000_000 - -1_799_000_000),
            -2_000_000
        );
        // A delta entirely inside the domain is untouched.
        assert_eq!(wrapped_longitude_delta_e7(5_000_000), 5_000_000);

        // Midpoint of that segment, computed in the unwrapped frame, is the
        // antimeridian itself - the value the old code produced as 0.
        let midpoint = 1_799_000_000 + 1_000_000;
        assert_eq!(normalize_longitude_e7(midpoint), 1_800_000_000);
        // Past the antimeridian folds to the western side.
        assert_eq!(normalize_longitude_e7(1_801_000_000), -1_799_000_000);
        // Legal longitudes are fixed points.
        assert_eq!(normalize_longitude_e7(125_000_000), 125_000_000);
        assert_eq!(normalize_longitude_e7(-1_799_000_000), -1_799_000_000);
    }

    #[test]
    fn segment_distance_crosses_antimeridian_on_short_arc() {
        let lat = 10.0_f64.to_radians();
        let (t, distance) = point_to_segment_distance_sq(
            PI,
            lat,
            179.9_f64.to_radians(),
            lat,
            (-179.9_f64).to_radians(),
            lat,
            lat.cos(),
        );
        assert!((t - 0.5).abs() < 1e-12, "t={t}");
        assert!(distance < 1e-24, "distance={distance}");
    }

    #[test]
    fn e7_conversion() {
        let rad = e7_to_rad(556_761_000); // ~55.6761 degrees
        let deg = rad * 180.0 / PI;
        assert!((deg - 55.6761).abs() < 1e-6);
    }

    #[test]
    fn segment_distance_perpendicular() {
        // Segment from (0, 0) to (1, 0) in radians, query at (0.5, 0.1)
        let cos_lat = 1.0; // At equator
        let (t, _dist_sq) = point_to_segment_distance_sq(0.5, 0.1, 0.0, 0.0, 1.0, 0.0, cos_lat);
        assert!((t - 0.5).abs() < 0.01); // Closest at midpoint
    }

    #[test]
    fn segment_distance_endpoint() {
        // Segment from (0, 0) to (1, 0), query at (-0.5, 0)
        let cos_lat = 1.0;
        let (t, _) = point_to_segment_distance_sq(-0.5, 0.0, 0.0, 0.0, 1.0, 0.0, cos_lat);
        assert!(t < 0.01); // Clamped to start
    }

    // -- simplify_ring -------------------------------------------------------

    #[test]
    fn simplify_already_small() {
        let ring = vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)];
        let result = simplify_ring(&ring, 10);
        assert_eq!(result.len(), ring.len());
    }

    #[test]
    fn simplify_reduces_vertices() {
        // A ring with many points that can be simplified
        let mut ring = Vec::new();
        for i in 0..100 {
            let angle = i as f64 * 2.0 * PI / 100.0;
            ring.push((angle.cos(), angle.sin()));
        }
        ring.push(ring[0]); // Close the ring

        let result = simplify_ring(&ring, 20);
        assert!(result.len() <= 20);
        assert!(result.len() >= 3);
    }

    /// Reference implementation: the recursive Douglas-Peucker marker that
    /// the explicit-stack version replaced, transcribed verbatim (sqrt
    /// distance against a non-squared epsilon, as it was).
    fn dp_mark_recursive_reference(
        pts: &[(f64, f64)],
        start: usize,
        end: usize,
        epsilon: f64,
        keep: &mut [bool],
    ) {
        if end <= start + 1 {
            return;
        }
        let (ax, ay) = pts[start];
        let (bx, by) = pts[end];
        let dx = bx - ax;
        let dy = by - ay;
        let len_sq = dx * dx + dy * dy;
        let mut max_dist = 0.0_f64;
        let mut max_idx = start;
        for (i, pt) in pts.iter().enumerate().take(end).skip(start + 1) {
            let px = pt.0 - ax;
            let py = pt.1 - ay;
            let dist = if len_sq < crate::tunables::DEGENERATE_SEGMENT_EPSILON {
                (px * px + py * py).sqrt()
            } else {
                let t = ((px * dx + py * dy) / len_sq).clamp(0.0, 1.0);
                let proj_x = t * dx - px;
                let proj_y = t * dy - py;
                (proj_x * proj_x + proj_y * proj_y).sqrt()
            };
            if dist > max_dist {
                max_dist = dist;
                max_idx = i;
            }
        }
        if max_dist > epsilon {
            keep[max_idx] = true;
            dp_mark_recursive_reference(pts, start, max_idx, epsilon, keep);
            dp_mark_recursive_reference(pts, max_idx, end, epsilon, keep);
        }
    }

    /// Replacing recursion with a stack must not move a single vertex.
    /// A stack-depth test cannot see a geometry change, and a changed
    /// simplification would be invisible right up to the point-in-polygon
    /// answers it feeds, so the equivalence is asserted directly - kept-set
    /// for the marker, count for the epsilon search - over shapes chosen to
    /// exercise both the collinear and the degenerate-chord branches.
    #[test]
    fn iterative_douglas_peucker_matches_the_recursive_version_exactly() {
        let mut state = 0x1234_5678_9abc_def0_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut rings: Vec<Vec<(f64, f64)>> = Vec::new();
        // Comb: worst case for recursion depth, and every split peels one.
        rings.push((0..400).map(|i| (f64::from(i), f64::from(i % 2))).collect());
        // Perfectly collinear: no split ever exceeds epsilon.
        rings.push((0..50).map(|i| (f64::from(i), 0.0)).collect());
        // Degenerate chord: first and last coincide, so len_sq < 1e-30.
        rings.push(
            (0..60)
                .map(|i| {
                    let a = f64::from(i) / 59.0 * std::f64::consts::TAU;
                    (a.cos(), a.sin())
                })
                .collect(),
        );
        // Random scatter.
        for _ in 0..8 {
            rings.push(
                (0..120)
                    .map(|_| {
                        #[allow(clippy::cast_precision_loss)]
                        let x = (next() % 10_000) as f64 / 100.0;
                        #[allow(clippy::cast_precision_loss)]
                        let y = (next() % 10_000) as f64 / 100.0;
                        (x, y)
                    })
                    .collect(),
            );
        }

        for ring in &rings {
            for &epsilon in &[0.0, 1e-9, 0.001, 0.05, 0.5, 3.0, 100.0] {
                let last = ring.len() - 1;
                let mut iterative = vec![false; ring.len()];
                dp_mark(ring, 0, last, epsilon, &mut iterative);
                let mut recursive = vec![false; ring.len()];
                dp_mark_recursive_reference(ring, 0, last, epsilon, &mut recursive);
                assert_eq!(
                    iterative,
                    recursive,
                    "dp_mark diverged at epsilon {epsilon} for a {}-point ring",
                    ring.len()
                );
                let expected = 2 + recursive.iter().filter(|k| **k).count();
                assert_eq!(
                    dp_count(ring, epsilon),
                    expected,
                    "dp_count diverged at epsilon {epsilon}"
                );
            }
        }
    }

    #[test]
    fn simplify_ring_handles_deep_path_without_recursion() {
        let ring: Vec<_> = (0..20_000)
            .map(|i| (i as f64, if i % 2 == 0 { 0.0 } else { 1.0 }))
            .collect();
        let simplified = simplify_ring(&ring, 64);
        assert!(simplified.len() <= 64);
        assert_eq!(simplified.first(), ring.first());
        assert_eq!(simplified.last(), ring.last());
    }

    // -- assemble_rings ------------------------------------------------------

    #[test]
    fn assemble_single_ring() {
        // Three segments forming a triangle
        let s1: Vec<(i32, i32)> = vec![(0, 0), (10, 0)];
        let s2: Vec<(i32, i32)> = vec![(10, 0), (5, 10)];
        let s3: Vec<(i32, i32)> = vec![(5, 10), (0, 0)];
        let segments: Vec<&[(i32, i32)]> = vec![&s1, &s2, &s3];

        let rings = assemble_rings(&segments);
        assert_eq!(rings.len(), 1);
        assert_eq!(rings[0].len(), 4); // 3 segments = 4 points (closed)
        assert_eq!(rings[0][0], rings[0][3]); // Closed
    }

    #[test]
    fn assemble_reversed_segment() {
        // Second segment is reversed
        let s1: Vec<(i32, i32)> = vec![(0, 0), (10, 0)];
        let s2: Vec<(i32, i32)> = vec![(5, 10), (10, 0)]; // reversed
        let s3: Vec<(i32, i32)> = vec![(5, 10), (0, 0)];
        let segments: Vec<&[(i32, i32)]> = vec![&s1, &s2, &s3];

        let rings = assemble_rings(&segments);
        assert_eq!(rings.len(), 1);
    }

    #[test]
    fn assemble_unclosed_dropped() {
        // Two segments that don't form a closed ring
        let s1: Vec<(i32, i32)> = vec![(0, 0), (10, 0)];
        let s2: Vec<(i32, i32)> = vec![(10, 0), (5, 10)];
        let segments: Vec<&[(i32, i32)]> = vec![&s1, &s2];

        let rings = assemble_rings(&segments);
        assert!(rings.is_empty());
    }

    #[test]
    fn assemble_empty() {
        let rings = assemble_rings(&[]);
        assert!(rings.is_empty());
    }

    // -- signed_area ---------------------------------------------------------

    /// Rings reach `signed_area` from `assemble_rings`, which produces
    /// `(lat_e7, lon_e7)` tuples. The shoelace therefore has to read the
    /// second component as x and the first as y; reading them positionally
    /// as `(x, y)` inverts the sign for every real caller.
    ///
    /// This fixture is clockwise ON THE MAP: as `(lon, lat)` it walks
    /// (0,0) -> (0,10) -> (10,10) -> (10,0), which turns right.
    #[test]
    fn signed_area_is_negative_for_a_clockwise_map_ring() {
        let clockwise_on_the_map = vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)];
        assert!(signed_area(&clockwise_on_the_map) < 0.0);
    }

    /// Counter-clockwise counterpart, same `(lat, lon)` reading.
    #[test]
    fn signed_area_is_positive_for_a_counter_clockwise_map_ring() {
        let ccw_on_the_map = vec![(0, 0), (0, 10), (10, 10), (10, 0), (0, 0)];
        assert!(signed_area(&ccw_on_the_map) > 0.0);
    }

    /// The canary. It was written when the convention was inverted, pinning
    /// the wrong sign so that correcting it could not land silently; it has
    /// now been flipped to pin the CORRECT sign, and it keeps that job.
    ///
    /// Nothing notices a sign error today because the only call site
    /// (`geocode_index/builder/admin.rs`) immediately takes `.abs()`. That
    /// is exactly why this test has to exist: the next caller that reads the
    /// sign must not walk into an undocumented inversion, and any future
    /// change of convention must fail here rather than pass quietly.
    #[test]
    fn signed_area_sign_matches_real_assemble_rings_output() {
        // A square that is counter-clockwise on the map, written in the
        // `(lat_e7, lon_e7)` order `assemble_rings` produces: going
        // (lon 0, lat 0) -> (10, 0) -> (10, 10) -> (0, 10).
        let ccw_on_the_map = vec![(0, 0), (0, 10), (10, 10), (10, 0), (0, 0)];
        assert!(
            signed_area(&ccw_on_the_map) > 0.0,
            "counter-clockwise map ring in (lat, lon) order must have positive area"
        );
        // And the mirror image must be the opposite sign, not merely
        // nonzero - a magnitude-only assertion would survive the inversion
        // this canary exists to catch.
        let mut reversed = ccw_on_the_map.clone();
        reversed.reverse();
        assert!(signed_area(&reversed) < 0.0);
        assert!(
            (signed_area(&reversed) + signed_area(&ccw_on_the_map)).abs() < f64::EPSILON,
            "reversing a ring must negate its area exactly"
        );
    }

    /// The local-origin subtraction must not change the answer, only the
    /// conditioning. Planet-scale coordinates are where naive shoelace
    /// cancellation loses precision, so the check runs there.
    #[test]
    fn signed_area_is_translation_invariant_at_world_scale() {
        let ring = vec![(0, 0), (0, 1000), (1000, 1000), (1000, 0), (0, 0)];
        let shifted: Vec<(i32, i32)> = ring
            .iter()
            .map(|&(lat, lon)| (lat + 500_000_000, lon + 1_700_000_000))
            .collect();
        let a = signed_area(&ring);
        let b = signed_area(&shifted);
        assert!(a > 0.0);
        assert!(
            (a - b).abs() / a < 1e-9,
            "world-scale translation changed the area: {a} vs {b}"
        );
    }

    #[test]
    fn signed_area_degenerate() {
        assert!(signed_area(&[]).abs() < f64::EPSILON);
        assert!(signed_area(&[(0, 0), (1, 1)]).abs() < f64::EPSILON);
    }

    #[cfg(feature = "commands")]
    #[test]
    fn bounded_assembly_refuses_an_exponential_search() {
        // Starting at a leaf forces a failed search through the complete
        // graph, and enumerating its trails grows factorially. The bounded
        // search gives up on the whole input instead of returning the
        // disconnected triangle alone.
        let mut segments = vec![vec![(-1, -1), (0, 0)]];
        for a in 0..12 {
            for b in a + 1..12 {
                segments.push(vec![(a, a), (b, b)]);
            }
        }
        segments.push(vec![(100, 100), (100, 101), (101, 100), (100, 100)]);
        let slices: Vec<_> = segments.iter().map(Vec::as_slice).collect();
        assert_eq!(
            assemble_rings_bounded(&slices),
            Err(RingSearchBudgetExceeded)
        );
    }

    #[cfg(feature = "commands")]
    #[test]
    fn bounded_assembly_matches_unbounded_within_budget() {
        // Two triangles touching at (5, 5). Full backtracking walks one
        // figure-eight ring through the touching point; a search that
        // pruned revisited endpoints would return two triangles instead.
        // The bounded search must keep the unbounded answer.
        let s0 = [(0, 0), (10, 0)];
        let s1 = [(10, 0), (5, 5)];
        let s2 = [(5, 5), (10, 10)];
        let s3 = [(10, 10), (0, 10)];
        let s4 = [(0, 10), (5, 5)];
        let s5 = [(5, 5), (0, 0)];
        let segments: [&[(i32, i32)]; 6] = [&s0, &s1, &s2, &s3, &s4, &s5];
        let unbounded = assemble_rings(&segments);
        assert_eq!(
            unbounded,
            vec![vec![
                (0, 0),
                (10, 0),
                (5, 5),
                (10, 10),
                (0, 10),
                (5, 5),
                (0, 0)
            ]]
        );
        assert_eq!(assemble_rings_bounded(&segments), Ok(unbounded));
    }

    #[cfg(feature = "commands")]
    #[test]
    fn ring_search_budget_covers_every_non_branching_input() {
        // Without a branching endpoint each start places at most n - 1
        // segments, so n^2 is an upper bound before the headroom.
        assert_eq!(ring_search_budget(0), RING_SEARCH_EXTRA_STEPS);
        assert_eq!(
            ring_search_budget(1_000),
            1_000_000 + RING_SEARCH_EXTRA_STEPS
        );
        assert_eq!(ring_search_budget(usize::MAX), u64::MAX);
    }

    #[test]
    fn bbox_constructor_validates_numeric_input() {
        assert!(Bbox::try_new(-180.0, -90.0, 180.0, 90.0).is_ok());
        for values in [
            (f64::NAN, 0.0, 1.0, 1.0),
            (0.0, 0.0, f64::INFINITY, 1.0),
            (-181.0, 0.0, 1.0, 1.0),
            (0.0, -91.0, 1.0, 1.0),
            (1.0, 0.0, 1.0, 1.0),
            (0.0, 1.0, 1.0, 0.0),
        ] {
            assert!(Bbox::try_new(values.0, values.1, values.2, values.3).is_err());
        }
    }

    #[test]
    fn assemble_rings_backtracks_at_ambiguous_junction() {
        let start = [(0, 0), (0, 1)];
        let dead_end = [(0, 1), (0, 2)];
        let good = [(0, 1), (1, 1)];
        let close = [(1, 1), (0, 0)];
        let rings = assemble_rings(&[&start, &dead_end, &good, &close]);
        assert_eq!(rings.len(), 1);
        assert_eq!(rings[0], vec![(0, 0), (0, 1), (1, 1), (0, 0)]);
    }
}
