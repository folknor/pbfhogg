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
    pub admin: Vec<AdminMatch<'a>>,
}

/// A matched address point.
#[derive(Debug)]
pub struct AddressMatch<'a> {
    pub lat_e7: i32,
    pub lon_e7: i32,
    pub house_number: &'a str,
    pub street: &'a str,
    pub postcode: Option<&'a str>,
    pub distance_m: f64,
}

/// A matched street segment.
#[derive(Debug)]
pub struct StreetMatch<'a> {
    pub name: &'a str,
    pub snap_lat_e7: i32,
    pub snap_lon_e7: i32,
    pub distance_m: f64,
}

/// A fully resolved interpolation match.
#[derive(Debug)]
pub struct InterpolationMatch<'a> {
    pub street: &'a str,
    pub house_number: u32,
    pub distance_m: f64,
}

/// An admin boundary match.
#[derive(Debug)]
pub struct AdminMatch<'a> {
    pub admin_level: u8,
    pub name: &'a str,
    pub country_code: Option<&'a str>,
}

/// Raw unranked candidates from [`Reader::candidates`].
#[derive(Debug)]
pub struct Candidates<'a> {
    pub addresses: Vec<AddressMatch<'a>>,
    pub streets: Vec<StreetMatch<'a>>,
    pub interpolations: Vec<InterpolationCandidate<'a>>,
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

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// A memory-mapped reverse geocoding index reader.
///
/// `Send + Sync` — all state is in read-only mmap'd files. Multiple threads
/// can call `query()` or `candidates()` concurrently.
pub struct Reader {
    header: Header,
    fine_max_dist_sq: f64,
    coarse_max_dist_sq: f64,

    // Mmap'd files
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

// Safety: all fields are read-only mmap'd data or plain values.
unsafe impl Send for Reader {}
unsafe impl Sync for Reader {}

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

    /// High-level query: returns ranked result (nearest of each type).
    /// Allocation-free internally — tracks only the nearest candidate during
    /// iteration.
    pub fn query(&self, lat: f64, lon: f64) -> ReverseResult<'_> {
        let ctx = QueryContext::new(lat, lon);

        // Fine-level search
        let mut best = BestTracker::new();
        self.search_street_level(
            &ctx,
            &self.geo_cells,
            &self.street_entries,
            &self.addr_entries,
            &self.interp_entries,
            self.header.street_cell_level,
            self.fine_max_dist_sq,
            &mut best,
        );

        // Coarse fallback if no street or address found
        if best.street.is_none() && best.addr.is_none() {
            self.search_street_level(
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

        // Admin lookup
        let admin = self.search_admin(&ctx);

        // Assemble result
        let address = best.addr.map(|(idx, dist_sq)| {
            let pt = self.read_addr_point(idx);
            let dist_m = radians_sq_to_meters(dist_sq);
            AddressMatch {
                lat_e7: pt.lat_e7,
                lon_e7: pt.lon_e7,
                house_number: self.read_string(pt.housenumber_offset),
                street: self.read_string(pt.street_offset),
                postcode: if pt.postcode_offset == 0 {
                    None
                } else {
                    Some(self.read_string(pt.postcode_offset))
                },
                distance_m: dist_m,
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

    /// Low-level query: returns all candidates within radius, unranked.
    pub fn candidates(&self, lat: f64, lon: f64) -> Candidates<'_> {
        let ctx = QueryContext::new(lat, lon);

        // Fine-level
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

        // Coarse fallback if no street or address
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

        let admin = self.search_admin(&ctx);

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
        let total_len = self.way_total_length(&self.interp_nodes, &iw.node_offset, iw.node_count);
        if total_len < 1e-15 {
            return None;
        }
        let acc_len = self.accumulated_length(
            &self.interp_nodes,
            &iw.node_offset,
            candidate.segment_index,
            candidate.snap_lat_e7,
            candidate.snap_lon_e7,
        );
        let t = acc_len / total_len;
        let start = iw.start_number;
        let end = iw.end_number;
        let diff = end as f64 - start as f64;
        let number = match iw.interpolation_type {
            0 => start as f64 + (t * diff).round(), // all
            _ => start as f64 + 2.0 * ((t * diff) / 2.0).round(), // even or odd
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
// Internal query helpers
// ---------------------------------------------------------------------------

/// Precomputed values for a query point.
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
        let lon_rad = lon.to_radians();
        Self {
            lat,
            lon,
            lat_rad,
            lon_rad,
            cos_lat: lat_rad.cos(),
        }
    }
}

/// Tracks the nearest candidate of each type for `query()`.
struct BestTracker {
    addr: Option<(u32, f64)>,             // (index, dist_sq)
    street: Option<(u32, i32, i32, f64)>, // (way_idx, snap_lat, snap_lon, dist_sq)
    interp: Option<(u32, u16, i32, i32, f64)>, // (way_idx, seg_idx, snap_lat, snap_lon, dist_sq)
}

impl BestTracker {
    fn new() -> Self {
        Self {
            addr: None,
            street: None,
            interp: None,
        }
    }
}

/// Collects all candidates for `candidates()`.
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

impl Reader {
    /// Search a street-level cell index (fine or coarse) for the best matches.
    #[allow(clippy::too_many_arguments)]
    fn search_street_level(
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
        let cells = cell_neighborhood(ctx.lat, ctx.lon, level);
        let cell_count = geo_cells_mmap.len() / GEO_CELL_SIZE;

        for cell_id in &cells {
            let Some(gc) = binary_search_cells(geo_cells_mmap, cell_count, *cell_id) else {
                continue;
            };

            // Address points
            if gc.addr_offset != format::NO_DATA_U32 {
                self.score_addr_points(ctx, addr_entries_mmap, gc.addr_offset, max_dist_sq, best);
            }

            // Street segments
            if gc.street_offset != format::NO_DATA_U64 {
                self.score_street_segments(
                    ctx,
                    street_entries_mmap,
                    gc.street_offset,
                    max_dist_sq,
                    best,
                );
            }

            // Interpolation segments
            if gc.interp_offset != format::NO_DATA_U32 {
                self.score_interp_segments(
                    ctx,
                    interp_entries_mmap,
                    gc.interp_offset,
                    max_dist_sq,
                    best,
                );
            }
        }
    }

    /// Collect all candidates from a street-level cell index.
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
        let cells = cell_neighborhood(ctx.lat, ctx.lon, level);
        let cell_count = geo_cells_mmap.len() / GEO_CELL_SIZE;

        for cell_id in &cells {
            let Some(gc) = binary_search_cells(geo_cells_mmap, cell_count, *cell_id) else {
                continue;
            };

            if gc.addr_offset != format::NO_DATA_U32 {
                self.collect_addr_points(ctx, addr_entries_mmap, gc.addr_offset, max_dist_sq, cands);
            }
            if gc.street_offset != format::NO_DATA_U64 {
                self.collect_street_segments(
                    ctx,
                    street_entries_mmap,
                    gc.street_offset,
                    max_dist_sq,
                    cands,
                );
            }
            if gc.interp_offset != format::NO_DATA_U32 {
                self.collect_interp_segments(
                    ctx,
                    interp_entries_mmap,
                    gc.interp_offset,
                    max_dist_sq,
                    cands,
                );
            }
        }
    }

    // -- Address scoring -----------------------------------------------------

    fn score_addr_points(
        &self,
        ctx: &QueryContext,
        entries_mmap: &Mmap,
        offset: u32,
        max_dist_sq: f64,
        best: &mut BestTracker,
    ) {
        for idx in read_u32_entries(entries_mmap, offset) {
            let pt = self.read_addr_point(idx);
            let dist_sq = geo::approx_distance_sq(
                ctx.lat_rad,
                ctx.lon_rad,
                geo::e7_to_rad(pt.lat_e7),
                geo::e7_to_rad(pt.lon_e7),
                ctx.cos_lat,
            );
            if dist_sq < max_dist_sq
                && (best.addr.is_none() || dist_sq < best.addr.map_or(f64::MAX, |b| b.1))
            {
                best.addr = Some((idx, dist_sq));
            }
        }
    }

    fn collect_addr_points<'a>(
        &'a self,
        ctx: &QueryContext,
        entries_mmap: &Mmap,
        offset: u32,
        max_dist_sq: f64,
        cands: &mut CandidateCollector<'a>,
    ) {
        for idx in read_u32_entries(entries_mmap, offset) {
            let pt = self.read_addr_point(idx);
            let dist_sq = geo::approx_distance_sq(
                ctx.lat_rad,
                ctx.lon_rad,
                geo::e7_to_rad(pt.lat_e7),
                geo::e7_to_rad(pt.lon_e7),
                ctx.cos_lat,
            );
            if dist_sq < max_dist_sq {
                cands.addresses.push(AddressMatch {
                    lat_e7: pt.lat_e7,
                    lon_e7: pt.lon_e7,
                    house_number: self.read_string(pt.housenumber_offset),
                    street: self.read_string(pt.street_offset),
                    postcode: if pt.postcode_offset == 0 {
                        None
                    } else {
                        Some(self.read_string(pt.postcode_offset))
                    },
                    distance_m: radians_sq_to_meters(dist_sq),
                });
            }
        }
    }

    // -- Street segment scoring ----------------------------------------------

    fn score_street_segments(
        &self,
        ctx: &QueryContext,
        entries_mmap: &Mmap,
        offset: u64,
        max_dist_sq: f64,
        best: &mut BestTracker,
    ) {
        for seg_ref in read_segment_entries(entries_mmap, offset) {
            let (snap_lat, snap_lon, dist_sq) =
                self.segment_distance(ctx, &self.street_nodes, &self.street_ways, &seg_ref);
            if dist_sq < max_dist_sq
                && (best.street.is_none() || dist_sq < best.street.map_or(f64::MAX, |b| b.3))
            {
                best.street = Some((seg_ref.way_index, snap_lat, snap_lon, dist_sq));
            }
        }
    }

    fn collect_street_segments<'a>(
        &'a self,
        ctx: &QueryContext,
        entries_mmap: &Mmap,
        offset: u64,
        max_dist_sq: f64,
        cands: &mut CandidateCollector<'a>,
    ) {
        for seg_ref in read_segment_entries(entries_mmap, offset) {
            let (snap_lat, snap_lon, dist_sq) =
                self.segment_distance(ctx, &self.street_nodes, &self.street_ways, &seg_ref);
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

    // -- Interpolation segment scoring ---------------------------------------

    fn score_interp_segments(
        &self,
        ctx: &QueryContext,
        entries_mmap: &Mmap,
        offset: u32,
        max_dist_sq: f64,
        best: &mut BestTracker,
    ) {
        for seg_ref in read_segment_entries_u32(entries_mmap, offset) {
            let (snap_lat, snap_lon, dist_sq) =
                self.interp_segment_distance(ctx, &seg_ref);
            if dist_sq < max_dist_sq
                && (best.interp.is_none() || dist_sq < best.interp.map_or(f64::MAX, |b| b.4))
            {
                best.interp = Some((
                    seg_ref.way_index,
                    seg_ref.segment_index,
                    snap_lat,
                    snap_lon,
                    dist_sq,
                ));
            }
        }
    }

    fn collect_interp_segments<'a>(
        &'a self,
        ctx: &QueryContext,
        entries_mmap: &Mmap,
        offset: u32,
        max_dist_sq: f64,
        cands: &mut CandidateCollector<'a>,
    ) {
        for seg_ref in read_segment_entries_u32(entries_mmap, offset) {
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

    // -- Segment distance helpers --------------------------------------------

    #[allow(clippy::cast_possible_truncation)] // u64→usize: Linux 64-bit only
    fn segment_distance(
        &self,
        ctx: &QueryContext,
        nodes_mmap: &Mmap,
        ways_mmap: &Mmap,
        seg_ref: &SegmentRef,
    ) -> (i32, i32, f64) {
        let way_offset = seg_ref.way_index as usize * STREET_WAY_SIZE;
        let way = read_record::<STREET_WAY_SIZE>(ways_mmap, way_offset)
            .map(StreetWay::from_bytes);
        let Some(way) = way else {
            return (0, 0, f64::MAX);
        };
        let node_byte_offset =
            way.node_offset as usize + seg_ref.segment_index as usize * NODE_COORD_SIZE;
        self.two_node_distance(ctx, nodes_mmap, node_byte_offset)
    }

    #[allow(clippy::cast_possible_truncation)] // u64→usize: Linux 64-bit only
    fn interp_segment_distance(
        &self,
        ctx: &QueryContext,
        seg_ref: &SegmentRef,
    ) -> (i32, i32, f64) {
        let way_offset = seg_ref.way_index as usize * INTERP_WAY_SIZE;
        let way = read_record::<INTERP_WAY_SIZE>(&self.interp_ways, way_offset)
            .map(InterpWay::from_bytes);
        let Some(way) = way else {
            return (0, 0, f64::MAX);
        };
        let node_byte_offset =
            way.node_offset as usize + seg_ref.segment_index as usize * NODE_COORD_SIZE;
        self.two_node_distance(ctx, &self.interp_nodes, node_byte_offset)
    }

    fn two_node_distance(
        &self,
        ctx: &QueryContext,
        nodes_mmap: &Mmap,
        byte_offset: usize,
    ) -> (i32, i32, f64) {
        let a = read_record::<NODE_COORD_SIZE>(nodes_mmap, byte_offset).map(NodeCoord::from_bytes);
        let b = read_record::<NODE_COORD_SIZE>(nodes_mmap, byte_offset + NODE_COORD_SIZE)
            .map(NodeCoord::from_bytes);
        let (Some(a), Some(b)) = (a, b) else {
            return (0, 0, f64::MAX);
        };

        let (t, dist_sq) = geo::point_to_segment_distance_sq(
            ctx.lon_rad,
            ctx.lat_rad,
            geo::e7_to_rad(a.lon_e7),
            geo::e7_to_rad(a.lat_e7),
            geo::e7_to_rad(b.lon_e7),
            geo::e7_to_rad(b.lat_e7),
            ctx.cos_lat,
        );

        // Compute snap point in e7
        #[allow(clippy::cast_possible_truncation)]
        let snap_lat = (a.lat_e7 as f64 + t * (b.lat_e7 - a.lat_e7) as f64) as i32;
        #[allow(clippy::cast_possible_truncation)]
        let snap_lon = (a.lon_e7 as f64 + t * (b.lon_e7 - a.lon_e7) as f64) as i32;

        (snap_lat, snap_lon, dist_sq)
    }

    // -- Admin lookup --------------------------------------------------------

    fn search_admin<'a>(&'a self, ctx: &QueryContext) -> Vec<AdminMatch<'a>> {
        let cells = cell_neighborhood(ctx.lat, ctx.lon, self.header.admin_cell_level);
        let cell_count = self.admin_cells.len() / ADMIN_CELL_SIZE;

        // Track best (smallest area) polygon per admin level
        let mut best_by_level: [Option<(u32, f32)>; 12] = [None; 12]; // (poly_idx, area)

        for cell_id in &cells {
            let Some(ac) = binary_search_admin_cells(&self.admin_cells, cell_count, *cell_id)
            else {
                continue;
            };

            for (poly_idx, is_interior) in read_admin_entries(&self.admin_entries, ac.entries_offset)
            {
                let poly = self.read_admin_polygon(poly_idx);
                let level = poly.admin_level as usize;
                if level >= 12 {
                    continue;
                }

                // Skip if we already have a smaller polygon at this level
                if let Some((_, best_area)) = best_by_level[level] {
                    if poly.area >= best_area {
                        continue;
                    }
                }

                // Interior hint: test first (likely match), but still verify with PIP
                let _ = is_interior; // Priority ordering only — always do PIP

                if self.admin_polygon_contains(ctx, &poly) {
                    best_by_level[level] = Some((poly_idx, poly.area));
                }
            }
        }

        let mut result = Vec::new();
        for &(poly_idx, _) in best_by_level.iter().flatten() {
            let poly = self.read_admin_polygon(poly_idx);
            // Country codes are packed as u16 in the polygon record.
            // The builder interns them into the string pool; for level-2
            // boundaries the name_offset points to the country name and
            // we store the 2-char code separately. For now, return None
            // — the builder will intern country codes as strings.
            result.push(AdminMatch {
                admin_level: poly.admin_level,
                name: self.read_string(poly.name_offset),
                country_code: None, // TODO: resolve from string pool once builder interns codes
            });
        }
        result
    }

    fn admin_polygon_contains(&self, ctx: &QueryContext, poly: &AdminPolygon) -> bool {
        let vertices = self.read_admin_vertices(poly);
        if vertices.is_empty() {
            return false;
        }

        // Split into exterior + holes at sentinel markers
        let mut rings: Vec<Vec<(f64, f64)>> = Vec::new();
        let mut current_ring: Vec<(f64, f64)> = Vec::new();

        for nc in &vertices {
            if *nc == format::RING_SENTINEL {
                if current_ring.len() >= 3 {
                    rings.push(std::mem::take(&mut current_ring));
                } else {
                    current_ring.clear();
                }
            } else {
                current_ring.push((nc.lon_e7 as f64 * 1e-7, nc.lat_e7 as f64 * 1e-7));
            }
        }
        if current_ring.len() >= 3 {
            rings.push(current_ring);
        }

        if rings.is_empty() {
            return false;
        }

        // First ring is exterior, rest are holes
        let exterior = &rings[0];
        let holes: Vec<&[(f64, f64)]> = rings[1..].iter().map(Vec::as_slice).collect();
        geo::point_in_polygon(ctx.lon, ctx.lat, exterior, &holes)
    }

    // -- Record readers ------------------------------------------------------

    fn read_addr_point(&self, index: u32) -> AddrPoint {
        let offset = index as usize * ADDR_POINT_SIZE;
        read_record::<ADDR_POINT_SIZE>(&self.addr_points, offset)
            .map(AddrPoint::from_bytes)
            .unwrap_or(AddrPoint {
                lat_e7: 0,
                lon_e7: 0,
                housenumber_offset: 0,
                street_offset: 0,
                postcode_offset: 0,
            })
    }

    fn read_street_way(&self, index: u32) -> StreetWay {
        let offset = index as usize * STREET_WAY_SIZE;
        read_record::<STREET_WAY_SIZE>(&self.street_ways, offset)
            .map(StreetWay::from_bytes)
            .unwrap_or(StreetWay {
                node_offset: 0,
                name_offset: 0,
                node_count: 0,
            })
    }

    fn read_interp_way(&self, index: u32) -> InterpWay {
        let offset = index as usize * INTERP_WAY_SIZE;
        read_record::<INTERP_WAY_SIZE>(&self.interp_ways, offset)
            .map(InterpWay::from_bytes)
            .unwrap_or(InterpWay {
                node_offset: 0,
                street_offset: 0,
                start_number: 0,
                end_number: 0,
                node_count: 0,
                interpolation_type: 0,
            })
    }

    fn read_admin_polygon(&self, index: u32) -> AdminPolygon {
        let offset = index as usize * ADMIN_POLYGON_SIZE;
        read_record::<ADMIN_POLYGON_SIZE>(&self.admin_polygons, offset)
            .map(AdminPolygon::from_bytes)
            .unwrap_or(AdminPolygon {
                area: 0.0,
                vertex_offset: 0,
                vertex_count: 0,
                name_offset: 0,
                country_code: 0,
                admin_level: 0,
            })
    }

    fn read_admin_vertices(&self, poly: &AdminPolygon) -> Vec<NodeCoord> {
        let start = poly.vertex_offset as usize;
        let count = poly.vertex_count as usize;
        let mut vertices = Vec::with_capacity(count);
        for i in 0..count {
            let offset = start + i * NODE_COORD_SIZE;
            if let Some(rec) = read_record::<NODE_COORD_SIZE>(&self.admin_vertices, offset) {
                vertices.push(NodeCoord::from_bytes(rec));
            }
        }
        vertices
    }

    fn read_string(&self, offset: u32) -> &str {
        if offset == 0 {
            return "";
        }
        let start = offset as usize;
        if start >= self.strings.len() {
            return "";
        }
        let remaining = &self.strings[start..];
        let end = remaining.iter().position(|&b| b == 0).unwrap_or(remaining.len());
        std::str::from_utf8(&remaining[..end]).unwrap_or("")
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
        let total_len = self.way_total_length(&self.interp_nodes, &iw.node_offset, iw.node_count);
        if total_len < 1e-15 {
            return None;
        }
        let acc_len = self.accumulated_length(
            &self.interp_nodes,
            &iw.node_offset,
            seg_idx,
            snap_lat,
            snap_lon,
        );
        let t = acc_len / total_len;
        let start = iw.start_number;
        let end = iw.end_number;
        let diff = end as f64 - start as f64;
        let number = match iw.interpolation_type {
            0 => start as f64 + (t * diff).round(),
            _ => start as f64 + 2.0 * ((t * diff) / 2.0).round(),
        };
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let house_number = number.max(0.0) as u32;
        Some(InterpolationMatch {
            street: self.read_string(iw.street_offset),
            house_number,
            distance_m: radians_sq_to_meters(dist_sq),
        })
    }

    #[allow(clippy::cast_possible_truncation)] // u64→usize: Linux 64-bit only
    fn way_total_length(&self, nodes_mmap: &Mmap, node_offset: &u64, node_count: u16) -> f64 {
        let mut total = 0.0;
        let base = *node_offset as usize;
        for i in 0..(node_count as usize).saturating_sub(1) {
            let a = read_record::<NODE_COORD_SIZE>(nodes_mmap, base + i * NODE_COORD_SIZE)
                .map(NodeCoord::from_bytes);
            let b = read_record::<NODE_COORD_SIZE>(nodes_mmap, base + (i + 1) * NODE_COORD_SIZE)
                .map(NodeCoord::from_bytes);
            if let (Some(a), Some(b)) = (a, b) {
                let d = geo::approx_distance_sq(
                    geo::e7_to_rad(a.lat_e7),
                    geo::e7_to_rad(a.lon_e7),
                    geo::e7_to_rad(b.lat_e7),
                    geo::e7_to_rad(b.lon_e7),
                    geo::e7_to_rad(a.lat_e7).cos(),
                );
                total += d.sqrt();
            }
        }
        total
    }

    #[allow(clippy::cast_possible_truncation)] // u64→usize: Linux 64-bit only
    fn accumulated_length(
        &self,
        nodes_mmap: &Mmap,
        node_offset: &u64,
        seg_idx: u16,
        snap_lat: i32,
        snap_lon: i32,
    ) -> f64 {
        let mut acc = 0.0;
        let base = *node_offset as usize;
        // Sum lengths of segments 0..seg_idx
        for i in 0..seg_idx as usize {
            let a = read_record::<NODE_COORD_SIZE>(nodes_mmap, base + i * NODE_COORD_SIZE)
                .map(NodeCoord::from_bytes);
            let b = read_record::<NODE_COORD_SIZE>(nodes_mmap, base + (i + 1) * NODE_COORD_SIZE)
                .map(NodeCoord::from_bytes);
            if let (Some(a), Some(b)) = (a, b) {
                let d = geo::approx_distance_sq(
                    geo::e7_to_rad(a.lat_e7),
                    geo::e7_to_rad(a.lon_e7),
                    geo::e7_to_rad(b.lat_e7),
                    geo::e7_to_rad(b.lon_e7),
                    geo::e7_to_rad(a.lat_e7).cos(),
                );
                acc += d.sqrt();
            }
        }
        // Add partial segment to snap point
        let seg_start =
            read_record::<NODE_COORD_SIZE>(nodes_mmap, base + seg_idx as usize * NODE_COORD_SIZE)
                .map(NodeCoord::from_bytes);
        if let Some(seg_start) = seg_start {
            let d = geo::approx_distance_sq(
                geo::e7_to_rad(seg_start.lat_e7),
                geo::e7_to_rad(seg_start.lon_e7),
                geo::e7_to_rad(snap_lat),
                geo::e7_to_rad(snap_lon),
                geo::e7_to_rad(seg_start.lat_e7).cos(),
            );
            acc += d.sqrt();
        }
        acc
    }
}

// ---------------------------------------------------------------------------
// S2 cell helpers
// ---------------------------------------------------------------------------

fn cell_neighborhood(lat: f64, lon: f64, level: u8) -> Vec<u64> {
    let ll = LatLng::from_degrees(lat, lon);
    let cell = CellID::from(ll).parent(level as u64);
    let mut cells = vec![cell.0];
    for neighbor in cell.all_neighbors(level as u64) {
        cells.push(neighbor.0);
    }
    cells
}

// ---------------------------------------------------------------------------
// Binary search helpers
// ---------------------------------------------------------------------------

fn binary_search_cells(mmap: &[u8], count: usize, target: u64) -> Option<GeoCell> {
    if count == 0 {
        return None;
    }
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
    if count == 0 {
        return None;
    }
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

// ---------------------------------------------------------------------------
// Entry list readers
// ---------------------------------------------------------------------------

/// Read u32 entries (for addr_entries, interp_entries with u32 offset).
fn read_u32_entries(mmap: &[u8], offset: u32) -> Vec<u32> {
    let off = offset as usize;
    if off + 2 > mmap.len() {
        return Vec::new();
    }
    let count = u16::from_le_bytes([mmap[off], mmap[off + 1]]) as usize;
    let data_start = off + 2;
    let data_end = data_start + count * 4;
    if data_end > mmap.len() {
        return Vec::new();
    }
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let base = data_start + i * 4;
        entries.push(u32::from_le_bytes([
            mmap[base],
            mmap[base + 1],
            mmap[base + 2],
            mmap[base + 3],
        ]));
    }
    entries
}

/// Read segment ref entries from street_entries (u64 offset).
#[allow(clippy::cast_possible_truncation)] // u64→usize: Linux 64-bit only
fn read_segment_entries(mmap: &[u8], offset: u64) -> Vec<SegmentRef> {
    let off = offset as usize;
    if off + 2 > mmap.len() {
        return Vec::new();
    }
    let count = u16::from_le_bytes([mmap[off], mmap[off + 1]]) as usize;
    let data_start = off + 2;
    let data_end = data_start + count * SEGMENT_REF_SIZE;
    if data_end > mmap.len() {
        return Vec::new();
    }
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let base = data_start + i * SEGMENT_REF_SIZE;
        if let Some(rec) = read_record::<SEGMENT_REF_SIZE>(mmap, base) {
            entries.push(SegmentRef::from_bytes(rec));
        }
    }
    entries
}

/// Read segment ref entries from interp_entries (u32 offset).
fn read_segment_entries_u32(mmap: &[u8], offset: u32) -> Vec<SegmentRef> {
    read_segment_entries(mmap, offset as u64)
}

/// Read admin entries: returns (polygon_index, is_interior_hint).
fn read_admin_entries(mmap: &[u8], offset: u32) -> Vec<(u32, bool)> {
    let off = offset as usize;
    if off + 2 > mmap.len() {
        return Vec::new();
    }
    let count = u16::from_le_bytes([mmap[off], mmap[off + 1]]) as usize;
    let data_start = off + 2;
    let data_end = data_start + count * 4;
    if data_end > mmap.len() {
        return Vec::new();
    }
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let base = data_start + i * 4;
        let raw = u32::from_le_bytes([mmap[base], mmap[base + 1], mmap[base + 2], mmap[base + 3]]);
        let is_interior = raw & INTERIOR_FLAG != 0;
        let idx = raw & INDEX_MASK;
        entries.push((idx, is_interior));
    }
    entries
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

fn read_record<const N: usize>(mmap: &[u8], offset: usize) -> Option<&[u8; N]> {
    mmap.get(offset..offset + N)?.try_into().ok()
}

fn radians_sq_to_meters(rad_sq: f64) -> f64 {
    rad_sq.sqrt() * geo::EARTH_RADIUS_M
}
