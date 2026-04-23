//! Memory-mapped reader for the reverse geocoding index.
//!
//! Opens an index directory, memory-maps all files, and provides two query
//! levels: raw [`Candidates`] and ranked [`ReverseResult`].

use std::path::Path;

use memmap2::Mmap;
use s2::cellid::CellID;
use s2::latlng::LatLng;

use crate::geo;
use super::format::{
    self, AdminCell, AdminPolygon, AddrPoint, GeoCell, Header, InterpWay, NodeCoord,
    SegmentRef, StreetWay, ADMIN_CELL_SIZE, ADMIN_POLYGON_SIZE, ADDR_POINT_SIZE,
    GEO_CELL_SIZE, HEADER_SIZE, INDEX_MASK, INTERIOR_FLAG, INTERP_WAY_SIZE,
    NODE_COORD_SIZE, SEGMENT_REF_SIZE, STREET_WAY_SIZE,
};

// ---------------------------------------------------------------------------
// Public result types
// ---------------------------------------------------------------------------

/// Ranked result from [`Reader::query`].
#[derive(Debug)]
pub struct ReverseResult<'a> {
    pub address: Option<AddressMatch<'a>>,
    pub street: Option<StreetMatch<'a>>,
    pub interpolation: Option<InterpolationMatch<'a>>,
    /// One entry per admin level (2-11), smallest-area polygon at each level.
    pub admin: Vec<AdminMatch<'a>>,
}

#[derive(Debug)]
pub struct AddressMatch<'a> {
    pub lat_e7: i32,
    pub lon_e7: i32,
    pub house_number: &'a str,
    pub street: &'a str,
    pub postcode: Option<&'a str>,
    pub distance_m: f64,
}

#[derive(Debug)]
pub struct StreetMatch<'a> {
    pub name: &'a str,
    pub snap_lat_e7: i32,
    pub snap_lon_e7: i32,
    pub distance_m: f64,
}

#[derive(Debug)]
pub struct InterpolationMatch<'a> {
    pub street: &'a str,
    pub house_number: u32,
    pub distance_m: f64,
}

#[derive(Debug)]
pub struct AdminMatch<'a> {
    pub admin_level: u8,
    pub name: &'a str,
    /// ISO 3166-1 alpha2, only populated for admin_level=2.
    pub country_code: Option<&'a str>,
    /// Approximate area in square degrees (from the polygon record).
    /// Used by `into_result()` to pick the smallest polygon per level.
    pub area: f32,
}

/// Raw unranked candidates from [`Reader::candidates`].
#[derive(Debug)]
pub struct Candidates<'a> {
    pub addresses: Vec<AddressMatch<'a>>,
    pub streets: Vec<StreetMatch<'a>>,
    pub interpolations: Vec<InterpolationCandidate<'a>>,
    /// All admin polygons containing the query point (not collapsed per level).
    pub admin: Vec<AdminMatch<'a>>,
}

/// A raw interpolation hit (house number not yet computed).
#[derive(Debug)]
pub struct InterpolationCandidate<'a> {
    pub street: &'a str,
    pub way_index: u32,
    pub segment_index: u16,
    pub snap_lat_e7: i32,
    pub snap_lon_e7: i32,
    pub distance_m: f64,
}

impl<'a> Candidates<'a> {
    /// Apply default ranking: nearest of each type, smallest admin per level,
    /// postcode fallback from nearest address if no postal boundary.
    pub fn into_result(self, reader: &'a Reader) -> ReverseResult<'a> {
        let address = self
            .addresses
            .into_iter()
            .min_by(|a, b| a.distance_m.partial_cmp(&b.distance_m).unwrap_or(std::cmp::Ordering::Equal));

        let street = self
            .streets
            .into_iter()
            .min_by(|a, b| a.distance_m.partial_cmp(&b.distance_m).unwrap_or(std::cmp::Ordering::Equal));

        let interpolation = self
            .interpolations
            .iter()
            .min_by(|a, b| a.distance_m.partial_cmp(&b.distance_m).unwrap_or(std::cmp::Ordering::Equal))
            .and_then(|c| {
                let hn = reader.interpolate(c)?;
                Some(InterpolationMatch {
                    street: c.street,
                    house_number: hn,
                    distance_m: c.distance_m,
                })
            });

        // Collapse admin to smallest-area per level (matches query() semantics)
        let mut best_by_level: [Option<AdminMatch<'a>>; 12] = Default::default();
        for m in self.admin {
            let level = m.admin_level as usize;
            if level < 12 {
                let dominated = best_by_level[level]
                    .as_ref()
                    .is_none_or(|existing| m.area < existing.area);
                if dominated {
                    best_by_level[level] = Some(m);
                }
            }
        }
        let admin: Vec<_> = best_by_level.into_iter().flatten().collect();

        ReverseResult {
            address,
            street,
            interpolation,
            admin,
        }
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// A memory-mapped reverse geocoding index reader.
///
/// `Send + Sync` - all fields are `Mmap` (which is `Send + Sync`) or plain
/// values.
pub struct Reader {
    header: Header,
    fine_max_dist_sq: f64,
    coarse_max_dist_sq: f64,

    geo_cells: Mmap,
    street_entries: Mmap,
    addr_entries: Mmap,
    interp_entries: Mmap,
    coarse_geo_cells: Mmap,
    coarse_street_entries: Mmap,
    coarse_addr_entries: Mmap,
    coarse_interp_entries: Mmap,
    street_ways: Mmap,
    street_nodes: Mmap,
    addr_points: Mmap,
    interp_ways: Mmap,
    interp_nodes: Mmap,
    admin_cells: Mmap,
    admin_entries: Mmap,
    admin_polygons: Mmap,
    admin_vertices: Mmap,
    strings: Mmap,
}

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

impl Reader {
    /// Open an index directory. Memory-maps all files.
    pub fn open(dir: &Path) -> Result<Self> {
        let header_bytes = std::fs::read(dir.join(format::FILE_HEADER))?;
        if header_bytes.len() < HEADER_SIZE {
            return Err("geocode header too short".into());
        }
        let header = Header::from_bytes(
            header_bytes[..HEADER_SIZE]
                .try_into()
                .map_err(|_| "header size mismatch")?,
        )?;

        let mmap = |name: &str| -> Result<Mmap> {
            let path = dir.join(name);
            let file = std::fs::File::open(&path)
                .map_err(|e| format!("failed to open {}: {e}", path.display()))?;
            let len = file.metadata()
                .map_err(|e| format!("failed to stat {}: {e}", path.display()))?
                .len();
            if len == 0 {
                // memmap2::Mmap::map() fails on zero-length files.
                // Return an empty read-only anonymous mmap instead.
                // All consumers check record bounds before reading,
                // so an empty mmap (len=0) is handled correctly.
                return Ok(
                    memmap2::MmapOptions::new().map_anon()
                        .map_err(|e| format!("failed to create empty mmap for {}: {e}", path.display()))?
                        .make_read_only()
                        .map_err(|e| format!("failed to make anon mmap read-only for {}: {e}", path.display()))?
                );
            }
            // SAFETY: the file is read-only for the lifetime of the Reader.
            let m = unsafe { Mmap::map(&file) }
                .map_err(|e| format!("failed to mmap {}: {e}", path.display()))?;
            Ok(m)
        };

        let fine_radius_m = header.fine_search_radius_m as f64;
        let coarse_radius_m = header.coarse_search_radius_m as f64;

        Ok(Self {
            fine_max_dist_sq: geo::meters_to_radians_sq(fine_radius_m),
            coarse_max_dist_sq: geo::meters_to_radians_sq(coarse_radius_m),
            header,
            geo_cells: mmap(format::FILE_GEO_CELLS)?,
            street_entries: mmap(format::FILE_STREET_ENTRIES)?,
            addr_entries: mmap(format::FILE_ADDR_ENTRIES)?,
            interp_entries: mmap(format::FILE_INTERP_ENTRIES)?,
            coarse_geo_cells: mmap(format::FILE_COARSE_GEO_CELLS)?,
            coarse_street_entries: mmap(format::FILE_COARSE_STREET_ENTRIES)?,
            coarse_addr_entries: mmap(format::FILE_COARSE_ADDR_ENTRIES)?,
            coarse_interp_entries: mmap(format::FILE_COARSE_INTERP_ENTRIES)?,
            street_ways: mmap(format::FILE_STREET_WAYS)?,
            street_nodes: mmap(format::FILE_STREET_NODES)?,
            addr_points: mmap(format::FILE_ADDR_POINTS)?,
            interp_ways: mmap(format::FILE_INTERP_WAYS)?,
            interp_nodes: mmap(format::FILE_INTERP_NODES)?,
            admin_cells: mmap(format::FILE_ADMIN_CELLS)?,
            admin_entries: mmap(format::FILE_ADMIN_ENTRIES)?,
            admin_polygons: mmap(format::FILE_ADMIN_POLYGONS)?,
            admin_vertices: mmap(format::FILE_ADMIN_VERTICES)?,
            strings: mmap(format::FILE_STRINGS)?,
        })
    }

    // -- Public query API ----------------------------------------------------

    /// High-level query: nearest of each type, allocation-free on the hot path.
    /// Internally iterates mmap'd entry lists without materializing Vecs.
    pub fn query(&self, lat: f64, lon: f64) -> ReverseResult<'_> {
        let ctx = QueryContext::new(lat, lon);
        let mut best = BestTracker::new();

        // Fine-level search
        self.search_street_level_best(
            &ctx,
            &self.geo_cells,
            &self.street_entries,
            &self.addr_entries,
            &self.interp_entries,
            self.header.street_cell_level,
            self.fine_max_dist_sq,
            &mut best,
        );

        // Coarse fallback
        if best.street.is_none() && best.addr.is_none() {
            self.search_street_level_best(
                &ctx,
                &self.coarse_geo_cells,
                &self.coarse_street_entries,
                &self.coarse_addr_entries,
                &self.coarse_interp_entries,
                self.header.coarse_cell_level,
                self.coarse_max_dist_sq,
                &mut best,
            );
        }

        // Admin lookup (collapsed to one per level)
        let admin = self.search_admin_ranked(&ctx);

        // Assemble
        let address = best.addr.map(|(idx, dist_sq)| {
            let pt = self.read_addr_point(idx);
            AddressMatch {
                lat_e7: pt.lat_e7,
                lon_e7: pt.lon_e7,
                house_number: self.read_string(pt.housenumber_offset),
                street: self.read_string(pt.street_offset),
                postcode: nonzero_string(self, pt.postcode_offset),
                distance_m: radians_sq_to_meters(dist_sq),
            }
        });

        let street = best.street.map(|(way_idx, snap_lat, snap_lon, dist_sq)| {
            let way = self.read_street_way(way_idx);
            StreetMatch {
                name: self.read_string(way.name_offset),
                snap_lat_e7: snap_lat,
                snap_lon_e7: snap_lon,
                distance_m: radians_sq_to_meters(dist_sq),
            }
        });

        let interpolation = best
            .interp
            .and_then(|(way_idx, seg_idx, snap_lat, snap_lon, dist_sq)| {
                self.resolve_interpolation(way_idx, seg_idx, snap_lat, snap_lon, dist_sq)
            });

        ReverseResult {
            address,
            street,
            interpolation,
            admin,
        }
    }

    /// Low-level query: all candidates within radius, unranked.
    pub fn candidates(&self, lat: f64, lon: f64) -> Candidates<'_> {
        let ctx = QueryContext::new(lat, lon);
        let mut cands = CandidateCollector::new();

        self.collect_candidates(
            &ctx,
            &self.geo_cells,
            &self.street_entries,
            &self.addr_entries,
            &self.interp_entries,
            self.header.street_cell_level,
            self.fine_max_dist_sq,
            &mut cands,
        );

        if cands.streets.is_empty() && cands.addresses.is_empty() {
            self.collect_candidates(
                &ctx,
                &self.coarse_geo_cells,
                &self.coarse_street_entries,
                &self.coarse_addr_entries,
                &self.coarse_interp_entries,
                self.header.coarse_cell_level,
                self.coarse_max_dist_sq,
                &mut cands,
            );
        }

        // Admin: return ALL containing polygons, not collapsed
        let admin = self.search_admin_all(&ctx);

        Candidates {
            addresses: cands.addresses,
            streets: cands.streets,
            interpolations: cands.interpolations,
            admin,
        }
    }

    /// Compute the interpolated house number for a raw candidate.
    pub fn interpolate(&self, candidate: &InterpolationCandidate<'_>) -> Option<u32> {
        let iw = self.read_interp_way(candidate.way_index);
        if iw.start_number == 0 || iw.end_number == 0 {
            return None;
        }
        let total_len = self.way_length(&self.interp_nodes, iw.node_offset, iw.node_count);
        if total_len < 1e-15 {
            return None;
        }
        let acc_len = self.accumulated_length(
            &self.interp_nodes,
            iw.node_offset,
            candidate.segment_index,
            candidate.snap_lat_e7,
            candidate.snap_lon_e7,
        );
        let t = acc_len / total_len;
        let diff = iw.end_number as f64 - iw.start_number as f64;
        let number = match iw.interpolation_type {
            0 => iw.start_number as f64 + (t * diff).round(),
            _ => iw.start_number as f64 + 2.0 * ((t * diff) / 2.0).round(),
        };
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        Some(number.max(0.0) as u32)
    }

    // -- Header accessors ----------------------------------------------------

    pub fn replication_sequence(&self) -> u32 {
        self.header.replication_sequence
    }

    pub fn replication_timestamp(&self) -> u64 {
        self.header.replication_timestamp
    }

    pub fn street_cell_level(&self) -> u8 {
        self.header.street_cell_level
    }

    pub fn coarse_cell_level(&self) -> u8 {
        self.header.coarse_cell_level
    }

    pub fn admin_cell_level(&self) -> u8 {
        self.header.admin_cell_level
    }
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

struct QueryContext {
    lat: f64,
    lon: f64,
    lat_rad: f64,
    lon_rad: f64,
    cos_lat: f64,
}

impl QueryContext {
    fn new(lat: f64, lon: f64) -> Self {
        let lat_rad = lat.to_radians();
        Self {
            lat,
            lon,
            lat_rad,
            lon_rad: lon.to_radians(),
            cos_lat: lat_rad.cos(),
        }
    }
}

/// Tracks nearest candidate of each type for `query()` (allocation-free).
struct BestTracker {
    addr: Option<(u32, f64)>,                   // (index, dist_sq)
    street: Option<(u32, i32, i32, f64)>,       // (way_idx, snap_lat, snap_lon, dist_sq)
    interp: Option<(u32, u16, i32, i32, f64)>,  // (way_idx, seg_idx, snap_lat, snap_lon, dist_sq)
}

impl BestTracker {
    fn new() -> Self {
        Self { addr: None, street: None, interp: None }
    }
}

struct CandidateCollector<'a> {
    addresses: Vec<AddressMatch<'a>>,
    streets: Vec<StreetMatch<'a>>,
    interpolations: Vec<InterpolationCandidate<'a>>,
}

impl<'a> CandidateCollector<'a> {
    fn new() -> Self {
        Self {
            addresses: Vec::new(),
            streets: Vec::new(),
            interpolations: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Zero-allocation entry iterators over mmap'd bytes
// ---------------------------------------------------------------------------

/// Iterator over u32 entries (addr_entries, interp_entries with u32 offset).
struct U32EntryIter<'a> {
    data: &'a [u8],
    pos: usize,
    remaining: u16,
}

impl<'a> U32EntryIter<'a> {
    #[allow(clippy::cast_possible_truncation)] // u64→usize: Linux 64-bit only
    fn new(mmap: &'a [u8], offset: u64) -> Self {
        let off = offset as usize;
        if off + 2 > mmap.len() {
            return Self { data: mmap, pos: 0, remaining: 0 };
        }
        let count = u16::from_le_bytes([mmap[off], mmap[off + 1]]);
        Self { data: mmap, pos: off + 2, remaining: count }
    }
}

impl Iterator for U32EntryIter<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        if self.remaining == 0 {
            return None;
        }
        let end = self.pos + 4;
        if end > self.data.len() {
            self.remaining = 0;
            return None;
        }
        let val = u32::from_le_bytes([
            self.data[self.pos],
            self.data[self.pos + 1],
            self.data[self.pos + 2],
            self.data[self.pos + 3],
        ]);
        self.pos = end;
        self.remaining -= 1;
        Some(val)
    }
}

/// Iterator over segment ref entries (6 bytes each).
struct SegmentEntryIter<'a> {
    data: &'a [u8],
    pos: usize,
    remaining: u16,
}

impl<'a> SegmentEntryIter<'a> {
    #[allow(clippy::cast_possible_truncation)] // u64→usize: Linux 64-bit only
    fn new(mmap: &'a [u8], offset: u64) -> Self {
        let off = offset as usize;
        if off + 2 > mmap.len() {
            return Self { data: mmap, pos: 0, remaining: 0 };
        }
        let count = u16::from_le_bytes([mmap[off], mmap[off + 1]]);
        Self { data: mmap, pos: off + 2, remaining: count }
    }

}

impl Iterator for SegmentEntryIter<'_> {
    type Item = SegmentRef;

    fn next(&mut self) -> Option<SegmentRef> {
        if self.remaining == 0 {
            return None;
        }
        let end = self.pos + SEGMENT_REF_SIZE;
        if end > self.data.len() {
            self.remaining = 0;
            return None;
        }
        let rec: &[u8; SEGMENT_REF_SIZE] = self.data[self.pos..end].try_into().ok()?;
        self.pos = end;
        self.remaining -= 1;
        Some(SegmentRef::from_bytes(rec))
    }
}

/// Iterator over admin entries (u32 values with interior flag).
struct AdminEntryIter<'a> {
    data: &'a [u8],
    pos: usize,
    remaining: u16,
}

impl<'a> AdminEntryIter<'a> {
    fn new(mmap: &'a [u8], offset: u32) -> Self {
        let off = offset as usize;
        if off + 2 > mmap.len() {
            return Self { data: mmap, pos: 0, remaining: 0 };
        }
        let count = u16::from_le_bytes([mmap[off], mmap[off + 1]]);
        Self { data: mmap, pos: off + 2, remaining: count }
    }
}

impl Iterator for AdminEntryIter<'_> {
    type Item = (u32, bool); // (polygon_index, is_interior_hint)

    fn next(&mut self) -> Option<(u32, bool)> {
        if self.remaining == 0 {
            return None;
        }
        let end = self.pos + 4;
        if end > self.data.len() {
            self.remaining = 0;
            return None;
        }
        let raw = u32::from_le_bytes([
            self.data[self.pos],
            self.data[self.pos + 1],
            self.data[self.pos + 2],
            self.data[self.pos + 3],
        ]);
        self.pos = end;
        self.remaining -= 1;
        Some((raw & INDEX_MASK, raw & INTERIOR_FLAG != 0))
    }
}

// ---------------------------------------------------------------------------
// S2 cell helpers (fixed-size array, no allocation)
// ---------------------------------------------------------------------------

/// Maximum cells in a neighborhood: 1 center + up to 8 neighbors.
const MAX_NEIGHBORHOOD: usize = 9;

/// Returns the cell + all neighbors at the given level as a fixed-size array.
///
/// S2 guarantees exactly 8 edge/corner neighbors for any non-face cell
/// at any level, so the `min(MAX_NEIGHBORHOOD - 1)` and `take(...)`
/// below are defensive clamps, not silent truncation: the S2 contract
/// cannot produce more than 8 neighbors at this API level. If the
/// upstream S2 crate ever changes that, update the constant to match.
fn cell_neighborhood(lat: f64, lon: f64, level: u8) -> ([u64; MAX_NEIGHBORHOOD], usize) {
    let ll = LatLng::from_degrees(lat, lon);
    let cell = CellID::from(ll).parent(level as u64);
    let neighbors = cell.all_neighbors(level as u64);
    let mut cells = [0u64; MAX_NEIGHBORHOOD];
    cells[0] = cell.0;
    let count = 1 + neighbors.len().min(MAX_NEIGHBORHOOD - 1);
    for (i, n) in neighbors.iter().enumerate().take(MAX_NEIGHBORHOOD - 1) {
        cells[i + 1] = n.0;
    }
    (cells, count)
}

// ---------------------------------------------------------------------------
// Search implementations
// ---------------------------------------------------------------------------

impl Reader {
    /// Allocation-free search: iterates entry lists via mmap iterators,
    /// tracks only the nearest candidate.
    #[allow(clippy::too_many_arguments)]
    fn search_street_level_best(
        &self,
        ctx: &QueryContext,
        geo_cells_mmap: &Mmap,
        street_entries_mmap: &Mmap,
        addr_entries_mmap: &Mmap,
        interp_entries_mmap: &Mmap,
        level: u8,
        max_dist_sq: f64,
        best: &mut BestTracker,
    ) {
        let (cells, count) = cell_neighborhood(ctx.lat, ctx.lon, level);
        let cell_count = geo_cells_mmap.len() / GEO_CELL_SIZE;

        for &cell_id in &cells[..count] {
            let Some(gc) = binary_search_cells(geo_cells_mmap, cell_count, cell_id) else {
                continue;
            };

            // Address points
            if gc.addr_offset != format::NO_DATA_U64 {
                for idx in U32EntryIter::new(addr_entries_mmap, gc.addr_offset) {
                    let pt = self.read_addr_point(idx);
                    let dist_sq = geo::approx_distance_sq(
                        ctx.lat_rad, ctx.lon_rad,
                        geo::e7_to_rad(pt.lat_e7), geo::e7_to_rad(pt.lon_e7),
                        ctx.cos_lat,
                    );
                    if dist_sq < max_dist_sq
                        && (best.addr.is_none() || dist_sq < best.addr.map_or(f64::MAX, |b| b.1))
                    {
                        best.addr = Some((idx, dist_sq));
                    }
                }
            }

            // Street segments
            if gc.street_offset != format::NO_DATA_U64 {
                for seg_ref in SegmentEntryIter::new(street_entries_mmap, gc.street_offset) {
                    let (snap_lat, snap_lon, dist_sq) =
                        self.street_segment_distance(ctx, &seg_ref);
                    if dist_sq < max_dist_sq
                        && (best.street.is_none() || dist_sq < best.street.map_or(f64::MAX, |b| b.3))
                    {
                        best.street = Some((seg_ref.way_index, snap_lat, snap_lon, dist_sq));
                    }
                }
            }

            // Interpolation segments
            if gc.interp_offset != format::NO_DATA_U64 {
                for seg_ref in SegmentEntryIter::new(interp_entries_mmap, gc.interp_offset) {
                    let (snap_lat, snap_lon, dist_sq) =
                        self.interp_segment_distance(ctx, &seg_ref);
                    if dist_sq < max_dist_sq
                        && (best.interp.is_none() || dist_sq < best.interp.map_or(f64::MAX, |b| b.4))
                    {
                        best.interp = Some((
                            seg_ref.way_index, seg_ref.segment_index,
                            snap_lat, snap_lon, dist_sq,
                        ));
                    }
                }
            }
        }
    }

    /// Collecting search: materializes all candidates within radius.
    #[allow(clippy::too_many_arguments)]
    fn collect_candidates<'a>(
        &'a self,
        ctx: &QueryContext,
        geo_cells_mmap: &Mmap,
        street_entries_mmap: &Mmap,
        addr_entries_mmap: &Mmap,
        interp_entries_mmap: &Mmap,
        level: u8,
        max_dist_sq: f64,
        cands: &mut CandidateCollector<'a>,
    ) {
        let (cells, count) = cell_neighborhood(ctx.lat, ctx.lon, level);
        let cell_count = geo_cells_mmap.len() / GEO_CELL_SIZE;

        for &cell_id in &cells[..count] {
            let Some(gc) = binary_search_cells(geo_cells_mmap, cell_count, cell_id) else {
                continue;
            };

            if gc.addr_offset != format::NO_DATA_U64 {
                for idx in U32EntryIter::new(addr_entries_mmap, gc.addr_offset) {
                    let pt = self.read_addr_point(idx);
                    let dist_sq = geo::approx_distance_sq(
                        ctx.lat_rad, ctx.lon_rad,
                        geo::e7_to_rad(pt.lat_e7), geo::e7_to_rad(pt.lon_e7),
                        ctx.cos_lat,
                    );
                    if dist_sq < max_dist_sq {
                        cands.addresses.push(AddressMatch {
                            lat_e7: pt.lat_e7,
                            lon_e7: pt.lon_e7,
                            house_number: self.read_string(pt.housenumber_offset),
                            street: self.read_string(pt.street_offset),
                            postcode: nonzero_string(self, pt.postcode_offset),
                            distance_m: radians_sq_to_meters(dist_sq),
                        });
                    }
                }
            }

            if gc.street_offset != format::NO_DATA_U64 {
                for seg_ref in SegmentEntryIter::new(street_entries_mmap, gc.street_offset) {
                    let (snap_lat, snap_lon, dist_sq) =
                        self.street_segment_distance(ctx, &seg_ref);
                    if dist_sq < max_dist_sq {
                        let way = self.read_street_way(seg_ref.way_index);
                        cands.streets.push(StreetMatch {
                            name: self.read_string(way.name_offset),
                            snap_lat_e7: snap_lat,
                            snap_lon_e7: snap_lon,
                            distance_m: radians_sq_to_meters(dist_sq),
                        });
                    }
                }
            }

            if gc.interp_offset != format::NO_DATA_U64 {
                for seg_ref in SegmentEntryIter::new(interp_entries_mmap, gc.interp_offset) {
                    let (snap_lat, snap_lon, dist_sq) =
                        self.interp_segment_distance(ctx, &seg_ref);
                    if dist_sq < max_dist_sq {
                        let iw = self.read_interp_way(seg_ref.way_index);
                        cands.interpolations.push(InterpolationCandidate {
                            street: self.read_string(iw.street_offset),
                            way_index: seg_ref.way_index,
                            segment_index: seg_ref.segment_index,
                            snap_lat_e7: snap_lat,
                            snap_lon_e7: snap_lon,
                            distance_m: radians_sq_to_meters(dist_sq),
                        });
                    }
                }
            }
        }
    }

    // -- Admin search --------------------------------------------------------

    /// Ranked admin: one per level, smallest area, with interior cell optimization.
    fn search_admin_ranked<'a>(&'a self, ctx: &QueryContext) -> Vec<AdminMatch<'a>> {
        let (cells, count) = cell_neighborhood(ctx.lat, ctx.lon, self.header.admin_cell_level);
        let cell_count = self.admin_cells.len() / ADMIN_CELL_SIZE;
        let mut best_by_level: [(Option<u32>, f32); 12] = [(None, f32::MAX); 12];

        for &cell_id in &cells[..count] {
            let Some(ac) = binary_search_admin_cells(&self.admin_cells, cell_count, cell_id) else {
                continue;
            };
            for (poly_idx, is_interior) in AdminEntryIter::new(&self.admin_entries, ac.entries_offset) {
                let poly = self.read_admin_polygon(poly_idx);
                let level = poly.admin_level as usize;
                if level >= 12 || poly.area >= best_by_level[level].1 {
                    continue;
                }
                // Interior hint: skip PIP test (accepted approximation per spec)
                if is_interior || self.admin_polygon_contains(ctx, &poly) {
                    best_by_level[level] = (Some(poly_idx), poly.area);
                }
            }
        }

        let mut result = Vec::new();
        for &(maybe_idx, _) in &best_by_level {
            if let Some(poly_idx) = maybe_idx {
                let poly = self.read_admin_polygon(poly_idx);
                result.push(AdminMatch {
                    admin_level: poly.admin_level,
                    name: self.read_string(poly.name_offset),
                    country_code: nonzero_string(self, poly.country_code_offset),
                    area: poly.area,
                });
            }
        }
        result
    }

    /// All admin polygons containing the point (for candidates API).
    fn search_admin_all<'a>(&'a self, ctx: &QueryContext) -> Vec<AdminMatch<'a>> {
        let (cells, count) = cell_neighborhood(ctx.lat, ctx.lon, self.header.admin_cell_level);
        let cell_count = self.admin_cells.len() / ADMIN_CELL_SIZE;
        let mut result = Vec::new();
        let mut seen: Vec<u32> = Vec::new(); // dedup polygon IDs across cells

        for &cell_id in &cells[..count] {
            let Some(ac) = binary_search_admin_cells(&self.admin_cells, cell_count, cell_id) else {
                continue;
            };
            for (poly_idx, is_interior) in AdminEntryIter::new(&self.admin_entries, ac.entries_offset) {
                if seen.contains(&poly_idx) {
                    continue;
                }
                let poly = self.read_admin_polygon(poly_idx);
                if is_interior || self.admin_polygon_contains(ctx, &poly) {
                    seen.push(poly_idx);
                    result.push(AdminMatch {
                        admin_level: poly.admin_level,
                        name: self.read_string(poly.name_offset),
                        country_code: nonzero_string(self, poly.country_code_offset),
                        area: poly.area,
                    });
                }
            }
        }
        result
    }

    fn admin_polygon_contains(&self, ctx: &QueryContext, poly: &AdminPolygon) -> bool {
        let start = poly.vertex_offset as usize;
        let count = poly.vertex_count as usize;
        if count < 3 {
            return false;
        }

        // Parse vertices into rings separated by sentinel
        let coords = (0..count).map_while(|i| {
            let offset = start + i * NODE_COORD_SIZE;
            let rec = read_record::<NODE_COORD_SIZE>(&self.admin_vertices, offset)?;
            Some(NodeCoord::from_bytes(rec))
        });
        let rings = format::parse_rings(coords);
        if rings.is_empty() {
            return false;
        }

        let exterior = &rings[0];
        let holes: Vec<&[(f64, f64)]> = rings[1..].iter().map(Vec::as_slice).collect();
        geo::point_in_polygon(ctx.lon, ctx.lat, exterior, &holes)
    }

    // -- Segment distance ----------------------------------------------------

    #[allow(clippy::cast_possible_truncation)]
    fn street_segment_distance(&self, ctx: &QueryContext, seg_ref: &SegmentRef) -> (i32, i32, f64) {
        let way_offset = seg_ref.way_index as usize * STREET_WAY_SIZE;
        let Some(rec) = read_record::<STREET_WAY_SIZE>(&self.street_ways, way_offset) else {
            return (0, 0, f64::MAX);
        };
        let way = StreetWay::from_bytes(rec);
        let node_byte_offset =
            way.node_offset as usize + seg_ref.segment_index as usize * NODE_COORD_SIZE;
        two_node_distance(ctx, &self.street_nodes, node_byte_offset)
    }

    #[allow(clippy::cast_possible_truncation)]
    fn interp_segment_distance(&self, ctx: &QueryContext, seg_ref: &SegmentRef) -> (i32, i32, f64) {
        let way_offset = seg_ref.way_index as usize * INTERP_WAY_SIZE;
        let Some(rec) = read_record::<INTERP_WAY_SIZE>(&self.interp_ways, way_offset) else {
            return (0, 0, f64::MAX);
        };
        let way = InterpWay::from_bytes(rec);
        let node_byte_offset =
            way.node_offset as usize + seg_ref.segment_index as usize * NODE_COORD_SIZE;
        two_node_distance(ctx, &self.interp_nodes, node_byte_offset)
    }

    // -- Record readers ------------------------------------------------------

    fn read_addr_point(&self, index: u32) -> AddrPoint {
        let offset = index as usize * ADDR_POINT_SIZE;
        read_record::<ADDR_POINT_SIZE>(&self.addr_points, offset)
            .map(AddrPoint::from_bytes)
            .unwrap_or(AddrPoint {
                lat_e7: 0, lon_e7: 0,
                housenumber_offset: 0, street_offset: 0, postcode_offset: 0,
            })
    }

    fn read_street_way(&self, index: u32) -> StreetWay {
        let offset = index as usize * STREET_WAY_SIZE;
        read_record::<STREET_WAY_SIZE>(&self.street_ways, offset)
            .map(StreetWay::from_bytes)
            .unwrap_or(StreetWay { node_offset: 0, name_offset: 0, node_count: 0 })
    }

    fn read_interp_way(&self, index: u32) -> InterpWay {
        let offset = index as usize * INTERP_WAY_SIZE;
        read_record::<INTERP_WAY_SIZE>(&self.interp_ways, offset)
            .map(InterpWay::from_bytes)
            .unwrap_or(InterpWay {
                node_offset: 0, street_offset: 0,
                start_number: 0, end_number: 0, node_count: 0, interpolation_type: 0,
            })
    }

    fn read_admin_polygon(&self, index: u32) -> AdminPolygon {
        let offset = index as usize * ADMIN_POLYGON_SIZE;
        read_record::<ADMIN_POLYGON_SIZE>(&self.admin_polygons, offset)
            .map(AdminPolygon::from_bytes)
            .unwrap_or(AdminPolygon {
                area: 0.0, vertex_offset: 0, vertex_count: 0,
                name_offset: 0, country_code_offset: 0, admin_level: 0,
            })
    }

    fn read_string(&self, offset: u32) -> &str {
        format::read_nul_string(&self.strings, offset)
    }

    // -- Interpolation helpers -----------------------------------------------

    fn resolve_interpolation(
        &self,
        way_idx: u32,
        seg_idx: u16,
        snap_lat: i32,
        snap_lon: i32,
        dist_sq: f64,
    ) -> Option<InterpolationMatch<'_>> {
        let iw = self.read_interp_way(way_idx);
        if iw.start_number == 0 || iw.end_number == 0 {
            return None;
        }
        let total_len = self.way_length(&self.interp_nodes, iw.node_offset, iw.node_count);
        if total_len < 1e-15 {
            return None;
        }
        let acc_len = self.accumulated_length(
            &self.interp_nodes, iw.node_offset, seg_idx, snap_lat, snap_lon,
        );
        let t = acc_len / total_len;
        let diff = iw.end_number as f64 - iw.start_number as f64;
        let number = match iw.interpolation_type {
            0 => iw.start_number as f64 + (t * diff).round(),
            _ => iw.start_number as f64 + 2.0 * ((t * diff) / 2.0).round(),
        };
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let house_number = number.max(0.0) as u32;
        Some(InterpolationMatch {
            street: self.read_string(iw.street_offset),
            house_number,
            distance_m: radians_sq_to_meters(dist_sq),
        })
    }

    #[allow(clippy::cast_possible_truncation)]
    fn way_length(&self, nodes_mmap: &Mmap, node_offset: u64, node_count: u16) -> f64 {
        let mut total = 0.0;
        let base = node_offset as usize;
        for i in 0..(node_count as usize).saturating_sub(1) {
            let a = read_record::<NODE_COORD_SIZE>(nodes_mmap, base + i * NODE_COORD_SIZE)
                .map(NodeCoord::from_bytes);
            let b = read_record::<NODE_COORD_SIZE>(nodes_mmap, base + (i + 1) * NODE_COORD_SIZE)
                .map(NodeCoord::from_bytes);
            if let (Some(a), Some(b)) = (a, b) {
                total += segment_length(&a, &b);
            }
        }
        total
    }

    #[allow(clippy::cast_possible_truncation)]
    fn accumulated_length(
        &self,
        nodes_mmap: &Mmap,
        node_offset: u64,
        seg_idx: u16,
        snap_lat: i32,
        snap_lon: i32,
    ) -> f64 {
        let mut acc = 0.0;
        let base = node_offset as usize;
        for i in 0..seg_idx as usize {
            let a = read_record::<NODE_COORD_SIZE>(nodes_mmap, base + i * NODE_COORD_SIZE)
                .map(NodeCoord::from_bytes);
            let b = read_record::<NODE_COORD_SIZE>(nodes_mmap, base + (i + 1) * NODE_COORD_SIZE)
                .map(NodeCoord::from_bytes);
            if let (Some(a), Some(b)) = (a, b) {
                acc += segment_length(&a, &b);
            }
        }
        // Partial segment to snap point
        if let Some(rec) = read_record::<NODE_COORD_SIZE>(
            nodes_mmap, base + seg_idx as usize * NODE_COORD_SIZE,
        ) {
            let seg_start = NodeCoord::from_bytes(rec);
            let snap = NodeCoord { lat_e7: snap_lat, lon_e7: snap_lon };
            acc += segment_length(&seg_start, &snap);
        }
        acc
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

fn two_node_distance(ctx: &QueryContext, nodes_mmap: &Mmap, byte_offset: usize) -> (i32, i32, f64) {
    let a = read_record::<NODE_COORD_SIZE>(nodes_mmap, byte_offset).map(NodeCoord::from_bytes);
    let b = read_record::<NODE_COORD_SIZE>(nodes_mmap, byte_offset + NODE_COORD_SIZE)
        .map(NodeCoord::from_bytes);
    let (Some(a), Some(b)) = (a, b) else {
        return (0, 0, f64::MAX);
    };
    let (t, dist_sq) = geo::point_to_segment_distance_sq(
        ctx.lon_rad, ctx.lat_rad,
        geo::e7_to_rad(a.lon_e7), geo::e7_to_rad(a.lat_e7),
        geo::e7_to_rad(b.lon_e7), geo::e7_to_rad(b.lat_e7),
        ctx.cos_lat,
    );
    #[allow(clippy::cast_possible_truncation)]
    let snap_lat = (a.lat_e7 as f64 + t * (b.lat_e7 - a.lat_e7) as f64) as i32;
    #[allow(clippy::cast_possible_truncation)]
    let snap_lon = (a.lon_e7 as f64 + t * (b.lon_e7 - a.lon_e7) as f64) as i32;
    (snap_lat, snap_lon, dist_sq)
}

fn segment_length(a: &NodeCoord, b: &NodeCoord) -> f64 {
    geo::approx_distance_sq(
        geo::e7_to_rad(a.lat_e7), geo::e7_to_rad(a.lon_e7),
        geo::e7_to_rad(b.lat_e7), geo::e7_to_rad(b.lon_e7),
        geo::e7_to_rad(a.lat_e7).cos(),
    )
    .sqrt()
}

fn binary_search_cells(mmap: &[u8], count: usize, target: u64) -> Option<GeoCell> {
    let mut lo = 0usize;
    let mut hi = count;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let rec = read_record::<GEO_CELL_SIZE>(mmap, mid * GEO_CELL_SIZE)?;
        let gc = GeoCell::from_bytes(rec);
        match gc.cell_id.cmp(&target) {
            std::cmp::Ordering::Equal => return Some(gc),
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
        }
    }
    None
}

fn binary_search_admin_cells(mmap: &[u8], count: usize, target: u64) -> Option<AdminCell> {
    let mut lo = 0usize;
    let mut hi = count;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let rec = read_record::<ADMIN_CELL_SIZE>(mmap, mid * ADMIN_CELL_SIZE)?;
        let ac = AdminCell::from_bytes(rec);
        match ac.cell_id.cmp(&target) {
            std::cmp::Ordering::Equal => return Some(ac),
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
        }
    }
    None
}

fn read_record<const N: usize>(mmap: &[u8], offset: usize) -> Option<&[u8; N]> {
    mmap.get(offset..offset + N)?.try_into().ok()
}

fn radians_sq_to_meters(rad_sq: f64) -> f64 {
    rad_sq.sqrt() * geo::EARTH_RADIUS_M
}

fn nonzero_string(reader: &Reader, offset: u32) -> Option<&str> {
    if offset == 0 { None } else { Some(reader.read_string(offset)) }
}
