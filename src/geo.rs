//! Shared geometry primitives for spatial operations.
//!
//! Extracted from `commands/extract.rs` and extended with distance functions,
//! polygon simplification, and ring assembly for the geocode index builder.

use std::f64::consts::PI;

/// Mean Earth radius in meters (WGS84 volumetric mean).
pub const EARTH_RADIUS_M: f64 = 6_371_008.8;

// ---------------------------------------------------------------------------
// Point-in-polygon (ray casting)
// ---------------------------------------------------------------------------

/// Ray-casting point-in-polygon test.
///
/// Coordinates are `(x, y)` = `(longitude, latitude)` in degrees.
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
    if !point_in_ring_with_antimeridian(px, py, exterior) {
        return false;
    }
    !holes
        .iter()
        .any(|hole| point_in_ring_with_antimeridian(px, py, hole))
}

/// Detect whether any ring segment crosses the antimeridian.
pub fn ring_crosses_antimeridian(ring: &[(f64, f64)]) -> bool {
    if ring.len() < 2 {
        return false;
    }
    ring.windows(2)
        .any(|segment| (segment[1].0 - segment[0].0).abs() > 180.0)
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
    let dlon = lon2_rad - lon1_rad;
    dlat * dlat + dlon * dlon * cos_lat * cos_lat
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
    e7 as f64 * 1e-7 * PI / 180.0
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
    let dx = (bx - ax) * cos_lat;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;

    let t = if len_sq < 1e-30 {
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
        if x < min_x { min_x = x; }
        if x > max_x { max_x = x; }
        if y < min_y { min_y = y; }
        if y > max_y { max_y = y; }
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
    // 2 for first and last, plus recursive count of interior kept vertices
    2 + dp_count_range(ring, 0, ring.len() - 1, epsilon)
}

/// Count vertices kept by Douglas-Peucker in the open range (start, end).
fn dp_count_range(pts: &[(f64, f64)], start: usize, end: usize, epsilon: f64) -> usize {
    if end <= start + 1 {
        return 0;
    }
    let (ax, ay) = pts[start];
    let (bx, by) = pts[end];
    let dx = bx - ax;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;

    let mut max_dist = 0.0_f64;
    let mut max_idx = start + 1;
    for (i, &(px, py)) in pts.iter().enumerate().skip(start + 1).take(end - start - 1) {
        let dist = if len_sq < 1e-30 {
            let ex = px - ax;
            let ey = py - ay;
            ex * ex + ey * ey
        } else {
            let cross = (px - ax) * dy - (py - ay) * dx;
            (cross * cross) / len_sq
        };
        if dist > max_dist {
            max_dist = dist;
            max_idx = i;
        }
    }
    if max_dist > epsilon * epsilon {
        1 + dp_count_range(pts, start, max_idx, epsilon)
            + dp_count_range(pts, max_idx, end, epsilon)
    } else {
        0
    }
}

/// Mark vertices to keep using Douglas-Peucker recursion.
fn dp_mark(pts: &[(f64, f64)], start: usize, end: usize, epsilon: f64, keep: &mut [bool]) {
    if end <= start + 1 {
        return;
    }

    let ax = pts[start].0;
    let ay = pts[start].1;
    let bx = pts[end].0;
    let by = pts[end].1;
    let dx = bx - ax;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;

    let mut max_dist = 0.0_f64;
    let mut max_idx = start;

    for (i, pt) in pts.iter().enumerate().take(end).skip(start + 1) {
        let px = pt.0 - ax;
        let py = pt.1 - ay;
        let dist = if len_sq < 1e-30 {
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
        dp_mark(pts, start, max_idx, epsilon, keep);
        dp_mark(pts, max_idx, end, epsilon, keep);
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
/// **Limitation:** This is a greedy assembler — it follows the first available
/// continuation at each endpoint without backtracking. On ambiguous endpoint
/// graphs (where multiple unused segments share an endpoint), an unlucky
/// branch can consume segments into a dead-end chain and prevent a valid
/// closed ring from being assembled later. This is acceptable for OSM admin
/// boundary relations, which produce well-formed endpoint graphs in practice.
pub fn assemble_rings(segments: &[&[(i32, i32)]]) -> Vec<Vec<(i32, i32)>> {
    if segments.is_empty() {
        return Vec::new();
    }

    // Build endpoint map: quantized endpoint -> list of segment indices
    let mut endpoint_map: std::collections::HashMap<(i32, i32), Vec<usize>> =
        std::collections::HashMap::new();
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

        loop {
            let current_end = ring[ring.len() - 1];

            // Ring is closed
            if ring.len() > 2 && current_end == ring_start {
                break;
            }

            // Find an unused segment that connects at current_end
            let next = endpoint_map
                .get(&current_end)
                .and_then(|indices| {
                    indices.iter().find(|&&idx| !used[idx])
                })
                .copied();

            let Some(next_idx) = next else {
                break; // Dead end — chain is unclosed
            };

            used[next_idx] = true;
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
                break; // Should not happen
            }
        }

        // Only keep closed rings
        if ring.len() > 2 && ring[ring.len() - 1] == ring_start {
            rings.push(ring);
        }
    }

    rings
}

/// Compute the signed area of a ring in decimicrodegree² units.
///
/// Positive = counter-clockwise (exterior), negative = clockwise (hole).
/// Uses the shoelace formula.
pub fn signed_area(ring: &[(i32, i32)]) -> f64 {
    if ring.len() < 3 {
        return 0.0;
    }
    let mut area = 0.0_f64;
    let n = ring.len();
    for i in 0..n {
        let j = (i + 1) % n;
        let (xi, yi) = (ring[i].0 as f64, ring[i].1 as f64);
        let (xj, yj) = (ring[j].0 as f64, ring[j].1 as f64);
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
        let hole = vec![
            (3.0, 3.0),
            (7.0, 3.0),
            (7.0, 7.0),
            (3.0, 7.0),
            (3.0, 3.0),
        ];
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

    #[test]
    fn signed_area_ccw() {
        // CCW square: positive area
        let ring = vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)];
        assert!(signed_area(&ring) > 0.0);
    }

    #[test]
    fn signed_area_cw() {
        // CW square: negative area
        let ring = vec![(0, 0), (0, 10), (10, 10), (10, 0), (0, 0)];
        assert!(signed_area(&ring) < 0.0);
    }

    #[test]
    fn signed_area_degenerate() {
        assert!(signed_area(&[]).abs() < f64::EPSILON);
        assert!(signed_area(&[(0, 0), (1, 1)]).abs() < f64::EPSILON);
    }
}
