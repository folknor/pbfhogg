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
//! relation whose topology the builder refuses (below). The
//! decoding is `tags-filter` without `-R`, which copies every member way
//! and every node of one, so absence there is absence in the input.
//!
//! Rings are assembled by the builder's node-id rules, found by a different
//! algorithm (`role_rings`): bridges are dropped, a walk touching itself at
//! a node splits into its lobes, and a block that is neither a bridge nor a
//! simple cycle, a ring revisiting a coordinate outside one run, a ring on
//! one line, or two rings of one graph not shown disjoint refuse the
//! relation, which is then withheld like an incomplete one. A way listed
//! twice in one role counts once. The one verdict left unmodelled is
//! two rings of one graph whose edge midpoints disagree on disjointness:
//! the builder decides by the first such midpoint of its walk, so member
//! order picks the answer. A member node whose record carries no location
//! leaves the relation's whole level unjudged, since its known coordinates
//! do not bound where the builder's rings run.
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
//! An unmodelled relation is not localised to the undecided pair: the
//! builder withholds a relation whole when it refuses any of its topology,
//! so every ring of the relation may vanish, and each ring's box stays
//! unjudged at its level.
//!
//! The antimeridian, as docs/cli/commands.md states it. Every ring is
//! judged in the frame containment tests it in: as given when no edge
//! (the closing one included) spans more than half a turn of longitude,
//! otherwise with its longitudes unwrapped from its first vertex, where a
//! query is tried at its own longitude and a turn either way. A relation
//! with a ring that winds round a pole, whose unwrapped longitudes span a
//! full turn, or with an edge of exactly half a turn is withheld: its
//! answer would depend on where its walk starts. In the unwrapped frame
//! +180 and -180 are one point, for the run and line rules as for
//! containment. Where a rule reads a point's spelling (the disjointness
//! midpoint, folded to the query range, against a ring that does not
//! unwrap), a verdict that depends on which spelling the builder's walk
//! produces is undecided. Bands, boxes and distances are measured over
//! every image of the query a turn apart.
//!
//! Same-level competition is aimed at, not only met by chance: after the
//! model is built, sites where two areas of one level may overlap are
//! sought (proper edge crossings, vertices strictly inside the other ring,
//! points of the intersection of their boxes, over every whole-turn
//! alignment), and witnesses near them are kept where the model finds two
//! containing areas of a level with one certified larger. The search finds
//! what it finds; it claims no overlap is missed.

use std::collections::HashMap;

use super::{Draw, EARTH_RADIUS_M, Record, lon, tag};
use crate::tunables::{
    GEOCODE_ADMIN_TOLERANCE_M, GEOCODE_ADMIN_WITNESS_REACH_M, GEOCODE_OVERLAP_ATTEMPTS,
    GEOCODE_OVERLAP_WITNESSES,
};

/// Levels the answer can carry, 2 to 11.
pub(super) const LEVELS: std::ops::RangeInclusive<u8> = 2..=11;

/// The band left unjudged round a boundary of the non-zero tolerance build.
pub(super) const TOLERANCE_BAND_M: f64 = GEOCODE_ADMIN_TOLERANCE_M * 1.01 + 1.0;

/// Clearance below which hole ownership is not decided, decimicrodegrees.
const OWNERSHIP_CLEARANCE_E7: f64 = 1.0;

/// The edge grid's cell, decimicrodegrees (about 5.5 km).
const EDGE_GRID_E7: i64 = 500_000;

/// Edge grid columns per turn of longitude.
const EDGE_COLUMNS: i64 = lon::TURN_E7 / EDGE_GRID_E7;

const TURN: i64 = lon::TURN_E7;
const HALF: i64 = lon::HALF_E7;

/// A node coordinate as decoded, `(lat, lon)` decimicrodegrees.
type Pt = (i32, i32);

/// A ring point in its ring's frame, `(lat, lon)` decimicrodegrees; an
/// unwrapped longitude can leave the `i32` range.
type Fp = (i64, i64);

/// `(min_lat, min_lon, max_lat, max_lon)` in some frame.
type Bbox = (i64, i64, i64, i64);

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

/// One polygon the model holds.
pub(super) struct Polygon {
    pub(super) level: u8,
    pub(super) name: String,
    pub(super) country_code: Option<String>,
    pub(super) outer: Ring,
    pub(super) holes: Vec<Ring>,
    /// Least and greatest stored area a correct computation can give.
    pub(super) area: (f64, f64),
}

/// One assembled ring, closed, in the frame containment tests it in.
#[derive(Debug, Clone)]
pub(super) struct Ring {
    pub(super) pts: Vec<Fp>,
    /// Unwrapped: a point is tried at its longitude and a turn either way.
    wraps: bool,
    bbox: Bbox,
    /// The builder's answer for it does not depend on where its walk starts.
    invariant: bool,
}

impl Ring {
    /// The frame of the closed raw ring `raw`.
    fn new(raw: &[Pt]) -> Self {
        let wraps = raw
            .windows(2)
            .any(|w| (i64::from(w[1].1) - i64::from(w[0].1)).abs() > HALF);
        let mut pts: Vec<Fp> = Vec::with_capacity(raw.len());
        let mut previous: Option<i64> = None;
        for &(lat, lon) in raw {
            let mut x = i64::from(lon);
            if wraps && let Some(p) = previous {
                while x - p > HALF {
                    x -= TURN;
                }
                while x - p < -HALF {
                    x += TURN;
                }
            }
            pts.push((i64::from(lat), x));
            previous = Some(x);
        }
        let bbox = bbox_of(pts.iter()).unwrap_or((0, 0, 0, 0));
        Self {
            pts,
            wraps,
            bbox,
            invariant: start_invariant(raw),
        }
    }

    /// The longitude shifts a point is tried at.
    fn images(&self) -> &'static [i64] {
        if self.wraps { &[0, TURN, -TURN] } else { &[0] }
    }

    /// The query predicate: even-odd containment of `(y, x)` (a query's own
    /// longitude, decimicrodegrees) at each image.
    fn contains(&self, y: f64, x: f64) -> bool {
        #[allow(clippy::cast_precision_loss)]
        self.images()
            .iter()
            .any(|&k| inside(&self.pts, y, x + k as f64))
    }
}

/// Whether the closed raw ring's frame answers the same whichever vertex
/// its walk starts at, as the documented withholding rule reads: no edge
/// spans half a turn, or else, lifting each edge the shorter way, none is
/// exactly half a turn, the lift closes, and it spans less than a turn.
fn start_invariant(raw: &[Pt]) -> bool {
    let d = |w: &[Pt]| i64::from(w[1].1) - i64::from(w[0].1);
    if raw.windows(2).all(|w| d(w).abs() < HALF) {
        return true;
    }
    let (mut at, mut lo, mut hi) = (0_i64, 0_i64, 0_i64);
    for w in raw.windows(2) {
        let step = match d(w) {
            s if s > HALF => s - TURN,
            s if s < -HALF => s + TURN,
            s => s,
        };
        if step.abs() == HALF {
            return false;
        }
        at += step;
        lo = lo.min(at);
        hi = hi.max(at);
    }
    at == 0 && hi - lo < TURN
}

/// What the relations make.
#[derive(Default)]
pub(super) struct AdminModel {
    pub(super) polygons: Vec<Polygon>,
    /// `(level, bbox)` not judged, each box in its ring's frame: every ring
    /// of an unmodelled relation, and each undecided hole.
    pub(super) unmodelled: Vec<(u8, Bbox)>,
    /// The id, level and why of each `unmodelled` box, for the run's account.
    pub(super) unmodelled_why: Vec<(String, u8, Unmodelled)>,
    pub(super) relations: usize,
    /// Selected relations the builder withholds whole, judged as no area.
    pub(super) withheld: usize,
    /// Every ring edge, `(polygon, a, b)` in its ring's frame, and the grid
    /// over them, columns periodic.
    edges: Vec<(u32, Fp, Fp)>,
    edge_cells: HashMap<(i64, i64), Vec<u32>>,
}

/// What the same-level overlap search found.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct OverlapAccount {
    /// Pairs of polygons of one level whose boxes meet in some alignment.
    pub(super) pairs: usize,
    pub(super) seeds: usize,
    /// Witnesses kept.
    pub(super) kept: usize,
}

/// Why a relation is not modelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Unmodelled {
    /// A member node's record carries no location; its whole level is
    /// unjudged.
    Unlocated,
    /// Two rings of one connected member graph whose edge midpoints
    /// disagree on whether one lies outside the other: the builder decides
    /// by the first of them its walk reaches, which member order can move,
    /// so it may publish the relation or withhold it.
    UndecidedLobes,
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

fn select(r: &Record<'_>) -> Option<Boundary> {
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
        // A way listed twice in one role is one boundary and counts once,
        // at its first occurrence, as the builder counts it.
        let list: &mut Vec<i64> = match role.as_ref() {
            "" | "outer" => &mut outer,
            "inner" => &mut inner,
            _ => continue,
        };
        if !list.contains(id) {
            list.push(*id);
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

fn bbox_of<'p>(points: impl Iterator<Item = &'p Fp>) -> Option<Bbox> {
    points.fold(None, |b, &(lat, lon)| {
        Some(match b {
            None => (lat, lon, lat, lon),
            Some((a, b, c, d)) => (a.min(lat), b.min(lon), c.max(lat), d.max(lon)),
        })
    })
}

/// What one role's member ways assemble to under the builder's rules.
#[derive(Debug)]
enum RoleRings {
    /// The builder refuses the role, whatever order its members come in.
    Refused,
    /// The rings, and whether the builder's verdict on two rings of one
    /// graph follows member order (see [`Unmodelled::UndecidedLobes`]), so
    /// it may refuse the role instead of returning them.
    Rings { rings: Vec<Ring>, undecided: bool },
}

impl RoleRings {
    /// Combine two independent graphs' outcomes: a refusal of either
    /// refuses the role in every order, then an undecided one leaves it
    /// undecided.
    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Refused, _) | (_, Self::Refused) => Self::Refused,
            (
                Self::Rings {
                    rings: mut a,
                    undecided: x,
                },
                Self::Rings {
                    rings: b,
                    undecided: y,
                },
            ) => {
                a.extend(b);
                Self::Rings {
                    rings: a,
                    undecided: x || y,
                }
            }
        }
    }
}

/// The rings the builder's node-id assembly makes of one role's member
/// ways (each way once, first occurrence first), stated independently of
/// its implementation.
///
/// The builder's rules (docs/cli/commands.md, and the module documentation
/// of its ring assembly): members join by node id; each contributes the
/// edges between consecutive distinct ids; a closed member (first id equal
/// to last) is a graph of its own, all open members share one. Every
/// biconnected block of a graph must be a single edge (a bridge, dropped)
/// or a simple cycle (a ring); any other block is refused. A ring may
/// visit one coordinate through several nodes only in one cyclic run, and
/// must not lie on one line. Two rings of one connected graph must each be
/// shown outside the other by an edge midpoint, of an edge with neither
/// node on the other ring; the builder takes the first such edge of its
/// walk.
///
/// Blocks are found here by chain decomposition (Schmidt), not by the
/// builder's low-link search: over a DFS forest, each non-tree edge taken
/// from its ancestor end in preorder, then up tree edges to the first
/// vertex an earlier chain or this one marked. Every block is a bridge or
/// a cycle exactly when every chain returns to the vertex it left; the
/// cycles are then the chains, and the bridges the uncovered tree edges.
fn role_rings(ways: &[Vec<Node>]) -> RoleRings {
    let (closed, open): (Vec<&[Node]>, Vec<&[Node]>) = ways
        .iter()
        .map(Vec::as_slice)
        .partition(|w| w.len() > 1 && w[0].0 == w[w.len() - 1].0);
    let mut out = graph_rings(&open);
    for w in closed {
        out = out.and(graph_rings(&[w]));
    }
    out
}

/// The cycles of the graph `adjacency` (per vertex: neighbour, edge), each
/// with its DFS component, by chain decomposition; `None` when a chain does
/// not return to the vertex it left, so some block is neither a bridge nor
/// a simple cycle.
fn cycles(adjacency: &[Vec<(usize, usize)>]) -> Option<Vec<(usize, Vec<usize>)>> {
    let n = adjacency.len();
    // DFS forest: preorder, tree parent (vertex and edge), component.
    let mut disc = vec![usize::MAX; n];
    let mut parent = vec![(usize::MAX, usize::MAX); n];
    let mut component = vec![0_usize; n];
    let mut preorder = Vec::with_capacity(n);
    for root in 0..n {
        if disc[root] != usize::MAX {
            continue;
        }
        let mut stack = vec![(root, usize::MAX, usize::MAX)];
        while let Some((v, from, edge)) = stack.pop() {
            if disc[v] != usize::MAX {
                continue;
            }
            disc[v] = preorder.len();
            preorder.push(v);
            parent[v] = (from, edge);
            component[v] = root;
            for &(w, e) in adjacency[v].iter().rev() {
                if disc[w] == usize::MAX {
                    stack.push((w, v, e));
                }
            }
        }
    }
    let tree = |v: usize, e: usize| parent[v].1 == e;
    // Chains.
    let mut marked = vec![false; n];
    let mut rings: Vec<(usize, Vec<usize>)> = Vec::new();
    for &u in &preorder {
        for &(w, e) in &adjacency[u] {
            // Each non-tree edge once, from its ancestor end.
            if tree(w, e) || tree(u, e) || disc[w] < disc[u] {
                continue;
            }
            marked[u] = true;
            let mut cycle = vec![u];
            let mut x = w;
            while !marked[x] {
                marked[x] = true;
                cycle.push(x);
                x = parent[x].0;
            }
            if x != u {
                return None;
            }
            rings.push((component[u], cycle));
        }
    }
    Some(rings)
}

/// One graph's rings: see [`role_rings`].
fn graph_rings(members: &[&[Node]]) -> RoleRings {
    let mut index: HashMap<i64, usize> = HashMap::new();
    let mut ids: Vec<i64> = Vec::new();
    let mut coords: Vec<Pt> = Vec::new();
    let mut adjacency: Vec<Vec<(usize, usize)>> = Vec::new();
    let mut edges = 0_usize;
    for w in members {
        let mut previous: Option<usize> = None;
        for &(id, p) in *w {
            let v = *index.entry(id).or_insert_with(|| {
                ids.push(id);
                coords.push(p);
                adjacency.push(Vec::new());
                ids.len() - 1
            });
            if let Some(u) = previous
                && u != v
            {
                adjacency[u].push((v, edges));
                adjacency[v].push((u, edges));
                edges += 1;
            }
            previous = Some(v);
        }
    }
    let Some(rings) = cycles(&adjacency) else {
        return RoleRings::Refused;
    };
    let mut walked: Vec<(usize, Vec<i64>, Ring)> = Vec::with_capacity(rings.len());
    for (c, cycle) in rings {
        let mut closed: Vec<Pt> = cycle.iter().map(|&v| coords[v]).collect();
        closed.push(closed[0]);
        // The run and line rules read the ring in its containment frame,
        // where an unwrapped ring has +180 and -180 as one point.
        let ring = Ring::new(&closed);
        if !one_run_per_point(&ring.pts[..ring.pts.len() - 1]) || collinear(&ring.pts) {
            return RoleRings::Refused;
        }
        walked.push((c, cycle.iter().map(|&v| ids[v]).collect(), ring));
    }
    let mut undecided = false;
    for (i, a) in walked.iter().enumerate() {
        for b in &walked[i + 1..] {
            if a.0 != b.0 {
                continue;
            }
            for (x, y) in [(a, b), (b, a)] {
                match outside_by_midpoints(&x.1, &x.2, &y.1, &y.2) {
                    Some(true) => {}
                    Some(false) => return RoleRings::Refused,
                    None => undecided = true,
                }
            }
        }
    }
    RoleRings::Rings {
        rings: walked.into_iter().map(|(_, _, ring)| ring).collect(),
        undecided,
    }
}

/// Whether the open cyclic walk `ring` visits each point in one run.
fn one_run_per_point(ring: &[Fp]) -> bool {
    let n = ring.len();
    let Some(start) = (0..n).find(|&i| ring[i] != ring[(i + n - 1) % n]) else {
        // One point throughout: collinear, refused by the caller.
        return true;
    };
    let mut seen = std::collections::HashSet::new();
    let mut previous = None;
    for k in 0..n {
        let p = ring[(start + k) % n];
        if previous == Some(p) {
            continue;
        }
        if !seen.insert(p) {
            return false;
        }
        previous = Some(p);
    }
    true
}

/// Whether ring `a` is outside ring `b` by every eligible witness: each
/// edge of `a` with neither node on `b`, its midpoint classified exactly.
/// `Some(true)` when every witness is strictly outside, `Some(false)` when
/// none is (or there is none), so the builder's first witness gives that
/// answer whichever edge it is; `None` when they disagree.
///
/// Each midpoint is taken in `a`'s frame and folded back to the longitude
/// range a query carries, then tried against `b` at every image `b`'s frame
/// tries. A midpoint folding onto the antimeridian keeps whichever spelling
/// the walk's lift gave it, and a ring that does not unwrap tells the two
/// apart, so there both spellings are tried, and a witness they disagree on
/// is undecided: the builder's walk may start where either is produced.
fn outside_by_midpoints(a_ids: &[i64], a: &Ring, b_ids: &[i64], b: &Ring) -> Option<bool> {
    let on_b: std::collections::HashSet<i64> = b_ids.iter().copied().collect();
    let bb = b.bbox;
    let strictly_out = |y2: i64, x2: i64| {
        b.images().iter().all(|&k| {
            let x2 = x2 + 2 * k;
            y2 < 2 * bb.0
                || y2 > 2 * bb.2
                || x2 < 2 * bb.1
                || x2 > 2 * bb.3
                || side_doubled(y2, x2, &b.pts) == Some(false)
        })
    };
    let n = a_ids.len();
    let (mut out, mut not_out) = (false, false);
    for i in 0..n {
        if on_b.contains(&a_ids[i]) || on_b.contains(&a_ids[(i + 1) % n]) {
            continue;
        }
        let (p, q) = (a.pts[i], a.pts[i + 1]);
        let (y2, mut x2) = (p.0 + q.0, p.1 + q.1);
        while x2 > 2 * HALF {
            x2 -= 2 * TURN;
        }
        while x2 < -2 * HALF {
            x2 += 2 * TURN;
        }
        let verdict = strictly_out(y2, x2);
        if !b.wraps && x2.abs() == 2 * HALF && strictly_out(y2, -x2) != verdict {
            return None;
        }
        if verdict {
            out = true;
        } else {
            not_out = true;
        }
        if out && not_out {
            return None;
        }
    }
    Some(out)
}

/// Exact even-odd side of the doubled point `(y2, x2)` against the closed
/// ring `ring`: `Some(true)` strictly inside, `Some(false)` strictly
/// outside, `None` on the boundary.
fn side_doubled(y2: i64, x2: i64, ring: &[Fp]) -> Option<bool> {
    let mut odd = false;
    for e in ring.windows(2) {
        let (ay, ax) = (2 * e[0].0, 2 * e[0].1);
        let (by, bx) = (2 * e[1].0, 2 * e[1].1);
        let cross =
            i128::from(bx - ax) * i128::from(y2 - ay) - i128::from(by - ay) * i128::from(x2 - ax);
        if cross == 0
            && x2 >= ax.min(bx)
            && x2 <= ax.max(bx)
            && y2 >= ay.min(by)
            && y2 <= ay.max(by)
        {
            return None;
        }
        if (ay > y2) != (by > y2) {
            // Left of the edge's crossing at y2, sign-corrected for its
            // direction.
            let left = (cross > 0) == (by > ay);
            if left {
                odd = !odd;
            }
        }
    }
    Some(odd)
}

/// Whether every point of `ring` lies on one line, exactly.
fn collinear(ring: &[Fp]) -> bool {
    let Some(&a) = ring.first() else {
        return true;
    };
    let Some(&b) = ring.iter().find(|&&p| p != a) else {
        return true;
    };
    let d = |p: Fp| {
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

/// Even-odd containment of `(y, x)` in decimicrodegrees.
#[allow(clippy::cast_precision_loss)]
fn inside(ring: &[Fp], y: f64, x: f64) -> bool {
    let mut odd = false;
    let n = ring.len();
    let mut j = n - 1;
    for i in 0..n {
        let (yi, xi) = (ring[i].0 as f64, ring[i].1 as f64);
        let (yj, xj) = (ring[j].0 as f64, ring[j].1 as f64);
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            odd = !odd;
        }
        j = i;
    }
    odd
}

fn orient(a: Fp, b: Fp, c: Fp) -> i128 {
    let (ax, ay) = (i128::from(a.1), i128::from(a.0));
    let (bx, by) = (i128::from(b.1), i128::from(b.0));
    let (cx, cy) = (i128::from(c.1), i128::from(c.0));
    ((bx - ax) * (cy - ay) - (by - ay) * (cx - ax)).signum()
}

#[allow(clippy::cast_precision_loss)]
fn point_segment(p: Fp, a: Fp, b: Fp) -> f64 {
    let (px, py) = (p.1 as f64, p.0 as f64);
    let (ax, ay) = (a.1 as f64, a.0 as f64);
    let (dx, dy) = (b.1 as f64 - ax, b.0 as f64 - ay);
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
fn segment_segment(a: Fp, b: Fp, c: Fp, d: Fp) -> f64 {
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

/// The grid cells, in the edge's own frame, an edge's box touches.
fn frame_cells(a: Fp, b: Fp) -> impl Iterator<Item = (i64, i64)> {
    let c = |v: i64| v.div_euclid(EDGE_GRID_E7);
    let (r0, r1) = (c(a.0.min(b.0)), c(a.0.max(b.0)));
    let (c0, c1) = (c(a.1.min(b.1)), c(a.1.max(b.1)));
    (r0..=r1).flat_map(move |r| (c0..=c1).map(move |k| (r, k)))
}

/// An edge whose box covers more cells than this is listed under
/// [`EVERYWHERE`], which every band search reads, so one long edge cannot
/// fill the grid.
const LONG_EDGE_CELLS: i64 = 4096;

const EVERYWHERE: (i64, i64) = (i64::MIN, 0);

/// The periodic grid cells an edge's box touches, each once.
fn edge_cells(a: Fp, b: Fp) -> Vec<(i64, i64)> {
    let c = |v: i64| v.div_euclid(EDGE_GRID_E7);
    let rows = c(a.0.max(b.0)) - c(a.0.min(b.0)) + 1;
    let width = (c(a.1.max(b.1)) - c(a.1.min(b.1)) + 1).min(EDGE_COLUMNS);
    if rows * width > LONG_EDGE_CELLS {
        return vec![EVERYWHERE];
    }
    let cols = lon::columns(c(a.1.min(b.1)), c(a.1.max(b.1)), EDGE_COLUMNS);
    (c(a.0.min(b.0))..=c(a.0.max(b.0)))
        .flat_map(|r| cols.iter().map(move |&k| (r, k)))
        .collect()
}

/// Whether two boxes meet once `b` is shifted `k` in longitude, with
/// `margin` of slack.
fn boxes_meet(a: Bbox, b: Bbox, k: i64, margin: i64) -> bool {
    !(b.0 > a.2 + margin || b.2 < a.0 - margin || b.1 + k > a.3 + margin || b.3 + k < a.1 - margin)
}

/// Whole-turn shifts of a ring in some frame. Every frame lies within a
/// turn and a half of zero, so two turns either way reach every alignment.
const ALIGNMENTS: [i64; 5] = [0, TURN, -TURN, 2 * TURN, -2 * TURN];

/// Whether `hole` is decided against `outer`: `Some(owned)`, or `None` when
/// the traces come nearer than the clearance in some alignment of the two
/// frames. Decided, one vertex of the hole, folded to the query range with
/// the antimeridian spelled +180, is tried as the builder tries a witness:
/// at every image when the outer unwraps, else at its own longitude and, on
/// the antimeridian, its other spelling.
fn owned(hole: &Ring, outer: &Ring) -> Option<bool> {
    let margin = 2;
    let shifts: Vec<i64> = ALIGNMENTS
        .into_iter()
        .filter(|&k| boxes_meet(outer.bbox, hole.bbox, k, margin))
        .collect();
    if shifts.is_empty() {
        return Some(false);
    }
    let ring = &outer.pts;
    let mut grid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    for i in 0..ring.len() - 1 {
        for key in frame_cells(ring[i], ring[i + 1]) {
            grid.entry(key).or_default().push(i);
        }
    }
    for k in shifts {
        let shift = |p: Fp| (p.0, p.1 + k);
        for i in 0..hole.pts.len() - 1 {
            let (c, d) = (shift(hole.pts[i]), shift(hole.pts[i + 1]));
            let mut checked = std::collections::HashSet::new();
            for (r, col) in frame_cells(c, d) {
                for dr in -1..=1 {
                    for dk in -1..=1 {
                        for &e in grid.get(&(r + dr, col + dk)).into_iter().flatten() {
                            if checked.insert(e)
                                && segment_segment(ring[e], ring[e + 1], c, d)
                                    < OWNERSHIP_CLEARANCE_E7
                            {
                                return None;
                            }
                        }
                    }
                }
            }
        }
    }
    let (y, x) = (hole.pts[0].0, lon::fold_e7(hole.pts[0].1));
    let xs: Vec<i64> = if outer.wraps {
        vec![x, x + TURN, x - TURN]
    } else if x == HALF {
        vec![x, -HALF]
    } else {
        vec![x]
    };
    Some(
        xs.into_iter()
            .any(|x| side_doubled(2 * y, 2 * x, ring) == Some(true)),
    )
}

/// `|doubled area|` exactly.
fn doubled_area(ring: &[Fp]) -> u128 {
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
///
/// An unwrapped ring is measured from degree coordinates, converted,
/// lifted by turns and taken about its first vertex before the products, so
/// each local coordinate carries an error of at most `DELTA_E7`; each of the
/// 2n products then errs by up to `delta * (W + H) + delta^2` more, the
/// products are bounded by the perturbed `(W + 2 delta) * (H + 2 delta)`, and
/// the rescale from square degrees adds a relative rounding.
fn area_interval(ring: &Ring) -> (f64, f64) {
    /// A generous bound on one local degree coordinate's rounding, in
    /// decimicrodegrees: under ten roundings of a value below 540 degrees.
    const DELTA_E7: f64 = 540.0 * 1e7 * 10.0 * f64::EPSILON;
    let bbox = ring.bbox;
    #[allow(clippy::cast_precision_loss)]
    let exact = doubled_area(&ring.pts) as f64 / 2.0;
    #[allow(clippy::cast_precision_loss)]
    let n = ring.pts.len() as f64;
    let u = f64::EPSILON / 2.0;
    let k = 2.0 * n + 1.0;
    let gamma = k * u / (1.0 - k * u);
    #[allow(clippy::cast_precision_loss)]
    let (w, h) = ((bbox.3 - bbox.1) as f64, (bbox.2 - bbox.0) as f64);
    let delta = if ring.wraps { DELTA_E7 } else { 0.0 };
    let products = (w + 2.0 * delta) * (h + 2.0 * delta);
    let coordinates = 2.0 * n * (delta * (w + h) + delta * delta);
    // The bound, the exact area's own rounding to f64 and the rescale's, and
    // outward slack.
    let bound = (n * products * gamma + coordinates + exact * 6.0 * u) * 1.000_001 + 1.0;
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
        relations: &[Record<'_>],
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
                let world = (i64::MIN / 4, i64::MIN / 4, i64::MAX / 4, i64::MAX / 4);
                model.unmodelled.push((b.level, world));
                model
                    .unmodelled_why
                    .push((r.id.to_string(), b.level, Unmodelled::Unlocated));
                continue;
            }
            model.assemble(&r.id, &b, &outer_ways, &inner_ways);
        }
        model.index_edges()?;
        Ok(model)
    }

    /// The builder's order: the outer role assembles, contributes nothing
    /// when it has no ring, and only then the inner role does; a refusal of
    /// either withholds the relation, and so does any ring of either role
    /// whose answer depends on where its walk starts, whatever an undecided
    /// pair would have made of the rest. Only then does an undecided pair
    /// leave every ring's box unjudged.
    fn assemble(&mut self, id: &str, b: &Boundary, outer: &[Vec<Node>], inner: &[Vec<Node>]) {
        let RoleRings::Rings {
            rings: outers,
            undecided: outer_undecided,
        } = role_rings(outer)
        else {
            self.withheld += 1;
            return;
        };
        if outers.is_empty() {
            return;
        }
        let RoleRings::Rings {
            rings: inners,
            undecided: inner_undecided,
        } = role_rings(inner)
        else {
            self.withheld += 1;
            return;
        };
        if !outers.iter().chain(&inners).all(|r| r.invariant) {
            self.withheld += 1;
            return;
        }
        if outer_undecided || inner_undecided {
            for ring in outers.iter().chain(&inners) {
                self.unmodelled.push((b.level, ring.bbox));
                self.unmodelled_why
                    .push((id.to_owned(), b.level, Unmodelled::UndecidedLobes));
            }
            return;
        }
        self.add_polygons(id, b, outers, &inners);
    }

    /// One polygon per assembled outer ring of `b`, with its decided holes.
    fn add_polygons(&mut self, id: &str, b: &Boundary, outers: Vec<Ring>, inners: &[Ring]) {
        let mut undecided = vec![false; inners.len()];
        for outer in outers {
            let mut holes = Vec::new();
            for (h, hole) in inners.iter().enumerate() {
                match owned(hole, &outer) {
                    Some(true) => holes.push(hole.clone()),
                    Some(false) => {}
                    None => undecided[h] = true,
                }
            }
            let area = area_interval(&outer);
            self.polygons.push(Polygon {
                level: b.level,
                name: b.name.clone(),
                country_code: b.country_code.clone(),
                outer,
                holes,
                area,
            });
        }
        // A hole whose ownership by some outer is undecided is left out of
        // that outer; whichever way the builder decides, only a query inside
        // the hole's box can be answered differently, so only that box is
        // unjudged at the level. Ownership changes no ring and no stored
        // area, and every decided attachment stands.
        for (hole, _) in inners.iter().zip(&undecided).filter(|(_, u)| **u) {
            self.unmodelled.push((b.level, hole.bbox));
            self.unmodelled_why
                .push((id.to_owned(), b.level, Unmodelled::HoleClearance));
        }
    }

    fn index_edges(&mut self) -> Result<(), String> {
        for (p, polygon) in self.polygons.iter().enumerate() {
            let p = u32::try_from(p).map_err(|_| "too many polygons")?;
            for ring in std::iter::once(&polygon.outer).chain(&polygon.holes) {
                for w in ring.pts.windows(2) {
                    let (a, b) = (w[0], w[1]);
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

    /// The polygons with an edge within `band_m` of the query, measured
    /// over the query's images a turn apart.
    pub(super) fn near_edges(
        &self,
        lat: f64,
        lon: f64,
        band_m: f64,
    ) -> std::collections::HashSet<u32> {
        #[allow(clippy::cast_precision_loss)]
        let (turn, half) = (TURN as f64, HALF as f64);
        let (y, x) = (lat * 1e7, lon * 1e7);
        let cos = lat.to_radians().cos();
        let to_m = 1e-7_f64.to_radians() * EARTH_RADIUS_M;
        let band_lat = (band_m / EARTH_RADIUS_M).to_degrees() * 1e7;
        let band_lon = if cos > 0.0 {
            band_lat / cos
        } else {
            f64::INFINITY
        };
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let index = |v: f64| (v / EDGE_GRID_E7 as f64).floor() as i64;
        let rows = index(y - band_lat) - 1..=index(y + band_lat) + 1;
        let cols = if band_lon >= half {
            (0..EDGE_COLUMNS).collect()
        } else {
            lon::columns(
                index(x - band_lon) - 1,
                index(x + band_lon) + 1,
                EDGE_COLUMNS,
            )
        };
        let mut near = std::collections::HashSet::new();
        let keys = rows
            .flat_map(|r| cols.iter().map(move |&c| (r, c)))
            .chain(std::iter::once(EVERYWHERE));
        {
            for key in keys {
                for &e in self.edge_cells.get(&key).into_iter().flatten() {
                    let (p, a, b) = self.edges[e as usize];
                    if near.contains(&p) {
                        continue;
                    }
                    #[allow(clippy::cast_precision_loss)]
                    let (a, b) = ((a.0 as f64, a.1 as f64), (b.0 as f64, b.1 as f64));
                    // The query's image nearest `a`, and one a turn either way.
                    let near_x = a.1 + ((x - a.1 + half).rem_euclid(turn) - half);
                    let d = [near_x, near_x + turn, near_x - turn]
                        .into_iter()
                        .map(|qx| {
                            // The flat frame at the query: longitude scaled
                            // by its cosine.
                            let (ax, ay) = ((a.1 - qx) * cos, a.0 - y);
                            let (bx, by) = ((b.1 - qx) * cos, b.0 - y);
                            let (dx, dy) = (bx - ax, by - ay);
                            let len = dx * dx + dy * dy;
                            let t = if len == 0.0 {
                                0.0
                            } else {
                                (-(ax * dx + ay * dy) / len).clamp(0.0, 1.0)
                            };
                            ((ax + t * dx).powi(2) + (ay + t * dy).powi(2)).sqrt() * to_m
                        })
                        .fold(f64::INFINITY, f64::min);
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
        let cos = lat.to_radians().cos();
        let band_lon = if cos > 0.0 {
            band_lat / cos
        } else {
            f64::INFINITY
        };
        #[allow(clippy::cast_precision_loss)]
        let in_box = |b: Bbox| {
            y >= b.0 as f64 - band_lat
                && y <= b.2 as f64 + band_lat
                && ALIGNMENTS.iter().any(|&k| {
                    let x = x + k as f64;
                    x >= b.1 as f64 - band_lon && x <= b.3 as f64 + band_lon
                })
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
            if unjudged[level] || !in_box(polygon.outer.bbox) {
                continue;
            }
            boxed[level] = true;
            #[allow(clippy::cast_possible_truncation)]
            if near.contains(&(p as u32)) {
                unjudged[level] = true;
                continue;
            }
            if !polygon.outer.contains(y, x) {
                continue;
            }
            if polygon.holes.iter().any(|h| h.contains(y, x)) {
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

/// A frame point back in query coordinates, folded, as `(lat, lon)`
/// decimicrodegrees.
#[allow(clippy::cast_possible_truncation)]
fn folded(p: Fp) -> Pt {
    (p.0 as i32, lon::fold_e7(p.1) as i32)
}

/// The proper crossing of segments `a`-`b` and `c`-`d`, if they cross.
#[allow(clippy::cast_precision_loss)]
fn crossing(a: Fp, b: Fp, c: Fp, d: Fp) -> Option<(f64, f64)> {
    if !(orient(a, b, c) * orient(a, b, d) < 0 && orient(c, d, a) * orient(c, d, b) < 0) {
        return None;
    }
    let f = |p: Fp| (p.0 as f64, p.1 as f64);
    let (a, b, c, d) = (f(a), f(b), f(c), f(d));
    let den = (b.1 - a.1) * (d.0 - c.0) - (b.0 - a.0) * (d.1 - c.1);
    let t = ((c.1 - a.1) * (d.0 - c.0) - (c.0 - a.0) * (d.1 - c.1)) / den;
    Some((a.0 + t * (b.0 - a.0), a.1 + t * (b.1 - a.1)))
}

impl AdminModel {
    /// The vertices admin witnesses are drawn near: every hole vertex and
    /// every 64th of an outer's (at least one each), folded.
    pub(super) fn witness_vertices(&self) -> Vec<Pt> {
        self.polygons
            .iter()
            .flat_map(|p| {
                p.holes
                    .iter()
                    .flat_map(|h| h.pts.iter())
                    .chain(p.outer.pts.iter().step_by((p.outer.pts.len() / 64).max(1)))
            })
            .map(|&p| folded(p))
            .collect()
    }

    /// Seed sites where two polygons of one level may overlap, in decimal
    /// degrees, and how many pairs were looked at.
    fn overlap_seeds(&self) -> (Vec<(f64, f64)>, usize) {
        #[allow(clippy::cast_precision_loss)]
        let deg = |lat: f64, lon: f64| (lat * 1e-7, lon::fold_deg(lon * 1e-7));
        let mut seeds = Vec::new();
        let mut pairs = std::collections::HashSet::new();
        for level in LEVELS {
            let mut of: Vec<usize> = (0..self.polygons.len())
                .filter(|&i| self.polygons[i].level == level)
                .collect();
            of.sort_by_key(|&i| self.polygons[i].outer.bbox.0);
            for (n, &i) in of.iter().enumerate() {
                let a = &self.polygons[i].outer;
                for &j in &of[n + 1..] {
                    let b = &self.polygons[j].outer;
                    if b.bbox.0 > a.bbox.2 {
                        break;
                    }
                    for k in ALIGNMENTS {
                        if !boxes_meet(a.bbox, b.bbox, k, 0) {
                            continue;
                        }
                        pairs.insert((i, j));
                        let shift = |p: Fp| (p.0, p.1 + k);
                        // Points of the boxes' intersection inside both
                        // outers: these find overlaps along shared lines,
                        // where nothing crosses and no vertex is inside.
                        let b_pts: Vec<Fp> = b.pts.iter().map(|&p| shift(p)).collect();
                        let lo = (a.bbox.0.max(b.bbox.0), a.bbox.1.max(b.bbox.1 + k));
                        let hi = (a.bbox.2.min(b.bbox.2), a.bbox.3.min(b.bbox.3 + k));
                        #[allow(clippy::cast_precision_loss)]
                        for (u, v) in [
                            (0.5, 0.5),
                            (0.25, 0.25),
                            (0.25, 0.75),
                            (0.75, 0.25),
                            (0.75, 0.75),
                        ] {
                            let (y, x) = (
                                lo.0 as f64 + u * (hi.0 - lo.0) as f64,
                                lo.1 as f64 + v * (hi.1 - lo.1) as f64,
                            );
                            if inside(&a.pts, y, x) && inside(&b_pts, y, x) {
                                seeds.push(deg(y, x));
                            }
                        }
                        // A vertex of either strictly inside the other: a
                        // few spread over each ring, those inside the
                        // other's box only, since each test walks a ring.
                        let b_box = (b.bbox.0, b.bbox.1 + k, b.bbox.2, b.bbox.3 + k);
                        for (pts, other, other_box) in
                            [(&a.pts, &b_pts, b_box), (&b_pts, &a.pts, a.bbox)]
                        {
                            let step = (pts.len() / 4).max(1);
                            let hit = pts.iter().step_by(step).find(|p| {
                                p.0 > other_box.0
                                    && p.0 < other_box.2
                                    && p.1 > other_box.1
                                    && p.1 < other_box.3
                                    && side_doubled(2 * p.0, 2 * p.1, other) == Some(true)
                            });
                            #[allow(clippy::cast_precision_loss)]
                            if let Some(&p) = hit {
                                seeds.push(deg(p.0 as f64, p.1 as f64));
                            }
                        }
                    }
                }
            }
        }
        self.crossing_seeds(&mut seeds);
        // The cells are a hash map: order the seeds so the draw is fixed.
        seeds.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.total_cmp(&y.1)));
        seeds.dedup();
        (seeds, pairs.len())
    }

    /// Proper crossings between edges of two polygons of one level that
    /// share a grid cell, the second aligned to the first, in degrees.
    fn crossing_seeds(&self, seeds: &mut Vec<(f64, f64)>) {
        let deg = |lat: f64, lon: f64| (lat * 1e-7, lon::fold_deg(lon * 1e-7));
        // An edge two polygons share (neighbours share their border as a
        // rule) crosses nothing the other's own edges do not, so shared
        // edges are left out before the pairs are tried.
        let level_of = |e: u32| self.polygons[self.edges[e as usize].0 as usize].level;
        let segment = |e: u32| {
            let (_, a, b) = self.edges[e as usize];
            (level_of(e), a.min(b), a.max(b))
        };
        for list in self.edge_cells.values() {
            let mut owners: HashMap<(u8, Fp, Fp), (u32, bool)> = HashMap::new();
            for &e in list {
                let p = self.edges[e as usize].0;
                owners
                    .entry(segment(e))
                    .and_modify(|(first, shared)| *shared |= *first != p)
                    .or_insert((p, false));
            }
            let mut list: Vec<u32> = list
                .iter()
                .copied()
                .filter(|&e| !owners[&segment(e)].1)
                .collect();
            list.sort_by_key(|&e| level_of(e));
            for (n, &e) in list.iter().enumerate() {
                let (p, a, b) = self.edges[e as usize];
                for &f in &list[n + 1..] {
                    if level_of(f) != level_of(e) {
                        break;
                    }
                    let (q, c, d) = self.edges[f as usize];
                    if p == q {
                        continue;
                    }
                    #[allow(clippy::cast_precision_loss)]
                    let k = ((a.1 + b.1 - c.1 - d.1) as f64 / (2.0 * TURN as f64)).round();
                    #[allow(clippy::cast_possible_truncation)]
                    let k = k as i64 * TURN;
                    if let Some((y, x)) = crossing(a, b, (c.0, c.1 + k), (d.0, d.1 + k)) {
                        seeds.push(deg(y, x));
                    }
                }
            }
        }
    }

    /// Witnesses near the overlap seeds, drawn from `draw`, each kept only
    /// where the model alone, at the non-zero tolerance's band, finds two
    /// or more containing areas of a level with one certified larger than
    /// another.
    pub(super) fn overlap_witnesses(&self, draw: &mut Draw) -> (Vec<(f64, f64)>, OverlapAccount) {
        let (seeds, pairs) = self.overlap_seeds();
        let mut account = OverlapAccount {
            pairs,
            seeds: seeds.len(),
            kept: 0,
        };
        let mut out = Vec::new();
        if seeds.is_empty() {
            return (out, account);
        }
        for _ in 0..GEOCODE_OVERLAP_ATTEMPTS {
            if out.len() >= GEOCODE_OVERLAP_WITNESSES {
                break;
            }
            #[allow(clippy::cast_possible_truncation)]
            let (lat, lon) = seeds[(draw.next() % seeds.len() as u64) as usize];
            let reach = draw.unit() * GEOCODE_ADMIN_WITNESS_REACH_M;
            let bearing = draw.unit() * std::f64::consts::TAU;
            let (lat, lon) = lon::destination(lat, lon, reach / EARTH_RADIUS_M, bearing);
            let (_, lat, lon) = super::point(lat, lon);
            let expect = self.expect(lat, lon, TOLERANCE_BAND_M);
            let competing = expect.levels.iter().any(|l| match l {
                Level::Some { candidates, .. } => candidates.iter().any(|&i| {
                    candidates
                        .iter()
                        .any(|&j| self.polygons[i].area.1 < self.polygons[j].area.0)
                }),
                _ => false,
            });
            if competing {
                out.push((lat, lon));
            }
        }
        account.kept = out.len();
        (out, account)
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
    /// Judged answers with some other containing candidate certified
    /// larger (its least possible area above the answer's greatest).
    pub(super) ruled_out: usize,
    /// Judged answers certified smaller than every other candidate.
    pub(super) unique: usize,
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
                        let larger = |j: &usize| {
                            *j != i && self.polygons[i].area.1 < self.polygons[*j].area.0
                        };
                        if candidates.iter().any(larger) {
                            tally.ruled_out += 1;
                        }
                        if candidates.iter().filter(|&&j| j != i).all(larger) {
                            tally.unique += 1;
                        }
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

    /// Members through the named nodes of `at`.
    fn members(list: &[&[i64]], at: impl Fn(i64) -> Pt) -> Vec<Vec<Node>> {
        list.iter()
            .map(|w| w.iter().map(|&id| (id, at(id))).collect())
            .collect()
    }

    /// The ring count, or the outcome when there are no rings to count.
    fn count(list: &[&[i64]], at: impl Fn(i64) -> Pt) -> Result<usize, String> {
        match role_rings(&members(list, at)) {
            RoleRings::Rings {
                rings,
                undecided: false,
            } => Ok(rings.len()),
            other => Err(format!("{other:?}")),
        }
    }

    /// A square `[0, 10] x [0, 10]` with named corners, as `(lat, lon)`:
    /// 1 (0,0), 2 (0,10), 3 (10,10), 4 (10,0); other IDs `lat * 100 + lon`.
    fn grid(id: i64) -> Pt {
        match id {
            1 => (0, 0),
            2 => (0, 10),
            3 => (10, 10),
            4 => (10, 0),
            _ => (
                i32::try_from(id / 100).unwrap(),
                i32::try_from(id % 100).unwrap(),
            ),
        }
    }

    /// The builder's ring-assembly cases (its own unit tests), judged by
    /// the independent oracle.
    #[test]
    fn the_oracle_assembles_as_the_builder_rules_say() {
        let refused = Err("Refused".to_owned());
        // Split members close one ring, reversed or not; a chain alone none.
        assert_eq!(count(&[&[1, 2], &[3, 2], &[3, 4, 1]], grid), Ok(1));
        assert_eq!(count(&[&[1, 2], &[2, 3]], grid), Ok(0));
        // A spur and a one-node member are dropped, not refused.
        assert_eq!(count(&[&[1, 2, 3, 4, 1], &[1, 9_999]], grid), Ok(1));
        assert_eq!(count(&[&[1, 2, 3], &[3, 4, 1], &[1]], grid), Ok(1));
        // Four paths between two nodes, and a theta: refused in any order.
        let mut paths: Vec<&[i64]> = vec![&[1, 102, 2], &[1, 304, 2], &[1, 506, 2], &[1, 708, 2]];
        for _ in 0..4 {
            assert_eq!(count(&paths, grid), refused);
            paths.rotate_left(1);
        }
        assert_eq!(count(&[&[1, 2], &[2, 3, 4, 1], &[2, 4]], grid), refused);
        // A leaf beside many loops at one node: each closed loop is its own
        // graph, never compared with the others, and the leaf is dropped.
        let loops: Vec<Vec<i64>> = (1..=20)
            .map(|k| vec![1, 100 * k + 1, 100 * k + 2, 1])
            .collect();
        let mut list: Vec<&[i64]> = vec![&[9_999, 1]];
        list.extend(loops.iter().map(Vec::as_slice));
        assert_eq!(count(&list, grid), Ok(20));
        // Two triangles touching at a node are two rings.
        let touch = |id: i64| {
            [(0, 0), (10, 0), (5, 5), (10, 10), (0, 10)][usize::try_from(id - 1).unwrap()]
        };
        assert_eq!(
            count(
                &[&[1, 2], &[2, 3], &[3, 4], &[4, 5], &[5, 3], &[3, 1]],
                touch
            ),
            Ok(2)
        );
        // Retraces and spikes.
        assert_eq!(count(&[&[1, 2, 1]], grid), refused);
        assert_eq!(count(&[&[1, 2], &[2, 1]], grid), refused);
        assert_eq!(count(&[&[1, 2, 3, 1, 2, 3, 1]], grid), refused);
        assert_eq!(count(&[&[1, 2, 3, 708, 3, 4, 1]], grid), refused);
        // Distinct nodes at one coordinate close nothing.
        let seven = |id: i64| if id == 7 { grid(1) } else { grid(id) };
        assert_eq!(count(&[&[1, 2, 3], &[3, 4, 7]], seven), Ok(0));
        // ... but adjacent ones are one run.
        assert_eq!(count(&[&[1, 2, 3, 4, 7, 1]], seven), Ok(1));
        // A revisit outside one run, and a collinear ring.
        let eight = |id: i64| match id {
            1 | 4 => (0, 0),
            2 => (10, 10),
            3 => (10, -10),
            5 => (-10, -10),
            _ => (-10, 10),
        };
        assert_eq!(count(&[&[1, 2, 3, 4, 5, 6, 1]], eight), refused);
        let flat = |id: i64| (0, i32::try_from(id).unwrap() * 10);
        assert_eq!(count(&[&[1, 2, 3, 1]], flat), refused);
        // Lobes of one walk: disjoint ones split, a nested one refuses.
        let lobes = |id: i64| match id {
            1 => (5, 10),
            2 => (0, 10),
            3 => (0, 0),
            4 => (10, 0),
            5 => (10, 10),
            6 => (4, 14),
            7 => (6, 14),
            8 => (4, 8),
            _ => (6, 8),
        };
        assert_eq!(count(&[&[1, 2, 3, 4, 5, 1, 6, 7, 1]], lobes), Ok(2));
        assert_eq!(count(&[&[1, 2, 3, 4, 5, 1, 8, 9, 1]], lobes), refused);
        // A refusal in one closed member refuses the role.
        assert_eq!(count(&[&[1, 2, 3, 4, 1], &[1, 2, 1]], grid), refused);
    }

    /// Two overlapping rectangles joined by a bridge: some midpoints of
    /// one lie outside the other and some inside, so the builder's verdict
    /// follows member order and the role is undecided.
    #[test]
    fn crossing_rings_of_one_graph_are_undecided() {
        let at = |id: i64| match id {
            1 => (0, 0),
            2 => (0, 10),
            3 => (10, 10),
            4 => (10, 0),
            11 => (2, 5),
            12 => (2, 15),
            13 => (8, 15),
            _ => (8, 5),
        };
        let r = role_rings(&members(
            &[&[1, 2, 3, 4, 1], &[3, 13], &[11, 12, 13, 14, 11]],
            at,
        ));
        // Closed members are graphs of their own: never compared.
        assert!(
            matches!(r, RoleRings::Rings { ref rings, undecided: false } if rings.len() == 2),
            "{r:?}"
        );
        let r = role_rings(&members(
            &[
                &[1, 2],
                &[2, 3],
                &[3, 4],
                &[4, 1],
                &[3, 13],
                &[11, 12],
                &[12, 13],
                &[13, 14],
                &[14, 11],
            ],
            at,
        ));
        assert!(
            matches!(
                r,
                RoleRings::Rings {
                    undecided: true,
                    ..
                }
            ),
            "{r:?}"
        );
    }

    #[test]
    fn closed_members_never_meet_and_a_refused_inner_withholds() {
        let at = |id: i64| match id {
            1 => (0, 0),
            2 => (0, 1_000_000),
            3 => (1_000_000, 1_000_000),
            4 => (1_000_000, 0),
            11 => (200_000, 500_000),
            12 => (200_000, 1_500_000),
            13 => (800_000, 1_500_000),
            _ => (800_000, 500_000),
        };
        let mut ways = HashMap::new();
        for (id, refs) in [
            (1, vec![1, 2]),
            (2, vec![2, 3]),
            (3, vec![3, 4]),
            (4, vec![4, 1]),
            (5, vec![3, 13]),
            (6, vec![11, 12, 13, 14, 11]),
            (7, vec![1, 2, 1]),
            (8, vec![11, 12, 13]),
            (9, vec![13, 14, 11]),
        ] {
            let nodes: Vec<Node> = refs.iter().map(|&r| (r, at(r))).collect();
            ways.insert(id, way(&nodes));
        }
        let records = NodeRecords::new();
        // Way 6 is closed, its own graph: the open square and it never meet.
        let model =
            AdminModel::build(&[relation("w1@,w2@,w3@,w4@,w5@,w6@,w6@")], &ways, &records).unwrap();
        assert_eq!((model.polygons.len(), model.withheld), (2, 0));
        // An inner role refused withholds whatever the outer is.
        let model =
            AdminModel::build(&[relation("w1@,w2@,w3@,w4@,w7@inner")], &ways, &records).unwrap();
        assert_eq!((model.polygons.len(), model.withheld), (0, 1));
        // Drawn by open ways, the bridged rectangles cross undecided: the
        // relation's box is unjudged at its level.
        let model =
            AdminModel::build(&[relation("w1@,w2@,w3@,w4@,w5@,w8@,w9@")], &ways, &records).unwrap();
        assert!(model.polygons.is_empty());
        assert_eq!(model.unmodelled_why[0].2, Unmodelled::UndecidedLobes);
        assert!(matches!(
            model.expect(0.05, 0.05, 1.0).levels[2],
            Level::Unjudged
        ));
    }

    fn relation(members: &str) -> Record<'static> {
        super::super::parse_record(&format!(
            "r1 v1 Tboundary=administrative,admin_level=2,name=C,ISO3166-1:alpha2=CC M{members}"
        ))
        .unwrap()
        .into_owned()
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
        let r = |s: Vec<Pt>| Ring::new(&s);
        let outer = r(square(0, 0, 100));
        assert_eq!(owned(&r(square(10, 10, 10)), &outer), Some(true));
        assert_eq!(owned(&r(square(200, 200, 10)), &outer), Some(false));
        // Sharing the outer's corner: undecided.
        assert_eq!(owned(&r(square(0, 0, 10)), &outer), None);
    }

    #[test]
    fn an_undecided_hole_leaves_only_its_box_unjudged() {
        let record = |line: &str| super::super::parse_record(line).unwrap().into_owned();
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

    /// Two nested same-level areas: the smaller is the answer, and naming
    /// the larger one at the smaller's interior is judged wrong.
    #[test]
    fn same_level_competition_rejects_the_larger_area() {
        let record = |line: &str| super::super::parse_record(line).unwrap().into_owned();
        let mut ways = HashMap::new();
        ways.insert(1, way(&ids(1, &square(0, 0, 1_000_000))));
        ways.insert(2, way(&ids(11, &square(200_000, 200_000, 200_000))));
        let relations = [
            record("r1 v1 Tboundary=administrative,admin_level=8,name=Big Mw1@outer"),
            record("r2 v1 Tboundary=administrative,admin_level=8,name=Small Mw2@outer"),
        ];
        let model = AdminModel::build(&relations, &ways, &NodeRecords::new()).unwrap();
        let expect = model.expect(0.03, 0.03, 1.0);
        let answer = |name: &str, area: f64| serde_json::json!([{ "admin_level": 8, "name": name, "country_code": null, "area": area }]);
        let mut tally = AdminTally::default();
        model
            .judge(&expect, &answer("Small", 4e10), &mut tally)
            .unwrap();
        assert_eq!(tally.several, 1);
        let why = model
            .judge(&expect, &answer("Big", 1e12), &mut tally)
            .unwrap_err();
        assert!(why.contains("expected the least of"), "{why}");
    }

    #[test]
    fn the_area_interval_holds_the_exact_area() {
        let (lo, hi) = area_interval(&Ring::new(&square(550_000_000, 120_000_000, 1_000_000)));
        let exact = 1e12;
        assert!(lo <= exact && exact <= hi, "{lo} {hi}");
        #[allow(clippy::cast_possible_truncation)]
        let stored = f64::from(exact as f32);
        assert!(lo <= stored && stored <= hi);
    }

    /// A ring across the antimeridian, closed in `(lat, lon)`.
    fn seam_ring(lat: i32, height: i32, east: i32) -> Vec<Pt> {
        vec![
            (lat, east),
            (lat, -east),
            (lat + height, -east),
            (lat + height, east),
            (lat, east),
        ]
    }

    #[test]
    fn rings_across_the_antimeridian_are_judged_unwrapped() {
        let seam = Ring::new(&seam_ring(0, 1_000_000, 1_799_000_000));
        assert!(seam.wraps && seam.invariant);
        assert!(seam.contains(500_000.0, 1_799_500_000.0));
        assert!(seam.contains(500_000.0, -1_799_500_000.0));
        assert!(seam.contains(500_000.0, 1_800_000_000.0));
        assert!(!seam.contains(500_000.0, 0.0));
        // The unwrapped area is the ring's own, not the rest of the world's.
        let (lo, hi) = area_interval(&seam);
        assert!(lo <= 2e12 && 2e12 <= hi, "{lo} {hi}");
        // A ring touching the antimeridian from the east does not unwrap.
        assert!(!Ring::new(&square(0, -1_800_000_000, 1_000_000)).wraps);
        // A ring winding round a pole and one with a half-turn edge depend
        // on where their walk starts.
        let winding = [
            (800_000_000, 0),
            (800_000_000, 1_200_000_000),
            (800_000_000, -1_200_000_000),
            (800_000_000, 0),
        ];
        assert!(!start_invariant(&winding));
        assert!(!start_invariant(&[
            (0, 0),
            (0, 1_800_000_000),
            (10, 0),
            (0, 0)
        ]));
    }

    #[test]
    fn an_owned_hole_across_the_antimeridian_is_decided() {
        let outer = Ring::new(&seam_ring(0, 1_000_000, 1_790_000_000));
        let hole = Ring::new(&seam_ring(400_000, 200_000, 1_799_000_000));
        assert_eq!(owned(&hole, &outer), Some(true));
        // Lifted from its western side, the same hole is the same answer.
        let mut west = seam_ring(400_000, 200_000, 1_799_000_000);
        west.rotate_left(1);
        let last = west[0];
        west[4] = last;
        assert_eq!(owned(&Ring::new(&west), &outer), Some(true));
        let far = Ring::new(&square(400_000, 0, 200_000));
        assert_eq!(owned(&far, &outer), Some(false));
    }

    /// A midpoint on the antimeridian against a ring that does not unwrap:
    /// one spelling is on its edge, the other outside, so the walk's start
    /// decides and the verdict is undecided.
    #[test]
    fn a_seam_midpoint_whose_spelling_decides_is_undecided() {
        let a = Ring::new(&seam_ring(0, 1_000_000, 1_799_000_000));
        let a_ids = [1, 2, 3, 4];
        let b = Ring::new(&square(-500_000, -1_800_000_000, 900_000));
        let b_ids = [11, 12, 13, 14];
        assert_eq!(outside_by_midpoints(&a_ids, &a, &b_ids, &b), None);
        // Clear of the edge, both spellings agree.
        let c = Ring::new(&square(-5_000_000, -1_700_000_000, 1_000_000));
        assert_eq!(outside_by_midpoints(&a_ids, &a, &b_ids, &c), Some(true));
    }

    /// Two areas of one level overlapping along collinear edges, with no
    /// proper crossing and no vertex strictly inside the other, are still
    /// found, and every witness kept has the larger certified larger.
    #[test]
    fn a_collinear_overlap_is_witnessed() {
        let record = |line: &str| super::super::parse_record(line).unwrap().into_owned();
        let rect = |lon: i32, width: i32| {
            vec![
                (0, lon),
                (0, lon + width),
                (1_000_000, lon + width),
                (1_000_000, lon),
                (0, lon),
            ]
        };
        let mut ways = HashMap::new();
        ways.insert(1, way(&ids(1, &rect(0, 2_000_000))));
        ways.insert(2, way(&ids(11, &rect(1_000_000, 3_000_000))));
        let relations = [
            record("r1 v1 Tboundary=administrative,admin_level=8,name=A Mw1@outer"),
            record("r2 v1 Tboundary=administrative,admin_level=8,name=B Mw2@outer"),
        ];
        let model = AdminModel::build(&relations, &ways, &NodeRecords::new()).unwrap();
        let (points, account) = model.overlap_witnesses(&mut Draw(1));
        assert_eq!(account.pairs, 1);
        assert!(!points.is_empty(), "{account:?}");
        for (lat, lon) in points {
            let Level::Some { candidates, .. } =
                &model.expect(lat, lon, TOLERANCE_BAND_M).levels[8]
            else {
                panic!("{lat} {lon}");
            };
            assert_eq!(candidates.len(), 2);
        }
    }
}
