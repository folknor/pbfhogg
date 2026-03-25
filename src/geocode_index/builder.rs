//! Builder for the reverse geocoding index.
//!
//! Reads an OSM PBF file in multiple passes and writes the set of binary index
//! files described in `notes/reverse-geocoding-spec.md` section 4.

use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;
use s2::cellid::CellID;
use s2::latlng::LatLng;

use crate::commands::add_locations_to_ways::DenseMmapIndex;
use crate::{BlobFilter, Element, ElementReader, MemberId};

use super::format::*;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// Highway exclusion list
// ---------------------------------------------------------------------------

const EXCLUDED_HIGHWAYS: &[&str] = &[
    "footway", "path", "track", "steps", "cycleway",
    "service", "pedestrian", "bridleway", "construction",
];

// ---------------------------------------------------------------------------
// Build configuration
// ---------------------------------------------------------------------------

/// Configuration for the geocode index builder.
pub struct BuildConfig {
    pub input_path: PathBuf,
    pub output_dir: PathBuf,
    pub force: bool,
    pub street_level: u8,
    pub coarse_level: u8,
    pub admin_level: u8,
    pub max_admin_vertices: u16,
    pub fine_search_radius_m: f32,
    pub coarse_search_radius_m: f32,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            input_path: PathBuf::new(),
            output_dir: PathBuf::new(),
            force: false,
            street_level: 17,
            coarse_level: 14,
            admin_level: 10,
            max_admin_vertices: 500,
            fine_search_radius_m: 75.0,
            coarse_search_radius_m: 1000.0,
        }
    }
}

/// Build statistics.
#[derive(Debug, Default)]
pub struct BuildStats {
    pub addr_points: u64,
    pub street_ways: u64,
    pub interp_ways: u64,
    pub admin_polygons: u64,
    pub fine_cells: u64,
    pub coarse_cells: u64,
    pub admin_cells: u64,
}

// ---------------------------------------------------------------------------
// String pool
// ---------------------------------------------------------------------------

struct StringPool {
    data: Vec<u8>,
    index: FxHashMap<String, u32>,
}

impl StringPool {
    fn new() -> Self {
        let mut pool = Self {
            data: Vec::new(),
            index: FxHashMap::default(),
        };
        // Offset 0 = empty string
        pool.data.push(0);
        pool
    }

    #[allow(clippy::cast_possible_truncation)]
    fn intern(&mut self, s: &str) -> u32 {
        if s.is_empty() {
            return 0;
        }
        if let Some(&offset) = self.index.get(s) {
            return offset;
        }
        let offset = self.data.len() as u32;
        self.index.insert(s.to_owned(), offset);
        self.data.extend_from_slice(s.as_bytes());
        self.data.push(0);
        offset
    }
}

// ---------------------------------------------------------------------------
// Intermediate data
// ---------------------------------------------------------------------------

struct RawAddrPoint {
    lat_e7: i32,
    lon_e7: i32,
    housenumber_offset: u32,
    street_offset: u32,
    postcode_offset: u32,
}

struct RawStreetWay {
    name_offset: u32,
    nodes: Vec<(i32, i32)>,
}

struct RawInterpWay {
    street_offset: u32,
    interpolation_type: u8,
    nodes: Vec<(i32, i32)>,
    start_number: u32,
    end_number: u32,
}

struct RawAdminRelation {
    admin_level: u8,
    name_offset: u32,
    country_code_offset: u32,
    outer_way_ids: Vec<i64>,
    inner_way_ids: Vec<i64>,
}

struct AssembledPolygon {
    admin_level: u8,
    name_offset: u32,
    country_code_offset: u32,
    area: f32,
    vertices: Vec<NodeCoord>,
}

// Cell entry types for sorting
struct AddrCellEntry { cell_id: u64, addr_index: u32 }
struct SegCellEntry { cell_id: u64, way_index: u32, segment_index: u16 }
struct AdminCellEntry { cell_id: u64, poly_index: u32, is_interior: bool }

// ---------------------------------------------------------------------------
// Main build function
// ---------------------------------------------------------------------------

/// Build the reverse geocoding index from an OSM PBF file.
///
/// # Known limitations
///
/// - **Planet scale:** All intermediate data is held in RAM. Planet-scale builds
///   (>>64 GB) require streaming to temp files and external merge sort. This
///   implementation works for regional extracts (Denmark, Germany, etc.).
#[allow(clippy::too_many_lines)]
pub fn build_geocode_index(config: &BuildConfig) -> Result<BuildStats> {
    let start_time = std::time::Instant::now();

    // Guard against silently overwriting an existing index
    let header_path = config.output_dir.join(FILE_HEADER);
    if header_path.exists() && !config.force {
        return Err(format!(
            "output directory {} already contains a geocode index. \
             Use --force to overwrite.",
            config.output_dir.display()
        )
        .into());
    }

    std::fs::create_dir_all(&config.output_dir)?;

    // Validate input
    crate::commands::require_indexdata(
        &config.input_path, false, config.force,
        "input PBF has no blob-level indexdata. Without indexdata, \
         every blob must be decompressed (significantly slower).",
    )?;

    // Read PBF header
    let reader = ElementReader::from_path(&config.input_path)?;
    let pbf_header = reader.header();
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let repl_seq = pbf_header.osmosis_replication_sequence_number().unwrap_or(0) as u32;
    #[allow(clippy::cast_sign_loss)]
    let repl_ts = pbf_header.osmosis_replication_timestamp().unwrap_or(0) as u64;
    drop(reader);

    let mut strings = StringPool::new();
    let mut addr_points: Vec<RawAddrPoint> = Vec::new();
    let mut street_ways: Vec<RawStreetWay> = Vec::new();
    let mut interp_ways: Vec<RawInterpWay> = Vec::new();

    // -----------------------------------------------------------------------
    // Pass 1: Nodes — address points + dense coordinate index
    // -----------------------------------------------------------------------
    eprintln!("Pass 1: Nodes...");
    let mut node_index = DenseMmapIndex::new(16_000_000_000, &config.output_dir)?;

    {
        let reader = ElementReader::from_path(&config.input_path)?;
        reader.with_blob_filter(BlobFilter::only_nodes()).for_each(|element| {
            if let Element::DenseNode(node) = element {
                let lat_e7 = node.decimicro_lat();
                let lon_e7 = node.decimicro_lon();
                node_index.set(node.id(), lat_e7, lon_e7);

                let mut hn: Option<&str> = None;
                let mut st: Option<&str> = None;
                let mut pc: Option<&str> = None;
                for (k, v) in node.tags() {
                    match k {
                        "addr:housenumber" => hn = Some(v),
                        "addr:street" => st = Some(v),
                        "addr:postcode" => pc = Some(v),
                        _ => {}
                    }
                }
                if let (Some(h), Some(s)) = (hn, st) {
                    addr_points.push(RawAddrPoint {
                        lat_e7, lon_e7,
                        housenumber_offset: strings.intern(h),
                        street_offset: strings.intern(s),
                        postcode_offset: pc.map_or(0, |p| strings.intern(p)),
                    });
                }
            }
        })?;
    }
    eprintln!("  {} address points from nodes", addr_points.len());

    // -----------------------------------------------------------------------
    // Pass 2: Ways — streets, building addresses, interpolation
    // -----------------------------------------------------------------------
    eprintln!("Pass 2: Ways...");

    {
        let reader = ElementReader::from_path(&config.input_path)?;
        reader.with_blob_filter(BlobFilter::only_ways()).for_each(|element| {
            if let Element::Way(way) = element {
                let coords: Vec<(i32, i32)> = way.refs()
                    .filter_map(|nid| node_index.get(nid))
                    .collect();
                if coords.is_empty() { return; }

                let mut highway: Option<&str> = None;
                let mut name: Option<&str> = None;
                let mut hn: Option<&str> = None;
                let mut addr_st: Option<&str> = None;
                let mut pc: Option<&str> = None;
                let mut building = false;
                let mut interp: Option<&str> = None;

                for (k, v) in way.tags() {
                    match k {
                        "highway" => highway = Some(v),
                        "name" => name = Some(v),
                        "addr:housenumber" => hn = Some(v),
                        "addr:street" => addr_st = Some(v),
                        "addr:postcode" => pc = Some(v),
                        "building" => building = true,
                        "addr:interpolation" => interp = Some(v),
                        _ => {}
                    }
                }

                // Interpolation ways
                if let (Some(itype_str), Some(st)) = (interp, addr_st) {
                    if coords.len() >= 2 {
                        let itype = match itype_str {
                            "even" => 1u8, "odd" => 2, _ => 0,
                        };
                        interp_ways.push(RawInterpWay {
                            street_offset: strings.intern(st),
                            interpolation_type: itype,
                            nodes: coords,
                            start_number: 0,
                            end_number: 0,
                        });
                    }
                    return;
                }

                // Building addresses (centroid)
                if building {
                    if let (Some(h), Some(s)) = (hn, addr_st) {
                        let (sum_lat, sum_lon) = coords.iter()
                            .fold((0i64, 0i64), |acc, &(lat, lon)| {
                                (acc.0 + i64::from(lat), acc.1 + i64::from(lon))
                            });
                        #[allow(clippy::cast_possible_wrap)]
                        let count = coords.len().max(1) as i64;
                        #[allow(clippy::cast_possible_truncation)]
                        let clat = (sum_lat / count) as i32;
                        #[allow(clippy::cast_possible_truncation)]
                        let clon = (sum_lon / count) as i32;
                        addr_points.push(RawAddrPoint {
                            lat_e7: clat, lon_e7: clon,
                            housenumber_offset: strings.intern(h),
                            street_offset: strings.intern(s),
                            postcode_offset: pc.map_or(0, |p| strings.intern(p)),
                        });
                    }
                }

                // Streets
                if let (Some(hw), Some(n)) = (highway, name) {
                    if coords.len() >= 2 && !EXCLUDED_HIGHWAYS.contains(&hw) {
                        street_ways.push(RawStreetWay {
                            name_offset: strings.intern(n),
                            nodes: coords,
                        });
                    }
                }
            }
        })?;
    }
    eprintln!("  {} streets, {} interp, {} total addr", street_ways.len(), interp_ways.len(), addr_points.len());

    // -----------------------------------------------------------------------
    // Pass 3: Relations — admin boundaries
    // -----------------------------------------------------------------------
    eprintln!("Pass 3: Admin boundaries...");

    let mut admin_relations: Vec<RawAdminRelation> = Vec::new();
    {
        let reader = ElementReader::from_path(&config.input_path)?;
        reader.with_blob_filter(BlobFilter::only_relations()).for_each(|element| {
            if let Element::Relation(rel) = element {
                let mut boundary: Option<&str> = None;
                let mut level_str: Option<&str> = None;
                let mut rel_name: Option<&str> = None;
                let mut cc: Option<&str> = None;
                let mut postal: Option<&str> = None;

                for (k, v) in rel.tags() {
                    match k {
                        "boundary" => boundary = Some(v),
                        "admin_level" => level_str = Some(v),
                        "name" => rel_name = Some(v),
                        "ISO3166-1:alpha2" => cc = Some(v),
                        "postal_code" => postal = Some(v),
                        _ => {}
                    }
                }

                let Some(b) = boundary else { return };
                let (is_admin, is_postal) = (b == "administrative", b == "postal_code");
                if !is_admin && !is_postal { return; }

                let admin_level = if is_admin {
                    let Some(ls) = level_str else { return };
                    let Ok(l) = ls.parse::<u8>() else { return };
                    if !(2..=10).contains(&l) { return; }
                    l
                } else { 11 };

                let name_str = if is_postal { postal.or(rel_name) } else { rel_name };
                let Some(ns) = name_str else { return };

                let name_offset = strings.intern(ns);
                let cc_offset = if admin_level == 2 { cc.map_or(0, |c| strings.intern(c)) } else { 0 };

                let mut outer = Vec::new();
                let mut inner = Vec::new();
                for m in rel.members() {
                    if let MemberId::Way(wid) = m.id {
                        let role = m.role().unwrap_or("");
                        if role == "inner" { inner.push(wid); }
                        else { outer.push(wid); }
                    }
                }

                admin_relations.push(RawAdminRelation {
                    admin_level, name_offset, country_code_offset: cc_offset,
                    outer_way_ids: outer, inner_way_ids: inner,
                });
            }
        })?;
    }
    eprintln!("  {} admin relations", admin_relations.len());

    // Scan 3b: resolve way geometries
    let mut needed = crate::commands::id_set_dense::IdSetDense::new();
    for r in &admin_relations {
        for &wid in &r.outer_way_ids { needed.set(wid); }
        for &wid in &r.inner_way_ids { needed.set(wid); }
    }

    let mut way_geom: HashMap<i64, Vec<(i32, i32)>> = HashMap::new();
    {
        let reader = ElementReader::from_path(&config.input_path)?;
        reader.with_blob_filter(BlobFilter::only_ways()).for_each(|element| {
            if let Element::Way(way) = element {
                if !needed.get(way.id()) { return; }
                let coords: Vec<(i32, i32)> = way.refs()
                    .filter_map(|nid| node_index.get(nid))
                    .collect();
                if !coords.is_empty() {
                    way_geom.insert(way.id(), coords);
                }
            }
        })?;
    }
    eprintln!("  {} way geometries resolved", way_geom.len());
    drop(needed);
    drop(node_index);

    // Ring assembly + simplification
    let admin_polygons = assemble_admin_polygons(&admin_relations, &way_geom, config);
    drop(way_geom);
    eprintln!("  {} admin polygons assembled", admin_polygons.len());

    // -----------------------------------------------------------------------
    // Interpolation endpoint resolution
    // -----------------------------------------------------------------------
    let resolved = resolve_interpolation_endpoints(
        &mut interp_ways, &addr_points, &strings, config.street_level,
    );
    eprintln!("  {resolved}/{} interpolation ways resolved", interp_ways.len());

    // -----------------------------------------------------------------------
    // Pass 4: S2 cell assignment + write index files
    // -----------------------------------------------------------------------
    eprintln!("Pass 4: S2 cells + write...");

    // Write data files
    write_street_data(&config.output_dir, &street_ways)?;
    write_addr_data(&config.output_dir, &addr_points)?;
    write_interp_data(&config.output_dir, &interp_ways)?;
    write_admin_data(&config.output_dir, &admin_polygons)?;
    std::fs::write(config.output_dir.join(FILE_STRINGS), &strings.data)?;

    // Compute S2 cell assignments
    let sl = config.street_level;
    let cl = config.coarse_level;

    let (fine_addr, coarse_addr) = assign_addr_cells(&addr_points, sl, cl);
    let street_node_lists: Vec<&[(i32, i32)]> = street_ways.iter().map(|w| w.nodes.as_slice()).collect();
    let (fine_street, coarse_street) = assign_seg_cells_generic(&street_node_lists, sl, cl);
    let interp_node_lists: Vec<&[(i32, i32)]> = interp_ways.iter().map(|w| w.nodes.as_slice()).collect();
    let (fine_interp, coarse_interp) = assign_seg_cells_generic(&interp_node_lists, sl, cl);
    let admin_cell_entries = assign_admin_cells(&admin_polygons, config.admin_level);

    eprintln!("  {} fine street, {} fine addr, {} admin cell entries",
        fine_street.len(), fine_addr.len(), admin_cell_entries.len());

    // Sort and write cell indices
    let mut fine_street = fine_street;
    let mut fine_addr = fine_addr;
    let mut fine_interp = fine_interp;
    let mut coarse_street = coarse_street;
    let mut coarse_addr = coarse_addr;
    let mut coarse_interp = coarse_interp;
    let mut admin_cell_entries = admin_cell_entries;

    let fine_count = write_merged_geo_index(
        &config.output_dir, FILE_GEO_CELLS, FILE_STREET_ENTRIES,
        FILE_ADDR_ENTRIES, FILE_INTERP_ENTRIES,
        &mut fine_street, &mut fine_addr, &mut fine_interp,
    )?;
    let coarse_count = write_merged_geo_index(
        &config.output_dir, FILE_COARSE_GEO_CELLS, FILE_COARSE_STREET_ENTRIES,
        FILE_COARSE_ADDR_ENTRIES, FILE_COARSE_INTERP_ENTRIES,
        &mut coarse_street, &mut coarse_addr, &mut coarse_interp,
    )?;
    let admin_count = write_admin_index(&config.output_dir, &mut admin_cell_entries)?;

    // Write header
    #[allow(clippy::cast_possible_truncation)]
    let header = Header {
        format_version: FORMAT_VERSION,
        street_cell_level: config.street_level,
        coarse_cell_level: config.coarse_level,
        admin_cell_level: config.admin_level,
        max_admin_vertices: config.max_admin_vertices,
        fine_search_radius_m: config.fine_search_radius_m,
        coarse_search_radius_m: config.coarse_search_radius_m,
        replication_sequence: repl_seq,
        replication_timestamp: repl_ts,
        addr_point_count: addr_points.len() as u32,
        street_way_count: street_ways.len() as u32,
        interp_way_count: interp_ways.len() as u32,
        admin_polygon_count: admin_polygons.len() as u32,
        geo_cell_count: fine_count,
        coarse_cell_count: coarse_count,
        admin_cell_count: admin_count,
    };
    std::fs::write(config.output_dir.join(FILE_HEADER), header.to_bytes())?;

    // Build-time smoke test: re-open with Reader and verify a query works
    #[cfg(feature = "geocode-reader")]
    {
        eprintln!("  Running smoke test...");
        let test_reader = super::reader::Reader::open(&config.output_dir)?;
        if !addr_points.is_empty() {
            let pt = &addr_points[0];
            let result = test_reader.query(pt.lat_e7 as f64 * 1e-7, pt.lon_e7 as f64 * 1e-7);
            if result.address.is_none() && result.street.is_none() {
                eprintln!("  WARNING: smoke test query returned no address or street match");
            }
        }
    }

    let elapsed = start_time.elapsed();
    eprintln!("Done in {:.1}s", elapsed.as_secs_f64());

    #[allow(clippy::cast_possible_truncation)]
    Ok(BuildStats {
        addr_points: addr_points.len() as u64,
        street_ways: street_ways.len() as u64,
        interp_ways: interp_ways.len() as u64,
        admin_polygons: admin_polygons.len() as u64,
        fine_cells: fine_count as u64,
        coarse_cells: coarse_count as u64,
        admin_cells: admin_count as u64,
    })
}

// ---------------------------------------------------------------------------
// Admin polygon assembly
// ---------------------------------------------------------------------------

fn assemble_admin_polygons(
    relations: &[RawAdminRelation],
    way_geom: &HashMap<i64, Vec<(i32, i32)>>,
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

// ---------------------------------------------------------------------------
// Interpolation endpoint resolution
// ---------------------------------------------------------------------------

/// Parse leading digits from a house number string (e.g., "42" from "42A").
fn parse_house_number(s: &str) -> u32 {
    let mut n = 0u32;
    for b in s.bytes() {
        if b.is_ascii_digit() {
            n = n.saturating_mul(10).saturating_add(u32::from(b - b'0'));
        } else {
            break;
        }
    }
    n
}

/// Resolve start/end house numbers for interpolation ways by matching
/// their endpoints against nearby address points with the same street name.
/// Returns the count of successfully resolved ways.
#[allow(clippy::cast_possible_truncation)]
fn resolve_interpolation_endpoints(
    interp_ways: &mut [RawInterpWay],
    addr_points: &[RawAddrPoint],
    strings: &StringPool,
    street_level: u8,
) -> u32 {
    if interp_ways.is_empty() || addr_points.is_empty() {
        return 0;
    }

    // Build spatial index: S2 cell at street_level -> list of addr point indices
    let mut cell_to_addrs: HashMap<u64, Vec<u32>> = HashMap::new();
    for (idx, pt) in addr_points.iter().enumerate() {
        let ll = LatLng::from_degrees(pt.lat_e7 as f64 * 1e-7, pt.lon_e7 as f64 * 1e-7);
        let cell = CellID::from(ll).parent(street_level as u64).0;
        cell_to_addrs.entry(cell).or_default().push(idx as u32);
    }

    let mut resolved = 0u32;

    for iw in interp_ways.iter_mut() {
        if iw.nodes.len() < 2 {
            continue;
        }

        let start_coord = iw.nodes[0];
        let end_coord = iw.nodes[iw.nodes.len() - 1];

        let start_hn = find_endpoint_house_number(
            start_coord, iw.street_offset, addr_points, strings, &cell_to_addrs, street_level,
        );
        let end_hn = find_endpoint_house_number(
            end_coord, iw.street_offset, addr_points, strings, &cell_to_addrs, street_level,
        );

        if let (Some(s), Some(e)) = (start_hn, end_hn) {
            iw.start_number = s;
            iw.end_number = e;
            resolved += 1;
        }
    }

    resolved
}

/// Find the house number of an address point near an interpolation endpoint.
/// Prefers coordinate-coincident points, falls back to nearest same-street point.
#[allow(clippy::cast_possible_truncation)]
fn find_endpoint_house_number(
    endpoint: (i32, i32),
    street_offset: u32,
    addr_points: &[RawAddrPoint],
    strings: &StringPool,
    cell_to_addrs: &HashMap<u64, Vec<u32>>,
    street_level: u8,
) -> Option<u32> {
    let (lat_e7, lon_e7) = endpoint;
    let ll = LatLng::from_degrees(lat_e7 as f64 * 1e-7, lon_e7 as f64 * 1e-7);
    let center = CellID::from(ll).parent(street_level as u64);

    // Search center cell + neighbors
    let mut best_idx: Option<u32> = None;
    let mut best_dist_sq = i64::MAX;
    let mut found_exact = false;

    let mut check_cell = |cell_id: u64| {
        let Some(indices) = cell_to_addrs.get(&cell_id) else { return };
        for &idx in indices {
            let pt = &addr_points[idx as usize];
            // Must be same street
            if pt.street_offset != street_offset {
                continue;
            }
            let dlat = (pt.lat_e7 - lat_e7) as i64;
            let dlon = (pt.lon_e7 - lon_e7) as i64;
            let dist_sq = dlat * dlat + dlon * dlon;

            // Coordinate-coincident (within 1 decimicrodegree ≈ 0.01m)
            let is_exact = dlat.abs() <= 1 && dlon.abs() <= 1;

            if is_exact && !found_exact {
                // First exact match beats any previous non-exact
                found_exact = true;
                best_idx = Some(idx);
                best_dist_sq = dist_sq;
            } else if is_exact || !found_exact {
                // Among same category (exact or non-exact), pick nearest
                if dist_sq < best_dist_sq {
                    best_idx = Some(idx);
                    best_dist_sq = dist_sq;
                }
            }
        }
    };

    check_cell(center.0);
    for n in center.all_neighbors(street_level as u64) {
        check_cell(n.0);
    }

    let idx = best_idx?;
    let pt = &addr_points[idx as usize];
    let hn_str = read_string_from_pool(strings, pt.housenumber_offset);
    let hn = parse_house_number(hn_str);
    if hn > 0 { Some(hn) } else { None }
}

/// Read a null-terminated string from the pool by offset.
fn read_string_from_pool(pool: &StringPool, offset: u32) -> &str {
    if offset == 0 {
        return "";
    }
    let start = offset as usize;
    if start >= pool.data.len() {
        return "";
    }
    let remaining = &pool.data[start..];
    let end = remaining.iter().position(|&b| b == 0).unwrap_or(remaining.len());
    std::str::from_utf8(&remaining[..end]).unwrap_or("")
}

// ---------------------------------------------------------------------------
// Data file writers
// ---------------------------------------------------------------------------

fn write_street_data(dir: &Path, ways: &[RawStreetWay]) -> Result<()> {
    let mut ways_out = BufWriter::new(std::fs::File::create(dir.join(FILE_STREET_WAYS))?);
    let mut nodes_out = BufWriter::new(std::fs::File::create(dir.join(FILE_STREET_NODES))?);
    let mut offset: u64 = 0;
    for w in ways {
        #[allow(clippy::cast_possible_truncation)]
        let rec = StreetWay {
            node_offset: offset,
            name_offset: w.name_offset,
            node_count: w.nodes.len().min(u16::MAX as usize) as u16,
        };
        ways_out.write_all(&rec.to_bytes())?;
        for &(lat, lon) in &w.nodes {
            nodes_out.write_all(&NodeCoord { lat_e7: lat, lon_e7: lon }.to_bytes())?;
        }
        offset += (w.nodes.len() * NODE_COORD_SIZE) as u64;
    }
    ways_out.flush()?;
    nodes_out.flush()?;
    Ok(())
}

fn write_addr_data(dir: &Path, points: &[RawAddrPoint]) -> Result<()> {
    let mut out = BufWriter::new(std::fs::File::create(dir.join(FILE_ADDR_POINTS))?);
    for pt in points {
        out.write_all(&AddrPoint {
            lat_e7: pt.lat_e7, lon_e7: pt.lon_e7,
            housenumber_offset: pt.housenumber_offset,
            street_offset: pt.street_offset,
            postcode_offset: pt.postcode_offset,
        }.to_bytes())?;
    }
    out.flush()?;
    Ok(())
}

fn write_interp_data(dir: &Path, ways: &[RawInterpWay]) -> Result<()> {
    let mut ways_out = BufWriter::new(std::fs::File::create(dir.join(FILE_INTERP_WAYS))?);
    let mut nodes_out = BufWriter::new(std::fs::File::create(dir.join(FILE_INTERP_NODES))?);
    let mut offset: u64 = 0;
    for iw in ways {
        #[allow(clippy::cast_possible_truncation)]
        let rec = InterpWay {
            node_offset: offset,
            street_offset: iw.street_offset,
            start_number: iw.start_number, end_number: iw.end_number,
            node_count: iw.nodes.len().min(u16::MAX as usize) as u16,
            interpolation_type: iw.interpolation_type,
        };
        ways_out.write_all(&rec.to_bytes())?;
        for &(lat, lon) in &iw.nodes {
            nodes_out.write_all(&NodeCoord { lat_e7: lat, lon_e7: lon }.to_bytes())?;
        }
        offset += (iw.nodes.len() * NODE_COORD_SIZE) as u64;
    }
    ways_out.flush()?;
    nodes_out.flush()?;
    Ok(())
}

fn write_admin_data(dir: &Path, polygons: &[AssembledPolygon]) -> Result<()> {
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

// ---------------------------------------------------------------------------
// S2 cell covering for line segments
// ---------------------------------------------------------------------------

/// Cover a line segment by sampling intermediate points to find all S2 cells
/// the segment passes through at the given level.
///
/// Walks the segment in steps smaller than the cell edge length, collecting
/// all unique cell IDs. This catches cells that the segment crosses diagonally
/// without having either endpoint inside them.
fn cover_segment(
    lat1_e7: i32, lon1_e7: i32,
    lat2_e7: i32, lon2_e7: i32,
    level: u8,
) -> Vec<u64> {
    let lat1 = lat1_e7 as f64 * 1e-7;
    let lon1 = lon1_e7 as f64 * 1e-7;
    let lat2 = lat2_e7 as f64 * 1e-7;
    let lon2 = lon2_e7 as f64 * 1e-7;

    let c1 = CellID::from(LatLng::from_degrees(lat1, lon1)).parent(level as u64);
    let c2 = CellID::from(LatLng::from_degrees(lat2, lon2)).parent(level as u64);

    if c1.0 == c2.0 {
        return vec![c1.0];
    }

    // Walk intermediate points. At level 17 (~77m), step ~30m to catch all crossings.
    // At level 14 (~620m), step ~250m. Use half the cell edge as step size.
    let dlat = lat2 - lat1;
    let dlon = lon2 - lon1;
    let seg_len_deg = ((dlat * 1e-7).powi(2) + (dlon * 1e-7).powi(2)).sqrt();

    // Approximate cell edge in degrees: ~180 / 2^(level/2) is a rough heuristic
    // More precise: level 17 ≈ 0.0007°, level 14 ≈ 0.006°, level 10 ≈ 0.01°
    let step_deg = match level {
        17 => 0.0003,
        14 => 0.003,
        10 => 0.005,
        _ => 0.001,
    };

    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let steps = ((seg_len_deg / step_deg).ceil() as usize).max(2);

    let mut cells = Vec::with_capacity(steps + 1);
    cells.push(c1.0);

    for i in 1..steps {
        let t = i as f64 / steps as f64;
        let lat = lat1 + t * (lat2 - lat1);
        let lon = lon1 + t * (lon2 - lon1);
        let c = CellID::from(LatLng::from_degrees(lat, lon)).parent(level as u64).0;
        if !cells.contains(&c) {
            cells.push(c);
        }
    }
    if !cells.contains(&c2.0) {
        cells.push(c2.0);
    }
    cells
}

// ---------------------------------------------------------------------------
// S2 cell assignment
// ---------------------------------------------------------------------------

#[allow(clippy::cast_possible_truncation)]
fn assign_addr_cells(
    points: &[RawAddrPoint], fine_level: u8, coarse_level: u8,
) -> (Vec<AddrCellEntry>, Vec<AddrCellEntry>) {
    let mut fine = Vec::with_capacity(points.len());
    let mut coarse = Vec::with_capacity(points.len());
    for (idx, pt) in points.iter().enumerate() {
        let ll = LatLng::from_degrees(pt.lat_e7 as f64 * 1e-7, pt.lon_e7 as f64 * 1e-7);
        let cell = CellID::from(ll);
        fine.push(AddrCellEntry { cell_id: cell.parent(fine_level as u64).0, addr_index: idx as u32 });
        coarse.push(AddrCellEntry { cell_id: cell.parent(coarse_level as u64).0, addr_index: idx as u32 });
    }
    (fine, coarse)
}

/// Assign segment cells for a generic set of ways (used for both streets and interp).
#[allow(clippy::cast_possible_truncation)]
fn assign_seg_cells_generic(
    node_lists: &[&[(i32, i32)]],
    fine_level: u8,
    coarse_level: u8,
) -> (Vec<SegCellEntry>, Vec<SegCellEntry>) {
    let mut fine = Vec::new();
    let mut coarse = Vec::new();
    for (way_idx, nodes) in node_lists.iter().enumerate() {
        for (seg_idx, pair) in nodes.windows(2).enumerate() {
            let (lat1, lon1) = pair[0];
            let (lat2, lon2) = pair[1];

            for cid in cover_segment(lat1, lon1, lat2, lon2, fine_level) {
                fine.push(SegCellEntry {
                    cell_id: cid,
                    way_index: way_idx as u32,
                    segment_index: seg_idx as u16,
                });
            }
            for cid in cover_segment(lat1, lon1, lat2, lon2, coarse_level) {
                coarse.push(SegCellEntry {
                    cell_id: cid,
                    way_index: way_idx as u32,
                    segment_index: seg_idx as u16,
                });
            }
        }
    }
    (fine, coarse)
}

#[allow(clippy::cast_possible_truncation)]
fn assign_admin_cells(polygons: &[AssembledPolygon], admin_level: u8) -> Vec<AdminCellEntry> {
    let mut entries = Vec::new();

    for (poly_idx, poly) in polygons.iter().enumerate() {
        // Parse vertices into rings (exterior + holes) separated by RING_SENTINEL
        let (ext_f64, hole_rings) = parse_polygon_rings(&poly.vertices);
        if ext_f64.len() < 3 { continue; }

        let hole_slices: Vec<&[(f64, f64)]> = hole_rings.iter().map(Vec::as_slice).collect();

        // Edge cells: cover each ring segment using cover_segment
        let mut edge_cells = std::collections::HashSet::new();
        for v in poly.vertices.windows(2) {
            if v[0] == RING_SENTINEL || v[1] == RING_SENTINEL { continue; }
            for cid in cover_segment(v[0].lat_e7, v[0].lon_e7, v[1].lat_e7, v[1].lon_e7, admin_level) {
                edge_cells.insert(cid);
            }
        }

        for &cid in &edge_cells {
            entries.push(AdminCellEntry { cell_id: cid, poly_index: poly_idx as u32, is_interior: false });
        }

        // Interior cells: flood-fill from centroid using point_in_polygon (with holes)
        let exterior_end = poly.vertices.iter()
            .position(|v| *v == RING_SENTINEL)
            .unwrap_or(poly.vertices.len());
        let exterior = &poly.vertices[..exterior_end];
        if exterior.len() < 3 { continue; }

        let (sum_lat, sum_lon, count) = exterior.iter()
            .fold((0i64, 0i64, 0i64), |(sl, sn, c), v| {
                (sl + v.lat_e7 as i64, sn + v.lon_e7 as i64, c + 1)
            });
        if count == 0 { continue; }
        let clat = sum_lat as f64 / count as f64 * 1e-7;
        let clon = sum_lon as f64 / count as f64 * 1e-7;

        // Centroid must be inside the polygon (exterior AND not in any hole)
        if !crate::geo::point_in_polygon(clon, clat, &ext_f64, &hole_slices) { continue; }

        let seed = CellID::from(LatLng::from_degrees(clat, clon)).parent(admin_level as u64);
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        visited.insert(seed.0);
        queue.push_back(seed);

        while let Some(cell) = queue.pop_front() {
            if edge_cells.contains(&cell.0) { continue; }

            let center_ll = s2::latlng::LatLng::from(cell);
            // Test with holes — cells inside enclaves are NOT interior
            if crate::geo::point_in_polygon(
                center_ll.lng.deg(), center_ll.lat.deg(), &ext_f64, &hole_slices,
            ) {
                entries.push(AdminCellEntry {
                    cell_id: cell.0, poly_index: poly_idx as u32, is_interior: true,
                });
                for n in cell.all_neighbors(admin_level as u64) {
                    if !visited.contains(&n.0) {
                        visited.insert(n.0);
                        queue.push_back(n);
                    }
                }
            }
        }
    }
    entries
}

/// Parse polygon vertices (with sentinel separators) into exterior + hole rings as f64.
#[allow(clippy::type_complexity)]
fn parse_polygon_rings(vertices: &[NodeCoord]) -> (Vec<(f64, f64)>, Vec<Vec<(f64, f64)>>) {
    let mut rings: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut current: Vec<(f64, f64)> = Vec::new();
    for v in vertices {
        if *v == RING_SENTINEL {
            if current.len() >= 3 {
                rings.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        } else {
            current.push((v.lon_e7 as f64 * 1e-7, v.lat_e7 as f64 * 1e-7));
        }
    }
    if current.len() >= 3 {
        rings.push(current);
    }
    let exterior = rings.first().cloned().unwrap_or_default();
    let holes = if rings.len() > 1 { rings[1..].to_vec() } else { Vec::new() };
    (exterior, holes)
}

// ---------------------------------------------------------------------------
// Cell index writers
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments, clippy::cast_possible_truncation)]
fn write_merged_geo_index(
    dir: &Path, cells_file: &str, street_file: &str, addr_file: &str, interp_file: &str,
    street: &mut [SegCellEntry], addr: &mut [AddrCellEntry], interp: &mut [SegCellEntry],
) -> Result<u32> {
    street.sort_unstable_by_key(|e| e.cell_id);
    addr.sort_unstable_by_key(|e| e.cell_id);
    interp.sort_unstable_by_key(|e| e.cell_id);

    let mut all_ids: Vec<u64> = Vec::new();
    for e in street.iter() { all_ids.push(e.cell_id); }
    for e in addr.iter() { all_ids.push(e.cell_id); }
    for e in interp.iter() { all_ids.push(e.cell_id); }
    all_ids.sort_unstable();
    all_ids.dedup();

    let street_off = write_seg_entries(&dir.join(street_file), &all_ids, street)?;
    let addr_off = write_u32_entries(&dir.join(addr_file), &all_ids, addr)?;
    let interp_off = write_seg_entries(&dir.join(interp_file), &all_ids, interp)?;

    let mut out = BufWriter::new(std::fs::File::create(dir.join(cells_file))?);
    for &cid in &all_ids {
        let gc = GeoCell {
            cell_id: cid,
            street_offset: street_off.get(&cid).copied().unwrap_or(NO_DATA_U64),
            addr_offset: addr_off.get(&cid).copied().unwrap_or(NO_DATA_U32),
            #[allow(clippy::cast_possible_truncation)]
            interp_offset: interp_off.get(&cid).copied().map_or(NO_DATA_U32, |v| v as u32),
        };
        out.write_all(&gc.to_bytes())?;
    }
    out.flush()?;
    Ok(all_ids.len() as u32)
}

fn write_seg_entries(
    path: &Path, cell_ids: &[u64], entries: &[SegCellEntry],
) -> Result<FxHashMap<u64, u64>> {
    let mut offsets = FxHashMap::default();
    let mut out = BufWriter::new(std::fs::File::create(path)?);
    let mut byte_off: u64 = 0;
    let mut i = 0;
    for &cid in cell_ids {
        let start = i;
        while i < entries.len() && entries[i].cell_id == cid { i += 1; }
        if start == i { continue; }
        offsets.insert(cid, byte_off);
        #[allow(clippy::cast_possible_truncation)]
        let count = (i - start).min(u16::MAX as usize) as u16;
        out.write_all(&count.to_le_bytes())?;
        byte_off += 2;
        for e in &entries[start..start + count as usize] {
            out.write_all(&SegmentRef { way_index: e.way_index, segment_index: e.segment_index }.to_bytes())?;
            byte_off += SEGMENT_REF_SIZE as u64;
        }
    }
    out.flush()?;
    Ok(offsets)
}

fn write_u32_entries(
    path: &Path, cell_ids: &[u64], entries: &[AddrCellEntry],
) -> Result<FxHashMap<u64, u32>> {
    let mut offsets = FxHashMap::default();
    let mut out = BufWriter::new(std::fs::File::create(path)?);
    let mut byte_off: u32 = 0;
    let mut i = 0;
    for &cid in cell_ids {
        let start = i;
        while i < entries.len() && entries[i].cell_id == cid { i += 1; }
        if start == i { continue; }
        offsets.insert(cid, byte_off);
        #[allow(clippy::cast_possible_truncation)]
        let count = (i - start).min(u16::MAX as usize) as u16;
        out.write_all(&count.to_le_bytes())?;
        byte_off += 2;
        for e in &entries[start..start + count as usize] {
            out.write_all(&e.addr_index.to_le_bytes())?;
            byte_off += 4;
        }
    }
    out.flush()?;
    Ok(offsets)
}

#[allow(clippy::cast_possible_truncation)]
fn write_admin_index(dir: &Path, entries: &mut [AdminCellEntry]) -> Result<u32> {
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

        let count = (i - start).min(u16::MAX as usize) as u16;
        entries_out.write_all(&count.to_le_bytes())?;
        byte_off += 2;
        for e in &entries[start..start + count as usize] {
            let val = if e.is_interior { e.poly_index | INTERIOR_FLAG } else { e.poly_index };
            entries_out.write_all(&val.to_le_bytes())?;
            byte_off += 4;
        }
    }
    cells_out.flush()?;
    entries_out.flush()?;
    Ok(cell_ids.len() as u32)
}
