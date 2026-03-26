//! Builder for the reverse geocoding index.
//!
//! Reads an OSM PBF file in multiple passes and writes the set of binary index
//! files described in `notes/reverse-geocoding-spec.md` section 4.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;
use s2::cellid::CellID;
use s2::latlng::LatLng;

use crate::commands::add_locations_to_ways::DenseMmapIndex;
use crate::{BlobFilter, Element, ElementReader, MemberId};

use super::format::*;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Read current RSS in kilobytes from `/proc/self/statm`.
#[cfg(feature = "hotpath")]
fn read_rss_kb() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4) // pages × 4096 / 1024
}

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

/// Slim interpolation metadata kept in memory during the build.
/// Node coordinates are written directly to interp_nodes.bin;
/// this struct stores only the file offset and count.
struct SlimInterpWay {
    street_offset: u32,
    interpolation_type: u8,
    node_file_offset: u64,
    node_count: u16,
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
#[hotpath::measure]
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
    let mut interp_ways: Vec<SlimInterpWay> = Vec::new();

    // -----------------------------------------------------------------------
    // Pass 1: Relations — collect admin boundary metadata + way member IDs
    // (Runs first so we know which way IDs to collect in pass 3.)
    // -----------------------------------------------------------------------
    eprintln!("Pass 1: Relations...");
    #[cfg(feature = "hotpath")]
    let pass1_start = std::time::Instant::now();

    let mut admin_relations: Vec<RawAdminRelation> = Vec::new();
    {
        let reader = ElementReader::from_path(&config.input_path)?;
        reader.with_blob_filter(BlobFilter::only_relations())
            .for_each_block_pipelined(|block| {
            for element in block.elements_skip_metadata() {
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

                    let Some(b) = boundary else { continue };
                    let (is_admin, is_postal) = (b == "administrative", b == "postal_code");
                    if !is_admin && !is_postal { continue; }

                    let admin_level = if is_admin {
                        let Some(ls) = level_str else { continue };
                        let Ok(l) = ls.parse::<u8>() else { continue };
                        if !(2..=10).contains(&l) { continue; }
                        l
                    } else { 11 };

                    let name_str = if is_postal { postal.or(rel_name) } else { rel_name };
                    let Some(ns) = name_str else { continue };

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
            }
            Ok(())
        })?;
    }
    eprintln!("  {} admin relations", admin_relations.len());

    // Build set of way IDs needed for admin boundary geometry
    let mut needed_admin_ways = crate::commands::id_set_dense::IdSetDense::new();
    for r in &admin_relations {
        for &wid in &r.outer_way_ids { needed_admin_ways.set(wid); }
        for &wid in &r.inner_way_ids { needed_admin_ways.set(wid); }
    }

    #[cfg(feature = "hotpath")]
    let pass1_ms = pass1_start.elapsed().as_millis();
    #[cfg(feature = "hotpath")]
    if let Some(rss) = read_rss_kb() { eprintln!("  rss_after_pass1_kb={rss}"); }

    // -----------------------------------------------------------------------
    // Pass 2: Nodes + Ways (fused single scan, pipelined)
    //
    // Sorted PBFs (Sort.Type_then_ID) guarantee all node blobs come before
    // way blobs. A single pipelined scan processes nodes first (populating
    // the dense coordinate index + address points), then ways (streets,
    // buildings, interpolation, admin geometry). The pipelined closure runs
    // sequentially on the caller thread — no concurrent data structure changes
    // needed. Decompression is overlapped with I/O in the pipeline's rayon pool.
    // -----------------------------------------------------------------------
    eprintln!("Pass 2: Nodes + Ways (pipelined)...");
    #[cfg(feature = "hotpath")]
    let pass2_start = std::time::Instant::now();

    let mut node_index = DenseMmapIndex::new(16_000_000_000, &config.output_dir)?;
    let mut way_geom: rustc_hash::FxHashMap<i64, Vec<(i32, i32)>> = rustc_hash::FxHashMap::default();

    // Streaming output: write data files directly during the scan instead
    // of accumulating Vecs. Running counters track offsets and record counts.
    let mut street_ways_out = BufWriter::new(
        std::fs::File::create(config.output_dir.join(FILE_STREET_WAYS))?,
    );
    let mut street_nodes_out = BufWriter::new(
        std::fs::File::create(config.output_dir.join(FILE_STREET_NODES))?,
    );
    let mut addr_points_out = BufWriter::new(
        std::fs::File::create(config.output_dir.join(FILE_ADDR_POINTS))?,
    );
    let mut interp_nodes_out = BufWriter::new(
        std::fs::File::create(config.output_dir.join(FILE_INTERP_NODES))?,
    );

    let mut street_node_offset: u64 = 0;
    let mut interp_node_offset: u64 = 0;
    let mut addr_point_count: u32 = 0;
    let mut street_way_count: u32 = 0;
    // First address point lat/lon for the smoke test (since we won't have the Vec)
    let mut first_addr_lat_e7: i32 = 0;
    let mut first_addr_lon_e7: i32 = 0;

    {
        // Filter: nodes + ways, skip relations (already scanned in pass 1).
        // Block-level pipelining with elements_skip_metadata() — we don't
        // need version/timestamp/changeset/uid/user for any element.
        let reader = ElementReader::from_path(&config.input_path)?;
        reader.with_blob_filter(BlobFilter::new(true, true, false))
            .for_each_block_pipelined(|block| {
            for element in block.elements_skip_metadata() {
                match element {
                    Element::DenseNode(node) => {
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
                            // Stream directly to addr_points.bin
                            let ap = AddrPoint {
                                lat_e7, lon_e7,
                                housenumber_offset: strings.intern(h),
                                street_offset: strings.intern(s),
                                postcode_offset: pc.map_or(0, |p| strings.intern(p)),
                            };
                            addr_points_out.write_all(&ap.to_bytes())?;
                            if addr_point_count == 0 {
                                first_addr_lat_e7 = lat_e7;
                                first_addr_lon_e7 = lon_e7;
                            }
                            addr_point_count += 1;
                        }
                    }
                    Element::Way(way) => {
                        let way_id = way.id();
                        let is_admin_way = needed_admin_ways.get(way_id);

                        // Tag-first classification: check tags before resolving
                        // coordinates. Skips node-index lookups for irrelevant ways.
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

                        // Skip coordinate resolution for irrelevant ways
                        let is_street = highway.is_some() && name.is_some()
                            && !EXCLUDED_HIGHWAYS.contains(&highway.unwrap_or(""));
                        let is_building_addr = building && hn.is_some() && addr_st.is_some();
                        let is_interp = interp.is_some() && addr_st.is_some();

                        if !is_admin_way && !is_street && !is_building_addr && !is_interp {
                            continue;
                        }

                        let coords: Vec<(i32, i32)> = way.refs()
                            .filter_map(|nid| node_index.get(nid))
                            .collect();
                        if coords.is_empty() { continue; }

                        // Admin way geometry — move coords if no other consumer needs them
                        if is_admin_way && !is_street && !is_building_addr && !is_interp {
                            way_geom.insert(way_id, coords);
                            continue;
                        }
                        if is_admin_way {
                            way_geom.insert(way_id, coords.clone());
                        }

                        // Interpolation ways — write nodes to file, keep slim metadata
                        if is_interp {
                            if coords.len() >= 2 {
                                let itype = match interp.unwrap_or("") {
                                    "even" => 1u8, "odd" => 2, _ => 0,
                                };
                                #[allow(clippy::cast_possible_truncation)]
                                let nc = coords.len().min(u16::MAX as usize) as u16;
                                interp_ways.push(SlimInterpWay {
                                    street_offset: strings.intern(addr_st.unwrap_or("")),
                                    interpolation_type: itype,
                                    node_file_offset: interp_node_offset,
                                    node_count: nc,
                                    start_number: 0,
                                    end_number: 0,
                                });
                                for &(lat, lon) in &coords {
                                    interp_nodes_out.write_all(
                                        &NodeCoord { lat_e7: lat, lon_e7: lon }.to_bytes()
                                    )?;
                                }
                                interp_node_offset += (coords.len() * NODE_COORD_SIZE) as u64;
                            }
                            continue;
                        }

                        // Building addresses (centroid) — stream to addr_points.bin
                        if is_building_addr {
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
                            let ap = AddrPoint {
                                lat_e7: clat, lon_e7: clon,
                                housenumber_offset: strings.intern(hn.unwrap_or("")),
                                street_offset: strings.intern(addr_st.unwrap_or("")),
                                postcode_offset: pc.map_or(0, |p| strings.intern(p)),
                            };
                            addr_points_out.write_all(&ap.to_bytes())?;
                            if addr_point_count == 0 {
                                first_addr_lat_e7 = clat;
                                first_addr_lon_e7 = clon;
                            }
                            addr_point_count += 1;
                        }

                        // Streets — stream to street_ways.bin + street_nodes.bin
                        if is_street && coords.len() >= 2 {
                            #[allow(clippy::cast_possible_truncation)]
                            let nc = coords.len().min(u16::MAX as usize) as u16;
                            let sw = StreetWay {
                                node_offset: street_node_offset,
                                name_offset: strings.intern(name.unwrap_or("")),
                                node_count: nc,
                            };
                            street_ways_out.write_all(&sw.to_bytes())?;
                            for &(lat, lon) in &coords {
                                street_nodes_out.write_all(
                                    &NodeCoord { lat_e7: lat, lon_e7: lon }.to_bytes()
                                )?;
                            }
                            street_node_offset += (coords.len() * NODE_COORD_SIZE) as u64;
                            street_way_count += 1;
                        }
                    }
                    _ => {} // Node (non-dense) — rare, ignore
                }
            }
            Ok(())
        })?;
    }

    // Flush and drop writers before mmap
    street_ways_out.flush()?;
    street_nodes_out.flush()?;
    addr_points_out.flush()?;
    interp_nodes_out.flush()?;
    drop(street_ways_out);
    drop(street_nodes_out);
    drop(addr_points_out);
    drop(interp_nodes_out);

    eprintln!("  {} addr, {} streets, {} interp, {} admin way geoms",
        addr_point_count, street_way_count, interp_ways.len(), way_geom.len());
    #[cfg(feature = "hotpath")]
    if let Some(rss) = read_rss_kb() { eprintln!("  rss_after_pass2_scan_kb={rss}"); }
    drop(needed_admin_ways);
    drop(node_index);
    #[cfg(feature = "hotpath")]
    if let Some(rss) = read_rss_kb() { eprintln!("  rss_after_pass2_drop_kb={rss}"); }

    // Mmap output files for coordinate access in cell assignment + interpolation
    // Mmap helper: handles empty files via anonymous read-only mmap (same as Reader).
    let mmap_file = |name: &str| -> Result<memmap2::Mmap> {
        let path = config.output_dir.join(name);
        let file = std::fs::File::open(&path)?;
        let len = file.metadata()?.len();
        if len == 0 {
            return Ok(unsafe {
                memmap2::MmapOptions::new().map_anon()?.make_read_only()?
            });
        }
        Ok(unsafe { memmap2::Mmap::map(&file)? })
    };
    let street_ways_mmap = mmap_file(FILE_STREET_WAYS)?;
    let street_nodes_mmap = mmap_file(FILE_STREET_NODES)?;
    let addr_points_mmap = mmap_file(FILE_ADDR_POINTS)?;
    let interp_nodes_mmap = mmap_file(FILE_INTERP_NODES)?;

    // Ring assembly + simplification
    let admin_polygons = assemble_admin_polygons(&admin_relations, &way_geom, config);
    drop(way_geom);
    eprintln!("  {} admin polygons assembled", admin_polygons.len());
    #[cfg(feature = "hotpath")]
    if let Some(rss) = read_rss_kb() { eprintln!("  rss_after_assembly_kb={rss}"); }

    #[cfg(feature = "hotpath")]
    let pass2_ms = pass2_start.elapsed().as_millis();

    // -----------------------------------------------------------------------
    // Interpolation endpoint resolution (reads from mmap'd addr_points.bin)
    // -----------------------------------------------------------------------
    let resolved = resolve_interpolation_endpoints_mmap(
        &mut interp_ways, &addr_points_mmap, &interp_nodes_mmap, &strings, config.street_level,
    );
    eprintln!("  {resolved}/{} interpolation ways resolved", interp_ways.len());
    #[cfg(feature = "hotpath")]
    if let Some(rss) = read_rss_kb() { eprintln!("  rss_after_interp_kb={rss}"); }

    // Write interp_ways.bin now (after resolution has set start/end numbers)
    {
        let mut iw_out = BufWriter::new(
            std::fs::File::create(config.output_dir.join(FILE_INTERP_WAYS))?,
        );
        for iw in &interp_ways {
            let rec = InterpWay {
                node_offset: iw.node_file_offset,
                street_offset: iw.street_offset,
                start_number: iw.start_number,
                end_number: iw.end_number,
                node_count: iw.node_count,
                interpolation_type: iw.interpolation_type,
            };
            iw_out.write_all(&rec.to_bytes())?;
        }
        iw_out.flush()?;
    }

    // Write admin + strings data files
    write_admin_data(&config.output_dir, &admin_polygons)?;
    std::fs::write(config.output_dir.join(FILE_STRINGS), &strings.data)?;

    // -----------------------------------------------------------------------
    // Pass 3: S2 cell assignment + write cell index files
    // -----------------------------------------------------------------------
    eprintln!("Pass 3: S2 cells + write...");
    #[cfg(feature = "hotpath")]
    let pass4_start = std::time::Instant::now();

    let sl = config.street_level;
    let cl = config.coarse_level;

    // Cell assignment reads from mmap'd files
    let (fine_addr, coarse_addr) = assign_addr_cells_mmap(&addr_points_mmap, sl, cl);
    let (fine_street, coarse_street) = assign_seg_cells_mmap(
        &street_ways_mmap, &street_nodes_mmap, street_way_count, sl, cl,
    );
    let interp_way_count = interp_ways.len();
    let (fine_interp, coarse_interp) = assign_seg_cells_interp_slim(
        &interp_ways, &interp_nodes_mmap, sl, cl,
    );
    let admin_cell_entries = assign_admin_cells(&admin_polygons, config.admin_level);

    eprintln!("  {} fine street, {} fine addr, {} admin cell entries",
        fine_street.len(), fine_addr.len(), admin_cell_entries.len());
    #[cfg(feature = "hotpath")]
    if let Some(rss) = read_rss_kb() { eprintln!("  rss_after_cell_assign_kb={rss}"); }

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
        addr_point_count,
        street_way_count,
        interp_way_count: interp_way_count as u32,
        admin_polygon_count: admin_polygons.len() as u32,
        geo_cell_count: fine_count,
        coarse_cell_count: coarse_count,
        admin_cell_count: admin_count,
    };
    std::fs::write(config.output_dir.join(FILE_HEADER), header.to_bytes())?;

    // Build-time smoke test
    #[cfg(feature = "geocode-reader")]
    {
        eprintln!("  Running smoke test...");
        let test_reader = super::reader::Reader::open(&config.output_dir)?;
        if addr_point_count > 0 {
            let result = test_reader.query(
                first_addr_lat_e7 as f64 * 1e-7,
                first_addr_lon_e7 as f64 * 1e-7,
            );
            if result.address.is_none() && result.street.is_none() {
                eprintln!("  WARNING: smoke test query returned no address or street match");
            }
        }
    }

    #[cfg(feature = "hotpath")]
    let pass4_ms = pass4_start.elapsed().as_millis();

    let elapsed = start_time.elapsed();
    eprintln!("Done in {:.1}s", elapsed.as_secs_f64());

    #[cfg(feature = "hotpath")]
    {
        eprintln!("pass1_relations_ms={pass1_ms}");
        eprintln!("pass2_nodes_ways_ms={pass2_ms}");
        eprintln!("pass3_cells_ms={pass4_ms}");
        eprintln!("addr_points={addr_point_count}");
        eprintln!("street_ways={street_way_count}");
        eprintln!("interp_ways={interp_way_count}");
        eprintln!("admin_polygons={}", admin_polygons.len());
        eprintln!("strings_bytes={}", strings.data.len());
        eprintln!("strings_unique={}", strings.index.len());
        if let Some(rss_kb) = read_rss_kb() {
            eprintln!("peak_rss_kb={rss_kb}");
        }
    }

    Ok(BuildStats {
        addr_points: u64::from(addr_point_count),
        street_ways: u64::from(street_way_count),
        interp_ways: interp_way_count as u64,
        admin_polygons: admin_polygons.len() as u64,
        fine_cells: u64::from(fine_count),
        coarse_cells: u64::from(coarse_count),
        admin_cells: u64::from(admin_count),
    })
}

// ---------------------------------------------------------------------------
// Admin polygon assembly
// ---------------------------------------------------------------------------

#[hotpath::measure]
fn assemble_admin_polygons(
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

// ---------------------------------------------------------------------------
// Interpolation endpoint resolution (mmap-based)
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

/// Read an AddrPoint from the mmap'd addr_points.bin by index.
fn read_addr_point_mmap(mmap: &[u8], index: u32) -> Option<AddrPoint> {
    let offset = index as usize * ADDR_POINT_SIZE;
    let end = offset + ADDR_POINT_SIZE;
    if end > mmap.len() { return None; }
    Some(AddrPoint::from_bytes(mmap[offset..end].try_into().ok()?))
}

/// Read a NodeCoord from a node mmap by byte offset.
#[allow(clippy::cast_possible_truncation)] // u64→usize: Linux 64-bit only
fn read_node_at(mmap: &[u8], byte_offset: u64) -> Option<(i32, i32)> {
    let off = byte_offset as usize;
    let end = off + NODE_COORD_SIZE;
    if end > mmap.len() { return None; }
    let nc = NodeCoord::from_bytes(mmap[off..end].try_into().ok()?);
    Some((nc.lat_e7, nc.lon_e7))
}

/// Resolve start/end house numbers for interpolation ways by matching
/// their endpoints against nearby address points with the same street name.
/// Reads address points from mmap'd addr_points.bin.
#[allow(clippy::cast_possible_truncation)]
#[hotpath::measure]
fn resolve_interpolation_endpoints_mmap(
    interp_ways: &mut [SlimInterpWay],
    addr_mmap: &[u8],
    interp_nodes_mmap: &[u8],
    strings: &StringPool,
    street_level: u8,
) -> u32 {
    let addr_count = addr_mmap.len() / ADDR_POINT_SIZE;
    if interp_ways.is_empty() || addr_count == 0 {
        return 0;
    }

    // Build spatial index: S2 cell -> list of addr point indices
    let mut cell_to_addrs: FxHashMap<u64, Vec<u32>> = FxHashMap::default();
    for idx in 0..addr_count {
        if let Some(pt) = read_addr_point_mmap(addr_mmap, idx as u32) {
            let ll = LatLng::from_degrees(pt.lat_e7 as f64 * 1e-7, pt.lon_e7 as f64 * 1e-7);
            let cell = CellID::from(ll).parent(street_level as u64).0;
            cell_to_addrs.entry(cell).or_default().push(idx as u32);
        }
    }

    let mut resolved = 0u32;

    for iw in interp_ways.iter_mut() {
        if iw.node_count < 2 { continue; }

        let Some(start_coord) = read_node_at(interp_nodes_mmap, iw.node_file_offset) else {
            continue;
        };
        let last_offset = iw.node_file_offset + (iw.node_count as u64 - 1) * NODE_COORD_SIZE as u64;
        let Some(end_coord) = read_node_at(interp_nodes_mmap, last_offset) else {
            continue;
        };

        let start_hn = find_endpoint_house_number_mmap(
            start_coord, iw.street_offset, addr_mmap, strings, &cell_to_addrs, street_level,
        );
        let end_hn = find_endpoint_house_number_mmap(
            end_coord, iw.street_offset, addr_mmap, strings, &cell_to_addrs, street_level,
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
#[allow(clippy::cast_possible_truncation)]
fn find_endpoint_house_number_mmap(
    endpoint: (i32, i32),
    street_offset: u32,
    addr_mmap: &[u8],
    strings: &StringPool,
    cell_to_addrs: &FxHashMap<u64, Vec<u32>>,
    street_level: u8,
) -> Option<u32> {
    let (lat_e7, lon_e7) = endpoint;
    let ll = LatLng::from_degrees(lat_e7 as f64 * 1e-7, lon_e7 as f64 * 1e-7);
    let center = CellID::from(ll).parent(street_level as u64);

    let mut best_idx: Option<u32> = None;
    let mut best_dist_sq = i64::MAX;
    let mut found_exact = false;

    let mut check_cell = |cell_id: u64| {
        let Some(indices) = cell_to_addrs.get(&cell_id) else { return };
        for &idx in indices {
            let Some(pt) = read_addr_point_mmap(addr_mmap, idx) else { continue };
            if pt.street_offset != street_offset { continue; }
            let dlat = (pt.lat_e7 - lat_e7) as i64;
            let dlon = (pt.lon_e7 - lon_e7) as i64;
            let dist_sq = dlat * dlat + dlon * dlon;
            let is_exact = dlat.abs() <= 1 && dlon.abs() <= 1;

            if is_exact && !found_exact {
                found_exact = true;
                best_idx = Some(idx);
                best_dist_sq = dist_sq;
            } else if (is_exact || !found_exact) && dist_sq < best_dist_sq {
                best_idx = Some(idx);
                best_dist_sq = dist_sq;
            }
        }
    };

    check_cell(center.0);
    for n in center.all_neighbors(street_level as u64) {
        check_cell(n.0);
    }

    let idx = best_idx?;
    let pt = read_addr_point_mmap(addr_mmap, idx)?;
    let hn_str = read_string_from_pool(strings, pt.housenumber_offset);
    let hn = parse_house_number(hn_str);
    if hn > 0 { Some(hn) } else { None }
}

/// Read a null-terminated string from the pool by offset.
fn read_string_from_pool(pool: &StringPool, offset: u32) -> &str {
    if offset == 0 { return ""; }
    let start = offset as usize;
    if start >= pool.data.len() { return ""; }
    let remaining = &pool.data[start..];
    let end = remaining.iter().position(|&b| b == 0).unwrap_or(remaining.len());
    std::str::from_utf8(&remaining[..end]).unwrap_or("")
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
/// Calls `emit(cell_id)` for each unique cell the segment crosses. No heap
/// allocation — uses a small stack buffer for deduplication (most segments
/// cross 1–4 cells).
fn cover_segment(
    lat1_e7: i32, lon1_e7: i32,
    lat2_e7: i32, lon2_e7: i32,
    level: u8,
    mut emit: impl FnMut(u64),
) {
    let lat1 = lat1_e7 as f64 * 1e-7;
    let lon1 = lon1_e7 as f64 * 1e-7;
    let lat2 = lat2_e7 as f64 * 1e-7;
    let lon2 = lon2_e7 as f64 * 1e-7;

    let c1 = CellID::from(LatLng::from_degrees(lat1, lon1)).parent(level as u64);
    let c2 = CellID::from(LatLng::from_degrees(lat2, lon2)).parent(level as u64);

    emit(c1.0);
    if c1.0 == c2.0 {
        return;
    }

    // Walk intermediate points. Step size < half cell edge to catch crossings.
    let dlat = lat2 - lat1;
    let dlon = lon2 - lon1;
    let seg_len_deg = ((dlat * 1e-7).powi(2) + (dlon * 1e-7).powi(2)).sqrt();

    let step_deg = match level {
        17 => 0.0003,
        14 => 0.003,
        10 => 0.005,
        _ => 0.001,
    };

    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let steps = ((seg_len_deg / step_deg).ceil() as usize).max(2);

    // Stack-based dedup for the common case (1–8 cells per segment)
    let mut seen = [0u64; 8];
    seen[0] = c1.0;
    let mut seen_count = 1usize;

    for i in 1..steps {
        let t = i as f64 / steps as f64;
        let lat = lat1 + t * (lat2 - lat1);
        let lon = lon1 + t * (lon2 - lon1);
        let c = CellID::from(LatLng::from_degrees(lat, lon)).parent(level as u64).0;
        let already = seen[..seen_count].contains(&c);
        if !already {
            emit(c);
            if seen_count < seen.len() {
                seen[seen_count] = c;
                seen_count += 1;
            }
        }
    }
    if !seen[..seen_count].contains(&c2.0) {
        emit(c2.0);
    }
}

// ---------------------------------------------------------------------------
// S2 cell assignment
// ---------------------------------------------------------------------------

/// Assign address point cells from mmap'd addr_points.bin.
#[allow(clippy::cast_possible_truncation)]
fn assign_addr_cells_mmap(
    addr_mmap: &[u8], fine_level: u8, coarse_level: u8,
) -> (Vec<AddrCellEntry>, Vec<AddrCellEntry>) {
    let count = addr_mmap.len() / ADDR_POINT_SIZE;
    let mut fine = Vec::with_capacity(count);
    let mut coarse = Vec::with_capacity(count);
    for idx in 0..count {
        if let Some(pt) = read_addr_point_mmap(addr_mmap, idx as u32) {
            let ll = LatLng::from_degrees(pt.lat_e7 as f64 * 1e-7, pt.lon_e7 as f64 * 1e-7);
            let cell = CellID::from(ll);
            fine.push(AddrCellEntry { cell_id: cell.parent(fine_level as u64).0, addr_index: idx as u32 });
            coarse.push(AddrCellEntry { cell_id: cell.parent(coarse_level as u64).0, addr_index: idx as u32 });
        }
    }
    (fine, coarse)
}

/// Assign street segment cells from mmap'd street_ways.bin + street_nodes.bin.
#[allow(clippy::cast_possible_truncation)]
#[hotpath::measure]
fn assign_seg_cells_mmap(
    ways_mmap: &[u8],
    nodes_mmap: &[u8],
    way_count: u32,
    fine_level: u8,
    coarse_level: u8,
) -> (Vec<SegCellEntry>, Vec<SegCellEntry>) {
    use rayon::prelude::*;

    let per_way: Vec<(Vec<SegCellEntry>, Vec<SegCellEntry>)> = (0..way_count)
        .into_par_iter()
        .map(|way_idx| {
            let offset = way_idx as usize * STREET_WAY_SIZE;
            let Some(rec) = ways_mmap.get(offset..offset + STREET_WAY_SIZE)
                .and_then(|b| <&[u8; STREET_WAY_SIZE]>::try_from(b).ok())
                .map(StreetWay::from_bytes) else {
                return (Vec::new(), Vec::new());
            };

            let mut fine = Vec::new();
            let mut coarse = Vec::new();
            let nc = rec.node_count as usize;
            if nc < 2 { return (fine, coarse); }

            for seg_idx in 0..nc - 1 {
                let off1 = rec.node_offset as usize + seg_idx * NODE_COORD_SIZE;
                let off2 = off1 + NODE_COORD_SIZE;
                let (Some(n1), Some(n2)) = (
                    read_node_at(nodes_mmap, off1 as u64),
                    read_node_at(nodes_mmap, off2 as u64),
                ) else { continue };

                let wi = way_idx;
                let si = seg_idx as u16;
                cover_segment(n1.0, n1.1, n2.0, n2.1, fine_level, |cid| {
                    fine.push(SegCellEntry { cell_id: cid, way_index: wi, segment_index: si });
                });
                cover_segment(n1.0, n1.1, n2.0, n2.1, coarse_level, |cid| {
                    coarse.push(SegCellEntry { cell_id: cid, way_index: wi, segment_index: si });
                });
            }
            (fine, coarse)
        })
        .collect();

    let total_fine: usize = per_way.iter().map(|(f, _)| f.len()).sum();
    let total_coarse: usize = per_way.iter().map(|(_, c)| c.len()).sum();
    let mut fine = Vec::with_capacity(total_fine);
    let mut coarse = Vec::with_capacity(total_coarse);
    for (f, c) in per_way {
        fine.extend(f);
        coarse.extend(c);
    }
    (fine, coarse)
}

/// Assign interpolation segment cells from slim metadata + mmap'd interp_nodes.
#[allow(clippy::cast_possible_truncation)]
#[hotpath::measure]
fn assign_seg_cells_interp_slim(
    interp_ways: &[SlimInterpWay],
    nodes_mmap: &[u8],
    fine_level: u8,
    coarse_level: u8,
) -> (Vec<SegCellEntry>, Vec<SegCellEntry>) {
    use rayon::prelude::*;

    let per_way: Vec<(Vec<SegCellEntry>, Vec<SegCellEntry>)> = interp_ways
        .par_iter()
        .enumerate()
        .map(|(way_idx, iw)| {
            let mut fine = Vec::new();
            let mut coarse = Vec::new();
            let nc = iw.node_count as usize;
            if nc < 2 { return (fine, coarse); }

            for seg_idx in 0..nc - 1 {
                let off1 = iw.node_file_offset as usize + seg_idx * NODE_COORD_SIZE;
                let off2 = off1 + NODE_COORD_SIZE;
                let (Some(n1), Some(n2)) = (
                    read_node_at(nodes_mmap, off1 as u64),
                    read_node_at(nodes_mmap, off2 as u64),
                ) else { continue };

                let wi = way_idx as u32;
                let si = seg_idx as u16;
                cover_segment(n1.0, n1.1, n2.0, n2.1, fine_level, |cid| {
                    fine.push(SegCellEntry { cell_id: cid, way_index: wi, segment_index: si });
                });
                cover_segment(n1.0, n1.1, n2.0, n2.1, coarse_level, |cid| {
                    coarse.push(SegCellEntry { cell_id: cid, way_index: wi, segment_index: si });
                });
            }
            (fine, coarse)
        })
        .collect();

    let total_fine: usize = per_way.iter().map(|(f, _)| f.len()).sum();
    let total_coarse: usize = per_way.iter().map(|(_, c)| c.len()).sum();
    let mut fine = Vec::with_capacity(total_fine);
    let mut coarse = Vec::with_capacity(total_coarse);
    for (f, c) in per_way {
        fine.extend(f);
        coarse.extend(c);
    }
    (fine, coarse)
}

#[allow(clippy::cast_possible_truncation)]
#[hotpath::measure]
fn assign_admin_cells(polygons: &[AssembledPolygon], admin_level: u8) -> Vec<AdminCellEntry> {
    let mut entries = Vec::new();

    for (poly_idx, poly) in polygons.iter().enumerate() {
        // Parse vertices into rings (exterior + holes) separated by RING_SENTINEL
        let (ext_f64, hole_rings) = parse_polygon_rings(&poly.vertices);
        if ext_f64.len() < 3 { continue; }

        let hole_slices: Vec<&[(f64, f64)]> = hole_rings.iter().map(Vec::as_slice).collect();

        // Edge cells: cover each ring segment using cover_segment
        let mut edge_cells = rustc_hash::FxHashSet::default();
        for v in poly.vertices.windows(2) {
            if v[0] == RING_SENTINEL || v[1] == RING_SENTINEL { continue; }
            cover_segment(v[0].lat_e7, v[0].lon_e7, v[1].lat_e7, v[1].lon_e7, admin_level, |cid| {
                edge_cells.insert(cid);
            });
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
        let mut visited = rustc_hash::FxHashSet::default();
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

#[hotpath::measure]
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
