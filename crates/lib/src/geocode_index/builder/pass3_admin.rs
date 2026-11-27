//! Admin-cell flood fill over prepared planar rings and their padded S2 cover.

use std::borrow::Cow;
use std::collections::VecDeque;

use rustc_hash::FxHashSet;
use s2::cellid::{CellID, NUM_FACES};
use s2::latlng::LatLng;

use super::super::admin::AssembledPolygon;
use super::super::cover::cover_path_raw;
use super::{AdminCellEntry, parse_polygon_rings};

pub(super) fn admin_cells_for_polygon(
    poly_idx: usize,
    poly: &AssembledPolygon,
    level: u8,
) -> Vec<AdminCellEntry> {
    let (outer_ring, hole_rings) = parse_polygon_rings(poly.vertices.iter().copied());
    if outer_ring.len() < 3 {
        return Vec::new();
    }
    #[expect(
        clippy::expect_used,
        reason = "the builder refuses an admin-polygon count over u32::MAX before cell assignment"
    )]
    let poly_index = u32::try_from(poly_idx).expect("admin count checked before cell assignment");

    let mut edge_cells = FxHashSet::default();
    let exterior = PreparedRing::new(&outer_ring, level, &mut edge_cells);
    drop(outer_ring);
    let holes: Vec<_> = hole_rings
        .into_iter()
        .map(|ring| PreparedRing::new(&ring, level, &mut edge_cells))
        .collect();
    let mut entries: Vec<_> = edge_cells
        .iter()
        .map(|&cell_id| AdminCellEntry {
            cell_id,
            poly_index,
            is_interior: false,
        })
        .collect();

    let mut classifier = CellClassifier {
        exterior: &exterior,
        holes: &holes,
        edge_cells: &edge_cells,
        subtrees: None,
    };
    let mut visited = FxHashSet::default();
    let mut queue = VecDeque::new();
    let mut enqueue = |cell: CellID, queue: &mut VecDeque<CellID>| {
        if visited.insert(cell.0) && classifier.classify(cell) == CellClass::Interior {
            queue.push_back(cell);
        }
    };

    // Keep the original edge iteration and inside-neighbor order. Testing at
    // discovery avoids re-testing seeds and keeps exterior and boundary cells
    // out of the queue. Those cells never expanded in the original flood.
    for &id in &edge_cells {
        for seed in CellID(id).all_neighbors(u64::from(level)) {
            enqueue(seed, &mut queue);
        }
    }
    while let Some(cell) = queue.pop_front() {
        entries.push(AdminCellEntry {
            cell_id: cell.0,
            poly_index,
            is_interior: true,
        });
        for neighbor in cell.all_neighbors(u64::from(level)) {
            enqueue(neighbor, &mut queue);
        }
    }
    entries
}

struct PreparedRing {
    edges: Vec<RayEdge>,
    lat_min: f64,
    lat_max: f64,
    lon_min: f64,
    lon_max: f64,
    retry_longitude: bool,
}

impl PreparedRing {
    fn new(ring: &[(f64, f64)], level: u8, cells: &mut FxHashSet<u64>) -> Self {
        let frame = crate::geo::pip_ring_frame(ring);
        let mut prepared = Self {
            edges: Vec::with_capacity(frame.len()),
            lat_min: 90.0,
            lat_max: -90.0,
            lon_min: f64::INFINITY,
            lon_max: f64::NEG_INFINITY,
            retry_longitude: matches!(frame, Cow::Owned(_)),
        };
        let next = frame.iter().copied().cycle().skip(1);
        for (previous, current) in frame.iter().copied().zip(next) {
            let (prev_lon, prev_lat) = previous;
            let (lon, lat) = current;
            prepared.lat_min = prepared.lat_min.min(lat);
            prepared.lat_max = prepared.lat_max.max(lat);

            // Cover in forward ring order, including the implicit closing
            // edge. The path cover's padding also covers points assigned to
            // a cell by S2 rounding. Preserve its exact emissions.
            cover_path_raw((prev_lat, prev_lon), (lat, lon), level, |cell| {
                cells.insert(cell);
            });
            if let Some(edge) = RayEdge::new(current, previous) {
                // Bound the *rounded* crossing longitude. The expression is
                // monotone in latitude, so its values at the two endpoints
                // enclose the values used by the ray test.
                let other_lon = edge.crossing_lon(prev_lat);
                prepared.lon_min = prepared.lon_min.min(lon).min(other_lon);
                prepared.lon_max = prepared.lon_max.max(lon).max(other_lon);
                prepared.edges.push(edge);
            }
        }
        if prepared.retry_longitude {
            // The three longitude retries can change answer at +/-180 even
            // where no ring edge runs. Include the same seam as ADR-0022.
            cover_path_raw(
                (prepared.lat_min, 180.0),
                (prepared.lat_max, 180.0),
                level,
                |cell| {
                    cells.insert(cell);
                },
            );
        }
        prepared
    }

    fn contains(&self, lon: f64, lat: f64) -> bool {
        self.lat_min <= lat
            && lat < self.lat_max
            && (self.contains_in_frame(lon, lat)
                || (self.retry_longitude
                    && (self.contains_in_frame(lon + 360.0, lat)
                        || self.contains_in_frame(lon - 360.0, lat))))
    }

    fn contains_in_frame(&self, lon: f64, lat: f64) -> bool {
        // West of all crossings the ray crosses an even number of edges:
        // the closed ring returns to the latitude side it started on.
        if lon < self.lon_min || lon >= self.lon_max {
            return false;
        }
        self.edges
            .iter()
            .fold(false, |inside, edge| inside ^ edge.crosses_ray(lon, lat))
    }
}

struct RayEdge {
    lon: f64,
    lat: f64,
    d_lon: f64,
    d_lat: f64,
    lat_min: f64,
    lat_max: f64,
}

impl RayEdge {
    fn new((lon, lat): (f64, f64), (prev_lon, prev_lat): (f64, f64)) -> Option<Self> {
        let lat_min = lat.min(prev_lat);
        let lat_max = lat.max(prev_lat);
        (lat_min < lat_max).then_some(Self {
            lon,
            lat,
            d_lon: prev_lon - lon,
            d_lat: prev_lat - lat,
            lat_min,
            lat_max,
        })
    }

    fn crossing_lon(&self, lat: f64) -> f64 {
        // Match geo::point_in_ring's edge direction and operation order.
        // A precomputed slope would change rounding near an edge.
        self.d_lon * (lat - self.lat) / self.d_lat + self.lon
    }

    fn crosses_ray(&self, lon: f64, lat: f64) -> bool {
        self.lat_min <= lat && lat < self.lat_max && lon < self.crossing_lon(lat)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CellClass {
    Boundary,
    Interior,
    Exterior,
}

struct CellClassifier<'a> {
    exterior: &'a PreparedRing,
    holes: &'a [PreparedRing],
    edge_cells: &'a FxHashSet<u64>,
    subtrees: Option<SubtreeCache>,
}

impl CellClassifier<'_> {
    fn classify(&mut self, cell: CellID) -> CellClass {
        if self.edge_cells.contains(&cell.0) {
            return CellClass::Boundary;
        }
        let exterior = self.exterior;
        let holes = self.holes;
        let test_center = || {
            let center = LatLng::from(cell);
            let (lon, lat) = (center.lng.deg(), center.lat.deg());
            exterior.contains(lon, lat) && !holes.iter().any(|hole| hole.contains(lon, lat))
        };

        let inside = if let Some(subtrees) = &mut self.subtrees {
            subtrees.contains(cell, test_center)
        } else if !test_center() {
            false
        } else if cell.is_face() {
            true
        } else {
            // Most tiny polygons have only boundary entries. Create the
            // hierarchy only once there is an interior to flood.
            self.subtrees
                .insert(SubtreeCache::new(self.edge_cells))
                .contains(cell, || true)
        };
        if inside {
            CellClass::Interior
        } else {
            CellClass::Exterior
        }
    }
}

#[derive(Clone, Copy)]
enum Subtree {
    Unclassified,
    Uniform(bool),
    Children(usize),
}

struct SubtreeCache {
    boundary: Vec<u64>,
    nodes: Vec<Subtree>,
}

impl SubtreeCache {
    fn new(edge_cells: &FxHashSet<u64>) -> Self {
        let mut boundary: Vec<_> = edge_cells.iter().copied().collect();
        boundary.sort_unstable();
        Self {
            boundary,
            nodes: vec![Subtree::Unclassified; usize::from(NUM_FACES)],
        }
    }

    /// For a target cell outside the padded boundary cover, find the largest
    /// ancestor with no boundary descendants. The descendants tile the
    /// ancestor, so a boundary through it would hit a covered descendant.
    /// One target-level center decides the whole edge-free range.
    fn contains(&mut self, target: CellID, test_center: impl FnOnce() -> bool) -> bool {
        let mut index = usize::from(target.face());
        let mut level = 0;
        loop {
            let children = match self.nodes[index] {
                Subtree::Uniform(inside) => return inside,
                Subtree::Children(children) => children,
                Subtree::Unclassified => {
                    let parent = target.parent(level);
                    if parent == target || !self.has_boundary(parent) {
                        let inside = test_center();
                        self.nodes[index] = Subtree::Uniform(inside);
                        return inside;
                    }
                    let children = self.nodes.len();
                    self.nodes.extend([Subtree::Unclassified; 4]);
                    self.nodes[index] = Subtree::Children(children);
                    children
                }
            };
            level += 1;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "S2 child position is 0..=3"
            )]
            let position = target.child_position(level) as usize;
            index = children + position;
        }
    }

    fn has_boundary(&self, cell: CellID) -> bool {
        let lo = cell.range_min().0;
        let hi = cell.range_max().0;
        let first = self.boundary.partition_point(|&id| id < lo);
        self.boundary.get(first).is_some_and(|&id| id <= hi)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{NodeCoord, RING_SENTINEL, admin_cells_for_polygon_oracle};
    use super::*;

    fn polygon(origin: (i32, i32), step: i32, rings: &[&[(i32, i32)]]) -> AssembledPolygon {
        let mut vertices = Vec::new();
        let half_turn = 1_800_000_000_i64;
        for (i, ring) in rings.iter().enumerate() {
            if i > 0 {
                vertices.push(RING_SENTINEL);
            }
            for &(x, y) in *ring {
                let lon = i64::from(origin.0) + i64::from(step) * i64::from(x);
                let wrapped = (lon + half_turn).rem_euclid(2 * half_turn) - half_turn;
                vertices.push(NodeCoord {
                    lon_e7: i32::try_from(wrapped).expect("wrapped test longitude"),
                    lat_e7: origin.1 + step * y,
                });
            }
        }
        AssembledPolygon {
            vertices,
            admin_level: 8,
            name_offset: 0,
            country_code_offset: 0,
            area: 1.0,
        }
    }

    fn generated_polygons() -> Vec<(&'static str, AssembledPolygon, u8)> {
        let square = &[(-4, -4), (4, -4), (4, 4), (-4, 4)];
        let hole = &[(-2, -1), (1, -2), (2, 2), (-2, 2), (-2, -1)];
        let notch = &[
            (-5, -4),
            (5, -4),
            (5, 5),
            (2, 5),
            (2, -1),
            (-2, -1),
            (-2, 5),
            (-5, 5),
        ];
        let bow_tie = &[(-5, -4), (5, 4), (-5, 4), (5, -4)];
        let winding = &[
            (170, 0),
            (-70, 30),
            (50, 60),
            (170, 60),
            (-70, 60),
            (50, 30),
            (170, 0),
        ];
        vec![
            (
                "hole",
                polygon((125_123_456, 550_500_000), 5_000_000, &[square, hole]),
                8,
            ),
            (
                "concave",
                polygon((70_000_000, -150_000_000), 7_000_000, &[notch]),
                8,
            ),
            (
                "self-intersection",
                polygon((30_100_000, 250_000_000), 8_000_000, &[bow_tie]),
                8,
            ),
            (
                "tiny",
                polygon((155_123_456, 241_234_567), 5, &[square]),
                10,
            ),
            (
                "huge",
                polygon((100_000_000, 80_000_000), 100_000_000, &[square, hole]),
                7,
            ),
            (
                "east seam",
                polygon((1_795_000_000, -300_000_000), 6_000_000, &[square, hole]),
                8,
            ),
            (
                "west seam",
                polygon((-1_795_000_000, 300_000_000), 6_000_000, &[notch, hole]),
                8,
            ),
            ("winding seam", polygon((0, 0), 10_000_000, &[winding]), 4),
        ]
    }

    #[test]
    fn prepared_admin_flood_matches_ordered_oracle() {
        for (index, (name, polygon, level)) in generated_polygons().into_iter().enumerate() {
            let new = admin_cells_for_polygon(index, &polygon, level);
            let old = admin_cells_for_polygon_oracle(index, &polygon, level);
            assert!(
                old.iter().any(|entry| !entry.is_interior),
                "{name} has no edge cells"
            );
            assert_eq!(
                old.iter().any(|entry| entry.is_interior),
                name != "tiny",
                "{name} has unexpected interior coverage"
            );
            assert_eq!(new.len(), old.len(), "{name} entry count");
            for (position, (new, old)) in new.iter().zip(&old).enumerate() {
                assert_eq!(
                    (new.cell_id, new.poly_index, new.is_interior),
                    (old.cell_id, old.poly_index, old.is_interior),
                    "{name} entry {position}"
                );
            }
        }
    }

    #[test]
    fn prepared_cells_match_the_padded_cover_and_legacy_center_test() {
        let level = 5;
        for (name, polygon, _) in generated_polygons() {
            let (exterior, holes) = parse_polygon_rings(polygon.vertices.iter().copied());
            let hole_slices: Vec<_> = holes.iter().map(Vec::as_slice).collect();
            let mut old_edges = FxHashSet::default();
            for ring in std::iter::once(&exterior).chain(&holes) {
                super::super::cover_pip_ring_boundary(ring, level, &mut old_edges);
            }

            let mut edge_cells = FxHashSet::default();
            let prepared_exterior = PreparedRing::new(&exterior, level, &mut edge_cells);
            let prepared_holes: Vec<_> = holes
                .iter()
                .map(|ring| PreparedRing::new(ring, level, &mut edge_cells))
                .collect();
            let mut classifier = CellClassifier {
                exterior: &prepared_exterior,
                holes: &prepared_holes,
                edge_cells: &edge_cells,
                subtrees: None,
            };
            for face in 0..u64::from(NUM_FACES) {
                for cell in CellID::from_face(face).child_iter_at_level(u64::from(level)) {
                    let expected = if old_edges.contains(&cell.0) {
                        CellClass::Boundary
                    } else {
                        let center = LatLng::from(cell);
                        if crate::geo::point_in_polygon(
                            center.lng.deg(),
                            center.lat.deg(),
                            &exterior,
                            &hole_slices,
                        ) {
                            CellClass::Interior
                        } else {
                            CellClass::Exterior
                        }
                    };
                    assert_eq!(classifier.classify(cell), expected, "{name} cell {cell}");
                }
            }
        }
    }

    #[test]
    fn prepared_ray_keeps_rounded_crossing_and_endpoint_rules() {
        for (name, polygon, _) in generated_polygons() {
            let (exterior, holes) = parse_polygon_rings(polygon.vertices.iter().copied());
            for ring in std::iter::once(&exterior).chain(&holes) {
                let frame = crate::geo::pip_ring_frame(ring);
                let prepared = PreparedRing::new(ring, 3, &mut FxHashSet::default());
                for edge in &prepared.edges {
                    for lat in [
                        edge.lat_min,
                        edge.lat_min.midpoint(edge.lat_max),
                        edge.lat_max,
                    ] {
                        let x = edge.crossing_lon(lat);
                        for lon in [x.next_down(), x, x.next_up()] {
                            assert_eq!(
                                prepared.contains_in_frame(lon, lat),
                                crate::geo::point_in_ring(lon, lat, &frame),
                                "{name} point ({lon}, {lat})"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn uniform_subtrees_reuse_one_target_center() {
        let [boundary, interior, exterior, _] = CellID::from_face(0).children();
        let edges = FxHashSet::from_iter([boundary.child_begin().0]);
        let mut cache = SubtreeCache::new(&edges);
        assert!(cache.contains(interior.child_begin(), || true));
        assert!(cache.contains(interior.children()[3], || false));
        assert!(!cache.contains(exterior.child_begin(), || false));
        assert!(!cache.contains(exterior.children()[3], || true));
    }
}
