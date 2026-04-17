//! Admin polygon assembly and on-disk writing.

use std::io::{BufWriter, Write};
use std::path::Path;

use rustc_hash::FxHashMap;

use super::Result;
use super::BuildConfig;
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
    let mut result = Vec::new();
    let max_verts = config.max_admin_vertices as usize;

    for rel in relations {
        let outer_segs: Vec<&[(i32, i32)]> = rel.outer_way_ids.iter()
            .filter_map(|wid| way_geom.get(wid).map(Vec::as_slice))
            .collect();
        let outer_rings = crate::geo::assemble_rings(&outer_segs);
        if outer_rings.is_empty() { continue; }

        let inner_segs: Vec<&[(i32, i32)]> = rel.inner_way_ids.iter()
            .filter_map(|wid| way_geom.get(wid).map(Vec::as_slice))
            .collect();
        let inner_rings = crate::geo::assemble_rings(&inner_segs);

        for outer_ring in &outer_rings {
            let outer_f64: Vec<(f64, f64)> = outer_ring.iter()
                .map(|&(lat, lon)| (lon as f64 * 1e-7, lat as f64 * 1e-7))
                .collect();

            let simplified = if max_verts > 0 {
                crate::geo::simplify_ring(&outer_f64, max_verts)
            } else { outer_f64.clone() };

            if simplified.len() < 3 { continue; }

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

            // Add inner rings (holes) that fall inside this outer ring
            for hole in &inner_rings {
                if hole.is_empty() { continue; }
                let hp = (hole[0].1 as f64 * 1e-7, hole[0].0 as f64 * 1e-7);
                if !crate::geo::point_in_ring(hp.0, hp.1, &simplified) { continue; }

                let hole_f64: Vec<(f64, f64)> = hole.iter()
                    .map(|&(lat, lon)| (lon as f64 * 1e-7, lat as f64 * 1e-7))
                    .collect();
                let sh = if max_verts > 0 {
                    crate::geo::simplify_ring(&hole_f64, max_verts)
                } else { hole_f64 };

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
                area, vertices,
            });
        }
    }
    result
}

pub(super) fn write_admin_data(dir: &Path, polygons: &[AssembledPolygon]) -> Result<()> {
    let mut poly_out = BufWriter::new(std::fs::File::create(dir.join(FILE_ADMIN_POLYGONS))?);
    let mut vert_out = BufWriter::new(std::fs::File::create(dir.join(FILE_ADMIN_VERTICES))?);
    let mut offset: u32 = 0;
    for p in polygons {
        #[allow(clippy::cast_possible_truncation)]
        let rec = AdminPolygon {
            area: p.area,
            vertex_offset: offset,
            vertex_count: p.vertices.len() as u32,
            name_offset: p.name_offset,
            country_code_offset: p.country_code_offset,
            admin_level: p.admin_level,
        };
        poly_out.write_all(&rec.to_bytes())?;
        for v in &p.vertices {
            vert_out.write_all(&v.to_bytes())?;
        }
        #[allow(clippy::cast_possible_truncation)]
        { offset += (p.vertices.len() * NODE_COORD_SIZE) as u32; }
    }
    poly_out.flush()?;
    vert_out.flush()?;
    Ok(())
}

#[allow(clippy::cast_possible_truncation)]
pub(super) fn write_admin_index(dir: &Path, entries: &mut [super::pass3::AdminCellEntry]) -> Result<u32> {
    entries.sort_unstable_by_key(|e| e.cell_id);
    let mut cell_ids: Vec<u64> = entries.iter().map(|e| e.cell_id).collect();
    cell_ids.sort_unstable();
    cell_ids.dedup();

    let mut entries_out = BufWriter::new(std::fs::File::create(dir.join(FILE_ADMIN_ENTRIES))?);
    let mut cells_out = BufWriter::new(std::fs::File::create(dir.join(FILE_ADMIN_CELLS))?);
    let mut byte_off: u32 = 0;
    let mut i = 0;

    for &cid in &cell_ids {
        let start = i;
        while i < entries.len() && entries[i].cell_id == cid { i += 1; }
        if start == i { continue; }

        cells_out.write_all(&AdminCell { cell_id: cid, entries_offset: byte_off }.to_bytes())?;

        // INVARIANT: the on-disk count for admin entries is u16 (see
        // `format::AdminEntryIter` reader-side). Hard-error on overflow rather
        // than silently truncating. If this fires, bump the count to u32 and
        // increment `FORMAT_VERSION`.
        let group_len = i - start;
        let count = u16::try_from(group_len).map_err(|_| format!(
            "write_admin_index: admin cell {cid} has {group_len} entries, exceeds u16::MAX. \
             Bump on-disk count to u32 and increment FORMAT_VERSION."
        ))?;
        entries_out.write_all(&count.to_le_bytes())?;
        byte_off += 2;
        for e in &entries[start..i] {
            let val = if e.is_interior { e.poly_index | INTERIOR_FLAG } else { e.poly_index };
            entries_out.write_all(&val.to_le_bytes())?;
            byte_off += 4;
        }
    }
    cells_out.flush()?;
    entries_out.flush()?;
    Ok(cell_ids.len() as u32)
}
