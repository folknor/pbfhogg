//! Builder for the reverse geocoding index.
//!
//! Reads an OSM PBF file in multiple passes and writes the set of binary index
//! files defined by the `format` module.

// Mmap offsets and record indices below require a 64-bit address space.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("the geocode index builder requires 64-bit pointers");

use crate::ElementReader;
use crate::coord_fmt::e7_to_deg;

use super::format::*;

mod admin;
mod cover;
mod input_digest;
mod interp;
mod pass1;
mod pass1_5;
mod pass2;
mod pass3;
mod strings;
mod way_store;

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

/// Configuration for the geocode index builder: how to build, not what
/// from. The input is [`build_geocode_index`]'s own argument, and where
/// the index goes, and whether an existing one may be replaced, is the
/// [`IndexOutput`] passed beside it.
#[derive(Clone)]
pub struct BuildConfig {
    /// Permit input PBFs that do not have blob-level indexdata.
    pub allow_unindexed: bool,
    pub direct_io: bool,
    pub street_level: u8,
    pub coarse_level: u8,
    pub admin_level: u8,
    /// How far, in metres, admin boundary simplification may move a
    /// boundary; 0 stores every vertex. A query farther than this from every
    /// original ring of a polygon gets the answer the unsimplified polygon
    /// gives. Must be finite and non-negative.
    pub admin_tolerance_m: f32,
    pub fine_search_radius_m: f32,
    pub coarse_search_radius_m: f32,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            allow_unindexed: false,
            street_level: crate::tunables::GEOCODE_DEFAULT_STREET_LEVEL,
            coarse_level: crate::tunables::GEOCODE_DEFAULT_COARSE_LEVEL,
            admin_level: crate::tunables::GEOCODE_DEFAULT_ADMIN_LEVEL,
            admin_tolerance_m: crate::tunables::GEOCODE_DEFAULT_ADMIN_TOLERANCE_M,
            fine_search_radius_m: crate::tunables::GEOCODE_DEFAULT_FINE_SEARCH_RADIUS_M,
            coarse_search_radius_m: crate::tunables::GEOCODE_DEFAULT_COARSE_SEARCH_RADIUS_M,
            direct_io: false,
        }
    }
}

impl BuildConfig {
    fn validate(&self) -> Result<()> {
        // Street and coarse levels are bounded by the per-segment cell
        // budget, not the S2 format limit of 30. A segment covers every cell
        // it passes through, so per-segment work doubles with every level.
        // Reject before reading input so a long segment cannot turn a build
        // into an unbounded walk.
        // Each level is refused outside the S2 domain first, then by the
        // budget the pass that uses it depends on: the street cover asserts
        // the cell budget for every segment, and the admin cover is bounded
        // by the flood-fill cell budget. The unit tests pin
        // `MAX_BUILD_S2_LEVEL` and `MAX_ADMIN_S2_LEVEL` to exactly these
        // boundaries, so the messages' ranges are the refusals'.
        let s2_max = crate::geocode_index::format::S2_MAX_LEVEL;
        let max_level = crate::tunables::MAX_BUILD_S2_LEVEL;
        for (name, level) in [("street", self.street_level), ("coarse", self.coarse_level)] {
            if level > s2_max || cover::level_exceeds_segment_cell_budget(level) {
                return Err(Error::invalid_argument(format!(
                    "{name} S2 level must be in 0..={max_level}, got {level}. Levels above \
                     {max_level} exceed the builder's per-segment cell-covering budget."
                )));
            }
        }
        let admin_level = self.admin_level;
        if admin_level > s2_max || pass3::admin_level_exceeds_cell_budget(admin_level) {
            let max_level = pass3::MAX_ADMIN_S2_LEVEL;
            return Err(Error::invalid_argument(format!(
                "admin S2 level must be in 0..={max_level}, got {admin_level}. Higher levels \
                 exceed the per-polygon flood-fill cell budget."
            )));
        }
        if self.coarse_level >= self.street_level {
            return Err(Error::invalid_argument(format!(
                "coarse S2 level ({}) must be less than street S2 level ({})",
                self.coarse_level, self.street_level
            )));
        }
        validate_radius(
            "search radius",
            self.fine_search_radius_m,
            self.street_level,
        )
        .map_err(Error::invalid_argument_from)?;
        validate_radius(
            "coarse search radius",
            self.coarse_search_radius_m,
            self.coarse_level,
        )
        .map_err(Error::invalid_argument_from)?;
        crate::geocode_index::format::validate_admin_tolerance(self.admin_tolerance_m)
            .map_err(Error::invalid_argument_from)?;
        Ok(())
    }
}

/// Result of the builder's post-write smoke test. A failed check is not an
/// outcome: the build returns an error and publishes nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum SmokeOutcome {
    /// No address point was written, so there was no record to check.
    #[default]
    SkippedNoAddressPoints,
    /// The written index opened, and the first address point is listed in
    /// its own fine cell and its own coarse cell. This is placement in the
    /// written lists, not a claim about what a query ranks first.
    Passed,
}

/// Build statistics.
///
/// The public count fields mirror counts stored in the index header. Keep
/// those fields for API compatibility, but populate them from the header so
/// the published index remains their single source of truth. Dropped admin
/// relations are build-only diagnostics. The `smoke_test` field is a
/// `SmokeOutcome` from checking the newly written index, so neither value
/// can be derived from the header.
#[derive(Debug, Default)]
pub struct BuildStats {
    pub addr_points: u64,
    pub street_ways: u64,
    /// Interpolation records in the index: one per section between the
    /// address nodes of a resolved way where an interior number exists.
    pub interp_ways: u64,
    pub admin_polygons: u64,
    /// Relations that produced no polygon because their boundary geometry was
    /// missing, unclosed, or degenerated during simplification, or because
    /// assembly refused their rings (reported as a warning per kind): member
    /// ways that do not determine the rings, a ring on one line, or a ring
    /// whose interior depends on where its walk starts.
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
///   measured peak memory and host are recorded in `reference/performance/geocode.md`
///   ("build-geocode-index"). A host with less memory than that record shows
///   needs those intermediates moved to temp files.
///
/// The input is read in several opens, so it must be a regular file
/// (`InvalidArgument` otherwise) and is pinned for the build: a path is
/// pinned once the configuration and output are checked, a caller's pin is
/// shared as it is (see
/// [`InputSource::pin`](crate::read::input::InputSource::pin)).
pub fn build_geocode_index<'a>(
    input: impl Into<crate::read::input::InputSource<'a>>,
    config: &BuildConfig,
    output: IndexOutput,
    host: &crate::host::Host,
    scratch: crate::scratch::RunScratch,
) -> Result<BuildStats> {
    let host = &host.enter()?;
    let mut output = output.0.bind_unissued(&host.invocation)?;
    host.invocation.check()?;
    let stats = build_geocode_index_to(input.into(), config, &mut output, host, scratch)?;
    output.publish()?;
    Ok(stats)
}

/// [`build_geocode_index`] up to, and not including, publication. The
/// profiler span sits here so it ends where the work ends: the fsync and the
/// directory swap that follow are the output's, not the build's.
#[hotpath::measure]
fn build_geocode_index_to(
    input: crate::read::input::InputSource<'_>,
    config: &BuildConfig,
    output: &mut crate::output::BoundOutputDir,
    host: &crate::host::Entered<'_>,
    scratch: crate::scratch::RunScratch,
) -> Result<BuildStats> {
    config.validate()?;
    refuse_scratch_inside_index(scratch.path(), output.destination())?;
    // Every failure of the build names the index it was building; a file
    // the failure is about is named beneath this.
    let destination = output.destination().to_path_buf();
    let building =
        |e: Error| e.context(format!("building geocode index {}", destination.display()));
    // The input is opened four times (see `build_generation`), every open
    // derived from this pin, taken before any of them, so an input renamed
    // over mid-build is never read partly from each file. A path is pinned
    // here, after the configuration and scratch checks; a caller's pin (one
    // that composes several steps on the input) is shared as it is.
    let input = input.pin(host).map_err(building)?;
    // Publication deletes the replaced index whole, so an input inside it
    // (`index/source.pbf`) would go with it. Refused now and again in the
    // output's final inspection, since the input can be moved in meanwhile.
    output.keep_input(input.clone())?;
    // The generation is built inside the output's staging directory, on
    // inodes of its own: publication swaps the whole directory, so a failed
    // build never truncates the files an already-open Reader has mapped,
    // and the previous index survives every failure. The writer is finished
    // only once the build returned with every file it wrote closed.
    let writer = output.writer()?;
    let stats = build_generation(
        &input,
        config,
        writer.path(),
        host,
        &scratch,
        pass3::StageB::Parallel,
        InputValidation::Scoped,
    )
    .map_err(building)?;
    writer.finish();
    // The run scratch goes before publication: its removal is the run's
    // cleanup, and publication only syncs and renames the index.
    drop(scratch);
    Ok(stats)
}

/// Create (or truncate) one of the build's data files, buffered, naming the
/// file in a failure: a bare "Permission denied" or "No space left on
/// device" does not say which of the index's files it was. Every path is
/// inside the output's staging directory, so this is not a second output
/// mechanism; the build's scratch buckets go through `crate::scratch`.
pub(super) fn create_data_file(
    path: &std::path::Path,
) -> Result<std::io::BufWriter<std::fs::File>> {
    create_index_file(path).map(|file| {
        std::io::BufWriter::with_capacity(crate::tunables::GEOCODE_DATA_FILE_BUFFER_BYTES, file)
    })
}

/// Open a file inside the staged index for direct or buffered writes.
#[allow(clippy::disallowed_methods)]
fn create_index_file(path: &std::path::Path) -> Result<std::fs::File> {
    std::fs::File::create(path)
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
/// An index is recognised by its header file: a regular file (never followed
/// through a symlink) at least a header long that starts with the format's
/// magic. The version after the magic is not read, so an index of any
/// format, older ones included, can be rebuilt in place; the magic and the
/// header's length have been the same in every format. The rest of the
/// index is not checked: a directory with a genuine header and other files
/// beside it is an index, and those files go with it.
///
/// # Errors
/// An invalid-argument error naming the directory when it holds anything
/// but an index; an I/O error when what it holds cannot be inspected.
pub fn replaceable_index_dir(destination: &std::path::Path) -> Result<()> {
    // The output layer refuses a destination that is a symlink or not a
    // directory before this runs, and a missing one has nothing to replace.
    match std::fs::symlink_metadata(destination) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(inspect_error(destination, e)),
    }
    let header = destination.join(FILE_HEADER);
    if holds_index_header(&header).map_err(|e| inspect_error(&header, e))? {
        return Ok(());
    }
    let mut entries = std::fs::read_dir(destination).map_err(|e| inspect_error(destination, e))?;
    match entries.next() {
        None => return Ok(()),
        Some(Err(e)) => return Err(inspect_error(destination, e)),
        Some(Ok(_)) => {}
    }
    Err(Error::invalid_argument(format!(
        "output path {} exists and is not a geocode index directory; publication would \
         replace it wholesale. Choose a new or empty directory.",
        destination.display()
    )))
}

fn inspect_error(path: &std::path::Path, e: std::io::Error) -> Error {
    Error::from(e).context(format!(
        "cannot inspect {} to decide whether it is a geocode index",
        path.display()
    ))
}

/// Whether `header` is a geocode index header: a regular file, not reached
/// through a symlink, of at least [`HEADER_SIZE`] bytes beginning with
/// [`HEADER_MAGIC`]. Checked by `lstat` before the open, so a device or FIFO
/// is never opened, and opened with `O_NOFOLLOW | O_NONBLOCK` then checked
/// again, so one swapped in meanwhile is neither followed nor waited on.
fn holds_index_header(header: &std::path::Path) -> std::io::Result<bool> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::symlink_metadata(header) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    }
    // A read of the replaced index's header, to recognise it, not an input
    // of the build.
    #[allow(clippy::disallowed_methods)]
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(header);
    let mut file = match file {
        Ok(file) => file,
        // ELOOP: a symlink was swapped in after the lstat.
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if !file.metadata()?.is_file() {
        return Ok(false);
    }
    let mut prefix = [0u8; HEADER_SIZE];
    match file.read_exact(&mut prefix) {
        Ok(()) => Ok(prefix[..HEADER_MAGIC.len()] == HEADER_MAGIC),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
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

/// Full input validation is retained as an oracle for generated fixtures.
#[derive(Clone, Copy)]
enum InputValidation {
    Scoped,
    #[cfg(test)]
    Full,
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn build_generation(
    input: &crate::read::input::PinnedInput,
    config: &BuildConfig,
    output_dir: &std::path::Path,
    host: &crate::host::Entered<'_>,
    scratch: &crate::scratch::RunScratch,
    stage_b: pass3::StageB,
    validation: InputValidation,
) -> Result<BuildStats> {
    let start_time = std::time::Instant::now();

    // `output_dir` is the output's staging directory, which `OutputDir`
    // created when it was staged; it is not created again here.

    // The input is opened four times, each open derived from the caller's
    // pin: the indexdata probe, the replication header read, the schedule
    // walk, whose one description the unindexed classify, pass 1, pass 1.5
    // and pass 2 then share, and the whole-file digest's own sequential
    // description.

    // Validate input
    crate::commands::require_indexdata(
        input,
        config.direct_io,
        config.allow_unindexed,
        "input PBF has no blob-level indexdata. Without indexdata, \
         every blob must be decompressed (significantly slower).",
        host,
    )?;

    // Read PBF header
    let reader = ElementReader::open_in(input, config.direct_io, host)?;
    let pbf_header = reader.header();
    // The geocode header stores the PBF header's replication sequence and
    // timestamp verbatim: signed, with their presence. Nothing here reads
    // them, so there is no value to refuse and none to narrow.
    let repl_seq = pbf_header.osmosis_replication_sequence_number();
    let repl_ts = pbf_header.osmosis_replication_timestamp();
    drop(reader);

    // -----------------------------------------------------------------------
    // One header walk produces everything the passes need: the relation
    // schedule (Pass 1), the way schedule (Pass 1.5), the node schedule
    // (Pass 2a), the node-ID bound (used for Pass 1.5's file-size-bounded
    // `IdSet` pre-allocation and verified in Pass 2a), and one shared file
    // handle for every pread phase. It runs first so Pass 1 reads only the
    // relation blobs (see `pass1_5::build_schedules`). Consolidating what were
    // two walks saved ~26 s at Europe / ~80 s at planet (2026-04-18 bench
    // `bf8f2038`).
    // -----------------------------------------------------------------------
    host.invocation.marker("GEOCODE_SCHEDULES_START");
    let pass1_5::Schedules {
        relations: relation_schedule,
        nodes: node_schedule,
        ways: way_schedule,
        max_node_id,
        file: shared_file,
        file_size: walked_size,
    } = pass1_5::build_schedules(input, host, validation)?;
    host.invocation.marker("GEOCODE_SCHEDULES_END");
    host.invocation.check()?;

    // The input's whole-file digest for the header, hashed on its own
    // description on a worker that runs beside passes 1 to 3 and is joined
    // only for the header write (see `input_digest`). Opened here, on the
    // calling thread, so the open is counted with the others. The worker
    // borrows the pin, so `input` lives until the scope has joined it.
    let digest_file = input_digest::open(input, &host.invocation)?;
    let digest_stop = std::sync::atomic::AtomicBool::new(false);
    crate::threads::scope(&host.invocation, |s| {
        // The scope's first thread: a refusal leaves nothing born.
        let worker = s
            .spawn(|| input_digest::hash(digest_file, input, &digest_stop, &host.invocation))
            .map_err(|e| Error::from(e).context("spawn the geocode input digest worker"))?;
        // Dropped unused (the build failed or unwound before its header),
        // this stops the hash before the scope joins it.
        let stop_on_failure = input_digest::StopOnDrop(&digest_stop);
        let finish_digest = || {
            stop_on_failure.disarm();
            host.invocation.marker("GEOCODE_INPUT_DIGEST_JOIN_START");
            let joined = worker.join();
            host.invocation.marker("GEOCODE_INPUT_DIGEST_JOIN_END");
            let digest = match joined {
                Ok(digest) => digest?,
                Err(payload) => std::panic::resume_unwind(payload),
            }
            .ok_or_else(|| Error::internal("the input digest stopped before the header write"))?;
            input_digest::check_extent(digest, walked_size, input)
        };
        build_from_schedules(
            pass1_5::Schedules {
                relations: relation_schedule,
                nodes: node_schedule,
                ways: way_schedule,
                max_node_id,
                file: shared_file,
                file_size: walked_size,
            },
            finish_digest,
            (repl_seq, repl_ts),
            start_time,
            config,
            output_dir,
            scratch,
            host,
            stage_b,
            validation,
        )
    })
}

/// Passes 1 to 3 and the header, from the schedule walk's output.
/// `finish_digest` waits for the input digest; it is called once, just
/// before the header is written.
#[allow(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::too_many_arguments
)]
fn build_from_schedules(
    schedules: pass1_5::Schedules,
    finish_digest: impl FnOnce() -> Result<input_digest::InputDigest>,
    (repl_seq, repl_ts): (Option<i64>, Option<i64>),
    start_time: std::time::Instant,
    config: &BuildConfig,
    output_dir: &std::path::Path,
    scratch: &crate::scratch::RunScratch,
    host: &crate::host::Entered<'_>,
    stage_b: pass3::StageB,
    validation: InputValidation,
) -> Result<BuildStats> {
    let pass1_5::Schedules {
        relations: relation_schedule,
        nodes: node_schedule,
        ways: way_schedule,
        max_node_id,
        file: shared_file,
        file_size: _,
    } = schedules;
    let mut strings = StringPool::new();

    // -----------------------------------------------------------------------
    // Pass 1: Relations - collect admin boundary metadata + way member IDs
    // (Runs before Pass 1.5 so it knows which admin member ways to keep.)
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

    let (admin_relations, needed_admin_ways) = pass1::run_pass1(
        &relation_schedule,
        &shared_file,
        &mut strings,
        host,
        validation,
    )?;
    drop(relation_schedule);
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
    // Pass 1.5: Way extraction + referenced node collection
    //
    // The one read of the way blobs: geocode-relevant ways (streets,
    // building addresses, interpolation, admin members) go to the scratch
    // way store Pass 2b resolves from, and the node IDs they reference are
    // collected. The coordinate store in pass 2 only holds entries for these
    // nodes, so its size follows the referenced subset rather than every
    // node in the file.
    // -----------------------------------------------------------------------
    host.invocation.marker("GEOCODE_PASS1_END");
    // Every pass boundary is a checkpoint; the passes check their own blob
    // reads, results and chunks in between.
    host.invocation.check()?;

    host.invocation.marker("GEOCODE_PASS1_5_START");
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.pass1_5",
        "Pass 1.5: Way extraction..."
    );

    let (referenced_nodes, way_store, admin_totals) = pass1_5::run_pass1_5(
        &way_schedule,
        max_node_id,
        &shared_file,
        &needed_admin_ways,
        scratch,
        host,
        validation,
    )?;
    drop(needed_admin_ways);
    host.invocation.marker("GEOCODE_PASS1_5_END");

    // -----------------------------------------------------------------------
    // Pass 2: Nodes, then ways
    //
    // The node blobs populate the coordinate store for referenced nodes and
    // write node address points; then the way store is resolved against it
    // (streets, buildings, interpolation, admin geometry).
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
    // footprint is in reference/performance/geocode.md; externalizing them needs a
    // coordinated streaming and external-merge design because later passes
    // consume each collection.
    host.invocation.marker("GEOCODE_PASS2_SCAN_START");
    let pass2::Pass2Output {
        addr_point_count,
        street_way_count,
        interp_ways,
        way_geom,
        street_ways,
        street_nodes,
        addr_points,
        interp_nodes,
    } = pass2::run_pass2(
        output_dir,
        &node_schedule,
        &way_schedule,
        &way_store,
        &shared_file,
        max_node_id,
        referenced_nodes,
        admin_totals,
        &mut strings,
        host,
        validation,
    )?;
    way_store.remove(scratch);
    drop(node_schedule);
    drop(way_schedule);
    // Pass 2 is the last pass to read the input; the digest worker may
    // still be reading its own description of it.
    drop(shared_file);
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
        admin::assemble_admin_polygons(&admin_relations, &way_geom, config, &host.executor)?;
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
    // The reader trusts the admin cover, which is exact only while a ring's
    // unwrapped frame stays within the bound. Assembly withholds every
    // relation with a ring whose lift winds or spans a full turn, so every
    // frame left lies within a turn and a half of zero; a polygon past the
    // bound is a broken invariant, refused before any admin record is
    // written.
    if let Some(index) = admin_polygons
        .iter()
        .position(|poly| !pass3::admin_frame_within_bound(poly))
    {
        return Err(Error::internal(format!(
            "admin polygon {index} has a ring frame past {} degrees, which ring \
             assembly withholds",
            crate::tunables::ADMIN_FRAME_MAX_ABS_LONGITUDE_DEG
        )));
    }
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
    if dropped_admin.ambiguous_topology > 0 {
        crate::host::diag!(
            host.diagnostics,
            Warning,
            "geocode.pass2.admin_ring_topology",
            "{} admin relations refused: their member ways do not determine their \
             rings (branching junction, repeated segment, self-touching ring, \
             ring on one line or nested lobes; counted among the dropped relations above)",
            dropped_admin.ambiguous_topology
        );
    }
    if dropped_admin.start_dependent_frame > 0 {
        crate::host::diag!(
            host.diagnostics,
            Warning,
            "geocode.pass2.admin_start_dependent_ring",
            "{} admin relations refused: a ring winds round a pole, spans a full \
             turn of longitude, or has an edge spanning exactly half a turn, \
             where its interior can depend on where its walk starts (counted \
             among the dropped relations above)",
            dropped_admin.start_dependent_frame
        );
    }
    host.invocation.marker("GEOCODE_PASS2_ADMIN_ASSEMBLY_END");
    host.invocation.check()?;
    // Deliberately a conjunction, not a disjunction. An admin-only index (a
    // boundaries extract with no addressable content) is legitimate; only an
    // index with nothing queryable at all is a build that should have failed.
    // The admin-only direction is pinned by the five admin query tests, which
    // all build from inputs with zero address points and zero streets.
    //
    // Staged interpolation ways are not a term, and the gate can therefore
    // run before resolution. An interpolation record takes its numbers only
    // from indexed address points on its own end nodes, so with no address
    // points no way resolves and none can answer;
    // with address points the gate already passes on them. Counting staged
    // interpolation ways here once let an input whose only content was an
    // unresolvable `addr:interpolation` way publish an index that answers
    // nothing.
    if addr_point_count == 0 && street_way_count == 0 && admin_polygons.is_empty() {
        return Err(Error::invalid_data(
            "geocode build found zero address points, streets and admin polygons; refusing to publish an empty index",
        ));
    }

    #[cfg(feature = "hotpath")]
    let pass2_ms = pass2_start.elapsed_ms();

    // -----------------------------------------------------------------------
    // Interpolation endpoint resolution (reads from mmap'd addr_points.bin)
    // -----------------------------------------------------------------------
    host.invocation.marker("GEOCODE_PASS2_INTERP_RESOLVE_START");
    strings.validate()?;
    let source_interp_ways = interp_ways.len();
    let interp::Resolution {
        records: interp_ways,
        resolved_ways,
        unresolved_ways,
        unresolved_ids,
    } = interp::resolve_interpolation_ways_mmap(
        interp_ways,
        addr_points.table(),
        interp_nodes.table(),
        &strings,
        &host.executor,
        &host.invocation,
    );
    // Resolution splits a way into one record per section, so the record
    // count is checked again before anything indexes records by u32.
    checked_count(interp_ways.len(), "interpolation records")?;
    host.invocation
        .counter("geocode_unresolved_interpolation_ways", unresolved_ways);
    if unresolved_ways > 0 {
        crate::host::diag!(
            host.diagnostics,
            Warning,
            "geocode.pass2.interp_unresolved",
            "{unresolved_ways} interpolation ways are unresolved (no single street or place at \
             both ends, a node with no address on its street or place at an end, coincident addresses that \
             disagree, a number that is not a positive decimal, equal numbers or the wrong \
             parity between address nodes, a missing node, or an unsupported \
             addr:interpolation value); way IDs (sample): {unresolved_ids:?}"
        );
    }
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.pass2.interp_resolved",
        "  {resolved_ways}/{source_interp_ways} interpolation ways resolved, {} records",
        interp_ways.len()
    );
    host.invocation.marker("GEOCODE_PASS2_INTERP_RESOLVE_END");
    host.invocation.check()?;

    host.invocation.marker("GEOCODE_PASS2_WRITE_START");
    interp::write_sections(output_dir, &interp_ways, interp_nodes.table())?;

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
    let pass3_start = crate::counters::Stopwatch::start();

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
            stage_b,
            cancel: &host.invocation,
        })?;
    host.invocation.marker("GEOCODE_PASS3_CELLS_END");
    host.invocation.marker("GEOCODE_PASS3_ADMIN_INDEX_START");
    let admin_count = admin::write_admin_index(output_dir, &mut { admin_cell_entries })?;
    host.invocation.marker("GEOCODE_PASS3_ADMIN_INDEX_END");
    // Pass 3 ends with the last cell file. The header write and the smoke
    // test below are not cell work, so they stay outside its span (and its
    // hotpath stopwatch); the smoke test, which reopens and validates every
    // written file, gets its own marker pair.
    #[cfg(feature = "hotpath")]
    let pass3_ms = pass3_start.elapsed_ms();
    host.invocation.marker("GEOCODE_PASS3_END");
    host.invocation.check()?;

    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.pass3.cells",
        "  {fine_count} fine cells, {coarse_count} coarse cells, {admin_count} admin cells"
    );

    // Write header, once the input digest is in.
    let digest = finish_digest()?;
    host.invocation.check()?;
    let header = Header {
        street_cell_level: config.street_level,
        coarse_cell_level: config.coarse_level,
        admin_cell_level: config.admin_level,
        admin_tolerance_m: config.admin_tolerance_m,
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
        input_xxh3_128: digest.xxh3_128,
        input_bytes: digest.bytes,
    };
    write_data_file(&output_dir.join(FILE_HEADER), &header.to_bytes())?;

    // The smoke test uses the record and string validation from
    // `Reader::open` on the files just written. Its work is part of the build wall
    // time; skipping it would leave written output unchecked by the reader
    // path.
    //
    // The semantic part checks one stated claim: address record 0 is listed
    // in its own fine and its own coarse cell, the cells Pass 3 filed it
    // under, each level judged on its own. It used to be a query at that
    // point that passed on any address or street and only warned on a miss,
    // so a broken address list could hide behind a nearby street and still
    // publish. A membership check needs no radius (a zero radius is valid
    // and would make a distance-based probe miss its own point) and no
    // ranking (a colocated address cannot shadow record 0). Every address
    // record gets one entry per level unconditionally, so no input can make
    // it miss: a miss is a builder bug and refuses publication. Public query
    // behaviour is the tests' to prove, not this check's.
    host.invocation.marker("GEOCODE_SMOKE_TEST_START");
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
        check_written_index(output_dir, &host.executor, first_addr_point, sl, cl)?
    };
    host.invocation.marker("GEOCODE_SMOKE_TEST_END");

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
            "pass3_cells_ms={pass3_ms}"
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

    Ok(stats)
}

fn checked_count(count: usize, kind: &str) -> Result<u32> {
    u32::try_from(count).map_err(|_| {
        Error::invalid_data(format!(
            "{kind} count exceeds u32::MAX; widen the on-disk count and bump FORMAT_VERSION"
        ))
    })
}

fn check_written_index(
    output_dir: &std::path::Path,
    executor: &crate::host::Executor,
    first_addr_point: Option<AddrPoint>,
    fine_level: u8,
    coarse_level: u8,
) -> Result<SmokeOutcome> {
    let membership = super::reader::Reader::check_written_index(output_dir, executor, || {
        first_addr_point.map(|point| {
            pass3::addr_point_cells(point.lat_e7, point.lon_e7, fine_level, coarse_level)
        })
    })?;
    let Some((fine, coarse)) = membership else {
        return Ok(SmokeOutcome::SkippedNoAddressPoints);
    };
    let point = first_addr_point
        .ok_or_else(|| Error::internal("smoke check tested a missing first address point"))?;
    let missing = match (fine, coarse) {
        (true, true) => return Ok(SmokeOutcome::Passed),
        (false, false) => "fine and coarse cells",
        (false, true) => "fine cell",
        (true, false) => "coarse cell",
    };
    Err(Error::internal(format!(
        "geocode smoke test: address point 0 at ({:.7}, {:.7}) is not listed in its \
         own {missing}; refusing to publish",
        e7_to_deg(f64::from(point.lat_e7)),
        e7_to_deg(f64::from(point.lon_e7)),
    )))
}

#[cfg(test)]
mod stage_b_tests {
    use std::path::Path;

    use crate::MemberId;
    use crate::block_builder::{BlockBuilder, HeaderBuilder, MemberData};
    use crate::host::Host;
    use crate::read::input::PinnedInput;
    use crate::scratch::RunScratch;
    use crate::writer::{Compression, PbfWriter};

    use super::*;

    fn write_block(writer: &mut PbfWriter<std::fs::File>, block: &mut BlockBuilder) {
        if let Some(bytes) = block.take().expect("encode fixture block") {
            writer
                .write_primitive_block(bytes)
                .expect("write fixture block");
        }
    }

    fn write_fixture(path: &Path, sites: i32, extra_addresses: i32) {
        // Several S2 faces, with coincident streets, addresses and
        // interpolation segments at each repeated site.
        let locations = [
            (557_000_000, 125_000_000),
            (-338_000_000, 1_512_000_000),
            (420_000_000, -710_000_000),
            (352_000_000, 1_390_000_000),
            (-341_000_000, -584_000_000),
        ];
        let file = std::fs::File::create(path).expect("create input");
        let mut writer = PbfWriter::new(file, Compression::default());
        let header = HeaderBuilder::new().sorted().build().expect("PBF header");
        writer.write_header(&header).expect("write PBF header");
        let mut block = BlockBuilder::new();
        for site in 0..sites {
            if site % 256 == 0 {
                write_block(&mut writer, &mut block);
            }
            let (lat, lon) =
                locations[usize::try_from(site).expect("nonnegative site") % locations.len()];
            let first = i64::from(site) * 4 + 1;
            block.add_node(
                first,
                lat,
                lon,
                [("addr:housenumber", "10"), ("addr:street", "Main Street")],
                None,
            );
            block.add_node(
                first + 1,
                lat,
                lon + 80_000,
                [("addr:housenumber", "14"), ("addr:street", "Main Street")],
                None,
            );
            block.add_node(first + 2, lat + 80_000, lon + 80_000, [], None);
            block.add_node(first + 3, lat + 80_000, lon, [], None);
        }
        for addr in 0..extra_addresses {
            if addr % 2048 == 0 {
                write_block(&mut writer, &mut block);
            }
            // This grid stays in one top-byte bucket at both levels, but
            // spreads addresses across cells within it.
            block.add_node(
                i64::from(sites) * 4 + i64::from(addr) + 1,
                120_000_000 + addr / 256 * 2_000,
                120_000_000 + addr % 256 * 2_000,
                [("addr:housenumber", "9"), ("addr:place", "West End")],
                None,
            );
        }
        write_block(&mut writer, &mut block);

        block.add_way(1, [], &[1, 2, 3, 4, 1], None);
        for site in 0..sites {
            if site % 256 == 0 {
                write_block(&mut writer, &mut block);
            }
            let first = i64::from(site) * 4 + 1;
            let way = i64::from(site) * 2 + 2;
            block.add_way(
                way,
                [("highway", "residential"), ("name", "Main Street")],
                &[first, first + 1],
                None,
            );
            block.add_way(
                way + 1,
                [
                    ("addr:interpolation", "even"),
                    ("addr:street", "Main Street"),
                ],
                &[first, first + 1],
                None,
            );
        }
        write_block(&mut writer, &mut block);
        block.add_relation(
            1,
            [
                ("type", "boundary"),
                ("boundary", "administrative"),
                ("admin_level", "8"),
                ("name", "Fixture"),
            ],
            &[MemberData {
                id: MemberId::Way(1),
                role: "outer",
            }],
            None,
        );
        write_block(&mut writer, &mut block);
        writer.flush().expect("flush input");
    }

    fn file_names(path: &Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = std::fs::read_dir(path)
            .expect("read index directory")
            .map(|entry| entry.expect("index directory entry").file_name())
            .collect();
        names.sort_unstable();
        names
    }

    fn generation(
        input: &Path,
        output: &Path,
        config: &BuildConfig,
        validation: InputValidation,
    ) -> Result<BuildStats> {
        let host = Host::for_tests();
        std::fs::create_dir(output)?;
        let pinned = PinnedInput::pin(input, &host)?;
        let parent = output
            .parent()
            .ok_or_else(|| Error::internal("generation has no parent directory"))?;
        let scratch = RunScratch::create(parent, &host)?;
        build_generation(
            &pinned,
            config,
            output,
            &host.entered(),
            &scratch,
            pass3::StageB::Parallel,
            validation,
        )
    }

    fn write_unindexed_block(path: &Path, bytes: &[u8]) {
        let file = std::fs::File::create(path).expect("create input");
        let mut writer = PbfWriter::new(file, Compression::default());
        let header = HeaderBuilder::new().sorted().build().expect("PBF header");
        writer.write_header(&header).expect("write PBF header");
        writer
            .write_primitive_block_no_indexdata(bytes)
            .expect("write raw block");
        writer.flush().expect("flush input");
    }

    #[test]
    fn scoped_validation_matches_full_validation_for_every_index_file() {
        use crate::read::wire_fixtures::{Group, block, dense, plain_node, way};

        let dir = crate::path_guard::tests::scratch();
        let indexed = dir.path().join("indexed.pbf");
        let unindexed = dir.path().join("unindexed.pbf");
        write_fixture(&indexed, 17, 0);
        // A mixed group exercises unindexed schedule discovery and the dense
        // before plain node ordering. Each has a duplicated address key.
        let keys = [
            "",
            "addr:housenumber",
            "3",
            "addr:street",
            "Main",
            "7",
            "name",
            "addr:interpolation",
            "all",
        ];
        let bytes = block(
            &keys,
            &[Group {
                dense: Some(dense(&[(1, 557_000_000, 125_000_000)], &[1, 5, 3, 4, 1, 2])),
                nodes: vec![plain_node(
                    2,
                    557_000_000,
                    125_001_000,
                    &[(1, 2), (6, 4), (1, 5), (3, 4)],
                    Some([1, 10, 20, 7, 0]),
                )],
                ways: vec![way(1, &[1, 2], (7, 8))],
                ..Group::default()
            }],
        );
        write_unindexed_block(&unindexed, &bytes);

        for (source, allow_unindexed) in [(&indexed, false), (&unindexed, true)] {
            let config = BuildConfig {
                allow_unindexed,
                ..BuildConfig::default()
            };
            let name = source.file_stem().expect("fixture name").to_string_lossy();
            let scoped = dir.path().join(format!("{name}-scoped"));
            let full = dir.path().join(format!("{name}-full"));
            let new = generation(source, &scoped, &config, InputValidation::Scoped)
                .expect("scoped build");
            let old =
                generation(source, &full, &config, InputValidation::Full).expect("full build");
            assert!(new.addr_points > 0 && new.interp_ways > 0);
            assert_eq!(new.addr_points, old.addr_points);
            assert_eq!(new.smoke_test, old.smoke_test);
            assert_eq!(file_names(&scoped), file_names(&full));
            for name in file_names(&full) {
                assert_eq!(
                    std::fs::read(scoped.join(&name)).expect("scoped file"),
                    std::fs::read(full.join(&name)).expect("full file"),
                    "{source:?} {name:?}",
                );
            }
        }
    }

    #[test]
    fn smoke_reports_the_serial_failure_for_a_broken_index() {
        use crate::geocode_index::reader::Reader;

        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("indexed.pbf");
        let index = dir.path().join("index");
        write_fixture(&input, 17, 0);
        let config = BuildConfig::default();
        generation(&input, &index, &config, InputValidation::Scoped).expect("build index");

        let path = index.join(FILE_GEO_CELLS);
        let mut bytes = std::fs::read(&path).expect("fine cells");
        let table = Table::<GeoCell>::new(&bytes);
        let mut first = table.get(0).expect("first cell");
        let mut second = table.get(1).expect("second cell");
        first.addr_offset = NO_DATA_U64 - 1;
        second.cell_id = first.cell_id;
        bytes[..GeoCell::SIZE].copy_from_slice(&first.to_bytes());
        bytes[GeoCell::SIZE..2 * GeoCell::SIZE].copy_from_slice(&second.to_bytes());
        std::fs::write(&path, bytes).expect("corrupt two cells");

        let serial = Reader::open(&index)
            .err()
            .expect("serial open failure")
            .report();
        assert!(
            serial.contains(&format!("{FILE_GEO_CELLS} record 0")),
            "{serial}"
        );
        assert!(serial.contains("address entry offset"), "{serial}");
        let executor = crate::host::Executor::for_tests();
        for _ in 0..4 {
            let parallel = check_written_index(
                &index,
                &executor,
                None,
                config.street_level,
                config.coarse_level,
            )
            .expect_err("smoke structural failure");
            assert_eq!(parallel.report(), serial);
        }
    }

    #[test]
    fn smoke_keeps_the_address_membership_error() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("indexed.pbf");
        let index = dir.path().join("index");
        write_fixture(&input, 1, 0);
        let config = BuildConfig::default();
        generation(&input, &index, &config, InputValidation::Scoped).expect("build index");
        let bytes = std::fs::read(index.join(FILE_ADDR_POINTS)).expect("address points");
        let point = Table::<AddrPoint>::new(&bytes)
            .get(0)
            .expect("first address");
        let (fine, _) = pass3::addr_point_cells(
            point.lat_e7,
            point.lon_e7,
            config.street_level,
            config.coarse_level,
        );
        let path = index.join(FILE_GEO_CELLS);
        let mut bytes = std::fs::read(&path).expect("fine cells");
        for record in bytes.as_chunks_mut::<{ GeoCell::SIZE }>().0 {
            let mut cell = GeoCell::read(record).expect("cell");
            if cell.cell_id == fine {
                cell.addr_offset = NO_DATA_U64;
                record.copy_from_slice(&cell.to_bytes());
            }
        }
        std::fs::write(&path, bytes).expect("remove fine address membership");
        crate::geocode_index::reader::Reader::open(&index).expect("structure is valid");
        let executor = crate::host::Executor::for_tests();
        let err = check_written_index(
            &index,
            &executor,
            Some(point),
            config.street_level,
            config.coarse_level,
        )
        .expect_err("smoke membership failure");
        let crate::ErrorKind::Internal { msg, .. } = err.kind() else {
            panic!("expected a smoke-test internal error, got {err}");
        };
        assert_eq!(
            msg.as_ref(),
            "geocode smoke test: address point 0 at (55.7000000, 12.5000000) is not listed in its own fine cell; refusing to publish",
        );
    }

    #[test]
    fn scoped_validation_keeps_exact_coordinate_and_role_refusals() {
        use crate::read::wire_fixtures::{
            Group, block, dense, field_bytes, packed, plain_node, relation,
        };

        let dir = crate::path_guard::tests::scratch();
        let config = BuildConfig {
            allow_unindexed: true,
            ..BuildConfig::default()
        };
        let mut bad_role = relation(1, &[(99, 1, 1)]);
        field_bytes(&mut bad_role, 2, &packed([1, 3, 5]));
        field_bytes(&mut bad_role, 3, &packed([2, 4, 6]));
        let fixtures = [
            (
                "dense-outside-i32",
                Group {
                    dense: Some(dense(&[(1, 4_294_967_296, 0)], &[])),
                    ..Group::default()
                },
                "lat 429496729600, lon 0 in nanodegrees",
            ),
            (
                "plain-outside-domain",
                Group {
                    nodes: vec![plain_node(1, 900_000_001, 0, &[], None)],
                    ..Group::default()
                },
                "lat 90000000100, lon 0 in nanodegrees",
            ),
            (
                "bad-role",
                Group {
                    relations: vec![bad_role],
                    ..Group::default()
                },
                "relation 1 member 0: role string index 99",
            ),
        ];
        for (name, group, message) in fixtures {
            let input = dir.path().join(format!("{name}.pbf"));
            let strings = [
                "",
                "boundary",
                "administrative",
                "admin_level",
                "8",
                "name",
                "Test",
            ];
            write_unindexed_block(&input, &block(&strings, &[group]));
            let error = |mode, suffix| {
                generation(
                    &input,
                    &dir.path().join(format!("{name}-{suffix}")),
                    &config,
                    mode,
                )
                .expect_err("fixture must be refused")
                .report()
            };
            let scoped = error(InputValidation::Scoped, "scoped");
            let full = error(InputValidation::Full, "full");
            assert!(full.contains(message), "{name}: {full}");
            assert_eq!(scoped, full, "{name}");
        }
    }

    #[test]
    fn parallel_stage_b_matches_serial_for_every_index_file() {
        for (sites, extra_addresses) in [(1, 0), (17, 0), (257, 74_000)] {
            let dir = crate::path_guard::tests::scratch();
            let input = dir.path().join("source.osm.pbf");
            let parallel = dir.path().join("parallel");
            let serial = dir.path().join("serial");
            write_fixture(&input, sites, extra_addresses);

            let config = BuildConfig::default();
            let host = Host::for_tests();
            let output = index_output(&parallel, crate::Overwrite::Refuse, &host.diagnostics)
                .expect("stage output");
            let stats = build_geocode_index(
                &input,
                &config,
                output,
                &host,
                RunScratch::create(dir.path(), &host).expect("parallel scratch"),
            )
            .expect("build parallel index");
            assert!(stats.addr_points > 0 && stats.street_ways > 0);
            assert!(stats.interp_ways > 0 && stats.admin_polygons > 0);

            std::fs::create_dir(&serial).expect("create oracle directory");
            let pinned = PinnedInput::pin(&input, &host).expect("pin oracle input");
            build_generation(
                &pinned,
                &config,
                &serial,
                &host.entered(),
                &RunScratch::create(dir.path(), &host).expect("serial scratch"),
                pass3::StageB::Serial,
                InputValidation::Scoped,
            )
            .expect("build serial index");

            let names = file_names(&serial);
            assert_eq!(file_names(&parallel), names);
            for name in names {
                let actual = std::fs::read(parallel.join(&name)).expect("parallel file");
                let expected = std::fs::read(serial.join(&name)).expect("serial file");
                assert!(
                    actual == expected,
                    "{name:?} differs at {sites} sites and {extra_addresses} extra addresses"
                );
            }
        }
    }
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
        crate::test_support::assert_err_kind!(
            super::checked_count(u32::MAX as usize + 1, "test"),
            crate::ErrorKind::InvalidData { msg, .. } if msg.contains("count exceeds u32::MAX"),
        );
    }

    #[test]
    fn rejects_invalid_levels_and_radii() {
        assert!(
            BuildConfig::default().validate().is_ok(),
            "the untouched defaults must validate"
        );
        let cases = [
            (
                BuildConfig {
                    street_level: 31,
                    ..BuildConfig::default()
                },
                "street S2 level must be in",
            ),
            (
                BuildConfig {
                    coarse_level: BuildConfig::default().street_level,
                    ..BuildConfig::default()
                },
                "must be less than street S2 level",
            ),
            (
                BuildConfig {
                    fine_search_radius_m: f32::NAN,
                    ..BuildConfig::default()
                },
                "search radius must be a finite non-negative number",
            ),
            (
                BuildConfig {
                    fine_search_radius_m: -1.0,
                    ..BuildConfig::default()
                },
                "search radius must be a finite non-negative number",
            ),
            (
                BuildConfig {
                    coarse_search_radius_m: f32::INFINITY,
                    ..BuildConfig::default()
                },
                "coarse search radius must be a finite non-negative number",
            ),
            // An explicit 1 km radius exceeds the reach at street level 19.
            (
                BuildConfig {
                    street_level: 19,
                    fine_search_radius_m: 1000.0,
                    ..BuildConfig::default()
                },
                "too wide for S2 level 19",
            ),
        ];
        for (config, refusal) in cases {
            let err = config.validate().expect_err(refusal);
            assert!(err.to_string().contains(refusal), "{err}");
        }
    }
}
