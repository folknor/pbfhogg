//! Admin polygon assembly and on-disk writing.

use std::io::{BufWriter, Write};
use std::path::Path;

use rustc_hash::FxHashMap;

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
) -> Vec<AssembledPolygon> {
    use rayon::prelude::*;

    let max_verts = config.max_admin_vertices as usize;

    // Per-relation work is independent - ring assembly, Douglas-Peucker
    // simplification, and hole attachment all read-only against `way_geom`
    // and the relation itself. `par_iter().flat_map_iter().collect()`
    // preserves input order so the output `Vec<AssembledPolygon>` is
    // byte-identical to the previous sequential path. Europe phase was
    // 50.6 s at UUID `bf8f2038` - expected ~5× on plantasjen's 12 cores
    // (plan item #8 in notes/geocode-build-opportunities.md).
    relations
        .par_iter()
        .flat_map_iter(|rel| assemble_one_relation(rel, way_geom, max_verts).into_iter())
        .collect()
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
            let hp = (hole[0].1 as f64 * 1e-7, hole[0].0 as f64 * 1e-7);
            if !crate::geo::point_in_ring(hp.0, hp.1, &outer_f64) {
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
    // `vertex_offset` is a u32 byte-offset into admin_vertices.bin (see
    // `AdminPolygon.vertex_offset` in format.rs). Hard-error on
    // overflow rather than silently wrapping; a wrap would make every
    // subsequent polygon point at the wrong vertex span. If this fires,
    // widen `vertex_offset` to u64 and bump FORMAT_VERSION. Matches the
    // sibling u16::MAX hard-error at `write_admin_index` for
    // admin-entry counts.
    let mut offset: u32 = 0;
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
        let step_u32 = u32::try_from(step_bytes).map_err(|_| -> Box<dyn std::error::Error> {
            format!(
                "write_admin_data: one polygon contributes {step_bytes} vertex bytes, \
                 exceeds u32::MAX. Widen AdminPolygon.vertex_offset to u64 and increment FORMAT_VERSION."
            )
            .into()
        })?;
        offset = offset
            .checked_add(step_u32)
            .ok_or_else(|| -> Box<dyn std::error::Error> {
                format!(
                    "write_admin_data: cumulative admin-vertex byte offset overflows u32 \
                     (current {offset}, step {step_u32}; total exceeds 4 GiB). Widen \
                     AdminPolygon.vertex_offset to u64 and increment FORMAT_VERSION."
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
    // `entries_offset` is a u32 byte-offset into admin_entries.bin (see
    // `AdminCell.entries_offset` in format.rs). Hard-error on overflow
    // rather than silently wrapping; a wrap would make every
    // subsequent cell read entries from the wrong byte range. Widen to
    // u64 and bump FORMAT_VERSION if this fires.
    let mut byte_off: u32 = 0;
    let mut i = 0;

    for &cid in &cell_ids {
        let start = i;
        while i < entries.len() && entries[i].cell_id == cid {
            i += 1;
        }
        if start == i {
            continue;
        }

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
                "write_admin_index: admin cell {cid} has {group_len} entries, exceeds u16::MAX. \
             Bump on-disk count to u32 and increment FORMAT_VERSION."
            )
        })?;
        entries_out.write_all(&count.to_le_bytes())?;
        byte_off = byte_off.checked_add(2).ok_or_else(|| {
            format!(
                "write_admin_index: admin-entries byte offset overflows u32 at cell {cid} \
             (current {byte_off}; total exceeds 4 GiB). Widen AdminCell.entries_offset \
             to u64 and increment FORMAT_VERSION."
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
                    "write_admin_index: poly_index {} in cell {cid} has the INTERIOR_FLAG bit set; \
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
                    "write_admin_index: admin-entries byte offset overflows u32 at cell {cid} \
                 (current {byte_off}; total exceeds 4 GiB). Widen AdminCell.entries_offset \
                 to u64 and increment FORMAT_VERSION."
                )
            })?;
        }
    }
    cells_out.flush()?;
    entries_out.flush()?;
    Ok(cell_ids.len() as u32)
}
