//! Builder for the reverse geocoding index.
//!
//! Reads an OSM PBF file in multiple passes and writes the set of binary index
//! files described in `notes/reverse-geocoding-spec.md` section 4.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;
use s2::cellid::CellID;
use s2::latlng::LatLng;

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
    pub direct_io: bool,
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
            direct_io: false,
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

// Cell entry types
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
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
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
        &config.input_path, config.direct_io, config.force,
        "input PBF has no blob-level indexdata. Without indexdata, \
         every blob must be decompressed (significantly slower).",
    )?;

    // Read PBF header
    let reader = ElementReader::open(&config.input_path, config.direct_io)?;
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
    crate::debug::emit_marker("GEOCODE_PASS1_START");
    eprintln!("Pass 1: Relations...");
    #[cfg(feature = "hotpath")]
    let pass1_start = std::time::Instant::now();

    let mut admin_relations: Vec<RawAdminRelation> = Vec::new();
    {
        let reader = ElementReader::open(&config.input_path, config.direct_io)?;
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

    // -----------------------------------------------------------------------
    // Pass 1.5: Referenced node collection (planet-scale memory optimization)
    //
    // Scan way blobs to collect node IDs referenced by geocode-relevant ways
    // (streets, building addresses, interpolation, admin members). The dense
    // node index in pass 2 only populates entries for these nodes, reducing
    // page cache working set from ~83 GB (all 10.4B nodes) to ~16 GB (~2B
    // referenced). Same pattern as ALTW pass 0.
    // -----------------------------------------------------------------------
    crate::debug::emit_marker("GEOCODE_PASS1_END");
    crate::debug::emit_marker("GEOCODE_PASS1_5_START");
    eprintln!("Pass 1.5: Referenced node collection...");

    let mut referenced_nodes = crate::commands::id_set_dense::IdSetDense::new();
    {
        let (schedule, shared_file) = crate::commands::build_classify_schedule(
            &config.input_path, Some(crate::blob_index::ElemKind::Way),
        )?;

        crate::commands::parallel_classify_accumulate(
            &shared_file,
            &schedule,
            crate::commands::id_set_dense::IdSetDense::new,
            |block, node_ids| {
                for element in block.elements_skip_metadata() {
                    if let Element::Way(way) = element {
                        let mut highway = false;
                        let mut name = false;
                        let mut hn = false;
                        let mut addr_st = false;
                        let mut building = false;
                        let mut interp = false;
                        let mut highway_val: Option<&str> = None;

                        for (k, _v) in way.tags() {
                            match k {
                                "highway" => { highway = true; highway_val = Some(_v); }
                                "name" => name = true,
                                "addr:housenumber" => hn = true,
                                "addr:street" => addr_st = true,
                                "building" => building = true,
                                "addr:interpolation" => interp = true,
                                _ => {}
                            }
                        }

                        let is_street = highway && name
                            && !EXCLUDED_HIGHWAYS.contains(&highway_val.unwrap_or(""));
                        let is_building_addr = building && hn && addr_st;
                        let is_interp = interp && addr_st;
                        let is_admin = needed_admin_ways.get(way.id());

                        if is_street || is_building_addr || is_interp || is_admin {
                            for r in way.refs() { node_ids.set(r); }
                        }
                    }
                }
            },
            |worker_node_ids| {
                referenced_nodes.merge(worker_node_ids);
            },
        )?;
    }
    crate::debug::emit_marker("GEOCODE_PASS1_5_END");

    // -----------------------------------------------------------------------
    // Pass 2: Nodes + Ways (fused single scan)
    //
    // Sorted PBFs (Sort.Type_then_ID) guarantee all node blobs come before
    // way blobs. A single sequential scan processes nodes first (populating
    // the dense coordinate index for referenced nodes + address points), then
    // ways (streets, buildings, interpolation, admin geometry).
    // -----------------------------------------------------------------------
    crate::debug::emit_marker("GEOCODE_PASS2_START");
    eprintln!("Pass 2: Nodes + Ways...");
    #[cfg(feature = "hotpath")]
    let pass2_start = std::time::Instant::now();

    // Compact rank-indexed coord array. Instead of a 128 GB sparse DenseMmapIndex
    // (direct-addressed by node ID), allocate only referenced_count × 8 bytes and
    // index by rank. Writes are sequential (sorted PBF → monotonic ranks). Reads
    // during way processing have good locality (contiguous pages, not scattered).
    // Planet: ~16 GB contiguous vs ~83 GB scattered page cache.
    referenced_nodes.build_rank_index();
    let referenced_count = referenced_nodes.total_count();
    eprintln!("  {referenced_count} referenced nodes, compact index = {} MB",
        referenced_count * 8 / 1_000_000);
    #[allow(clippy::cast_possible_truncation)]
    let coord_array_len = referenced_count as usize * 8; // 8 bytes per (lat_e7: i32, lon_e7: i32)
    let mut coord_mmap = memmap2::MmapMut::map_anon(coord_array_len.max(1))?;
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
        // Sequential reader to avoid PrimitiveBlock cross-thread alloc/free
        // retention (25+ GB at Europe/planet scale). The fused node+way scan
        // needs full PrimitiveBlock (for tag access), so we can't use the
        // node-only scanner here. But sequential decode keeps all alloc/free
        // on one thread, bounding heap to ~1.6 GB.
        // See notes/cross-pipeline-optimization-plan.md.
        let mut blob_reader = crate::blob::BlobReader::open(&config.input_path, config.direct_io)?;
        blob_reader.set_parse_indexdata(true);
        blob_reader.next()
            .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
        let decompress_pool = crate::blob::DecompressPool::new();
        let mut st_scratch: Vec<(u32, u32)> = Vec::new();
        let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

        for blob_result in &mut blob_reader {
            let blob = blob_result?;
            if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
                continue;
            }
            if let Some(idx) = blob.index() {
                if matches!(idx.kind, crate::blob_index::ElemKind::Relation) {
                    continue;
                }
            }
            let decompressed = blob.decompress_pooled(&decompress_pool)?;
            let block = crate::block::PrimitiveBlock::new_with_scratch(decompressed, &mut st_scratch, &mut gr_scratch)?;
            for element in block.elements_skip_metadata() {
                match element {
                    Element::DenseNode(node) => {
                        let lat_e7 = node.decimicro_lat();
                        let lon_e7 = node.decimicro_lon();
                        if referenced_nodes.get(node.id()) {
                            #[allow(clippy::cast_possible_truncation)]
                            let r = referenced_nodes.rank(node.id()) as usize;
                            let off = r * 8;
                            coord_mmap[off..off + 4].copy_from_slice(&lat_e7.to_le_bytes());
                            coord_mmap[off + 4..off + 8].copy_from_slice(&lon_e7.to_le_bytes());
                        }

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
                            .filter_map(|nid| {
                                if !referenced_nodes.get(nid) { return None; }
                                #[allow(clippy::cast_possible_truncation)]
                                let r = referenced_nodes.rank(nid) as usize;
                                let off = r * 8;
                                let lat = i32::from_le_bytes(coord_mmap[off..off+4].try_into().ok()?);
                                let lon = i32::from_le_bytes(coord_mmap[off+4..off+8].try_into().ok()?);
                                if lat == 0 && lon == 0 { None } else { Some((lat, lon)) }
                            })
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
        } // for blob_result
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
    drop(needed_admin_ways);
    drop(referenced_nodes);
    drop(coord_mmap);

    // Mmap output files for coordinate access in cell assignment + interpolation
    // Mmap helper: handles empty files via anonymous read-only mmap (same as Reader).
    let mmap_file = |name: &str| -> Result<memmap2::Mmap> {
        let path = config.output_dir.join(name);
        let file = std::fs::File::open(&path)?;
        let len = file.metadata()?.len();
        if len == 0 {
            return Ok(
                memmap2::MmapOptions::new().map_anon()?.make_read_only()?
            );
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
    let pass2_ms = pass2_start.elapsed().as_millis();

    // -----------------------------------------------------------------------
    // Interpolation endpoint resolution (reads from mmap'd addr_points.bin)
    // -----------------------------------------------------------------------
    let resolved = resolve_interpolation_endpoints_mmap(
        &mut interp_ways, &addr_points_mmap, &interp_nodes_mmap, &strings, config.street_level,
    );
    eprintln!("  {resolved}/{} interpolation ways resolved", interp_ways.len());

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
    crate::debug::emit_marker("GEOCODE_PASS2_END");
    crate::debug::emit_marker("GEOCODE_PASS3_START");
    // Pass 3: Bucketed S2 cell assignment + write cell index files
    //
    // Instead of accumulating all cell entries in RAM (~19 GB at planet),
    // partition into 256 buckets by top 8 bits of cell_id. Write tagged
    // entries to temp bucket files, then process one bucket at a time.
    // -----------------------------------------------------------------------
    eprintln!("Pass 3: S2 cells + write (bucketed)...");
    #[cfg(feature = "hotpath")]
    let pass4_start = std::time::Instant::now();

    let sl = config.street_level;
    let cl = config.coarse_level;
    let interp_way_count = interp_ways.len();

    // Admin cells are small enough to stay in memory
    let admin_cell_entries = assign_admin_cells(&admin_polygons, config.admin_level);

    // Process fine and coarse levels via bucketed distribution
    let fine_count = bucketed_cell_assignment(
        &config.output_dir,
        FILE_GEO_CELLS, FILE_STREET_ENTRIES, FILE_ADDR_ENTRIES, FILE_INTERP_ENTRIES,
        &street_ways_mmap, &street_nodes_mmap, street_way_count,
        &addr_points_mmap, addr_point_count,
        &interp_ways, &interp_nodes_mmap,
        sl,
    )?;
    let coarse_count = bucketed_cell_assignment(
        &config.output_dir,
        FILE_COARSE_GEO_CELLS, FILE_COARSE_STREET_ENTRIES,
        FILE_COARSE_ADDR_ENTRIES, FILE_COARSE_INTERP_ENTRIES,
        &street_ways_mmap, &street_nodes_mmap, street_way_count,
        &addr_points_mmap, addr_point_count,
        &interp_ways, &interp_nodes_mmap,
        cl,
    )?;
    let admin_count = write_admin_index(&config.output_dir, &mut { admin_cell_entries })?;

    eprintln!("  {fine_count} fine cells, {coarse_count} coarse cells, {admin_count} admin cells");

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
    }
    crate::debug::emit_marker("GEOCODE_PASS3_END");

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

// ---------------------------------------------------------------------------
// Bucketed cell assignment
// ---------------------------------------------------------------------------

const NUM_BUCKETS: usize = 256;
const STREET_CHUNK: usize = 100_000;
const ADDR_CHUNK: usize = 500_000;

/// Tagged bucket record: 15 bytes on disk.
/// cell_id (8) + entry_type (1) + way_or_addr_index (4) + segment_index (2)
const BUCKET_RECORD_SIZE: usize = 15;
const ENTRY_TYPE_STREET: u8 = 0;
const ENTRY_TYPE_ADDR: u8 = 1;
const ENTRY_TYPE_INTERP: u8 = 2;

fn bucket_for_cell(cell_id: u64) -> usize {
    (cell_id >> 56) as usize
}

/// Ensure bucket writer exists, creating the file lazily.
fn ensure_bucket_writer(
    writers: &mut [Option<BufWriter<std::fs::File>>],
    bucket: usize,
    bucket_dir: &Path,
) -> Result<()> {
    if writers[bucket].is_none() {
        let path = bucket_dir.join(format!("{bucket:03}"));
        writers[bucket] = Some(BufWriter::new(std::fs::File::create(path)?));
    }
    Ok(())
}

fn write_bucket_record(w: &mut BufWriter<std::fs::File>, cell_id: u64, etype: u8, index: u32, segment: u16) -> std::io::Result<()> {
    let mut buf = [0u8; BUCKET_RECORD_SIZE];
    buf[0..8].copy_from_slice(&cell_id.to_le_bytes());
    buf[8] = etype;
    buf[9..13].copy_from_slice(&index.to_le_bytes());
    buf[13..15].copy_from_slice(&segment.to_le_bytes());
    w.write_all(&buf)
}

struct ParsedBucketEntry {
    cell_id: u64,
    etype: u8,
    index: u32,
    segment: u16,
}

fn parse_bucket_file(data: &[u8]) -> Vec<ParsedBucketEntry> {
    let count = data.len() / BUCKET_RECORD_SIZE;
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let off = i * BUCKET_RECORD_SIZE;
        let cell_id = u64::from_le_bytes([
            data[off], data[off+1], data[off+2], data[off+3],
            data[off+4], data[off+5], data[off+6], data[off+7],
        ]);
        let etype = data[off+8];
        let index = u32::from_le_bytes([data[off+9], data[off+10], data[off+11], data[off+12]]);
        let segment = u16::from_le_bytes([data[off+13], data[off+14]]);
        entries.push(ParsedBucketEntry { cell_id, etype, index, segment });
    }
    entries
}

/// Run bucketed cell assignment for one level (fine or coarse).
/// Returns the number of unique cells written.
#[allow(clippy::too_many_arguments, clippy::cast_possible_truncation, clippy::too_many_lines, clippy::cognitive_complexity)]
#[hotpath::measure]
fn bucketed_cell_assignment(
    output_dir: &Path,
    cells_file: &str,
    street_entries_file: &str,
    addr_entries_file: &str,
    interp_entries_file: &str,
    street_ways_mmap: &[u8],
    street_nodes_mmap: &[u8],
    street_way_count: u32,
    addr_points_mmap: &[u8],
    addr_point_count: u32,
    interp_ways: &[SlimInterpWay],
    interp_nodes_mmap: &[u8],
    level: u8,
) -> Result<u32> {
    use rayon::prelude::*;

    // Create temp directory for bucket files (remove first to avoid stale files
    // from a failed prior run contaminating this build).
    let bucket_dir = output_dir.join(format!(".buckets-level{level}"));
    if bucket_dir.exists() {
        std::fs::remove_dir_all(&bucket_dir)?;
    }
    std::fs::create_dir_all(&bucket_dir)?;

    // Open 256 bucket writers (lazy — most will be used)
    let mut bucket_writers: Vec<Option<BufWriter<std::fs::File>>> = (0..NUM_BUCKETS)
        .map(|_| None)
        .collect();

    // Stage A: Chunked parallel compute + single-threaded distribute

    // Streets
    let mut chunk_start = 0u32;
    while chunk_start < street_way_count {
        let chunk_end = (chunk_start + STREET_CHUNK as u32).min(street_way_count);
        let entries: Vec<(u64, u32, u16)> = (chunk_start..chunk_end)
            .into_par_iter()
            .flat_map_iter(|way_idx| {
                let offset = way_idx as usize * STREET_WAY_SIZE;
                let rec = street_ways_mmap.get(offset..offset + STREET_WAY_SIZE)
                    .and_then(|b| <&[u8; STREET_WAY_SIZE]>::try_from(b).ok())
                    .map(StreetWay::from_bytes);
                let mut out = Vec::new();
                if let Some(rec) = rec {
                    let nc = rec.node_count as usize;
                    if nc >= 2 {
                        for seg_idx in 0..nc - 1 {
                            let off1 = rec.node_offset as usize + seg_idx * NODE_COORD_SIZE;
                            let off2 = off1 + NODE_COORD_SIZE;
                            if let (Some(n1), Some(n2)) = (
                                read_node_at(street_nodes_mmap, off1 as u64),
                                read_node_at(street_nodes_mmap, off2 as u64),
                            ) {
                                cover_segment(n1.0, n1.1, n2.0, n2.1, level, |cid| {
                                    out.push((cid, way_idx, seg_idx as u16));
                                });
                            }
                        }
                    }
                }
                out.into_iter()
            })
            .collect();

        for &(cid, wi, si) in &entries {
            let b = bucket_for_cell(cid);
            ensure_bucket_writer(&mut bucket_writers, b, &bucket_dir)?;
            write_bucket_record(bucket_writers[b].as_mut().expect("ensured"), cid, ENTRY_TYPE_STREET, wi, si)?;
        }
        chunk_start = chunk_end;
    }

    // Address points
    let addr_count = addr_point_count as usize;
    let mut chunk_start = 0usize;
    while chunk_start < addr_count {
        let chunk_end = (chunk_start + ADDR_CHUNK).min(addr_count);
        for idx in chunk_start..chunk_end {
            if let Some(pt) = read_addr_point_mmap(addr_points_mmap, idx as u32) {
                let ll = LatLng::from_degrees(pt.lat_e7 as f64 * 1e-7, pt.lon_e7 as f64 * 1e-7);
                let cid = CellID::from(ll).parent(level as u64).0;
                let b = bucket_for_cell(cid);
                ensure_bucket_writer(&mut bucket_writers, b, &bucket_dir)?;
                write_bucket_record(bucket_writers[b].as_mut().expect("ensured"), cid, ENTRY_TYPE_ADDR, idx as u32, 0)?;
            }
        }
        chunk_start = chunk_end;
    }

    // Interpolation — collect per-way entries, then distribute with proper error propagation
    for (way_idx, iw) in interp_ways.iter().enumerate() {
        let nc = iw.node_count as usize;
        if nc < 2 { continue; }
        let mut way_entries: Vec<(u64, u32, u16)> = Vec::new();
        for seg_idx in 0..nc - 1 {
            let off1 = iw.node_file_offset as usize + seg_idx * NODE_COORD_SIZE;
            let off2 = off1 + NODE_COORD_SIZE;
            if let (Some(n1), Some(n2)) = (
                read_node_at(interp_nodes_mmap, off1 as u64),
                read_node_at(interp_nodes_mmap, off2 as u64),
            ) {
                cover_segment(n1.0, n1.1, n2.0, n2.1, level, |cid| {
                    way_entries.push((cid, way_idx as u32, seg_idx as u16));
                });
            }
        }
        for &(cid, wi, si) in &way_entries {
            let b = bucket_for_cell(cid);
            ensure_bucket_writer(&mut bucket_writers, b, &bucket_dir)?;
            write_bucket_record(bucket_writers[b].as_mut().expect("ensured"), cid, ENTRY_TYPE_INTERP, wi, si)?;
        }
    }

    // Flush and drop all bucket writers
    for writer in bucket_writers.iter_mut().flatten() {
        writer.flush()?;
    }
    drop(bucket_writers);

    // Stage B: Process buckets in order, write merged output

    let mut cells_out = BufWriter::new(std::fs::File::create(output_dir.join(cells_file))?);
    let mut street_out = BufWriter::new(std::fs::File::create(output_dir.join(street_entries_file))?);
    let mut addr_out = BufWriter::new(std::fs::File::create(output_dir.join(addr_entries_file))?);
    let mut interp_out = BufWriter::new(std::fs::File::create(output_dir.join(interp_entries_file))?);

    let mut street_byte_offset: u64 = 0;
    let mut addr_byte_offset: u32 = 0;
    let mut interp_byte_offset: u64 = 0;
    let mut total_cells: u32 = 0;
    let mut prev_cell_id: u64 = 0;

    for bucket_idx in 0..NUM_BUCKETS {
        let bucket_path = bucket_dir.join(format!("{bucket_idx:03}"));
        if !bucket_path.exists() { continue; }

        let data = std::fs::read(&bucket_path)?;
        std::fs::remove_file(&bucket_path)?;
        if data.is_empty() { continue; }

        let mut entries = parse_bucket_file(&data);
        drop(data); // free the raw bytes

        // Sort by cell_id
        entries.sort_unstable_by_key(|e| e.cell_id);

        // Group by cell_id and write merged output
        let mut i = 0;
        let mut streets: Vec<&ParsedBucketEntry> = Vec::new();
        let mut addrs: Vec<&ParsedBucketEntry> = Vec::new();
        let mut interps: Vec<&ParsedBucketEntry> = Vec::new();
        while i < entries.len() {
            let cell_id = entries[i].cell_id;
            let group_start = i;
            while i < entries.len() && entries[i].cell_id == cell_id {
                i += 1;
            }
            let group = &entries[group_start..i];

            // Partition into typed sub-groups
            streets.clear();
            addrs.clear();
            interps.clear();
            for e in group {
                match e.etype {
                    ENTRY_TYPE_STREET => streets.push(e),
                    ENTRY_TYPE_ADDR => addrs.push(e),
                    ENTRY_TYPE_INTERP => interps.push(e),
                    _ => {}
                }
            }

            // Write street entries
            let has_streets = !streets.is_empty();
            if has_streets {
                let count = streets.len().min(u16::MAX as usize) as u16;
                street_out.write_all(&count.to_le_bytes())?;
                for e in &streets[..count as usize] {
                    street_out.write_all(&SegmentRef {
                        way_index: e.index,
                        segment_index: e.segment,
                    }.to_bytes())?;
                }
            }

            // Write addr entries
            let has_addrs = !addrs.is_empty();
            if has_addrs {
                let count = addrs.len().min(u16::MAX as usize) as u16;
                addr_out.write_all(&count.to_le_bytes())?;
                for e in &addrs[..count as usize] {
                    addr_out.write_all(&e.index.to_le_bytes())?;
                }
            }

            // Write interp entries
            let has_interps = !interps.is_empty();
            if has_interps {
                let count = interps.len().min(u16::MAX as usize) as u16;
                interp_out.write_all(&count.to_le_bytes())?;
                for e in &interps[..count as usize] {
                    interp_out.write_all(&SegmentRef {
                        way_index: e.index,
                        segment_index: e.segment,
                    }.to_bytes())?;
                }
            }

            // Write geo cell record
            let gc = GeoCell {
                cell_id,
                street_offset: if has_streets { street_byte_offset } else { NO_DATA_U64 },
                addr_offset: if has_addrs { addr_byte_offset } else { NO_DATA_U32 },
                interp_offset: if has_interps {
                    #[allow(clippy::cast_possible_truncation)]
                    { interp_byte_offset as u32 }
                } else { NO_DATA_U32 },
            };
            cells_out.write_all(&gc.to_bytes())?;
            debug_assert!(
                cell_id > prev_cell_id || total_cells == 0,
                "bucket ordering violated: cell {cell_id} <= prev {prev_cell_id}"
            );
            prev_cell_id = cell_id;
            total_cells += 1;

            // Advance byte offsets
            if has_streets {
                let count = streets.len().min(u16::MAX as usize);
                street_byte_offset += 2 + (count * SEGMENT_REF_SIZE) as u64;
            }
            if has_addrs {
                let count = addrs.len().min(u16::MAX as usize);
                addr_byte_offset += 2 + (count * 4) as u32;
            }
            if has_interps {
                let count = interps.len().min(u16::MAX as usize);
                interp_byte_offset += 2 + (count * SEGMENT_REF_SIZE) as u64;
            }
        }
    }

    cells_out.flush()?;
    street_out.flush()?;
    addr_out.flush()?;
    interp_out.flush()?;

    // Clean up bucket directory
    std::fs::remove_dir_all(&bucket_dir).ok();

    Ok(total_cells)
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
