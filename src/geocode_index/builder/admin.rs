//! Admin polygon assembly and on-disk writing.

use std::io::{BufWriter, Write};
use std::path::Path;

use rustc_hash::FxHashMap;
use s2::cellid::CellID;

use super::BuildConfig;
use super::Result;
use super::pass1::RawAdminRelation;

use super::super::format::*;

pub(super) struct AssembledPolygon {
    pub(super) admin_level: u8,
    pub(super) name_offset: u32,
    pub(super) country_code_offset: u32,
    pub(super) area: f32,
    pub(super) vertices: Vec<NodeCoord>,
}

#[hotpath::measure]
pub(super) fn assemble_admin_polygons(
    relations: &[RawAdminRelation],
    way_geom: &FxHashMap<i64, Vec<(i32, i32)>>,
    config: &BuildConfig,
    executor: &crate::host::Executor,
) -> (Vec<AssembledPolygon>, usize) {
    use rayon::prelude::*;

    let max_verts = config.max_admin_vertices as usize;

    // Per-relation work is independent - ring assembly, Douglas-Peucker
    // simplification, and hole attachment all read-only against `way_geom`
    // and the relation itself. `par_iter().flat_map_iter().collect()`
    // preserves input order so the output `Vec<AssembledPolygon>` is
    // byte-identical to the previous sequential path. Europe phase was
    // 50.6 s at UUID `bf8f2038` - expected ~5× on plantasjen's 12 cores.
    let per_relation: Vec<Vec<AssembledPolygon>> = executor.install(|| {
        relations
            .par_iter()
            .map(|rel| {
                assemble_one_relation(rel, way_geom, admin_vertex_cap(rel.admin_level, max_verts))
            })
            .collect()
    });
    let dropped = per_relation
        .iter()
        .filter(|polygons| polygons.is_empty())
        .count();
    let polygons = per_relation.into_iter().flatten().collect();
    (polygons, dropped)
}

/// Interpret the CLI cap as the local-boundary budget and retain progressively
/// more detail for larger administrative levels. This reduces border
/// displacement without making a country and a municipality share one
/// arbitrary vertex budget.
fn admin_vertex_cap(admin_level: u8, base: usize) -> usize {
    if base == 0 {
        return 0;
    }
    let multiplier = match admin_level {
        0..=2 => 16,
        3 => 8,
        4 => 4,
        5 => 2,
        _ => 1,
    };
    base.saturating_mul(multiplier)
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
    const SAMPLES: usize = 5;
    let step = (hole.len() / SAMPLES).max(1);
    let mut inside = 0_usize;
    let mut tested = 0_usize;
    for &(lat, lon) in hole.iter().step_by(step).take(SAMPLES) {
        tested += 1;
        if crate::geo::point_in_ring_with_antimeridian(
            lon as f64 * 1e-7,
            lat as f64 * 1e-7,
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
fn assemble_one_relation(
    rel: &RawAdminRelation,
    way_geom: &FxHashMap<i64, Vec<(i32, i32)>>,
    max_verts: usize,
) -> Vec<AssembledPolygon> {
    let outer_segs: Vec<&[(i32, i32)]> = rel
        .outer_way_ids
        .iter()
        .filter_map(|wid| way_geom.get(wid).map(Vec::as_slice))
        .collect();
    let outer_rings = crate::geo::assemble_rings(&outer_segs);
    if outer_rings.is_empty() {
        return Vec::new();
    }

    let inner_segs: Vec<&[(i32, i32)]> = rel
        .inner_way_ids
        .iter()
        .filter_map(|wid| way_geom.get(wid).map(Vec::as_slice))
        .collect();
    let inner_rings = crate::geo::assemble_rings(&inner_segs);

    let mut result = Vec::with_capacity(outer_rings.len());
    for outer_ring in &outer_rings {
        let outer_f64: Vec<(f64, f64)> = outer_ring
            .iter()
            .map(|&(lat, lon)| (lon as f64 * 1e-7, lat as f64 * 1e-7))
            .collect();

        let simplified = if max_verts > 0 {
            crate::geo::simplify_ring(&outer_f64, max_verts)
        } else {
            outer_f64.clone()
        };

        if simplified.len() < 3 {
            continue;
        }

        #[allow(clippy::cast_possible_truncation)]
        let area = crate::geo::signed_area(outer_ring).abs() as f32;

        let mut vertices = Vec::new();
        for &(lon_deg, lat_deg) in &simplified {
            #[allow(clippy::cast_possible_truncation)]
            vertices.push(NodeCoord {
                lat_e7: (lat_deg * 1e7) as i32,
                lon_e7: (lon_deg * 1e7) as i32,
            });
        }

        // Add inner rings (holes) that fall inside this outer ring.
        //
        // Test containment against the *original* `outer_f64`, not the
        // Douglas-Peucker-simplified `simplified`. Aggressive simplification
        // on the outer can shift the boundary by up to the DP tolerance,
        // and a hole whose `hole[0]` lies near the original boundary can
        // end up outside `simplified` even though the hole is topologically
        // inside. "Which outer owns this hole" is about original topology;
        // the rendered geometry we store in `vertices` is still the
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
                .map(|&(lat, lon)| (lon as f64 * 1e-7, lat as f64 * 1e-7))
                .collect();
            let sh = if max_verts > 0 {
                crate::geo::simplify_ring(&hole_f64, max_verts)
            } else {
                hole_f64
            };

            if sh.len() >= 3 {
                vertices.push(RING_SENTINEL);
                for &(lon_deg, lat_deg) in &sh {
                    #[allow(clippy::cast_possible_truncation)]
                    vertices.push(NodeCoord {
                        lat_e7: (lat_deg * 1e7) as i32,
                        lon_e7: (lon_deg * 1e7) as i32,
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
    result
}

#[hotpath::measure]
pub(super) fn write_admin_data(dir: &Path, polygons: &[AssembledPolygon]) -> Result<()> {
    let mut poly_out = BufWriter::new(std::fs::File::create(dir.join(FILE_ADMIN_POLYGONS))?);
    let mut vert_out = BufWriter::new(std::fs::File::create(dir.join(FILE_ADMIN_VERTICES))?);
    let mut offset: u64 = 0;
    for p in polygons {
        let vertex_count =
            u32::try_from(p.vertices.len()).map_err(|_| -> Box<dyn std::error::Error> {
                format!(
                    "write_admin_data: polygon has {} vertices, exceeds u32::MAX. \
                 Widen AdminPolygon.vertex_count to u64 and increment FORMAT_VERSION.",
                    p.vertices.len()
                )
                .into()
            })?;
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
        let step_bytes = p.vertices.len().checked_mul(NODE_COORD_SIZE).ok_or_else(
            || -> Box<dyn std::error::Error> {
                format!(
                    "write_admin_data: polygon vertex bytes overflow usize ({} * {NODE_COORD_SIZE}).",
                    p.vertices.len(),
                )
                .into()
            },
        )?;
        let step_u64 = u64::try_from(step_bytes)
            .map_err(|_| "write_admin_data: polygon vertex bytes exceed u64")?;
        offset = offset
            .checked_add(step_u64)
            .ok_or_else(|| -> Box<dyn std::error::Error> {
                format!(
                    "write_admin_data: cumulative admin-vertex byte offset overflows u64 \
                     (current {offset}, step {step_u64})"
                )
                .into()
            })?;
    }
    poly_out.flush()?;
    vert_out.flush()?;
    Ok(())
}

#[allow(clippy::cast_possible_truncation)]
#[hotpath::measure]
pub(super) fn write_admin_index(
    dir: &Path,
    entries: &mut [super::pass3::AdminCellEntry],
) -> Result<u32> {
    entries.sort_unstable_by_key(|e| e.cell_id);
    let mut cell_ids: Vec<u64> = entries.iter().map(|e| e.cell_id).collect();
    cell_ids.sort_unstable();
    cell_ids.dedup();

    let mut entries_out = BufWriter::new(std::fs::File::create(dir.join(FILE_ADMIN_ENTRIES))?);
    let mut cells_out = BufWriter::new(std::fs::File::create(dir.join(FILE_ADMIN_CELLS))?);
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

        // INVARIANT: the on-disk count for admin entries is u16 (see
        // `format::AdminEntryIter` reader-side). Hard-error on overflow rather
        // than silently truncating. If this fires, bump the count to u32 and
        // increment `FORMAT_VERSION`.
        let group_len = i - start;
        let count = u16::try_from(group_len).map_err(|_| {
            format!(
                "write_admin_index: admin cell {cell_token} ({cid}) has {group_len} entries, exceeds u16::MAX. \
             Bump on-disk count to u32 and increment FORMAT_VERSION."
            )
        })?;
        entries_out.write_all(&count.to_le_bytes())?;
        byte_off = byte_off.checked_add(2).ok_or_else(|| {
            format!(
                "write_admin_index: admin-entries byte offset overflows u64 at cell {cell_token} ({cid}) \
             (current {byte_off})"
            )
        })?;
        for e in &entries[start..i] {
            // INTERIOR_FLAG (= 0x8000_0000) ORs into the high bit of
            // poly_index. If `poly_index >= 0x8000_0000` (>2.1 B admin
            // polygons) the OR silently collides with existing bits
            // and the decoded poly_index would be wrong. Guard here
            // rather than at the upstream counter.
            if e.poly_index & INTERIOR_FLAG != 0 {
                return Err(format!(
                    "write_admin_index: poly_index {} in cell {cell_token} ({cid}) has the INTERIOR_FLAG bit set; \
                     admin polygon count has exceeded 2^31 - 1. Split INTERIOR_FLAG into a \
                     separate byte or grow AdminEntry to u64 and increment FORMAT_VERSION.",
                    e.poly_index
                )
                .into());
            }
            let val = if e.is_interior {
                e.poly_index | INTERIOR_FLAG
            } else {
                e.poly_index
            };
            entries_out.write_all(&val.to_le_bytes())?;
            byte_off = byte_off.checked_add(4).ok_or_else(|| {
                format!(
                    "write_admin_index: admin-entries byte offset overflows u64 at cell {cell_token} ({cid}) \
                 (current {byte_off})"
                )
            })?;
        }
    }
    cells_out.flush()?;
    entries_out.flush()?;
    Ok(cell_ids.len() as u32)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use rustc_hash::FxHashMap;

    use super::{admin_vertex_cap, assemble_admin_polygons, hole_inside_outer};
    use crate::geocode_index::builder::BuildConfig;
    use crate::geocode_index::builder::pass1::RawAdminRelation;

    #[test]
    fn admin_vertex_budget_scales_by_level() {
        assert_eq!(admin_vertex_cap(8, 500), 500);
        assert_eq!(admin_vertex_cap(5, 500), 1_000);
        assert_eq!(admin_vertex_cap(4, 500), 2_000);
        assert_eq!(admin_vertex_cap(3, 500), 4_000);
        assert_eq!(admin_vertex_cap(2, 500), 8_000);
        assert_eq!(admin_vertex_cap(2, usize::MAX), usize::MAX);
        assert_eq!(admin_vertex_cap(2, 0), 0);
    }

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
                admin_level: 8,
                name_offset: 1,
                country_code_offset: 0,
                outer_way_ids: vec![99],
                inner_way_ids: Vec::new(),
            },
            // Names no ways at all: dropped.
            RawAdminRelation {
                admin_level: 8,
                name_offset: 2,
                country_code_offset: 0,
                outer_way_ids: Vec::new(),
                inner_way_ids: Vec::new(),
            },
            // Closed ring: assembles, and must NOT be counted as dropped.
            RawAdminRelation {
                admin_level: 8,
                name_offset: 3,
                country_code_offset: 0,
                outer_way_ids: vec![7],
                inner_way_ids: Vec::new(),
            },
        ];
        let (polygons, dropped) = assemble_admin_polygons(
            &relations,
            &way_geom,
            &BuildConfig::default(),
            &crate::host::Executor::Ambient,
        );
        assert_eq!(polygons.len(), 1, "the closed ring should assemble");
        assert_eq!(polygons[0].name_offset, 3);
        assert_eq!(
            dropped, 2,
            "only the two geometry-less relations are dropped"
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
            .map(|(lon, lat)| ((lat * 1e7) as i32, (lon * 1e7) as i32))
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
}
