use crate::Way;
use crate::coord_fmt::format_coord;

pub(super) const AREA_KEYS: &[&str] = &[
    "building", "landuse", "natural", "leisure", "amenity", "boundary", "waterway",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WayGeom {
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

pub(super) fn write_point(buf: &mut String, scratch: &mut String, lon: f64, lat: f64) {
    buf.clear();
    buf.push_str("{\"type\":\"Point\",\"coordinates\":");
    push_position(buf, scratch, lon, lat);
    buf.push('}');
}

pub(super) fn collect_coords(way: &Way<'_>, coords: &mut Vec<(f64, f64)>) {
    coords.clear();
    coords.extend(
        way.node_locations()
            .map(|location| (location.lon(), location.lat())),
    );
}

fn signed_area(coords: &[(f64, f64)], closed: bool) -> f64 {
    let mut twice_area = coords
        .windows(2)
        .map(|pair| pair[0].0 * pair[1].1 - pair[1].0 * pair[0].1)
        .sum::<f64>();
    if !closed {
        let first = coords[0];
        let last = coords[coords.len() - 1];
        twice_area += last.0 * first.1 - first.0 * last.1;
    }
    twice_area / 2.0
}

fn has_three_distinct_positions(coords: &[(f64, f64)]) -> bool {
    let Some(&first) = coords.first() else {
        return false;
    };
    let Some(second) = coords.iter().copied().find(|coord| *coord != first) else {
        return false;
    };
    coords
        .iter()
        .any(|coord| *coord != first && *coord != second)
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
) -> WayGeom {
    if !is_area && coords.len() < 2 {
        return WayGeom::Invalid;
    }

    let closed = coords.first() == coords.last();
    if is_area {
        let output_len = coords.len() + usize::from(!closed);
        if output_len < 4 || !has_three_distinct_positions(coords) {
            return WayGeom::Invalid;
        }
    }
    let reverse = is_area && signed_area(coords, closed) < 0.0;

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
    WayGeom::Written
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
        write_point(&mut buf, &mut scratch, 12.5, 55.123_456_7);
        assert_eq!(
            buf,
            "{\"type\":\"Point\",\"coordinates\":[12.5,55.1234567]}"
        );
        assert_eq!(
            write_way_geometry(&mut buf, &mut scratch, &[(1.0, 2.0), (3.0, 4.0)], false),
            WayGeom::Written
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
            WayGeom::Written
        );
        assert_eq!(
            buf,
            "{\"type\":\"Polygon\",\"coordinates\":[[[0,0],[1,0],[0,1],[0,0]]]}"
        );
        assert_eq!(
            write_way_geometry(&mut buf, &mut scratch, &[(0.0, 0.0)], false),
            WayGeom::Invalid
        );
        assert_eq!(
            write_way_geometry(&mut buf, &mut scratch, &[(0.0, 0.0)], true),
            WayGeom::Invalid
        );
        assert_eq!(
            write_way_geometry(
                &mut buf,
                &mut scratch,
                &[(0.0, 0.0), (0.0, 0.0), (1.0, 1.0)],
                true
            ),
            WayGeom::Invalid
        );
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
