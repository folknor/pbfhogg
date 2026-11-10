//! Polygon regions: osmium extract's integer membership rule.
//!
//! A polygon region is prepared once, from f64 input rings, into integer
//! segments in decimicrodegrees, and every node is then decided by integer
//! arithmetic alone, as osmium-tool's `ExtractPolygon::contains`
//! (`research/osmium-tool/src/extract/extract_polygon.cpp`) decides it:
//!
//! - Each vertex is quantized as libosmium's `Location(double, double)` does,
//!   `round(v * 1e7)` on the f64 product, half away from zero.
//! - Before quantizing, an outer ring is reversed unless it is
//!   counterclockwise and an inner ring unless it is clockwise, by osmium's
//!   `calculate_double_area` over the input f64 vertices in input order.
//!   The edge rule below depends on segment direction, so this decides which
//!   nodes on an edge are inside.
//! - All segments of all rings of one geometry (a GeoJSON Polygon or
//!   MultiPolygon, one osmium area) share one crossing parity: a hole is a
//!   second crossing, and overlapping members of a MultiPolygon cancel.
//! - A node outside the WGS84 domain (osmium's `Location::valid`) or outside
//!   the inclusive integer envelope of the outer rings is outside. A node equal to any segment endpoint is inside. Otherwise each
//!   segment that straddles the node's latitude (one endpoint strictly
//!   above it, the other not) toggles membership when the node lies left of
//!   it, with the node exactly on an upward segment counted as left and on
//!   a downward one not; the comparison is a cross product in integers.
//! - The latitude range is cut into osmium's bands (ten segments a band, at
//!   most 10 000 bands) so a node is tested against the segments of its band
//!   only. The bands change the cost, never the answer.
//!
//! So on closed rings that do not cross the antimeridian, a region read from
//! one geometry selects exactly osmium's nodes. Where pbfhogg goes beyond
//! osmium (DEVIATIONS.md "extract: polygon input beyond osmium"):
//!
//! - Geometries are unioned: a FeatureCollection contributes every feature.
//! - An open ring is closed: osmium drops the closing segment of an open
//!   GeoJSON ring; here the first vertex is repeated, and the orientation
//!   is judged on the closed ring.
//! - A geometry with an edge spanning more than 180 degrees of longitude
//!   (osmium documents such polygons as unsupported) is periodic: see
//!   [`Periodic`].
//! - A ring with zero signed area, or with a coordinate outside the WGS84
//!   domain before rounding, is refused where osmium would accept it; the
//!   user gets an error, never a different extract.
//!
//! This is extract's own predicate. `crate::geo::point_in_polygon` is the
//! geocode index's f64 test on simplified admin rings, whose contract is not
//! osmium parity; the two are deliberately separate.

use crate::coord_fmt::from_decimicro;
use crate::error::Error;
use crate::geo::Bbox;

use super::Result;

/// A single polygon given as f64 input: exterior ring and holes, each a list
/// of `(lon, lat)` in degrees. The input to [`Polygon::new`].
pub struct PolygonRings {
    /// Exterior ring: Vec of (lon, lat) in degrees.
    pub exterior: Vec<(f64, f64)>,
    /// Interior rings (holes): Vec of rings, each a Vec of (lon, lat).
    pub holes: Vec<Vec<(f64, f64)>>,
}

/// One revolution of longitude, in decimicrodegrees.
const TURN: i64 = 3_600_000_000;
/// Half a revolution, in decimicrodegrees.
const HALF_TURN: i64 = 1_800_000_000;
/// osmium's band sizing (`segments_per_band`, `max_bands`), so the band a
/// segment lands in matches the reference's. A reference constant.
const SEGMENTS_PER_BAND: usize = 10;
const MAX_BANDS: usize = 10_000;

/// A prepared polygon region: one or more geometries, unioned.
///
/// Built by [`Polygon::new`], which validates and quantizes the input once;
/// the prepared form is immutable, so nothing is re-derived per node.
pub struct Polygon {
    geometries: Vec<Geometry>,
    bbox: Bbox,
}

enum Geometry {
    /// Every ring within one longitude window: osmium's area.
    Planar(Rings),
    /// Some ring crosses the antimeridian.
    Periodic(Periodic),
}

/// A geometry with a ring crossing the antimeridian. osmium declares such
/// polygons unsupported; pbfhogg supports them under these rules:
///
/// - Each member polygon is kept apart, and members are unioned, not
///   XORed as in a planar geometry.
/// - A member's outer ring is unwrapped from its first vertex, so every edge
///   spans at most 180 degrees; each hole is unwrapped the same way and then
///   moved by whole turns so its longitude midpoint lies within 180 degrees
///   of its own outer ring's. A ring whose unwrapped path does not return to
///   its start (it winds around a pole), or that spans 360 degrees or more,
///   is refused.
/// - A node is inside a member when it is inside under the planar rule at
///   its own longitude or one turn east or west of it.
///
/// The unwrapping is decided per vertex as a whole number of turns and added
/// to the quantized vertex, so every copy of a vertex keeps its column.
struct Periodic {
    members: Vec<Rings>,
}

/// Directed integer segments of a set of rings sharing one parity, with the
/// envelope of their outer rings and osmium's latitude bands.
struct Rings {
    min_x: i64,
    max_x: i64,
    min_y: i64,
    max_y: i64,
    dy: i64,
    /// `band_starts[b]..band_starts[b + 1]` indexes `banded` for band `b`.
    band_starts: Vec<usize>,
    banded: Vec<Segment>,
}

/// A directed segment from `(x1, y1)` to `(x2, y2)`, osmium's
/// `segment.first()` to `segment.second()`.
#[derive(Clone, Copy)]
struct Segment {
    x1: i64,
    y1: i64,
    x2: i64,
    y2: i64,
}

impl Segment {
    /// osmium's per-segment step: `Some(true)` when `(px, py)` is an
    /// endpoint (inside, final), else whether the segment toggles parity.
    #[inline]
    fn step(&self, px: i64, py: i64) -> Option<bool> {
        if (self.x1 == px && self.y1 == py) || (self.x2 == px && self.y2 == py) {
            return None;
        }
        if (self.y2 > py) == (self.y1 > py) {
            return Some(false);
        }
        let ax = i128::from(self.x1 - self.x2);
        let ay = i128::from(self.y1 - self.y2);
        let tx = i128::from(px - self.x2);
        let ty = i128::from(py - self.y2);
        let comp = tx * ay < ax * ty;
        Some((ay > 0) == comp)
    }
}

impl Rings {
    /// `rings` are closed, directed vertex lists, each flagged `true` when it
    /// is an outer ring; only outer rings bound the envelope, as in
    /// libosmium's `Area::envelope`.
    fn new(rings: &[(bool, Vec<(i64, i64)>)]) -> Self {
        let mut min_x = i64::MAX;
        let mut max_x = i64::MIN;
        let mut min_y = i64::MAX;
        let mut max_y = i64::MIN;
        let mut segments = Vec::new();
        for (outer, ring) in rings {
            if *outer {
                for &(x, y) in ring {
                    min_x = min_x.min(x);
                    max_x = max_x.max(x);
                    min_y = min_y.min(y);
                    max_y = max_y.max(y);
                }
            }
            segments.extend(ring.windows(2).map(|w| Segment {
                x1: w[0].0,
                y1: w[0].1,
                x2: w[1].0,
                y2: w[1].1,
            }));
        }
        // Bands as osmium sizes them, num_bands + 1 of them so y_max has its
        // own. A latitude span that quantizes to zero still gets a step of
        // one (osmium would divide by zero), and a hole segment reaching
        // outside the outer envelope is clamped into the band range, where
        // it can only matter to a node it straddles or ends on.
        let num_bands = (segments.len() / SEGMENTS_PER_BAND).clamp(1, MAX_BANDS);
        let nb = i64::try_from(num_bands).unwrap_or(1);
        let dy = ((max_y - min_y + nb - 1) / nb).max(1);
        let band_of =
            |y: i64| -> usize { usize::try_from(((y - min_y) / dy).clamp(0, nb)).unwrap_or(0) };
        let mut counts = vec![0_usize; num_bands + 2];
        for s in &segments {
            let (lo, hi) = (band_of(s.y1.min(s.y2)), band_of(s.y1.max(s.y2)));
            for c in &mut counts[lo + 1..=hi + 1] {
                *c += 1;
            }
        }
        for b in 1..counts.len() {
            counts[b] += counts[b - 1];
        }
        let band_starts = counts.clone();
        let mut fill = counts;
        let mut banded = vec![
            Segment {
                x1: 0,
                y1: 0,
                x2: 0,
                y2: 0,
            };
            band_starts[num_bands + 1]
        ];
        for s in &segments {
            let (lo, hi) = (band_of(s.y1.min(s.y2)), band_of(s.y1.max(s.y2)));
            for slot in &mut fill[lo..=hi] {
                banded[*slot] = *s;
                *slot += 1;
            }
        }
        Self {
            min_x,
            max_x,
            min_y,
            max_y,
            dy,
            band_starts,
            banded,
        }
    }

    /// osmium's `ExtractPolygon::contains` on an integer point.
    fn contains(&self, px: i64, py: i64) -> bool {
        if px < self.min_x || px > self.max_x || py < self.min_y || py > self.max_y {
            return false;
        }
        let band = usize::try_from((py - self.min_y) / self.dy).unwrap_or(0);
        let segments = &self.banded[self.band_starts[band]..self.band_starts[band + 1]];
        let mut inside = false;
        for segment in segments {
            match segment.step(px, py) {
                None => return true,
                Some(true) => inside = !inside,
                Some(false) => {}
            }
        }
        inside
    }
}

impl Periodic {
    fn contains(&self, px: i64, py: i64) -> bool {
        self.members
            .iter()
            .any(|m| [0, TURN, -TURN].iter().any(|&s| m.contains(px + s, py)))
    }
}

impl Polygon {
    /// Prepare a polygon region from geometries, each one GeoJSON Polygon
    /// (one entry) or MultiPolygon (one entry per member). Geometries are
    /// unioned; the rings of one planar geometry share osmium's single
    /// parity, while the members of a geometry crossing the antimeridian are
    /// unioned (see [`Periodic`]).
    ///
    /// Refuses a ring with fewer than three positions, a coordinate that is
    /// not finite or outside the WGS84 domain, a ring whose signed area is
    /// zero, a crossing ring that winds around a pole or spans 360 degrees,
    /// and a region with no polygon at all.
    pub fn new(geometries: &[Vec<PolygonRings>]) -> Result<Self> {
        let mut prepared = Vec::with_capacity(geometries.len());
        let mut lat = (i64::MAX, i64::MIN);
        let mut lon = (i64::MAX, i64::MIN);
        let mut any_periodic = false;
        for geometry in geometries {
            for poly in geometry {
                validate_ring(&poly.exterior)?;
                for hole in &poly.holes {
                    validate_ring(hole)?;
                }
            }
            if geometry.is_empty() {
                continue;
            }
            let crossing = geometry.iter().any(|p| {
                crate::geo::ring_crosses_antimeridian(&p.exterior)
                    || p.holes
                        .iter()
                        .any(|h| crate::geo::ring_crosses_antimeridian(h))
            });
            let g = if crossing {
                any_periodic = true;
                Geometry::Periodic(prepare_periodic(geometry)?)
            } else {
                Geometry::Planar(prepare_planar(geometry))
            };
            let rings: &[Rings] = match &g {
                Geometry::Planar(r) => std::slice::from_ref(r),
                Geometry::Periodic(p) => &p.members,
            };
            for r in rings {
                lat = (lat.0.min(r.min_y), lat.1.max(r.max_y));
                lon = (lon.0.min(r.min_x), lon.1.max(r.max_x));
            }
            prepared.push(g);
        }
        if prepared.is_empty() {
            return Err(Error::invalid_data(
                "no exterior ring vertices found for bounding box",
            ));
        }
        // A periodic geometry needs both longitude lobes, and Region::bbox
        // and blob scheduling take one ordinary interval, so its region box
        // spans every longitude: conservative, it costs skip efficiency and
        // never a node.
        if any_periodic {
            lon = (-HALF_TURN, HALF_TURN);
        }
        let to_deg = |v: i64| from_decimicro(i32::try_from(v).unwrap_or(0));
        let bbox = Bbox {
            min_lon: to_deg(lon.0),
            min_lat: to_deg(lat.0),
            max_lon: to_deg(lon.1),
            max_lat: to_deg(lat.1),
        };
        Ok(Self {
            geometries: prepared,
            bbox,
        })
    }

    /// The region's envelope: each edge is `q / 1e7` for the integer edge
    /// `q` membership uses, so `BboxInt::inside_bbox` of it is that integer
    /// envelope exactly.
    pub fn bbox(&self) -> &Bbox {
        &self.bbox
    }

    /// Whether a decimicrodegree point is inside the region.
    pub(crate) fn contains_decimicro(&self, lat: i32, lon: i32) -> bool {
        self.contains_fix(i64::from(lon), i64::from(lat))
    }

    /// Whether an f64 point in degrees is inside, the point quantized by
    /// the vertex rule (`round(v * 1e7)`) so an input vertex queried as f64
    /// lands on its own column, as does any f64 made from a stored
    /// decimicrodegree. A nonfinite point is outside.
    pub(crate) fn contains_degrees(&self, lat: f64, lon: f64) -> bool {
        if !lat.is_finite() || !lon.is_finite() {
            return false;
        }
        self.contains_fix(to_fix(lon), to_fix(lat))
    }

    /// osmium's `contains` starts with `location.valid()`: a point outside
    /// the WGS84 domain is outside. That also bounds the probes of a
    /// periodic geometry, which add a turn to the longitude.
    fn contains_fix(&self, x: i64, y: i64) -> bool {
        if !(-HALF_TURN..=HALF_TURN).contains(&x) || !(-HALF_TURN / 2..=HALF_TURN / 2).contains(&y)
        {
            return false;
        }
        self.geometries.iter().any(|g| match g {
            Geometry::Planar(r) => r.contains(x, y),
            Geometry::Periodic(p) => p.contains(x, y),
        })
    }
}

/// libosmium's `Location::double_to_fix`: `round(v * 1e7)`, the f64 product
/// rounded half away from zero. Not `geo::quantize_f64`, which rounds the
/// exact binary value and so differs from osmium at a half-unit; a GeoJSON
/// vertex must land on the column osmium puts it on.
#[allow(clippy::cast_possible_truncation)]
fn to_fix(v: f64) -> i64 {
    (v * 1e7).round() as i64
}

/// The ring with its first position repeated at the end if it is open.
fn closed(ring: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut out = ring.to_vec();
    if ring.first() != ring.last() {
        out.push(ring[0]);
    }
    out
}

/// osmium-tool's `calculate_double_area` (`geometry_util.cpp`), term for
/// term in the same order, so the reversal decision is osmium's own.
fn double_area(ring: &[(f64, f64)]) -> f64 {
    let mut total = 0.0;
    for w in ring.windows(2) {
        total += w[0].0 * w[1].1 - w[1].0 * w[0].1;
    }
    total
}

/// Orient a closed ring as osmium does: outer counterclockwise, inner
/// clockwise, judged on its f64 vertices.
fn orient(ring: &mut [(f64, f64)], outer: bool) {
    let ccw = double_area(ring) > 0.0;
    if ccw != outer {
        ring.reverse();
    }
}

fn prepare_planar(geometry: &[PolygonRings]) -> Rings {
    let mut rings = Vec::new();
    for poly in geometry {
        for (outer, ring) in
            std::iter::once((true, &poly.exterior)).chain(poly.holes.iter().map(|h| (false, h)))
        {
            let mut ring = closed(ring);
            orient(&mut ring, outer);
            rings.push((
                outer,
                ring.iter().map(|&(x, y)| (to_fix(x), to_fix(y))).collect(),
            ));
        }
    }
    Rings::new(&rings)
}

/// Whole turns to add to each vertex of a closed ring so no edge spans more
/// than half a turn, starting from zero at the first vertex. Refuses a ring
/// whose unwrapped path does not return to its start.
fn unwrap_turns(ring: &[(f64, f64)]) -> Result<Vec<i64>> {
    let mut turns = Vec::with_capacity(ring.len());
    let mut k = 0_i64;
    let mut prev = ring[0].0;
    turns.push(0);
    for &(lon, _) in &ring[1..] {
        // Validated longitudes are within one turn of each other, so at most
        // one step either way.
        #[allow(clippy::cast_precision_loss)]
        let lifted = lon + 360.0 * k as f64;
        if lifted - prev > 180.0 {
            k -= 1;
        } else if lifted - prev < -180.0 {
            k += 1;
        }
        #[allow(clippy::cast_precision_loss)]
        let lifted = lon + 360.0 * k as f64;
        prev = lifted;
        turns.push(k);
    }
    if turns.last() != Some(&0) {
        return Err(Error::invalid_data(
            "polygon ring crossing the antimeridian winds around a pole; \
             split it into rings that close without circling the globe",
        ));
    }
    Ok(turns)
}

/// A closed ring unwrapped: f64 lifted vertices for orientation, integer
/// lifted vertices for the segments, and the integer longitude range.
struct Lifted {
    f64s: Vec<(f64, f64)>,
    ints: Vec<(i64, i64)>,
}

impl Lifted {
    fn new(ring: &[(f64, f64)]) -> Result<Self> {
        let ring = closed(ring);
        let turns = unwrap_turns(&ring)?;
        #[allow(clippy::cast_precision_loss)]
        let f64s = ring
            .iter()
            .zip(&turns)
            .map(|(&(x, y), &k)| (x + 360.0 * k as f64, y))
            .collect();
        let ints = ring
            .iter()
            .zip(&turns)
            .map(|(&(x, y), &k)| (to_fix(x) + k * TURN, to_fix(y)))
            .collect();
        let lifted = Self { f64s, ints };
        let (lo, hi) = lifted.x_range();
        if hi - lo >= TURN {
            return Err(Error::invalid_data(
                "polygon ring crossing the antimeridian spans 360 degrees of longitude or more",
            ));
        }
        Ok(lifted)
    }

    fn x_range(&self) -> (i64, i64) {
        self.ints
            .iter()
            .fold((i64::MAX, i64::MIN), |(lo, hi), &(x, _)| {
                (lo.min(x), hi.max(x))
            })
    }

    /// Move the ring by `k` whole turns.
    fn shift(&mut self, k: i64) {
        #[allow(clippy::cast_precision_loss)]
        let deg = 360.0 * k as f64;
        for p in &mut self.f64s {
            p.0 += deg;
        }
        for p in &mut self.ints {
            p.0 += k * TURN;
        }
    }

    fn oriented(mut self, outer: bool) -> (bool, Vec<(i64, i64)>) {
        let ccw = double_area(&self.f64s) > 0.0;
        if ccw != outer {
            self.ints.reverse();
        }
        (outer, self.ints)
    }
}

fn prepare_periodic(geometry: &[PolygonRings]) -> Result<Periodic> {
    let mut members = Vec::with_capacity(geometry.len());
    for poly in geometry {
        let outer = Lifted::new(&poly.exterior)?;
        let (lo, hi) = outer.x_range();
        let mid = lo + (hi - lo) / 2;
        let mut rings = vec![outer.oriented(true)];
        for hole in &poly.holes {
            let mut hole = Lifted::new(hole)?;
            let (hlo, hhi) = hole.x_range();
            let hmid = hlo + (hhi - hlo) / 2;
            // The k whose shift brings the hole's midpoint within half a
            // turn of its own outer's; a hole inside its outer has exactly
            // one such lift.
            let k = (mid - hmid + HALF_TURN).div_euclid(TURN);
            hole.shift(k);
            rings.push(hole.oriented(false));
        }
        members.push(Rings::new(&rings));
    }
    Ok(Periodic { members })
}

/// The ring checks pbfhogg refuses on, where osmium would select from a
/// ring with no interior or round an out-of-domain value into range.
fn validate_ring(ring: &[(f64, f64)]) -> Result<()> {
    // A one- or two-point ring has a bounding box but no interior, so
    // accepting it turns a malformed polygon into a successful empty
    // extract. Open rings are accepted and closed.
    if ring.len() < 3 {
        return Err(Error::invalid_data(
            "polygon ring must contain at least 3 coordinate positions",
        ));
    }
    for &(lon, lat) in ring {
        crate::coord_pack::validate_lon("polygon longitude", lon)
            .map_err(Error::invalid_data_from)?;
        crate::coord_pack::validate_lat("polygon latitude", lat)
            .map_err(Error::invalid_data_from)?;
    }
    // Self-intersections are deliberately not rejected: osmium extract
    // accepts them too, and the shared crossing parity decides them the same
    // way. A ring can still collapse to a line or point (fewer than three
    // distinct positions, or all collinear) and so have no interior to
    // select; so can a ring whose signed area cancels, including a
    // symmetric bow-tie. Taking the shoelace sum relative to the first
    // position makes the closing edge's term zero, so this is the full area
    // whether or not the input repeats its first position.
    let origin = ring[0];
    let twice_area = ring
        .windows(2)
        .map(|edge| {
            (edge[0].0 - origin.0) * (edge[1].1 - origin.1)
                - (edge[1].0 - origin.0) * (edge[0].1 - origin.1)
        })
        .sum::<f64>();
    if twice_area == 0.0 {
        return Err(Error::invalid_data(
            "polygon ring must enclose a non-zero area",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn ring(points: &[(f64, f64)]) -> Vec<(f64, f64)> {
        points.to_vec()
    }

    fn one(exterior: Vec<(f64, f64)>, holes: Vec<Vec<(f64, f64)>>) -> Polygon {
        Polygon::new(&[vec![PolygonRings { exterior, holes }]]).unwrap()
    }

    fn at(p: &Polygon, lon: f64, lat: f64) -> bool {
        p.contains_degrees(lat, lon)
    }

    fn unit_square() -> Vec<(f64, f64)> {
        ring(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)])
    }

    /// The vertex osmium includes and an f64 ray cast excluded, and the
    /// edge rule on each side of a counterclockwise square: west, east and
    /// south edges are inside, the north edge between its vertices is not.
    #[test]
    fn vertices_and_edges_follow_osmium() {
        let square = one(unit_square(), vec![]);
        for (lon, lat) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
            assert!(at(&square, lon, lat), "vertex ({lon},{lat})");
        }
        assert!(at(&square, 0.0, 0.5), "west edge");
        assert!(at(&square, 1.0, 0.5), "east edge");
        assert!(at(&square, 0.5, 0.0), "south edge");
        assert!(!at(&square, 0.5, 1.0), "north edge");
        assert!(at(&square, 0.5, 0.5));
        assert!(!at(&square, 1.0000001, 0.5));
        assert!(!at(&square, 0.5, -0.0000001));
    }

    /// Input orientation does not matter: a clockwise ring is reversed.
    #[test]
    fn clockwise_input_is_reversed() {
        let mut cw = unit_square();
        cw.reverse();
        let square = one(cw, vec![]);
        assert!(at(&square, 1.0, 0.5));
        assert!(at(&square, 0.5, 0.0));
        assert!(!at(&square, 0.5, 1.0));
    }

    /// A hole is a second crossing: its interior is outside, its vertices
    /// inside, and an edge point is inside unless the edge is horizontal
    /// with the region only below it, which for a hole is its south edge.
    #[test]
    fn hole_vertices_are_inside_and_its_interior_is_not() {
        let hole = ring(&[(3.0, 3.0), (7.0, 3.0), (7.0, 7.0), (3.0, 7.0), (3.0, 3.0)]);
        let outer = ring(&[
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
            (0.0, 0.0),
        ]);
        let p = one(outer, vec![hole]);
        assert!(at(&p, 1.0, 1.0));
        assert!(!at(&p, 5.0, 5.0));
        assert!(at(&p, 3.0, 3.0));
        assert!(at(&p, 7.0, 7.0));
        assert!(at(&p, 3.0, 5.0), "west edge of the hole");
        assert!(at(&p, 7.0, 5.0), "east edge of the hole");
        assert!(at(&p, 5.0, 7.0), "north edge of the hole");
        assert!(!at(&p, 5.0, 3.0), "south edge of the hole");
        assert!(!at(&p, 15.0, 5.0));
        assert!(!at(&p, 1e300, 5.0), "out of the domain");
    }

    /// Members of one MultiPolygon share a parity, so an overlap cancels as
    /// in osmium; separate geometries (FeatureCollection features) union.
    #[test]
    fn multipolygon_overlap_cancels_and_features_union() {
        let a = || ring(&[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0), (0.0, 0.0)]);
        let b = || ring(&[(2.0, 2.0), (6.0, 2.0), (6.0, 6.0), (2.0, 6.0), (2.0, 2.0)]);
        let rings = |r: Vec<(f64, f64)>| PolygonRings {
            exterior: r,
            holes: vec![],
        };
        let multi = Polygon::new(&[vec![rings(a()), rings(b())]]).unwrap();
        assert!(at(&multi, 1.0, 1.0));
        assert!(at(&multi, 5.0, 5.0));
        assert!(!at(&multi, 3.0, 3.0), "overlap cancels within one geometry");
        let features = Polygon::new(&[vec![rings(a())], vec![rings(b())]]).unwrap();
        assert!(at(&features, 3.0, 3.0), "features union");
        let b = features.bbox();
        assert_eq!(
            (b.min_lon, b.min_lat, b.max_lon, b.max_lat),
            (0.0, 0.0, 6.0, 6.0)
        );
    }

    /// Vertices round as libosmium's `round(v * 1e7)`: `1.00000115` times
    /// 1e7 is 10000011.499999998 in f64, so the vertex is column 10000011
    /// (the typed decimal would round to 10000012), and a node there is on
    /// the vertex.
    #[test]
    fn vertices_quantize_as_libosmium() {
        let p = one(
            ring(&[(1.00000115, 1.0), (2.0, 1.0), (2.0, 2.0), (1.00000115, 2.0)]),
            vec![],
        );
        assert!(p.contains_decimicro(10_000_000, 10_000_011));
        assert!(!p.contains_decimicro(10_000_000, 10_000_010));
        assert!(!p.contains_decimicro(30_000_000, 10_000_011));
        assert_eq!(p.bbox().min_lon, 1.0000011);
    }

    /// An open ring gets its closing segment.
    #[test]
    fn open_ring_is_closed() {
        let p = one(
            ring(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)]),
            vec![],
        );
        assert!(at(&p, 0.5, 0.5));
        assert!(at(&p, 0.0, 0.5), "west edge is the closing segment");
    }

    /// The banded lookup gives the answer a scan of every segment gives, on a
    /// ring with enough segments for many bands, at random points and at
    /// every vertex.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    #[test]
    fn bands_agree_with_a_full_scan() {
        let mut state = 0x1234_5678_9ABC_DEF0_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        // A star with 400 spikes of random radius: concave, many bands.
        let mut star = Vec::new();
        for i in 0..400 {
            let t = f64::from(i) / 400.0 * std::f64::consts::TAU;
            let r = 1.0 + (next() % 1000) as f64 / 1000.0;
            star.push((10.0 + r * t.cos(), 50.0 + r * t.sin() * 0.5));
        }
        star.push(star[0]);
        let p = one(star.clone(), vec![]);
        let Geometry::Planar(rings) = &p.geometries[0] else {
            panic!("planar");
        };
        let all: Vec<Segment> = {
            let mut segs: Vec<Segment> = Vec::new();
            for s in &rings.banded {
                if !segs
                    .iter()
                    .any(|t| (t.x1, t.y1, t.x2, t.y2) == (s.x1, s.y1, s.x2, s.y2))
                {
                    segs.push(*s);
                }
            }
            segs
        };
        assert_eq!(all.len(), 400);
        let scan = |px: i64, py: i64| {
            if px < rings.min_x || px > rings.max_x || py < rings.min_y || py > rings.max_y {
                return false;
            }
            let mut inside = false;
            for s in &all {
                match s.step(px, py) {
                    None => return true,
                    Some(true) => inside = !inside,
                    Some(false) => {}
                }
            }
            inside
        };
        let mut points: Vec<(i64, i64)> =
            star.iter().map(|&(x, y)| (to_fix(x), to_fix(y))).collect();
        for _ in 0..20_000 {
            let x = 70_000_000 + (next() % 60_000_000) as i64;
            let y = 485_000_000 + (next() % 30_000_000) as i64;
            points.push((x, y));
        }
        let mut inside = 0;
        for (x, y) in points {
            let banded = rings.contains(x, y);
            assert_eq!(banded, scan(x, y), "({x},{y})");
            inside += usize::from(banded);
        }
        assert!(inside > 1000, "the fixture must exercise the interior");
    }

    #[test]
    fn antimeridian_rectangle() {
        let p = one(
            ring(&[
                (179.0, 10.0),
                (-179.0, 10.0),
                (-179.0, 12.0),
                (179.0, 12.0),
                (179.0, 10.0),
            ]),
            vec![],
        );
        assert!(at(&p, 179.5, 11.0));
        assert!(at(&p, -179.5, 11.0));
        assert!(at(&p, 179.0, 11.0), "a vertex column on the east lobe");
        assert!(at(&p, -179.0, 10.0), "a vertex on the west lobe");
        assert!(!at(&p, 0.0, 11.0));
        assert!(!at(&p, 178.9, 11.0));
        assert!(
            !at(&p, 1e300, 11.0),
            "a huge longitude is outside, not an overflow"
        );
        assert!(
            !at(&p, 180.5, 11.0),
            "out of the domain, though one turn west is inside"
        );
        let b = p.bbox();
        assert_eq!((b.min_lon, b.max_lon), (-180.0, 180.0));
        assert_eq!((b.min_lat, b.max_lat), (10.0, 12.0));
    }

    /// A hole is placed in its own outer ring's longitude window, whatever
    /// the order of the members: a remote crossing member does not move it.
    #[test]
    fn periodic_hole_stays_with_its_outer() {
        let rings = |exterior, holes| PolygonRings { exterior, holes };
        let a = ring(&[
            (-179.0, -10.0),
            (-177.0, -10.0),
            (-177.0, -8.0),
            (-179.0, -8.0),
            (-179.0, -10.0),
        ]);
        let b = ring(&[
            (-100.0, 0.0),
            (35.0, 0.0),
            (170.0, 0.0),
            (170.0, 20.0),
            (35.0, 20.0),
            (-100.0, 20.0),
            (-100.0, 0.0),
        ]);
        let hole = ring(&[
            (-90.0, 5.0),
            (-90.0, 15.0),
            (-80.0, 15.0),
            (-80.0, 5.0),
            (-90.0, 5.0),
        ]);
        let c = ring(&[
            (179.0, -20.0),
            (-179.0, -20.0),
            (-179.0, -18.0),
            (179.0, -18.0),
            (179.0, -20.0),
        ]);
        let p = Polygon::new(&[vec![
            rings(a, vec![]),
            rings(b, vec![hole]),
            rings(c, vec![]),
        ]])
        .unwrap();
        assert!(!at(&p, -85.0, 10.0), "inside the hole");
        assert!(at(&p, -95.0, 10.0));
        assert!(at(&p, 100.0, 10.0));
        assert!(at(&p, -178.0, -9.0));
        assert!(at(&p, 179.5, -19.0));
        assert!(at(&p, -179.5, -19.0));
    }

    /// A ring that circles a pole has no planar lift and is refused.
    #[test]
    fn a_ring_winding_around_a_pole_is_refused() {
        let err = Polygon::new(&[vec![PolygonRings {
            exterior: ring(&[(-120.0, 80.0), (0.0, 85.0), (120.0, 81.0), (-120.0, 80.0)]),
            holes: vec![],
        }]])
        .err()
        .expect("winding ring must be refused");
        assert!(err.to_string().contains("winds around a pole"), "{err}");
    }
}
