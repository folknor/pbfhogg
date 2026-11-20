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
use crate::geo::{RingMember, RingRefusal};

use super::super::format::*;

pub(super) struct AssembledPolygon {
    pub(super) admin_level: u8,
    pub(super) name_offset: u32,
    pub(super) country_code_offset: u32,
    pub(super) area: f32,
    pub(super) vertices: Vec<NodeCoord>,
}

/// Relations that produced no polygon. `ambiguous_topology` is the subset
/// whose complete member ways do not determine their rings
/// (`crate::geo::assemble_rings` lists the refusals; a way listed as both
/// outer and inner is one more); it is included in `total`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct DroppedAdminRelations {
    pub(super) total: usize,
    pub(super) ambiguous_topology: usize,
    pub(super) sample_relation_ids: Vec<i64>,
}

/// Where one fully located admin member way lives in [`WayGeometryMap`]:
/// its refs as the source's packed `sint64` delta bytes, and one
/// `(lat_e7, lon_e7)` coordinate per ref. Rings are joined by node ID, so
/// two nodes at one coordinate stay two vertices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AdminWaySpan {
    refs: (usize, usize),
    coords: (usize, usize),
}

/// One admin member way's geometry: the way when every ref resolved,
/// `None` when the way is in the file but at least one of its nodes is not
/// (or it has no refs).
pub(super) type AdminWayGeometry = Option<AdminWaySpan>;

/// Admin member way geometry by way ID, last file occurrence winning. A way
/// absent from the map is absent from the file.
///
/// The payload sits in two flat anonymous mappings sized once from the
/// Pass 1.5 totals, never in per-way heap allocations. Millions of small
/// per-way vectors (node IDs and coordinates) once fragmented the heap:
/// at planet the arena kept about 1.6 GB resident from the end of Pass 2
/// to the end of the run, after the map itself was dropped. A mapping is
/// returned to the OS when it drops. An occurrence superseded by a later
/// one keeps its bytes until then; the totals count every occurrence, so
/// the store still fits.
pub(super) struct WayGeometryMap {
    spans: FxHashMap<i64, AdminWayGeometry>,
    refs: memmap2::MmapMut,
    refs_len: usize,
    refs_cap: usize,
    coords: memmap2::MmapMut,
    coords_len: usize,
    coords_cap: usize,
}

/// A fully located way as Phase 2b hands it over: packed delta refs and
/// one coordinate per ref.
pub(super) type LocatedWay<'a> = (&'a [u8], &'a [(i32, i32)]);

/// Bytes per stored coordinate: `lat_e7` then `lon_e7`, little-endian.
const COORD_BYTES: usize = 8;

impl WayGeometryMap {
    /// A store for at most `ref_bytes` packed ref bytes and `coords`
    /// coordinates over all admin way occurrences.
    pub(super) fn with_capacity(ref_bytes: u64, coords: u64) -> Result<Self> {
        let refs_cap = usize::try_from(ref_bytes).map_err(|_| {
            Error::invalid_data(format!("{ref_bytes} admin ref bytes exceed usize"))
        })?;
        let coords_cap = usize::try_from(coords).map_err(|_| {
            Error::invalid_data(format!("{coords} admin way coordinates exceed usize"))
        })?;
        let coord_bytes = coords_cap.checked_mul(COORD_BYTES).ok_or_else(|| {
            Error::invalid_data(format!("{coords} admin way coordinates overflow usize"))
        })?;
        // `map_anon` rejects a zero length.
        Ok(Self {
            spans: FxHashMap::default(),
            refs: memmap2::MmapMut::map_anon(refs_cap.max(1))?,
            refs_len: 0,
            refs_cap,
            coords: memmap2::MmapMut::map_anon(coord_bytes.max(1))?,
            coords_len: 0,
            coords_cap,
        })
    }

    /// Record the next file occurrence of `way_id`: `Some((refs, coords))`
    /// for a fully located way (`refs` its packed delta bytes, one coordinate
    /// per ref), `None` for one that is not. A later occurrence replaces an
    /// earlier one, `None` included.
    pub(super) fn insert(&mut self, way_id: i64, geometry: Option<LocatedWay<'_>>) -> Result<()> {
        let span = match geometry {
            None => None,
            Some((refs, coords)) => {
                let refs_end = self
                    .refs_len
                    .checked_add(refs.len())
                    .filter(|&end| end <= self.refs_cap)
                    .ok_or_else(|| {
                        Error::internal(format!(
                            "admin way {way_id} overflows the {}-byte ref store sized by pass 1.5",
                            self.refs_cap
                        ))
                    })?;
                let coords_end = self
                    .coords_len
                    .checked_add(coords.len())
                    .filter(|&end| end <= self.coords_cap)
                    .ok_or_else(|| {
                        Error::internal(format!(
                            "admin way {way_id} overflows the {}-coordinate store sized by pass 1.5",
                            self.coords_cap
                        ))
                    })?;
                self.refs[self.refs_len..refs_end].copy_from_slice(refs);
                let out = &mut self.coords[self.coords_len * COORD_BYTES..coords_end * COORD_BYTES];
                for (chunk, &(lat, lon)) in
                    out.as_chunks_mut::<COORD_BYTES>().0.iter_mut().zip(coords)
                {
                    chunk[..4].copy_from_slice(&lat.to_le_bytes());
                    chunk[4..].copy_from_slice(&lon.to_le_bytes());
                }
                let span = AdminWaySpan {
                    refs: (self.refs_len, refs_end),
                    coords: (self.coords_len, coords_end),
                };
                self.refs_len = refs_end;
                self.coords_len = coords_end;
                Some(span)
            }
        };
        self.spans.insert(way_id, span);
        Ok(())
    }

    /// The last occurrence of `way_id`: `None` when absent from the file,
    /// `Some(None)` when present but not fully located.
    pub(super) fn get(&self, way_id: i64) -> Option<AdminWayGeometry> {
        self.spans.get(&way_id).copied()
    }

    pub(super) fn len(&self) -> usize {
        self.spans.len()
    }

    /// Append the node IDs and coordinates of `span` to `nodes` and
    /// `coords`. A decoded ref count that differs from the coordinate count
    /// is an internal error: the two were written together.
    fn decode_into(
        &self,
        way_id: i64,
        span: AdminWaySpan,
        nodes: &mut Vec<i64>,
        coords: &mut Vec<(i32, i32)>,
    ) -> Result<()> {
        let refs = self.refs.get(span.refs.0..span.refs.1);
        let stored = self
            .coords
            .get(span.coords.0 * COORD_BYTES..span.coords.1 * COORD_BYTES);
        let (Some(refs), Some(stored)) = (refs, stored) else {
            return Err(Error::internal(format!(
                "admin way {way_id} has a span outside its store"
            )));
        };
        let start = nodes.len();
        nodes.extend(crate::read::elements::WayRefIter::delta_decoded(refs));
        let decoded = nodes.len() - start;
        let expected = span.coords.1 - span.coords.0;
        if decoded != expected {
            return Err(Error::internal(format!(
                "admin way {way_id} decodes {decoded} refs for {expected} stored coordinates"
            )));
        }
        coords.extend(stored.as_chunks::<COORD_BYTES>().0.iter().map(|c| {
            (
                i32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                i32::from_le_bytes([c[4], c[5], c[6], c[7]]),
            )
        }));
        Ok(())
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct MissingAdminWayGeometry {
    pub(super) member_refs: u64,
    pub(super) sample_way_ids: Vec<i64>,
}

/// Count relation member references whose way is absent from the file or
/// not fully located, and retain a bounded, stable sample of their way IDs.
pub(super) fn missing_admin_way_geometry(
    relations: &[RawAdminRelation],
    way_geom: &WayGeometryMap,
) -> MissingAdminWayGeometry {
    let mut missing = MissingAdminWayGeometry::default();
    for relation in relations {
        for &way_id in relation.outer_way_ids.iter().chain(&relation.inner_way_ids) {
            if !matches!(way_geom.get(way_id), Some(Some(_))) {
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
    way_geom: &WayGeometryMap,
    config: &BuildConfig,
    executor: &crate::host::Executor,
) -> Result<(Vec<AssembledPolygon>, DroppedAdminRelations)> {
    use rayon::prelude::*;

    let tolerance_m = f64::from(config.admin_tolerance_m);

    // Per-relation work is independent - ring assembly, Douglas-Peucker
    // simplification, and hole attachment all read-only against `way_geom`
    // and the relation itself. `par_iter().flat_map_iter().collect()`
    // preserves input order so the output `Vec<AssembledPolygon>` is
    // byte-identical to the previous sequential path. Europe phase was
    // 50.6 s at UUID `bf8f2038` - expected ~5× on plantasjen's 12 cores.
    // A store fault (`Err` outside) fails the build; a ring refusal
    // (`Err` inside) drops only its relation.
    #[allow(clippy::type_complexity)]
    let per_relation: Vec<Result<std::result::Result<Vec<AssembledPolygon>, RingRefusal>>> =
        executor.install(|| {
            relations
                .par_iter()
                .map(|rel| assemble_one_relation(rel, way_geom, tolerance_m))
                .collect()
        });
    let mut dropped = DroppedAdminRelations::default();
    let mut polygons = Vec::new();
    for (rel, relation) in relations.iter().zip(per_relation) {
        match relation? {
            Ok(assembled) if assembled.is_empty() => {
                dropped.total += 1;
                super::sample_id(&mut dropped.sample_relation_ids, rel.id);
            }
            Ok(assembled) => polygons.extend(assembled),
            Err(_) => {
                dropped.total += 1;
                dropped.ambiguous_topology += 1;
                super::sample_id(&mut dropped.sample_relation_ids, rel.id);
            }
        }
    }
    Ok((polygons, dropped))
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

/// A kept vertex back in decimicrodegrees. `simplify_ring` only selects
/// vertices of the original integer ring, so this recovers the original
/// integer: rounded, not truncated, because `x * 1e-7 * 1e7` can land just
/// under `x` (550000007 comes back as 550000006.9999999), which moved
/// boundaries by a unit even at `--admin-tolerance 0`, where every vertex
/// is documented as kept.
#[allow(clippy::cast_possible_truncation)]
fn vertex_e7(lon_deg: f64, lat_deg: f64) -> NodeCoord {
    NodeCoord {
        lat_e7: deg_to_e7(lat_deg).round() as i32,
        lon_e7: deg_to_e7(lon_deg).round() as i32,
    }
}

/// Assemble all `AssembledPolygon`s produced by a single relation
/// (one per outer ring, each carrying any inner rings that fall inside
/// it). Pure function - no shared mutable state, safe to call from
/// rayon workers.
///
/// Rings are assembled per role by node topology (`crate::geo::assemble_rings`).
/// A refusal on either role refuses the whole relation: a polygon assembled
/// without its holes, or from a partial ring set, would be a silently
/// different boundary. A way listed more than once in one role is one
/// boundary and counts once; a way listed as both outer and inner asserts
/// one boundary as shell and hole, and refuses the relation. The same edge
/// under two different way IDs, one per role, is not refused: the reader
/// answers `in outer and not in hole`, so a shared edge is a notch in the
/// set difference, not a different area. Nor are edges shared with other
/// relations, which are answered independently; neighbouring boundaries
/// share edges as a rule.
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
    way_geom: &WayGeometryMap,
    tolerance_m: f64,
) -> Result<std::result::Result<Vec<AssembledPolygon>, RingRefusal>> {
    let Some(outer) = RoleScratch::decode(&rel.outer_way_ids, way_geom)? else {
        return Ok(Ok(Vec::new()));
    };
    let Some(inner) = RoleScratch::decode(&rel.inner_way_ids, way_geom)? else {
        return Ok(Ok(Vec::new()));
    };
    Ok(assemble_decoded(
        rel,
        &outer.members(),
        &inner.members(),
        tolerance_m,
    ))
}

/// Assemble a relation from its decoded member ways, outer and inner
/// each in first-occurrence order.
fn assemble_decoded(
    rel: &RawAdminRelation,
    outer_members: &[RingMember<'_>],
    inner_members: &[RingMember<'_>],
    tolerance_m: f64,
) -> std::result::Result<Vec<AssembledPolygon>, RingRefusal> {
    // Extracts may contain a relation but omit member ways, or keep a member
    // way while omitting some of its nodes. Such a relation contributes no
    // polygon at all. Assembling around the hole is not safe in any form:
    // - joining a partial way's surviving nodes across the gap invents an
    //   edge, which can close a smaller ring;
    // - feeding its runs separately lets an endpoint at an equal coordinate
    //   on another node close a ring the source never closes;
    // - dropping a whole member, outer or inner, changes which observed
    //   edges the endpoint search pairs into rings (several members sharing
    //   both endpoints pair differently once one is gone), and a missing
    //   inner turns its hole into an affirmative match.
    // The builder has no evidence that a missing member cannot affect a
    // given outer, so the relation is withheld and counted as dropped
    // (`RoleScratch::decode` answers `None` for it).
    let inner_ids: rustc_hash::FxHashSet<i64> = rel.inner_way_ids.iter().copied().collect();
    if rel.outer_way_ids.iter().any(|id| inner_ids.contains(id)) {
        return Err(RingRefusal::AmbiguousJunction);
    }
    let outer_rings = crate::geo::assemble_rings(outer_members)?;
    if outer_rings.is_empty() {
        return Ok(Vec::new());
    }

    let inner_rings = crate::geo::assemble_rings(inner_members)?;

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
            vertices.push(vertex_e7(lon_deg, lat_deg));
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
                    vertices.push(vertex_e7(lon_deg, lat_deg));
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

/// One role's member ways decoded into relation-local buffers, the first
/// occurrence of each way ID only.
struct RoleScratch {
    nodes: Vec<i64>,
    coords: Vec<(i32, i32)>,
    /// Each member's end in `nodes` and `coords` (the two run in lockstep).
    ends: Vec<usize>,
}

impl RoleScratch {
    /// `Ok(None)` when any member lacks complete geometry.
    fn decode(way_ids: &[i64], way_geom: &WayGeometryMap) -> Result<Option<Self>> {
        let mut seen = rustc_hash::FxHashSet::default();
        let mut role = Self {
            nodes: Vec::new(),
            coords: Vec::new(),
            ends: Vec::with_capacity(way_ids.len()),
        };
        for &id in way_ids {
            let Some(Some(span)) = way_geom.get(id) else {
                return Ok(None);
            };
            if seen.insert(id) {
                way_geom.decode_into(id, span, &mut role.nodes, &mut role.coords)?;
                role.ends.push(role.nodes.len());
            }
        }
        Ok(Some(role))
    }

    fn members(&self) -> Vec<RingMember<'_>> {
        let mut start = 0;
        self.ends
            .iter()
            .map(|&end| {
                let member = RingMember {
                    nodes: &self.nodes[start..end],
                    coords: &self.coords[start..end],
                };
                start = end;
                member
            })
            .collect()
    }
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
            // The interior flag ORs into the high bit of poly_index. If
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

    use super::{
        DroppedAdminRelations, assemble_admin_polygons, hole_inside_outer,
        missing_admin_way_geometry,
    };
    use crate::geocode_index::builder::BuildConfig;
    use crate::geocode_index::builder::pass1::RawAdminRelation;

    /// A located way whose node IDs are derived from its coordinates, so
    /// fixtures that share a coordinate share the node: its packed delta
    /// refs and its coordinates.
    type OwnedWay = Option<(Vec<u8>, Vec<(i32, i32)>)>;

    fn located(coords: Vec<(i32, i32)>) -> OwnedWay {
        let nodes: Vec<i64> = coords
            .iter()
            .map(|&(lat, lon)| (i64::from(lat) << 32) | i64::from(lon.cast_unsigned()))
            .collect();
        Some((crate::read::wire_fixtures::deltas(&nodes), coords))
    }

    fn assemble(
        relations: &[RawAdminRelation],
        way_geom: &super::WayGeometryMap,
        config: &BuildConfig,
        executor: &crate::host::Executor,
    ) -> (Vec<super::AssembledPolygon>, DroppedAdminRelations) {
        assemble_admin_polygons(relations, way_geom, config, executor).unwrap()
    }

    impl super::WayGeometryMap {
        fn for_tests() -> Self {
            Self::with_capacity(1 << 20, 1 << 17).unwrap()
        }

        #[allow(clippy::needless_pass_by_value)]
        fn insert_test(&mut self, way_id: i64, way: OwnedWay) {
            self.insert(
                way_id,
                way.as_ref().map(|(r, c)| (r.as_slice(), c.as_slice())),
            )
            .unwrap();
        }
    }

    /// Delta-decoded refs survive the store whole: wrapping deltas across
    /// the i64 extremes, a repeated closing node, and a later occurrence
    /// replacing an earlier one, a `None` occurrence included.
    #[test]
    fn the_store_returns_each_ways_last_occurrence_exactly() {
        let mut store = super::WayGeometryMap::for_tests();
        let nodes = [i64::MAX, i64::MIN, -1, i64::MAX];
        let coords = vec![(1, 2), (3, 4), (5, 6), (1, 2)];
        let refs = crate::read::wire_fixtures::deltas(&nodes);
        store.insert_test(1, Some((refs.clone(), coords.clone())));
        store.insert_test(2, Some((refs.clone(), coords.clone())));
        store.insert_test(2, None);
        store.insert_test(3, None);
        store.insert_test(3, Some((refs, coords.clone())));

        let decode = |id| {
            let span = store.get(id).unwrap().unwrap();
            let (mut n, mut c) = (Vec::new(), Vec::new());
            store.decode_into(id, span, &mut n, &mut c).unwrap();
            (n, c)
        };
        assert_eq!(decode(1), (nodes.to_vec(), coords.clone()));
        assert_eq!(store.get(2), Some(None));
        assert_eq!(decode(3), (nodes.to_vec(), coords));
        assert_eq!(store.get(4), None);
    }

    /// A store sized below what is written refuses instead of truncating.
    #[test]
    fn the_store_refuses_more_than_its_capacity() {
        let mut store = super::WayGeometryMap::with_capacity(2, 2).unwrap();
        let refs = crate::read::wire_fixtures::deltas(&[1, 2, 3]);
        assert!(
            store
                .insert(1, Some((&refs, &[(0, 0), (1, 1), (2, 2)])))
                .is_err()
        );
    }

    /// Refs that decode to a count other than the coordinates written
    /// beside them fail the build rather than drop the relation.
    #[test]
    fn a_ref_count_mismatch_is_an_error_not_a_dropped_relation() {
        let mut way_geom = super::WayGeometryMap::for_tests();
        let refs = crate::read::wire_fixtures::deltas(&[1, 2, 3, 1]);
        way_geom
            .insert(7, Some((&refs, &[(0, 0), (0, 10), (10, 0)])))
            .unwrap();
        let result = super::assemble_admin_polygons(
            &[relation(1, vec![7], Vec::new())],
            &way_geom,
            &BuildConfig::default(),
            &crate::host::Executor::for_tests(),
        );
        assert!(result.is_err());
    }

    fn relation(id: i64, outer: Vec<i64>, inner: Vec<i64>) -> RawAdminRelation {
        RawAdminRelation {
            id,
            admin_level: 8,
            name_offset: u32::try_from(id).unwrap(),
            country_code_offset: 0,
            outer_way_ids: outer,
            inner_way_ids: inner,
        }
    }

    fn square_e7() -> Vec<(i32, i32)> {
        vec![
            (550_000_000, 120_000_000),
            (550_000_000, 121_000_000),
            (551_000_000, 121_000_000),
            (551_000_000, 120_000_000),
            (550_000_000, 120_000_000),
        ]
    }

    #[test]
    fn relations_without_assembled_geometry_are_counted() {
        // The MIXED case is what makes this discriminating: an implementation
        // that simply reported `relations.len()`, or one that reported zero,
        // would pass an all-dropped or all-kept fixture. One of each is the
        // only shape that pins the count to the relations that actually
        // failed to assemble.
        let mut way_geom = super::WayGeometryMap::for_tests();
        way_geom.insert_test(7, located(square_e7()));
        // In the file, but one of its nodes is not.
        way_geom.insert_test(8, None);

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
            // The same closed outer plus an outer that is not fully located:
            // dropped whole, the complete ring is not published alone.
            RawAdminRelation {
                id: 4,
                admin_level: 8,
                name_offset: 4,
                country_code_offset: 0,
                outer_way_ids: vec![7, 8],
                inner_way_ids: Vec::new(),
            },
            // The same closed outer with an absent inner: its hole would
            // become an affirmative match, so dropped whole.
            RawAdminRelation {
                id: 5,
                admin_level: 8,
                name_offset: 5,
                country_code_offset: 0,
                outer_way_ids: vec![7],
                inner_way_ids: vec![98],
            },
        ];
        assert_eq!(
            missing_admin_way_geometry(&relations, &way_geom),
            super::MissingAdminWayGeometry {
                member_refs: 3,
                sample_way_ids: vec![8, 98, 99],
            }
        );
        let (polygons, dropped) = assemble(
            &relations,
            &way_geom,
            &BuildConfig::default(),
            &crate::host::Executor::for_tests(),
        );
        assert_eq!(polygons.len(), 1, "the closed ring should assemble");
        assert_eq!(polygons[0].name_offset, 3);
        assert_eq!(
            dropped,
            DroppedAdminRelations {
                total: 4,
                ambiguous_topology: 0,
                sample_relation_ids: vec![1, 2, 4, 5],
            },
            "every relation without complete member geometry is dropped"
        );
    }

    /// At tolerance 0 every vertex is kept exactly: 550000007 * 1e-7 * 1e7
    /// is 550000006.9999999, which a truncating conversion stored one unit
    /// off.
    #[test]
    fn tolerance_zero_keeps_every_vertex_exactly() {
        let ring = vec![
            (550_000_007, 120_000_007),
            (550_000_007, 121_000_007),
            (551_000_007, 121_000_007),
            (551_000_007, 120_000_007),
            (550_000_007, 120_000_007),
        ];
        let mut way_geom = super::WayGeometryMap::for_tests();
        way_geom.insert_test(7, located(ring.clone()));
        let relations = vec![RawAdminRelation {
            id: 1,
            admin_level: 8,
            name_offset: 1,
            country_code_offset: 0,
            outer_way_ids: vec![7],
            inner_way_ids: Vec::new(),
        }];
        let config = BuildConfig {
            admin_tolerance_m: 0.0,
            ..BuildConfig::default()
        };
        let (polygons, _) = assemble(
            &relations,
            &way_geom,
            &config,
            &crate::host::Executor::for_tests(),
        );
        let stored: Vec<(i32, i32)> = polygons[0]
            .vertices
            .iter()
            .map(|v| (v.lat_e7, v.lon_e7))
            .collect();
        assert_eq!(stored, ring);
    }

    /// Complete member ways whose rings are not determined refuse their
    /// relation and are counted; determined shapes beside them publish every
    /// ring, whatever surrounds them.
    #[test]
    fn ambiguous_topology_is_refused_and_counted() {
        let mut way_geom = super::WayGeometryMap::for_tests();
        // A leaf into a complete graph on 12 nodes: a block that is not a
        // cycle, so its rings could be paired many ways.
        way_geom.insert_test(1, located(vec![(-1, -1), (0, 0)]));
        let mut way_id = 2;
        let mut complete_graph = vec![1];
        for a in 0..12 {
            for b in a + 1..12 {
                way_geom.insert_test(way_id, located(vec![(a, a), (b, b)]));
                complete_graph.push(way_id);
                way_id += 1;
            }
        }
        way_geom.insert_test(1_000, located(square_e7()));
        complete_graph.push(1_000);

        // A leaf followed by self-closed loops at its end node, beside a
        // disconnected closed square. The leaf is on no ring; every loop
        // and the square is one. This once exhausted a search budget and
        // lost the square too.
        let mut loops = vec![2_000];
        way_geom.insert_test(2_000, located(vec![(-5_000, -5_000), (0, 0)]));
        for k in 1..=8 {
            let id = 2_000 + k;
            let x = i32::try_from(k).unwrap() * 1_000;
            way_geom.insert_test(id, located(vec![(0, 0), (x, 100), (x, 200), (0, 0)]));
            loops.push(id);
        }
        loops.push(1_000);

        let relations = vec![
            relation(1, complete_graph, Vec::new()),
            relation(2, loops, Vec::new()),
            // One way as both shell and hole.
            relation(3, vec![1_000], vec![1_000]),
            // A way listed twice in one role is one boundary.
            relation(4, vec![1_000, 1_000], Vec::new()),
        ];
        let config = BuildConfig {
            admin_tolerance_m: 0.0,
            ..BuildConfig::default()
        };
        let (polygons, dropped) = assemble(
            &relations,
            &way_geom,
            &config,
            &crate::host::Executor::for_tests(),
        );
        let names: Vec<u32> = polygons.iter().map(|p| p.name_offset).collect();
        assert_eq!(names, [vec![2; 9], vec![4]].concat());
        assert_eq!(
            dropped,
            DroppedAdminRelations {
                total: 2,
                ambiguous_topology: 2,
                sample_relation_ids: vec![1, 3],
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
        let mut way_geom = super::WayGeometryMap::for_tests();
        way_geom.insert_test(1, located(ring_e7(&outer)));
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
            let (polygons, _) = assemble(
                &relations,
                &way_geom,
                &config,
                &crate::host::Executor::for_tests(),
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

    /// A 10 m exclave of B inside A, mapped as B's outer and A's hole, as
    /// German cadastral boundaries carry hundreds of them. At the default
    /// tolerance both rings once collapsed, so a query in the middle of the
    /// exclave answered A, the neighbour, at every level both were mapped at.
    #[test]
    fn an_exclave_inside_the_tolerance_keeps_its_answer() {
        let lat0: f64 = 51.37;
        let c = lat0.to_radians().cos();
        let d = crate::geo::EARTH_RADIUS_M.recip().to_degrees();
        let (w_lon, w_lat) = (10.0 * d / c, 10.0 * d);
        let mainland = rect_lonlat(12.0, 51.3, 12.4, 51.45);
        let exclave = rect_lonlat(12.2, lat0, 12.2 + w_lon, lat0 + w_lat);
        let mut way_geom = super::WayGeometryMap::for_tests();
        way_geom.insert_test(1, located(ring_e7(&mainland)));
        way_geom.insert_test(2, located(ring_e7(&exclave)));
        let relations = vec![
            RawAdminRelation {
                id: 1,
                admin_level: 4,
                name_offset: 1,
                country_code_offset: 0,
                outer_way_ids: vec![1],
                inner_way_ids: vec![2],
            },
            RawAdminRelation {
                id: 2,
                admin_level: 4,
                name_offset: 2,
                country_code_offset: 0,
                outer_way_ids: vec![2],
                inner_way_ids: Vec::new(),
            },
        ];
        let (polygons, _) = assemble(
            &relations,
            &way_geom,
            &BuildConfig::default(),
            &crate::host::Executor::for_tests(),
        );
        let middle = (12.2 + w_lon / 2.0, lat0 + w_lat / 2.0);
        let answers: Vec<u32> = polygons
            .iter()
            .filter(|p| stored_contains(&p.vertices, middle.0, middle.1))
            .map(|p| p.name_offset)
            .collect();
        assert_eq!(answers, vec![2], "the exclave's middle must answer B alone");
        // Passing counterpart: outside the exclave, A answers.
        let beside = (12.1, 51.4);
        let answers: Vec<u32> = polygons
            .iter()
            .filter(|p| stored_contains(&p.vertices, beside.0, beside.1))
            .map(|p| p.name_offset)
            .collect();
        assert_eq!(answers, vec![1]);
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
        let mut way_geom = super::WayGeometryMap::for_tests();
        way_geom.insert_test(1, located(ring_e7(&outer)));
        way_geom.insert_test(2, located(ring_e7(&hole)));
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
        let (polygons, _) = assemble(
            &relations,
            &way_geom,
            &config,
            &crate::host::Executor::for_tests(),
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
