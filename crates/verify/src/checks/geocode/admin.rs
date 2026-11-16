//! The admin part of the geocode model: the areas the build-geocode-index
//! rules in docs/cli/commands.md make of the boundary relations in the
//! osmium decoding, and the answer a query gets at each level.
//!
//! A relation is a boundary when its `boundary` is `administrative` with an
//! `admin_level` that parses as an integer 2 to 10, or `postal_code` (level
//! 11, named by `postal_code`, else `name`); it needs a name (an empty one
//! counts); its country code (`ISO3166-1:alpha2`, empty meaning none) is
//! kept at level 2 only. The last occurrence of a key counts. Its way
//! members with role `outer` or an empty role are outer, `inner` inner,
//! any other role ignored; a member way absent from the decoding is
//! omitted.
//!
//! Rings are assembled with no search: each member occurrence is an edge
//! between its first and last node. A connected group of edges in which
//! every endpoint is shared by exactly two edge ends forms exactly one
//! closed walk whatever order it is followed in, and that walk is the ring
//! (kept when it has more than two coordinates); a group that is an open
//! path forms none. Any other group (an endpoint shared three or more ways,
//! a one-node way), or a member way with a node osmium could not locate,
//! leaves the relation unmodelled: the model does not know which rings the
//! builder's search picks.
//!
//! Each outer ring is a polygon. An inner ring belongs to every outer ring
//! whose trace contains its trace: that is decided only where the two
//! traces stay at least one decimicrodegree apart (then one vertex decides,
//! and every vertex agrees), and a hole nearer than that to an outer it may
//! belong to leaves the relation unmodelled. A query is in a polygon when it
//! is inside the outer ring and in none of its holes, by even-odd crossing.
//!
//! The answer at a level is a containing polygon of least stored area, the
//! outer ring's planar area in square decimicrodegrees stored as f32. The
//! model computes the exact area and the interval every f32 a correct
//! floating computation can store lies in; the answer must name one
//! containing polygon, carry an area in its interval, and carry no more
//! than any other containing polygon's largest possible area (the order
//! between equal stored areas is not public).
//!
//! Not judged at a level: a query within the band of any ring of a
//! polygon of that level (1 mm at tolerance 0, for float crossing tests;
//! the tolerance plus margin otherwise, where the contract lets the answer
//! move), or inside the bounding box of an unmodelled relation of that
//! level, which could hold any answer.

use std::collections::HashMap;

use super::{EARTH_RADIUS_M, Record, tag};

/// Levels the answer can carry, 2 to 11.
pub(super) const LEVELS: std::ops::RangeInclusive<u8> = 2..=11;

/// Clearance below which hole ownership is not decided, decimicrodegrees.
const OWNERSHIP_CLEARANCE_E7: f64 = 1.0;

/// The edge grid's cell, decimicrodegrees (about 5.5 km).
const EDGE_GRID_E7: i32 = 500_000;

type Pt = (i32, i32);

/// One polygon the model holds.
pub(super) struct Polygon {
    pub(super) level: u8,
    pub(super) name: String,
    pub(super) country_code: Option<String>,
    pub(super) outer: Vec<Pt>,
    pub(super) holes: Vec<Vec<Pt>>,
    /// Least and greatest stored area a correct computation can give.
    pub(super) area: (f64, f64),
    bbox: (i32, i32, i32, i32),
}

/// What the relations make.
#[derive(Default)]
pub(super) struct AdminModel {
    pub(super) polygons: Vec<Polygon>,
    /// `(level, bbox)` of each unmodelled relation's member coordinates.
    pub(super) unmodelled: Vec<(u8, (i32, i32, i32, i32))>,
    pub(super) relations: usize,
    /// Every ring edge, `(polygon, a, b)`, and the grid over them.
    edges: Vec<(u32, Pt, Pt)>,
    edge_cells: HashMap<(i32, i32), Vec<u32>>,
}

/// A boundary relation, as selected.
struct Boundary {
    level: u8,
    name: String,
    country_code: Option<String>,
    outer: Vec<i64>,
    inner: Vec<i64>,
}

fn select(r: &Record) -> Option<Boundary> {
    let boundary = tag(&r.tags, "boundary")?;
    let level = match boundary {
        "administrative" => {
            let l: u8 = tag(&r.tags, "admin_level")?.parse().ok()?;
            (2..=10).contains(&l).then_some(l)?
        }
        "postal_code" => 11,
        _ => return None,
    };
    let name = if level == 11 {
        tag(&r.tags, "postal_code").or_else(|| tag(&r.tags, "name"))
    } else {
        tag(&r.tags, "name")
    }?
    .to_owned();
    let country_code = if level == 2 {
        tag(&r.tags, "ISO3166-1:alpha2")
            .filter(|c| !c.is_empty())
            .map(str::to_owned)
    } else {
        None
    };
    let (mut outer, mut inner) = (Vec::new(), Vec::new());
    for (kind, id, role) in &r.members {
        if *kind != b'w' {
            continue;
        }
        match role.as_str() {
            "" | "outer" => outer.push(*id),
            "inner" => inner.push(*id),
            _ => {}
        }
    }
    Some(Boundary {
        level,
        name,
        country_code,
        outer,
        inner,
    })
}

fn bbox_of<'p>(points: impl Iterator<Item = &'p Pt>) -> Option<(i32, i32, i32, i32)> {
    points.fold(None, |b, &(lat, lon)| {
        Some(match b {
            None => (lat, lon, lat, lon),
            Some((a, b, c, d)) => (a.min(lat), b.min(lon), c.max(lat), d.max(lon)),
        })
    })
}

/// The rings of one role's member ways, or `None` when they are not
/// uniquely assembled.
fn rings(ways: &[&[Pt]]) -> Option<Vec<Vec<Pt>>> {
    let mut ends: HashMap<Pt, Vec<usize>> = HashMap::new();
    for (i, w) in ways.iter().enumerate() {
        if w.len() < 2 {
            return None;
        }
        ends.entry(w[0]).or_default().push(i);
        ends.entry(w[w.len() - 1]).or_default().push(i);
    }
    let mut seen = vec![false; ways.len()];
    let mut out = Vec::new();
    for start in 0..ways.len() {
        if seen[start] {
            continue;
        }
        // The connected group of `start`.
        let mut group = vec![start];
        seen[start] = true;
        let mut k = 0;
        while k < group.len() {
            let w = ways[group[k]];
            for end in [w[0], w[w.len() - 1]] {
                for &j in &ends[&end] {
                    if !seen[j] {
                        seen[j] = true;
                        group.push(j);
                    }
                }
            }
            k += 1;
        }
        let degrees: Vec<usize> = group
            .iter()
            .flat_map(|&i| [ways[i][0], ways[i][ways[i].len() - 1]])
            .map(|e| ends[&e].len())
            .collect();
        if degrees.iter().all(|&d| d == 2) {
            out.extend(walk(ways, &group, &ends));
        } else if degrees.iter().filter(|&&d| d == 1).count() == 2
            && degrees.iter().all(|&d| d <= 2)
        {
            // An open path: no ring.
        } else {
            return None;
        }
    }
    Some(out)
}

/// The closed walk of a group whose every endpoint has degree two.
fn walk(ways: &[&[Pt]], group: &[usize], ends: &HashMap<Pt, Vec<usize>>) -> Option<Vec<Pt>> {
    let mut used = std::collections::HashSet::new();
    let first = group[0];
    used.insert(first);
    let mut ring: Vec<Pt> = ways[first].to_vec();
    while used.len() < group.len() {
        let end = ring[ring.len() - 1];
        let &next = ends[&end].iter().find(|j| !used.contains(*j))?;
        used.insert(next);
        let w = ways[next];
        if w[0] == end {
            ring.extend_from_slice(&w[1..]);
        } else {
            ring.extend(w[..w.len() - 1].iter().rev());
        }
    }
    (ring.len() > 2).then_some(ring)
}

/// Even-odd containment of `(y, x)` in decimicrodegrees.
fn inside(ring: &[Pt], y: f64, x: f64) -> bool {
    let mut odd = false;
    let n = ring.len();
    let mut j = n - 1;
    for i in 0..n {
        let (yi, xi) = (f64::from(ring[i].0), f64::from(ring[i].1));
        let (yj, xj) = (f64::from(ring[j].0), f64::from(ring[j].1));
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            odd = !odd;
        }
        j = i;
    }
    odd
}

fn orient(a: Pt, b: Pt, c: Pt) -> i128 {
    let (ax, ay) = (i128::from(a.1), i128::from(a.0));
    let (bx, by) = (i128::from(b.1), i128::from(b.0));
    let (cx, cy) = (i128::from(c.1), i128::from(c.0));
    ((bx - ax) * (cy - ay) - (by - ay) * (cx - ax)).signum()
}

fn point_segment(p: Pt, a: Pt, b: Pt) -> f64 {
    let (px, py) = (f64::from(p.1), f64::from(p.0));
    let (ax, ay) = (f64::from(a.1), f64::from(a.0));
    let (dx, dy) = (f64::from(b.1) - ax, f64::from(b.0) - ay);
    let len = dx * dx + dy * dy;
    let t = if len == 0.0 {
        0.0
    } else {
        (((px - ax) * dx + (py - ay) * dy) / len).clamp(0.0, 1.0)
    };
    ((ax + t * dx - px).powi(2) + (ay + t * dy - py).powi(2)).sqrt()
}

/// The planar distance between two segments, decimicrodegrees: zero when
/// they meet (exactly).
fn segment_segment(a: Pt, b: Pt, c: Pt, d: Pt) -> f64 {
    let (o1, o2, o3, o4) = (
        orient(a, b, c),
        orient(a, b, d),
        orient(c, d, a),
        orient(c, d, b),
    );
    if o1 * o2 < 0 && o3 * o4 < 0 {
        return 0.0;
    }
    point_segment(a, c, d)
        .min(point_segment(b, c, d))
        .min(point_segment(c, a, b))
        .min(point_segment(d, a, b))
}

fn edge_cells(a: Pt, b: Pt) -> impl Iterator<Item = (i32, i32)> {
    let c = |v: i32| v.div_euclid(EDGE_GRID_E7);
    let (r0, r1) = (c(a.0.min(b.0)), c(a.0.max(b.0)));
    let (c0, c1) = (c(a.1.min(b.1)), c(a.1.max(b.1)));
    (r0..=r1).flat_map(move |r| (c0..=c1).map(move |k| (r, k)))
}

/// Whether `hole` is decided against `outer`: `Some(owned)`, or `None` when
/// the traces come nearer than the clearance.
fn owned(hole: &[Pt], outer: &[Pt], outer_box: (i32, i32, i32, i32)) -> Option<bool> {
    let hole_box = bbox_of(hole.iter())?;
    let margin = 2;
    if hole_box.0 > outer_box.2 + margin
        || hole_box.2 < outer_box.0 - margin
        || hole_box.1 > outer_box.3 + margin
        || hole_box.3 < outer_box.1 - margin
    {
        return Some(false);
    }
    let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    for i in 0..outer.len() {
        let (a, b) = (outer[i], outer[(i + 1) % outer.len()]);
        for key in edge_cells(a, b) {
            grid.entry(key).or_default().push(i);
        }
    }
    for i in 0..hole.len() {
        let (c, d) = (hole[i], hole[(i + 1) % hole.len()]);
        let mut checked = std::collections::HashSet::new();
        for (r, k) in edge_cells(c, d) {
            for dr in -1..=1 {
                for dk in -1..=1 {
                    for &e in grid.get(&(r + dr, k + dk)).into_iter().flatten() {
                        if !checked.insert(e) {
                            continue;
                        }
                        let (a, b) = (outer[e], outer[(e + 1) % outer.len()]);
                        if segment_segment(a, b, c, d) < OWNERSHIP_CLEARANCE_E7 {
                            return None;
                        }
                    }
                }
            }
        }
    }
    Some(inside(outer, f64::from(hole[0].0), f64::from(hole[0].1)))
}

/// `|doubled area|` exactly.
fn doubled_area(ring: &[Pt]) -> u128 {
    let n = ring.len();
    let mut sum: i128 = 0;
    for i in 0..n {
        let (a, b) = (ring[i], ring[(i + 1) % n]);
        sum += i128::from(a.1) * i128::from(b.0) - i128::from(b.1) * i128::from(a.0);
    }
    sum.unsigned_abs()
}

/// Every f32 a floating shoelace over `ring`, with any vertex as origin,
/// can store: the exact area widened by the accumulated rounding bound
/// `n * W * H * gamma(2n + 1)` (each of the 2n products is at most `W * H`
/// in magnitude), then by one f32 step either side for the cast.
fn area_interval(ring: &[Pt], bbox: (i32, i32, i32, i32)) -> (f64, f64) {
    #[allow(clippy::cast_precision_loss)]
    let exact = doubled_area(ring) as f64 / 2.0;
    #[allow(clippy::cast_precision_loss)]
    let n = ring.len() as f64;
    let u = f64::EPSILON / 2.0;
    let k = 2.0 * n + 1.0;
    let gamma = k * u / (1.0 - k * u);
    let w = f64::from(bbox.3) - f64::from(bbox.1);
    let h = f64::from(bbox.2) - f64::from(bbox.0);
    // The bound, the exact area's own rounding to f64, and outward slack.
    let bound = (n * w * h * gamma + exact * 4.0 * u) * 1.000_001 + 1.0;
    let (lo, hi) = ((exact - bound).max(0.0), exact + bound);
    #[allow(clippy::cast_possible_truncation)]
    let (lo32, hi32) = (lo as f32, hi as f32);
    (
        f64::from(lo32.next_down().max(0.0)),
        f64::from(hi32.next_up()),
    )
}

impl AdminModel {
    /// The model of the decoded relations and their member ways.
    pub(super) fn build(
        relations: &[Record],
        ways: &HashMap<i64, Vec<Option<Pt>>>,
    ) -> Result<Self, String> {
        let mut model = Self::default();
        for r in relations {
            let Some(b) = select(r) else {
                continue;
            };
            model.relations += 1;
            let mut members: Vec<Vec<Pt>> = Vec::new();
            let mut located = true;
            let collect = |ids: &[i64], members: &mut Vec<Vec<Pt>>, located: &mut bool| {
                let mut out = Vec::new();
                for id in ids {
                    let Some(nodes) = ways.get(id) else {
                        continue;
                    };
                    match nodes.iter().copied().collect::<Option<Vec<Pt>>>() {
                        Some(coords) if !coords.is_empty() => {
                            members.push(coords.clone());
                            out.push(coords);
                        }
                        Some(_) => {}
                        None => {
                            *located = false;
                            members.push(nodes.iter().flatten().copied().collect());
                        }
                    }
                }
                out
            };
            let outer_ways = collect(&b.outer, &mut members, &mut located);
            let inner_ways = collect(&b.inner, &mut members, &mut located);
            let Some(member_box) = bbox_of(members.iter().flatten()) else {
                continue;
            };
            for p in members.iter().flatten() {
                if p.0.abs() > 850_000_000 || p.1.abs() > 1_700_000_000 {
                    return Err(format!(
                        "{}: boundary coordinate {p:?} is outside the modelled domain",
                        r.id
                    ));
                }
            }
            // The reader unwraps a ring with an edge spanning half a turn of
            // longitude; the model does not. Ring edges are member edges
            // (rings close through shared endpoints), so checking members
            // covers every ring, and the box of an unmodelled relation too.
            for w in &members {
                if let Some(pair) = w
                    .windows(2)
                    .find(|p| (i64::from(p[1].1) - i64::from(p[0].1)).abs() >= 1_800_000_000)
                {
                    return Err(format!(
                        "{}: a boundary edge from {:?} to {:?} spans half a turn of longitude, \
                         outside the modelled domain",
                        r.id, pair[0], pair[1]
                    ));
                }
            }
            fn as_slices(v: &[Vec<Pt>]) -> Vec<&[Pt]> {
                v.iter().map(Vec::as_slice).collect()
            }
            let assembled = if located {
                rings(&as_slices(&outer_ways)).zip(rings(&as_slices(&inner_ways)))
            } else {
                None
            };
            let Some((outers, inners)) = assembled else {
                model.unmodelled.push((b.level, member_box));
                continue;
            };
            let mut polygons = Vec::new();
            let mut decided = true;
            for outer in outers {
                let Some(bbox) = bbox_of(outer.iter()) else {
                    continue;
                };
                let mut holes = Vec::new();
                for hole in &inners {
                    match owned(hole, &outer, bbox) {
                        Some(true) => holes.push(hole.clone()),
                        Some(false) => {}
                        None => decided = false,
                    }
                }
                let area = area_interval(&outer, bbox);
                polygons.push(Polygon {
                    level: b.level,
                    name: b.name.clone(),
                    country_code: b.country_code.clone(),
                    outer,
                    holes,
                    area,
                    bbox,
                });
            }
            if !decided {
                model.unmodelled.push((b.level, member_box));
                continue;
            }
            model.polygons.extend(polygons);
        }
        model.index_edges()?;
        Ok(model)
    }

    fn index_edges(&mut self) -> Result<(), String> {
        for (p, polygon) in self.polygons.iter().enumerate() {
            let p = u32::try_from(p).map_err(|_| "too many polygons")?;
            for ring in std::iter::once(&polygon.outer).chain(&polygon.holes) {
                for i in 0..ring.len() {
                    let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
                    let n = u32::try_from(self.edges.len()).map_err(|_| "too many edges")?;
                    for key in edge_cells(a, b) {
                        self.edge_cells.entry(key).or_default().push(n);
                    }
                    self.edges.push((p, a, b));
                }
            }
        }
        Ok(())
    }

    /// The polygons with an edge within `band_m` of the query.
    fn near_edges(&self, lat: f64, lon: f64, band_m: f64) -> std::collections::HashSet<u32> {
        let (y, x) = (lat * 1e7, lon * 1e7);
        let cos = lat.to_radians().cos();
        let to_m = 1e-7_f64.to_radians() * EARTH_RADIUS_M;
        #[allow(clippy::cast_possible_truncation)]
        let (r, c) = (
            (y.floor() as i32).div_euclid(EDGE_GRID_E7),
            (x.floor() as i32).div_euclid(EDGE_GRID_E7),
        );
        let mut near = std::collections::HashSet::new();
        for dr in -1..=1 {
            for dc in -1..=1 {
                for &e in self.edge_cells.get(&(r + dr, c + dc)).into_iter().flatten() {
                    let (p, a, b) = self.edges[e as usize];
                    if near.contains(&p) {
                        continue;
                    }
                    // The flat frame at the query: longitude scaled by its
                    // cosine.
                    let (ax, ay) = ((f64::from(a.1) - x) * cos, f64::from(a.0) - y);
                    let (bx, by) = ((f64::from(b.1) - x) * cos, f64::from(b.0) - y);
                    let (dx, dy) = (bx - ax, by - ay);
                    let len = dx * dx + dy * dy;
                    let t = if len == 0.0 {
                        0.0
                    } else {
                        (-(ax * dx + ay * dy) / len).clamp(0.0, 1.0)
                    };
                    let d = ((ax + t * dx).powi(2) + (ay + t * dy).powi(2)).sqrt() * to_m;
                    if d <= band_m {
                        near.insert(p);
                    }
                }
            }
        }
        near
    }

    /// The model's expectation at every level for one query and band.
    pub(super) fn expect(&self, lat: f64, lon: f64, band_m: f64) -> AdminExpect {
        let near = self.near_edges(lat, lon, band_m);
        let (y, x) = (lat * 1e7, lon * 1e7);
        // The band and a unit, in decimicrodegrees of each axis.
        let band_lat = (band_m / EARTH_RADIUS_M).to_degrees() * 1e7 + 2.0;
        let band_lon = band_lat / lat.to_radians().cos();
        let in_box = |b: (i32, i32, i32, i32)| {
            y >= f64::from(b.0) - band_lat
                && y <= f64::from(b.2) + band_lat
                && x >= f64::from(b.1) - band_lon
                && x <= f64::from(b.3) + band_lon
        };
        // Per level: unjudged, boxed, a hole excluded the query, containing.
        let mut unjudged = [false; 12];
        let mut boxed = [false; 12];
        let mut in_hole = [false; 12];
        let mut containing: [Vec<usize>; 12] = Default::default();
        for &(level, b) in &self.unmodelled {
            if in_box(b) {
                unjudged[usize::from(level)] = true;
            }
        }
        for (p, polygon) in self.polygons.iter().enumerate() {
            let level = usize::from(polygon.level);
            if unjudged[level] || !in_box(polygon.bbox) {
                continue;
            }
            boxed[level] = true;
            #[allow(clippy::cast_possible_truncation)]
            if near.contains(&(p as u32)) {
                unjudged[level] = true;
                continue;
            }
            if !inside(&polygon.outer, y, x) {
                continue;
            }
            if polygon.holes.iter().any(|h| inside(h, y, x)) {
                in_hole[level] = true;
                continue;
            }
            containing[level].push(p);
        }
        let mut out = AdminExpect::default();
        for level in LEVELS {
            let l = usize::from(level);
            out.levels[l] = if unjudged[l] {
                Level::Unjudged
            } else if containing[l].is_empty() {
                Level::None {
                    boxed: boxed[l],
                    in_hole: in_hole[l],
                }
            } else {
                Level::Some {
                    candidates: std::mem::take(&mut containing[l]),
                    in_hole: in_hole[l],
                }
            };
        }
        out
    }
}

/// The expectation at one level.
#[derive(Debug, Clone, Default)]
pub(super) enum Level {
    /// Within a band or an unmodelled relation's box.
    #[default]
    Unjudged,
    /// No containing polygon; `boxed` when some polygon of the level boxes
    /// the query, `in_hole` when a hole of one excludes it.
    None { boxed: bool, in_hole: bool },
    /// The containing polygons, by index.
    Some {
        candidates: Vec<usize>,
        in_hole: bool,
    },
}

/// The expectation at every level (indexed by level, 0 and 1 unused).
#[derive(Debug, Default)]
pub(super) struct AdminExpect {
    pub(super) levels: [Level; 12],
}

/// What was judged, per path.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct AdminTally {
    pub(super) containing: usize,
    pub(super) empty_boxed: usize,
    pub(super) in_owned_hole: usize,
    pub(super) several: usize,
    pub(super) postal: usize,
    pub(super) country_code: usize,
    pub(super) unjudged: usize,
}

/// One answered admin entry.
struct Entry {
    name: String,
    country_code: Option<String>,
    area: f64,
}

fn read_entries(answer: &serde_json::Value) -> Result<HashMap<u8, Entry>, String> {
    let list = answer
        .as_array()
        .ok_or_else(|| format!("admin is {answer}, not an array"))?;
    let mut out = HashMap::new();
    let mut previous = 0_u8;
    for v in list {
        let level = super::field(v, "admin_level")?
            .as_u64()
            .and_then(|l| u8::try_from(l).ok())
            .ok_or_else(|| format!("admin_level is not a level in {v}"))?;
        if level <= previous {
            return Err(format!("admin levels are not strictly ascending at {v}"));
        }
        previous = level;
        let area = super::field(v, "area")?
            .as_f64()
            .filter(|a| a.is_finite() && *a >= 0.0)
            .ok_or_else(|| format!("area is not a non-negative number in {v}"))?;
        out.insert(
            level,
            Entry {
                name: super::string(v, "name")?,
                country_code: super::optional(v, "country_code")?,
                area,
            },
        );
    }
    Ok(out)
}

impl AdminModel {
    /// Judge one query's admin array, tallying what was judged.
    pub(super) fn judge(
        &self,
        expect: &AdminExpect,
        answer: &serde_json::Value,
        tally: &mut AdminTally,
    ) -> Result<(), String> {
        let entries = read_entries(answer)?;
        for &level in entries.keys() {
            if !LEVELS.contains(&level) {
                return Err(format!("answered level {level}, which no boundary has"));
            }
        }
        for level in LEVELS {
            let got = entries.get(&level);
            match (&expect.levels[usize::from(level)], got) {
                (Level::Unjudged, _) => tally.unjudged += 1,
                (Level::None { boxed, in_hole }, None) => {
                    if *boxed {
                        tally.empty_boxed += 1;
                    }
                    if *in_hole {
                        tally.in_owned_hole += 1;
                    }
                }
                (Level::None { .. }, Some(e)) => {
                    return Err(format!(
                        "level {level}: answered {:?} ({}), expected no area",
                        e.name, e.area
                    ));
                }
                (Level::Some { candidates, .. }, None) => {
                    return Err(format!(
                        "level {level}: answered nothing, expected {:?}",
                        self.polygons[candidates[0]].name
                    ));
                }
                (
                    Level::Some {
                        candidates,
                        in_hole,
                    },
                    Some(e),
                ) => {
                    let fits = |&i: &usize| {
                        let p = &self.polygons[i];
                        p.name == e.name
                            && p.country_code == e.country_code
                            && e.area >= p.area.0
                            && e.area <= p.area.1
                            && candidates
                                .iter()
                                .all(|&j| j == i || e.area <= self.polygons[j].area.1)
                    };
                    let Some(&i) = candidates.iter().find(|i| fits(i)) else {
                        let want: Vec<(&str, (f64, f64))> = candidates
                            .iter()
                            .map(|&i| (self.polygons[i].name.as_str(), self.polygons[i].area))
                            .collect();
                        return Err(format!(
                            "level {level}: answered {:?} {:?} area {}, expected the least of {want:?}",
                            e.name, e.country_code, e.area
                        ));
                    };
                    tally.containing += 1;
                    if candidates.len() > 1 {
                        tally.several += 1;
                    }
                    if *in_hole {
                        tally.in_owned_hole += 1;
                    }
                    if level == 11 {
                        tally.postal += 1;
                    }
                    if self.polygons[i].country_code.is_some() {
                        tally.country_code += 1;
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn square(lat: i32, lon: i32, side: i32) -> Vec<Pt> {
        vec![
            (lat, lon),
            (lat, lon + side),
            (lat + side, lon + side),
            (lat + side, lon),
            (lat, lon),
        ]
    }

    #[test]
    fn degree_two_groups_close_and_others_are_unmodelled() {
        let s = square(0, 0, 10);
        let (a, b) = (s[..3].to_vec(), s[2..].to_vec());
        let r = rings(&[&a[..], &b[..]]).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].len(), 5);
        // A reversed member joins as well.
        let rev: Vec<Pt> = b.iter().rev().copied().collect();
        assert_eq!(rings(&[&a[..], &rev[..]]).unwrap()[0].len(), 5);
        // An open path forms nothing.
        assert!(rings(&[&a[..]]).unwrap().is_empty());
        // Three ends at one point: not modelled.
        let spur = [s[0], (-5, -5)];
        assert!(rings(&[&a[..], &b[..], &spur[..]]).is_none());
        // A one-node way: not modelled.
        assert!(rings(&[&s[..], &[s[0]][..]]).is_none());
    }

    #[test]
    fn hole_ownership_needs_clearance() {
        let outer = square(0, 0, 100);
        let b = bbox_of(outer.iter()).unwrap();
        assert_eq!(owned(&square(10, 10, 10), &outer, b), Some(true));
        assert_eq!(owned(&square(200, 200, 10), &outer, b), Some(false));
        // Sharing the outer's corner: undecided.
        assert_eq!(owned(&square(0, 0, 10), &outer, b), None);
    }

    #[test]
    fn the_area_interval_holds_the_exact_area() {
        let ring = square(550_000_000, 120_000_000, 1_000_000);
        let b = bbox_of(ring.iter()).unwrap();
        let (lo, hi) = area_interval(&ring, b);
        let exact = 1e12;
        assert!(lo <= exact && exact <= hi, "{lo} {hi}");
        #[allow(clippy::cast_possible_truncation)]
        let stored = f64::from(exact as f32);
        assert!(lo <= stored && stored <= hi);
    }
}
