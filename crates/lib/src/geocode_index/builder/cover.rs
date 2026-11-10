//! Conservative S2 cell cover of a path that is linear in (lat, lon).
//!
//! # Contract
//!
//! A path runs from one point to another linearly in latitude and longitude
//! degrees, in one continuous longitude frame chosen by the caller (any
//! longitude delta is a legal path; the points on it are placed on the sphere
//! through sin and cos). This is the geometry the geocode reader uses: a
//! street or interpolation segment is the path `geo::point_to_segment_distance_sq`
//! measures to, with the far longitude unwrapped around the near one, and an
//! admin ring edge is the planar edge `geo::point_in_ring_with_antimeridian`
//! ray-casts against, in the ring frame `geo::pip_ring_frame` gives it.
//!
//! The cover is two-sided:
//!
//! - every S2 cell at the level that meets the path (cells are closed sets,
//!   so a path on a cell boundary yields the cells on both sides) is emitted;
//! - every emitted cell meets the `rho`-neighbourhood of the path, where
//!   `rho = delta + (sqrt 2 + 1) * delta_uv` is a few thousandths of the
//!   smallest cell width (see [`Plan`]).
//!
//! # Method
//!
//! The path is cut into `n` pieces. Each piece is replaced by the great-circle
//! chord between its ends, with a proved bound `delta` on the distance
//! between the piece and its chord ([`deviation`]). A chord is a straight
//! segment in the (u, v) plane of every S2 cube face it reaches (that plane is
//! the face's gnomonic projection), and S2 cells are axis-aligned rectangles
//! there, so the cells within `delta` of the chord are found exactly by a walk
//! over the face's grid ([`walk_face`]) after dilating the chord by
//! `delta_uv`, `delta` converted to (u, v) units plus a rounding slack.
//!
//! This replaces a sampler that stepped along the path at 0.4 of the
//! smallest cell width: a path that clips a cell near its corner for less
//! than one step was never emitted. On random street-length segments at
//! level 17 that missed at least one touched cell on about one segment in
//! six.

use std::f64::consts::{PI, SQRT_2};

use s2::cellid::CellID;
use s2::latlng::LatLng;
use s2::metric::{MAX_DIAGMETRIC, MIN_AREAMETRIC, MIN_WIDTHMETRIC};
use s2::point::Point;

use crate::coord_fmt::e7_to_deg;
use crate::geo;
use crate::tunables::{
    MAX_SEGMENT_CELLS, PATH_COVER_MAX_PIECE_RAD, PATH_COVER_TOLERANCE_WIDTH_FRACTION,
    PATH_COVER_UV_SLACK,
};

/// Leaf cells per face side (`2^30`), as S2's `MAX_SIZE`.
const LEAF_CELLS_PER_SIDE: f64 = 1_073_741_824.0;

/// Upper bound on the gnomonic stretch (sphere angle to (u, v) length) over
/// every point the walk compares against a face's grid. The stretch at
/// `(u, v)` is at most `1 + u^2 + v^2`; the clipped chords lie in the face
/// square padded by `delta_uv`, and with their dilation in `|u|, |v| <= 1.2`
/// while `delta_uv <= 0.1` (it is at most a few thousandths of a level-0
/// cell), where `1 + 2 * 1.2^2 = 3.88 <= 4`.
const UV_STRETCH: f64 = 4.0;

/// A chord reaches face `f`'s padded square only if both its ends have face
/// axis component at least this. Points of a face have axis component at
/// least `1/sqrt 3` (54.74 degrees from the axis); a chord is no longer than
/// `PATH_COVER_MAX_PIECE_RAD` (0.1 rad, 5.73 degrees) and the padding is far
/// below a degree, so each end of a chord that reaches the face is within
/// 60.5 degrees of its axis, `cos(60.5 deg) = 0.492`.
const FACE_REACH_AXIS_COMPONENT: f64 = 0.49;

const _: () = assert!(
    PATH_COVER_MAX_PIECE_RAD <= 0.1,
    "FACE_REACH_AXIS_COMPONENT is derived for chords of at most 0.1 rad"
);

/// A way node, converted to a sphere point once and shared by both segments
/// it ends.
#[derive(Clone, Copy, Debug)]
pub(super) struct Vertex {
    lon_e7: i32,
    lat_deg: f64,
    lon_deg: f64,
    xyz: [f64; 3],
}

impl Vertex {
    pub(super) fn new(lat_e7: i32, lon_e7: i32) -> Self {
        let lat_deg = e7_to_deg(f64::from(lat_e7));
        let lon_deg = e7_to_deg(f64::from(lon_e7));
        Self {
            lon_e7,
            lat_deg,
            lon_deg,
            xyz: point_xyz(lat_deg, lon_deg),
        }
    }
}

/// One end of a path: its (lat, lon) in the path's frame and the sphere point
/// it names. An end built from a stored vertex keeps that vertex's own point,
/// so the walk reaches the very cell `CellID::from` gives a query there.
#[derive(Clone, Copy, Debug)]
struct PathEnd {
    lat_deg: f64,
    lon_deg: f64,
    xyz: [f64; 3],
}

fn point_xyz(lat_deg: f64, lon_deg: f64) -> [f64; 3] {
    let p = Point::from(LatLng::from_degrees(lat_deg, lon_deg));
    [p.0.x, p.0.y, p.0.z]
}

// ---------------------------------------------------------------------------
// The S2 cube projection, as s2 0.2 computes it.
//
// s2 keeps these in a private module. They are reproduced operation for
// operation so a point's (face, i, j) here is bit-identical to
// `CellID::from(Point)`; `s2_projection_matches_cellid_from_point` pins that.
// ---------------------------------------------------------------------------

fn face_of(p: &[f64; 3]) -> u8 {
    let (ax, ay, az) = (p[0].abs(), p[1].abs(), p[2].abs());
    let mut id = 0;
    let mut value = p[0];
    if ay > ax {
        id = 1;
        value = p[1];
    }
    if az > value.abs() {
        id = 2;
        value = p[2];
    }
    if value < 0.0 {
        id += 3;
    }
    id
}

/// The component of `p` along face `face`'s outward axis.
fn axis_component(face: u8, p: &[f64; 3]) -> f64 {
    match face {
        0 => p[0],
        1 => p[1],
        2 => p[2],
        3 => -p[0],
        4 => -p[1],
        _ => -p[2],
    }
}

/// (u, v) of `p` on face `face`; valid where `axis_component(face, p) > 0`.
fn face_uv(face: u8, p: &[f64; 3]) -> (f64, f64) {
    let [x, y, z] = *p;
    match face {
        0 => (y / x, z / x),
        1 => (-x / y, z / y),
        2 => (-x / z, -y / z),
        3 => (z / x, y / x),
        4 => (z / y, -x / y),
        _ => (-y / z, -x / z),
    }
}

fn st_to_uv(s: f64) -> f64 {
    if s >= 0.5 {
        (1. / 3.) * (4. * s * s - 1.)
    } else {
        (1. / 3.) * (1. - 4. * (1. - s) * (1. - s))
    }
}

fn uv_to_st(u: f64) -> f64 {
    if u >= 0. {
        0.5 * (1. + 3. * u).sqrt()
    } else {
        1. - 0.5 * (1. - 3. * u).sqrt()
    }
}

/// Row or column index at the level whose `shift` is `30 - level`, for a
/// face coordinate; coordinates past the face edge clamp to the edge index,
/// as S2's `st_to_ij` does.
fn grid_index(uv: f64, shift: u32) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    let leaf = ((LEAF_CELLS_PER_SIDE * uv_to_st(uv)).floor() as i32).clamp(0, (1 << 30) - 1);
    leaf >> shift
}

fn cell_at(face: u8, i: i32, j: i32, shift: u32, level: u8) -> u64 {
    CellID::from_face_ij(face, i << shift, j << shift)
        .parent(u64::from(level))
        .0
}

// ---------------------------------------------------------------------------
// Chords and their distance from the path
// ---------------------------------------------------------------------------

/// How a path is cut, and how far its chords provably are from it.
#[derive(Clone, Copy, Debug)]
struct Plan {
    /// Number of pieces, each replaced by one chord.
    pieces: u64,
    /// Bound on the angular distance between each piece and its chord.
    delta: f64,
}

impl Plan {
    /// The chord dilation in face (u, v) units.
    fn delta_uv(&self) -> f64 {
        UV_STRETCH * self.delta + PATH_COVER_UV_SLACK
    }
}

/// Bound on the distance between one of `n` equal pieces of a path and the
/// chord joining its ends, for a path whose whole latitude and longitude
/// deltas are `b` and `d` radians; `None` when the bound's own preconditions
/// fail (the plan then cuts finer).
///
/// Derivation, per piece reparametrised on `t` in `[0, 1]` with deltas
/// `bp = b/n`, `dp = d/n`: on the unit sphere
/// `gamma'' = -(bp^2 + dp^2 cos^2 phi) gamma + dp sin(phi) (-2 bp E + dp cos(phi) N)`
/// with `E`, `N` the unit east and north vectors. `f = normal . gamma`, the
/// sine of the distance to the chord's great circle, vanishes at both ends,
/// so `max |f| <= max |f''| / 8 <= (Kp max|f| + Hp) / 8`, with
/// `Kp = bp^2 + dp^2` and `Hp = |dp| sqrt(4 bp^2 + dp^2)` bounding the
/// tangential term; hence `max |f| <= F = Hp / (8 - Kp)` and the cross-track
/// distance is at most `asin F`. The along-track angle `alpha` of the point's
/// projection onto the chord plane runs from 0 to the chord length with
/// `|alpha''| <= (Kp + Hp)/r + 2 F Kp / r^3`, `r = sqrt(1 - F^2)`, so it
/// overshoots either end by at most an eighth of that. The bound is their
/// sum.
fn deviation(b: f64, d: f64, n: u64) -> Option<f64> {
    #[allow(clippy::cast_precision_loss)]
    let n2 = (n as f64) * (n as f64);
    let kp = (b * b + d * d) / n2;
    let hp = d.abs() * (4.0 * b * b + d * d).sqrt() / n2;
    if kp >= 8.0 {
        return None;
    }
    let f = hp / (8.0 - kp);
    if f >= 1.0 {
        return None;
    }
    let r = (1.0 - f * f).sqrt();
    let overshoot = ((kp + hp) / r + 2.0 * f * kp / (r * r * r)) / 8.0;
    Some(f.asin() + overshoot)
}

/// Cut a path with deltas `b`, `d` (radians) so each piece is at most
/// `PATH_COVER_MAX_PIECE_RAD` long in parameter and within the level's
/// tolerance of its chord.
fn plan(b: f64, d: f64, level: u8) -> Plan {
    let k = b * b + d * d;
    let h = d.abs() * (4.0 * b * b + d * d).sqrt();
    let tolerance = MIN_WIDTHMETRIC.value(level) * PATH_COVER_TOLERANCE_WIDTH_FRACTION;
    // The bound is about (Kp + 2 Hp) / 8 for small pieces; start there and
    // step up until the exact bound holds.
    let by_length = (k.sqrt() / PATH_COVER_MAX_PIECE_RAD).ceil();
    let by_deviation = ((k + 2.0 * h) / (8.0 * tolerance)).sqrt().ceil();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let mut pieces = by_length.max(by_deviation).max(1.0) as u64;
    loop {
        if let Some(delta) = deviation(b, d, pieces)
            && delta <= tolerance
        {
            return Plan { pieces, delta };
        }
        pieces += pieces / 8 + 1;
    }
}

/// Upper bound on the raw cell entries (before deduplication) the walk emits
/// for a path with deltas `b`, `d` cut by `plan`.
///
/// Each chord's walk emits distinct cells (each face walk emits a given
/// `(face, i, j)` once, and cells on different faces differ), and every one
/// meets the chord's `rho`-neighbourhood, so it lies within
/// `R = rho + MAX_DIAG(level)` of the chord (`MAX_DIAG` is the largest cell
/// diameter). Cell interiors are disjoint and at least `MIN_AREA(level)` in
/// area, and the `R`-tube around a great-circle arc of length `l` has area
/// at most `2 R l + pi R^2`; so a chord emits at most
/// `(2 R l + pi R^2) / MIN_AREA` cells. The chords' lengths sum to at most
/// the path's length, which is at most `sqrt(b^2 + d^2)`.
fn raw_cell_bound(b: f64, d: f64, level: u8, plan: &Plan) -> f64 {
    let rho = plan.delta + (SQRT_2 + 1.0) * plan.delta_uv();
    let reach = rho + MAX_DIAGMETRIC.value(level);
    let length = (b * b + d * d).sqrt();
    #[allow(clippy::cast_precision_loss)]
    let chords = plan.pieces as f64;
    (2.0 * reach * length + chords * PI * reach * reach) / MIN_AREAMETRIC.value(level)
}

/// Raw cell entries the widest legal street segment can need at `level`:
/// after the far endpoint is unwrapped around the near one, a legal segment
/// spans at most pi in latitude and pi in longitude, and the bound grows with
/// both.
pub(super) fn worst_case_segment_cells(level: u8) -> f64 {
    raw_cell_bound(PI, PI, level, &plan(PI, PI, level))
}

/// Whether a street or interpolation segment at `level` can exceed the
/// per-segment cell budget.
pub(in crate::geocode_index) fn level_exceeds_segment_cell_budget(level: u8) -> bool {
    level > 30 || worst_case_segment_cells(level) > MAX_SEGMENT_CELLS
}

// ---------------------------------------------------------------------------
// The walk
// ---------------------------------------------------------------------------

/// Emit the cells of every chord of `plan` along the path from `a` to `b`.
fn walk_path(a: &PathEnd, b: &PathEnd, level: u8, plan: &Plan, emit: &mut impl FnMut(u64)) {
    let pad = plan.delta_uv();
    let mut prev = a.xyz;
    for k in 1..=plan.pieces {
        let next = if k == plan.pieces {
            b.xyz
        } else {
            #[allow(clippy::cast_precision_loss)]
            let t = k as f64 / plan.pieces as f64;
            point_xyz(
                a.lat_deg + t * (b.lat_deg - a.lat_deg),
                a.lon_deg + t * (b.lon_deg - a.lon_deg),
            )
        };
        walk_chord(&prev, &next, level, pad, emit);
        prev = next;
    }
}

/// Emit every cell meeting the chord from `a` to `b` dilated by `pad` in each
/// face's (u, v) plane. Each cell is emitted once.
fn walk_chord(a: &[f64; 3], b: &[f64; 3], level: u8, pad: f64, emit: &mut impl FnMut(u64)) {
    let face = face_of(a);
    if face == face_of(b) {
        let ua = face_uv(face, a);
        let ub = face_uv(face, b);
        // A chord at least 4 pad inside its face's square is at least
        // 4 pad / 3 (the stretch inside the square is at most 3) from the
        // face edge in angle, so more than pad outside every other face's
        // square (projection never shortens): no other face can hold a cell
        // within the dilation, and the straight chord stays in the square.
        let inner = 1.0 - 4.0 * pad;
        if ua.0.abs() <= inner && ua.1.abs() <= inner && ub.0.abs() <= inner && ub.1.abs() <= inner
        {
            walk_face(face, ua, ub, level, pad, emit);
            return;
        }
    }
    for face in 0..6u8 {
        if axis_component(face, a) < FACE_REACH_AXIS_COMPONENT
            || axis_component(face, b) < FACE_REACH_AXIS_COMPONENT
        {
            continue;
        }
        let bound = 1.0 + pad;
        if let Some((ca, cb)) = clip_to_square(face_uv(face, a), face_uv(face, b), bound) {
            walk_face(face, ca, cb, level, pad, emit);
        }
    }
}

/// Liang-Barsky clip of the segment `a`-`b` to `[-bound, bound]^2`.
fn clip_to_square(a: (f64, f64), b: (f64, f64), bound: f64) -> Option<((f64, f64), (f64, f64))> {
    let du = b.0 - a.0;
    let dv = b.1 - a.1;
    let mut t0 = 0.0f64;
    let mut t1 = 1.0f64;
    for (p, q) in [
        (-du, a.0 + bound),
        (du, bound - a.0),
        (-dv, a.1 + bound),
        (dv, bound - a.1),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                if r > t1 {
                    return None;
                }
                t0 = t0.max(r);
            } else {
                if r < t0 {
                    return None;
                }
                t1 = t1.min(r);
            }
        }
    }
    Some((
        (a.0 + t0 * du, a.1 + t0 * dv),
        (a.0 + t1 * du, a.1 + t1 * dv),
    ))
}

/// Emit every cell of `face` meeting the straight (u, v) segment `a`-`b`
/// dilated by the square `[-pad, pad]^2`, each once.
///
/// The walk runs along the segment's longer axis (the "major" one), so its
/// slope against the other is at most 1. For each major index whose interval,
/// widened by `pad`, meets the segment, the segment's minor-coordinate range
/// over that widened interval, widened by `pad` again, gives the minor
/// indices. Indices past the face edge clamp to the edge row or column.
fn walk_face(
    face: u8,
    a: (f64, f64),
    b: (f64, f64),
    level: u8,
    pad: f64,
    emit: &mut impl FnMut(u64),
) {
    let shift = 30 - u32::from(level);
    let cells_per_side = f64::from(1u32 << level);
    let along_u = (b.0 - a.0).abs() >= (b.1 - a.1).abs();
    let ((mut p0, mut q0), (mut p1, mut q1)) = if along_u {
        (a, b)
    } else {
        ((a.1, a.0), (b.1, b.0))
    };
    if p0 > p1 {
        std::mem::swap(&mut p0, &mut p1);
        std::mem::swap(&mut q0, &mut q1);
    }
    let (q_min, q_max) = (q0.min(q1), q0.max(q1));
    let span = p1 - p0;
    let minor_at = |s: f64| (q0 + (s - p0) / span * (q1 - q0)).clamp(q_min, q_max);

    let first = grid_index(p0 - pad, shift);
    let last = grid_index(p1 + pad, shift);
    for k in first..=last {
        let lo = st_to_uv(f64::from(k) / cells_per_side) - pad;
        let hi = st_to_uv(f64::from(k + 1) / cells_per_side) + pad;
        // The part of the segment under the widened interval. Clamping both
        // ends into the segment collapses it onto the nearer end when
        // rounding leaves the widened interval just short of the segment,
        // which still emits the cells there.
        let s0 = lo.clamp(p0, p1);
        let s1 = hi.clamp(p0, p1);
        let (qa, qb) = if span > 0.0 {
            (minor_at(s0), minor_at(s1))
        } else {
            (q_min, q_max)
        };
        let j_first = grid_index(qa.min(qb) - pad, shift);
        let j_last = grid_index(qa.max(qb) + pad, shift);
        for j in j_first..=j_last {
            let (i, j) = if along_u { (k, j) } else { (j, k) };
            emit(cell_at(face, i, j, shift, level));
        }
    }
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Cover one street or interpolation segment from `a` to `b` at `level`,
/// calling `emit` once for each distinct cell.
///
/// The segment is the path `geo::point_to_segment_distance_sq` measures to:
/// the far longitude is unwrapped around the near one in the integer domain.
/// When the longitude delta is exactly 180 degrees the two half-world paths
/// are equally short, and the reader's radian wrapping can pick either
/// (rounding of the independently converted ends decides), so both are
/// covered, each budgeted and deduplicated on its own and then merged. Any
/// other delta is at least 1e-7 degree from the tie, far beyond that
/// rounding, so both sides agree on the branch.
///
/// `level` must have passed [`level_exceeds_segment_cell_budget`], which
/// `BuildConfig::validate` enforces for the street and coarse levels; given
/// that, no legal segment exceeds `MAX_SEGMENT_CELLS` raw entries, and the
/// per-segment bound is asserted rather than trusted.
pub(super) fn cover_segment(a: &Vertex, b: &Vertex, level: u8, mut emit: impl FnMut(u64)) {
    const HALF_TURN_E7: i64 = geo::LON_FULL_TURN_E7 / 2;
    let delta_e7 = geo::wrapped_longitude_delta_e7(i64::from(b.lon_e7) - i64::from(a.lon_e7));
    let start = PathEnd {
        lat_deg: a.lat_deg,
        lon_deg: a.lon_deg,
        xyz: a.xyz,
    };
    let end_at = |lon_deg: f64| PathEnd {
        lat_deg: b.lat_deg,
        lon_deg,
        xyz: b.xyz,
    };
    if delta_e7.abs() == HALF_TURN_E7 {
        let east = collect_path(&start, &end_at(a.lon_deg + 180.0), level);
        let west = collect_path(&start, &end_at(a.lon_deg - 180.0), level);
        merge_sorted_unique(&east, &west, &mut emit);
        return;
    }
    #[allow(clippy::cast_precision_loss)]
    let end = end_at(a.lon_deg + e7_to_deg(delta_e7 as f64));
    let plan = budgeted_plan(&start, &end, level);
    if plan.pieces == 1 {
        // One chord: its walk emits each cell once already.
        walk_path(&start, &end, level, &plan, &mut emit);
    } else {
        for cell in collect_with(&start, &end, level, &plan) {
            emit(cell);
        }
    }
}

/// Cover the path from `(lat1, lon1)` to `(lat2, lon2)`, degrees in one
/// continuous longitude frame, at `level`. Cells may repeat; the admin caller
/// collects them into a set, which bounds its memory by the cells at the
/// level, while the work is linear in the path's length.
pub(super) fn cover_path_raw(
    (lat1, lon1): (f64, f64),
    (lat2, lon2): (f64, f64),
    level: u8,
    mut emit: impl FnMut(u64),
) {
    let a = PathEnd {
        lat_deg: lat1,
        lon_deg: lon1,
        xyz: point_xyz(lat1, lon1),
    };
    let b = PathEnd {
        lat_deg: lat2,
        lon_deg: lon2,
        xyz: point_xyz(lat2, lon2),
    };
    let plan = plan(
        (lat2 - lat1).to_radians(),
        (lon2 - lon1).to_radians(),
        level,
    );
    walk_path(&a, &b, level, &plan, &mut emit);
}

fn budgeted_plan(a: &PathEnd, b: &PathEnd, level: u8) -> Plan {
    let dlat = (b.lat_deg - a.lat_deg).to_radians();
    let dlon = (b.lon_deg - a.lon_deg).to_radians();
    let plan = plan(dlat, dlon, level);
    let bound = raw_cell_bound(dlat, dlon, level, &plan);
    assert!(
        bound <= MAX_SEGMENT_CELLS,
        "segment cover at level {level} may need {bound} cells, over the \
         {MAX_SEGMENT_CELLS} budget: BuildConfig::validate should have \
         rejected this level"
    );
    plan
}

fn collect_path(a: &PathEnd, b: &PathEnd, level: u8) -> Vec<u64> {
    let plan = budgeted_plan(a, b, level);
    collect_with(a, b, level, &plan)
}

fn collect_with(a: &PathEnd, b: &PathEnd, level: u8, plan: &Plan) -> Vec<u64> {
    let mut cells = Vec::new();
    walk_path(a, b, level, plan, &mut |cell| cells.push(cell));
    cells.sort_unstable();
    cells.dedup();
    cells
}

fn merge_sorted_unique(a: &[u64], b: &[u64], emit: &mut impl FnMut(u64)) {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                emit(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                emit(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                emit(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    for &cell in a[i..].iter().chain(&b[j..]) {
        emit(cell);
    }
}

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "fixture coordinates are legal E7 values, well inside i32"
)]
mod tests {
    use super::*;
    use crate::tunables::MAX_BUILD_S2_LEVEL;
    use rustc_hash::FxHashSet;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        /// Uniform in `[lo, hi)`.
        fn range(&mut self, lo: i64, hi: i64) -> i64 {
            lo + (self.next() % (hi - lo) as u64) as i64
        }
    }

    fn cover(lat1: i32, lon1: i32, lat2: i32, lon2: i32, level: u8) -> Vec<u64> {
        let mut cells = Vec::new();
        cover_segment(
            &Vertex::new(lat1, lon1),
            &Vertex::new(lat2, lon2),
            level,
            |c| cells.push(c),
        );
        cells
    }

    fn cell_of(lat_deg: f64, lon_deg: f64, level: u8) -> u64 {
        CellID::from(LatLng::from_degrees(lat_deg, lon_deg))
            .parent(u64::from(level))
            .0
    }

    /// Cells holding dense samples of the path (`samples + 1` points), in
    /// the frame where the far longitude is `lon2_deg`. A subset of the
    /// cells the path meets: what any conservative cover must contain.
    fn oracle(
        (lat1, lon1): (f64, f64),
        (lat2, lon2): (f64, f64),
        level: u8,
        samples: u32,
    ) -> FxHashSet<u64> {
        (0..=samples)
            .map(|i| {
                let t = f64::from(i) / f64::from(samples);
                cell_of(lat1 + t * (lat2 - lat1), lon1 + t * (lon2 - lon1), level)
            })
            .collect()
    }

    fn deg(e7: i32) -> f64 {
        e7_to_deg(f64::from(e7))
    }

    /// The projection reproduced here must agree with s2's own cell lookup
    /// bit for bit, or the walk could emit a neighbour of the cell a query
    /// at a vertex looks up.
    #[test]
    fn s2_projection_matches_cellid_from_point() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        for n in 0..200_000u32 {
            let lat = rng.range(-900_000_000, 900_000_001) as i32;
            let lon = rng.range(-1_800_000_000, 1_800_000_001) as i32;
            let xyz = point_xyz(deg(lat), deg(lon));
            let level = (n % 31) as u8;
            let shift = 30 - u32::from(level);
            let face = face_of(&xyz);
            let (u, v) = face_uv(face, &xyz);
            let ours = cell_at(
                face,
                grid_index(u, shift),
                grid_index(v, shift),
                shift,
                level,
            );
            let theirs = CellID::from(Point::from(LatLng::from_degrees(deg(lat), deg(lon))))
                .parent(u64::from(level))
                .0;
            assert_eq!(ours, theirs, "({lat}, {lon}) at level {level}");
        }
    }

    /// A segment the old 0.4-width sampler covered incompletely: it clips a
    /// level-17 cell near the corner for less than one sampling step, so no
    /// sample landed in it.
    #[test]
    fn corner_clipped_cell_is_covered() {
        let (lat1, lon1, lat2, lon2) = (559_268_077, 120_471_407, 559_292_855, 120_487_805);
        let a = (deg(lat1), deg(lon1));
        let b = (deg(lat2), deg(lon2));
        let touched = oracle(a, b, 17, 20_000);

        // The fixture is only meaningful while the old sampler misses it.
        let step = MIN_WIDTHMETRIC.value(17).to_degrees() * 0.4;
        let len = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let steps = (len / step).ceil() as u32;
        let mut sampled = oracle(a, b, 17, steps);
        sampled.insert(cell_of(a.0, a.1, 17));
        sampled.insert(cell_of(b.0, b.1, 17));
        assert!(
            !touched.is_subset(&sampled),
            "fixture no longer clips a cell the 0.4-width sampler steps over"
        );

        let cells: FxHashSet<u64> = cover(lat1, lon1, lat2, lon2, 17).into_iter().collect();
        assert!(
            touched.is_subset(&cells),
            "the corner-clipped cell must be covered"
        );
    }

    /// Completeness against a dense oracle, and tightness: emitted cells
    /// beyond the oracle are rare (only cells within the dilation of the
    /// path, or clipped closer than the oracle's spacing).
    #[test]
    fn random_segments_cover_every_touched_cell() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        // Regions: equatorial face, polar face (Denmark), a face corner
        // (lat 35.26, lon 45), the antimeridian, and near the pole (off the
        // lon 0 meridian, which is a grid line of face 2 at every level).
        let regions: [(i64, i64); 5] = [
            (0, 100_000_000),
            (556_000_000, 125_000_000),
            (352_643_897, 450_000_000),
            (100_000_000, 1_799_990_000),
            (899_000_000, 373_000_000),
        ];
        let mut extra = 0usize;
        let mut total = 0usize;
        for trial in 0..6_000u32 {
            let (clat, clon) = regions[(trial % 5) as usize];
            let level = [10u8, 14, 17][(trial / 5 % 3) as usize];
            let spread = 2_000_000i64 >> (level - 10).min(6);
            let lat1 = (clat + rng.range(-spread, spread)).clamp(-900_000_000, 900_000_000) as i32;
            let lon1 = geo::normalize_longitude_e7(clon + rng.range(-spread, spread)) as i32;
            let lat2 = (i64::from(lat1) + rng.range(-spread, spread))
                .clamp(-900_000_000, 900_000_000) as i32;
            let lon2 =
                geo::normalize_longitude_e7(i64::from(lon1) + rng.range(-spread, spread)) as i32;
            let delta = geo::wrapped_longitude_delta_e7(i64::from(lon2) - i64::from(lon1));
            let far_lon = deg(lon1) + e7_to_deg(delta as f64);
            let touched = oracle((deg(lat1), deg(lon1)), (deg(lat2), far_lon), level, 3_000);
            let emitted = cover(lat1, lon1, lat2, lon2, level);
            let cells: FxHashSet<u64> = emitted.iter().copied().collect();
            assert_eq!(cells.len(), emitted.len(), "each cell is emitted once");
            let missing: Vec<_> = touched.difference(&cells).collect();
            assert!(
                missing.is_empty(),
                "({lat1}, {lon1}) -> ({lat2}, {lon2}) at level {level} misses {missing:?}"
            );
            for &cell in &cells {
                let neighbour_of_touched = touched.contains(&cell)
                    || CellID(cell)
                        .all_neighbors(u64::from(level))
                        .iter()
                        .any(|n| touched.contains(&n.0));
                assert!(
                    neighbour_of_touched,
                    "({lat1}, {lon1}) -> ({lat2}, {lon2}) at level {level} emits a cell \
                     far from the path"
                );
            }
            // Cells beyond the oracle are counted where no grid line was put
            // under the fixture on purpose: the face corner region sits on
            // three face edges, and paths through the pole itself meet the
            // four cells around face 2's centre.
            if trial % 5 != 2 && !(lat1.abs() == 900_000_000 || lat2.abs() == 900_000_000) {
                extra += cells.len() - touched.len();
                total += touched.len();
            }
        }
        assert!(
            extra * 100 <= total,
            "covers should be close to tight: {extra} extra cells over {total}"
        );
    }

    #[test]
    fn exact_half_turn_covers_both_half_world_paths() {
        let (lat1, lon1, lat2, lon2) = (100_000_000, -114_122_776, 120_000_000, 1_685_877_224);
        let cells: FxHashSet<u64> = cover(lat1, lon1, lat2, lon2, 6).into_iter().collect();
        for far in [deg(lon1) + 180.0, deg(lon1) - 180.0] {
            let touched = oracle((deg(lat1), deg(lon1)), (deg(lat2), far), 6, 20_000);
            assert!(touched.is_subset(&cells), "half-world path to {far} missed");
        }
    }

    /// A vertex at exactly lon -180 off the poles sits on S2 face 3's
    /// midline, where -180 and +180 land in different cells at every level;
    /// both spellings name the same point, and both cells must be covered.
    #[test]
    fn antimeridian_vertex_covers_both_seam_cells() {
        let cells: FxHashSet<u64> = cover(0, -1_800_000_000, 100_000_000, -1_800_000_000, 12)
            .into_iter()
            .collect();
        let minus = cell_of(0.0, -180.0, 12);
        let plus = cell_of(0.0, 180.0, 12);
        assert_ne!(minus, plus, "fixture must sit on the face 3 seam");
        assert!(cells.contains(&minus) && cells.contains(&plus));
    }

    /// `MAX_BUILD_S2_LEVEL` is exactly the boundary of `MAX_SEGMENT_CELLS`.
    #[test]
    fn max_build_level_is_exactly_the_segment_cell_budget_boundary() {
        for level in 0..=MAX_BUILD_S2_LEVEL {
            assert!(
                !level_exceeds_segment_cell_budget(level),
                "level {level} is accepted but may need {} cells, over the \
                 {MAX_SEGMENT_CELLS} budget",
                worst_case_segment_cells(level)
            );
        }
        assert!(
            level_exceeds_segment_cell_budget(MAX_BUILD_S2_LEVEL + 1),
            "level {} must be over budget, otherwise the cap rejects levels that build fine",
            MAX_BUILD_S2_LEVEL + 1
        );
        assert!(level_exceeds_segment_cell_budget(30));
    }

    /// The bound is only a bound if the walk respects it: the widest legal
    /// segment, pole to pole across half the globe, at a level where it is
    /// cheap to walk.
    #[test]
    fn widest_legal_segment_stays_under_its_bound() {
        const LEVEL: u8 = 9;
        let a = PathEnd {
            lat_deg: -90.0,
            lon_deg: 0.0,
            xyz: point_xyz(-90.0, 0.0),
        };
        let b = PathEnd {
            lat_deg: 90.0,
            lon_deg: 180.0,
            xyz: point_xyz(90.0, 180.0),
        };
        let plan = plan(PI, PI, LEVEL);
        let mut raw = 0usize;
        walk_path(&a, &b, LEVEL, &plan, &mut |_| raw += 1);
        let bound = raw_cell_bound(PI, PI, LEVEL, &plan);
        assert!(
            (raw as f64) <= bound,
            "widest legal segment emitted {raw} raw cells at level {LEVEL}, over its bound {bound}"
        );
        assert!(
            raw > 1_000,
            "the widest segment must produce a real cover, got {raw}"
        );
    }

    #[test]
    fn short_street_segment_is_one_chord() {
        let a = Vertex::new(556_762_000, 125_683_000);
        // About 420 m north-east at 55.7 degrees.
        let b = Vertex::new(556_790_000, 125_730_000);
        let p = plan(
            (b.lat_deg - a.lat_deg).to_radians(),
            (b.lon_deg - a.lon_deg).to_radians(),
            17,
        );
        assert_eq!(p.pieces, 1, "an ordinary street segment needs one chord");
    }
}
