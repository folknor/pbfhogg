//! Check address, street, interpolation and admin answers from
//! `pbfhogg geocode-query` against a model built from osmium's decoding
//! of the input used to build the geocode index.
//!
//! Both osmium decodings use a sorted copy when the input header lacks
//! `Sort.Type_then_ID`. Each runs `osmium tags-filter` followed by
//! `osmium add-locations-to-ways -f opl`. The first filters for
//! `nw/addr:housenumber w/highway w/addr:interpolation`. The second
//! filters for `r/boundary=administrative,postal_code` and keeps the
//! untagged node records needed to model boundaries.
//!
//! The first decoding supplies the following candidates under the
//! build-geocode-index rules in docs/cli/commands.md.
//!
//! - an address point: a node, or a way with `building`, carrying
//!   `addr:housenumber` and a non-empty `addr:street` or `addr:place`; a way
//!   is placed at the integer mean (sum over count, truncated) of all its
//!   node coordinates, a repeated closing node counted again. An empty
//!   postcode is no postcode.
//! - a street: a way with `highway` (not one of the excluded classes) and a
//!   `name` key, empty value included, of at least two nodes, each straight
//!   segment between consecutive nodes a candidate.
//!
//! Where a key repeats, the last value counts, as the builder's decoded tags
//! read. For each query the expected answer of a class is its nearest
//! candidate by the reader's flat metric (latitude and longitude in radians,
//! longitude scaled by the cosine of the query's latitude, a segment's
//! nearest point found in that frame), answered when nearer than the larger
//! search radius: the fine radius answers everything within it and the
//! coarse one is searched for a class with nothing there, so together they
//! answer the nearest closer than either. The model finds candidates through
//! a plain latitude/longitude grid that lists each segment under every cell
//! its bounding box touches, with no S2 and no code of pbfhogg's, so a
//! record lost from the index's cells, a wrong coordinate or name, wrong
//! pruning or a wrong fallback shows as a difference.
//!
//! A query is not judged for a class whose nearest candidate lies within
//! the distance tolerance (`GEOCODE_DISTANCE_TOLERANCE_M`, 0.1 mm) of the
//! coarse radius, where the two float paths may round to opposite sides.
//! Candidates within that tolerance of the nearest tie, and the answer must
//! be one of them: every text field and an address's coordinates exactly,
//! its distance within the tolerance of that candidate's, a street's
//! nearest point within two decimicrodegrees (a truncated parameter two
//! float paths compute a rounding apart). Every key of the answer must be
//! present; a missing one is never read as `null`.
//!
//! Interpolation answers are a third nearest-candidate class, judged the
//! same way over the ranges `interp` resolves. On an input with no answering
//! range only absence is judged, and the run says so. Admin answers are
//! modelled by `admin` from the boundary decoding and are
//! judged on two builds: at `--admin-tolerance 0`, and at a non-zero
//! tolerance outside its band. Address, street and interpolation are judged
//! on the non-zero build alone; the admin tolerance does not reach them.
//!
//! Both builds read the input itself. osmium locates a way's refs as it
//! passes the way, so on input whose header does not claim
//! `Sort.Type_then_ID` both decodings read the osmium-sorted copy
//! (`InputOrder`), where every node precedes the ways; the check trusts a
//! claim that is present, so a file falsely claiming its order can still
//! leave a way unlocated (a verifier error, or an interpolation way the model
//! silently refuses), and a pass does not validate the claim.
//!
//! Comparison domain, refused as a verifier error rather than judged: an
//! object repeated in a decoding (the builder keeps the last occurrence in
//! file order, osmium's sort orders repeats by version), an
//! address or street way with a node osmium could not locate (the model
//! would have to guess how the builder shortens it), any modelled
//! coordinate within 10 degrees of the antimeridian or beyond 85 degrees of
//! latitude, and a street segment spanning half a turn of longitude or more
//! (the model does not unwrap longitude; the reader takes the shorter way). libosmium cuts a string
//! at a NUL byte where pbfhogg answers it whole, so a tag value or a
//! member role holding one
//! is outside what this comparison can see; a FAIL naming such a value is
//! that limit, not a pbfhogg defect. So is a way whose refs are split
//! across packed fields with an empty one last: libosmium keeps only the
//! last segment, so the decoding shows a way pbfhogg reads with nodes as
//! one with none, and a boundary listing it is withheld by the model only.
//!
//! Queries are drawn from a fixed seed: uniform points over the extent of
//! the modelled data, and witness points placed near modelled records,
//! aiming at a quota per class and distance category (within the fine
//! radius, between the radii, beyond both) until the draw's attempts run
//! out. The model alone decides what a point is before any pbfhogg answer
//! is read. The quota is a target; the enforced floor is one judged query
//! per class and category, and a category with none is a verifier error,
//! never a PASS that did not look.

mod admin;
mod interp;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use serde_json::Value;

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::{Harness, Verdict};
use crate::reference::ToolRun;
use crate::tunables::{
    GEOCODE_ADMIN_EXACT_BAND_M, GEOCODE_ADMIN_TOLERANCE_M, GEOCODE_ADMIN_WITNESS_REACH_M,
    GEOCODE_ADMIN_WITNESSES, GEOCODE_COARSE_RADIUS_M, GEOCODE_DISTANCE_TOLERANCE_M as TOL,
    GEOCODE_FINE_RADIUS_M, GEOCODE_GRID_E7, GEOCODE_SEED, GEOCODE_UNIFORM_QUERIES,
    GEOCODE_WITNESS_ATTEMPTS, GEOCODE_WITNESS_QUOTA, GEOCODE_WITNESS_REACH_M,
};
use crate::verify_diff_opl::{decode, parse_decimicro};
use crate::verify_input_order::InputOrder;

/// The mean Earth radius the reader converts radians to meters with.
const EARTH_RADIUS_M: f64 = 6_371_008.8;

/// Highway values that make no street.
const EXCLUDED_HIGHWAYS: [&str; 9] = [
    "footway",
    "path",
    "track",
    "steps",
    "cycleway",
    "service",
    "pedestrian",
    "bridleway",
    "construction",
];

/// Mismatches shown per class before only counting.
const SHOWN_MISMATCHES: usize = 10;

#[derive(Debug, Clone, PartialEq)]
struct Address {
    lat: i32,
    lon: i32,
    house_number: String,
    street: Option<String>,
    place: Option<String>,
    postcode: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct Segment {
    name: u32,
    a: (i32, i32),
    b: (i32, i32),
}

#[derive(Default)]
struct Model {
    addresses: Vec<Address>,
    names: Vec<String>,
    segments: Vec<Segment>,
    address_cells: HashMap<(i32, i32), Vec<u32>>,
    segment_cells: HashMap<(i32, i32), Vec<u32>>,
    /// `(min_lat, min_lon, max_lat, max_lon)` of every modelled coordinate.
    extent: Option<(i32, i32, i32, i32)>,
    interp_ways: Vec<interp::RawWay>,
    interp: interp::Resolved,
    interp_segments: Vec<interp::InterpSegment>,
    interp_cells: HashMap<(i32, i32), Vec<u32>>,
}

/// A street candidate's answer: its name and nearest point.
#[derive(Debug, Clone)]
struct StreetAnswer {
    name: String,
    snap_lat: i64,
    snap_lon: i64,
}

/// What the model expects of one class at one query.
#[derive(Debug)]
enum Expect<T> {
    /// Nothing nearer than the coarse radius.
    None,
    /// The nearest is within the fine radius (`true`) or between the radii,
    /// and the answer must be one of these tied candidates.
    Some(bool, Vec<(f64, T)>),
    /// The nearest sits on the coarse radius: not judged.
    Unjudged,
}

impl<T> Expect<T> {
    /// The distance category, 0 fine, 1 between the radii, 2 none.
    fn category(&self) -> Option<usize> {
        match self {
            Self::Some(true, _) => Some(0),
            Self::Some(false, _) => Some(1),
            Self::None => Some(2),
            Self::Unjudged => None,
        }
    }
}

fn cell(lat: i32, lon: i32) -> (i32, i32) {
    (
        lat.div_euclid(GEOCODE_GRID_E7),
        lon.div_euclid(GEOCODE_GRID_E7),
    )
}

fn rad(e7: i64) -> f64 {
    (e7 as f64 * 1e-7).to_radians()
}

impl Model {
    fn widen(&mut self, lat: i32, lon: i32) -> Result<(), String> {
        if lat.abs() > 850_000_000 || lon.abs() > 1_700_000_000 {
            return Err(format!(
                "coordinate {lat},{lon} (decimicrodegrees) is outside the modelled domain \
                 (latitude within 85 degrees, longitude 10 degrees clear of the antimeridian)"
            ));
        }
        let e = self.extent.get_or_insert((lat, lon, lat, lon));
        *e = (e.0.min(lat), e.1.min(lon), e.2.max(lat), e.3.max(lon));
        Ok(())
    }

    fn add_address(&mut self, address: Address) -> Result<(), String> {
        self.widen(address.lat, address.lon)?;
        let index = u32::try_from(self.addresses.len()).map_err(|_| "too many addresses")?;
        self.address_cells
            .entry(cell(address.lat, address.lon))
            .or_default()
            .push(index);
        self.addresses.push(address);
        Ok(())
    }

    fn add_street(&mut self, name: String, coords: &[(i32, i32)]) -> Result<(), String> {
        if coords.len() < 2 {
            return Ok(());
        }
        let name_index = u32::try_from(self.names.len()).map_err(|_| "too many streets")?;
        self.names.push(name);
        for &(lat, lon) in coords {
            self.widen(lat, lon)?;
        }
        for pair in coords.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            // The reader takes a segment the shorter way round in longitude;
            // the model does not unwrap, so a segment spanning half a turn or
            // more is outside its domain.
            if (i64::from(b.1) - i64::from(a.1)).abs() >= 1_800_000_000 {
                return Err(format!(
                    "a street segment from {a:?} to {b:?} spans half a turn of longitude, \
                     outside the modelled domain"
                ));
            }
            let index = u32::try_from(self.segments.len()).map_err(|_| "too many segments")?;
            let (lo, hi) = (
                cell(a.0.min(b.0), a.1.min(b.1)),
                cell(a.0.max(b.0), a.1.max(b.1)),
            );
            for row in lo.0..=hi.0 {
                for col in lo.1..=hi.1 {
                    self.segment_cells
                        .entry((row, col))
                        .or_default()
                        .push(index);
                }
            }
            self.segments.push(Segment {
                name: name_index,
                a,
                b,
            });
        }
        Ok(())
    }

    /// The grid cells that hold every candidate nearer than the coarse
    /// radius to `(lat, lon)`, with one cell of margin.
    fn cells_near(lat: f64, lon: f64) -> impl Iterator<Item = (i32, i32)> {
        let reach_rad = GEOCODE_COARSE_RADIUS_M / EARTH_RADIUS_M;
        let dlat_deg = reach_rad.to_degrees();
        let dlon_deg = dlat_deg / lat.to_radians().cos();
        #[allow(clippy::cast_possible_truncation)]
        let e7 = |deg: f64| (deg * 1e7).floor() as i32;
        let lo = cell(e7(lat - dlat_deg), e7(lon - dlon_deg));
        let hi = cell(e7(lat + dlat_deg), e7(lon + dlon_deg));
        (lo.0 - 1..=hi.0 + 1).flat_map(move |row| (lo.1 - 1..=hi.1 + 1).map(move |col| (row, col)))
    }

    fn expect_address(&self, lat: f64, lon: f64) -> Expect<Address> {
        let (qlat, qlon, cos) = (lat.to_radians(), lon.to_radians(), lat.to_radians().cos());
        let mut found: Vec<(f64, usize)> = Vec::new();
        for key in Self::cells_near(lat, lon) {
            for &i in self.address_cells.get(&key).into_iter().flatten() {
                let a = &self.addresses[i as usize];
                let dlat = rad(i64::from(a.lat)) - qlat;
                let dlon = (rad(i64::from(a.lon)) - qlon) * cos;
                let d = (dlat * dlat + dlon * dlon).sqrt() * EARTH_RADIUS_M;
                found.push((d, i as usize));
            }
        }
        expectation(&found, |i| self.addresses[i].clone())
    }

    fn expect_street(&self, lat: f64, lon: f64) -> Expect<StreetAnswer> {
        let (qlat, qlon, cos) = (lat.to_radians(), lon.to_radians(), lat.to_radians().cos());
        let mut seen = std::collections::HashSet::new();
        let mut found: Vec<(f64, StreetAnswer)> = Vec::new();
        for key in Self::cells_near(lat, lon) {
            for &i in self.segment_cells.get(&key).into_iter().flatten() {
                if !seen.insert(i) {
                    continue;
                }
                let s = self.segments[i as usize];
                let (t, distance_m) = nearest_on_segment(qlat, qlon, cos, s.a, s.b);
                #[allow(clippy::cast_possible_truncation)]
                let along =
                    |a: i32, b: i32| (f64::from(a) + t * (f64::from(b) - f64::from(a))) as i64;
                found.push((
                    distance_m,
                    StreetAnswer {
                        name: self.names[s.name as usize].clone(),
                        snap_lat: along(s.a.0, s.b.0),
                        snap_lon: along(s.a.1, s.b.1),
                    },
                ));
            }
        }
        let indexed: Vec<(f64, usize)> = found.iter().enumerate().map(|(i, f)| (f.0, i)).collect();
        expectation(&indexed, |i| found[i].1.clone())
    }

    /// Resolve the interpolation ways once every address point is known.
    fn finish(&mut self) -> Result<(), String> {
        self.interp = interp::resolve(&self.interp_ways, &self.addresses);
        let (segments, cells) = interp::grid(&self.interp.sections)?;
        self.interp_segments = segments;
        self.interp_cells = cells;
        Ok(())
    }

    fn expect_interp(&self, lat: f64, lon: f64) -> Expect<interp::InterpAnswer> {
        let (qlat, qlon, cos) = (lat.to_radians(), lon.to_radians(), lat.to_radians().cos());
        let mut seen = std::collections::HashSet::new();
        let mut found: Vec<(f64, interp::InterpAnswer)> = Vec::new();
        for key in Self::cells_near(lat, lon) {
            for &i in self.interp_cells.get(&key).into_iter().flatten() {
                if !seen.insert(i) {
                    continue;
                }
                let s = self.interp_segments[i as usize];
                let section = &self.interp.sections[s.section as usize];
                let index = usize::from(s.index);
                let (a, b) = (section.nodes[index], section.nodes[index + 1]);
                let (t, distance_m) = nearest_on_segment(qlat, qlon, cos, a, b);
                found.push((distance_m, section.answer(index, t)));
            }
        }
        let indexed: Vec<(f64, usize)> = found.iter().enumerate().map(|(i, f)| (f.0, i)).collect();
        expectation(&indexed, |i| found[i].1.clone())
    }
}

/// The nearest point of segment `a`-`b` to the query: its parameter and
/// distance in meters, in the frame with the query at the origin, x the
/// longitude scaled by the query's cosine and y the latitude, both radians.
fn nearest_on_segment(qlat: f64, qlon: f64, cos: f64, a: (i32, i32), b: (i32, i32)) -> (f64, f64) {
    let ax = (rad(i64::from(a.1)) - qlon) * cos;
    let ay = rad(i64::from(a.0)) - qlat;
    let dx = (rad(i64::from(b.1)) - rad(i64::from(a.1))) * cos;
    let dy = rad(i64::from(b.0)) - rad(i64::from(a.0));
    let len_sq = dx * dx + dy * dy;
    let t = if len_sq == 0.0 {
        0.0
    } else {
        (-(ax * dx + ay * dy) / len_sq).clamp(0.0, 1.0)
    };
    let (px, py) = (ax + t * dx, ay + t * dy);
    (t, (px * px + py * py).sqrt() * EARTH_RADIUS_M)
}

/// The expectation of one class from every candidate's distance.
fn expectation<T>(found: &[(f64, usize)], get: impl Fn(usize) -> T) -> Expect<T> {
    let nearest = found.iter().map(|f| f.0).fold(f64::INFINITY, f64::min);
    if nearest >= GEOCODE_COARSE_RADIUS_M + TOL {
        return Expect::None;
    }
    if nearest > GEOCODE_COARSE_RADIUS_M - TOL {
        return Expect::Unjudged;
    }
    let tied = found
        .iter()
        .filter(|f| f.0 <= nearest + TOL)
        .map(|&(d, i)| (d, get(i)))
        .collect();
    Expect::Some(nearest < GEOCODE_FINE_RADIUS_M, tied)
}

/// The value of tag `key` in a decoded tag list, the last occurrence winning.
fn tag<'t>(tags: &'t [(String, String)], key: &str) -> Option<&'t str> {
    tags.iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value.filter(|v| !v.is_empty()).map(str::to_owned)
}

/// One OPL record, as far as the model reads it.
struct Record {
    kind: u8,
    tags: Vec<(String, String)>,
    location: Option<(i32, i32)>,
    /// A way's node locations, `None` where osmium had none.
    nodes: Vec<Option<(i32, i32)>>,
    /// A way's node ids, signed, one per entry of `nodes`.
    refs: Vec<i64>,
    /// A relation's members: type letter, id and role.
    members: Vec<(u8, i64, String)>,
    id: String,
}

fn parse_record(line: &str) -> Result<Record, String> {
    let mut fields = line.split(' ');
    let first = fields.next().unwrap_or_default();
    let kind = *first.as_bytes().first().ok_or("empty record")?;
    let (mut tags, mut x, mut y, mut nodes) = (Vec::new(), None, None, Vec::new());
    let (mut members, mut refs) = (Vec::new(), Vec::new());
    let coordinate = |text: &str| {
        parse_decimicro(text)
            .and_then(|v| i32::try_from(v).ok())
            .ok_or_else(|| format!("{first}: coordinate {text:?}"))
    };
    for field in fields {
        let (letter, value) = field.split_at(field.len().min(1));
        match letter {
            "T" if !value.is_empty() => {
                for pair in value.split(',') {
                    let (k, v) = pair
                        .split_once('=')
                        .ok_or_else(|| format!("{first}: tag {pair:?}"))?;
                    tags.push((decode(k)?, decode(v)?));
                }
            }
            "x" if !value.is_empty() => x = Some(coordinate(value)?),
            "y" if !value.is_empty() => y = Some(coordinate(value)?),
            "N" if !value.is_empty() => {
                for token in value.split(',') {
                    let id = token
                        .strip_prefix('n')
                        .map(|t| t.split_once('x').map_or(t, |(id, _)| id))
                        .and_then(|id| id.parse::<i64>().ok())
                        .ok_or_else(|| format!("{first}: node ref {token:?}"))?;
                    refs.push(id);
                    // A bare ref, `xy`, or a complete pair; nothing else.
                    let location = match token.split_once('x') {
                        None => None,
                        Some((_, at)) => match at.split_once('y') {
                            Some(("", "")) => None,
                            Some((lon, lat)) if !lon.is_empty() && !lat.is_empty() => {
                                Some((lat, lon))
                            }
                            _ => return Err(format!("{first}: node ref {token:?}")),
                        },
                    };
                    nodes.push(match location {
                        Some((lat, lon)) => Some((coordinate(lat)?, coordinate(lon)?)),
                        None => None,
                    });
                }
            }
            "M" if !value.is_empty() => {
                for token in value.split(',') {
                    let (member, role) = token
                        .split_once('@')
                        .ok_or_else(|| format!("{first}: member {token:?}"))?;
                    let (kind, id) = member.split_at(member.len().min(1));
                    let id: i64 = id
                        .parse()
                        .map_err(|_| format!("{first}: member {token:?}"))?;
                    members.push((*kind.as_bytes().first().unwrap_or(&b'?'), id, decode(role)?));
                }
            }
            _ => {}
        }
    }
    Ok(Record {
        kind,
        tags,
        location: x.zip(y).map(|(lon, lat)| (lat, lon)),
        nodes,
        refs,
        members,
        id: first.to_owned(),
    })
}

/// The model of the OPL file at `path`.
fn build_model(path: &Path) -> Result<Model, Error> {
    let file = std::fs::File::open(path)
        .map_err(|e| Error::Failed(format!("cannot open {}: {e}", path.display())))?;
    let mut model = Model::default();
    let mut previous = None;
    for (n, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|e| Error::Failed(format!("{}: {e}", path.display())))?;
        let at = |why: String| Error::Failed(format!("{} line {}: {why}", path.display(), n + 1));
        let record = parse_record(&line).map_err(at)?;
        refuse_repeat(&mut previous, &record).map_err(Error::Tool)?;
        model_record(&mut model, &record).map_err(Error::Tool)?;
    }
    model.finish().map_err(Error::Tool)?;
    Ok(model)
}

/// Refuse a record repeating the previous one's type and ID. The decodings
/// read input in some type-then-ID order (the input under its sort claim, or
/// osmium's sorted copy), so repeats that survive selection are adjacent; the
/// builder keeps the last occurrence in file order while osmium's sort orders
/// them by version, so a repeat is outside the compared domain. This does not
/// certify the input's uniqueness: a non-adjacent repeat in a file falsely
/// claiming its sort order is not seen.
fn refuse_repeat(previous: &mut Option<(u8, String)>, r: &Record) -> Result<(), String> {
    if previous
        .as_ref()
        .is_some_and(|(kind, id)| *kind == r.kind && *id == r.id)
    {
        return Err(format!(
            "{}: the decoding repeats this object, outside the compared domain",
            r.id
        ));
    }
    *previous = Some((r.kind, r.id.clone()));
    Ok(())
}

fn model_record(model: &mut Model, r: &Record) -> Result<(), String> {
    let house_number = tag(&r.tags, "addr:housenumber");
    let street = non_empty(tag(&r.tags, "addr:street"));
    let place = non_empty(tag(&r.tags, "addr:place"));
    let postcode = non_empty(tag(&r.tags, "addr:postcode"));
    let addressed = house_number.is_some() && (street.is_some() || place.is_some());
    match r.kind {
        b'n' if addressed => {
            let (lat, lon) = r
                .location
                .ok_or_else(|| format!("{}: an address node without a location", r.id))?;
            model.add_address(Address {
                lat,
                lon,
                house_number: house_number.unwrap_or_default().to_owned(),
                street,
                place,
                postcode,
            })
        }
        b'w' => {
            if house_number.is_none()
                && let Some(mode) = tag(&r.tags, "addr:interpolation")
            {
                for &(lat, lon) in r.nodes.iter().flatten() {
                    model.widen(lat, lon)?;
                }
                let located: Vec<(i32, i32)> = r.nodes.iter().flatten().copied().collect();
                if located
                    .windows(2)
                    .any(|w| (i64::from(w[1].1) - i64::from(w[0].1)).abs() >= 1_800_000_000)
                {
                    return Err(format!(
                        "{}: an interpolation segment spans half a turn of longitude, outside \
                         the modelled domain",
                        r.id
                    ));
                }
                model.interp_ways.push(interp::RawWay {
                    mode: interp::Mode::from_tag(mode),
                    street: street.clone(),
                    place: place.clone(),
                    nodes: r.nodes.clone(),
                });
            }
            let highway = tag(&r.tags, "highway");
            let is_street = highway.is_some_and(|h| !EXCLUDED_HIGHWAYS.contains(&h))
                && tag(&r.tags, "name").is_some();
            let is_building = addressed && tag(&r.tags, "building").is_some();
            if !is_street && !is_building {
                return Ok(());
            }
            let coords: Vec<(i32, i32)> = r
                .nodes
                .iter()
                .copied()
                .collect::<Option<_>>()
                .ok_or_else(|| {
                    format!(
                        "{}: a node of this address or street way has no location, outside \
                         the compared domain",
                        r.id
                    )
                })?;
            if is_building && !coords.is_empty() {
                let count = i64::try_from(coords.len()).map_err(|_| "way too long")?;
                let (sum_lat, sum_lon) = coords.iter().fold((0_i64, 0_i64), |s, c| {
                    (s.0 + i64::from(c.0), s.1 + i64::from(c.1))
                });
                #[allow(clippy::cast_possible_truncation)]
                model.add_address(Address {
                    lat: (sum_lat / count) as i32,
                    lon: (sum_lon / count) as i32,
                    house_number: house_number.unwrap_or_default().to_owned(),
                    street: street.clone(),
                    place: place.clone(),
                    postcode: postcode.clone(),
                })?;
            }
            if is_street {
                let name = tag(&r.tags, "name").unwrap_or_default().to_owned();
                model.add_street(name, &coords)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// splitmix64: a fixed, documented sequence.
struct Draw(u64);

impl Draw {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1_u64 << 53) as f64
    }
}

/// A query as written to the points file and read back, so the model and
/// pbfhogg query the same `f64`s.
fn point(lat: f64, lon: f64) -> (String, f64, f64) {
    let text = format!("{lat:.7} {lon:.7}");
    let (a, b) = text.split_once(' ').unwrap_or_default();
    (
        text.clone(),
        a.parse().unwrap_or(lat),
        b.parse().unwrap_or(lon),
    )
}

/// One query and the model's expectation of each class.
struct Query {
    text: String,
    lat: f64,
    lon: f64,
    address: Expect<Address>,
    street: Expect<StreetAnswer>,
    interpolation: Expect<interp::InterpAnswer>,
}

fn query(model: &Model, lat: f64, lon: f64) -> Query {
    let (text, lat, lon) = point(lat, lon);
    Query {
        address: model.expect_address(lat, lon),
        street: model.expect_street(lat, lon),
        interpolation: model.expect_interp(lat, lon),
        text,
        lat,
        lon,
    }
}

const CLASSES: [&str; 3] = ["address", "street", "interpolation"];
const CATEGORIES: [&str; 3] = ["within the fine radius", "between the radii", "beyond both"];

/// A point `reach_m` meters from `(lat, lon)` (decimicrodegrees) at
/// `bearing`.
fn offset(lat: i32, lon: i32, reach_m: f64, bearing: f64) -> (f64, f64) {
    let reach = reach_m / EARTH_RADIUS_M;
    let lat = f64::from(lat) * 1e-7 + (reach * bearing.cos()).to_degrees();
    let lon = f64::from(lon) * 1e-7 + (reach * bearing.sin()).to_degrees() / lat.to_radians().cos();
    (lat, lon)
}

/// The fixed query set: uniform points over the extent, then witnesses
/// until every class the model holds has its quota in each category, then
/// admin witnesses near boundary vertices.
fn queries(model: &Model, admin: &admin::AdminModel) -> Result<Vec<Query>, Error> {
    let (min_lat, min_lon, max_lat, max_lon) = model
        .extent
        .ok_or_else(|| Error::tool("the input holds no address point and no street"))?;
    let deg = |e7: i32| f64::from(e7) * 1e-7;
    let mut draw = Draw(GEOCODE_SEED);
    let mut out = Vec::new();
    for _ in 0..GEOCODE_UNIFORM_QUERIES {
        let lat = deg(min_lat) + draw.unit() * (deg(max_lat) - deg(min_lat));
        let lon = deg(min_lon) + draw.unit() * (deg(max_lon) - deg(min_lon));
        out.push(query(model, lat, lon));
    }
    // A class the model holds nothing of has no witness to look for.
    let held = [true, true, !model.interp_segments.is_empty()];
    let mut filled = [[0_usize; 3]; 3];
    for q in &out {
        tally(&mut filled, q);
    }
    // Anchors are drawn class first, uniformly over every class with any,
    // then uniformly within it: a draw over the pooled anchors would almost
    // never reach a small class (a few thousand interpolation segments
    // beside tens of millions of addresses). A filled class keeps its share,
    // since its anchors still witness other classes' categories.
    let pools = [
        model.addresses.len(),
        model.segments.len(),
        model.interp_segments.len(),
    ];
    let classes: Vec<usize> = (0..3).filter(|&c| pools[c] > 0).collect();
    let full = |filled: &[[usize; 3]; 3]| {
        (0..3).all(|c| !held[c] || filled[c].iter().all(|&n| n >= GEOCODE_WITNESS_QUOTA))
    };
    for _ in 0..GEOCODE_WITNESS_ATTEMPTS {
        if classes.is_empty() || full(&filled) {
            break;
        }
        #[allow(clippy::cast_possible_truncation)]
        let class = classes[(draw.next() % classes.len() as u64) as usize];
        #[allow(clippy::cast_possible_truncation)]
        let pick = (draw.next() % pools[class] as u64) as usize;
        let (lat, lon) = match class {
            0 => (model.addresses[pick].lat, model.addresses[pick].lon),
            1 => model.segments[pick].a,
            _ => {
                let s = model.interp_segments[pick];
                model.interp.sections[s.section as usize].nodes[usize::from(s.index)]
            }
        };
        let reach = draw.unit() * GEOCODE_WITNESS_REACH_M;
        let (lat, lon) = offset(lat, lon, reach, draw.unit() * std::f64::consts::TAU);
        let q = query(model, lat, lon);
        let wanted = |class: usize, c: Option<usize>| {
            held[class] && c.is_some_and(|c| filled[class][c] < GEOCODE_WITNESS_QUOTA)
        };
        if wanted(0, q.address.category())
            || wanted(1, q.street.category())
            || wanted(2, q.interpolation.category())
        {
            tally(&mut filled, &q);
            out.push(q);
        }
    }
    for class in 0..3 {
        if !held[class] {
            continue;
        }
        for (category, n) in CATEGORIES.iter().zip(filled[class]) {
            if n == 0 {
                return Err(Error::tool(format!(
                    "no {} query {category} was found in the input; the check cannot \
                     cover that path",
                    CLASSES[class]
                )));
            }
        }
    }
    // Admin witnesses: near outer and hole vertices, where containment,
    // holes and competing polygons are decided.
    let vertices: Vec<(i32, i32)> = admin
        .polygons
        .iter()
        .flat_map(|p| {
            p.holes
                .iter()
                .flatten()
                .chain(p.outer.iter().step_by((p.outer.len() / 64).max(1)))
        })
        .copied()
        .collect();
    if !vertices.is_empty() {
        for _ in 0..GEOCODE_ADMIN_WITNESSES {
            #[allow(clippy::cast_possible_truncation)]
            let (lat, lon) = vertices[(draw.next() % vertices.len() as u64) as usize];
            let reach = draw.unit() * GEOCODE_ADMIN_WITNESS_REACH_M;
            let (lat, lon) = offset(lat, lon, reach, draw.unit() * std::f64::consts::TAU);
            out.push(query(model, lat, lon));
        }
    }
    Ok(out)
}

fn tally(filled: &mut [[usize; 3]; 3], q: &Query) {
    let categories = [
        q.address.category(),
        q.street.category(),
        q.interpolation.category(),
    ];
    for (class, c) in categories.into_iter().enumerate() {
        if let Some(c) = c {
            filled[class][c] += 1;
        }
    }
}

/// osmium's `tags-filter` of `pbf` by `filters` (keeping referenced
/// objects), then `add-locations-to-ways` as OPL; `keep_nodes` keeps the
/// untagged node records too.
fn located_opl(
    harness: &mut Harness,
    pbf: &Path,
    outdir: &Path,
    stem: &str,
    filters: &[&str],
    keep_nodes: bool,
) -> Result<std::path::PathBuf, Error> {
    let filtered = outdir.join(format!("{stem}-filtered.osm.pbf"));
    let ended = harness.reference(
        &ToolRun::osmium(format!("{stem}-filter.1"))
            .arg("tags-filter")
            .input(pbf)
            .args(filters)
            .arg("-o")
            .output(&filtered)
            .arg("--overwrite"),
        &outdir.join(format!("osmium-{stem}-filter.stdout")),
    )?;
    harness.require_success(&ended, "osmium tags-filter")?;
    let opl = outdir.join(format!("{stem}-located.opl"));
    let ended = harness.reference(
        &ToolRun::osmium(format!("{stem}-locations.1"))
            .args(&["add-locations-to-ways", "--ignore-missing-nodes"])
            .args(if keep_nodes {
                &["--keep-untagged-nodes"][..]
            } else {
                &[]
            })
            .input(&filtered)
            .args(&["-f", "opl", "-o"])
            .output(&opl)
            .arg("--overwrite"),
        &outdir.join(format!("osmium-{stem}-locations.stdout")),
    )?;
    harness.require_success(&ended, "osmium add-locations-to-ways")?;
    Ok(opl)
}

/// The admin model of osmium's decoding of the input's boundary relations.
///
/// The decoding keeps every node record (`located_opl` with `keep_nodes`),
/// and each is recorded, so the model tells a ref the input lacks (no
/// record) from one whose record carries no location. The decoding reads
/// sorted input (see `run`), so a ref osmium left unlocated is joined
/// against its record; only a file falsely claiming its sort order can
/// still store a node after its way.
fn admin_model(path: &Path) -> Result<admin::AdminModel, Error> {
    let file = std::fs::File::open(path)
        .map_err(|e| Error::Failed(format!("cannot open {}: {e}", path.display())))?;
    let mut ways: HashMap<i64, admin::Way> = HashMap::new();
    let mut node_records: admin::NodeRecords = HashMap::new();
    let mut relations = Vec::new();
    let mut previous = None;
    for (n, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|e| Error::Failed(format!("{}: {e}", path.display())))?;
        let at = |why: String| Error::Failed(format!("{} line {}: {why}", path.display(), n + 1));
        let record = parse_record(&line).map_err(at)?;
        refuse_repeat(&mut previous, &record).map_err(Error::Tool)?;
        match record.kind {
            b'w' => {
                let id = record.id[1..]
                    .parse()
                    .map_err(|_| at(format!("way id {}", record.id)))?;
                ways.insert(
                    id,
                    admin::Way {
                        refs: record.refs,
                        nodes: record.nodes,
                    },
                );
            }
            b'n' => {
                let id = record.id[1..]
                    .parse()
                    .map_err(|_| at(format!("node id {}", record.id)))?;
                node_records.insert(id, record.location);
            }
            b'r' => relations.push(record),
            _ => {}
        }
    }
    admin::AdminModel::build(&relations, &ways, &node_records).map_err(Error::Tool)
}

/// Build an index of `pbf` at `admin_tolerance` meters into `index`.
fn build_index(
    harness: &mut Harness,
    pbf: &Path,
    index: &Path,
    admin_tolerance: f64,
    direct_io: bool,
) -> Result<(), Error> {
    let ended = harness.pbfhogg_discard(
        PbfArgv::new("build-geocode-index")
            .input(pbf)
            .output_flag("--output-dir", index)
            .scratch()
            .arg("--search-radius")
            .arg(GEOCODE_FINE_RADIUS_M.to_string())
            .arg("--coarse-search-radius")
            .arg(GEOCODE_COARSE_RADIUS_M.to_string())
            .arg("--admin-tolerance")
            .arg(admin_tolerance.to_string())
            .arg_if(direct_io, "--direct-io"),
    )?;
    harness.require_success(&ended, "pbfhogg build-geocode-index")
}

/// `geocode-query` over `index`: one parsed answer per query, each echoing
/// its query.
fn answers(
    harness: &mut Harness,
    index: &Path,
    points: &Path,
    out: &Path,
    queries: &[Query],
) -> Result<Vec<Value>, Error> {
    let ended = harness.pbfhogg(
        PbfArgv::new("geocode-query").input(index).input(points),
        out,
    )?;
    harness.require_success(&ended, "pbfhogg geocode-query")?;
    let text = std::fs::read_to_string(out)
        .map_err(|e| Error::Failed(format!("pbfhogg answers are not readable text: {e}")))?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() != queries.len() {
        return Err(Error::Failed(format!(
            "pbfhogg geocode-query printed {} answers for {} queries",
            lines.len(),
            queries.len()
        )));
    }
    queries
        .iter()
        .zip(&lines)
        .map(|(q, line)| {
            let answer: Value = serde_json::from_str(line)
                .map_err(|e| Error::Failed(format!("answer to {}: not JSON: {e}", q.text)))?;
            if answer["lat"].as_f64() != Some(q.lat) || answer["lon"].as_f64() != Some(q.lon) {
                return Err(Error::Failed(format!(
                    "answer {line} does not echo its query {}",
                    q.text
                )));
            }
            Ok(answer)
        })
        .collect()
}

/// Judge the nearest-candidate classes, recording one verdict per class.
fn judge_classes(harness: &mut Harness, model: &Model, queries: &[Query], answered: &[Value]) {
    let mut judged = [[0_usize; 3]; 3];
    let mut mismatches: [Vec<String>; 3] = Default::default();
    for (q, answer) in queries.iter().zip(answered) {
        let class = |key: &str| {
            answer
                .get(key)
                .ok_or_else(|| format!("query {}: the answer has no {key} key", q.text))
        };
        let results = [
            class("address").and_then(|v| judge(&q.address, v)),
            class("street").and_then(|v| judge(&q.street, v)),
            class("interpolation").and_then(|v| judge(&q.interpolation, v)),
        ];
        for (class, result) in results.into_iter().enumerate() {
            match result {
                Ok(Some(c)) => judged[class][c] += 1,
                Ok(None) => {}
                Err(why) => mismatches[class].push(format!("query {}: {why}", q.text)),
            }
        }
    }
    let absence_only = model.interp_segments.is_empty();
    for (class, name) in CLASSES.iter().enumerate() {
        let [fine, between, none] = judged[class];
        let wrong = &mismatches[class];
        harness.msg(&format!(
            "  {name}: {} judged ({fine} within the fine radius, {between} between the radii, \
             {none} beyond both), {} wrong{}",
            fine + between + none + wrong.len(),
            wrong.len(),
            if class == 2 && absence_only {
                "; the input holds no answering range, so only absence was judged"
            } else {
                ""
            }
        ));
        for why in wrong.iter().take(SHOWN_MISMATCHES) {
            harness.msg(&format!("    {why}"));
        }
        harness.record(
            &format!("geocode {name} answers vs the osmium-derived model"),
            Verdict::from_pass(wrong.is_empty()),
        );
    }
}

/// Judge one build's admin answers at `band_m`, recording its verdict.
fn judge_admin(
    harness: &mut Harness,
    admin: &admin::AdminModel,
    queries: &[Query],
    answered: &[Value],
    label: &str,
    band_m: f64,
) -> Result<(), Error> {
    let mut tally = admin::AdminTally::default();
    let mut wrong = Vec::new();
    for (q, answer) in queries.iter().zip(answered) {
        let expect = admin.expect(q.lat, q.lon, band_m);
        let result = answer
            .get("admin")
            .ok_or_else(|| "the answer has no admin key".to_owned())
            .and_then(|v| admin.judge(&expect, v, &mut tally));
        if let Err(why) = result {
            wrong.push(format!("query {}: {why}", q.text));
        }
    }
    harness.msg(&format!(
        "  admin {label}: {} containing answers judged ({} among several containing areas, \
         {} postal, {} with a country code), {} empty levels inside a polygon's box, {} \
         levels inside an owned hole, {} levels not judged, {} queries wrong",
        tally.containing,
        tally.several,
        tally.postal,
        tally.country_code,
        tally.empty_boxed,
        tally.in_owned_hole,
        tally.unjudged,
        wrong.len()
    ));
    for why in wrong.iter().take(SHOWN_MISMATCHES) {
        harness.msg(&format!("    {why}"));
    }
    // A withheld relation is judged as absent, so an input whose every
    // boundary is withheld covers the absence path only; one the model
    // leaves unmodelled covers nothing.
    if admin.relations > admin.withheld && tally.containing == 0 && wrong.is_empty() {
        return Err(Error::tool(format!(
            "no admin answer {label} was judged containing; the check cannot cover that path"
        )));
    }
    harness.record(
        &format!("geocode admin answers {label} vs the osmium-derived model"),
        Verdict::from_pass(wrong.is_empty()),
    );
    Ok(())
}

pub fn run(harness: &mut Harness, pbf: &Path, direct_io: bool) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("geocode")?;
    harness.msg("=== verify geocode (address, street, interpolation and admin answers) ===");

    let index = outdir.join("index");
    build_index(harness, pbf, &index, GEOCODE_ADMIN_TOLERANCE_M, direct_io)?;
    let exact_index = outdir.join("index-tolerance-0");
    build_index(harness, pbf, &exact_index, 0.0, direct_io)?;

    // osmium locates a way's refs as it passes the way, so on claimless input
    // a way stored before its nodes would come out unlocated though the
    // builder resolves it; the decodings read an osmium-sorted copy there.
    let order = InputOrder::of(harness, pbf, &outdir)?;
    let reference = order.reference_input.as_path();
    let model = build_model(&located_opl(
        harness,
        reference,
        &outdir,
        "geocode",
        &["nw/addr:housenumber", "w/highway", "w/addr:interpolation"],
        false,
    )?)?;
    let admin = admin_model(&located_opl(
        harness,
        reference,
        &outdir,
        "geocode-admin",
        &["r/boundary=administrative,postal_code"],
        true,
    )?)?;
    harness.msg(&format!(
        "  model: {} address points, {} street segments, {} interpolation ranges from {} \
         resolved ways ({} refused), {} admin polygons from {} boundary relations ({} \
         withheld as incomplete or listing a way in both roles, {} unjudged boxes of \
         unmodelled relations and undecided holes)",
        model.addresses.len(),
        model.segments.len(),
        model.interp.sections.len(),
        model.interp.resolved_ways,
        model.interp.refused_ways,
        admin.polygons.len(),
        admin.relations,
        admin.withheld,
        admin.unmodelled.len(),
    ));
    // Every unmodelled relation makes its level unjudged across its box, so
    // name the coarse ones: a run says which relation a gap comes from.
    for ((id, level, why), (_, b)) in admin.unmodelled_why.iter().zip(&admin.unmodelled) {
        if *level <= 4 {
            harness.msg(&format!(
                "    unmodelled level {level} {id} ({why:?}), box {:.4},{:.4} to {:.4},{:.4}",
                f64::from(b.0) * 1e-7,
                f64::from(b.1) * 1e-7,
                f64::from(b.2) * 1e-7,
                f64::from(b.3) * 1e-7,
            ));
        }
    }
    let queries = queries(&model, &admin)?;
    let points = outdir.join("points.txt");
    {
        let mut file = std::io::BufWriter::new(std::fs::File::create(&points)?);
        for q in &queries {
            writeln!(file, "{}", q.text)?;
        }
        file.flush()?;
    }
    harness.run.watch(&points)?;

    let answered = answers(
        harness,
        &index,
        &points,
        &outdir.join("pbfhogg-answers.jsonl"),
        &queries,
    )?;
    judge_classes(harness, &model, &queries, &answered);
    let exact = answers(
        harness,
        &exact_index,
        &points,
        &outdir.join("pbfhogg-answers-tolerance-0.jsonl"),
        &queries,
    )?;
    judge_admin(
        harness,
        &admin,
        &queries,
        &exact,
        "at --admin-tolerance 0",
        GEOCODE_ADMIN_EXACT_BAND_M,
    )?;
    judge_admin(
        harness,
        &admin,
        &queries,
        &answered,
        "outside the band of a non-zero --admin-tolerance",
        GEOCODE_ADMIN_TOLERANCE_M * 1.01 + 1.0,
    )?;
    Ok(Some(format!(
        "{} queries: every judged address, street and interpolation answer is the nearest \
         documented candidate in the osmium decoding of the input, or none beyond the radii, \
         and every judged admin level the least containing area of the modelled boundaries",
        queries.len()
    )))
}

/// Whether an answer is this candidate: the same record, at the candidate's
/// distance within the tolerance.
trait Candidate: std::fmt::Debug {
    type Answer: std::fmt::Debug;
    fn read(v: &Value) -> Result<(Self::Answer, f64), String>;
    fn is(&self, got: &Self::Answer) -> bool;
}

impl Candidate for Address {
    type Answer = Self;

    fn read(v: &Value) -> Result<(Self, f64), String> {
        read_address(v)
    }

    fn is(&self, other: &Self) -> bool {
        self == other
    }
}

impl Candidate for StreetAnswer {
    type Answer = Self;

    fn read(v: &Value) -> Result<(Self, f64), String> {
        read_street(v)
    }

    /// The name exactly; the nearest point within two decimicrodegrees, the
    /// truncation of a parameter two float paths compute a rounding apart.
    fn is(&self, other: &Self) -> bool {
        self.name == other.name
            && self.snap_lat.abs_diff(other.snap_lat) <= 2
            && self.snap_lon.abs_diff(other.snap_lon) <= 2
    }
}

/// An interpolation answer as printed.
#[derive(Debug)]
struct InterpGot {
    street: Option<String>,
    place: Option<String>,
    house_number: u32,
}

impl Candidate for interp::InterpAnswer {
    type Answer = InterpGot;

    fn read(v: &Value) -> Result<(InterpGot, f64), String> {
        let house_number = field(v, "house_number")?
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| format!("house_number is not a u32 in {v}"))?;
        Ok((
            InterpGot {
                street: optional(v, "street")?,
                place: optional(v, "place")?,
                house_number,
            },
            distance(v)?,
        ))
    }

    /// The street and place exactly, the number one its position allows.
    fn is(&self, got: &InterpGot) -> bool {
        self.street == got.street && self.place == got.place && self.allows(got.house_number)
    }
}

/// One class's answer against its expectation: the judged category, `None`
/// when not judged, or why it is wrong.
fn judge<T: Candidate>(expect: &Expect<T>, answer: &Value) -> Result<Option<usize>, String> {
    let category = expect.category();
    match (expect, answer.is_null()) {
        (Expect::Unjudged, _) => Ok(None),
        (Expect::None, true) => Ok(category),
        (Expect::None, false) => Err(format!("answered {answer}, expected nothing")),
        (Expect::Some(_, tied), true) => {
            Err(format!("answered nothing, expected {:?}", tied.first()))
        }
        (Expect::Some(_, tied), false) => {
            let (got, at) = T::read(answer)?;
            if tied
                .iter()
                .any(|(d, want)| want.is(&got) && (d - at).abs() <= TOL)
            {
                Ok(category)
            } else {
                Err(format!(
                    "answered {got:?} at {at} m, expected one of {} tied: {:?}",
                    tied.len(),
                    tied.first()
                ))
            }
        }
    }
}

/// The value at `key`, which the contract says is always present: a missing
/// key is a defect, never read as `null`.
fn field<'v>(v: &'v Value, key: &str) -> Result<&'v Value, String> {
    if !v.is_object() {
        return Err(format!("{v} is not an object"));
    }
    v.get(key).ok_or_else(|| format!("no {key} key in {v}"))
}

fn string(v: &Value, key: &str) -> Result<String, String> {
    field(v, key)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{key} is not a string in {v}"))
}

fn optional(v: &Value, key: &str) -> Result<Option<String>, String> {
    match field(v, key)? {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        other => Err(format!("{key} is {other}, not a string or null")),
    }
}

/// A decimicrodegree coordinate, within the geographic domain.
fn integer(v: &Value, key: &str) -> Result<i64, String> {
    field(v, key)?
        .as_i64()
        .filter(|n| n.unsigned_abs() <= 1_800_000_000)
        .ok_or_else(|| format!("{key} is not a coordinate in decimicrodegrees in {v}"))
}

fn distance(v: &Value) -> Result<f64, String> {
    field(v, "distance_m")?
        .as_f64()
        .filter(|d| d.is_finite() && *d >= 0.0)
        .ok_or_else(|| format!("distance_m is not a non-negative number in {v}"))
}

fn read_address(v: &Value) -> Result<(Address, f64), String> {
    let coord =
        |key| integer(v, key).and_then(|n| i32::try_from(n).map_err(|_| format!("{key} {n}")));
    Ok((
        Address {
            lat: coord("lat_e7")?,
            lon: coord("lon_e7")?,
            house_number: string(v, "house_number")?,
            street: optional(v, "street")?,
            place: optional(v, "place")?,
            postcode: optional(v, "postcode")?,
        },
        distance(v)?,
    ))
}

fn read_street(v: &Value) -> Result<(StreetAnswer, f64), String> {
    Ok((
        StreetAnswer {
            name: string(v, "name")?,
            snap_lat: integer(v, "snap_lat_e7")?,
            snap_lon: integer(v, "snap_lon_e7")?,
        },
        distance(v)?,
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn model(lines: &[&str]) -> Model {
        let mut m = Model::default();
        for line in lines {
            model_record(&mut m, &parse_record(line).unwrap()).unwrap();
        }
        m
    }

    #[test]
    fn the_documented_rules_select_addresses_and_streets() {
        let m = model(&[
            "n1 v1 Taddr:housenumber=1,addr:street=A%20%St x12.0 y55.0",
            "n2 v1 Taddr:housenumber=2,addr:street= x12.0 y55.0",
            "n3 v1 Taddr:housenumber=3,addr:place=P,addr:postcode= x12.0 y55.0",
            "n4 v1 T x12.0000001 y55.0000001",
            "w5 v1 Tbuilding=yes,addr:housenumber=5,addr:street=B \
             Nn1x12.0y55.0,n4x12.0000001y55.0000001,n1x12.0y55.0",
            "w6 v1 Thighway=residential,name=,addr:interpolation=all \
             Nn1x12.0y55.0,n4x12.0000001y55.0000001",
            "w7 v1 Thighway=footway,name=F Nn1x12.0y55.0,n4x12.0000001y55.0000001",
            "w8 v1 Thighway=primary Nn1x12.0y55.0,n4x12.0000001y55.0000001",
        ]);
        let numbers: Vec<&str> = m
            .addresses
            .iter()
            .map(|a| a.house_number.as_str())
            .collect();
        assert_eq!(numbers, ["1", "3", "5"]);
        assert_eq!(m.addresses[0].street.as_deref(), Some("A St"));
        assert_eq!(m.addresses[1].postcode, None);
        // The mean of 550000000, 550000001 and 550000000, truncated.
        assert_eq!(
            (m.addresses[2].lat, m.addresses[2].lon),
            (550_000_000, 120_000_000)
        );
        assert_eq!(m.names, [""]);
        assert_eq!(m.segments.len(), 1);
    }

    #[test]
    fn an_unlocated_node_of_a_modelled_way_is_outside_the_domain() {
        let mut m = Model::default();
        let r = parse_record("w1 v1 Thighway=residential,name=A Nn1x12.0y55.0,n2xy").unwrap();
        assert!(model_record(&mut m, &r).is_err());
        let r = parse_record("w2 v1 Thighway=path,name=A Nn1x12.0y55.0,n2").unwrap();
        assert!(model_record(&mut m, &r).is_ok());
        // Both ends are in the domain, but the reader takes the segment the
        // short way round the antimeridian, which the model does not.
        let r =
            parse_record("w3 v1 Thighway=residential,name=A Nn1x-100.0y0.0,n2x100.0y0.0").unwrap();
        assert!(model_record(&mut m, &r).is_err());
    }

    #[test]
    fn a_repeated_object_is_outside_the_domain() {
        let mut previous = None;
        for line in ["n42 v1 x1.0 y1.0", "w42 v1 Nn42", "w43 v1 Nn42"] {
            assert!(refuse_repeat(&mut previous, &parse_record(line).unwrap()).is_ok());
        }
        let r = parse_record("w43 v2 Nn42").unwrap();
        assert!(refuse_repeat(&mut previous, &r).is_err());
    }

    #[test]
    fn the_nearest_candidate_within_the_larger_radius_is_expected() {
        let m = model(&["w1 v1 Thighway=residential,name=A Nn1x12.0y55.0,n2x12.01y55.0"]);
        let Expect::Some(true, tied) = m.expect_street(55.0, 12.005) else {
            panic!("a street through the query is within the fine radius");
        };
        assert!(tied[0].0 < 1e-6, "{tied:?}");
        assert!((tied[0].1.snap_lon - 120_050_000).abs() <= 1, "{tied:?}");
        // About 556 m north: beyond the fine radius, within the coarse one.
        assert!(matches!(
            m.expect_street(55.005, 12.005),
            Expect::Some(false, _)
        ));
        assert!(matches!(m.expect_street(55.05, 12.005), Expect::None));
        assert!(matches!(m.expect_address(55.0, 12.005), Expect::None));
    }

    #[test]
    fn a_candidate_on_the_coarse_radius_is_not_judged() {
        let found = [(GEOCODE_COARSE_RADIUS_M, 0)];
        assert!(matches!(expectation(&found, |_| ()), Expect::Unjudged));
        let found = [(10.0, 0), (10.0 + TOL / 2.0, 1), (11.0, 2)];
        let Expect::Some(true, tied) = expectation(&found, |i| i) else {
            panic!("within the fine radius");
        };
        assert_eq!(tied.iter().map(|t| t.1).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn an_answer_must_be_one_tied_candidate_in_every_field() {
        let want = StreetAnswer {
            name: "A".into(),
            snap_lat: 10,
            snap_lon: 20,
        };
        let expect = Expect::Some(true, vec![(5.0, want)]);
        let answer = |name: &str, lon: i64, d: f64| serde_json::json!({"name": name, "snap_lat_e7": 10, "snap_lon_e7": lon, "distance_m": d});
        assert_eq!(judge(&expect, &answer("A", 21, 5.0)), Ok(Some(0)));
        assert!(judge(&expect, &answer("B", 20, 5.0)).is_err());
        assert!(judge(&expect, &answer("A", 30, 5.0)).is_err());
        assert!(judge(&expect, &answer("A", 20, 5.1)).is_err());
        assert!(judge(&expect, &Value::Null).is_err());
        assert!(judge(&expect, &answer("A", 20, -0.00005)).is_err());
        let partial = serde_json::json!({"name": "A", "snap_lat_e7": 10, "snap_lon_e7": 20});
        assert!(
            judge(&expect, &partial).is_err(),
            "a missing key is no answer"
        );
        assert!(judge::<StreetAnswer>(&Expect::None, &answer("A", 20, 5.0)).is_err());
        assert_eq!(
            judge::<StreetAnswer>(&Expect::None, &Value::Null),
            Ok(Some(2))
        );
    }
}
