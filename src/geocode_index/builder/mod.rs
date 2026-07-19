//! Builder for the reverse geocoding index.
//!
//! Reads an OSM PBF file in multiple passes and writes the set of binary index
//! files defined by the `format` module.

use std::io::{BufWriter, Write};
use std::path::PathBuf;

use crate::ElementReader;

use super::format::*;

mod admin;
mod interp;
mod pass1;
mod pass1_5;
mod pass2;
mod pass3;
mod strings;

// Under the `test-hooks` feature, expose Pass 3 Stage A fault-
// injection hooks so integration tests can arm them. The rest of
// `pass3` stays crate-private.
#[cfg(feature = "test-hooks")]
pub mod pass3_test_hooks {
    pub use super::pass3::test_hooks::{PANIC_AT_STREETS_WAY_IDX, reset};
}

use strings::StringPool;

pub(super) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

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
    // Cap glibc arenas at 2 to prevent cross-thread free fragmentation in
    // the Pass 2a pread worker pool. Without this, PrimitiveBlock Vec<u8>s
    // allocated on decode workers and freed on the main-thread merge path
    // cause arena accumulation growing to 25+ GB anon RSS at planet scale
    // (documented rationale for the previous sequential-Pass-2 choice).
    // Scoped to this command; other pbfhogg paths unaffected. Same prelude
    // used by renumber_external / check_refs / verify_ids for the same
    // reason.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }

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
        &config.input_path,
        config.direct_io,
        config.force,
        "input PBF has no blob-level indexdata. Without indexdata, \
         every blob must be decompressed (significantly slower).",
    )?;

    // Read PBF header
    let reader = ElementReader::open(&config.input_path, config.direct_io)?;
    let pbf_header = reader.header();
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let repl_seq = pbf_header
        .osmosis_replication_sequence_number()
        .unwrap_or(0) as u32;
    #[allow(clippy::cast_sign_loss)]
    let repl_ts = pbf_header.osmosis_replication_timestamp().unwrap_or(0) as u64;
    drop(reader);

    let mut strings = StringPool::new();

    // -----------------------------------------------------------------------
    // Pass 1: Relations - collect admin boundary metadata + way member IDs
    // (Runs first so we know which way IDs to collect in pass 3.)
    // -----------------------------------------------------------------------
    crate::debug::emit_marker("GEOCODE_PASS1_START");
    eprintln!("Pass 1: Relations...");
    #[cfg(feature = "hotpath")]
    let pass1_start = std::time::Instant::now();

    let (admin_relations, needed_admin_ways) =
        pass1::run_pass1(&config.input_path, config.direct_io, &mut strings)?;
    eprintln!("  {} admin relations", admin_relations.len());

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

    // -----------------------------------------------------------------------
    // One header walk produces everything Pass 1.5 and Pass 2a need: the
    // way schedule (Pass 1.5), the node schedule (Pass 2a), the max node
    // ID from indexdata (Pass 1.5's `IdSet::pre_allocate`), and a
    // single shared file handle reused by both phases' pread workers.
    // Previously two separate header walks: Pass 1.5's own walker and
    // Pass 2a's `build_classify_schedule(Node)` call. Consolidated walker
    // saves ~26 s at Europe / ~80 s at planet (2026-04-18 bench `bf8f2038`).
    // -----------------------------------------------------------------------
    crate::debug::emit_marker("GEOCODE_SCHEDULES_START");
    let (node_schedule, way_schedule, max_node_id, shared_file) =
        pass1_5::build_pass2_schedules(&config.input_path)?;
    crate::debug::emit_marker("GEOCODE_SCHEDULES_END");

    crate::debug::emit_marker("GEOCODE_PASS1_5_START");
    eprintln!("Pass 1.5: Referenced node collection...");

    let referenced_nodes =
        pass1_5::run_pass1_5(&way_schedule, max_node_id, &shared_file, &needed_admin_ways)?;
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

    crate::debug::emit_marker("GEOCODE_PASS2_SCAN_START");
    let pass2::Pass2Output {
        addr_point_count,
        street_way_count,
        first_addr_lat_e7,
        first_addr_lon_e7,
        mut interp_ways,
        way_geom,
        street_ways_mmap,
        street_nodes_mmap,
        addr_points_mmap,
        interp_nodes_mmap,
    } = pass2::run_pass2(
        config,
        &node_schedule,
        &way_schedule,
        &shared_file,
        needed_admin_ways,
        referenced_nodes,
        &mut strings,
    )?;
    drop(node_schedule);
    drop(way_schedule);
    drop(shared_file);
    crate::debug::emit_marker("GEOCODE_PASS2_SCAN_END");

    // Ring assembly + simplification
    crate::debug::emit_marker("GEOCODE_PASS2_ADMIN_ASSEMBLY_START");
    let admin_polygons = admin::assemble_admin_polygons(&admin_relations, &way_geom, config);
    drop(way_geom);
    eprintln!("  {} admin polygons assembled", admin_polygons.len());
    crate::debug::emit_marker("GEOCODE_PASS2_ADMIN_ASSEMBLY_END");

    #[cfg(feature = "hotpath")]
    let pass2_ms = pass2_start.elapsed().as_millis();

    // -----------------------------------------------------------------------
    // Interpolation endpoint resolution (reads from mmap'd addr_points.bin)
    // -----------------------------------------------------------------------
    crate::debug::emit_marker("GEOCODE_PASS2_INTERP_RESOLVE_START");
    let resolved = interp::resolve_interpolation_endpoints_mmap(
        &mut interp_ways,
        &addr_points_mmap,
        &interp_nodes_mmap,
        &strings,
        config.street_level,
    );
    eprintln!(
        "  {resolved}/{} interpolation ways resolved",
        interp_ways.len()
    );
    crate::debug::emit_marker("GEOCODE_PASS2_INTERP_RESOLVE_END");

    // Write interp_ways.bin now (after resolution has set start/end numbers)
    crate::debug::emit_marker("GEOCODE_PASS2_WRITE_START");
    {
        let mut iw_out = BufWriter::new(std::fs::File::create(
            config.output_dir.join(FILE_INTERP_WAYS),
        )?);
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
    admin::write_admin_data(&config.output_dir, &admin_polygons)?;
    std::fs::write(config.output_dir.join(FILE_STRINGS), &strings.data)?;
    crate::debug::emit_marker("GEOCODE_PASS2_WRITE_END");

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
    crate::debug::emit_marker("GEOCODE_PASS3_ADMIN_CELLS_START");
    let admin_cell_entries = pass3::assign_admin_cells(&admin_polygons, config.admin_level);
    crate::debug::emit_marker("GEOCODE_PASS3_ADMIN_CELLS_END");

    // Fused fine + coarse cell assignment (plan item #4). Single Stage A
    // pass over streets/addrs/interps at the fine level derives coarse
    // cells on the fly via S2 parent + per-segment dedup, eliminating
    // the duplicate `cover_segment` pass that ran separately at coarse
    // level. Two Stage B invocations follow - one per bucket tree.
    crate::debug::emit_marker("GEOCODE_PASS3_CELLS_START");
    let (fine_count, coarse_count) =
        pass3::bucketed_cell_assignment_fused(&pass3::FusedCellAssignmentParams {
            output_dir: &config.output_dir,
            street_ways_mmap: &street_ways_mmap,
            street_nodes_mmap: &street_nodes_mmap,
            street_way_count,
            addr_points_mmap: &addr_points_mmap,
            addr_point_count,
            interp_ways: &interp_ways,
            interp_nodes_mmap: &interp_nodes_mmap,
            fine_level: sl,
            coarse_level: cl,
            fine_cells_file: FILE_GEO_CELLS,
            fine_street_entries_file: FILE_STREET_ENTRIES,
            fine_addr_entries_file: FILE_ADDR_ENTRIES,
            fine_interp_entries_file: FILE_INTERP_ENTRIES,
            coarse_cells_file: FILE_COARSE_GEO_CELLS,
            coarse_street_entries_file: FILE_COARSE_STREET_ENTRIES,
            coarse_addr_entries_file: FILE_COARSE_ADDR_ENTRIES,
            coarse_interp_entries_file: FILE_COARSE_INTERP_ENTRIES,
        })?;
    crate::debug::emit_marker("GEOCODE_PASS3_CELLS_END");
    crate::debug::emit_marker("GEOCODE_PASS3_ADMIN_INDEX_START");
    let admin_count = admin::write_admin_index(&config.output_dir, &mut { admin_cell_entries })?;
    crate::debug::emit_marker("GEOCODE_PASS3_ADMIN_INDEX_END");

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
