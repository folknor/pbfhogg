//! Admin polygon assembly and on-disk writing.

use std::io::Write;
use std::path::Path;

use crate::coord_fmt::{deg_to_e7, e7_to_deg};
use rustc_hash::FxHashMap;
use s2::cellid::CellID;

use super::BuildConfig;
use super::Result;
use super::pass1::RawAdminRelation;
use crate::error::Error;
use crate::geo::RingSearchBudgetExceeded;

use super::super::format::*;

pub(super) struct AssembledPolygon {
    pub(super) admin_level: u8,
    pub(super) name_offset: u32,
    pub(super) country_code_offset: u32,
    pub(super) area: f32,
    pub(super) vertices: Vec<NodeCoord>,
}

/// Relations that produced no polygon. `over_budget` is the subset refused
/// because ring assembly exceeded its step budget; it is included in `total`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct DroppedAdminRelations {
    pub(super) total: usize,
    pub(super) over_budget: usize,
    pub(super) sample_relation_ids: Vec<i64>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct MissingAdminWayGeometry {
    pub(super) member_refs: u64,
    pub(super) sample_way_ids: Vec<i64>,
}

/// Count relation member references whose way geometry was not collected and
/// retain a bounded, stable sample of the missing way IDs.
pub(super) fn missing_admin_way_geometry(
    relations: &[RawAdminRelation],
    way_geom: &FxHashMap<i64, Vec<(i32, i32)>>,
) -> MissingAdminWayGeometry {
    let mut missing = MissingAdminWayGeometry::default();
    for relation in relations {
        for &way_id in relation.outer_way_ids.iter().chain(&relation.inner_way_ids) {
            if !way_geom.contains_key(&way_id) {
                missing.member_refs = missing.member_refs.saturating_add(1);
                super::sample_id(&mut missing.sample_way_ids, way_id);
            }
        }
    }
    missing
}

#[hotpath::measure]
pub(super) fn assemble_admin_polygons(
    relations: &[RawAdminRelation],
    way_geom: &FxHashMap<i64, Vec<(i32, i32)>>,
    config: &BuildConfig,
    executor: &crate::host::Executor,
) -> (Vec<AssembledPolygon>, DroppedAdminRelations) {
    use rayon::prelude::*;

    let tolerance_m = f64::from(config.admin_tolerance_m);

    // Per-relation work is independent - ring assembly, Douglas-Peucker
    // simplification, and hole attachment all read-only against `way_geom`
    // and the relation itself. `par_iter().flat_map_iter().collect()`
    // preserves input order so the output `Vec<AssembledPolygon>` is
    // byte-identical to the previous sequential path. Europe phase was
    // 50.6 s at UUID `bf8f2038` - expected ~5× on plantasjen's 12 cores.
    let per_relation: Vec<std::result::Result<Vec<AssembledPolygon>, RingSearchBudgetExceeded>> =
        executor.install(|| {
            relations
                .par_iter()
                .map(|rel| assemble_one_relation(rel, way_geom, tolerance_m))
                .collect()
        });
    let mut dropped = DroppedAdminRelations::default();
    let mut polygons = Vec::new();
    for (rel, relation) in relations.iter().zip(per_relation) {
        match relation {
            Ok(assembled) if assembled.is_empty() => {
                dropped.total += 1;
                super::sample_id(&mut dropped.sample_relation_ids, rel.id);
            }
            Ok(assembled) => polygons.extend(assembled),
            Err(RingSearchBudgetExceeded) => {
                dropped.total += 1;
                dropped.over_budget += 1;
                super::sample_id(&mut dropped.sample_relation_ids, rel.id);
            }
        }
    }
    (polygons, dropped)
}

/// Decide whether `hole` belongs to the outer ring `outer_f64`.
///
/// Two things this must get right, both of which the previous
/// `point_in_ring(hole[0])` did not:
///
/// 1. **Antimeridian.** Every other point-in-ring call in the codebase goes
///    through `point_in_ring_with_antimeridian`. An admin boundary crossing
///    +/-180 (Russia, Fiji, the Antarctic claims) has a longitude-
///    discontinuous outer ring, and the plain ray cast assigns its holes to
///    the wrong outer or drops them. This runs on planet data, so those
///    relations are present.
///
/// 2. **Boundary vertices.** `hole[0]` for an admin boundary is very often a
///    node shared with the outer ring, i.e. exactly on the boundary, where
///    ray casting is undefined and answers either way. Sampling several
///    vertices spread around the hole and taking the majority means one
///    degenerate vertex no longer decides ownership. Vertices, not a
///    centroid: the centroid of a non-convex hole can fall outside both the
///    hole and its outer.
fn hole_inside_outer(hole: &[(i32, i32)], outer_f64: &[(f64, f64)]) -> bool {
    use crate::tunables::HOLE_OWNERSHIP_SAMPLES;
    let step = (hole.len() / HOLE_OWNERSHIP_SAMPLES).max(1);
    let mut inside = 0_usize;
    let mut tested = 0_usize;
    for &(lat, lon) in hole.iter().step_by(step).take(HOLE_OWNERSHIP_SAMPLES) {
        tested += 1;
        if crate::geo::point_in_ring_with_antimeridian(
            e7_to_deg(lon as f64),
            e7_to_deg(lat as f64),
            outer_f64,
        ) {
            inside += 1;
        }
    }
    tested > 0 && inside * 2 > tested
}

/// Assemble all `AssembledPolygon`s produced by a single relation
/// (one per outer ring, each carrying any inner rings that fall inside
/// it). Pure function - no shared mutable state, safe to call from
/// rayon workers.
///
/// Relation geometry is untrusted input, so ring assembly runs under its step
/// budget. Exceeding it on either the outer or the inner rings refuses the
/// whole relation: a polygon assembled without its holes, or from a partial
/// ring set, would be a silently different boundary.
///
/// Every ring is simplified on its own to `tolerance_m` (`geo::simplify_ring`
/// states the contract). The reader's answer for this polygon is
/// `in outer and not in any hole`, so if each ring keeps its own answer for a
/// point, the polygon does: a point farther than the tolerance from every
/// original ring of the polygon is answered as the unsimplified polygon
/// would answer it. Simplified rings may therefore cross each other or
/// themselves near the boundary; nothing here repairs that, because no
/// answer outside the tolerance depends on it. What must NOT see simplified
/// geometry is the decision which outer owns which hole: it changes the
/// boolean formula itself, deep inside the hole, so it is made on the
/// original rings.
fn assemble_one_relation(
    rel: &RawAdminRelation,
    way_geom: &FxHashMap<i64, Vec<(i32, i32)>>,
    tolerance_m: f64,
) -> std::result::Result<Vec<AssembledPolygon>, RingSearchBudgetExceeded> {
    // Extracts may contain a relation but omit one or more member ways. Only
    // member geometry present in this PBF can close an outer ring; missing
    // geometry is omitted, and the relation contributes no polygon if no
    // usable outer ring remains.
    let outer_segs: Vec<&[(i32, i32)]> = rel
        .outer_way_ids
        .iter()
        .filter_map(|wid| way_geom.get(wid).map(Vec::as_slice))
        .collect();
    let outer_rings = crate::geo::assemble_rings_bounded(&outer_segs)?;
    if outer_rings.is_empty() {
        return Ok(Vec::new());
    }

    let inner_segs: Vec<&[(i32, i32)]> = rel
        .inner_way_ids
        .iter()
        .filter_map(|wid| way_geom.get(wid).map(Vec::as_slice))
        .collect();
    let inner_rings = crate::geo::assemble_rings_bounded(&inner_segs)?;

    let mut result = Vec::with_capacity(outer_rings.len());
    for outer_ring in &outer_rings {
        let outer_f64: Vec<(f64, f64)> = outer_ring
            .iter()
            .map(|&(lat, lon)| (e7_to_deg(lon as f64), e7_to_deg(lat as f64)))
            .collect();

        let simplified = crate::geo::simplify_ring(&outer_f64, tolerance_m);

        if simplified.len() < 3 {
            continue;
        }

        #[allow(clippy::cast_possible_truncation)]
        let area = crate::geo::signed_area(outer_ring).abs() as f32;

        let mut vertices = Vec::new();
        for &(lon_deg, lat_deg) in &simplified {
            #[allow(clippy::cast_possible_truncation)]
            vertices.push(NodeCoord {
                lat_e7: deg_to_e7(lat_deg) as i32,
                lon_e7: deg_to_e7(lon_deg) as i32,
            });
        }

        // Add inner rings (holes) that fall inside this outer ring.
        //
        // Test ownership against the *original* `outer_f64` and the original
        // hole vertices, not simplified ones: simplification moves the
        // boundary by up to the tolerance, so a hole sampled near it could
        // change owner, which changes answers everywhere inside the hole,
        // far beyond the tolerance. The geometry stored in `vertices` is the
        // simplified form.
        for hole in &inner_rings {
            if hole.is_empty() {
                continue;
            }
            if !hole_inside_outer(hole, &outer_f64) {
                continue;
            }

            let hole_f64: Vec<(f64, f64)> = hole
                .iter()
                .map(|&(lat, lon)| (e7_to_deg(lon as f64), e7_to_deg(lat as f64)))
                .collect();
            let sh = crate::geo::simplify_ring(&hole_f64, tolerance_m);

            if sh.len() >= 3 {
                vertices.push(RING_SENTINEL);
                for &(lon_deg, lat_deg) in &sh {
                    #[allow(clippy::cast_possible_truncation)]
                    vertices.push(NodeCoord {
                        lat_e7: deg_to_e7(lat_deg) as i32,
                        lon_e7: deg_to_e7(lon_deg) as i32,
                    });
                }
            }
        }

        result.push(AssembledPolygon {
            admin_level: rel.admin_level,
            name_offset: rel.name_offset,
            country_code_offset: rel.country_code_offset,
            area,
            vertices,
        });
    }
    Ok(result)
}

#[hotpath::measure]
pub(super) fn write_admin_data(dir: &Path, polygons: &[AssembledPolygon]) -> Result<()> {
    let mut poly_out = super::create_data_file(&dir.join(FILE_ADMIN_POLYGONS))?;
    let mut vert_out = super::create_data_file(&dir.join(FILE_ADMIN_VERTICES))?;
    let mut offset: u64 = 0;
    // assemble_admin_polygons obtains only explicitly closed rings from
    // assemble_rings. simplify_ring preserves both endpoints and conversion
    // back to e7 applies the same operation to equal endpoints. Sentinels
    // separate closed exterior/hole rings; serialization preserves them verbatim.
    // No extra vertex validation pass is needed for these producer-owned values.
    for p in polygons {
        let vertex_count = super::checked_count(p.vertices.len(), "admin polygon vertex")?;
        let rec = AdminPolygon {
            area: p.area,
            vertex_offset: offset,
            vertex_count,
            name_offset: p.name_offset,
            country_code_offset: p.country_code_offset,
            admin_level: p.admin_level,
        };
        poly_out.write_all(&rec.to_bytes())?;
        for v in &p.vertices {
            vert_out.write_all(&v.to_bytes())?;
        }
        let step_bytes = p
            .vertices
            .len()
            .checked_mul(NodeCoord::SIZE)
            .ok_or_else(|| {
                Error::invalid_data(format!(
                    "write_admin_data: polygon vertex bytes overflow usize ({} * {}).",
                    p.vertices.len(),
                    NodeCoord::SIZE,
                ))
            })?;
        let step_u64 = u64::try_from(step_bytes).map_err(|_| {
            Error::invalid_data("write_admin_data: polygon vertex bytes exceed u64")
        })?;
        offset = offset.checked_add(step_u64).ok_or_else(|| {
            Error::invalid_data(format!(
                "write_admin_data: cumulative admin-vertex byte offset overflows u64 \
                 (current {offset}, step {step_u64})"
            ))
        })?;
    }
    poly_out.flush()?;
    vert_out.flush()?;
    Ok(())
}

#[hotpath::measure]
pub(super) fn write_admin_index(
    dir: &Path,
    entries: &mut [super::pass3::AdminCellEntry],
) -> Result<u32> {
    entries.sort_unstable_by_key(|e| e.cell_id);
    let mut cell_ids: Vec<u64> = entries.iter().map(|e| e.cell_id).collect();
    cell_ids.sort_unstable();
    cell_ids.dedup();

    let mut entries_out = super::create_data_file(&dir.join(FILE_ADMIN_ENTRIES))?;
    let mut cells_out = super::create_data_file(&dir.join(FILE_ADMIN_CELLS))?;
    let mut byte_off: u64 = 0;
    let mut i = 0;

    for &cid in &cell_ids {
        let start = i;
        while i < entries.len() && entries[i].cell_id == cid {
            i += 1;
        }
        if start == i {
            continue;
        }
        let cell_token = CellID(cid);

        cells_out.write_all(
            &AdminCell {
                cell_id: cid,
                entries_offset: byte_off,
            }
            .to_bytes(),
        )?;

        // INVARIANT: the on-disk list count is `ListCount` (u16), the
        // framing `format::Table::list` reads. Hard-error on overflow rather
        // than silently truncating. If this fires, bump the count to u32 and
        // increment `FORMAT_VERSION`.
        let group_len = i - start;
        let count = ListCount::try_from(group_len).map_err(|_| {
            Error::invalid_data(format!(
                "write_admin_index: admin cell {cell_token} ({cid}) has {group_len} entries, exceeds u16::MAX. \
             Bump on-disk count to u32 and increment FORMAT_VERSION."
            ))
        })?;
        entries_out.write_all(&count.to_le_bytes())?;
        for e in &entries[start..i] {
            // The interior hint ORs into the high bit of poly_index. If
            // `poly_index >= 0x8000_0000` (>2.1 B admin polygons) the OR
            // would silently collide with existing bits and the decoded
            // poly_index would be wrong, so `AdminEntry::new` refuses it.
            // Guard here rather than at the upstream counter.
            let entry = AdminEntry::new(e.poly_index, e.is_interior).ok_or_else(|| {
                Error::invalid_data(format!(
                    "write_admin_index: poly_index {} in cell {cell_token} ({cid}) has the INTERIOR_FLAG bit set; \
                     admin polygon count has exceeded 2^31 - 1. Split INTERIOR_FLAG into a \
                     separate byte or grow AdminEntry to u64 and increment FORMAT_VERSION.",
                    e.poly_index
                ))
            })?;
            entries_out.write_all(&entry.to_bytes())?;
        }
        byte_off = byte_off
            .checked_add(list_byte_len::<AdminEntry>(count))
            .ok_or_else(|| {
                Error::invalid_data(format!(
                    "write_admin_index: admin-entries byte offset overflows u64 at cell {cell_token} ({cid}) \
                 (current {byte_off})"
                ))
            })?;
    }
    cells_out.flush()?;
    entries_out.flush()?;
    super::checked_count(cell_ids.len(), "admin cells")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use crate::coord_fmt::deg_to_e7;
    use rustc_hash::FxHashMap;

    use super::{
        DroppedAdminRelations, assemble_admin_polygons, hole_inside_outer,
        missing_admin_way_geometry,
    };
    use crate::geocode_index::builder::BuildConfig;
    use crate::geocode_index::builder::pass1::RawAdminRelation;

    #[test]
    fn relations_without_assembled_geometry_are_counted() {
        // The MIXED case is what makes this discriminating: an implementation
        // that simply reported `relations.len()`, or one that reported zero,
        // would pass an all-dropped or all-kept fixture. One of each is the
        // only shape that pins the count to the relations that actually
        // failed to assemble.
        let mut way_geom: FxHashMap<i64, Vec<(i32, i32)>> = FxHashMap::default();
        way_geom.insert(
            7,
            vec![
                (550_000_000, 120_000_000),
                (550_000_000, 121_000_000),
                (551_000_000, 121_000_000),
                (551_000_000, 120_000_000),
                (550_000_000, 120_000_000),
            ],
        );

        let relations = vec![
            // Names a way that has no geometry: dropped.
            RawAdminRelation {
                id: 1,
                admin_level: 8,
                name_offset: 1,
                country_code_offset: 0,
                outer_way_ids: vec![99],
                inner_way_ids: Vec::new(),
            },
            // Names no ways at all: dropped.
            RawAdminRelation {
                id: 2,
                admin_level: 8,
                name_offset: 2,
                country_code_offset: 0,
                outer_way_ids: Vec::new(),
                inner_way_ids: Vec::new(),
            },
            // Closed ring: assembles, and must NOT be counted as dropped.
            RawAdminRelation {
                id: 3,
                admin_level: 8,
                name_offset: 3,
                country_code_offset: 0,
                outer_way_ids: vec![7],
                inner_way_ids: Vec::new(),
            },
        ];
        assert_eq!(
            missing_admin_way_geometry(&relations, &way_geom),
            super::MissingAdminWayGeometry {
                member_refs: 1,
                sample_way_ids: vec![99],
            }
        );
        let (polygons, dropped) = assemble_admin_polygons(
            &relations,
            &way_geom,
            &BuildConfig::default(),
            &crate::host::Executor::Ambient,
        );
        assert_eq!(polygons.len(), 1, "the closed ring should assemble");
        assert_eq!(polygons[0].name_offset, 3);
        assert_eq!(
            dropped,
            DroppedAdminRelations {
                total: 2,
                over_budget: 0,
                sample_relation_ids: vec![1, 2],
            },
            "only the two geometry-less relations are dropped"
        );
    }

    /// A relation whose ring search exceeds the budget is refused as a whole
    /// and counted separately, while an ordinary relation beside it still
    /// assembles. The over-budget relation also carries a closed ring that an
    /// unbounded search would have returned; it must not leak out alone.
    #[test]
    fn over_budget_relations_are_refused_and_counted() {
        let mut way_geom: FxHashMap<i64, Vec<(i32, i32)>> = FxHashMap::default();
        // Leaf way first, so the first search runs through the complete graph.
        way_geom.insert(1, vec![(-1, -1), (0, 0)]);
        let mut way_id = 2;
        let mut complete_graph = vec![1];
        for a in 0..12 {
            for b in a + 1..12 {
                way_geom.insert(way_id, vec![(a, a), (b, b)]);
                complete_graph.push(way_id);
                way_id += 1;
            }
        }
        let square = vec![
            (550_000_000, 120_000_000),
            (550_000_000, 121_000_000),
            (551_000_000, 121_000_000),
            (551_000_000, 120_000_000),
            (550_000_000, 120_000_000),
        ];
        way_geom.insert(1_000, square);
        complete_graph.push(1_000);

        let relations = vec![
            RawAdminRelation {
                id: 1,
                admin_level: 8,
                name_offset: 1,
                country_code_offset: 0,
                outer_way_ids: complete_graph,
                inner_way_ids: Vec::new(),
            },
            RawAdminRelation {
                id: 2,
                admin_level: 8,
                name_offset: 2,
                country_code_offset: 0,
                outer_way_ids: vec![1_000],
                inner_way_ids: Vec::new(),
            },
        ];
        let (polygons, dropped) = assemble_admin_polygons(
            &relations,
            &way_geom,
            &BuildConfig::default(),
            &crate::host::Executor::Ambient,
        );
        assert_eq!(polygons.len(), 1);
        assert_eq!(polygons[0].name_offset, 2);
        assert_eq!(
            dropped,
            DroppedAdminRelations {
                total: 1,
                over_budget: 1,
                sample_relation_ids: vec![1],
            }
        );
    }

    /// Build a closed axis-aligned rectangle ring in `(lon, lat)` degrees,
    /// which is the orientation `hole_inside_outer` expects for its outer.
    fn rect_lonlat(lon0: f64, lat0: f64, lon1: f64, lat1: f64) -> Vec<(f64, f64)> {
        vec![
            (lon0, lat0),
            (lon1, lat0),
            (lon1, lat1),
            (lon0, lat1),
            (lon0, lat0),
        ]
    }

    /// Same rectangle as an assembled ring, which carries `(lat_e7, lon_e7)`.
    #[allow(clippy::cast_possible_truncation)]
    fn rect_ring_e7(lon0: f64, lat0: f64, lon1: f64, lat1: f64) -> Vec<(i32, i32)> {
        rect_lonlat(lon0, lat0, lon1, lat1)
            .into_iter()
            .map(|(lon, lat)| (deg_to_e7(lat) as i32, deg_to_e7(lon) as i32))
            .collect()
    }

    /// An outer ring crossing the antimeridian has discontinuous
    /// longitudes, so the plain ray cast puts its holes outside it. Every
    /// other point-in-ring call in the codebase is antimeridian-aware; this
    /// one was not, and it runs on planet data where Russia and Fiji are.
    #[test]
    fn hole_in_an_antimeridian_crossing_outer_is_owned_by_it() {
        // Outer spans 175E to 175W across the dateline.
        let outer = rect_lonlat(175.0, -5.0, -175.0, 5.0);
        let hole = rect_ring_e7(179.0, -1.0, 179.5, 1.0);
        assert!(
            hole_inside_outer(&hole, &outer),
            "a hole inside an antimeridian-crossing outer must be owned by it"
        );

        // Passing counterpart: a hole on the far side of the planet is not
        // owned, so this is not satisfied by a function that always agrees.
        let elsewhere = rect_ring_e7(0.0, -1.0, 0.5, 1.0);
        assert!(!hole_inside_outer(&elsewhere, &outer));
    }

    /// The hole's first vertex is very often a node shared with the outer
    /// ring, i.e. exactly on the boundary, where ray casting is undefined.
    /// Sampling several vertices means one degenerate vertex no longer
    /// decides ownership on its own.
    #[test]
    fn a_hole_touching_the_outer_boundary_is_still_owned() {
        let outer = rect_lonlat(0.0, 0.0, 10.0, 10.0);
        // First vertex sits exactly on the outer's eastern edge, where the
        // ray cast answers "outside"; the rest of the hole is unambiguously
        // inside. Testing hole[0] alone therefore disowns the whole hole.
        let mut hole = vec![(50_000_000_i32, 100_000_000_i32)];
        hole.extend(rect_ring_e7(1.0, 4.0, 3.0, 6.0));
        assert!(
            hole_inside_outer(&hole, &outer),
            "a hole sharing a vertex with the outer boundary must still be \
             owned by it"
        );
    }

    /// Ordinary case, and the guard against a majority rule that always
    /// says yes: a hole wholly outside the outer belongs to nobody.
    #[test]
    fn a_hole_outside_the_outer_is_not_owned() {
        let outer = rect_lonlat(0.0, 0.0, 10.0, 10.0);
        let hole = rect_ring_e7(20.0, 20.0, 22.0, 22.0);
        assert!(!hole_inside_outer(&hole, &outer));
    }

    /// `(lon, lat)` degrees to an assembled `(lat_e7, lon_e7)` ring.
    #[allow(clippy::cast_possible_truncation)]
    fn ring_e7(lonlat: &[(f64, f64)]) -> Vec<(i32, i32)> {
        lonlat
            .iter()
            .map(|&(lon, lat)| (deg_to_e7(lat) as i32, deg_to_e7(lon) as i32))
            .collect()
    }

    /// Point-in-polygon over a stored polygon, exactly as the reader runs it.
    fn stored_contains(
        vertices: &[crate::geocode_index::format::NodeCoord],
        lon: f64,
        lat: f64,
    ) -> bool {
        let rings = crate::geocode_index::format::parse_rings(vertices.iter().copied());
        let holes: Vec<&[(f64, f64)]> = rings[1..].iter().map(Vec::as_slice).collect();
        crate::geo::point_in_polygon(lon, lat, &rings[0], &holes)
    }

    /// A simple ring whose bottom edge dips under the tip of a deep notch
    /// coming down from the top edge. Douglas-Peucker under the former
    /// vertex cap (10 here) dropped exactly the dip vertex (5, 1), its
    /// smallest deviation, a full degree: the new bottom segment
    /// (2, 2)-(8, 2) passed above the notch tip (5, 1.5), the stored ring
    /// crossed itself, and (5, 1.2), some 21 km inside the boundary, read as
    /// outside it. A count knob let the error reach whatever the count
    /// demanded; a metric tolerance below that distance cannot.
    fn notch_over_dip_ring() -> Vec<(f64, f64)> {
        vec![
            (0.0, 0.0),
            (2.0, 2.0),
            (5.0, 1.0),
            (8.0, 2.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (6.0, 10.0),
            (5.0, 1.5),
            (4.0, 10.0),
            (0.0, 10.0),
            (0.0, 0.0),
        ]
    }

    #[test]
    fn simplification_keeps_containment_beside_a_notch() {
        let outer = notch_over_dip_ring();
        let query = (5.0, 1.2);
        assert!(
            crate::geo::point_in_ring(query.0, query.1, &outer),
            "the query point is inside the unsimplified boundary"
        );
        let mut way_geom: FxHashMap<i64, Vec<(i32, i32)>> = FxHashMap::default();
        way_geom.insert(1, ring_e7(&outer));
        let relations = vec![RawAdminRelation {
            id: 1,
            admin_level: 8,
            name_offset: 1,
            country_code_offset: 0,
            outer_way_ids: vec![1],
            inner_way_ids: Vec::new(),
        }];
        for admin_tolerance_m in [BuildConfig::default().admin_tolerance_m, 15_000.0] {
            let config = BuildConfig {
                admin_tolerance_m,
                ..BuildConfig::default()
            };
            let (polygons, _) = assemble_admin_polygons(
                &relations,
                &way_geom,
                &config,
                &crate::host::Executor::Ambient,
            );
            assert_eq!(polygons.len(), 1);
            assert!(
                stored_contains(&polygons[0].vertices, query.0, query.1),
                "simplification at {admin_tolerance_m} m moved a point 21 km inside \
                 the boundary outside it: {:?}",
                polygons[0].vertices
            );
        }
    }

    /// Through the builder, on a ring dense enough to be simplified hard:
    /// every sample point farther than the tolerance from the original
    /// boundary keeps its answer, holes included.
    #[test]
    fn far_points_keep_their_admin_answer() {
        let wavy = |lon0: f64, radius: f64, n: u32, amp: f64, freq: f64| {
            let mut ring: Vec<(f64, f64)> = (0..n)
                .map(|i| {
                    let a = f64::from(i) / f64::from(n) * std::f64::consts::TAU;
                    let r = radius * (1.0 + amp * f64::sin(f64::from(i) * freq));
                    (lon0 + r * a.cos(), 55.0 + r * a.sin())
                })
                .collect();
            ring.push(ring[0]);
            ring
        };
        // Wiggles of a few hundred metres: under the 1 km tolerance, so most
        // vertices go, but not by much, so the bands overlap the wiggles.
        let outer = wavy(10.0, 0.2, 2_000, 0.03, 1.7);
        let hole = wavy(10.05, 0.05, 500, 0.1, 2.3);
        let mut way_geom: FxHashMap<i64, Vec<(i32, i32)>> = FxHashMap::default();
        way_geom.insert(1, ring_e7(&outer));
        way_geom.insert(2, ring_e7(&hole));
        let relations = vec![RawAdminRelation {
            id: 1,
            admin_level: 4,
            name_offset: 1,
            country_code_offset: 0,
            outer_way_ids: vec![1],
            inner_way_ids: vec![2],
        }];
        let tolerance = 1_000.0_f32;
        let config = BuildConfig {
            admin_tolerance_m: tolerance,
            ..BuildConfig::default()
        };
        let (polygons, _) = assemble_admin_polygons(
            &relations,
            &way_geom,
            &config,
            &crate::host::Executor::Ambient,
        );
        assert_eq!(polygons.len(), 1);
        let stored = &polygons[0].vertices;
        assert!(
            stored.len() < (outer.len() + hole.len()) / 2,
            "{} stored",
            stored.len()
        );

        // Conservative metres: the smallest cos(lat) over the box.
        let c = f64::to_radians(55.25).cos();
        let seg_dist_m = |p: (f64, f64), a: (f64, f64), b: (f64, f64)| {
            let m = |(lon, lat): (f64, f64)| (lon.to_radians() * c, lat.to_radians());
            let ((px, py), (ax, ay), (bx, by)) = (m(p), m(a), m(b));
            let (dx, dy) = (bx - ax, by - ay);
            let t = (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
            let (qx, qy) = (ax + t * dx - px, ay + t * dy - py);
            (qx * qx + qy * qy).sqrt() * crate::geo::EARTH_RADIUS_M
        };
        let mut checked = 0;
        for i in 0..=100 {
            for j in 0..=100 {
                let p = (
                    9.75 + 0.5 * f64::from(i) / 100.0,
                    54.75 + 0.5 * f64::from(j) / 100.0,
                );
                let near = [&outer, &hole]
                    .iter()
                    .flat_map(|r| r.windows(2))
                    .any(|w| seg_dist_m(p, w[0], w[1]) <= f64::from(tolerance));
                if near {
                    continue;
                }
                checked += 1;
                let original = crate::geo::point_in_polygon(p.0, p.1, &outer, &[&hole]);
                assert_eq!(stored_contains(stored, p.0, p.1), original, "{p:?}");
            }
        }
        assert!(checked > 5_000, "only {checked} points checked");
    }
}
