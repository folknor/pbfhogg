use crate::commands::extract::Bbox;
use crate::coord_fmt::format_coord;
use crate::{Way, WayNodeLocation};

pub(super) const AREA_KEYS: &[&str] = &[
    "building", "landuse", "natural", "leisure", "amenity", "boundary", "waterway",
];

/// Emitted latitude bound, in 10^-7 degrees, inclusive.
const MAX_LAT_E7: i128 = 900_000_000;
/// Emitted longitude bound, in 10^-7 degrees, inclusive.
const MAX_LON_E7: i128 = 1_800_000_000;

/// One ordinate as the element reports it: the exact nanodegree value and
/// the `f64` degrees of its `lon()` / `lat()` accessor.
#[derive(Clone, Copy)]
pub(super) struct Ordinate {
    pub(super) nano: i128,
    pub(super) deg: f64,
}

/// The exact count of 10^-7 degrees `format_coord` text spells. `None` for
/// text that is not a plain decimal (`NaN`, `inf`) or does not fit.
fn parse_e7(text: &str) -> Option<i128> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (int_part, frac_part) = digits.split_once('.').unwrap_or((digits, ""));
    if int_part.is_empty() || frac_part.len() > 7 {
        return None;
    }
    let mut value: i128 = 0;
    let padded = frac_part.bytes().chain(std::iter::repeat(b'0')).take(7);
    for byte in int_part.bytes().chain(padded) {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add(i128::from(byte - b'0'))?;
    }
    Some(if negative { -value } else { value })
}

/// Appends `e7` spelled as `format_coord` spells a coordinate of exactly
/// that many 10^-7 degrees: the sign, the whole degrees, then the fraction
/// with its trailing zeros, and a bare point, trimmed.
fn push_e7(out: &mut Vec<u8>, e7: i128) {
    use std::io::Write;

    if e7 < 0 {
        out.push(b'-');
    }
    let magnitude = e7.unsigned_abs();
    let whole = magnitude / 10_000_000;
    let mut frac = magnitude % 10_000_000;
    write!(out, "{whole}").ok();
    if frac != 0 {
        let mut width = 7;
        while frac % 10 == 0 {
            frac /= 10;
            width -= 1;
        }
        write!(out, ".{frac:0width$}").ok();
    }
}

/// Appends the text export writes for one ordinate and returns its exact
/// emitted value in 10^-7 degrees, or `None` when that text is not a plain
/// decimal or lies past `bound`.
///
/// The text is `format_coord(deg)`: seven decimals correctly rounded from
/// the `f64`, trailing zeros trimmed. When the exact value is a whole number
/// `k` of 10^-7 degrees (every coordinate of a default-granularity file)
/// and `|k|` is within the bound, that rounding is `k` itself: `deg` is
/// `1e-9 * nano` evaluated in `f64`, within a few ulps (under 3e-14 at 180
/// degrees) of `k * 1e-7`, far from the 5e-8 a rounding would have to
/// cross, so the text is written from `k` with no float formatting. A
/// whole `k` past the bound is outside the domain whichever text it would
/// print: the nearest such value is a full 10^-7 degree past the edge and
/// the conversion is monotone. Any other value (a non-default granularity
/// or offset) can sit on or near a half-unit, where only the `f64` decides,
/// and goes through `format_coord`, which also keeps a small negative
/// value's `-0`.
fn push_ordinate(
    out: &mut Vec<u8>,
    scratch: &mut String,
    ord: Ordinate,
    bound: i128,
) -> Option<i128> {
    if ord.nano % 100 == 0 {
        let e7 = ord.nano / 100;
        if e7.abs() > bound {
            return None;
        }
        push_e7(out, e7);
        return Some(e7);
    }
    format_coord(scratch, ord.deg);
    let e7 = parse_e7(scratch).filter(|v| v.abs() <= bound)?;
    out.extend_from_slice(scratch.as_bytes());
    Some(e7)
}

/// A feature's positions, each formatted once: the text of every position
/// (`lon,lat`), its exact emitted value, which the domain, area and
/// orientation checks read, and the first and last input pair, which decide
/// whether a ring is closed. Emission copies the text.
#[derive(Default)]
pub(super) struct Positions {
    text: Vec<u8>,
    /// Byte end of each position's text; a position starts where the
    /// previous one ends.
    ends: Vec<usize>,
    e7: Vec<(i128, i128)>,
    first: Option<(f64, f64)>,
    last: Option<(f64, f64)>,
    scratch: String,
    before_bbox_hit: Vec<WayNodeLocation>,
}

impl Positions {
    fn clear(&mut self) {
        self.text.clear();
        self.ends.clear();
        self.e7.clear();
        self.first = None;
        self.last = None;
    }

    /// Adds one position, or returns `false` when its emitted text is not a
    /// plain decimal or lies outside -180..=180 longitude or -90..=90
    /// latitude. RFC 7946 positions are WGS 84 degrees, so the domain is
    /// judged on what is printed: a value just past a pole that rounds to
    /// `90` is valid output, and libosmium's undefined location
    /// (`2147483647` on both axes, printed as `214.7483647`) is not. The
    /// readers do not check the geographic domain, so export enforces it at
    /// its own output.
    fn push(&mut self, lon: Ordinate, lat: Ordinate) -> bool {
        let Some(lon_e7) = push_ordinate(&mut self.text, &mut self.scratch, lon, MAX_LON_E7) else {
            return false;
        };
        self.text.push(b',');
        let Some(lat_e7) = push_ordinate(&mut self.text, &mut self.scratch, lat, MAX_LAT_E7) else {
            return false;
        };
        self.ends.push(self.text.len());
        self.e7.push((lon_e7, lat_e7));
        if self.first.is_none() {
            self.first = Some((lon.deg, lat.deg));
        }
        self.last = Some((lon.deg, lat.deg));
        true
    }

    /// Formats one point; `false` when it is outside the domain.
    pub(super) fn fill_point(&mut self, lon: Ordinate, lat: Ordinate) -> bool {
        self.clear();
        self.push(lon, lat)
    }

    fn push_location(&mut self, location: WayNodeLocation, lat: f64, lon: f64) -> bool {
        self.push(
            Ordinate {
                nano: location.nano_lon_exact(),
                deg: lon,
            },
            Ordinate {
                nano: location.nano_lat_exact(),
                deg: lat,
            },
        )
    }

    fn push_bbox_prefix(&mut self) -> bool {
        let valid = (0..self.before_bbox_hit.len()).all(|index| {
            let location = self.before_bbox_hit[index];
            self.push_location(location, location.lat(), location.lon())
        });
        self.before_bbox_hit.clear();
        valid
    }

    /// Collects coordinate text and reference endpoints in one source walk.
    /// Save decoded positions until the first bbox hit so excluded ways
    /// never need coordinate formatting.
    pub(super) fn fill_way(&mut self, way: &Way<'_>, bbox: Option<&Bbox>) -> WayScan {
        self.clear();
        self.before_bbox_hit.clear();
        let mut hits_bbox = bbox.is_none();
        let mut first_ref = None;
        let mut last_ref = None;
        // Block validation guarantees that a way with locations has one
        // per ref. Without locations there is no geometry to classify.
        for (location, reference) in way.node_locations().zip(way.refs()) {
            first_ref.get_or_insert(reference);
            last_ref = Some(reference);
            let lat = location.lat();
            let lon = location.lon();
            if !hits_bbox {
                // Use the same quantized point test as nodes, on the input
                // degrees before any GeoJSON coordinate formatting.
                if !bbox.is_some_and(|bbox| bbox.contains(lat, lon)) {
                    self.before_bbox_hit.push(location);
                    continue;
                }
                hits_bbox = true;
                if !self.push_bbox_prefix() {
                    return WayScan::invalid();
                }
            }
            if !self.push_location(location, lat, lon) {
                return WayScan::invalid();
            }
        }
        WayScan {
            hits_bbox,
            valid: true,
            closed_refs: self.len() >= 4 && first_ref == last_ref,
        }
    }

    #[cfg(test)]
    fn fill(&mut self, coords: &[(f64, f64)]) -> bool {
        self.clear();
        coords
            .iter()
            .all(|&(lon, lat)| self.push(test_ordinate(lon), test_ordinate(lat)))
    }

    fn len(&self) -> usize {
        self.ends.len()
    }

    /// The bytes a geometry of these positions takes at most: every text,
    /// its brackets and separators, the closing position and the frame.
    pub(super) fn bound(&self) -> usize {
        self.text.len() + self.ends.len() * 3 + self.ends.first().copied().unwrap_or(0) + 64
    }

    fn push_position(&self, out: &mut Vec<u8>, i: usize) {
        let start = if i == 0 { 0 } else { self.ends[i - 1] };
        out.push(b'[');
        out.extend_from_slice(&self.text[start..self.ends[i]]);
        out.push(b']');
    }

    /// Twice the signed area of the ring export emits, in squared 10^-7
    /// degrees, computed exactly. The orientation is judged on the emitted
    /// values, not on the input: an `f64` shoelace over absolute coordinates
    /// loses the sign to cancellation (a small clockwise ring near
    /// `(128, 64)` sums products near 8192 to recover a twice-area near
    /// `1e-14`), and exact input nanodegrees can disagree in sign with the
    /// rounded output when block offsets are not multiples of 100. Vertices
    /// are taken relative to the first, so the closing edge of an open ring
    /// contributes zero and needs no term. `None` for no positions, or when
    /// the checked `i128` arithmetic overflows, which no in-domain ring
    /// reaches.
    fn twice_area(&self) -> Option<i128> {
        let mut points = self.e7.iter();
        let &origin = points.next()?;
        let mut prev = (0_i128, 0_i128);
        let mut twice_area: i128 = 0;
        for &(lon, lat) in points {
            let cur = (lon.checked_sub(origin.0)?, lat.checked_sub(origin.1)?);
            let det = prev
                .0
                .checked_mul(cur.1)?
                .checked_sub(cur.0.checked_mul(prev.1)?)?;
            twice_area = twice_area.checked_add(det)?;
            prev = cur;
        }
        Some(twice_area)
    }
}

pub(super) struct WayScan {
    pub(super) hits_bbox: bool,
    pub(super) valid: bool,
    pub(super) closed_refs: bool,
}

impl WayScan {
    fn invalid() -> Self {
        Self {
            hits_bbox: true,
            valid: false,
            closed_refs: false,
        }
    }
}

/// How a valid way is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WayShape {
    area: bool,
    reverse: bool,
    closed: bool,
}

/// Judges a way's formatted positions: `None` when its geometry is invalid.
/// Every position already lies in the geographic domain
/// ([`Positions::fill_way`]). A LineString needs two positions. An area ring
/// is judged by one rule, on the coordinates it emits: a non-zero exact
/// emitted twice-area. That rejects rings that are collinear or that
/// rounding to seven decimals collapses, which a check on the unrounded
/// input passes. It also implies every older guard: a non-zero determinant
/// names two emitted positions distinct from the first and not collinear
/// with it, so the ring has three distinct positions and, closed or with
/// its closing position appended, at least four. A self-intersecting ring
/// whose lobes cancel to exactly zero net area is rejected too;
/// self-intersection is not otherwise checked. Whether the ring is closed
/// is decided on the input pairs, so endpoints that print alike but differ
/// still get the closing position appended.
pub(super) fn way_shape(positions: &Positions, is_area: bool) -> Option<WayShape> {
    let closed = positions.first == positions.last;
    if !is_area {
        return (positions.len() >= 2).then_some(WayShape {
            area: false,
            reverse: false,
            closed,
        });
    }
    match positions.twice_area() {
        None | Some(0) => None,
        Some(twice_area) => Some(WayShape {
            area: true,
            reverse: twice_area < 0,
            closed,
        }),
    }
}

/// Writes a Point of the one formatted position.
pub(super) fn write_point(out: &mut Vec<u8>, positions: &Positions) {
    out.extend_from_slice(b"{\"type\":\"Point\",\"coordinates\":");
    positions.push_position(out, 0);
    out.push(b'}');
}

/// Writes a way's LineString, or its Polygon closed and counterclockwise.
pub(super) fn write_way(out: &mut Vec<u8>, positions: &Positions, shape: WayShape) {
    if shape.area {
        out.extend_from_slice(b"{\"type\":\"Polygon\",\"coordinates\":[[");
    } else {
        out.extend_from_slice(b"{\"type\":\"LineString\",\"coordinates\":[");
    }
    let n = positions.len();
    let mut push = |i: usize, first: bool| {
        if !first {
            out.push(b',');
        }
        positions.push_position(out, i);
    };
    if shape.reverse && !shape.closed {
        push(0, true);
        for i in (1..n).rev() {
            push(i, false);
        }
    } else if shape.reverse {
        for i in (0..n).rev() {
            push(i, i == n - 1);
        }
    } else {
        for i in 0..n {
            push(i, i == 0);
        }
    }
    if shape.area && !shape.closed {
        push(0, false);
    }
    if shape.area {
        out.extend_from_slice(b"]]}");
    } else {
        out.extend_from_slice(b"]}");
    }
}

pub(super) fn is_area_tags<'a, T>(tags: T) -> bool
where
    T: Iterator<Item = (&'a str, &'a str)>,
{
    let mut area_yes = false;
    let mut area_no = false;
    let mut area_key = false;
    for (key, value) in tags {
        if key == "area" {
            area_yes |= value == "yes";
            area_no |= value == "no";
        }
        area_key |= AREA_KEYS.contains(&key);
    }
    !area_no && (area_yes || area_key)
}

/// An ordinate as a test states it in degrees, with the nanodegree value
/// the accessors would hold for it: exact where the degrees are a whole
/// nanodegree count, otherwise one that is not a whole 10^-7 degree, so the
/// `format_coord` path decides.
#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
fn test_ordinate(deg: f64) -> Ordinate {
    let nano = (deg * 1e9).round();
    if deg.is_finite() && nano.abs() < 1e15 && (1e-9 * nano - deg).abs() <= f64::EPSILON * deg.abs()
    {
        Ordinate {
            nano: nano as i128,
            deg,
        }
    } else {
        Ordinate { nano: 1, deg }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(lon: f64, lat: f64) -> Option<String> {
        let mut positions = Positions::default();
        if !positions.fill_point(test_ordinate(lon), test_ordinate(lat)) {
            return None;
        }
        let mut out = Vec::new();
        write_point(&mut out, &positions);
        Some(String::from_utf8(out).expect("geometry is ASCII"))
    }

    fn way(coords: &[(f64, f64)], is_area: bool) -> Option<String> {
        let mut positions = Positions::default();
        if !positions.fill(coords) {
            return None;
        }
        let shape = way_shape(&positions, is_area)?;
        let mut out = Vec::new();
        write_way(&mut out, &positions, shape);
        assert!(out.len() <= positions.bound());
        Some(String::from_utf8(out).expect("geometry is ASCII"))
    }

    #[test]
    fn point_and_line_format() {
        assert_eq!(
            point(12.5, 55.123_456_7).as_deref(),
            Some("{\"type\":\"Point\",\"coordinates\":[12.5,55.1234567]}")
        );
        assert_eq!(
            way(&[(1.0, 2.0), (3.0, 4.0)], false).as_deref(),
            Some("{\"type\":\"LineString\",\"coordinates\":[[1,2],[3,4]]}")
        );
    }

    #[test]
    fn polygon_closes_and_reorients_clockwise_ring() {
        assert_eq!(
            way(&[(0.0, 0.0), (0.0, 1.0), (1.0, 0.0)], true).as_deref(),
            Some("{\"type\":\"Polygon\",\"coordinates\":[[[0,0],[1,0],[0,1],[0,0]]]}")
        );
        assert_eq!(way(&[(0.0, 0.0)], false), None);
        assert_eq!(way(&[(0.0, 0.0)], true), None);
        assert_eq!(way(&[(0.0, 0.0), (0.0, 0.0), (1.0, 1.0)], true), None);
    }

    #[test]
    fn clockwise_ring_far_from_origin_is_reoriented() {
        let ring = [
            (128.0, 64.0),
            (128.0, 64.000_000_1),
            (128.000_000_1, 64.0),
            (128.0, 64.0),
        ];
        assert_eq!(
            way(&ring, true).as_deref(),
            Some(
                "{\"type\":\"Polygon\",\"coordinates\":[[[128,64],[128.0000001,64],\
                 [128,64.0000001],[128,64]]]}"
            )
        );
    }

    #[test]
    fn orientation_follows_emitted_coordinates() {
        // Exact nanodegrees (offset 50) wind counterclockwise, but the
        // seven-decimal output winds clockwise; the output is what must be
        // counterclockwise.
        let ring = [
            (-5050e-9, 8350e-9),
            (-1950e-9, 11850e-9),
            (1150e-9, 15450e-9),
            (-5050e-9, 8350e-9),
        ];
        assert_eq!(
            way(&ring, true).as_deref(),
            Some(
                "{\"type\":\"Polygon\",\"coordinates\":[[[-0.000005,0.0000083],\
                 [0.0000012,0.0000154],[-0.0000019,0.0000119],[-0.000005,0.0000083]]]}"
            )
        );
    }

    #[test]
    fn endpoints_that_print_alike_still_close() {
        // The last input position differs from the first below print
        // precision, so the ring is open and the first position is appended.
        let ring = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1e-9, 0.0)];
        assert_eq!(
            way(&ring, true).as_deref(),
            Some("{\"type\":\"Polygon\",\"coordinates\":[[[0,0],[1,0],[0,1],[0,0],[0,0]]]}")
        );
    }

    #[test]
    fn zero_emitted_area_is_invalid() {
        let collinear = [(0.0, 0.0), (1.0, 1.0), (2.0, 2.0), (0.0, 0.0)];
        assert_eq!(way(&collinear, true), None);
        // Three distinct input positions that all round to [0,0].
        let collapsed = [(0.0, 0.0), (0.0, 1e-9), (1e-9, 0.0), (0.0, 0.0)];
        assert_eq!(way(&collapsed, true), None);
        // Distinct input that rounds to two distinct positions.
        let flattened = [(0.0, 0.0), (1e-9, 1.0), (0.0, 1.0), (0.0, 0.0)];
        assert_eq!(way(&flattened, true), None);
        // Lobes of a bowtie cancel to zero net area.
        let bowtie = [(0.0, 0.0), (1.0, 1.0), (1.0, 0.0), (0.0, 1.0), (0.0, 0.0)];
        assert_eq!(way(&bowtie, true), None);
        // An open ring needs no fourth input position.
        assert!(way(&[(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)], true).is_some());
    }

    #[test]
    fn unformattable_area_coordinate_is_invalid() {
        assert_eq!(
            way(&[(0.0, 0.0), (f64::NAN, 1.0), (1.0, 0.0), (0.0, 0.0)], true),
            None
        );
    }

    #[test]
    fn positions_outside_the_geographic_domain_are_invalid() {
        assert_eq!(point(0.0, 100.0), None);
        assert_eq!(point(-180.000_000_1, 0.0), None);
        // Edges are inclusive, and a value that rounds onto a pole is valid.
        assert_eq!(
            point(-180.0, 90.000_000_04).as_deref(),
            Some("{\"type\":\"Point\",\"coordinates\":[-180,90]}")
        );
        // libosmium's undefined location.
        let undefined = 214.748_364_7;
        assert_eq!(way(&[(1.0, 2.0), (undefined, undefined)], false), None);
        assert_eq!(
            way(
                &[(0.0, 0.0), (1.0, 0.0), (1.0, 90.000_000_1), (0.0, 0.0)],
                true
            ),
            None
        );
        assert_eq!(way(&[(0.0, 0.0), (0.0, 1e300)], false), None);
    }

    #[test]
    fn parse_e7_reads_formatted_text() {
        let mut scratch = String::new();
        for (deg, want) in [
            (-0.000_000_01, Some(0)),
            (-0.000_000_1, Some(-1)),
            (12.5, Some(125_000_000)),
            (f64::INFINITY, None),
        ] {
            format_coord(&mut scratch, deg);
            assert_eq!(parse_e7(&scratch), want, "{deg}");
        }
    }

    /// The whole-decimicrodegree path writes exactly the text and value the
    /// `format_coord` path does, for the `f64` the accessors compute
    /// (`1e-9 * nano`), across every magnitude in the domain.
    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn integer_text_matches_float_formatting() {
        let mut scratch = String::new();
        let mut check = |e7: i128| {
            let nano = e7 * 100;
            let deg = 1e-9 * nano as f64;
            let mut fast = Vec::new();
            let fast_e7 =
                push_ordinate(&mut fast, &mut scratch, Ordinate { nano, deg }, MAX_LON_E7);
            format_coord(&mut scratch, deg);
            let slow_e7 = parse_e7(&scratch).filter(|v| v.abs() <= MAX_LON_E7);
            assert_eq!(fast_e7, slow_e7, "{e7}");
            if fast_e7.is_some() {
                assert_eq!(fast, scratch.as_bytes(), "{e7}");
            }
        };
        for e7 in -20_000..=20_000 {
            check(e7);
        }
        for edge in [MAX_LAT_E7, MAX_LON_E7] {
            for e7 in edge - 20_000..=edge + 20_000 {
                check(e7);
                check(-e7);
            }
        }
        // A fixed linear congruential walk over the whole domain.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..400_000 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let e7 = i128::from(state >> 33) % (2 * MAX_LON_E7 + 1) - MAX_LON_E7;
            check(e7);
        }
    }

    #[test]
    fn area_heuristic_truth_table() {
        assert!(is_area_tags([("building", "yes")].into_iter()));
        assert!(!is_area_tags(
            [("building", "yes"), ("area", "no")].into_iter()
        ));
        assert!(is_area_tags([("area", "yes")].into_iter()));
        assert!(!is_area_tags([("highway", "service")].into_iter()));
    }
}
