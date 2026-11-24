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
//! any other role ignored.
//!
//! Withheld, as the builder withholds it, and judged as contributing no
//! area: a relation with an outer or inner member way absent from the
//! decoding, present with no nodes, or referring to a node the decoding
//! holds no record of, a relation listing one way in both roles, and a
//! relation with an assembled ring whose points all lie on one line. The
//! decoding is `tags-filter` without `-R`, which copies every member way
//! and every node of one, so absence there is absence in the input.
//!
//! Rings are assembled with no search, by node id: each member occurrence
//! is an edge between its first and last node. A connected group of edges
//! in which every endpoint is shared by exactly two edge ends forms exactly
//! one closed walk whatever order it is followed in, and that walk is the
//! ring; a group that is an
//! open path forms none. The builder assembles over every node, not only
//! endpoints, and refuses topology this walk cannot see, so a relation is
//! left unmodelled where the two could differ: a group of any other shape
//! (an endpoint shared by three or more member ends, a one-node way), a way
//! listed twice in one role, a node of a member at an interior position
//! that also occurs elsewhere among the role's members, a ring revisiting
//! a coordinate, or a ring of fewer than three coordinates. A member node whose record carries no location leaves the
//! relation's whole level unjudged, since its known coordinates do not
//! bound where the builder's rings run.
//!
//! Each outer ring is a polygon. An inner ring belongs to every outer ring
//! whose trace contains its trace: that is decided only where the two
//! traces stay at least one decimicrodegree apart (then one vertex decides,
//! and every vertex agrees). A hole nearer than that to an outer it may
//! belong to is left out of that outer, and its bounding box is not judged
//! at the level: ownership only decides answers inside the hole, and it
//! changes no ring and no stored area. A query is in a polygon when it
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
//! move), inside the bounding box of an unmodelled relation of that
//! level, which could hold any answer, or inside the box of an undecided
//! hole of that level.
//!
//! An unmodelled group is not localised the same way, though any ring the
//! builder makes of it lies inside the group's box: the builder withholds a
//! relation whole when it refuses any of its topology, so the relation's
//! other polygons may vanish and its whole box stays unjudged.

use std::collections::HashMap;

use super::{EARTH_RADIUS_M, Record, tag};

/// Levels the answer can carry, 2 to 11.
pub(super) const LEVELS: std::ops::RangeInclusive<u8> = 2..=11;

/// Clearance below which hole ownership is not decided, decimicrodegrees.
const OWNERSHIP_CLEARANCE_E7: f64 = 1.0;

/// The edge grid's cell, decimicrodegrees (about 5.5 km).
const EDGE_GRID_E7: i32 = 500_000;

type Pt = (i32, i32);

/// A located member node: its id and coordinates.
type Node = (i64, Pt);

/// One way record of the decoding.
pub(super) struct Way {
    /// Node ids, signed.
    pub(super) refs: Vec<i64>,
    /// osmium's location of each ref, `None` where it had none.
    pub(super) nodes: Vec<Option<Pt>>,
}

/// Every node record of the decoding: its location, `None` when the record
/// carries none.
pub(super) type NodeRecords = HashMap<i64, Option<Pt>>;

/// What one member way of a relation resolves to.
enum Resolved {
    /// Every node located.
    Located(Vec<Node>),
    /// Every node has a record, and some record has no location; the
    /// located coordinates.
    Locationless(Vec<Pt>),
}

/// A member way's nodes, or `None` when the builder withholds the relation
/// for it: no refs, or a ref with no node record.
fn resolve(way: &Way, records: &NodeRecords) -> Result<Option<Resolved>, String> {
    if way.refs.len() != way.nodes.len() {
        return Err("a way's refs and locations differ in count".to_owned());
    }
    if way.refs.is_empty() {
        return Ok(None);
    }
    let mut located = Vec::with_capacity(way.refs.len());
    let mut locationless = false;
    for (&id, &at) in way.refs.iter().zip(&way.nodes) {
        match at.or_else(|| records.get(&id).copied().flatten()) {
            Some(p) => located.push((id, p)),
            None if records.contains_key(&id) => locationless = true,
            None => return Ok(None),
        }
    }
    Ok(Some(if locationless {
        Resolved::Locationless(located.into_iter().map(|(_, p)| p).collect())
    } else {
        Resolved::Located(located)
    }))
}

/// Whether one role's members hold a shape the endpoint walk cannot judge
/// against the builder's node-id assembly: a way listed twice, or a node at
/// an interior position of a member that occurs anywhere else among them.
fn needs_full_topology(ids: &[i64], ways: &[Vec<Node>]) -> bool {
    let mut listed = std::collections::HashSet::new();
    if !ids.iter().all(|id| listed.insert(*id)) {
        return true;
    }
    let mut count: HashMap<i64, usize> = HashMap::new();
    for w in ways {
        for &(id, _) in w {
            *count.entry(id).or_default() += 1;
        }
    }
    ways.iter()
        .any(|w| w.len() > 2 && w[1..w.len() - 1].iter().any(|(id, _)| count[id] > 1))
}

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
    /// `(level, bbox)` not judged: each unmodelled relation's member
    /// coordinates, and each undecided hole.
    pub(super) unmodelled: Vec<(u8, (i32, i32, i32, i32))>,
    /// The id, level and why of each `unmodelled` box, for the run's account.
    pub(super) unmodelled_why: Vec<(String, u8, Unmodelled)>,
    pub(super) relations: usize,
    /// Selected relations the builder withholds whole, judged as no area.
    pub(super) withheld: usize,
    /// Every ring edge, `(polygon, a, b)`, and the grid over them.
    edges: Vec<(u32, Pt, Pt)>,
    edge_cells: HashMap<(i32, i32), Vec<u32>>,
}

/// Why a relation is not modelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Unmodelled {
    /// A member node's record carries no location; its whole level is
    /// unjudged.
    Unlocated,
    /// Its outer or inner ways hold a shape the endpoint walk cannot judge:
    /// an endpoint shared by three or more member ends, a one-node member,
    /// a way listed twice in a role, a shared interior node, or a ring
    /// revisiting a coordinate.
    Junction,
    /// A hole comes within the clearance of an outer it may belong to; the
    /// relation is modelled and only the hole's box is unjudged.
    HoleClearance,
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
fn rings(ways: &[&[Node]]) -> Option<Vec<Vec<Pt>>> {
    let mut ends: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, w) in ways.iter().enumerate() {
        if w.len() < 2 {
            return None;
        }
        ends.entry(w[0].0).or_default().push(i);
        ends.entry(w[w.len() - 1].0).or_default().push(i);
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
            for end in [w[0].0, w[w.len() - 1].0] {
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
            .flat_map(|&i| [ways[i][0].0, ways[i][ways[i].len() - 1].0])
            .map(|e| ends[&e].len())
            .collect();
        if degrees.iter().all(|&d| d == 2) {
            let ring = walk(ways, &group, &ends)?;
            // A coordinate visited twice, by distinct nodes, is a revisit
            // the builder may refuse or keep apart, and a closed walk of
            // fewer than three coordinates is one it refuses whole.
            let mut at = std::collections::HashSet::new();
            if ring.len() < 4 || !ring[..ring.len() - 1].iter().all(|p| at.insert(*p)) {
                return None;
            }
            out.push(ring);
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

/// Whether every point of `ring` lies on one line, exactly.
fn collinear(ring: &[Pt]) -> bool {
    let Some(&a) = ring.first() else {
        return true;
    };
    let Some(&b) = ring.iter().find(|&&p| p != a) else {
        return true;
    };
    let d = |p: Pt| {
        (
            i128::from(p.0) - i128::from(a.0),
            i128::from(p.1) - i128::from(a.1),
        )
    };
    let (d0, d1) = d(b);
    ring.iter().all(|&p| {
        let (e0, e1) = d(p);
        d0 * e1 == d1 * e0
    })
}

/// The closed walk of a group whose every endpoint has degree two.
fn walk(ways: &[&[Node]], group: &[usize], ends: &HashMap<i64, Vec<usize>>) -> Option<Vec<Pt>> {
    let mut used = std::collections::HashSet::new();
    let first = group[0];
    used.insert(first);
    let mut ring: Vec<Node> = ways[first].to_vec();
    while used.len() < group.len() {
        let end = ring[ring.len() - 1].0;
        let &next = ends[&end].iter().find(|j| !used.contains(*j))?;
        used.insert(next);
        let w = ways[next];
        if w[0].0 == end {
            ring.extend_from_slice(&w[1..]);
        } else {
            ring.extend(w[..w.len() - 1].iter().rev());
        }
    }
    Some(ring.into_iter().map(|(_, p)| p).collect())
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
        ways: &HashMap<i64, Way>,
        records: &NodeRecords,
    ) -> Result<Self, String> {
        let mut model = Self::default();
        for r in relations {
            let Some(b) = select(r) else {
                continue;
            };
            model.relations += 1;
            if b.outer.iter().any(|id| b.inner.contains(id)) {
                model.withheld += 1;
                continue;
            }
            let mut members: Vec<Vec<Pt>> = Vec::new();
            let mut located = true;
            let mut complete = true;
            let mut collect = |ids: &[i64], members: &mut Vec<Vec<Pt>>| -> Result<_, String> {
                let mut out = Vec::new();
                for id in ids {
                    let resolved = match ways.get(id) {
                        Some(way) => resolve(way, records).map_err(|e| format!("w{id}: {e}"))?,
                        None => None,
                    };
                    match resolved {
                        Some(Resolved::Located(nodes)) => {
                            members.push(nodes.iter().map(|&(_, p)| p).collect());
                            out.push(nodes);
                        }
                        Some(Resolved::Locationless(coords)) => {
                            located = false;
                            members.push(coords);
                        }
                        None => complete = false,
                    }
                }
                Ok(out)
            };
            let outer_ways = collect(&b.outer, &mut members)?;
            let inner_ways = collect(&b.inner, &mut members)?;
            if !complete {
                model.withheld += 1;
                continue;
            }
            if !located {
                // The known coordinates do not bound the builder's rings.
                let world = (i32::MIN, i32::MIN, i32::MAX, i32::MAX);
                model.unmodelled.push((b.level, world));
                model
                    .unmodelled_why
                    .push((r.id.clone(), b.level, Unmodelled::Unlocated));
                continue;
            }
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
            fn as_slices(v: &[Vec<Node>]) -> Vec<&[Node]> {
                v.iter().map(Vec::as_slice).collect()
            }
            let assembled = if needs_full_topology(&b.outer, &outer_ways)
                || needs_full_topology(&b.inner, &inner_ways)
            {
                None
            } else {
                rings(&as_slices(&outer_ways)).zip(rings(&as_slices(&inner_ways)))
            };
            let Some((outers, inners)) = assembled else {
                model.unmodelled.push((b.level, member_box));
                model
                    .unmodelled_why
                    .push((r.id.clone(), b.level, Unmodelled::Junction));
                continue;
            };
            // A ring whose points all lie on one line has no interior, and
            // the builder refuses its relation whole. No edge spans half a
            // turn here, so the raw ring is the reader's frame.
            if outers.iter().chain(&inners).any(|ring| collinear(ring)) {
                model.withheld += 1;
                continue;
            }
            model.add_polygons(&r.id, &b, outers, &inners);
        }
        model.index_edges()?;
        Ok(model)
    }

    /// One polygon per assembled outer ring of `b`, with its decided holes.
    fn add_polygons(&mut self, id: &str, b: &Boundary, outers: Vec<Vec<Pt>>, inners: &[Vec<Pt>]) {
        let mut undecided = vec![false; inners.len()];
        for outer in outers {
            let Some(bbox) = bbox_of(outer.iter()) else {
                continue;
            };
            let mut holes = Vec::new();
            for (h, hole) in inners.iter().enumerate() {
                match owned(hole, &outer, bbox) {
                    Some(true) => holes.push(hole.clone()),
                    Some(false) => {}
                    None => undecided[h] = true,
                }
            }
            let area = area_interval(&outer, bbox);
            self.polygons.push(Polygon {
                level: b.level,
                name: b.name.clone(),
                country_code: b.country_code.clone(),
                outer,
                holes,
                area,
                bbox,
            });
        }
        // A hole whose ownership by some outer is undecided is left out of
        // that outer; whichever way the builder decides, only a query inside
        // the hole's box can be answered differently, so only that box is
        // unjudged at the level. Ownership changes no ring and no stored
        // area, and every decided attachment stands.
        for (hole, _) in inners.iter().zip(&undecided).filter(|(_, u)| **u) {
            if let Some(hole_box) = bbox_of(hole.iter()) {
                self.unmodelled.push((b.level, hole_box));
                self.unmodelled_why
                    .push((id.to_owned(), b.level, Unmodelled::HoleClearance));
            }
        }
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

    /// Nodes `first..` over `pts`, a closing point taking the first id.
    fn ids(first: i64, pts: &[Pt]) -> Vec<Node> {
        let mut out: Vec<Node> = Vec::new();
        for (i, &p) in pts.iter().enumerate() {
            let closing = i > 0 && i + 1 == pts.len() && p == pts[0];
            #[allow(clippy::cast_possible_wrap)]
            let id = if closing { first } else { first + i as i64 };
            out.push((id, p));
        }
        out
    }

    fn way(nodes: &[Node]) -> Way {
        Way {
            refs: nodes.iter().map(|n| n.0).collect(),
            nodes: nodes.iter().map(|n| Some(n.1)).collect(),
        }
    }

    #[test]
    fn degree_two_groups_close_and_others_are_unmodelled() {
        let s = ids(1, &square(0, 0, 10));
        let (a, b) = (s[..3].to_vec(), s[2..].to_vec());
        let r = rings(&[&a[..], &b[..]]).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].len(), 5);
        // A reversed member joins as well.
        let rev: Vec<Node> = b.iter().rev().copied().collect();
        assert_eq!(rings(&[&a[..], &rev[..]]).unwrap()[0].len(), 5);
        // An open path forms nothing.
        assert!(rings(&[&a[..]]).unwrap().is_empty());
        // Three ends at one node: not modelled.
        let spur = [s[0], (99, (-5, -5))];
        assert!(rings(&[&a[..], &b[..], &spur[..]]).is_none());
        // A one-node way: not modelled.
        assert!(rings(&[&s[..], &[s[0]][..]]).is_none());
        // Distinct nodes at the first coordinate close nothing by id.
        let mut open = s.clone();
        open[4].0 = 50;
        assert!(rings(&[&open[..]]).unwrap().is_empty());
        // A ring revisiting a coordinate through distinct nodes: not modelled.
        let revisit = [
            (1, (0, 0)),
            (2, (0, 10)),
            (3, (0, 0)),
            (4, (10, 0)),
            (1, (0, 0)),
        ];
        assert!(rings(&[&revisit[..]]).is_none());
    }

    #[test]
    fn repeated_and_shared_interior_members_need_full_topology() {
        let s = ids(1, &square(0, 0, 10));
        assert!(needs_full_topology(&[7, 7], &[s.clone(), s.clone()]));
        assert!(!needs_full_topology(&[7], std::slice::from_ref(&s)));
        // A bridge leaving the triangle at its corner node 1: A B C A D.
        let bridged = vec![s[0], s[1], s[2], s[0], (9, (-5, -5))];
        assert!(needs_full_topology(&[7], &[bridged]));
    }

    fn relation(members: &str) -> Record {
        super::super::parse_record(&format!(
            "r1 v1 Tboundary=administrative,admin_level=2,name=C,ISO3166-1:alpha2=CC M{members}"
        ))
        .unwrap()
    }

    #[test]
    fn an_incomplete_relation_is_withheld_and_judged_as_no_area() {
        let shell = ids(1, &square(0, 0, 1_000_000));
        let mut ways = HashMap::new();
        ways.insert(1, way(&shell));
        let records = NodeRecords::new();
        // A missing inner way, an empty way and a missing node each withhold.
        let mut empty = HashMap::new();
        empty.insert(1, way(&shell));
        empty.insert(
            2,
            Way {
                refs: Vec::new(),
                nodes: Vec::new(),
            },
        );
        let mut unresolved = HashMap::new();
        let mut half = way(&shell);
        half.nodes[1] = None;
        unresolved.insert(1, half);
        for (members, ways) in [
            ("w1@outer,w2@inner", &ways),
            ("w1@outer,w2@outer", &empty),
            ("w1@outer", &unresolved),
            ("w1@outer,w1@inner", &ways),
        ] {
            let model = AdminModel::build(&[relation(members)], ways, &records).unwrap();
            assert_eq!((model.relations, model.withheld), (1, 1), "{members}");
            assert!(model.polygons.is_empty() && model.unmodelled.is_empty());
            assert!(matches!(
                model.expect(0.05, 0.05, 1.0).levels[2],
                Level::None { .. }
            ));
        }
        // A same-level relation that is complete still answers.
        let other = super::super::parse_record(
            "r2 v1 Tboundary=administrative,admin_level=2,name=D Mw1@outer",
        )
        .unwrap();
        let model =
            AdminModel::build(&[relation("w1@outer,w2@inner"), other], &ways, &records).unwrap();
        assert_eq!((model.relations, model.withheld), (2, 1));
        let Level::Some { candidates, .. } = model.expect(0.05, 0.05, 1.0).levels[2].clone() else {
            panic!("the complete relation answers");
        };
        assert_eq!(model.polygons[candidates[0]].name, "D");
    }

    #[test]
    fn a_late_node_resolves_and_a_locationless_one_unjudges_the_level() {
        let shell = ids(1, &square(0, 0, 1_000_000));
        let mut late = way(&shell);
        let apex = late.nodes[2].take().unwrap();
        let mut ways = HashMap::new();
        ways.insert(1, late);
        let mut records = NodeRecords::new();
        records.insert(3, Some(apex));
        let model = AdminModel::build(&[relation("w1@outer")], &ways, &records).unwrap();
        assert_eq!(model.polygons.len(), 1);
        records.insert(3, None);
        let model = AdminModel::build(&[relation("w1@outer")], &ways, &records).unwrap();
        assert!(model.polygons.is_empty());
        assert_eq!(model.unmodelled_why[0].2, Unmodelled::Unlocated);
        assert!(matches!(
            model.expect(50.0, 50.0, 1.0).levels[2],
            Level::Unjudged
        ));
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
    fn an_undecided_hole_leaves_only_its_box_unjudged() {
        let record = |line: &str| super::super::parse_record(line).unwrap();
        // An outer square of side 100 and a hole sharing its corner.
        let relation = record(
            "r1 v1 Tboundary=administrative,admin_level=2,name=C,ISO3166-1:alpha2=CC \
             Mw1@outer,w2@inner",
        );
        let mut ways = HashMap::new();
        ways.insert(1, way(&ids(1, &square(0, 0, 1_000_000))));
        ways.insert(2, way(&ids(11, &square(0, 0, 100_000))));
        let model = AdminModel::build(&[relation], &ways, &NodeRecords::new()).unwrap();
        assert_eq!(model.polygons.len(), 1);
        assert!(model.polygons[0].holes.is_empty());
        assert_eq!(model.unmodelled.len(), 1);
        assert_eq!(model.unmodelled_why[0].2, Unmodelled::HoleClearance);
        // Inside the hole's box: not judged. Elsewhere in the outer: judged.
        let at = |lat: f64, lon: f64| model.expect(lat, lon, 1.0).levels[2].clone();
        assert!(matches!(at(0.005, 0.005), Level::Unjudged));
        assert!(matches!(at(0.05, 0.05), Level::Some { .. }));
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
