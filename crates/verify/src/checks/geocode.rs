//! Verify: geocode - the address and street answers of `pbfhogg
//! geocode-query` over an index `pbfhogg build-geocode-index` built from
//! the input, against an exhaustive model of the documented query rules
//! built from osmium's decoding of the same input.
//!
//! The model reads `osmium tags-filter nw/addr:housenumber w/highway`
//! (which keeps the nodes of matched ways), then `osmium
//! add-locations-to-ways -f opl`, so every way carries its node
//! coordinates. From that it takes, by the rules docs/cli/commands.md
//! (build-geocode-index) states and the builder applies:
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
//! Interpolation ranges and admin areas are not modelled, so this check
//! claims nothing about them; the classes are separate minimums in the
//! reader, so leaving them out does not bend the two that are judged.
//!
//! Comparison domain, refused as a verifier error rather than judged: an
//! address or street way with a node osmium could not locate (the model
//! would have to guess how the builder shortens it), any modelled
//! coordinate within 10 degrees of the antimeridian or beyond 85 degrees of
//! latitude, and a street segment spanning half a turn of longitude or more
//! (the model does not unwrap longitude; the reader takes the shorter way). libosmium cuts a string
//! at a NUL byte where pbfhogg answers it whole, so a tag value holding one
//! is outside what this comparison can see; a FAIL naming such a value is
//! that limit, not a pbfhogg defect.
//!
//! Queries are drawn from a fixed seed: uniform points over the extent of
//! the modelled data, and witness points placed near modelled records,
//! aiming at a quota per class and distance category (within the fine
//! radius, between the radii, beyond both) until the draw's attempts run
//! out. The model alone decides what a point is before any pbfhogg answer
//! is read. The quota is a target; the enforced floor is one judged query
//! per class and category, and a category with none is a verifier error,
//! never a PASS that did not look.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use serde_json::Value;

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::{Harness, Verdict};
use crate::reference::ToolRun;
use crate::tunables::{
    GEOCODE_COARSE_RADIUS_M, GEOCODE_DISTANCE_TOLERANCE_M as TOL, GEOCODE_FINE_RADIUS_M,
    GEOCODE_GRID_E7, GEOCODE_SEED, GEOCODE_UNIFORM_QUERIES, GEOCODE_WITNESS_ATTEMPTS,
    GEOCODE_WITNESS_QUOTA, GEOCODE_WITNESS_REACH_M,
};
use crate::verify_diff_opl::{decode, parse_decimicro};

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
                // The query at the origin of a frame in which x is longitude
                // scaled by the query's cosine and y latitude, both radians.
                let ax = (rad(i64::from(s.a.1)) - qlon) * cos;
                let ay = rad(i64::from(s.a.0)) - qlat;
                let dx = (rad(i64::from(s.b.1)) - rad(i64::from(s.a.1))) * cos;
                let dy = rad(i64::from(s.b.0)) - rad(i64::from(s.a.0));
                let len_sq = dx * dx + dy * dy;
                let t = if len_sq == 0.0 {
                    0.0
                } else {
                    (-(ax * dx + ay * dy) / len_sq).clamp(0.0, 1.0)
                };
                let (px, py) = (ax + t * dx, ay + t * dy);
                let distance_m = (px * px + py * py).sqrt() * EARTH_RADIUS_M;
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
    id: String,
}

fn parse_record(line: &str) -> Result<Record, String> {
    let mut fields = line.split(' ');
    let first = fields.next().unwrap_or_default();
    let kind = *first.as_bytes().first().ok_or("empty record")?;
    let (mut tags, mut x, mut y, mut nodes) = (Vec::new(), None, None, Vec::new());
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
                    let location = token.split_once('x').and_then(|(_, at)| {
                        let (lon, lat) = at.split_once('y')?;
                        (!lon.is_empty() && !lat.is_empty()).then_some((lat, lon))
                    });
                    nodes.push(match location {
                        Some((lat, lon)) => Some((coordinate(lat)?, coordinate(lon)?)),
                        None => None,
                    });
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
        id: first.to_owned(),
    })
}

/// The model of the OPL file at `path`.
fn build_model(path: &Path) -> Result<Model, Error> {
    let file = std::fs::File::open(path)
        .map_err(|e| Error::Failed(format!("cannot open {}: {e}", path.display())))?;
    let mut model = Model::default();
    for (n, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|e| Error::Failed(format!("{}: {e}", path.display())))?;
        let at = |why: String| Error::Failed(format!("{} line {}: {why}", path.display(), n + 1));
        let record = parse_record(&line).map_err(at)?;
        model_record(&mut model, &record).map_err(Error::Tool)?;
    }
    Ok(model)
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
}

fn query(model: &Model, lat: f64, lon: f64) -> Query {
    let (text, lat, lon) = point(lat, lon);
    Query {
        address: model.expect_address(lat, lon),
        street: model.expect_street(lat, lon),
        text,
        lat,
        lon,
    }
}

/// The fixed query set: uniform points over the extent, then witnesses
/// until every class has its quota in each category.
fn queries(model: &Model) -> Result<Vec<Query>, Error> {
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
    let mut filled = [[0_usize; 3]; 2];
    for q in &out {
        tally(&mut filled, q);
    }
    let anchors = model.addresses.len() + model.segments.len();
    for _ in 0..GEOCODE_WITNESS_ATTEMPTS {
        if filled.iter().flatten().all(|&n| n >= GEOCODE_WITNESS_QUOTA) {
            break;
        }
        #[allow(clippy::cast_possible_truncation)]
        let pick = (draw.next() % anchors as u64) as usize;
        let (lat, lon) = match model.addresses.get(pick) {
            Some(a) => (a.lat, a.lon),
            None => model.segments[pick - model.addresses.len()].a,
        };
        let reach = draw.unit() * GEOCODE_WITNESS_REACH_M / EARTH_RADIUS_M;
        let bearing = draw.unit() * std::f64::consts::TAU;
        let lat = deg(lat) + (reach * bearing.cos()).to_degrees();
        let lon = deg(lon) + (reach * bearing.sin()).to_degrees() / lat.to_radians().cos();
        let q = query(model, lat, lon);
        let wanted = |class: usize, c: Option<usize>| {
            c.is_some_and(|c| filled[class][c] < GEOCODE_WITNESS_QUOTA)
        };
        if wanted(0, q.address.category()) || wanted(1, q.street.category()) {
            tally(&mut filled, &q);
            out.push(q);
        }
    }
    for (class, counts) in ["address", "street"].iter().zip(filled) {
        for (category, n) in ["within the fine radius", "between the radii", "beyond both"]
            .iter()
            .zip(counts)
        {
            if n == 0 {
                return Err(Error::tool(format!(
                    "no {class} query {category} was found in the input; the check cannot \
                     cover that path"
                )));
            }
        }
    }
    Ok(out)
}

fn tally(filled: &mut [[usize; 3]; 2], q: &Query) {
    if let Some(c) = q.address.category() {
        filled[0][c] += 1;
    }
    if let Some(c) = q.street.category() {
        filled[1][c] += 1;
    }
}

/// osmium's located OPL of the input's address and highway objects.
fn reference_opl(
    harness: &mut Harness,
    pbf: &Path,
    outdir: &Path,
) -> Result<std::path::PathBuf, Error> {
    let filtered = outdir.join("geocode-filtered.osm.pbf");
    let ended = harness.reference(
        &ToolRun::osmium("geocode-filter.1")
            .arg("tags-filter")
            .input(pbf)
            .args(&["nw/addr:housenumber", "w/highway", "-o"])
            .output(&filtered)
            .arg("--overwrite"),
        &outdir.join("osmium-filter.stdout"),
    )?;
    harness.require_success(&ended, "osmium tags-filter")?;
    let opl = outdir.join("geocode-located.opl");
    let ended = harness.reference(
        &ToolRun::osmium("geocode-locations.1")
            .args(&["add-locations-to-ways", "--ignore-missing-nodes"])
            .input(&filtered)
            .args(&["-f", "opl", "-o"])
            .output(&opl)
            .arg("--overwrite"),
        &outdir.join("osmium-locations.stdout"),
    )?;
    harness.require_success(&ended, "osmium add-locations-to-ways")?;
    Ok(opl)
}

pub fn run(harness: &mut Harness, pbf: &Path, direct_io: bool) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("geocode")?;
    harness.msg("=== verify geocode (address and street answers) ===");

    let index = outdir.join("index");
    let ended = harness.pbfhogg_discard(
        PbfArgv::new("build-geocode-index")
            .input(pbf)
            .output_flag("--output-dir", &index)
            .scratch()
            .arg("--search-radius")
            .arg(GEOCODE_FINE_RADIUS_M.to_string())
            .arg("--coarse-search-radius")
            .arg(GEOCODE_COARSE_RADIUS_M.to_string())
            .arg_if(direct_io, "--direct-io"),
    )?;
    harness.require_success(&ended, "pbfhogg build-geocode-index")?;

    let model = build_model(&reference_opl(harness, pbf, &outdir)?)?;
    harness.msg(&format!(
        "  model: {} address points, {} street segments",
        model.addresses.len(),
        model.segments.len()
    ));
    let queries = queries(&model)?;
    let points = outdir.join("points.txt");
    {
        let mut file = std::io::BufWriter::new(std::fs::File::create(&points)?);
        for q in &queries {
            writeln!(file, "{}", q.text)?;
        }
        file.flush()?;
    }
    harness.run.watch(&points)?;

    let answers = outdir.join("pbfhogg-answers.jsonl");
    let ended = harness.pbfhogg(
        PbfArgv::new("geocode-query").input(&index).input(&points),
        &answers,
    )?;
    harness.require_success(&ended, "pbfhogg geocode-query")?;
    let text = std::fs::read_to_string(&answers)
        .map_err(|e| Error::Failed(format!("pbfhogg answers are not readable text: {e}")))?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() != queries.len() {
        return Err(Error::Failed(format!(
            "pbfhogg geocode-query printed {} answers for {} queries",
            lines.len(),
            queries.len()
        )));
    }

    let mut judged = [[0_usize; 3]; 2];
    let mut mismatches: [Vec<String>; 2] = [Vec::new(), Vec::new()];
    for (q, line) in queries.iter().zip(&lines) {
        let answer: Value = serde_json::from_str(line)
            .map_err(|e| Error::Failed(format!("answer to {}: not JSON: {e}", q.text)))?;
        if answer["lat"].as_f64() != Some(q.lat) || answer["lon"].as_f64() != Some(q.lon) {
            return Err(Error::Failed(format!(
                "answer {line} does not echo its query {}",
                q.text
            )));
        }
        let class = |key: &str| {
            answer
                .get(key)
                .ok_or_else(|| format!("query {}: the answer has no {key} key", q.text))
        };
        let results = [
            class("address").and_then(|v| judge(&q.address, v)),
            class("street").and_then(|v| judge(&q.street, v)),
        ];
        for (class, result) in results.into_iter().enumerate() {
            match result {
                Ok(Some(c)) => judged[class][c] += 1,
                Ok(None) => {}
                Err(why) => mismatches[class].push(format!("query {}: {why}", q.text)),
            }
        }
    }

    for (class, name) in ["address", "street"].iter().enumerate() {
        let [fine, between, none] = judged[class];
        let wrong = &mismatches[class];
        harness.msg(&format!(
            "  {name}: {} judged ({fine} within the fine radius, {between} between the radii, \
             {none} beyond both), {} wrong",
            fine + between + none + wrong.len(),
            wrong.len()
        ));
        for why in wrong.iter().take(SHOWN_MISMATCHES) {
            harness.msg(&format!("    {why}"));
        }
        harness.record(
            &format!("geocode {name} answers vs the osmium-derived model"),
            Verdict::from_pass(wrong.is_empty()),
        );
    }
    Ok(Some(format!(
        "{} queries: every judged address and street answer is the nearest documented \
         candidate in the osmium decoding of the input, or none beyond the radii; \
         interpolation and admin answers are not judged",
        queries.len()
    )))
}

/// Whether an answer is this candidate: the same record, at the candidate's
/// distance within the tolerance.
trait Candidate: std::fmt::Debug + Sized {
    fn read(v: &Value) -> Result<(Self, f64), String>;
    fn is(&self, other: &Self) -> bool;
}

impl Candidate for Address {
    fn read(v: &Value) -> Result<(Self, f64), String> {
        read_address(v)
    }

    fn is(&self, other: &Self) -> bool {
        self == other
    }
}

impl Candidate for StreetAnswer {
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
