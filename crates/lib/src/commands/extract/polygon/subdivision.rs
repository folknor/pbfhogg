//! Exact point location on the integer coordinate lattice.
//!
//! Within a cell, a segment strictly to its left contributes nothing.
//! A segment strictly to its right contributes one crossing on the
//! half-open latitude interval between its endpoints. XOR those intervals
//! into the base parity for the cell. All other segments can touch the cell
//! and keep the original directed edge and endpoint rule. In particular,
//! a horizontal segment can matter at its endpoints even though it never
//! toggles parity.

use std::cmp::Ordering;

use crate::geo::BboxInt;
use crate::host::Entered;
use crate::read::columnar::DenseNodeColumns;
use crate::read::region_grid::RegionGrid;
use crate::tunables::{GRID_REGION_THRESHOLD, MAX_EXTRACTS};

use super::super::Region;
use super::super::membership::{admits, mask_words, regions_of, restrict};
use super::{Geometry, HALF_TURN, Polygon, Rings, Segment, TURN};

use crate::tunables::{
    REGION_SUBDIVISION_LEAF_TESTS, REGION_SUBDIVISION_MAX_CELLS, REGION_SUBDIVISION_MAX_DEPTH,
    REGION_SUBDIVISION_MAX_WORK, REGION_SUBDIVISION_MIN_DEPTH,
};

type Mask = [u64; MAX_EXTRACTS.div_ceil(64)];

fn set(mask: &mut Mask, region: usize) {
    mask[region / 64] |= 1 << (region % 64);
}

fn get(mask: &Mask, region: usize) -> bool {
    mask[region / 64] & (1 << (region % 64)) != 0
}

/// Inclusive bounds. Sibling cells partition integer points with no overlap.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Rect {
    min_x: i64,
    max_x: i64,
    min_y: i64,
    max_y: i64,
}

impl Rect {
    const WORLD: Self = Self {
        min_x: -HALF_TURN,
        max_x: HALF_TURN,
        min_y: -HALF_TURN / 2,
        max_y: HALF_TURN / 2,
    };

    fn from_bbox(bbox: &BboxInt) -> Option<Self> {
        let rect = Self {
            min_x: i64::from(bbox.min_lon),
            max_x: i64::from(bbox.max_lon),
            min_y: i64::from(bbox.min_lat),
            max_y: i64::from(bbox.max_lat),
        };
        rect.valid().then_some(rect)
    }

    fn valid(self) -> bool {
        self.min_x <= self.max_x && self.min_y <= self.max_y
    }

    fn intersection(self, other: Self) -> Option<Self> {
        let rect = Self {
            min_x: self.min_x.max(other.min_x),
            max_x: self.max_x.min(other.max_x),
            min_y: self.min_y.max(other.min_y),
            max_y: self.max_y.min(other.max_y),
        };
        rect.valid().then_some(rect)
    }

    fn union(self, other: Self) -> Self {
        Self {
            min_x: self.min_x.min(other.min_x),
            max_x: self.max_x.max(other.max_x),
            min_y: self.min_y.min(other.min_y),
            max_y: self.max_y.max(other.max_y),
        }
    }

    fn contains(self, x: i64, y: i64) -> bool {
        self.min_x <= x && x <= self.max_x && self.min_y <= y && y <= self.max_y
    }

    fn split(self) -> Option<(Split, Self, Self)> {
        let width = self.max_x - self.min_x;
        let height = self.max_y - self.min_y;
        if width == 0 && height == 0 {
            return None;
        }
        if width >= height {
            let mid = self.min_x + width / 2;
            Some((
                Split::X(mid),
                Self { max_x: mid, ..self },
                Self {
                    min_x: mid + 1,
                    ..self
                },
            ))
        } else {
            let mid = self.min_y + height / 2;
            Some((
                Split::Y(mid),
                Self { max_y: mid, ..self },
                Self {
                    min_y: mid + 1,
                    ..self
                },
            ))
        }
    }
}

enum Split {
    X(i64),
    Y(i64),
}

impl Split {
    fn first(&self, x: i64, y: i64) -> bool {
        match *self {
            Self::X(mid) => x <= mid,
            Self::Y(mid) => y <= mid,
        }
    }
}

enum Position {
    Miss,
    Right { low: i64, high: i64 },
    Touch,
}

impl Segment {
    /// Compare the segment's intersection with latitude y to x. The caller
    /// uses non-horizontal segments and y within the segment's closed span.
    fn x_at_y(&self, x: i64, y: i64) -> Ordering {
        let dy = i128::from(self.y2 - self.y1);
        let projected = i128::from(self.x2 - self.x1) * i128::from(y - self.y1);
        let offset = i128::from(x - self.x1) * dy;
        if dy > 0 {
            projected.cmp(&offset)
        } else {
            offset.cmp(&projected)
        }
    }

    fn position(&self, rect: Rect) -> Position {
        let low = self.y1.min(self.y2);
        let high = self.y1.max(self.y2);
        let bottom = low.max(rect.min_y);
        let top = high.min(rect.max_y);
        if bottom > top || self.x1.max(self.x2) < rect.min_x {
            return Position::Miss;
        }
        if self.x1.min(self.x2) > rect.max_x {
            return Position::Right { low, high };
        }
        if low == high {
            return Position::Touch;
        }
        // Longitude along a non-horizontal segment is affine in latitude.
        // At both ends of the clipped span it must be strictly on one side
        // to discard it. Equality keeps touching edges and vertices.
        if self.x_at_y(rect.min_x, bottom) == Ordering::Less
            && self.x_at_y(rect.min_x, top) == Ordering::Less
        {
            Position::Miss
        } else if self.x_at_y(rect.max_x, bottom) == Ordering::Greater
            && self.x_at_y(rect.max_x, top) == Ordering::Greater
        {
            Position::Right { low, high }
        } else {
            Position::Touch
        }
    }
}

#[derive(Default)]
struct Parity {
    /// Parity at the lowest integer latitude in the part's bounds.
    bottom: bool,
    /// Sorted, unique integer latitudes at which parity toggles.
    changes: Vec<i64>,
}

impl Parity {
    fn at(&self, y: i64) -> bool {
        self.bottom ^ (self.changes.partition_point(|&change| change <= y) % 2 == 1)
    }

    fn clip(&self, rect: Rect) -> Self {
        let start = self.changes.partition_point(|&y| y <= rect.min_y);
        let end = self.changes.partition_point(|&y| y <= rect.max_y);
        Self {
            bottom: self.bottom ^ (start % 2 == 1),
            changes: self.changes[start..end].to_vec(),
        }
    }

    fn xor_interval(&mut self, low: i64, high: i64, rect: Rect) {
        if low == high {
            return;
        }
        self.bottom ^= low <= rect.min_y && rect.min_y < high;
        for y in [low, high] {
            if rect.min_y < y && y <= rect.max_y {
                self.changes.push(y);
            }
        }
    }

    fn cancel_pairs(&mut self) {
        self.changes.sort_unstable();
        let mut kept = 0;
        for i in 0..self.changes.len() {
            let y = self.changes[i];
            if kept != 0 && y == self.changes[kept - 1] {
                kept -= 1;
            } else {
                self.changes[kept] = y;
                kept += 1;
            }
        }
        self.changes.truncate(kept);
    }
}

/// One component of a region's union. A planar geometry contributes all
/// rings to one parity. A periodic geometry contributes a separate component
/// per member and longitude image. A bbox starts with constant true parity.
struct Part {
    region: usize,
    bounds: Rect,
    parity: Parity,
    segments: Vec<Segment>,
}

impl Part {
    fn clip(&self, cell: Rect) -> Option<Self> {
        let bounds = self.bounds.intersection(cell)?;
        Some(Self::project(
            self.region,
            bounds,
            self.parity.clip(bounds),
            self.segments.iter().copied(),
        ))
    }

    fn project(
        region: usize,
        bounds: Rect,
        mut parity: Parity,
        edges: impl Iterator<Item = Segment>,
    ) -> Self {
        let mut segments = Vec::new();
        for segment in edges {
            match segment.position(bounds) {
                Position::Touch => segments.push(segment),
                Position::Right { low, high } => parity.xor_interval(low, high, bounds),
                Position::Miss => {}
            }
        }
        parity.cancel_pairs();
        Self {
            region,
            bounds,
            parity,
            segments,
        }
    }

    fn constant(&self) -> Option<bool> {
        (self.segments.is_empty() && self.parity.changes.is_empty()).then_some(self.parity.bottom)
    }

    fn contains(&self, x: i64, y: i64) -> bool {
        self.bounds.contains(x, y) && Segment::contains(&self.segments, x, y, self.parity.at(y))
    }
}

struct Parts {
    inside: Mask,
    boundary: Vec<Part>,
}

impl Parts {
    fn clip(&self, cell: Rect) -> Self {
        Self::resolve(
            cell,
            self.inside,
            self.boundary.iter().filter_map(|p| p.clip(cell)),
        )
    }

    fn resolve(cell: Rect, mut inside: Mask, parts: impl Iterator<Item = Part>) -> Self {
        let mut boundary = Vec::new();
        for part in parts {
            if get(&inside, part.region) {
                continue;
            }
            match part.constant() {
                Some(false) => {}
                Some(true) if part.bounds == cell => set(&mut inside, part.region),
                _ => boundary.push(part),
            }
        }
        boundary.retain(|part| !get(&inside, part.region));
        Self { inside, boundary }
    }

    fn small(&self) -> bool {
        self.boundary
            .iter()
            .map(|p| p.segments.len().max(1))
            .sum::<usize>()
            <= REGION_SUBDIVISION_LEAF_TESTS
    }

    fn work(&self) -> usize {
        self.boundary
            .iter()
            .map(|p| 1 + p.segments.len() + p.parity.changes.len())
            .sum()
    }
}

enum Tests {
    Local(Vec<Part>),
    Exact(Box<Mask>),
}

struct Leaf {
    inside: Mask,
    tests: Tests,
}

enum Cell {
    Branch {
        split: Split,
        first: usize,
        second: usize,
    },
    Leaf(Leaf),
}

pub(super) struct Subdivision {
    bounds: Rect,
    cells: Vec<Cell>,
    words: usize,
}

/// Charge both children before descending. The depth bounds the call stack,
/// and each projected part or edge is covered by the work budget. An input
/// edge can introduce at most two parity events per projection.
struct Builder {
    cells: Vec<Cell>,
    cells_left: usize,
    work_left: usize,
}

impl Builder {
    fn new(work_left: usize) -> Self {
        Self {
            cells: Vec::new(),
            cells_left: REGION_SUBDIVISION_MAX_CELLS - 1,
            work_left,
        }
    }

    fn reserve_split(&mut self, parts: &Parts) -> bool {
        let work = 2 * parts.work();
        if self.cells_left < 2 || self.work_left < work {
            return false;
        }
        self.work_left -= work;
        self.cells_left -= 2;
        true
    }

    fn cell(&mut self, bounds: Rect, parts: Parts, depth: usize) -> usize {
        let small = parts.small();
        let stop = parts.boundary.is_empty() || (small && depth >= REGION_SUBDIVISION_MIN_DEPTH);
        let cell = if !stop
            && depth < REGION_SUBDIVISION_MAX_DEPTH
            && let Some((split, first_bounds, second_bounds)) = bounds.split()
            && self.reserve_split(&parts)
        {
            let first_parts = parts.clip(first_bounds);
            let second_parts = parts.clip(second_bounds);
            drop(parts);
            let first = self.cell(first_bounds, first_parts, depth + 1);
            let second = self.cell(second_bounds, second_parts, depth + 1);
            Cell::Branch {
                split,
                first,
                second,
            }
        } else {
            let tests = if small {
                Tests::Local(parts.boundary)
            } else {
                let mut candidates = Mask::default();
                for part in &parts.boundary {
                    set(&mut candidates, part.region);
                }
                Tests::Exact(Box::new(candidates))
            };
            Cell::Leaf(Leaf {
                inside: parts.inside,
                tests,
            })
        };
        let index = self.cells.len();
        self.cells.push(cell);
        index
    }
}

struct Sources {
    parts: Vec<Part>,
    work_left: usize,
}

impl Sources {
    fn new() -> Self {
        Self {
            parts: Vec::new(),
            work_left: REGION_SUBDIVISION_MAX_WORK,
        }
    }

    fn bbox(&mut self, region: usize, bounds: Rect) -> Option<()> {
        self.work_left = self.work_left.checked_sub(1)?;
        self.parts.push(Part {
            region,
            bounds,
            parity: Parity {
                bottom: true,
                changes: Vec::new(),
            },
            segments: Vec::new(),
        });
        Some(())
    }

    fn rings(&mut self, region: usize, rings: &Rings, shift: i64, clip: Rect) -> Option<()> {
        let envelope = Rect {
            min_x: rings.min_x - shift,
            max_x: rings.max_x - shift,
            min_y: rings.min_y,
            max_y: rings.max_y,
        };
        if let Some(bounds) = envelope.intersection(clip) {
            self.work_left = self.work_left.checked_sub(1 + rings.segments.len())?;
            let edges = rings.segments.iter().map(|s| Segment {
                x1: s.x1 - shift,
                x2: s.x2 - shift,
                ..*s
            });
            self.parts
                .push(Part::project(region, bounds, Parity::default(), edges));
        }
        Some(())
    }

    fn polygon(&mut self, region: usize, polygon: &Polygon, clip: Rect) -> Option<()> {
        let Some(clip) = clip.intersection(Rect::WORLD) else {
            return Some(());
        };
        for geometry in &polygon.geometries {
            match geometry {
                Geometry::Planar(rings) => self.rings(region, rings, 0, clip)?,
                Geometry::Periodic(periodic) => {
                    for rings in &periodic.members {
                        for shift in [0, TURN, -TURN] {
                            self.rings(region, rings, shift, clip)?;
                        }
                    }
                }
            }
        }
        Some(())
    }

    fn build(self, regions: usize) -> Option<Subdivision> {
        let bounds = self.parts.iter().map(|p| p.bounds).reduce(Rect::union)?;
        let root = Parts::resolve(bounds, Mask::default(), self.parts.into_iter());
        let mut builder = Builder::new(self.work_left);
        builder.cell(bounds, root, 0);
        Some(Subdivision {
            bounds,
            cells: builder.cells,
            words: mask_words(regions),
        })
    }
}

impl Subdivision {
    pub(super) fn for_polygon(polygon: &Polygon) -> Option<Self> {
        let mut sources = Sources::new();
        sources.polygon(0, polygon, Rect::WORLD)?;
        sources.build(1)
    }

    fn for_regions(regions: &[Region], exact: &[BboxInt]) -> Option<Self> {
        if regions.len() > MAX_EXTRACTS {
            return None;
        }
        let mut sources = Sources::new();
        for (region, (geometry, bounds)) in regions.iter().zip(exact).enumerate() {
            if let Some(bounds) = Rect::from_bbox(bounds) {
                match geometry {
                    Region::Bbox(_) => sources.bbox(region, bounds)?,
                    Region::Polygon(polygon) => sources.polygon(region, polygon, bounds)?,
                }
            }
        }
        sources.build(regions.len())
    }

    fn leaf(&self, x: i64, y: i64) -> Option<&Leaf> {
        if !self.bounds.contains(x, y) {
            return None;
        }
        let mut at = self.cells.len() - 1;
        loop {
            match &self.cells[at] {
                Cell::Branch {
                    split,
                    first,
                    second,
                } => {
                    at = if split.first(x, y) { *first } else { *second };
                }
                Cell::Leaf(leaf) => return Some(leaf),
            }
        }
    }

    pub(super) fn contains(&self, x: i64, y: i64, exact: impl FnOnce() -> bool) -> bool {
        self.leaf(x, y).is_some_and(|leaf| {
            get(&leaf.inside, 0)
                || match &leaf.tests {
                    Tests::Local(parts) => parts.iter().any(|part| part.contains(x, y)),
                    Tests::Exact(candidates) => get(candidates, 0) && exact(),
                }
        })
    }

    fn visit(
        &self,
        x: i64,
        y: i64,
        admit: Option<&[u64]>,
        exact: impl Fn(usize) -> bool,
        hit: impl FnMut(usize),
    ) {
        let Some(leaf) = self.leaf(x, y) else {
            return;
        };
        let mut inside = leaf.inside;
        restrict(&mut inside[..self.words], admit);
        match &leaf.tests {
            Tests::Local(parts) => {
                for part in parts {
                    if !get(&inside, part.region)
                        && admits(admit, part.region)
                        && part.contains(x, y)
                    {
                        set(&mut inside, part.region);
                    }
                }
            }
            Tests::Exact(candidates) => {
                for region in regions_of(&candidates[..self.words]) {
                    if admits(admit, region) && exact(region) {
                        set(&mut inside, region);
                    }
                }
            }
        }
        regions_of(&inside[..self.words]).for_each(hit);
    }
}

/// The sorted multi-extract node classifier, built once before scanning nodes.
pub(in crate::commands::extract) struct RegionClassifier<'a> {
    regions: &'a [Region],
    exact: &'a [BboxInt],
    subdivision: Option<Subdivision>,
    grid: Option<RegionGrid>,
}

impl<'a> RegionClassifier<'a> {
    pub(in crate::commands::extract) fn new(
        regions: &'a [Region],
        exact: &'a [BboxInt],
        host: &Entered<'_>,
    ) -> Self {
        assert_eq!(regions.len(), exact.len());
        // Drive the original linear predicate for byte-for-byte fault tests.
        #[cfg(pbfhogg_test_hooks)]
        if host.invocation.faults().forces_linear_region_grid() {
            return Self {
                regions,
                exact,
                subdivision: None,
                grid: None,
            };
        }
        let subdivision = Subdivision::for_regions(regions, exact);
        let grid = if subdivision.is_none() && regions.len() >= GRID_REGION_THRESHOLD {
            let boxes: Vec<_> = exact
                .iter()
                .map(|b| (b.min_lat, b.max_lat, b.min_lon, b.max_lon))
                .collect();
            let grid = RegionGrid::build(&boxes);
            if grid.is_none() {
                crate::host::diag!(
                    host.diagnostics,
                    Note,
                    "extract.multi.grid_fallback",
                    "Multi-extract region grid declined {} regions; classifying nodes against every region",
                    regions.len()
                );
                host.invocation.counter("multi_extract_grid_fallback", 1);
            }
            grid
        } else {
            None
        };
        Self {
            regions,
            exact,
            subdivision,
            grid,
        }
    }

    /// Visit matching regions once each, in ascending region order.
    pub(in crate::commands::extract) fn visit(
        &self,
        lat: i32,
        lon: i32,
        admit: Option<&[u64]>,
        mut hit: impl FnMut(usize),
    ) {
        let (x, y) = (i64::from(lon), i64::from(lat));
        let exact = |r: usize| {
            self.exact[r].contains(lat, lon)
                && match &self.regions[r] {
                    Region::Bbox(_) => true,
                    Region::Polygon(polygon) => polygon.contains_exact(x, y),
                }
        };
        if let Some(subdivision) = &self.subdivision {
            subdivision.visit(x, y, admit, exact, hit);
        } else if let Some(grid) = &self.grid {
            for &candidate in grid.candidates(lat, lon) {
                let region = candidate as usize;
                if admits(admit, region) && exact(region) {
                    hit(region);
                }
            }
        } else {
            for region in 0..self.regions.len() {
                if admits(admit, region) && exact(region) {
                    hit(region);
                }
            }
        }
    }

    /// Keep the existing dense-column kernels for bbox-only fallback runs.
    pub(in crate::commands::extract) fn collect_bbox_columns(
        &self,
        columns: &DenseNodeColumns,
        bboxes: &[(i32, i32, i32, i32)],
        out: &mut [Vec<i64>],
    ) {
        if self.subdivision.is_some() {
            for i in 0..columns.len() {
                self.visit(columns.lats[i], columns.lons[i], None, |r| {
                    out[r].push(columns.ids[i]);
                });
            }
        } else if let Some(grid) = &self.grid {
            columns.collect_matching_ids_multi_bbox_grid(bboxes, grid, out);
        } else {
            columns.collect_matching_ids_multi_bbox(bboxes, out);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::coord_fmt::from_decimicro;
    use crate::geo::Bbox;

    use super::super::PolygonRings;

    fn fixed_ring(points: &[(i64, i64)]) -> Vec<(f64, f64)> {
        let degrees = |v| from_decimicro(i32::try_from(v).unwrap());
        points
            .iter()
            .map(|&(x, y)| (degrees(x), degrees(y)))
            .collect()
    }

    fn square(x: i64, y: i64, width: i64) -> Vec<(f64, f64)> {
        fixed_ring(&[
            (x, y),
            (x + width, y),
            (x + width, y + width),
            (x, y + width),
        ])
    }

    fn random(state: &mut u64, low: i64, high: i64) -> i64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        low + i64::try_from(*state >> 1).unwrap() % (high - low + 1)
    }

    fn check(polygon: &Polygon, x: i64, y: i64) {
        assert_eq!(
            polygon.contains_fix(x, y),
            polygon.contains_exact(x, y),
            "point ({x}, {y})"
        );
    }

    fn check_edges(polygon: &Polygon) {
        let probe = |rings: &Rings, shift| {
            for edge in &rings.segments {
                for i in 0..=8 {
                    let x = ((8 - i) * edge.x1 + i * edge.x2).div_euclid(8) - shift;
                    let y = ((8 - i) * edge.y1 + i * edge.y2).div_euclid(8);
                    for dx in -1..=1 {
                        for dy in -1..=1 {
                            check(polygon, x + dx, y + dy);
                        }
                    }
                }
            }
        };
        for geometry in &polygon.geometries {
            match geometry {
                Geometry::Planar(rings) => probe(rings, 0),
                Geometry::Periodic(periodic) => {
                    for rings in &periodic.members {
                        for shift in [0, TURN, -TURN] {
                            probe(rings, shift);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn right_parity_can_change_within_a_boundary_leaf() {
        let rings = Rings::new(&[(true, vec![(0, 0), (10, 5), (10, 10), (0, 0)])]);
        let mut sources = Sources::new();
        sources.rings(0, &rings, 0, Rect::WORLD).unwrap();
        let rect = Rect {
            min_x: 0,
            max_x: 2,
            min_y: 0,
            max_y: 10,
        };
        let part = sources.parts[0].clip(rect).unwrap();
        assert_eq!(part.segments.len(), 2);
        assert_eq!(part.parity.changes, [5, 10]);
        for y in 0..=10 {
            for x in 0..=2 {
                assert_eq!(part.contains(x, y), rings.contains(x, y), "({x}, {y})");
            }
        }
    }

    #[test]
    fn prepared_complex_polygon_matches_bands() {
        let mut state = 0xA310_15E9_02E2_9BBB;
        let mut vertices = vec![(0, -2_000_000), (4_000_000, -2_000_000)];
        for i in (0..400).rev() {
            vertices.push((i * 10_000, random(&mut state, 1_000_000, 4_000_000)));
        }
        let polygon = Polygon::new(&[vec![PolygonRings {
            exterior: fixed_ring(&vertices),
            holes: vec![square(500_000, -800_000, 600_000)],
        }]])
        .unwrap();
        check_edges(&polygon);
        for _ in 0..20_000 {
            let x = random(&mut state, -1_000_000, 5_000_000);
            let y = random(&mut state, -3_000_000, 5_000_000);
            check(&polygon, x, y);
        }
        let index = polygon.subdivision.get().unwrap().as_ref().unwrap();
        assert!(
            index
                .leaf(2_500_000, 0)
                .is_some_and(|leaf| get(&leaf.inside, 0))
        );
        assert!(index.cells.iter().any(|cell| {
            matches!(cell, Cell::Leaf(Leaf { tests: Tests::Local(parts), .. })
                if parts.iter().any(|part| !part.segments.is_empty()))
        }));
    }

    #[test]
    fn prepared_planar_members_and_feature_union_match_bands() {
        let polygon = Polygon::new(&[
            vec![
                PolygonRings {
                    exterior: square(0, 0, 64),
                    holes: vec![square(8, 8, 16), square(60, 16, 16), square(92, 18, 16)],
                },
                PolygonRings {
                    exterior: square(32, 32, 64),
                    holes: vec![],
                },
            ],
            vec![PolygonRings {
                exterior: fixed_ring(&[(48, 40), (64, 56), (32, 56), (48, 40), (24, 24), (64, 24)]),
                holes: vec![],
            }],
        ])
        .unwrap();
        check_edges(&polygon);
        for y in -2..=98 {
            for x in -2..=98 {
                check(&polygon, x, y);
            }
        }
    }

    #[test]
    fn prepared_ring_quantized_to_one_point() {
        let polygon = Polygon::new(&[vec![PolygonRings {
            exterior: vec![(0.0, 0.0), (0.000_000_04, 0.0), (0.0, 0.000_000_04)],
            holes: vec![],
        }]])
        .unwrap();
        for y in -1..=1 {
            for x in -1..=1 {
                check(&polygon, x, y);
            }
        }
        assert!(polygon.contains_fix(0, 0));
    }

    #[test]
    fn prepared_periodic_members_match_bands() {
        let polygon = Polygon::new(&[vec![
            PolygonRings {
                exterior: vec![(178.0, 10.0), (-178.0, 10.0), (-178.0, 16.0), (178.0, 16.0)],
                holes: vec![vec![
                    (-179.0, 11.0),
                    (179.0, 11.0),
                    (179.0, 14.0),
                    (-179.0, 14.0),
                ]],
            },
            PolygonRings {
                exterior: vec![
                    (-180.0, 12.0),
                    (-177.0, 12.0),
                    (-176.0, 15.0),
                    (-180.0, 15.0),
                ],
                holes: vec![],
            },
        ]])
        .unwrap();
        check_edges(&polygon);
        let mut state = 0xF0A1_3C49_2C18_A4A2;
        for _ in 0..10_000 {
            let x = random(&mut state, HALF_TURN - 50_000_000, HALF_TURN + 1);
            let y = random(&mut state, 85_000_000, 170_000_000);
            check(&polygon, x, y);
            check(&polygon, -x, y);
        }
        for y in (90_000_000..=170_000_000).step_by(250_000) {
            check(&polygon, -HALF_TURN, y);
            check(&polygon, HALF_TURN, y);
        }
    }

    #[test]
    fn unresolved_leaf_uses_original_endpoint_and_crossing_test() {
        // Even coincident copies cancel in the interior, while their shared
        // vertices are inside. No depth can split the coincident edges apart.
        let members = (0..12)
            .map(|_| PolygonRings {
                exterior: square(0, 0, 32),
                holes: vec![],
            })
            .collect();
        let polygon = Polygon::new(&[members]).unwrap();
        let index = Subdivision::for_polygon(&polygon).unwrap();
        let mut fell_back = false;
        assert!(index.contains(0, 0, || {
            fell_back = true;
            polygon.contains_exact(0, 0)
        }));
        assert!(fell_back);
        assert!(!index.contains(16, 16, || panic!("interior must have a proof")));
        check_edges(&polygon);
    }

    #[test]
    fn mixed_regions_visit_once_in_order_and_honor_admission() {
        let bbox = |min, max| {
            Region::Bbox(Bbox {
                min_lon: min,
                max_lon: max,
                min_lat: min,
                max_lat: max,
            })
        };
        let mut regions: Vec<_> = (0..66).map(|_| bbox(-1.0, 1.0)).collect();
        regions[1] = bbox(1.0, -1.0);
        regions[65] = Region::Polygon(
            Polygon::new(&[vec![PolygonRings {
                exterior: square(0, 0, 10_000_000),
                holes: vec![square(2_000_000, 2_000_000, 1_000_000)],
            }]])
            .unwrap(),
        );
        regions.push(bbox(-195.0, -185.0));
        let exact: Vec<_> = regions
            .iter()
            .map(|r| BboxInt::inside_bbox(r.bbox()))
            .collect();
        let index = Subdivision::for_regions(&regions, &exact).unwrap();
        let mut state = 0xFE12_85AF_6435_FF27;
        let given = [0x5500_0000_0000_0017, 7];
        let mut points = vec![
            (0, 0),
            (10_000_000, 10_000_000),
            (-1_900_000_000, -1_900_000_000),
        ];
        for _ in 0..2_000 {
            points.push((
                random(&mut state, -11_000_000, 11_000_000),
                random(&mut state, -11_000_000, 11_000_000),
            ));
        }
        for (x, y) in points {
            let original = |r: usize| {
                exact[r].contains(i32::try_from(y).unwrap(), i32::try_from(x).unwrap())
                    && match &regions[r] {
                        Region::Bbox(_) => true,
                        Region::Polygon(polygon) => polygon.contains_exact(x, y),
                    }
            };
            for admit in [None, Some(given.as_slice())] {
                let expected: Vec<_> = (0..regions.len())
                    .filter(|&r| admits(admit, r) && original(r))
                    .collect();
                let mut hits = Vec::new();
                index.visit(x, y, admit, original, |r| hits.push(r));
                assert_eq!(hits, expected, "point ({x}, {y})");
            }
        }
    }
}
