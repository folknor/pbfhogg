//! Builder for the reverse geocoding index.
//!
//! Reads an OSM PBF file in multiple passes and writes the set of binary index
//! files defined by the `format` module.

// Mmap offsets and record indices below require a 64-bit address space.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("the geocode index builder requires 64-bit pointers");

use std::io::Write;
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

use strings::StringPool;

pub(super) type Result<T> = crate::Result<T>;

use crate::error::Error;

use crate::tunables::DIAGNOSTIC_ID_SAMPLE_LIMIT;

/// Keep the lowest distinct IDs so parallel callers produce a stable, bounded
/// sample regardless of worker completion order.
pub(super) fn sample_id(sample: &mut Vec<i64>, id: i64) {
    if sample.contains(&id) {
        return;
    }
    if sample.len() == DIAGNOSTIC_ID_SAMPLE_LIMIT {
        if sample.last().is_some_and(|&last| id >= last) {
            return;
        }
        sample.pop();
    }
    sample.push(id);
    sample.sort_unstable();
}

// ---------------------------------------------------------------------------
// Build configuration
// ---------------------------------------------------------------------------

/// Configuration for the geocode index builder.
#[derive(Clone)]
///
/// Where the index goes, and whether an existing one may be replaced, is the
/// [`OutputDir`](crate::output::OutputDir) passed beside it.
pub struct BuildConfig {
    pub input_path: PathBuf,
    /// Permit input PBFs that do not have blob-level indexdata.
    pub allow_unindexed: bool,
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
            allow_unindexed: false,
            street_level: crate::tunables::GEOCODE_DEFAULT_STREET_LEVEL,
            coarse_level: crate::tunables::GEOCODE_DEFAULT_COARSE_LEVEL,
            admin_level: crate::tunables::GEOCODE_DEFAULT_ADMIN_LEVEL,
            max_admin_vertices: crate::tunables::GEOCODE_DEFAULT_MAX_ADMIN_VERTICES,
            fine_search_radius_m: crate::tunables::GEOCODE_DEFAULT_FINE_SEARCH_RADIUS_M,
            coarse_search_radius_m: crate::tunables::GEOCODE_DEFAULT_COARSE_SEARCH_RADIUS_M,
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
        let max_level = crate::tunables::MAX_BUILD_S2_LEVEL;
        for (name, level) in [("street", self.street_level), ("coarse", self.coarse_level)] {
            if level > max_level {
                return Err(Error::invalid_argument(format!(
                    "{name} S2 level must be in 0..={max_level}, got {level}. Levels above \
                     {max_level} exceed the builder's per-segment cell-covering budget."
                )));
            }
            debug_assert!(
                !pass3::level_exceeds_sampling_budget(level),
                "level {level} passed validation but exceeds the sampling budget"
            );
        }
        if pass3::admin_level_exceeds_cell_budget(self.admin_level) {
            let max_level = pass3::MAX_ADMIN_S2_LEVEL;
            return Err(Error::invalid_argument(format!(
                "admin S2 level must be in 0..={max_level}, got {}. Higher levels exceed the \
                 per-polygon flood-fill cell budget.",
                self.admin_level
            )));
        }
        debug_assert!(self.admin_level <= pass3::MAX_ADMIN_S2_LEVEL);
        if self.coarse_level >= self.street_level {
            return Err(Error::invalid_argument(format!(
                "coarse S2 level ({}) must be less than street S2 level ({})",
                self.coarse_level, self.street_level
            )));
        }
        validate_radius("search radius", self.fine_search_radius_m)
            .map_err(Error::invalid_argument_from)?;
        validate_radius("coarse search radius", self.coarse_search_radius_m)
            .map_err(Error::invalid_argument_from)?;
        Ok(())
    }
}

/// Result of the builder's query smoke test.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum SmokeOutcome {
    /// No address point was written, so there was no query coordinate.
    #[default]
    SkippedNoAddressPoints,
    /// The query at the first address point found an address or a street.
    Passed,
    /// The query at the first address point found neither an address nor a
    /// street. The coordinates are in decimicrodegrees (10^-7 degrees).
    NoMatch { latitude_e7: i32, longitude_e7: i32 },
}

/// Build statistics.
///
/// The public count fields mirror counts stored in the index header. Keep
/// those fields for API compatibility, but populate them from the header so
/// the published index remains their single source of truth. Dropped admin
/// relations are build-only diagnostics. The `smoke_test` field is a
/// `SmokeOutcome` from querying the newly written index, so neither value can
/// be derived from the header.
#[derive(Debug, Default)]
pub struct BuildStats {
    pub addr_points: u64,
    pub street_ways: u64,
    pub interp_ways: u64,
    pub admin_polygons: u64,
    /// Relations that produced no polygon because their boundary geometry was
    /// missing, unclosed, or degenerated during simplification, or because
    /// ring assembly exceeded its search budget (reported as a warning).
    pub dropped_admin_relations: u64,
    pub fine_cells: u64,
    pub coarse_cells: u64,
    pub admin_cells: u64,
    /// Outcome of the reader query performed against the newly written index.
    pub smoke_test: SmokeOutcome,
}

// ---------------------------------------------------------------------------
// Main build function
// ---------------------------------------------------------------------------

/// Build the reverse geocoding index from an OSM PBF file.
///
/// # Known limitations
///
/// - **Memory:** Pass 3's cell entries are bucketed through temp files, but
///   the other intermediates (way geometry, admin polygons, interpolation
///   ways, admin cell entries) are held in RAM and grow with the input, so
///   peak memory scales with the extract. Planet builds have completed; their
///   measured peak memory and host are recorded in `reference/performance.md`
///   ("build-geocode-index"). A host with less memory than that record shows
///   needs those intermediates moved to temp files.
pub fn build_geocode_index(
    config: &BuildConfig,
    output: IndexOutput,
    host: &crate::host::Host,
    scratch: crate::scratch::RunScratch,
) -> Result<BuildStats> {
    let output = output.0.bind(&host.invocation);
    host.invocation.check()?;
    let stats = build_geocode_index_to(config, &output, host, scratch)?;
    output.publish()?;
    Ok(stats)
}

/// [`build_geocode_index`] up to, and not including, publication. The
/// profiler span sits here so it ends where the work ends: the fsync and the
/// directory swap that follow are the output's, not the build's.
#[hotpath::measure]
fn build_geocode_index_to(
    config: &BuildConfig,
    output: &crate::output::BoundOutputDir,
    host: &crate::host::Host,
    scratch: crate::scratch::RunScratch,
) -> Result<BuildStats> {
    config.validate()?;
    refuse_scratch_inside_index(scratch.path(), output.destination())?;
    // The generation is built inside the output's staging directory, on
    // inodes of its own: publication swaps the whole directory, so a failed
    // build never truncates the files an already-open Reader has mapped,
    // and the previous index survives every failure. The writer is finished
    // only once the build returned with every file it wrote closed.
    let writer = output.writer()?;
    // Every failure of the build names the index it was building; a file
    // the failure is about is named beneath this.
    let stats = build_generation(config, writer.path(), host, &scratch).map_err(|e| {
        e.context(format!(
            "building geocode index {}",
            output.destination().display()
        ))
    })?;
    writer.finish();
    // The run scratch goes before publication: its removal is the run's
    // cleanup, and publication only syncs and renames the index.
    drop(scratch);
    Ok(stats)
}

/// Create (or truncate) one of the build's data files, buffered, naming the
/// file in a failure: a bare "Permission denied" or "No space left on
/// device" does not say which of the index's files it was. Every path is
/// inside the build's own directories (the output's staging directory or a
/// run-scratch bucket tree), so this is not a second output mechanism.
#[allow(clippy::disallowed_methods)]
pub(super) fn create_data_file(
    path: &std::path::Path,
) -> Result<std::io::BufWriter<std::fs::File>> {
    std::fs::File::create(path)
        .map(std::io::BufWriter::new)
        .map_err(|e| Error::from(e).context(format!("failed to create {}", path.display())))
}

/// Write a whole data file into the output's staging directory, naming it
/// in a failure.
#[allow(clippy::disallowed_methods)]
pub(super) fn write_data_file(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes)
        .map_err(|e| Error::from(e).context(format!("failed to write {}", path.display())))
}

/// The output directory of a geocode index: an
/// [`OutputDir`](crate::output::OutputDir) sure to carry
/// [`replaceable_index_dir`], so overwrite permission can replace an index
/// but never an arbitrary directory. Built only by [`index_output`].
#[derive(Debug)]
pub struct IndexOutput(crate::output::OutputDir);

impl IndexOutput {
    /// The directory the caller named.
    #[must_use]
    pub fn destination(&self) -> &std::path::Path {
        self.0.destination()
    }
}

/// Scratch must not live inside the directory the index is published over:
/// publication moves a replaced directory aside and deletes it, scratch and
/// all. The scratch directory is already created when this runs, so an
/// existing destination that contains it is refused.
fn refuse_scratch_inside_index(
    scratch: &std::path::Path,
    destination: &std::path::Path,
) -> Result<()> {
    if crate::scratch::is_within(scratch, destination)? {
        return Err(Error::invalid_argument(format!(
            "the scratch directory {} is inside the index output {}, which publication replaces; \
             choose a scratch directory outside it",
            scratch.display(),
            destination.display()
        )));
    }
    Ok(())
}

/// The geocode index's rule for replacing an existing output directory:
/// only an index, or an empty directory, may be replaced. Publication swaps
/// the whole directory, so anything else in it would be deleted with it;
/// overwrite permission replaces an index, never an arbitrary directory.
/// Run when the output is built and again immediately before publication,
/// after the output layer's own rule for every directory it replaces: one
/// that holds anything pbfhogg never deletes (a backup, staging, a run
/// directory, an older version's scratch) is refused whatever this says, and
/// only an index's `.buckets-level*` trees go with it.
///
/// # Errors
/// An invalid-argument error naming the directory when it holds anything
/// but an index.
pub fn replaceable_index_dir(destination: &std::path::Path) -> Result<()> {
    if !destination.is_dir() || destination.join(FILE_HEADER).exists() {
        return Ok(());
    }
    if std::fs::read_dir(destination)?.next().is_none() {
        return Ok(());
    }
    Err(Error::invalid_argument(format!(
        "output path {} exists and is not a geocode index directory; publication would \
         replace it wholesale. Choose a new or empty directory.",
        destination.display()
    )))
}

/// Check and stage the output for an index at `destination`;
/// `diagnostics` receives its cleanup warnings.
///
/// # Errors
/// As [`OutputDir::new`](crate::output::OutputDir::new), plus
/// [`replaceable_index_dir`]'s refusal.
pub fn index_output(
    destination: impl AsRef<std::path::Path>,
    overwrite: crate::output::Overwrite,
    diagnostics: &crate::host::Diagnostics,
) -> Result<IndexOutput> {
    index_output_with_staging(
        destination,
        overwrite,
        crate::output::Staging::Beside,
        diagnostics,
    )
}

/// [`index_output`], with the staging directory, and the backup of a
/// replaced index, placed where `staging` says.
///
/// # Errors
/// As [`OutputDir::with_staging`](crate::output::OutputDir::with_staging),
/// plus [`replaceable_index_dir`]'s refusal.
pub fn index_output_with_staging(
    destination: impl AsRef<std::path::Path>,
    overwrite: crate::output::Overwrite,
    staging: crate::output::Staging<'_>,
    diagnostics: &crate::host::Diagnostics,
) -> Result<IndexOutput> {
    crate::output::OutputDir::with_staging(
        destination,
        overwrite,
        Some(replaceable_index_dir),
        staging,
        diagnostics,
    )
    .map(IndexOutput)
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn build_generation(
    config: &BuildConfig,
    output_dir: &std::path::Path,
    host: &crate::host::Host,
    scratch: &crate::scratch::RunScratch,
) -> Result<BuildStats> {
    // Pass 2a pread worker pool: PrimitiveBlock Vec<u8>s allocated on
    // decode workers and freed on the main-thread merge path. Uncapped,
    // arenas grow to 25+ GB anon RSS at planet scale (documented
    // rationale for the previous sequential-Pass-2 choice).
    host.process.cross_thread_alloc_pool();

    let start_time = std::time::Instant::now();

    // `output_dir` is the output's staging directory, which `OutputDir`
    // created when it was staged; it is not created again here.

    // The input is opened four times: the indexdata probe, the replication
    // header read, pass 1's relation scan and the schedule walk, whose one
    // description the unindexed classify, pass 1.5 and pass 2 then share.
    // Every open is derived from this pin, taken before any of them, so an
    // input renamed over mid-build is never read partly from each file.
    // Pinned here rather than by the caller: nothing else reads the input
    // before the build, and a failure to pin is named with the index.
    let input = crate::read::input::PinnedInput::pin(&config.input_path, host)?;

    // Validate input
    crate::commands::require_indexdata(
        &input,
        config.direct_io,
        config.allow_unindexed,
        "input PBF has no blob-level indexdata. Without indexdata, \
         every blob must be decompressed (significantly slower).",
        host,
    )?;

    // Read PBF header
    let reader = ElementReader::open_in(&input, config.direct_io, host)?;
    let pbf_header = reader.header();
    // The geocode header stores replication sequence in a u32. Missing
    // metadata is represented by zero, but wrapping a present larger value
    // would publish a plausible, incorrect source version, so refuse it.
    let repl_seq = u32::try_from(
        pbf_header
            .osmosis_replication_sequence_number()
            .unwrap_or(0),
    )
    .map_err(|_| {
        Error::invalid_data(
            "replication sequence exceeds the geocode header u32 range; bump FORMAT_VERSION",
        )
    })?;
    #[allow(clippy::cast_sign_loss)]
    let repl_ts = pbf_header.osmosis_replication_timestamp().unwrap_or(0) as u64;
    drop(reader);

    let mut strings = StringPool::new();

    // -----------------------------------------------------------------------
    // Pass 1: Relations - collect admin boundary metadata + way member IDs
    // (Runs first so we know which way IDs to collect in pass 3.)
    // -----------------------------------------------------------------------
    host.invocation.marker("GEOCODE_PASS1_START");
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.pass1",
        "Pass 1: Relations..."
    );
    #[cfg(feature = "hotpath")]
    let pass1_start = crate::counters::Stopwatch::start();

    let (admin_relations, needed_admin_ways) =
        pass1::run_pass1(&input, config.direct_io, &mut strings, host)?;
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.pass1.admin_relations",
        "  {} admin relations",
        admin_relations.len()
    );

    #[cfg(feature = "hotpath")]
    let pass1_ms = pass1_start.elapsed_ms();

    // -----------------------------------------------------------------------
    // Pass 1.5: Referenced node collection (planet-scale memory optimization)
    //
    // Scan way blobs to collect node IDs referenced by geocode-relevant ways
    // (streets, building addresses, interpolation, admin members). The dense
    // node index in pass 2 only populates entries for these nodes, so its
    // page-cache working set follows the referenced subset rather than every
    // node in the file. Same pattern as ALTW pass 0.
    // -----------------------------------------------------------------------
    host.invocation.marker("GEOCODE_PASS1_END");
    // Every pass boundary is a checkpoint; the passes check their own blob
    // reads, results and chunks in between.
    host.invocation.check()?;

    // -----------------------------------------------------------------------
    // One header walk produces everything Pass 1.5 and Pass 2a need: the
    // way schedule (Pass 1.5), the node schedule (Pass 2a), the node-ID
    // bound (used for Pass 1.5's file-size-bounded `IdSet` pre-allocation and
    // verified in Pass 2a), and one shared file handle for both pread phases.
    // Previously two separate header walks: Pass 1.5's own walker and
    // Pass 2a's `build_classify_schedule(Node)` call. Consolidated walker
    // saves ~26 s at Europe / ~80 s at planet (2026-04-18 bench `bf8f2038`).
    // -----------------------------------------------------------------------
    host.invocation.marker("GEOCODE_SCHEDULES_START");
    let (node_schedule, way_schedule, max_node_id, shared_file) =
        pass1_5::build_pass2_schedules(&input, host)?;
    host.invocation.marker("GEOCODE_SCHEDULES_END");

    host.invocation.marker("GEOCODE_PASS1_5_START");
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
    host.invocation.marker("GEOCODE_PASS1_5_END");

    // -----------------------------------------------------------------------
    // Pass 2: Nodes + Ways (fused single scan)
    //
    // Sorted PBFs (Sort.Type_then_ID) guarantee all node blobs come before
    // way blobs. A single sequential scan processes nodes first (populating
    // the dense coordinate index for referenced nodes + address points), then
    // ways (streets, buildings, interpolation, admin geometry).
    // -----------------------------------------------------------------------
    host.invocation.marker("GEOCODE_PASS2_START");
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.pass2",
        "Pass 2: Nodes + Ways..."
    );
    #[cfg(feature = "hotpath")]
    let pass2_start = crate::counters::Stopwatch::start();

    // `way_geom`, `admin_polygons`, `interp_ways`, and `admin_cell_entries`
    // remain input-sized in-memory intermediates. Their measured planet
    // footprint is in reference/performance.md; externalizing them needs a
    // coordinated streaming and external-merge design because later passes
    // consume each collection.
    host.invocation.marker("GEOCODE_PASS2_SCAN_START");
    let pass2::Pass2Output {
        addr_point_count,
        street_way_count,
        mut interp_ways,
        way_geom,
        street_ways,
        street_nodes,
        addr_points,
        interp_nodes,
    } = pass2::run_pass2(
        output_dir,
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
    // Pass 2 was the input's last reader.
    drop(input);
    host.invocation.marker("GEOCODE_PASS2_SCAN_END");
    host.invocation.check()?;

    // Ring assembly + simplification.
    //
    // No cancellation checkpoint inside this pass, interpolation resolve,
    // the pass 2 data file writes, pass 3 admin cell assignment or the admin
    // index write: after the token trips each runs to its end, bounded by
    // the `host.invocation.check()` calls around it (admin assembly is the
    // longest at planet scale). A check inside would be per element or per
    // polygon in a hot loop. If a stop ever proves too slow, the fix is to
    // chunk the pass and check per chunk, with a matched A/B showing the
    // untripped check costs nothing.
    host.invocation.marker("GEOCODE_PASS2_ADMIN_ASSEMBLY_START");
    let (admin_polygons, dropped_admin) =
        admin::assemble_admin_polygons(&admin_relations, &way_geom, config, &host.executor);
    let missing_admin_geometry = admin::missing_admin_way_geometry(&admin_relations, &way_geom);
    let dropped_admin_relations = dropped_admin.total;
    host.invocation
        .counter("geocode_admin_relation_drops", dropped_admin_relations);
    host.invocation.counter(
        "geocode_admin_way_member_refs_without_geometry",
        missing_admin_geometry.member_refs,
    );
    if missing_admin_geometry.member_refs > 0 {
        crate::host::diag!(
            host.diagnostics,
            Warning,
            "geocode.pass2.admin_way_geometry_missing",
            "{} admin way member refs have no geometry; way IDs (sample): {:?}",
            missing_admin_geometry.member_refs,
            missing_admin_geometry.sample_way_ids
        );
    }
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
    if dropped_admin_relations > 0 {
        crate::host::diag!(
            host.diagnostics,
            Warning,
            "geocode.pass2.admin_dropped_ids",
            "{dropped_admin_relations} admin relations dropped; relation IDs (sample): {:?}",
            dropped_admin.sample_relation_ids
        );
    }
    if dropped_admin.over_budget > 0 {
        crate::host::diag!(
            host.diagnostics,
            Warning,
            "geocode.pass2.admin_ring_budget",
            "{} admin relations refused: ring assembly exceeded its search budget \
             (counted among the dropped relations above)",
            dropped_admin.over_budget
        );
    }
    host.invocation.marker("GEOCODE_PASS2_ADMIN_ASSEMBLY_END");
    host.invocation.check()?;
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
        return Err(Error::invalid_data(
            "geocode build found zero address points, streets, interpolation ways, and admin polygons; refusing to publish an empty index",
        ));
    }

    #[cfg(feature = "hotpath")]
    let pass2_ms = pass2_start.elapsed_ms();

    // -----------------------------------------------------------------------
    // Interpolation endpoint resolution (reads from mmap'd addr_points.bin)
    // -----------------------------------------------------------------------
    host.invocation.marker("GEOCODE_PASS2_INTERP_RESOLVE_START");
    strings.validate()?;
    checked_count(interp_ways.len(), "interpolation ways")?;
    let resolved = interp::resolve_interpolation_endpoints_mmap(
        &mut interp_ways,
        addr_points.table(),
        interp_nodes.table(),
        &strings,
        config.street_level,
        &host.executor,
        &host.invocation,
    );
    let mut unresolved_interpolation_count = 0_u64;
    let mut unresolved_interpolation_ids = Vec::new();
    for way in &interp_ways {
        if way.start_number == 0 || way.end_number == 0 {
            unresolved_interpolation_count = unresolved_interpolation_count.saturating_add(1);
            sample_id(&mut unresolved_interpolation_ids, way.way_id);
        }
    }
    host.invocation.counter(
        "geocode_unresolved_interpolation_ways",
        unresolved_interpolation_count,
    );
    if unresolved_interpolation_count > 0 {
        crate::host::diag!(
            host.diagnostics,
            Warning,
            "geocode.pass2.interp_unresolved",
            "{unresolved_interpolation_count} interpolation ways have unresolved endpoints; \
             way IDs (sample): {unresolved_interpolation_ids:?}"
        );
    }
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.pass2.interp_resolved",
        "  {resolved}/{} interpolation ways resolved",
        interp_ways.len()
    );
    host.invocation.marker("GEOCODE_PASS2_INTERP_RESOLVE_END");
    host.invocation.check()?;

    // Write interp_ways.bin now (after resolution has set start/end numbers)
    host.invocation.marker("GEOCODE_PASS2_WRITE_START");
    {
        let mut iw_out = create_data_file(&output_dir.join(FILE_INTERP_WAYS))?;
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
        return Err(Error::invalid_data(
            "admin polygon count exceeds the format interior-flag limit; bump FORMAT_VERSION",
        ));
    }
    admin::write_admin_data(output_dir, &admin_polygons)?;
    write_data_file(&output_dir.join(FILE_STRINGS), &strings.data)?;
    host.invocation.marker("GEOCODE_PASS2_WRITE_END");

    // -----------------------------------------------------------------------
    host.invocation.marker("GEOCODE_PASS2_END");
    host.invocation.check()?;
    host.invocation.marker("GEOCODE_PASS3_START");
    // Pass 3: Bucketed S2 cell assignment + write cell index files
    //
    // Instead of accumulating all cell entries in RAM, where they grow with
    // the input, partition into 256 buckets by top 8 bits of cell_id. Write tagged
    // entries to temp bucket files, then process one bucket at a time.
    // -----------------------------------------------------------------------
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.pass3",
        "Pass 3: S2 cells + write (bucketed)..."
    );
    #[cfg(feature = "hotpath")]
    let pass4_start = crate::counters::Stopwatch::start();

    let sl = config.street_level;
    let cl = config.coarse_level;
    let interp_way_count = interp_ways.len();

    // Admin cells are small enough to stay in memory
    host.invocation.marker("GEOCODE_PASS3_ADMIN_CELLS_START");
    let admin_cell_entries =
        pass3::assign_admin_cells(&admin_polygons, config.admin_level, &host.executor);
    host.invocation.marker("GEOCODE_PASS3_ADMIN_CELLS_END");
    host.invocation.check()?;

    // Fused fine + coarse cell assignment (plan item #4). Single Stage A
    // pass over streets/addrs/interps at the fine level derives coarse
    // cells on the fly via S2 parent + per-segment dedup, eliminating
    // the duplicate `cover_segment` pass that ran separately at coarse
    // level. Two Stage B invocations follow - one per bucket tree.
    host.invocation.marker("GEOCODE_PASS3_CELLS_START");
    let (fine_count, coarse_count) =
        pass3::bucketed_cell_assignment_fused(&pass3::FusedCellAssignmentParams {
            output_dir,
            scratch,
            street_ways: street_ways.table(),
            street_nodes: street_nodes.table(),
            street_way_count,
            addr_points: addr_points.table(),
            addr_point_count,
            interp_ways: &interp_ways,
            interp_nodes: interp_nodes.table(),
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
            cancel: &host.invocation,
        })?;
    host.invocation.marker("GEOCODE_PASS3_CELLS_END");
    host.invocation.marker("GEOCODE_PASS3_ADMIN_INDEX_START");
    let admin_count = admin::write_admin_index(output_dir, &mut { admin_cell_entries })?;
    host.invocation.marker("GEOCODE_PASS3_ADMIN_INDEX_END");
    host.invocation.check()?;

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
    write_data_file(&output_dir.join(FILE_HEADER), &header.to_bytes())?;

    // `Reader::open` runs the same record and string validation described on
    // its public API, so this smoke test intentionally repeats that structural
    // pass on the files just written before querying the result. Its work is
    // part of the build wall time; skipping it would leave written output
    // unchecked by the reader path.
    let first_addr_point = if addr_point_count > 0 {
        Some(addr_points.table().get(0).ok_or_else(|| {
            Error::internal(
                "address point count is nonzero but addr_points.bin has no first record",
            )
        })?)
    } else {
        None
    };
    let smoke_test = {
        crate::host::diag!(
            host.diagnostics,
            Progress,
            "geocode.smoke_test",
            "  Running smoke test..."
        );
        let test_reader = super::reader::Reader::open(output_dir)?;
        if let Some(first_addr_point) = first_addr_point {
            let result = test_reader
                .query(
                    e7_to_deg(f64::from(first_addr_point.lat_e7)),
                    e7_to_deg(f64::from(first_addr_point.lon_e7)),
                )
                .map_err(|e| Error::internal_with("geocode smoke test query failed", e))?;
            if result.address.is_none() && result.street.is_none() {
                crate::host::diag!(
                    host.diagnostics,
                    Warning,
                    "geocode.smoke_test.no_match",
                    "smoke test query at ({:.7}, {:.7}) in index {} returned no address \
                     or street match",
                    e7_to_deg(f64::from(first_addr_point.lat_e7)),
                    e7_to_deg(f64::from(first_addr_point.lon_e7)),
                    output_dir.display()
                );
                SmokeOutcome::NoMatch {
                    latitude_e7: first_addr_point.lat_e7,
                    longitude_e7: first_addr_point.lon_e7,
                }
            } else {
                SmokeOutcome::Passed
            }
        } else {
            SmokeOutcome::SkippedNoAddressPoints
        }
    };
    host.invocation.counter(
        "geocode_smoke_test_no_match",
        matches!(smoke_test, SmokeOutcome::NoMatch { .. }),
    );

    #[cfg(feature = "hotpath")]
    let pass4_ms = pass4_start.elapsed_ms();

    let stats = BuildStats {
        addr_points: u64::from(header.addr_point_count),
        street_ways: u64::from(header.street_way_count),
        interp_ways: u64::from(header.interp_way_count),
        admin_polygons: u64::from(header.admin_polygon_count),
        dropped_admin_relations: dropped_admin_relations as u64,
        fine_cells: u64::from(header.geo_cell_count),
        coarse_cells: u64::from(header.coarse_cell_count),
        admin_cells: u64::from(header.admin_cell_count),
        smoke_test,
    };
    // The counts come before the closing "Done" line so the run reads in
    // sequence; the CLI used to print them after it.
    crate::host::diag!(
        host.diagnostics,
        Summary,
        "geocode.index_built",
        "Index built: {} addr, {} streets, {} interp, {} admin ({} relations dropped), \
         {} fine cells, {} coarse cells, {} admin cells",
        stats.addr_points,
        stats.street_ways,
        stats.interp_ways,
        stats.admin_polygons,
        stats.dropped_admin_relations,
        stats.fine_cells,
        stats.coarse_cells,
        stats.admin_cells,
    );

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
            strings.unique_count()
        );
    }
    host.invocation.marker("GEOCODE_PASS3_END");

    Ok(stats)
}

fn checked_count(count: usize, kind: &str) -> Result<u32> {
    u32::try_from(count).map_err(|_| {
        Error::invalid_data(format!(
            "{kind} count exceeds u32::MAX; widen the on-disk count and bump FORMAT_VERSION"
        ))
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
