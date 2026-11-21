use crate::Way;
use crate::coord_fmt::format_coord;

pub(super) const AREA_KEYS: &[&str] = &[
    "building", "landuse", "natural", "leisure", "amenity", "boundary", "waterway",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Geom {
    Written,
    Invalid,
}

fn push_position(buf: &mut String, scratch: &mut String, lon: f64, lat: f64) {
    buf.push('[');
    format_coord(scratch, lon);
    buf.push_str(scratch);
    buf.push(',');
    format_coord(scratch, lat);
    buf.push_str(scratch);
    buf.push(']');
}

/// Writes a Point, or returns `Invalid` when its emitted position is outside
/// the geographic domain (see [`emitted_position`]).
pub(super) fn write_point(buf: &mut String, scratch: &mut String, lon: f64, lat: f64) -> Geom {
    if emitted_position(scratch, lon, lat).is_none() {
        return Geom::Invalid;
    }
    buf.clear();
    buf.push_str("{\"type\":\"Point\",\"coordinates\":");
    push_position(buf, scratch, lon, lat);
    buf.push('}');
    Geom::Written
}

pub(super) fn collect_coords(way: &Way<'_>, coords: &mut Vec<(f64, f64)>) {
    coords.clear();
    coords.extend(
        way.node_locations()
            .map(|location| (location.lon(), location.lat())),
    );
}

/// The coordinate `format_coord` writes for `deg`, as an exact count of
/// 10^-7 degrees, parsed back from that same text. `None` for text that is
/// not a plain decimal (`NaN`, `inf`) or does not fit.
fn emitted_e7(scratch: &mut String, deg: f64) -> Option<i128> {
    format_coord(scratch, deg);
    let (negative, digits) = match scratch.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, scratch.as_str()),
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

/// Emitted latitude bound, in 10^-7 degrees, inclusive.
const MAX_LAT_E7: i128 = 900_000_000;
/// Emitted longitude bound, in 10^-7 degrees, inclusive.
const MAX_LON_E7: i128 = 1_800_000_000;

/// The position export writes for `(lon, lat)`, as exact 10^-7 degree
/// counts parsed from the very text `format_coord` produces, or `None` when
/// that text is not a plain decimal or lies outside -180..=180 longitude or
/// -90..=90 latitude. RFC 7946 positions are WGS 84 degrees, so the domain
/// is judged on what is printed: a value just past a pole that rounds to
/// `90` is valid output, and libosmium's undefined location (`2147483647`
/// on both axes, printed as `214.7483647`) is not. The readers do not check
/// the geographic domain, so export enforces it at its own output. Points
/// and LineStrings format each ordinate once more than writing does; export
/// has no bench, and that was judged not worth a format-once restructure.
fn emitted_position(scratch: &mut String, lon: f64, lat: f64) -> Option<(i128, i128)> {
    let lon = emitted_e7(scratch, lon).filter(|v| v.abs() <= MAX_LON_E7)?;
    let lat = emitted_e7(scratch, lat).filter(|v| v.abs() <= MAX_LAT_E7)?;
    Some((lon, lat))
}

/// Twice the signed area of the ring export emits, in squared 10^-7 degrees,
/// computed exactly. The orientation is judged on the emitted text, not on
/// the input: an `f64` shoelace over absolute coordinates loses the sign to
/// cancellation (a small clockwise ring near `(128, 64)` sums products near
/// 8192 to recover a twice-area near `1e-14`), and exact input nanodegrees
/// can disagree in sign with the rounded output when block offsets are not
/// multiples of 100. Vertices are taken relative to the first, so the
/// closing edge of an open ring contributes zero and needs no term. `None`
/// when a coordinate does not format to a parseable decimal or the checked
/// `i128` arithmetic overflows, which no geographically valid ring reaches,
/// or when any emitted position is outside the geographic domain.
fn emitted_twice_area(scratch: &mut String, coords: &[(f64, f64)]) -> Option<i128> {
    let mut points = coords.iter();
    let &(lon0, lat0) = points.next()?;
    let origin = emitted_position(scratch, lon0, lat0)?;
    let mut prev = (0_i128, 0_i128);
    let mut twice_area: i128 = 0;
    for &(lon, lat) in points {
        let (lon, lat) = emitted_position(scratch, lon, lat)?;
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

fn push_positions<'a, I>(
    buf: &mut String,
    scratch: &mut String,
    positions: I,
    wrote_position: &mut bool,
) where
    I: Iterator<Item = &'a (f64, f64)>,
{
    for &(lon, lat) in positions {
        if *wrote_position {
            buf.push(',');
        }
        push_position(buf, scratch, lon, lat);
        *wrote_position = true;
    }
}

pub(super) fn write_way_geometry(
    buf: &mut String,
    scratch: &mut String,
    coords: &[(f64, f64)],
    is_area: bool,
) -> Geom {
    if !is_area && coords.len() < 2 {
        return Geom::Invalid;
    }
    // Every emitted position must lie in the geographic domain; an area
    // ring is held to it by `emitted_twice_area` below.
    if !is_area
        && coords
            .iter()
            .any(|&(lon, lat)| emitted_position(scratch, lon, lat).is_none())
    {
        return Geom::Invalid;
    }

    let closed = coords.first() == coords.last();
    // An area ring is judged by one rule, on the coordinates it emits: a
    // non-zero exact emitted twice-area. That rejects rings that are
    // collinear or that rounding to seven decimals collapses, which a
    // check on the unrounded input passes. It also implies every older
    // guard: a non-zero determinant names two emitted positions distinct
    // from the first and not collinear with it, so the ring has three
    // distinct positions and, closed or with its closing position
    // appended, at least four. A self-intersecting ring whose lobes cancel
    // to exactly zero net area is rejected too; self-intersection is not
    // otherwise checked.
    let reverse = if is_area {
        match emitted_twice_area(scratch, coords) {
            None | Some(0) => return Geom::Invalid,
            Some(twice_area) => twice_area < 0,
        }
    } else {
        false
    };

    buf.clear();
    if is_area {
        buf.push_str("{\"type\":\"Polygon\",\"coordinates\":[[");
    } else {
        buf.push_str("{\"type\":\"LineString\",\"coordinates\":[");
    }
    let mut wrote_position = false;
    if reverse && !closed {
        push_positions(buf, scratch, coords[..1].iter(), &mut wrote_position);
        push_positions(buf, scratch, coords[1..].iter().rev(), &mut wrote_position);
    } else if reverse {
        push_positions(buf, scratch, coords.iter().rev(), &mut wrote_position);
    } else {
        push_positions(buf, scratch, coords.iter(), &mut wrote_position);
    }
    if is_area && !closed {
        buf.push(',');
        let &(lon, lat) = &coords[0];
        push_position(buf, scratch, lon, lat);
    }
    if is_area {
        buf.push_str("]]}");
    } else {
        buf.push_str("]}");
    }
    Geom::Written
}

fn is_area_tags<'a, T>(tags: T) -> bool
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

#[cfg(test)]
fn is_area_parts<'a, T>(refs: &[i64], tags: T) -> bool
where
    T: Iterator<Item = (&'a str, &'a str)>,
{
    refs.len() >= 4 && refs.first() == refs.last() && is_area_tags(tags)
}

pub(super) fn is_area_way(way: &Way<'_>) -> bool {
    let mut refs = way.refs();
    let Some(first) = refs.next() else {
        return false;
    };
    let mut count = 1_usize;
    let mut last = first;
    for reference in refs {
        count += 1;
        last = reference;
    }
    if count < 4 || first != last {
        return false;
    }
    is_area_tags(way.tags())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_and_line_format() {
        let mut buf = String::new();
        let mut scratch = String::new();
        assert_eq!(
            write_point(&mut buf, &mut scratch, 12.5, 55.123_456_7),
            Geom::Written
        );
        assert_eq!(
            buf,
            "{\"type\":\"Point\",\"coordinates\":[12.5,55.1234567]}"
        );
        assert_eq!(
            write_way_geometry(&mut buf, &mut scratch, &[(1.0, 2.0), (3.0, 4.0)], false),
            Geom::Written
        );
        assert_eq!(
            buf,
            "{\"type\":\"LineString\",\"coordinates\":[[1,2],[3,4]]}"
        );
    }

    #[test]
    fn polygon_closes_and_reorients_clockwise_ring() {
        let mut buf = String::new();
        let mut scratch = String::new();
        assert_eq!(
            write_way_geometry(
                &mut buf,
                &mut scratch,
                &[(0.0, 0.0), (0.0, 1.0), (1.0, 0.0)],
                true
            ),
            Geom::Written
        );
        assert_eq!(
            buf,
            "{\"type\":\"Polygon\",\"coordinates\":[[[0,0],[1,0],[0,1],[0,0]]]}"
        );
        assert_eq!(
            write_way_geometry(&mut buf, &mut scratch, &[(0.0, 0.0)], false),
            Geom::Invalid
        );
        assert_eq!(
            write_way_geometry(&mut buf, &mut scratch, &[(0.0, 0.0)], true),
            Geom::Invalid
        );
        assert_eq!(
            write_way_geometry(
                &mut buf,
                &mut scratch,
                &[(0.0, 0.0), (0.0, 0.0), (1.0, 1.0)],
                true
            ),
            Geom::Invalid
        );
    }

    #[test]
    fn clockwise_ring_far_from_origin_is_reoriented() {
        let mut buf = String::new();
        let mut scratch = String::new();
        let ring = [
            (128.0, 64.0),
            (128.0, 64.000_000_1),
            (128.000_000_1, 64.0),
            (128.0, 64.0),
        ];
        assert_eq!(
            write_way_geometry(&mut buf, &mut scratch, &ring, true),
            Geom::Written
        );
        assert_eq!(
            buf,
            "{\"type\":\"Polygon\",\"coordinates\":[[[128,64],[128.0000001,64],\
             [128,64.0000001],[128,64]]]}"
        );
    }

    #[test]
    fn orientation_follows_emitted_coordinates() {
        // Exact nanodegrees (offset 50) wind counterclockwise, but the
        // seven-decimal output winds clockwise; the output is what must be
        // counterclockwise.
        let mut buf = String::new();
        let mut scratch = String::new();
        let ring = [
            (-5050e-9, 8350e-9),
            (-1950e-9, 11850e-9),
            (1150e-9, 15450e-9),
            (-5050e-9, 8350e-9),
        ];
        assert_eq!(
            write_way_geometry(&mut buf, &mut scratch, &ring, true),
            Geom::Written
        );
        assert_eq!(
            buf,
            "{\"type\":\"Polygon\",\"coordinates\":[[[-0.000005,0.0000083],\
             [0.0000012,0.0000154],[-0.0000019,0.0000119],[-0.000005,0.0000083]]]}"
        );
    }

    #[test]
    fn zero_emitted_area_is_invalid() {
        let mut buf = String::new();
        let mut scratch = String::new();
        let collinear = [(0.0, 0.0), (1.0, 1.0), (2.0, 2.0), (0.0, 0.0)];
        assert_eq!(
            write_way_geometry(&mut buf, &mut scratch, &collinear, true),
            Geom::Invalid
        );
        // Three distinct input positions that all round to [0,0].
        let collapsed = [(0.0, 0.0), (0.0, 1e-9), (1e-9, 0.0), (0.0, 0.0)];
        assert_eq!(
            write_way_geometry(&mut buf, &mut scratch, &collapsed, true),
            Geom::Invalid
        );
        // Distinct input that rounds to two distinct positions.
        let flattened = [(0.0, 0.0), (1e-9, 1.0), (0.0, 1.0), (0.0, 0.0)];
        assert_eq!(
            write_way_geometry(&mut buf, &mut scratch, &flattened, true),
            Geom::Invalid
        );
        // Lobes of a bowtie cancel to zero net area.
        let bowtie = [(0.0, 0.0), (1.0, 1.0), (1.0, 0.0), (0.0, 1.0), (0.0, 0.0)];
        assert_eq!(
            write_way_geometry(&mut buf, &mut scratch, &bowtie, true),
            Geom::Invalid
        );
        // An open ring needs no fourth input position.
        assert_eq!(
            write_way_geometry(
                &mut buf,
                &mut scratch,
                &[(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)],
                true
            ),
            Geom::Written
        );
    }

    #[test]
    fn unformattable_area_coordinate_is_invalid() {
        let mut buf = String::new();
        let mut scratch = String::new();
        assert_eq!(
            write_way_geometry(
                &mut buf,
                &mut scratch,
                &[(0.0, 0.0), (f64::NAN, 1.0), (1.0, 0.0), (0.0, 0.0)],
                true
            ),
            Geom::Invalid
        );
    }

    #[test]
    fn positions_outside_the_geographic_domain_are_invalid() {
        let mut buf = String::new();
        let mut scratch = String::new();
        assert_eq!(
            write_point(&mut buf, &mut scratch, 0.0, 100.0),
            Geom::Invalid
        );
        assert_eq!(
            write_point(&mut buf, &mut scratch, -180.000_000_1, 0.0),
            Geom::Invalid
        );
        // Edges are inclusive, and a value that rounds onto a pole is valid.
        assert_eq!(
            write_point(&mut buf, &mut scratch, -180.0, 90.000_000_04),
            Geom::Written
        );
        assert_eq!(buf, "{\"type\":\"Point\",\"coordinates\":[-180,90]}");
        // libosmium's undefined location.
        let undefined = 214.748_364_7;
        assert_eq!(
            write_way_geometry(
                &mut buf,
                &mut scratch,
                &[(1.0, 2.0), (undefined, undefined)],
                false
            ),
            Geom::Invalid
        );
        assert_eq!(
            write_way_geometry(
                &mut buf,
                &mut scratch,
                &[(0.0, 0.0), (1.0, 0.0), (1.0, 90.000_000_1), (0.0, 0.0)],
                true
            ),
            Geom::Invalid
        );
        assert_eq!(
            write_way_geometry(&mut buf, &mut scratch, &[(0.0, 0.0), (0.0, 1e300)], false),
            Geom::Invalid
        );
    }

    #[test]
    fn emitted_e7_parses_formatted_text() {
        let mut scratch = String::new();
        assert_eq!(emitted_e7(&mut scratch, -0.000_000_01), Some(0));
        assert_eq!(emitted_e7(&mut scratch, -0.000_000_1), Some(-1));
        assert_eq!(emitted_e7(&mut scratch, 12.5), Some(125_000_000));
        assert_eq!(emitted_e7(&mut scratch, f64::INFINITY), None);
    }

    #[test]
    fn area_heuristic_truth_table() {
        let closed = [1, 2, 3, 1];
        assert!(is_area_parts(&closed, [("building", "yes")].into_iter()));
        assert!(!is_area_parts(
            &closed,
            [("building", "yes"), ("area", "no")].into_iter()
        ));
        assert!(is_area_parts(&closed, [("area", "yes")].into_iter()));
        assert!(!is_area_parts(
            &closed,
            [("highway", "service")].into_iter()
        ));
        assert!(!is_area_parts(
            &[1, 2, 3],
            [("building", "yes")].into_iter()
        ));
    }
}
