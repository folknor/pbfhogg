//! Memory-mapped reader for the reverse geocoding index.
//!
//! Opens an index directory, memory-maps all files, and provides two query
//! levels: raw [`Candidates`] and ranked [`ReverseResult`].

use std::path::Path;
use std::{error::Error, fmt};

use memmap2::Mmap;
use rustc_hash::FxHashSet;
use s2::cellid::CellID;
use s2::latlng::LatLng;

use super::format::{
    self, ADDR_POINT_SIZE, ADMIN_CELL_SIZE, ADMIN_POLYGON_SIZE, AddrPoint, AdminCell, AdminPolygon,
    GEO_CELL_SIZE, GeoCell, HEADER_SIZE, Header, INDEX_MASK, INTERIOR_FLAG, INTERP_WAY_SIZE,
    InterpWay, NODE_COORD_SIZE, NodeCoord, SEGMENT_REF_SIZE, STREET_WAY_SIZE, SegmentRef,
    StreetWay,
};
use crate::geo;

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

/// Invalid coordinate supplied to [`Reader::query`] or [`Reader::candidates`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum QueryError {
    NonFiniteLatitude(f64),
    NonFiniteLongitude(f64),
    LatitudeOutOfRange(f64),
    LongitudeOutOfRange(f64),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFiniteLatitude(value) => write!(f, "query latitude ({value}) must be finite"),
            Self::NonFiniteLongitude(value) => {
                write!(f, "query longitude ({value}) must be finite")
            }
            Self::LatitudeOutOfRange(value) => write!(
                f,
                "query latitude ({value}) must be within -90 and 90 degrees"
            ),
            Self::LongitudeOutOfRange(value) => write!(
                f,
                "query longitude ({value}) must be within -180 and 180 degrees"
            ),
        }
    }
}

impl Error for QueryError {}

impl<'a> Candidates<'a> {
    /// Apply default ranking: nearest of each type and smallest admin per level.
    pub fn into_result(self, reader: &'a Reader) -> ReverseResult<'a> {
        let address = self.addresses.into_iter().min_by(|a, b| {
            a.distance_m
                .partial_cmp(&b.distance_m)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let street = self.streets.into_iter().min_by(|a, b| {
            a.distance_m
                .partial_cmp(&b.distance_m)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let interpolation = self
            .interpolations
            .iter()
            .min_by(|a, b| {
                a.distance_m
                    .partial_cmp(&b.distance_m)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
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
/// `Send + Sync` - all fields are `Mmap` (which is `Send + Sync`), plain
/// values, or (under `test-hooks`) an atomic counter.
pub struct Reader {
    /// Admin point-in-polygon tests run by this reader since it was opened
    /// or last reset. Per reader, so a test observes only its own queries.
    #[cfg(feature = "test-hooks")]
    admin_pip_calls: std::sync::atomic::AtomicUsize,

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
    ///
    /// Opening performs a structural validation pass, so its work grows with
    /// the number of fine and coarse cells, admin cells, street and
    /// interpolation ways, address points, and admin polygons. It checks
    /// header-counted record-file lengths, every cell's entry-list span, each
    /// geometry span, and every string offset referenced by a way, address
    /// point, or polygon.
    /// The string checks read through each referenced string's terminator.
    /// This touches file-backed mmap pages and can fault them into memory.
    /// Coordinate payloads and individual cell-entry references are not
    /// scanned; the latter are bounds-checked when queries use them. The
    /// builder's smoke test calls this same open path, so its validation pass
    /// is also part of build time. There is no unvalidated open: a caller that
    /// reopens an index it has just built or checksummed pays this pass again
    /// (a content checksum would not avoid it, since verifying one reads every
    /// byte).
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
            let len = file
                .metadata()
                .map_err(|e| format!("failed to stat {}: {e}", path.display()))?
                .len();
            if len == 0 {
                // memmap2::Mmap::map() fails on zero-length files.
                // Return an empty read-only anonymous mmap instead.
                // All consumers check record bounds before reading,
                // so an empty mmap (len=0) is handled correctly.
                return Ok(memmap2::MmapOptions::new()
                    .map_anon()
                    .map_err(|e| {
                        format!("failed to create empty mmap for {}: {e}", path.display())
                    })?
                    .make_read_only()
                    .map_err(|e| {
                        format!(
                            "failed to make anon mmap read-only for {}: {e}",
                            path.display()
                        )
                    })?);
            }
            // SAFETY: the file is read-only for the lifetime of the Reader.
            let m = unsafe { Mmap::map(&file) }
                .map_err(|e| format!("failed to mmap {}: {e}", path.display()))?;
            Ok(m)
        };

        let fine_radius_m = header.fine_search_radius_m as f64;
        let coarse_radius_m = header.coarse_search_radius_m as f64;

        let reader = Self {
            #[cfg(feature = "test-hooks")]
            admin_pip_calls: std::sync::atomic::AtomicUsize::new(0),
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
        };
        reader.validate_structure()?;
        Ok(reader)
    }

    // Deliberately no per-entry reference sweep here (way_index against
    // street_way_count, segment_index + 1 against the way's node_count,
    // address and admin indices against their counts). Fine and coarse trees
    // repeat segment references across cells, so the sweep is a walk over
    // every entry file plus a scattered way-table read per segment entry, on
    // every open and again in the builder smoke test. Open is already
    // O(index size); growing it further belongs after a trusted-open path
    // exists, not before. The builder derives every entry from an in-range
    // index and fails the build on missing producer data, and every
    // query-time consumer bounds-checks the entry it reads - the
    // segment checks against the owning way's node_count, not the shared
    // node file length, so an entry can never borrow the next way's nodes.
    // To revisit: Reader::open wall and majflt on a cold planet index, with
    // and without the sweep.
    fn validate_structure(&self) -> Result<()> {
        fn exact(name: &str, actual: usize, count: u32, size: usize) -> Result<()> {
            let expected = usize::try_from(count)
                .ok()
                .and_then(|n| n.checked_mul(size))
                .ok_or_else(|| format!("{name} size from header overflows this platform"))?;
            if actual != expected {
                return Err(format!(
                    "invalid geocode index: {name} is {actual} bytes, expected {expected}"
                )
                .into());
            }
            Ok(())
        }
        exact(
            format::FILE_GEO_CELLS,
            self.geo_cells.len(),
            self.header.geo_cell_count,
            GEO_CELL_SIZE,
        )?;
        exact(
            format::FILE_COARSE_GEO_CELLS,
            self.coarse_geo_cells.len(),
            self.header.coarse_cell_count,
            GEO_CELL_SIZE,
        )?;
        exact(
            format::FILE_ADMIN_CELLS,
            self.admin_cells.len(),
            self.header.admin_cell_count,
            ADMIN_CELL_SIZE,
        )?;
        exact(
            format::FILE_STREET_WAYS,
            self.street_ways.len(),
            self.header.street_way_count,
            STREET_WAY_SIZE,
        )?;
        exact(
            format::FILE_ADDR_POINTS,
            self.addr_points.len(),
            self.header.addr_point_count,
            ADDR_POINT_SIZE,
        )?;
        exact(
            format::FILE_INTERP_WAYS,
            self.interp_ways.len(),
            self.header.interp_way_count,
            INTERP_WAY_SIZE,
        )?;
        exact(
            format::FILE_ADMIN_POLYGONS,
            self.admin_polygons.len(),
            self.header.admin_polygon_count,
            ADMIN_POLYGON_SIZE,
        )?;

        self.validate_geo_tree(
            &self.geo_cells,
            &self.street_entries,
            &self.addr_entries,
            &self.interp_entries,
        )?;
        self.validate_geo_tree(
            &self.coarse_geo_cells,
            &self.coarse_street_entries,
            &self.coarse_addr_entries,
            &self.coarse_interp_entries,
        )?;
        self.validate_admin_tree()?;
        self.validate_way_geometry()?;
        Ok(())
    }

    /// Admin cells point into `admin_entries.bin` exactly the way geo cells
    /// point into the three geo entry files. Validating two of the three list
    /// kinds and leaving the third is how a truncated or hostile
    /// `admin_entries.bin` opens successfully and then silently returns
    /// incomplete admin results: `AdminEntryIter` stops at the end of the
    /// mmap and reports no error, so the caller cannot tell a short list from
    /// a genuinely empty one.
    fn validate_admin_tree(&self) -> Result<()> {
        for index in 0..self.admin_cells.len() / ADMIN_CELL_SIZE {
            let rec = read_record::<ADMIN_CELL_SIZE>(&self.admin_cells, index * ADMIN_CELL_SIZE)
                .ok_or("invalid admin cell record")?;
            let cell = AdminCell::from_bytes(rec);
            validate_list(cell.entries_offset, &self.admin_entries, 4, "admin entry")?;
        }
        Ok(())
    }

    /// The remaining offset-into-a-file relationships in the format, all of
    /// which are `offset + count * record_size` rather than a length-prefixed
    /// list:
    ///
    /// - `StreetWay.node_offset` + `node_count` into `street_nodes.bin`
    /// - `InterpWay.node_offset` + `node_count` into `interp_nodes.bin`
    /// - `AdminPolygon.vertex_offset` + `vertex_count` into
    ///   `admin_vertices.bin`
    ///
    /// Each was previously unchecked at open time. The read paths that follow
    /// them are individually bounds-safe - `record_at` and `read_record`
    /// return `None` past the end - but "safe" is not "correct": a truncated
    /// nodes file turns a street into a shorter street and a polygon into a
    /// different polygon, silently, and the reader answers queries about it.
    ///
    /// String-pool offsets (`name_offset`, `housenumber_offset`,
    /// `street_offset`, `postcode_offset`, `country_code_offset`) are validated
    /// here as well. `read_nul_string` independently returns `None` for an
    /// invalid offset, missing terminator, or invalid UTF-8.
    fn validate_way_geometry(&self) -> Result<()> {
        for index in 0..self.street_ways.len() / STREET_WAY_SIZE {
            let rec = read_record::<STREET_WAY_SIZE>(&self.street_ways, index * STREET_WAY_SIZE)
                .ok_or("invalid street way record")?;
            let way = StreetWay::from_bytes(rec);
            validate_span(
                way.node_offset,
                u64::from(way.node_count),
                NODE_COORD_SIZE,
                &self.street_nodes,
                "street way node",
            )?;
            validate_string_offset(way.name_offset, &self.strings, "street way name")?;
        }
        for index in 0..self.interp_ways.len() / INTERP_WAY_SIZE {
            let rec = read_record::<INTERP_WAY_SIZE>(&self.interp_ways, index * INTERP_WAY_SIZE)
                .ok_or("invalid interpolation way record")?;
            let way = InterpWay::from_bytes(rec);
            validate_span(
                way.node_offset,
                u64::from(way.node_count),
                NODE_COORD_SIZE,
                &self.interp_nodes,
                "interpolation way node",
            )?;
            validate_string_offset(way.street_offset, &self.strings, "interpolation way street")?;
        }
        for index in 0..self.admin_polygons.len() / ADMIN_POLYGON_SIZE {
            let rec =
                read_record::<ADMIN_POLYGON_SIZE>(&self.admin_polygons, index * ADMIN_POLYGON_SIZE)
                    .ok_or("invalid admin polygon record")?;
            let poly = AdminPolygon::from_bytes(rec);
            // Producer rings repeat their first vertex: assemble_rings keeps
            // only closed rings and simplify_ring preserves both endpoints.
            // Do not scan every vertex here solely to recertify closure; that
            // would add coordinate-page reads to metadata validation at open.
            validate_span(
                poly.vertex_offset,
                u64::from(poly.vertex_count),
                NODE_COORD_SIZE,
                &self.admin_vertices,
                "admin polygon vertex",
            )?;
            validate_string_offset(poly.name_offset, &self.strings, "admin polygon name")?;
            validate_string_offset(
                poly.country_code_offset,
                &self.strings,
                "admin polygon country code",
            )?;
        }
        for index in 0..self.addr_points.len() / ADDR_POINT_SIZE {
            let rec = read_record::<ADDR_POINT_SIZE>(&self.addr_points, index * ADDR_POINT_SIZE)
                .ok_or("invalid address point record")?;
            let pt = AddrPoint::from_bytes(rec);
            validate_string_offset(pt.housenumber_offset, &self.strings, "address housenumber")?;
            validate_string_offset(pt.street_offset, &self.strings, "address street")?;
            validate_string_offset(pt.postcode_offset, &self.strings, "address postcode")?;
        }
        Ok(())
    }

    fn validate_geo_tree(
        &self,
        cells: &[u8],
        streets: &[u8],
        addrs: &[u8],
        interps: &[u8],
    ) -> Result<()> {
        for index in 0..cells.len() / GEO_CELL_SIZE {
            let rec = read_record::<GEO_CELL_SIZE>(cells, index * GEO_CELL_SIZE)
                .ok_or("invalid geo cell record")?;
            let cell = GeoCell::from_bytes(rec);
            validate_list(cell.addr_offset, addrs, 4, "address entry")?;
            validate_list(
                cell.street_offset,
                streets,
                SEGMENT_REF_SIZE,
                "street entry",
            )?;
            validate_list(
                cell.interp_offset,
                interps,
                SEGMENT_REF_SIZE,
                "interpolation entry",
            )?;
        }
        Ok(())
    }

    // -- Public query API ----------------------------------------------------

    /// High-level query: nearest of each type, allocation-free on the hot path.
    /// Internally iterates mmap'd entry lists without materializing Vecs.
    pub fn query(&self, lat: f64, lon: f64) -> std::result::Result<ReverseResult<'_>, QueryError> {
        let ctx = QueryContext::new(lat, lon)?;
        let mut best = BestTracker::new();

        // Fine-level search
        self.search_level(
            &ctx,
            &self.geo_cells,
            &self.street_entries,
            &self.addr_entries,
            &self.interp_entries,
            self.header.street_cell_level,
            self.fine_max_dist_sq,
            SearchKinds::ALL,
            &mut best,
        );

        // Coarse fallback
        if best.street.is_none() || best.addr.is_none() || best.interp.is_none() {
            self.search_level(
                &ctx,
                &self.coarse_geo_cells,
                &self.coarse_street_entries,
                &self.coarse_addr_entries,
                &self.coarse_interp_entries,
                self.header.coarse_cell_level,
                self.coarse_max_dist_sq,
                SearchKinds {
                    addr: best.addr.is_none(),
                    street: best.street.is_none(),
                    interp: best.interp.is_none(),
                },
                &mut best,
            );
        }

        // Admin lookup (collapsed to one per level)
        let admin = self.search_admin_ranked(&ctx);

        // Assemble
        let address = best.addr.and_then(|(idx, dist_sq)| {
            let pt = self.read_addr_point(idx)?;
            AddressMatch {
                lat_e7: pt.lat_e7,
                lon_e7: pt.lon_e7,
                house_number: self.read_string(pt.housenumber_offset),
                street: self.read_string(pt.street_offset),
                postcode: nonzero_string(self, pt.postcode_offset),
                distance_m: radians_sq_to_meters(dist_sq),
            }
            .into()
        });

        let street = best
            .street
            .and_then(|(way_idx, snap_lat, snap_lon, dist_sq)| {
                let way = self.read_street_way(way_idx)?;
                StreetMatch {
                    name: self.read_string(way.name_offset),
                    snap_lat_e7: snap_lat,
                    snap_lon_e7: snap_lon,
                    distance_m: radians_sq_to_meters(dist_sq),
                }
                .into()
            });

        let interpolation =
            best.interp
                .and_then(|(way_idx, seg_idx, snap_lat, snap_lon, dist_sq)| {
                    self.resolve_interpolation(way_idx, seg_idx, snap_lat, snap_lon, dist_sq)
                });

        Ok(ReverseResult {
            address,
            street,
            interpolation,
            admin,
        })
    }

    /// Low-level query: all candidates within radius, unranked.
    pub fn candidates(
        &self,
        lat: f64,
        lon: f64,
    ) -> std::result::Result<Candidates<'_>, QueryError> {
        let ctx = QueryContext::new(lat, lon)?;
        let mut cands = CandidateCollector::new();

        self.search_level(
            &ctx,
            &self.geo_cells,
            &self.street_entries,
            &self.addr_entries,
            &self.interp_entries,
            self.header.street_cell_level,
            self.fine_max_dist_sq,
            SearchKinds::ALL,
            &mut cands,
        );

        if cands.streets.is_empty() || cands.addresses.is_empty() || cands.interpolations.is_empty()
        {
            self.search_level(
                &ctx,
                &self.coarse_geo_cells,
                &self.coarse_street_entries,
                &self.coarse_addr_entries,
                &self.coarse_interp_entries,
                self.header.coarse_cell_level,
                self.coarse_max_dist_sq,
                SearchKinds {
                    addr: cands.addresses.is_empty(),
                    street: cands.streets.is_empty(),
                    interp: cands.interpolations.is_empty(),
                },
                &mut cands,
            );
        }

        // Admin: return ALL containing polygons, not collapsed
        let admin = self.search_admin_all(&ctx);

        Ok(Candidates {
            addresses: cands.addresses,
            streets: cands.streets,
            interpolations: cands.interpolations,
            admin,
        })
    }

    /// Compute the interpolated house number for a raw candidate.
    pub fn interpolate(&self, candidate: &InterpolationCandidate<'_>) -> Option<u32> {
        self.interpolated_number(
            candidate.way_index,
            candidate.segment_index,
            candidate.snap_lat_e7,
            candidate.snap_lon_e7,
        )
    }

    /// Whether an interpolation way can ever yield a house number.
    ///
    /// This is the resolvability half of [`Self::interpolated_number`]: apart
    /// from the segment bound, every reason that function returns `None` is a
    /// property of the way, never of the snap point. The segment bound is not
    /// repeated here because search only produces segments that belong to
    /// their way; `interpolated_number` checks it for caller-built candidates,
    /// whose fields are public. Search sinks use it so that "no interpolation
    /// result" and "no interpolation candidate" mean the same thing - otherwise a
    /// nearby but unresolvable fine segment both fails to produce a house
    /// number and suppresses the coarse fallback that would have.
    fn interpolation_resolvable(&self, way_index: u32) -> bool {
        let Some(iw) = self.read_interp_way(way_index) else {
            return false;
        };
        if iw.start_number == 0 || iw.end_number == 0 {
            return false;
        }
        self.way_length_radians(&self.interp_nodes, iw.node_offset, iw.node_count) >= 1e-15
    }

    fn interpolated_number(
        &self,
        way_index: u32,
        segment_index: u16,
        snap_lat_e7: i32,
        snap_lon_e7: i32,
    ) -> Option<u32> {
        let iw = self.read_interp_way(way_index)?;
        // Candidates have public fields and may be constructed by callers.
        // Bound the segment to its owning way before walking node storage.
        if u32::from(segment_index) + 1 >= u32::from(iw.node_count) {
            return None;
        }
        if iw.start_number == 0 || iw.end_number == 0 {
            return None;
        }
        let total_len = self.way_length_radians(&self.interp_nodes, iw.node_offset, iw.node_count);
        if total_len < 1e-15 {
            return None;
        }
        let acc_len = self.accumulated_length_radians(
            &self.interp_nodes,
            iw.node_offset,
            segment_index,
            snap_lat_e7,
            snap_lon_e7,
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

    /// Zero this reader's admin point-in-polygon call count.
    #[cfg(feature = "test-hooks")]
    pub fn reset_admin_pip_calls(&self) {
        self.admin_pip_calls
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Admin point-in-polygon tests this reader has run since it was opened
    /// or last reset. Other readers do not contribute; concurrent queries on
    /// this same reader do.
    #[cfg(feature = "test-hooks")]
    pub fn admin_pip_calls(&self) -> usize {
        self.admin_pip_calls
            .load(std::sync::atomic::Ordering::Relaxed)
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
    fn new(lat: f64, lon: f64) -> std::result::Result<Self, QueryError> {
        if !lat.is_finite() {
            return Err(QueryError::NonFiniteLatitude(lat));
        }
        if !lon.is_finite() {
            return Err(QueryError::NonFiniteLongitude(lon));
        }
        if !(-90.0..=90.0).contains(&lat) {
            return Err(QueryError::LatitudeOutOfRange(lat));
        }
        if !(-180.0..=180.0).contains(&lon) {
            return Err(QueryError::LongitudeOutOfRange(lon));
        }
        let lat_rad = lat.to_radians();
        Ok(Self {
            lat,
            lon,
            lat_rad,
            lon_rad: lon.to_radians(),
            cos_lat: lat_rad.cos(),
        })
    }
}

/// Tracks nearest candidate of each type for `query()` (allocation-free).
struct BestTracker {
    addr: Option<(u32, f64)>,                  // (index, dist_sq)
    street: Option<(u32, i32, i32, f64)>,      // (way_idx, snap_lat, snap_lon, dist_sq)
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

struct CandidateCollector<'a> {
    addresses: Vec<AddressMatch<'a>>,
    streets: Vec<StreetMatch<'a>>,
    interpolations: Vec<InterpolationCandidate<'a>>,
}

#[derive(Clone, Copy)]
struct SearchKinds {
    addr: bool,
    street: bool,
    interp: bool,
}

impl SearchKinds {
    const ALL: Self = Self {
        addr: true,
        street: true,
        interp: true,
    };
}

trait SearchSink<'a> {
    fn address(&mut self, reader: &'a Reader, index: u32, point: AddrPoint, dist_sq: f64);
    fn street(
        &mut self,
        reader: &'a Reader,
        segment: SegmentRef,
        snap_lat: i32,
        snap_lon: i32,
        dist_sq: f64,
    );
    fn interpolation(
        &mut self,
        reader: &'a Reader,
        segment: SegmentRef,
        snap_lat: i32,
        snap_lon: i32,
        dist_sq: f64,
    );
}

// Both sinks apply the SAME admission test before recording anything: a
// candidate the assembly step could not turn into a result must not be
// recorded, because `query()` and `candidates()` gate the coarse fallback on
// emptiness. Recording an unusable candidate produces no result AND suppresses
// the fallback that would have produced one.
impl SearchSink<'_> for BestTracker {
    fn address(&mut self, reader: &Reader, index: u32, _point: AddrPoint, dist_sq: f64) {
        if reader.read_addr_point(index).is_none() {
            return;
        }
        if dist_sq < self.addr.map_or(f64::MAX, |best| best.1) {
            self.addr = Some((index, dist_sq));
        }
    }

    fn street(
        &mut self,
        reader: &Reader,
        segment: SegmentRef,
        snap_lat: i32,
        snap_lon: i32,
        dist_sq: f64,
    ) {
        if reader.read_street_way(segment.way_index).is_none() {
            return;
        }
        if dist_sq < self.street.map_or(f64::MAX, |best| best.3) {
            self.street = Some((segment.way_index, snap_lat, snap_lon, dist_sq));
        }
    }

    fn interpolation(
        &mut self,
        reader: &Reader,
        segment: SegmentRef,
        snap_lat: i32,
        snap_lon: i32,
        dist_sq: f64,
    ) {
        if !reader.interpolation_resolvable(segment.way_index) {
            return;
        }
        if dist_sq < self.interp.map_or(f64::MAX, |best| best.4) {
            self.interp = Some((
                segment.way_index,
                segment.segment_index,
                snap_lat,
                snap_lon,
                dist_sq,
            ));
        }
    }
}

impl<'a> SearchSink<'a> for CandidateCollector<'a> {
    fn address(&mut self, reader: &'a Reader, _index: u32, point: AddrPoint, dist_sq: f64) {
        self.addresses.push(AddressMatch {
            lat_e7: point.lat_e7,
            lon_e7: point.lon_e7,
            house_number: reader.read_string(point.housenumber_offset),
            street: reader.read_string(point.street_offset),
            postcode: nonzero_string(reader, point.postcode_offset),
            distance_m: radians_sq_to_meters(dist_sq),
        });
    }

    fn street(
        &mut self,
        reader: &'a Reader,
        segment: SegmentRef,
        snap_lat: i32,
        snap_lon: i32,
        dist_sq: f64,
    ) {
        let Some(way) = reader.read_street_way(segment.way_index) else {
            return;
        };
        self.streets.push(StreetMatch {
            name: reader.read_string(way.name_offset),
            snap_lat_e7: snap_lat,
            snap_lon_e7: snap_lon,
            distance_m: radians_sq_to_meters(dist_sq),
        });
    }

    fn interpolation(
        &mut self,
        reader: &'a Reader,
        segment: SegmentRef,
        snap_lat: i32,
        snap_lon: i32,
        dist_sq: f64,
    ) {
        let Some(way) = reader.read_interp_way(segment.way_index) else {
            return;
        };
        if !reader.interpolation_resolvable(segment.way_index) {
            return;
        }
        self.interpolations.push(InterpolationCandidate {
            street: reader.read_string(way.street_offset),
            way_index: segment.way_index,
            segment_index: segment.segment_index,
            snap_lat_e7: snap_lat,
            snap_lon_e7: snap_lon,
            distance_m: radians_sq_to_meters(dist_sq),
        });
    }
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
    fn new(mmap: &'a [u8], offset: u64) -> Self {
        let Ok(off) = usize::try_from(offset) else {
            return Self {
                data: mmap,
                pos: 0,
                remaining: 0,
            };
        };
        let Some(end) = off.checked_add(2) else {
            return Self {
                data: mmap,
                pos: 0,
                remaining: 0,
            };
        };
        if end > mmap.len() {
            return Self {
                data: mmap,
                pos: 0,
                remaining: 0,
            };
        }
        let count = u16::from_le_bytes([mmap[off], mmap[off + 1]]);
        Self {
            data: mmap,
            pos: end,
            remaining: count,
        }
    }
}

impl Iterator for U32EntryIter<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        if self.remaining == 0 {
            return None;
        }
        let end = self.pos.checked_add(4)?;
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
    fn new(mmap: &'a [u8], offset: u64) -> Self {
        let Ok(off) = usize::try_from(offset) else {
            return Self {
                data: mmap,
                pos: 0,
                remaining: 0,
            };
        };
        let Some(end) = off.checked_add(2) else {
            return Self {
                data: mmap,
                pos: 0,
                remaining: 0,
            };
        };
        if end > mmap.len() {
            return Self {
                data: mmap,
                pos: 0,
                remaining: 0,
            };
        }
        let count = u16::from_le_bytes([mmap[off], mmap[off + 1]]);
        Self {
            data: mmap,
            pos: end,
            remaining: count,
        }
    }
}

impl Iterator for SegmentEntryIter<'_> {
    type Item = SegmentRef;

    fn next(&mut self) -> Option<SegmentRef> {
        if self.remaining == 0 {
            return None;
        }
        let end = self.pos.checked_add(SEGMENT_REF_SIZE)?;
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
    fn new(mmap: &'a [u8], offset: u64) -> Self {
        let Ok(off) = usize::try_from(offset) else {
            return Self {
                data: mmap,
                pos: 0,
                remaining: 0,
            };
        };
        let Some(end) = off.checked_add(2) else {
            return Self {
                data: mmap,
                pos: 0,
                remaining: 0,
            };
        };
        if end > mmap.len() {
            return Self {
                data: mmap,
                pos: 0,
                remaining: 0,
            };
        }
        let count = u16::from_le_bytes([mmap[off], mmap[off + 1]]);
        Self {
            data: mmap,
            pos: end,
            remaining: count,
        }
    }
}

impl Iterator for AdminEntryIter<'_> {
    type Item = (u32, bool); // (polygon_index, is_interior_hint)

    fn next(&mut self) -> Option<(u32, bool)> {
        if self.remaining == 0 {
            return None;
        }
        let end = self.pos.checked_add(4)?;
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
    /// Shared mmap traversal. The sink decides whether hits are ranked or
    /// materialized; `kinds` lets coarse fallback fill only missing classes.
    #[allow(clippy::too_many_arguments)]
    fn search_level<'a, S: SearchSink<'a>>(
        &'a self,
        ctx: &QueryContext,
        geo_cells_mmap: &Mmap,
        street_entries_mmap: &Mmap,
        addr_entries_mmap: &Mmap,
        interp_entries_mmap: &Mmap,
        level: u8,
        max_dist_sq: f64,
        kinds: SearchKinds,
        sink: &mut S,
    ) {
        let (cells, count) = cell_neighborhood(ctx.lat, ctx.lon, level);
        let cell_count = geo_cells_mmap.len() / GEO_CELL_SIZE;

        // An entry naming a record that does not exist is skipped, not
        // reported: open certifies list spans but not entry contents (see
        // validate_structure for why), and only a corrupt or hand-edited
        // index can carry one. The segment distance helpers return f64::MAX
        // for the same case, which never passes the radius test.
        for &cell_id in &cells[..count] {
            let Some(gc) = binary_search_cells(geo_cells_mmap, cell_count, cell_id) else {
                continue;
            };

            // Address points
            if kinds.addr && gc.addr_offset != format::NO_DATA_U64 {
                for idx in U32EntryIter::new(addr_entries_mmap, gc.addr_offset) {
                    let Some(pt) = self.read_addr_point(idx) else {
                        continue;
                    };
                    let dist_sq = geo::approx_distance_sq(
                        ctx.lat_rad,
                        ctx.lon_rad,
                        geo::e7_to_rad(pt.lat_e7),
                        geo::e7_to_rad(pt.lon_e7),
                        ctx.cos_lat,
                    );
                    if dist_sq < max_dist_sq {
                        sink.address(self, idx, pt, dist_sq);
                    }
                }
            }

            // Street segments
            if kinds.street && gc.street_offset != format::NO_DATA_U64 {
                for seg_ref in SegmentEntryIter::new(street_entries_mmap, gc.street_offset) {
                    let (snap_lat, snap_lon, dist_sq) = self.street_segment_distance(ctx, &seg_ref);
                    if dist_sq < max_dist_sq {
                        sink.street(self, seg_ref, snap_lat, snap_lon, dist_sq);
                    }
                }
            }

            // Interpolation segments
            if kinds.interp && gc.interp_offset != format::NO_DATA_U64 {
                for seg_ref in SegmentEntryIter::new(interp_entries_mmap, gc.interp_offset) {
                    let (snap_lat, snap_lon, dist_sq) = self.interp_segment_distance(ctx, &seg_ref);
                    if dist_sq < max_dist_sq {
                        sink.interpolation(self, seg_ref, snap_lat, snap_lon, dist_sq);
                    }
                }
            }
        }
    }

    // -- Admin search --------------------------------------------------------

    /// Ranked admin: one per level, smallest area. The interior high bit is
    /// read but ignored as a PIP bypass; every candidate still runs
    /// admin_polygon_contains (see the comment below).
    fn search_admin_ranked<'a>(&'a self, ctx: &QueryContext) -> Vec<AdminMatch<'a>> {
        let (cells, count) = cell_neighborhood(ctx.lat, ctx.lon, self.header.admin_cell_level);
        let cell_count = self.admin_cells.len() / ADMIN_CELL_SIZE;
        let mut best_by_level: [(Option<u32>, f32); 12] = [(None, f32::MAX); 12];

        for &cell_id in &cells[..count] {
            let Some(ac) = binary_search_admin_cells(&self.admin_cells, cell_count, cell_id) else {
                continue;
            };
            for (poly_idx, _is_interior) in
                AdminEntryIter::new(&self.admin_entries, ac.entries_offset)
            {
                // Out-of-range entry: skipped, as in search_level.
                let Some(poly) = self.read_admin_polygon(poly_idx) else {
                    continue;
                };
                let level = poly.admin_level as usize;
                if level >= 12 || poly.area >= best_by_level[level].1 {
                    continue;
                }
                // The interior high bit is only a hint: the reader still runs the
                // point-in-polygon test unconditionally. pbfhogg's builder (pass 3
                // flood-fill, cell-center sampling, no interior erosion / geometric
                // covering) does not guarantee that a flagged cell is fully inside
                // the polygon, so honoring the flag as a PIP bypass would mis-attribute
                // near-boundary and neighbor-cell queries. A false flag now only costs
                // wasted work, never a wrong answer. Re-enabling the skip requires a
                // builder that makes the interior hint sound through interior erosion
                // and geometric covering.
                if self.admin_polygon_contains(ctx, &poly) {
                    best_by_level[level] = (Some(poly_idx), poly.area);
                }
            }
        }

        let mut result = Vec::new();
        for &(maybe_idx, _) in &best_by_level {
            if let Some(poly_idx) = maybe_idx {
                let Some(poly) = self.read_admin_polygon(poly_idx) else {
                    continue;
                };
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
        let mut seen: FxHashSet<u32> = FxHashSet::default();

        for &cell_id in &cells[..count] {
            let Some(ac) = binary_search_admin_cells(&self.admin_cells, cell_count, cell_id) else {
                continue;
            };
            for (poly_idx, _is_interior) in
                AdminEntryIter::new(&self.admin_entries, ac.entries_offset)
            {
                // Record every attempted polygon, not only successful PIP
                // matches, so a non-containing polygon shared by neighboring
                // cells is fetched and tested exactly once.
                if !first_admin_attempt(&mut seen, poly_idx) {
                    continue;
                }
                // Out-of-range entry: skipped, as in search_level.
                let Some(poly) = self.read_admin_polygon(poly_idx) else {
                    continue;
                };
                // Interior high bit is a hint only; always run the PIP test. The
                // builder's flood-fill and cell-center sampling do not ensure flagged
                // cells lie inside the polygon, so bypassing this test can misattribute
                // near-boundary and neighbor-cell queries.
                if self.admin_polygon_contains(ctx, &poly) {
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
        #[cfg(feature = "test-hooks")]
        self.admin_pip_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Ok(start) = usize::try_from(poly.vertex_offset) else {
            return false;
        };
        let count = poly.vertex_count as usize;
        if count < 3 {
            return false;
        }

        // Parse vertices into rings separated by sentinel
        let coords = (0..count).map_while(|i| {
            let offset = i.checked_mul(NODE_COORD_SIZE)?.checked_add(start)?;
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
        // A globally in-bounds node can belong to the next way. A segment
        // needs two nodes within this way, including when node_count is zero.
        if u32::from(seg_ref.segment_index) + 1 >= u32::from(way.node_count) {
            return (0, 0, f64::MAX);
        }
        let Some(node_byte_offset) = usize::try_from(way.node_offset).ok().and_then(|base| {
            usize::from(seg_ref.segment_index)
                .checked_mul(NODE_COORD_SIZE)?
                .checked_add(base)
        }) else {
            return (0, 0, f64::MAX);
        };
        two_node_distance(ctx, &self.street_nodes, node_byte_offset)
    }

    #[allow(clippy::cast_possible_truncation)]
    fn interp_segment_distance(&self, ctx: &QueryContext, seg_ref: &SegmentRef) -> (i32, i32, f64) {
        let way_offset = seg_ref.way_index as usize * INTERP_WAY_SIZE;
        let Some(rec) = read_record::<INTERP_WAY_SIZE>(&self.interp_ways, way_offset) else {
            return (0, 0, f64::MAX);
        };
        let way = InterpWay::from_bytes(rec);
        // Check the owning geometry before accessing the shared node file.
        if u32::from(seg_ref.segment_index) + 1 >= u32::from(way.node_count) {
            return (0, 0, f64::MAX);
        }
        let Some(node_byte_offset) = usize::try_from(way.node_offset).ok().and_then(|base| {
            usize::from(seg_ref.segment_index)
                .checked_mul(NODE_COORD_SIZE)?
                .checked_add(base)
        }) else {
            return (0, 0, f64::MAX);
        };
        two_node_distance(ctx, &self.interp_nodes, node_byte_offset)
    }

    // -- Record readers ------------------------------------------------------

    fn read_addr_point(&self, index: u32) -> Option<AddrPoint> {
        decode_addr_point(&self.addr_points, index)
    }

    fn read_street_way(&self, index: u32) -> Option<StreetWay> {
        decode_street_way(&self.street_ways, index)
    }

    fn read_interp_way(&self, index: u32) -> Option<InterpWay> {
        decode_interp_way(&self.interp_ways, index)
    }

    fn read_admin_polygon(&self, index: u32) -> Option<AdminPolygon> {
        decode_admin_polygon(&self.admin_polygons, index)
    }

    fn read_string(&self, offset: u32) -> &str {
        // Open validates all referenced strings. The builder publishes separate
        // inodes, so refreshes preserve this invariant for existing mappings.
        // Rechecking here cannot make arbitrary in-place edits safe: truncation
        // can SIGBUS while reading the mmap, before an error can be returned.
        // External writers must likewise replace files instead of mutating them.
        format::read_nul_string(&self.strings, offset)
            .expect("Reader::open validated every referenced string")
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
        let iw = self.read_interp_way(way_idx)?;
        let house_number = self.interpolated_number(way_idx, seg_idx, snap_lat, snap_lon)?;
        Some(InterpolationMatch {
            street: self.read_string(iw.street_offset),
            house_number,
            distance_m: radians_sq_to_meters(dist_sq),
        })
    }

    #[allow(clippy::cast_possible_truncation)]
    fn way_length_radians(&self, nodes_mmap: &Mmap, node_offset: u64, node_count: u16) -> f64 {
        let mut total = 0.0;
        let Ok(base) = usize::try_from(node_offset) else {
            return 0.0;
        };
        for i in 0..(node_count as usize).saturating_sub(1) {
            let a = record_at(nodes_mmap, base, i).map(NodeCoord::from_bytes);
            let b = record_at(nodes_mmap, base, i + 1).map(NodeCoord::from_bytes);
            if let (Some(a), Some(b)) = (a, b) {
                total += segment_length_radians(&a, &b);
            }
        }
        total
    }

    #[allow(clippy::cast_possible_truncation)]
    fn accumulated_length_radians(
        &self,
        nodes_mmap: &Mmap,
        node_offset: u64,
        seg_idx: u16,
        snap_lat: i32,
        snap_lon: i32,
    ) -> f64 {
        let mut acc = 0.0;
        let Ok(base) = usize::try_from(node_offset) else {
            return 0.0;
        };
        for i in 0..seg_idx as usize {
            let a = record_at(nodes_mmap, base, i).map(NodeCoord::from_bytes);
            let b = record_at(nodes_mmap, base, i + 1).map(NodeCoord::from_bytes);
            if let (Some(a), Some(b)) = (a, b) {
                acc += segment_length_radians(&a, &b);
            }
        }
        // Partial segment to snap point
        if let Some(rec) = record_at(nodes_mmap, base, usize::from(seg_idx)) {
            let seg_start = NodeCoord::from_bytes(rec);
            let snap = NodeCoord {
                lat_e7: snap_lat,
                lon_e7: snap_lon,
            };
            acc += segment_length_radians(&seg_start, &snap);
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
        ctx.lon_rad,
        ctx.lat_rad,
        geo::e7_to_rad(a.lon_e7),
        geo::e7_to_rad(a.lat_e7),
        geo::e7_to_rad(b.lon_e7),
        geo::e7_to_rad(b.lat_e7),
        ctx.cos_lat,
    );
    let (snap_lat, snap_lon) = interpolate_coord(&a, &b, t);
    (snap_lat, snap_lon, dist_sq)
}

/// Interpolate a point at parameter `t` along the segment `a` -> `b`.
///
/// `point_to_segment_distance_sq` computes `t` in a longitude frame unwrapped
/// around `a`, so the interpolation has to happen in that same frame: a
/// segment from 179.9 to -179.9 with `t = 0.5` is at ±180, not at 0. The
/// result is folded back into the legal longitude domain afterwards.
#[allow(clippy::cast_possible_truncation)]
fn interpolate_coord(a: &NodeCoord, b: &NodeCoord, t: f64) -> (i32, i32) {
    let dlat = i64::from(b.lat_e7) - i64::from(a.lat_e7);
    let snap_lat = (f64::from(a.lat_e7) + t * dlat as f64) as i64;

    let dlon = geo::wrapped_longitude_delta_e7(i64::from(b.lon_e7) - i64::from(a.lon_e7));
    let unwrapped = (f64::from(a.lon_e7) + t * dlon as f64) as i64;
    let snap_lon = geo::normalize_longitude_e7(unwrapped);

    (snap_lat as i32, snap_lon as i32)
}

fn segment_length_radians(a: &NodeCoord, b: &NodeCoord) -> f64 {
    geo::approx_distance_sq(
        geo::e7_to_rad(a.lat_e7),
        geo::e7_to_rad(a.lon_e7),
        geo::e7_to_rad(b.lat_e7),
        geo::e7_to_rad(b.lon_e7),
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
    mmap.get(offset..offset.checked_add(N)?)?.try_into().ok()
}

fn record_at(mmap: &[u8], base: usize, index: usize) -> Option<&[u8; NODE_COORD_SIZE]> {
    let offset = index.checked_mul(NODE_COORD_SIZE)?.checked_add(base)?;
    read_record::<NODE_COORD_SIZE>(mmap, offset)
}

fn validate_list(offset: u64, data: &[u8], item_size: usize, kind: &str) -> Result<()> {
    if offset == format::NO_DATA_U64 {
        return Ok(());
    }
    let off = usize::try_from(offset).map_err(|_| {
        format!("invalid geocode index: {kind} offset {offset} does not fit this platform")
    })?;
    let count_end = off
        .checked_add(2)
        .ok_or_else(|| format!("invalid geocode index: {kind} offset overflows"))?;
    let count_bytes = data
        .get(off..count_end)
        .ok_or_else(|| format!("invalid geocode index: {kind} offset {offset} is out of bounds"))?;
    let count = usize::from(u16::from_le_bytes([count_bytes[0], count_bytes[1]]));
    let end = count
        .checked_mul(item_size)
        .and_then(|bytes| count_end.checked_add(bytes))
        .ok_or_else(|| format!("invalid geocode index: {kind} list size overflows"))?;
    if end > data.len() {
        return Err(format!(
            "invalid geocode index: {kind} list at {offset} extends past end of file"
        )
        .into());
    }
    Ok(())
}

/// Validate a fixed-count run of records at `offset` in `data`. Unlike
/// [`validate_list`] the count comes from the referring record rather than
/// from a length prefix in the target file.
fn validate_span(offset: u64, count: u64, item_size: usize, data: &[u8], kind: &str) -> Result<()> {
    if count == 0 {
        return Ok(());
    }
    let off = usize::try_from(offset).map_err(|_| {
        format!("invalid geocode index: {kind} offset {offset} does not fit this platform")
    })?;
    let end = usize::try_from(count)
        .ok()
        .and_then(|n| n.checked_mul(item_size))
        .and_then(|bytes| off.checked_add(bytes))
        .ok_or_else(|| format!("invalid geocode index: {kind} span at {offset} overflows"))?;
    if end > data.len() {
        return Err(format!(
            "invalid geocode index: {kind} span at {offset} covering {count} records extends \
             past end of file ({} bytes)",
            data.len()
        )
        .into());
    }
    Ok(())
}

/// Validate a string-pool offset. Offset 0 is the "no string" sentinel that
/// [`format::read_nul_string`] already treats as empty.
fn validate_string_offset(offset: u32, strings: &[u8], kind: &str) -> Result<()> {
    format::read_nul_string(strings, offset)
        .map(|_| ())
        .ok_or_else(|| {
            format!(
                "invalid geocode index: {kind} string at offset {offset} is out of bounds, \
             unterminated, or invalid UTF-8"
            )
            .into()
        })
}

fn radians_sq_to_meters(rad_sq: f64) -> f64 {
    rad_sq.sqrt() * geo::EARTH_RADIUS_M
}

fn nonzero_string(reader: &Reader, offset: u32) -> Option<&str> {
    if offset == 0 {
        None
    } else {
        Some(reader.read_string(offset))
    }
}

fn decode_addr_point(data: &[u8], index: u32) -> Option<AddrPoint> {
    read_record::<ADDR_POINT_SIZE>(data, index as usize * ADDR_POINT_SIZE)
        .map(AddrPoint::from_bytes)
}

fn decode_street_way(data: &[u8], index: u32) -> Option<StreetWay> {
    read_record::<STREET_WAY_SIZE>(data, index as usize * STREET_WAY_SIZE)
        .map(StreetWay::from_bytes)
}

fn decode_interp_way(data: &[u8], index: u32) -> Option<InterpWay> {
    read_record::<INTERP_WAY_SIZE>(data, index as usize * INTERP_WAY_SIZE)
        .map(InterpWay::from_bytes)
}

fn decode_admin_polygon(data: &[u8], index: u32) -> Option<AdminPolygon> {
    read_record::<ADMIN_POLYGON_SIZE>(data, index as usize * ADMIN_POLYGON_SIZE)
        .map(AdminPolygon::from_bytes)
}

fn first_admin_attempt(seen: &mut FxHashSet<u32>, polygon_index: u32) -> bool {
    seen.insert(polygon_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defensive_fixed_record_readers_expose_absence_in_their_types() {
        let _: fn(&Reader, u32) -> Option<AddrPoint> = Reader::read_addr_point;
        let _: fn(&Reader, u32) -> Option<StreetWay> = Reader::read_street_way;
        let _: fn(&Reader, u32) -> Option<InterpWay> = Reader::read_interp_way;
        let _: fn(&Reader, u32) -> Option<AdminPolygon> = Reader::read_admin_polygon;
        assert!(decode_addr_point(&[], 0).is_none());
        assert!(decode_street_way(&[], 0).is_none());
        assert!(decode_interp_way(&[], 0).is_none());
        assert!(decode_admin_polygon(&[], 0).is_none());
    }

    #[test]
    fn failed_admin_containment_candidates_are_still_deduplicated() {
        let mut seen = FxHashSet::default();
        assert!(first_admin_attempt(&mut seen, 17));
        // This helper is called before polygon fetch and containment. A failed
        // PIP therefore cannot make the same polygon eligible in a neighbor.
        assert!(!first_admin_attempt(&mut seen, 17));
    }

    #[test]
    fn string_validation_rejects_every_corruption_class() {
        // Offset 0 is the format's genuine empty string, used by every absent
        // optional field (a missing postcode, say). `StringPool::new` always
        // writes the sentinel NUL, so even an index carrying no names at all
        // has a one-byte pool - validation must accept it, or such an index
        // would be unopenable.
        assert!(validate_string_offset(0, b"\0", "test").is_ok());
        // A zero-length pool is a different thing: no builder emits one, so a
        // truncated strings.bin is corruption and is rejected rather than
        // silently reading as empty.
        assert!(validate_string_offset(0, b"", "test").is_err());
        for (offset, pool) in [
            (1, b"\0\xff\0" as &[u8]),
            (1, b"\0unterminated"),
            (99, b"\0"),
        ] {
            let error = validate_string_offset(offset, pool, "test")
                .expect_err("corrupt string must be rejected");
            assert!(error.to_string().contains("unterminated, or invalid UTF-8"));
        }
    }
}
