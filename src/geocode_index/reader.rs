//! Memory-mapped reader for the reverse geocoding index.
//!
//! Opens an index directory, memory-maps all files, and provides two query
//! levels: raw [`Candidates`] and ranked [`ReverseResult`].

use std::marker::PhantomData;
use std::path::Path;
use std::{error::Error, fmt};

use memmap2::Mmap;
use rustc_hash::FxHashSet;
use s2::cellid::CellID;
use s2::latlng::LatLng;

use super::format::{
    self, ADMIN_LEVEL_SLOTS, AddrEntry, AddrPoint, AdminCell, AdminEntry, AdminPolygon, GeoCell,
    HEADER_SIZE, Header, InterpWay, InterpolationType, NodeCoord, Record, Records, RunError,
    SegmentRef, StreetWay, Table,
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
    /// One entry per administrative level (2-10) plus postal boundaries (11),
    /// smallest-area polygon at each level.
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
        let mut best_by_level: [Option<AdminMatch<'a>>; ADMIN_LEVEL_SLOTS] = Default::default();
        for m in self.admin {
            let level = m.admin_level as usize;
            if level < ADMIN_LEVEL_SLOTS {
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
/// `Send + Sync` - all fields are `Mmap` (which is `Send + Sync`), typed
/// wrappers of one, plain values, or (under `test-hooks`) an atomic counter.
pub struct Reader {
    /// Admin point-in-polygon tests run by this reader since it was opened
    /// or last reset. Per reader, so a test observes only its own queries.
    #[cfg(feature = "test-hooks")]
    admin_pip_calls: std::sync::atomic::AtomicUsize,

    header: Header,
    fine_max_dist_sq: f64,
    coarse_max_dist_sq: f64,

    geo_cells: Records<GeoCell>,
    street_entries: Lists<SegmentRef>,
    addr_entries: Lists<AddrEntry>,
    interp_entries: Lists<SegmentRef>,
    coarse_geo_cells: Records<GeoCell>,
    coarse_street_entries: Lists<SegmentRef>,
    coarse_addr_entries: Lists<AddrEntry>,
    coarse_interp_entries: Lists<SegmentRef>,
    street_ways: Records<StreetWay>,
    street_nodes: Records<NodeCoord>,
    addr_points: Records<AddrPoint>,
    interp_ways: Records<InterpWay>,
    interp_nodes: Records<NodeCoord>,
    admin_cells: Records<AdminCell>,
    admin_entries: Lists<AdminEntry>,
    admin_polygons: Records<AdminPolygon>,
    admin_vertices: Records<NodeCoord>,
    strings: Mmap,
}

/// A mapped file of length-prefixed lists of `T`, one per cell. (Files that
/// share a record type - the street and interpolation entry files - are bound
/// to their roles only by the field they are opened into.)
struct Lists<T> {
    map: Mmap,
    record: PhantomData<fn() -> T>,
}

impl<T: Record> Lists<T> {
    fn map(path: &Path) -> Result<Self> {
        Ok(Self {
            map: format::mmap_or_empty(path)?,
            record: PhantomData,
        })
    }

    #[inline]
    fn list(&self, offset: u64) -> std::result::Result<Table<'_, T>, RunError> {
        Table::list(&self.map, offset)
    }

    /// The list at `offset` for a query. Open validated every list a cell
    /// names, so a malformed one is unreachable here; it reads as empty
    /// rather than failing the query.
    #[inline]
    fn entries(&self, offset: u64) -> Table<'_, T> {
        self.list(offset).unwrap_or_default()
    }
}

type Result<T> = crate::Result<T>;

/// A malformed index file: the index content is bad, not the request.
fn corrupt(msg: impl Into<std::borrow::Cow<'static, str>>) -> crate::Error {
    crate::Error::invalid_data(msg)
}

/// A validation failure while checking record `index` of `file`.
#[cold]
fn in_record(err: crate::Error, file: &str, index: usize) -> crate::Error {
    err.context(format!("{file} record {index}"))
}

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
    ///
    /// # Errors
    /// A file is missing, unreadable or malformed; the error names the
    /// index directory, and the file (and record) where one is at fault.
    pub fn open(dir: &Path) -> Result<Self> {
        Self::open_in(dir).map_err(|e| e.locate(crate::error::Location::in_file(dir)))
    }

    fn open_in(dir: &Path) -> Result<Self> {
        let header_path = dir.join(format::FILE_HEADER);
        let header_bytes = crate::read::input::read(&header_path)?;
        let in_header = |e: crate::Error| e.locate(crate::error::Location::in_file(&header_path));
        if header_bytes.len() < HEADER_SIZE {
            return Err(in_header(corrupt(format!(
                "geocode header too short: {} bytes, expected {HEADER_SIZE}",
                header_bytes.len()
            ))));
        }
        let header = Header::from_bytes(
            header_bytes[..HEADER_SIZE]
                .try_into()
                .map_err(|_| in_header(corrupt("header size mismatch")))?,
        )
        .map_err(|e| in_header(crate::Error::invalid_data_from(e)))?;

        let path = |name: &str| dir.join(name);

        let fine_radius_m = header.fine_search_radius_m as f64;
        let coarse_radius_m = header.coarse_search_radius_m as f64;

        let reader = Self {
            #[cfg(feature = "test-hooks")]
            admin_pip_calls: std::sync::atomic::AtomicUsize::new(0),
            fine_max_dist_sq: geo::meters_to_radians_sq(fine_radius_m),
            coarse_max_dist_sq: geo::meters_to_radians_sq(coarse_radius_m),
            header,
            geo_cells: Records::map(&path(format::FILE_GEO_CELLS))?,
            street_entries: Lists::map(&path(format::FILE_STREET_ENTRIES))?,
            addr_entries: Lists::map(&path(format::FILE_ADDR_ENTRIES))?,
            interp_entries: Lists::map(&path(format::FILE_INTERP_ENTRIES))?,
            coarse_geo_cells: Records::map(&path(format::FILE_COARSE_GEO_CELLS))?,
            coarse_street_entries: Lists::map(&path(format::FILE_COARSE_STREET_ENTRIES))?,
            coarse_addr_entries: Lists::map(&path(format::FILE_COARSE_ADDR_ENTRIES))?,
            coarse_interp_entries: Lists::map(&path(format::FILE_COARSE_INTERP_ENTRIES))?,
            street_ways: Records::map(&path(format::FILE_STREET_WAYS))?,
            street_nodes: Records::map(&path(format::FILE_STREET_NODES))?,
            addr_points: Records::map(&path(format::FILE_ADDR_POINTS))?,
            interp_ways: Records::map(&path(format::FILE_INTERP_WAYS))?,
            interp_nodes: Records::map(&path(format::FILE_INTERP_NODES))?,
            admin_cells: Records::map(&path(format::FILE_ADMIN_CELLS))?,
            admin_entries: Lists::map(&path(format::FILE_ADMIN_ENTRIES))?,
            admin_polygons: Records::map(&path(format::FILE_ADMIN_POLYGONS))?,
            admin_vertices: Records::map(&path(format::FILE_ADMIN_VERTICES))?,
            strings: format::mmap_or_empty(&path(format::FILE_STRINGS))?,
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
                .ok_or_else(|| {
                    corrupt(format!("{name} size from header overflows this platform"))
                })?;
            if actual != expected {
                return Err(corrupt(format!(
                    "invalid geocode index: {name} is {actual} bytes, expected {expected}"
                )));
            }
            Ok(())
        }
        exact(
            format::FILE_GEO_CELLS,
            self.geo_cells.byte_len(),
            self.header.geo_cell_count,
            GeoCell::SIZE,
        )?;
        exact(
            format::FILE_COARSE_GEO_CELLS,
            self.coarse_geo_cells.byte_len(),
            self.header.coarse_cell_count,
            GeoCell::SIZE,
        )?;
        exact(
            format::FILE_ADMIN_CELLS,
            self.admin_cells.byte_len(),
            self.header.admin_cell_count,
            AdminCell::SIZE,
        )?;
        exact(
            format::FILE_STREET_WAYS,
            self.street_ways.byte_len(),
            self.header.street_way_count,
            StreetWay::SIZE,
        )?;
        exact(
            format::FILE_ADDR_POINTS,
            self.addr_points.byte_len(),
            self.header.addr_point_count,
            AddrPoint::SIZE,
        )?;
        exact(
            format::FILE_INTERP_WAYS,
            self.interp_ways.byte_len(),
            self.header.interp_way_count,
            InterpWay::SIZE,
        )?;
        exact(
            format::FILE_ADMIN_POLYGONS,
            self.admin_polygons.byte_len(),
            self.header.admin_polygon_count,
            AdminPolygon::SIZE,
        )?;

        // Every walk below runs after the exact-length checks above, so each
        // record file's table holds exactly its header count of records.
        Self::validate_geo_tree(
            format::FILE_GEO_CELLS,
            &self.geo_cells,
            &self.street_entries,
            &self.addr_entries,
            &self.interp_entries,
        )?;
        Self::validate_geo_tree(
            format::FILE_COARSE_GEO_CELLS,
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
    /// incomplete admin results: a query reads a malformed list as empty, so
    /// the caller cannot tell a broken list from a genuinely empty one.
    fn validate_admin_tree(&self) -> Result<()> {
        for (index, cell) in self.admin_cells.table().iter().enumerate() {
            validate_list(&self.admin_entries, cell.entries_offset, "admin entry")
                .map_err(|e| in_record(e, format::FILE_ADMIN_CELLS, index))?;
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
    /// them are individually bounds-safe - a span that does not fit reads as
    /// no geometry - but "safe" is not "correct": a truncated nodes file
    /// turns a street into a missing street and a polygon into no polygon,
    /// silently, and the reader answers queries without it.
    ///
    /// String-pool offsets (`name_offset`, `housenumber_offset`,
    /// `street_offset`, `postcode_offset`, `country_code_offset`) are validated
    /// here as well. `read_nul_string` independently returns `None` for an
    /// invalid offset, missing terminator, or invalid UTF-8.
    fn validate_way_geometry(&self) -> Result<()> {
        for (index, way) in self.street_ways.table().iter().enumerate() {
            (|| -> Result<()> {
                validate_span(
                    &self.street_nodes,
                    way.node_offset,
                    u64::from(way.node_count),
                    "street way node",
                )?;
                validate_string_offset(way.name_offset, &self.strings, "street way name")
            })()
            .map_err(|e| in_record(e, format::FILE_STREET_WAYS, index))?;
        }
        for (index, way) in self.interp_ways.table().iter().enumerate() {
            (|| -> Result<()> {
                InterpolationType::try_from(way.interpolation_type)
                    .map_err(crate::Error::invalid_data_from)?;
                validate_span(
                    &self.interp_nodes,
                    way.node_offset,
                    u64::from(way.node_count),
                    "interpolation way node",
                )?;
                validate_string_offset(way.street_offset, &self.strings, "interpolation way street")
            })()
            .map_err(|e| in_record(e, format::FILE_INTERP_WAYS, index))?;
        }
        for (index, poly) in self.admin_polygons.table().iter().enumerate() {
            (|| -> Result<()> {
                // Producer rings repeat their first vertex: assemble_rings
                // keeps only closed rings and simplify_ring preserves both
                // endpoints. Do not scan every vertex here solely to
                // recertify closure; that would add coordinate-page reads to
                // metadata validation at open.
                validate_span(
                    &self.admin_vertices,
                    poly.vertex_offset,
                    u64::from(poly.vertex_count),
                    "admin polygon vertex",
                )?;
                validate_string_offset(poly.name_offset, &self.strings, "admin polygon name")?;
                validate_string_offset(
                    poly.country_code_offset,
                    &self.strings,
                    "admin polygon country code",
                )
            })()
            .map_err(|e| in_record(e, format::FILE_ADMIN_POLYGONS, index))?;
        }
        for (index, pt) in self.addr_points.table().iter().enumerate() {
            (|| -> Result<()> {
                validate_string_offset(
                    pt.housenumber_offset,
                    &self.strings,
                    "address housenumber",
                )?;
                validate_string_offset(pt.street_offset, &self.strings, "address street")?;
                validate_string_offset(pt.postcode_offset, &self.strings, "address postcode")
            })()
            .map_err(|e| in_record(e, format::FILE_ADDR_POINTS, index))?;
        }
        Ok(())
    }

    // Fine and coarse trees have the same shape and different files. Keep
    // the pairing visible at the two call sites: the street and
    // interpolation entry files share a type, so the types alone cannot
    // certify which is which.
    fn validate_geo_tree(
        cells_file: &'static str,
        cells: &Records<GeoCell>,
        streets: &Lists<SegmentRef>,
        addrs: &Lists<AddrEntry>,
        interps: &Lists<SegmentRef>,
    ) -> Result<()> {
        for (index, cell) in cells.table().iter().enumerate() {
            (|| -> Result<()> {
                validate_list(addrs, cell.addr_offset, "address entry")?;
                validate_list(streets, cell.street_offset, "street entry")?;
                validate_list(interps, cell.interp_offset, "interpolation entry")
            })()
            .map_err(|e| in_record(e, cells_file, index))?;
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

        let interpolation = best.interp.and_then(
            |(way_idx, seg_idx, snap_lat, snap_lon, dist_sq, total_len)| {
                self.resolve_interpolation(way_idx, seg_idx, snap_lat, snap_lon, dist_sq, total_len)
            },
        );

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

    /// Return the total length for a way that has a usable house-number range.
    ///
    /// Keep this calculation candidate-driven. Building a table at open time
    /// would walk every interpolation node even when a reader serves only a
    /// small area. The ranked query retains the winning length, avoiding the
    /// second walk when it computes that result. Search sinks use this check
    /// before recording a hit so an unusable fine segment cannot suppress a
    /// usable coarse fallback. The unranked candidate API still checks each
    /// returned hit and avoids an eager all-ways cache at reader open.
    fn interpolation_total_length(&self, iw: InterpWay) -> Option<f64> {
        if iw.start_number == 0 || iw.end_number == 0 {
            return None;
        }
        let total_len = self.way_length_radians(&iw);
        (total_len >= crate::tunables::INTERP_MIN_TOTAL_LEN).then_some(total_len)
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
        // Reject an invalid segment before reading every node in its way.
        if u32::from(segment_index) + 1 >= u32::from(iw.node_count) {
            return None;
        }
        let total_len = self.interpolation_total_length(iw)?;
        self.interpolated_number_at_length(iw, segment_index, snap_lat_e7, snap_lon_e7, total_len)
    }

    fn interpolated_number_at_length(
        &self,
        iw: InterpWay,
        segment_index: u16,
        snap_lat_e7: i32,
        snap_lon_e7: i32,
        total_len: f64,
    ) -> Option<u32> {
        // Keep the segment bound at the calculation boundary as defense in
        // depth; the public entry point checks it before reading the way.
        if u32::from(segment_index) + 1 >= u32::from(iw.node_count) {
            return None;
        }
        if iw.start_number == 0
            || iw.end_number == 0
            || total_len < crate::tunables::INTERP_MIN_TOTAL_LEN
        {
            return None;
        }
        let acc_len = self.accumulated_length_radians(&iw, segment_index, snap_lat_e7, snap_lon_e7);
        let t = acc_len / total_len;
        let diff = iw.end_number as f64 - iw.start_number as f64;
        // Even and odd both preserve start_number parity. Endpoint numbers
        // remain authoritative, including imported ranges with mismatched tags.
        let number = match InterpolationType::try_from(iw.interpolation_type).ok()? {
            InterpolationType::All => iw.start_number as f64 + (t * diff).round(),
            InterpolationType::Even | InterpolationType::Odd => {
                iw.start_number as f64 + 2.0 * ((t * diff) / 2.0).round()
            }
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
    addr: Option<(u32, f64)>,             // (index, dist_sq)
    street: Option<(u32, i32, i32, f64)>, // (way_idx, snap_lat, snap_lon, dist_sq)
    // (way_idx, seg_idx, snap_lat, snap_lon, dist_sq, total way length)
    interp: Option<(u32, u16, i32, i32, f64, f64)>,
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
        if dist_sq < self.interp.map_or(f64::MAX, |best| best.4) {
            let Some(way) = reader.read_interp_way(segment.way_index) else {
                return;
            };
            // Only candidates that can improve the current result need a full
            // way-length scan. Retain the winner's length to avoid scanning it
            // again while assembling the final house number.
            let Some(total_len) = reader.interpolation_total_length(way) else {
                return;
            };
            self.interp = Some((
                segment.way_index,
                segment.segment_index,
                snap_lat,
                snap_lon,
                dist_sq,
                total_len,
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
        // This API returns every viable segment hit, so validate each one
        // before it can make the fine-level candidate list nonempty.
        if reader.interpolation_total_length(way).is_none() {
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
    // The fine/coarse call sites pair already-open mmaps with their levels
    // and radii. A file-name descriptor alone cannot enforce that relationship.
    fn search_level<'a, S: SearchSink<'a>>(
        &'a self,
        ctx: &QueryContext,
        geo_cells: &Records<GeoCell>,
        street_entries: &Lists<SegmentRef>,
        addr_entries: &Lists<AddrEntry>,
        interp_entries: &Lists<SegmentRef>,
        level: u8,
        max_dist_sq: f64,
        kinds: SearchKinds,
        sink: &mut S,
    ) {
        let (cells, count) = cell_neighborhood(ctx.lat, ctx.lon, level);
        let geo_cells = geo_cells.table();

        // An entry naming a record that does not exist is skipped, not
        // reported: open certifies list spans but not entry contents (see
        // validate_structure for why), and only a corrupt or hand-edited
        // index can carry one. The segment distance helpers return f64::MAX
        // for the same case, which never passes the radius test.
        for &cell_id in &cells[..count] {
            let Some(gc) = geo_cells.find_by_key(cell_id, |cell| cell.cell_id) else {
                continue;
            };

            // Address points
            if kinds.addr {
                for entry in addr_entries.entries(gc.addr_offset).iter() {
                    let idx = entry.addr_index;
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
            if kinds.street {
                for seg_ref in street_entries.entries(gc.street_offset).iter() {
                    let (snap_lat, snap_lon, dist_sq) = self.street_segment_distance(ctx, &seg_ref);
                    if dist_sq < max_dist_sq {
                        sink.street(self, seg_ref, snap_lat, snap_lon, dist_sq);
                    }
                }
            }

            // Interpolation segments
            if kinds.interp {
                for seg_ref in interp_entries.entries(gc.interp_offset).iter() {
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
        let admin_cells = self.admin_cells.table();
        let mut best_by_level: [(Option<u32>, f32); ADMIN_LEVEL_SLOTS] =
            [(None, f32::MAX); ADMIN_LEVEL_SLOTS];

        for &cell_id in &cells[..count] {
            let Some(ac) = admin_cells.find_by_key(cell_id, |cell| cell.cell_id) else {
                continue;
            };
            for entry in self.admin_entries.entries(ac.entries_offset).iter() {
                let poly_idx = entry.polygon_index();
                // Out-of-range entry: skipped, as in search_level.
                let Some(poly) = self.read_admin_polygon(poly_idx) else {
                    continue;
                };
                let level = poly.admin_level as usize;
                if level >= ADMIN_LEVEL_SLOTS || poly.area >= best_by_level[level].1 {
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
        let admin_cells = self.admin_cells.table();
        let mut result = Vec::new();
        let mut seen: FxHashSet<u32> = FxHashSet::default();

        for &cell_id in &cells[..count] {
            let Some(ac) = admin_cells.find_by_key(cell_id, |cell| cell.cell_id) else {
                continue;
            };
            for entry in self.admin_entries.entries(ac.entries_offset).iter() {
                let poly_idx = entry.polygon_index();
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
        if poly.vertex_count < 3 {
            return false;
        }
        // Open validated this span; re-deriving it here is the local bound.
        let Ok(vertices) = self
            .admin_vertices
            .span(poly.vertex_offset, u64::from(poly.vertex_count))
        else {
            return false;
        };

        // Parse vertices into rings separated by sentinel
        let rings = format::parse_rings(vertices.iter());
        if rings.is_empty() {
            return false;
        }

        let exterior = &rings[0];
        let holes: Vec<&[(f64, f64)]> = rings[1..].iter().map(Vec::as_slice).collect();
        geo::point_in_polygon(ctx.lon, ctx.lat, exterior, &holes)
    }

    // -- Segment distance ----------------------------------------------------

    fn street_segment_distance(&self, ctx: &QueryContext, seg_ref: &SegmentRef) -> (i32, i32, f64) {
        let Some(way) = self.read_street_way(seg_ref.way_index) else {
            return SEGMENT_MISS;
        };
        segment_distance(
            ctx,
            &self.street_nodes,
            way.node_offset,
            way.node_count,
            seg_ref.segment_index,
        )
    }

    fn interp_segment_distance(&self, ctx: &QueryContext, seg_ref: &SegmentRef) -> (i32, i32, f64) {
        let Some(way) = self.read_interp_way(seg_ref.way_index) else {
            return SEGMENT_MISS;
        };
        segment_distance(
            ctx,
            &self.interp_nodes,
            way.node_offset,
            way.node_count,
            seg_ref.segment_index,
        )
    }

    // -- Record readers ------------------------------------------------------

    #[inline]
    fn read_addr_point(&self, index: u32) -> Option<AddrPoint> {
        self.addr_points.table().get(index as usize)
    }

    #[inline]
    fn read_street_way(&self, index: u32) -> Option<StreetWay> {
        self.street_ways.table().get(index as usize)
    }

    #[inline]
    fn read_interp_way(&self, index: u32) -> Option<InterpWay> {
        self.interp_ways.table().get(index as usize)
    }

    #[inline]
    fn read_admin_polygon(&self, index: u32) -> Option<AdminPolygon> {
        self.admin_polygons.table().get(index as usize)
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
        total_len: f64,
    ) -> Option<InterpolationMatch<'_>> {
        let iw = self.read_interp_way(way_idx)?;
        let house_number =
            self.interpolated_number_at_length(iw, seg_idx, snap_lat, snap_lon, total_len)?;
        Some(InterpolationMatch {
            street: self.read_string(iw.street_offset),
            house_number,
            distance_m: radians_sq_to_meters(dist_sq),
        })
    }

    /// The interpolation way's nodes. Open validated the span; a way whose
    /// span does not fit reads as having no nodes.
    #[inline]
    fn interp_way_nodes(&self, iw: &InterpWay) -> Table<'_, NodeCoord> {
        self.interp_nodes
            .span(iw.node_offset, u64::from(iw.node_count))
            .unwrap_or_default()
    }

    fn way_length_radians(&self, iw: &InterpWay) -> f64 {
        let mut nodes = self.interp_way_nodes(iw).iter();
        let Some(mut prev) = nodes.next() else {
            return 0.0;
        };
        let mut total = 0.0;
        for node in nodes {
            total += segment_length_radians(&prev, &node);
            prev = node;
        }
        total
    }

    /// Length along the way from its first node to the snap point on
    /// segment `seg_idx`. The caller has checked `seg_idx + 1 < node_count`.
    fn accumulated_length_radians(
        &self,
        iw: &InterpWay,
        seg_idx: u16,
        snap_lat: i32,
        snap_lon: i32,
    ) -> f64 {
        let mut nodes = self
            .interp_way_nodes(iw)
            .iter()
            .take(usize::from(seg_idx) + 1);
        let Some(mut prev) = nodes.next() else {
            return 0.0;
        };
        let mut acc = 0.0;
        for node in nodes {
            acc += segment_length_radians(&prev, &node);
            prev = node;
        }
        // Partial segment from the segment's start node to the snap point
        let snap = NodeCoord {
            lat_e7: snap_lat,
            lon_e7: snap_lon,
        };
        acc + segment_length_radians(&prev, &snap)
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// `(snap_lat, snap_lon, dist_sq)` for an entry that names no usable
/// segment: the distance never passes a radius test.
const SEGMENT_MISS: (i32, i32, f64) = (0, 0, f64::MAX);

/// Distance from the query to segment `segment` of the way whose nodes are
/// the `node_count` records at `node_offset` in `nodes`. Both nodes are read
/// from the way's own span: a node that is in the file but past this way's
/// count belongs to the next way.
fn segment_distance(
    ctx: &QueryContext,
    nodes: &Records<NodeCoord>,
    node_offset: u64,
    node_count: u16,
    segment: u16,
) -> (i32, i32, f64) {
    // Widened before the +1, and fails for node_count 0 and 1.
    if u32::from(segment) + 1 >= u32::from(node_count) {
        return SEGMENT_MISS;
    }
    let Ok(way_nodes) = nodes.span(node_offset, u64::from(node_count)) else {
        return SEGMENT_MISS;
    };
    let first = usize::from(segment);
    let (Some(a), Some(b)) = (way_nodes.get(first), way_nodes.get(first + 1)) else {
        return SEGMENT_MISS;
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

/// Validate the list a cell names at `offset`. The framing is
/// [`Table::list`]'s; this only words its refusal.
fn validate_list<T: Record>(lists: &Lists<T>, offset: u64, kind: &str) -> Result<()> {
    lists.list(offset).map(drop).map_err(|err| {
        corrupt(match err {
            RunError::OffsetUnaddressable => {
                format!("invalid geocode index: {kind} offset {offset} does not fit this platform")
            }
            RunError::CountOutOfBounds => {
                format!("invalid geocode index: {kind} offset {offset} is out of bounds")
            }
            RunError::SizeOverflow => {
                format!("invalid geocode index: {kind} offset {offset}: list size overflows")
            }
            RunError::PastEnd => {
                format!("invalid geocode index: {kind} list at {offset} extends past end of file")
            }
        })
    })
}

/// Validate a fixed-count run of records at `offset`. Unlike
/// [`validate_list`] the count comes from the referring record rather than
/// from a length prefix in the target file.
fn validate_span<T: Record>(
    records: &Records<T>,
    offset: u64,
    count: u64,
    kind: &str,
) -> Result<()> {
    records.span(offset, count).map(drop).map_err(|err| {
        corrupt(match err {
            RunError::OffsetUnaddressable => {
                format!("invalid geocode index: {kind} offset {offset} does not fit this platform")
            }
            RunError::SizeOverflow | RunError::CountOutOfBounds => {
                format!("invalid geocode index: {kind} span at {offset} overflows")
            }
            RunError::PastEnd => format!(
                "invalid geocode index: {kind} span at {offset} covering {count} records extends \
                 past end of file ({} bytes)",
                records.byte_len()
            ),
        })
    })
}

/// Validate a string-pool offset. Offset 0 is the "no string" sentinel that
/// [`format::read_nul_string`] already treats as empty.
fn validate_string_offset(offset: u32, strings: &[u8], kind: &str) -> Result<()> {
    format::read_nul_string(strings, offset)
        .map(|_| ())
        .ok_or_else(|| {
            corrupt(format!(
                "invalid geocode index: {kind} string at offset {offset} is out of bounds, \
             unterminated, or invalid UTF-8"
            ))
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
        assert!(Table::<AddrPoint>::new(&[]).get(0).is_none());
        assert!(Table::<StreetWay>::new(&[]).get(0).is_none());
        assert!(Table::<InterpWay>::new(&[]).get(0).is_none());
        assert!(Table::<AdminPolygon>::new(&[]).get(0).is_none());
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
