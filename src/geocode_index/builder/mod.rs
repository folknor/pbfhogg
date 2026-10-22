//! Builder for the reverse geocoding index.
//!
//! Reads an OSM PBF file in multiple passes and writes the set of binary index
//! files defined by the `format` module.

// Mmap offsets and record indices below require a 64-bit address space.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("the geocode index builder requires 64-bit pointers");

use std::io::{BufWriter, Write};
use std::path::PathBuf;

use crate::ElementReader;
use crate::coord_fmt::e7_to_deg;

use super::format::*;

mod admin;
mod interp;
mod pass1;
mod pass1_5;
mod pass2;
mod pass3;
mod strings;

// Under the `static-test-hooks` feature, expose Pass 3 Stage A fault-
// injection hooks so an integration-test target of its own can arm them. The rest of
// `pass3` stays crate-private.
#[cfg(all(feature = "static-test-hooks", not(test)))]
pub mod pass3_test_hooks {
    pub use super::pass3::test_hooks::{PANIC_AT_STREETS_WAY_IDX, reset};
}

use strings::StringPool;

pub(super) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// Build configuration
// ---------------------------------------------------------------------------

/// Configuration for the geocode index builder.
#[derive(Clone)]
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

impl BuildConfig {
    fn validate(&self) -> Result<()> {
        // Street and coarse levels are bounded by the segment sampling budget,
        // not the S2 format limit of 30. Cell covering samples each segment at
        // a fraction of the cell width, so per-segment work doubles with every
        // level. Reject before reading input so a long or antimeridian-crossing
        // segment cannot turn a build into an unbounded walk.
        let max_level = pass3::MAX_BUILD_S2_LEVEL;
        for (name, level) in [("street", self.street_level), ("coarse", self.coarse_level)] {
            if level > max_level {
                return Err(format!(
                    "{name} S2 level must be in 0..={max_level}, got {level}. Levels above \
                     {max_level} exceed the builder's per-segment cell-covering budget."
                )
                .into());
            }
            debug_assert!(
                !pass3::level_exceeds_sampling_budget(level),
                "level {level} passed validation but exceeds the sampling budget"
            );
        }
        if pass3::admin_level_exceeds_cell_budget(self.admin_level) {
            let max_level = pass3::MAX_ADMIN_S2_LEVEL;
            return Err(format!(
                "admin S2 level must be in 0..={max_level}, got {}. Higher levels exceed the \
                 per-polygon flood-fill cell budget.",
                self.admin_level
            )
            .into());
        }
        debug_assert!(self.admin_level <= pass3::MAX_ADMIN_S2_LEVEL);
        if self.coarse_level >= self.street_level {
            return Err(format!(
                "coarse S2 level ({}) must be less than street S2 level ({})",
                self.coarse_level, self.street_level
            )
            .into());
        }
        validate_radius("search radius", self.fine_search_radius_m)?;
        validate_radius("coarse search radius", self.coarse_search_radius_m)?;
        Ok(())
    }
}

/// Build statistics.
#[derive(Debug, Default)]
pub struct BuildStats {
    pub addr_points: u64,
    pub street_ways: u64,
    pub interp_ways: u64,
    pub admin_polygons: u64,
    /// Relations that produced no polygon because their boundary geometry was
    /// missing, unclosed, or degenerated during simplification.
    pub dropped_admin_relations: u64,
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
pub fn build_geocode_index(config: &BuildConfig, host: &crate::host::Host) -> Result<BuildStats> {
    config.validate()?;
    let destination = &config.output_dir;
    // Publication renames the directory itself, which `.`, `..` or a root
    // cannot be (and `.` would hold its own staging directory). Refuse here,
    // not after a full build has been spent on an unpublishable generation.
    if destination.file_name().is_none() {
        return Err(format!(
            "output directory {} must name a directory, not '.', '..' or a root, \
             because publication renames it",
            destination.display()
        )
        .into());
    }
    let has_index = destination.join(FILE_HEADER).exists();
    if has_index && !config.force {
        return Err(format!(
            "output directory {} already contains a geocode index. \
             Use --force to replace it.",
            destination.display()
        )
        .into());
    }
    // Publication replaces the whole directory, so anything that is not an
    // index (or empty) would be deleted with it. `--force` replaces an index,
    // never an arbitrary directory, so this refusal holds under it too.
    if !has_index && destination.exists() {
        let replaceable = destination.is_dir() && std::fs::read_dir(destination)?.next().is_none();
        if !replaceable {
            return Err(format!(
                "output path {} exists and is not a geocode index directory; \
                 publication would replace it wholesale. Choose a new or empty directory.",
                destination.display()
            )
            .into());
        }
    }
    // Build on separate inodes. Renaming a completed generation never truncates
    // the files mapped by an already-open Reader, including on failed builds.
    let staging = StagedIndex::new(destination)?;
    let mut staged_config = config.clone();
    staged_config.output_dir = staging.path.join("new");
    let stats = build_generation(&staged_config, host)?;
    staging.publish(destination)?;
    Ok(stats)
}

struct StagedIndex {
    path: PathBuf,
}

impl StagedIndex {
    fn new(destination: &std::path::Path) -> Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(parent)?;
        loop {
            let path = parent.join(format!(
                ".geocode-build-{}",
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn publish(&self, destination: &std::path::Path) -> Result<()> {
        let backup = self.path.join("old");
        let had_old = match std::fs::rename(destination, &backup) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.into()),
        };
        // There is a short pathname gap between these two renames. Callers
        // must serialize publication with Reader::open and other builders;
        // queries on already-open readers need no such coordination.
        if let Err(e) = std::fs::rename(self.path.join("new"), destination) {
            if had_old && let Err(restore) = std::fs::rename(&backup, destination) {
                // Preserve the only old generation if rollback itself fails.
                return Err(format!(
                    "publish failed: {e}; restore failed: {restore}; old index retained at {}",
                    backup.display()
                )
                .into());
            }
            return Err(e.into());
        }
        Ok(())
    }
}

impl Drop for StagedIndex {
    fn drop(&mut self) {
        // A failed rollback leaves the backup here for manual recovery.
        if !self.path.join("old").exists() || !self.path.join("new").exists() {
            drop(std::fs::remove_dir_all(&self.path));
        }
    }
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn build_generation(config: &BuildConfig, host: &crate::host::Host) -> Result<BuildStats> {
    config.validate()?;
    // Pass 2a pread worker pool: PrimitiveBlock Vec<u8>s allocated on
    // decode workers and freed on the main-thread merge path. Uncapped,
    // arenas grow to 25+ GB anon RSS at planet scale (documented
    // rationale for the previous sequential-Pass-2 choice).
    host.process.cross_thread_alloc_pool();

    let start_time = std::time::Instant::now();

    std::fs::create_dir_all(&config.output_dir)?;

    // Validate input
    crate::commands::require_indexdata(
        &config.input_path,
        config.direct_io,
        config.force,
        "input PBF has no blob-level indexdata. Without indexdata, \
         every blob must be decompressed (significantly slower).",
        host,
    )?;

    // Read PBF header
    let reader = ElementReader::open_in(&config.input_path, config.direct_io, host)?;
    let pbf_header = reader.header();
    let repl_seq = u32::try_from(
        pbf_header
            .osmosis_replication_sequence_number()
            .unwrap_or(0),
    )
    .map_err(
        |_| "replication sequence exceeds the geocode header u32 range; bump FORMAT_VERSION",
    )?;
    #[allow(clippy::cast_sign_loss)]
    let repl_ts = pbf_header.osmosis_replication_timestamp().unwrap_or(0) as u64;
    drop(reader);

    let mut strings = StringPool::new();

    // -----------------------------------------------------------------------
    // Pass 1: Relations - collect admin boundary metadata + way member IDs
    // (Runs first so we know which way IDs to collect in pass 3.)
    // -----------------------------------------------------------------------
    crate::debug::emit_marker("GEOCODE_PASS1_START");
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.pass1",
        "Pass 1: Relations..."
    );
    #[cfg(feature = "hotpath")]
    let pass1_start = std::time::Instant::now();

    let (admin_relations, needed_admin_ways) =
        pass1::run_pass1(&config.input_path, config.direct_io, &mut strings, host)?;
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.pass1.admin_relations",
        "  {} admin relations",
        admin_relations.len()
    );

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
    // way schedule (Pass 1.5), the node schedule (Pass 2a), the node-ID
    // bound (Pass 1.5's `IdSet::pre_allocate`, verified in Pass 2a), and a
    // single shared file handle reused by both phases' pread workers.
    // Previously two separate header walks: Pass 1.5's own walker and
    // Pass 2a's `build_classify_schedule(Node)` call. Consolidated walker
    // saves ~26 s at Europe / ~80 s at planet (2026-04-18 bench `bf8f2038`).
    // -----------------------------------------------------------------------
    crate::debug::emit_marker("GEOCODE_SCHEDULES_START");
    let (node_schedule, way_schedule, max_node_id, shared_file) =
        pass1_5::build_pass2_schedules(&config.input_path, host)?;
    crate::debug::emit_marker("GEOCODE_SCHEDULES_END");

    crate::debug::emit_marker("GEOCODE_PASS1_5_START");
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.pass1_5",
        "Pass 1.5: Referenced node collection..."
    );

    let referenced_nodes = pass1_5::run_pass1_5(
        &way_schedule,
        max_node_id,
        &shared_file,
        &needed_admin_ways,
        host,
    )?;
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
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.pass2",
        "Pass 2: Nodes + Ways..."
    );
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
        max_node_id,
        needed_admin_ways,
        referenced_nodes,
        &mut strings,
        host,
    )?;
    drop(node_schedule);
    drop(way_schedule);
    drop(shared_file);
    crate::debug::emit_marker("GEOCODE_PASS2_SCAN_END");

    // Ring assembly + simplification
    crate::debug::emit_marker("GEOCODE_PASS2_ADMIN_ASSEMBLY_START");
    let (admin_polygons, dropped_admin_relations) =
        admin::assemble_admin_polygons(&admin_relations, &way_geom, config, &host.executor);
    drop(way_geom);
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.pass2.admin_polygons",
        "  {} admin polygons assembled",
        admin_polygons.len()
    );
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.pass2.admin_dropped",
        "  {dropped_admin_relations} admin relations dropped"
    );
    crate::debug::emit_marker("GEOCODE_PASS2_ADMIN_ASSEMBLY_END");
    // Deliberately a conjunction, not a disjunction. An admin-only index (a
    // boundaries extract with no addressable content) and an
    // interpolation-only index are both legitimate; only an index with
    // nothing queryable at all is a build that should have failed. The
    // admin-only direction is pinned by the five admin query tests, which all
    // build from inputs with zero address points and zero streets.
    if addr_point_count == 0
        && street_way_count == 0
        && interp_ways.is_empty()
        && admin_polygons.is_empty()
    {
        return Err("geocode build found zero address points, streets, interpolation ways, and admin polygons; refusing to publish an empty index".into());
    }

    #[cfg(feature = "hotpath")]
    let pass2_ms = pass2_start.elapsed().as_millis();

    // -----------------------------------------------------------------------
    // Interpolation endpoint resolution (reads from mmap'd addr_points.bin)
    // -----------------------------------------------------------------------
    crate::debug::emit_marker("GEOCODE_PASS2_INTERP_RESOLVE_START");
    strings.validate()?;
    checked_count(interp_ways.len(), "interpolation ways")?;
    let resolved = interp::resolve_interpolation_endpoints_mmap(
        &mut interp_ways,
        &addr_points_mmap,
        &interp_nodes_mmap,
        &strings,
        config.street_level,
        &host.executor,
    );
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.pass2.interp_resolved",
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
    let polygon_count = checked_count(admin_polygons.len(), "admin polygons")?;
    if polygon_count > INTERIOR_FLAG {
        return Err(
            "admin polygon count exceeds the format interior-flag limit; bump FORMAT_VERSION"
                .into(),
        );
    }
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
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.pass3",
        "Pass 3: S2 cells + write (bucketed)..."
    );
    #[cfg(feature = "hotpath")]
    let pass4_start = std::time::Instant::now();

    let sl = config.street_level;
    let cl = config.coarse_level;
    let interp_way_count = interp_ways.len();

    // Admin cells are small enough to stay in memory
    crate::debug::emit_marker("GEOCODE_PASS3_ADMIN_CELLS_START");
    let admin_cell_entries =
        pass3::assign_admin_cells(&admin_polygons, config.admin_level, &host.executor);
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
            executor: &host.executor,
        })?;
    crate::debug::emit_marker("GEOCODE_PASS3_CELLS_END");
    crate::debug::emit_marker("GEOCODE_PASS3_ADMIN_INDEX_START");
    let admin_count = admin::write_admin_index(&config.output_dir, &mut { admin_cell_entries })?;
    crate::debug::emit_marker("GEOCODE_PASS3_ADMIN_INDEX_END");

    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.pass3.cells",
        "  {fine_count} fine cells, {coarse_count} coarse cells, {admin_count} admin cells"
    );

    // Write header
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
        interp_way_count: checked_count(interp_way_count, "interpolation ways")?,
        admin_polygon_count: checked_count(admin_polygons.len(), "admin polygons")?,
        geo_cell_count: fine_count,
        coarse_cell_count: coarse_count,
        admin_cell_count: admin_count,
    };
    std::fs::write(config.output_dir.join(FILE_HEADER), header.to_bytes())?;

    // Build-time smoke test
    {
        crate::host::diag!(
            host.diagnostics,
            Progress,
            "geocode.smoke_test",
            "  Running smoke test..."
        );
        let test_reader = super::reader::Reader::open(&config.output_dir)?;
        if addr_point_count > 0 {
            let result = test_reader.query(
                e7_to_deg(first_addr_lat_e7 as f64),
                e7_to_deg(first_addr_lon_e7 as f64),
            )?;
            if result.address.is_none() && result.street.is_none() {
                crate::host::diag!(
                    host.diagnostics,
                    Warning,
                    "geocode.smoke_test.no_match",
                    "  WARNING: smoke test query returned no address or street match"
                );
            }
        }
    }

    #[cfg(feature = "hotpath")]
    let pass4_ms = pass4_start.elapsed().as_millis();

    let elapsed = start_time.elapsed();
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.done",
        "Done in {:.1}s",
        elapsed.as_secs_f64()
    );

    #[cfg(feature = "hotpath")]
    {
        crate::host::diag!(
            host.diagnostics,
            Developer,
            "geocode.hotpath.pass1_relations_ms",
            "pass1_relations_ms={pass1_ms}"
        );
        crate::host::diag!(
            host.diagnostics,
            Developer,
            "geocode.hotpath.pass2_nodes_ways_ms",
            "pass2_nodes_ways_ms={pass2_ms}"
        );
        crate::host::diag!(
            host.diagnostics,
            Developer,
            "geocode.hotpath.pass3_cells_ms",
            "pass3_cells_ms={pass4_ms}"
        );
        crate::host::diag!(
            host.diagnostics,
            Developer,
            "geocode.hotpath.addr_points",
            "addr_points={addr_point_count}"
        );
        crate::host::diag!(
            host.diagnostics,
            Developer,
            "geocode.hotpath.street_ways",
            "street_ways={street_way_count}"
        );
        crate::host::diag!(
            host.diagnostics,
            Developer,
            "geocode.hotpath.interp_ways",
            "interp_ways={interp_way_count}"
        );
        crate::host::diag!(
            host.diagnostics,
            Developer,
            "geocode.hotpath.admin_polygons",
            "admin_polygons={}",
            admin_polygons.len()
        );
        crate::host::diag!(
            host.diagnostics,
            Developer,
            "geocode.hotpath.strings_bytes",
            "strings_bytes={}",
            strings.data.len()
        );
        crate::host::diag!(
            host.diagnostics,
            Developer,
            "geocode.hotpath.strings_unique",
            "strings_unique={}",
            strings.index.len()
        );
    }
    crate::debug::emit_marker("GEOCODE_PASS3_END");

    Ok(BuildStats {
        addr_points: u64::from(addr_point_count),
        street_ways: u64::from(street_way_count),
        interp_ways: interp_way_count as u64,
        admin_polygons: admin_polygons.len() as u64,
        dropped_admin_relations: dropped_admin_relations as u64,
        fine_cells: u64::from(fine_count),
        coarse_cells: u64::from(coarse_count),
        admin_cells: u64::from(admin_count),
    })
}

fn checked_count(count: usize, kind: &str) -> Result<u32> {
    u32::try_from(count).map_err(|_| {
        format!("{kind} count exceeds u32::MAX; widen the on-disk count and bump FORMAT_VERSION")
            .into()
    })
}

#[cfg(test)]
mod config_tests {
    use super::BuildConfig;

    #[test]
    fn count_limit_rejects_the_first_unrepresentable_record_count() {
        assert_eq!(
            super::checked_count(u32::MAX as usize, "test").expect("limit"),
            u32::MAX
        );
        assert!(super::checked_count(u32::MAX as usize + 1, "test").is_err());
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn rejects_invalid_levels_and_radii() {
        let mut config = BuildConfig::default();
        config.street_level = 31;
        assert!(config.validate().is_err());
        config.street_level = 17;
        config.coarse_level = 17;
        assert!(config.validate().is_err());
        config.coarse_level = 14;
        config.fine_search_radius_m = f32::NAN;
        assert!(config.validate().is_err());
        config.fine_search_radius_m = -1.0;
        assert!(config.validate().is_err());
        config.fine_search_radius_m = 75.0;
        config.coarse_search_radius_m = f32::INFINITY;
        assert!(config.validate().is_err());
    }
}
