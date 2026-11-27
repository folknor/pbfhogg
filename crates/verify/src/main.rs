//! Cross-validate the pbfhogg CLI against reference tools.
//!
//! Run as `brokkr run --with pbfhogg pbfhogg-verify -- CHECK --dataset D`:
//! brokkr builds the CLI, holds the machine lock for the whole run, and
//! hands the executable in `BROKKR_WITH_EXE`. This program never calls
//! brokkr, links no pbfhogg, and runs a private copy of the executable it
//! was handed. `reference/verify.md` is the contract.
mod all;
mod counts;
mod diff_summary;
mod extract_config;
mod osc;
mod pbf_frames;
mod verify_change_listing;
mod verify_diff_detail;
mod verify_diff_opl;
mod verify_element_diff;
mod verify_extract_simple;
mod verify_extsort;
mod verify_fileinfo;
mod verify_getid_refs;
mod verify_input_order;
mod verify_opl;
mod verify_polygon;
mod verify_reencode;
mod verify_signs;

mod argv;
mod checks;
mod error;
mod harness;
mod opl_compare;
mod output;
mod process;
mod producer;
mod reference;
mod reference_cache;
mod run;
mod selection;
mod stream;
#[cfg(test)]
mod test_support;
mod tunables;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args as ClapArgs, Parser, Subcommand};
use pbfhogg_devkit::{Config, user_config_path};

use crate::error::Error;
use crate::harness::{CheckStatus, ENVIRONMENT_EXIT_CODE, Harness};

#[derive(Parser)]
#[command(
    name = "pbfhogg-verify",
    about = "Cross-validate the pbfhogg CLI against osmium"
)]
struct Args {
    /// pbfhogg CLI executable (otherwise BROKKR_WITH_EXE). Never builds one.
    #[arg(long, global = true)]
    pbfhogg: Option<PathBuf>,
    #[arg(long, global = true, default_value = "brokkr.toml")]
    config: PathBuf,
    /// Config host section (otherwise the kernel's hostname).
    #[arg(long, global = true)]
    host: Option<String>,
    /// Stream every check's detail instead of replaying it only on failure.
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Keep the run directory even when everything passed.
    #[arg(long, global = true)]
    keep: bool,
    /// Execute reference tools even when a completed execution is cached.
    #[arg(long, global = true)]
    fresh_reference: bool,
    /// Maximum logical artifact bytes retained after a run; zero clears entries.
    #[arg(long, global = true, default_value_t = tunables::REFERENCE_BUDGET)]
    reference_budget: u64,
    #[command(subcommand)]
    check: Check,
}

/// Where a check's input PBF comes from.
#[derive(ClapArgs, Debug, Clone)]
struct Source {
    /// Dataset: denmark or germany (larger regions are bench-only).
    #[arg(long, default_value = "denmark")]
    dataset: String,
    #[arg(long, default_value = "indexed")]
    variant: String,
    /// Named snapshot of the dataset (`base` is the primary).
    #[arg(long, conflicts_with = "input")]
    snapshot: Option<String>,
    /// A fixture PBF instead of a dataset, relative to the project root.
    #[arg(long)]
    input: Option<PathBuf>,
}

#[derive(ClapArgs, Debug, Clone)]
struct PbfArgs {
    #[command(flatten)]
    source: Source,
    /// Pass --direct-io to the pbfhogg commands that take it.
    #[arg(long)]
    direct_io: bool,
}

#[derive(ClapArgs, Debug, Clone)]
struct SuiteArgs {
    #[command(flatten)]
    pbf: PbfArgs,
    #[command(flatten)]
    selections: selection::Selections,
    #[arg(long, allow_hyphen_values = true)]
    bbox: Option<pbfhogg_devkit::Bbox>,
}

#[derive(ClapArgs)]
struct ApplyArgs {
    #[command(flatten)]
    pbf: PbfArgs,
    #[command(flatten)]
    selections: selection::Selections,
    #[arg(long)]
    locations_on_ways: bool,
}

#[derive(ClapArgs)]
struct DegradeArgs {
    #[command(flatten)]
    pbf: PbfArgs,
    /// Require inline way coordinates in the input (fail without them).
    #[arg(long)]
    locations_on_ways: bool,
}

#[derive(ClapArgs)]
struct ExtractArgs {
    #[command(flatten)]
    pbf: PbfArgs,
    #[arg(long, allow_hyphen_values = true)]
    bbox: Option<pbfhogg_devkit::Bbox>,
}
#[derive(ClapArgs)]
struct MultiExtractArgs {
    #[command(flatten)]
    extract: ExtractArgs,
    #[arg(long, default_value_t = tunables::MULTI_EXTRACT_REGIONS)]
    regions: usize,
}

use checks::add_locations::AltwMode;

#[derive(ClapArgs)]
struct AltwArgs {
    #[command(flatten)]
    pbf: PbfArgs,
    #[arg(long, value_enum, default_value = "all")]
    mode: AltwMode,
    /// Refused: injected output is incompatible with the reference tool.
    #[arg(long)]
    inject_prepass: bool,
}

#[derive(ClapArgs)]
struct TagsArgs {
    #[command(flatten)]
    pbf: PbfArgs,
    #[arg(long, value_parser = ["pbf", "osc"], default_value = "pbf")]
    input_kind: String,
    #[command(flatten)]
    selections: selection::Selections,
}

#[derive(ClapArgs)]
struct MergeArgs {
    #[command(flatten)]
    source: Source,
    #[arg(long, conflicts_with_all = ["osc", "osc_range"])]
    osc_seq: Option<u64>,
    #[arg(long, conflicts_with = "osc_range")]
    osc: Vec<PathBuf>,
    #[arg(long)]
    osc_range: Option<String>,
}

#[derive(ClapArgs)]
struct DiffArgs {
    #[command(flatten)]
    pbf: PbfArgs,
    #[arg(long, value_parser = ["text", "osc"], default_value = "text")]
    format: String,
    #[arg(long, conflicts_with = "osc")]
    osc_seq: Option<u64>,
    #[arg(long)]
    osc: Option<PathBuf>,
}

/// A check that builds its own input: no dataset, only the I/O choice.
#[derive(ClapArgs)]
struct FixtureArgs {
    #[arg(long)]
    direct_io: bool,
}

#[derive(ClapArgs)]
struct RenumberArgs {
    #[command(flatten)]
    pbf: PbfArgs,
    #[arg(long, allow_hyphen_values = true)]
    start_id: Option<String>,
}

#[derive(Subcommand)]
enum Check {
    /// The element comparison's field policies held to one-field alterations of a real slice
    FieldPolicies(Source),
    /// pbfhogg sort vs osmium sort
    Sort(PbfArgs),
    Diff(DiffArgs),
    Renumber(RenumberArgs),
    Cat(PbfArgs),
    /// pbfhogg cat --dedupe held to its input (self-merge, subset, disjoint union)
    CatDedupe(PbfArgs),
    ApplyChanges(ApplyArgs),
    #[command(name = "check-ids")]
    Ids(PbfArgs),
    Repack(PbfArgs),
    Degrade(DegradeArgs),
    TagsFilter(TagsArgs),
    /// pbfhogg inspect tags vs osmium tags-count
    InspectTags(PbfArgs),
    /// Check geocode-query address, street, interpolation and admin answers against an input model
    Geocode(PbfArgs),
    /// The geocode check on a fixed fixture across the antimeridian and past 85 degrees of latitude
    GeocodeSeam(FixtureArgs),
    MergeChanges(MergeArgs),
    Getparents(PbfArgs),
    #[command(alias = "getid")]
    GetidRemoveid(PbfArgs),
    Extract(ExtractArgs),
    MultiExtract(MultiExtractArgs),
    #[command(name = "check-refs")]
    Refs(Source),
    AddLocationsToWays(AltwArgs),
    /// Remove all completed reference-cache entries (never live staging).
    CleanReferences,
    /// Every check in suite order
    All(SuiteArgs),
}

/// Configuration and environment access stays at this executable boundary.
struct Boundary {
    config: Config,
    host: String,
    pbfhogg: PathBuf,
    tools: reference::Tools,
}

fn load_config(args: &Args) -> Result<Config, Error> {
    let override_path = std::env::var_os("BROKKR_USER_CONFIG");
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");
    let user = user_config_path(override_path.as_deref(), xdg.as_deref(), home.as_deref());
    let config = Config::load(&args.config, user.as_deref())?;
    Ok(config)
}

fn boundary(args: &Args) -> Result<Boundary, Error> {
    let config = load_config(args)?;
    let with_exe = std::env::var_os("BROKKR_WITH_EXE");
    let path = std::env::var_os("PATH");
    let host = match &args.host {
        Some(host) => host.clone(),
        None => hostname()?,
    };
    let pbfhogg = args
        .pbfhogg
        .clone()
        .or_else(|| with_exe.map(PathBuf::from))
        .ok_or_else(|| {
            Error::tool("supply --pbfhogg PATH or BROKKR_WITH_EXE (brokkr run --with pbfhogg)")
        })?;
    let pbfhogg = executable(&pbfhogg)?;
    let osmium = path
        .as_deref()
        .and_then(|p| find_in_path("osmium", p))
        .ok_or_else(|| Error::tool("'osmium' not found in PATH (the reference tool)"))?;
    Ok(Boundary {
        config,
        host,
        pbfhogg,
        tools: reference::Tools { osmium },
    })
}

fn find_in_path(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| executable(candidate).is_ok())
}

fn hostname() -> Result<String, Error> {
    let mut name = [0_u8; 256];
    // SAFETY: writable buffer with its correct capacity.
    if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } == -1 {
        return Err(Error::tool(format!(
            "hostname: {}",
            std::io::Error::last_os_error()
        )));
    }
    let end = name
        .iter()
        .position(|b| *b == 0)
        .ok_or_else(|| Error::tool("hostname is too long"))?;
    String::from_utf8(name[..end].to_vec()).map_err(|e| Error::tool(format!("hostname: {e}")))
}

fn executable(path: &Path) -> Result<PathBuf, Error> {
    let path =
        std::fs::canonicalize(path).map_err(|e| Error::tool(format!("{}: {e}", path.display())))?;
    let meta = std::fs::metadata(&path)?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(Error::tool(format!(
            "{} is not an executable file",
            path.display()
        )));
    }
    Ok(path)
}

/// The datasets real-data verification runs on. Project policy keyed on the
/// dataset name, the identity the rule is stated in; a region's size class
/// is never inferred from its bytes. Planet and continents are bench-only.
///
/// Decided by the maintainer: the list stays a name list, with no size
/// limit and no other home for the policy (a per-dataset key in the host
/// config was rejected because brokkr refuses unknown dataset keys).
/// Germany is the scale upper bound, and on germany real runs surfaced
/// problems denmark did not: the osmium sort reference was OOM-killed, OPL
/// decodings filled scratch, and renumber met a far larger orphan volume.
/// More regions are not expected; a new one is a one-line change here.
const ELIGIBLE_DATASETS: &[&str] = &["denmark", "germany"];

/// Refuse a dataset-backed input from a dataset outside
/// [`ELIGIBLE_DATASETS`]. Every dataset-resolved input (PBF, snapshot, OSC,
/// an alternate base) passes through here; only an explicit fixture path
/// does not.
fn eligible(dataset: &str) -> Result<(), Error> {
    if ELIGIBLE_DATASETS.contains(&dataset) {
        return Ok(());
    }
    Err(Error::tool(format!(
        "dataset '{dataset}' is not verified on real data (only {}); pass a fixture with \
         --input instead",
        ELIGIBLE_DATASETS.join(", ")
    )))
}

/// The input PBF: a fixture named with `--input`, or an eligible dataset.
/// The resolver's observed digest is dropped on purpose: no stamp brackets
/// it against the one `Run::watch` takes later, so it could describe an
/// earlier version of the file, and the run's stamp-checked digest memo
/// already hashes the input only once.
fn resolve_input(b: &Boundary, source: &Source) -> Result<PathBuf, Error> {
    if let Some(path) = &source.input {
        return selection::fixture(&b.config, path);
    }
    eligible(&source.dataset)?;
    Ok(b.config
        .resolve_input(
            &b.host,
            &source.dataset,
            source.snapshot.as_deref(),
            "input",
            &source.variant,
        )?
        .path)
}

/// What a run ended as, for the exit code.
enum Ending {
    Counts(all::Counts),
    Single(CheckStatus),
}

fn execute(args: &Args, mut life: process::Lifecycle) -> Result<ExitCode, Error> {
    life.checkpoint()?;
    if matches!(args.check, Check::CleanReferences) {
        let config = load_config(args)?;
        let host = args.host.clone().map(Ok).unwrap_or_else(hostname)?;
        let cache = reference_cache::ReferenceCache::new(
            &config.scratch_root(&host)?,
            String::new(),
            false,
        )?;
        life.checkpoint()?;
        cache.retain(Some(0));
        return Ok(ExitCode::SUCCESS);
    }
    let b = boundary(args)?;
    let scratch = b.config.scratch_root(&b.host)?;
    let run = run::Run::create(&scratch, &b.pbfhogg, args.keep)?;
    output::note(&format!("run directory: {}", run.dir.display()));
    output::note(&format!(
        "pbfhogg under test: {} (xxh3-128 {})",
        b.pbfhogg.display(),
        run.pbfhogg_digest
    ));
    let cache = reference_cache::ReferenceCache::new(
        &scratch,
        run.dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        args.fresh_reference,
    )?;
    let mut h = Harness::new(life, run, b.tools.clone(), args.verbose, cache);
    let ending = dispatch(&b, &mut h, &args.check);
    let passed = matches!(
        ending,
        Ok(Ending::Single(CheckStatus::Passed))
            | Ok(Ending::Counts(all::Counts {
                failed: 0,
                environment: 0,
                ..
            }))
    );
    // Nothing is removed once a stop signal is pending.
    let ending = ending.and_then(|ending| h.checkpoint().map(|()| ending));
    if ending.is_ok() {
        h.retain_references(args.reference_budget)?;
        // Kept failure evidence is a link of its own; the store's copies go.
        h.run.decodings.discard();
    }
    h.run.finish(passed && ending.is_ok());
    Ok(match ending? {
        Ending::Single(CheckStatus::Passed) => ExitCode::SUCCESS,
        Ending::Single(CheckStatus::Failed) => ExitCode::FAILURE,
        Ending::Single(CheckStatus::Environment) => ExitCode::from(ENVIRONMENT_EXIT_CODE),
        Ending::Counts(c) if c.failed > 0 => ExitCode::FAILURE,
        Ending::Counts(c) if c.environment > 0 => ExitCode::from(ENVIRONMENT_EXIT_CODE),
        Ending::Counts(_) => ExitCode::SUCCESS,
    })
}

#[expect(
    clippy::too_many_lines,
    reason = "one arm per check; the arms that need more live in their own dispatch functions"
)]
fn dispatch(b: &Boundary, h: &mut Harness, check: &Check) -> Result<Ending, Error> {
    match check {
        Check::CleanReferences => Err(Error::tool(
            "clean-references is handled before check dispatch",
        )),
        Check::Diff(args) => dispatch_diff(b, h, args),
        Check::Renumber(args) => {
            let input = resolve_input(b, &args.pbf.source)?;
            h.run.watch(&input)?;
            Ok(Ending::Single(h.run_check("renumber", |h| {
                checks::renumber::run(
                    h,
                    &input,
                    "input",
                    args.start_id.as_deref(),
                    true,
                    args.pbf.direct_io,
                )
            })?))
        }
        Check::TagsFilter(args) => dispatch_tags(b, h, args),
        Check::GeocodeSeam(args) => seam(h, args.direct_io),
        Check::MergeChanges(args) => dispatch_merge(b, h, args),
        Check::ApplyChanges(args) => dispatch_apply(b, h, args),
        Check::Sort(pbf) => {
            let input = resolve_input(b, &pbf.source)?;
            h.run.watch(&input)?;
            let direct_io = pbf.direct_io;
            Ok(Ending::Single(h.run_check("sort", |h| {
                checks::sort::run(h, &input, direct_io)
            })?))
        }
        Check::Cat(pbf)
        | Check::CatDedupe(pbf)
        | Check::InspectTags(pbf)
        | Check::Geocode(pbf)
        | Check::Getparents(pbf)
        | Check::GetidRemoveid(pbf)
        | Check::Ids(pbf)
        | Check::Repack(pbf)
        | Check::Degrade(DegradeArgs { pbf, .. }) => {
            let input = resolve_input(b, &pbf.source)?;
            h.run.watch(&input)?;
            let (name, run) = read_check(check);
            Ok(Ending::Single(
                h.run_check(name, |h| run(h, &input, pbf.direct_io))?,
            ))
        }
        Check::Extract(args) => {
            let input = resolve_input(b, &args.pbf.source)?;
            h.run.watch(&input)?;
            let (bbox, polygons) = extract_regions(b, h, &args.pbf.source, args.bbox.as_ref())?;
            if bbox.is_none() && polygons.is_empty() {
                return Err(Error::tool("extract needs a bbox or dataset polygon"));
            }
            Ok(Ending::Single(h.run_check("extract", |h| {
                checks::extract::run(h, &input, bbox.as_ref(), &polygons, args.pbf.direct_io)
            })?))
        }
        Check::MultiExtract(args) => {
            let pbf = &args.extract.pbf;
            let input = resolve_input(b, &pbf.source)?;
            h.run.watch(&input)?;
            let (bbox, polygons) = extract_regions(b, h, &pbf.source, args.extract.bbox.as_ref())?;
            let bbox = bbox.ok_or_else(|| Error::tool("multi-extract needs a bbox"))?;
            if args.regions == 0 {
                return Err(Error::tool("multi-extract requires at least 1 region"));
            }
            Ok(Ending::Single(h.run_check("multi-extract", |h| {
                checks::multi_extract::run(h, &input, &bbox, &polygons, args.regions, pbf.direct_io)
            })?))
        }
        Check::FieldPolicies(source) => {
            let input = resolve_input(b, source)?;
            h.run.watch(&input)?;
            Ok(Ending::Single(h.run_check("field-policies", |h| {
                checks::field_policies::run(h, &input, false)
            })?))
        }
        Check::Refs(source) => {
            let input = resolve_input(b, source)?;
            h.run.watch(&input)?;
            Ok(Ending::Single(h.run_check("check-refs", |h| {
                checks::check_refs::run(h, &input)
            })?))
        }
        Check::AddLocationsToWays(args) => {
            if args.inject_prepass {
                return Err(Error::tool(
                    "--inject-prepass cannot be verified: enriched output is osmium-incompatible; run flag-off verification for element semantics",
                ));
            }
            let input = resolve_input(b, &args.pbf.source)?;
            h.run.watch(&input)?;
            Ok(Ending::Single(
                h.run_check("add-locations-to-ways", |h| {
                    checks::add_locations::run(h, &input, args.mode, args.pbf.direct_io)
                })?,
            ))
        }
        Check::All(suite) => dispatch_all(b, h, suite),
    }
}

/// The geocode check on its own seam fixture: no input to resolve.
fn seam(h: &mut Harness, direct_io: bool) -> Result<Ending, Error> {
    Ok(Ending::Single(h.run_check("geocode-seam", |h| {
        checks::geocode::seam::run(h, direct_io)
    })?))
}

/// The name and entry point of a check that reads one PBF.
fn read_check(check: &Check) -> (&'static str, checks::ReadCheck) {
    match check {
        Check::Cat(_) => ("cat", checks::cat::run),
        Check::CatDedupe(_) => ("cat --dedupe", checks::cat_dedupe::run),
        Check::InspectTags(_) => ("inspect-tags", checks::inspect_tags::run),
        Check::Geocode(_) => ("geocode", checks::geocode::run),
        Check::Ids(_) => ("check-ids", checks::check_ids::run),
        Check::Repack(_) => ("repack", checks::repack::run),
        Check::Degrade(DegradeArgs {
            locations_on_ways: true,
            ..
        }) => ("degrade -l", checks::degrade::run_locations),
        Check::Degrade(_) => ("degrade", checks::degrade::run),
        Check::Getparents(_) => ("getparents", checks::getparents::run),
        _ => ("getid-removeid", checks::getid_removeid::run),
    }
}

fn dispatch_diff(b: &Boundary, h: &mut Harness, args: &DiffArgs) -> Result<Ending, Error> {
    let input = resolve_input(b, &args.pbf.source)?;
    h.run.watch(&input)?;
    let selection = selection::Selection {
        config: &b.config,
        host: &b.host,
        source: &args.pbf.source,
    };
    let osc = selection
        .osc(&selection::Selections {
            osc_seq: args.osc_seq,
            osc: args.osc.clone(),
        })?
        .ok_or_else(|| Error::tool("diff needs a configured OSC or --osc"))?;
    h.run.watch(&osc)?;
    let (name, run): (&str, checks::PairCheck) = if args.format == "osc" {
        ("diff --format osc", checks::derive_changes::run)
    } else {
        ("diff", checks::diff::run)
    };
    Ok(Ending::Single(h.run_check(name, |h| {
        run(h, &input, &osc, args.pbf.direct_io)
    })?))
}

fn dispatch_all(b: &Boundary, h: &mut Harness, suite: &SuiteArgs) -> Result<Ending, Error> {
    let pbf = &suite.pbf;
    let input = resolve_input(b, &pbf.source)?;
    h.run.watch(&input)?;
    Ok(Ending::Counts({
        let (bbox, polygons) = extract_regions(b, h, &pbf.source, suite.bbox.as_ref())?;
        let selection = selection::Selection {
            config: &b.config,
            host: &b.host,
            source: &pbf.source,
        };
        let osc = selection.osc(&suite.selections)?;
        let locations = selection.locations_base()?;
        if let Some(osc) = &osc {
            h.run.watch(osc)?;
        }
        if let Some(base) = &locations {
            h.run.watch(&base.path)?;
        }
        all::run(
            h,
            &input,
            pbf.direct_io,
            bbox.as_ref(),
            &polygons,
            osc.as_deref(),
            locations
                .as_ref()
                .map(|i| i.path.as_path())
                .ok_or(if pbf.source.input.is_some() {
                    "fixture PBF has no keep-untagged base"
                } else {
                    "no altw-keep-untagged base configured"
                }),
        )?
    }))
}

fn dispatch_apply(b: &Boundary, h: &mut Harness, args: &ApplyArgs) -> Result<Ending, Error> {
    let input = resolve_input(b, &args.pbf.source)?;
    h.run.watch(&input)?;
    let selection = selection::Selection {
        config: &b.config,
        host: &b.host,
        source: &args.pbf.source,
    };
    let osc = selection
        .osc(&args.selections)?
        .ok_or_else(|| Error::tool("apply-changes needs a configured OSC or --osc"))?;
    h.run.watch(&osc)?;
    Ok(Ending::Single(h.run_check("apply-changes", |h| {
        checks::apply_changes::run(h, &input, &osc, args.pbf.direct_io, args.locations_on_ways)
    })?))
}

fn extract_regions(
    b: &Boundary,
    h: &mut Harness,
    source: &Source,
    bbox: Option<&pbfhogg_devkit::Bbox>,
) -> Result<
    (
        Option<pbfhogg_devkit::Bbox>,
        Vec<extract_config::PolygonInput>,
    ),
    Error,
> {
    let selection = selection::Selection {
        config: &b.config,
        host: &b.host,
        source,
    };
    let bbox = selection.bbox(bbox)?;
    let polygons = selection
        .polygons()?
        .unwrap_or_default()
        .into_iter()
        .map(|i| extract_config::PolygonInput {
            name: i.variant,
            path: i.path,
        })
        .collect::<Vec<_>>();
    for polygon in &polygons {
        h.run.watch(&polygon.path)?;
    }
    Ok((bbox, polygons))
}

fn main() -> ExitCode {
    // First, before anything could start a thread.
    let life = match process::Lifecycle::install() {
        Ok(life) => life,
        Err(e) => {
            output::error(&format!("cannot own child processes: {e}"));
            return ExitCode::from(2);
        }
    };
    let args = Args::parse();
    match execute(&args, life) {
        Ok(code) => code,
        Err(Error::Interrupted) => ExitCode::from(130),
        Err(e) => {
            output::error(&e.to_string());
            ExitCode::from(2)
        }
    }
}

fn dispatch_tags(b: &Boundary, h: &mut Harness, args: &TagsArgs) -> Result<Ending, Error> {
    if args.input_kind == "osc" {
        if args.pbf.direct_io || args.pbf.source.input.is_some() {
            return Err(Error::tool(
                "OSC tags-filter uses --osc and refuses --direct-io/--input",
            ));
        }
        let osc = selection::Selection {
            config: &b.config,
            host: &b.host,
            source: &args.pbf.source,
        }
        .osc(&args.selections)?
        .ok_or_else(|| Error::tool("tags-filter needs an OSC"))?;
        h.run.watch(&osc)?;
        Ok(Ending::Single(
            h.run_check("tags-filter --input-kind osc", |h| {
                checks::tags_filter_osc::run(h, &osc)
            })?,
        ))
    } else {
        if args.selections.osc.is_some() || args.selections.osc_seq.is_some() {
            return Err(Error::tool("OSC selection requires --input-kind osc"));
        }
        let input = resolve_input(b, &args.pbf.source)?;
        h.run.watch(&input)?;
        Ok(Ending::Single(h.run_check("tags-filter", |h| {
            checks::tags_filter::run(h, &input, args.pbf.direct_io)
        })?))
    }
}

fn dispatch_merge(b: &Boundary, h: &mut Harness, args: &MergeArgs) -> Result<Ending, Error> {
    if args.source.input.is_some() {
        return Err(Error::tool("merge-changes uses --osc, not --input"));
    }
    let selection = selection::Selection {
        config: &b.config,
        host: &b.host,
        source: &args.source,
    };
    let oscs = if !args.osc.is_empty() {
        args.osc
            .iter()
            .map(|p| selection::fixture(&b.config, p))
            .collect::<Result<Vec<_>, _>>()?
    } else if let Some(range) = &args.osc_range {
        selection.osc_range(range)?
    } else {
        selection
            .osc(&selection::Selections {
                osc_seq: args.osc_seq,
                ..Default::default()
            })?
            .into_iter()
            .collect()
    };
    if oscs.is_empty() {
        return Err(Error::tool("merge-changes needs an OSC"));
    }
    for osc in &oscs {
        h.run.watch(osc)?;
    }
    Ok(Ending::Single(h.run_check("merge-changes", |h| {
        checks::merge_changes::run(h, &oscs)
    })?))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_parses_checks() {
        Args::command().debug_assert();
        for name in [
            "field-policies",
            "cat",
            "cat-dedupe",
            "diff",
            "renumber",
            "apply-changes",
            "check-ids",
            "repack",
            "degrade",
            "tags-filter",
            "inspect-tags",
            "geocode",
            "getparents",
            "getid-removeid",
            "getid",
            "extract",
            "multi-extract",
            "check-refs",
            "add-locations-to-ways",
        ] {
            assert!(Args::try_parse_from(["x", name, "--input", "fixture.pbf"]).is_ok());
        }
        // A check that builds its own input takes none.
        assert!(Args::try_parse_from(["x", "geocode-seam", "--direct-io"]).is_ok());
        crate::test_support::refused(
            Args::try_parse_from(["x", "geocode-seam", "--input", "fixture.pbf"]),
            "unexpected argument '--input' found",
        );
        for mode in ["sparse", "external", "all"] {
            assert!(Args::try_parse_from(["x", "add-locations-to-ways", "--mode", mode]).is_ok());
        }
        crate::test_support::refused(
            Args::try_parse_from(["x", "add-locations-to-ways", "--mode", "dense"]),
            "invalid value 'dense' for '--mode <MODE>'",
        );
        crate::test_support::refused(
            Args::try_parse_from(["x", "check-refs", "--direct-io"]),
            "unexpected argument '--direct-io' found",
        );
        assert!(
            matches!(Args::try_parse_from(["x", "degrade", "--locations-on-ways"]).unwrap().check,
            Check::Degrade(a) if a.locations_on_ways)
        );
        assert!(
            matches!(Args::try_parse_from(["x", "add-locations-to-ways", "--inject-prepass"]).unwrap().check,
            Check::AddLocationsToWays(a) if a.inject_prepass)
        );

        let clean =
            Args::try_parse_from(["x", "clean-references", "--reference-budget", "0"]).unwrap();
        assert!(matches!(clean.check, Check::CleanReferences));
        let fresh = Args::try_parse_from(["x", "sort", "--fresh-reference"]).unwrap();
        assert!(fresh.fresh_reference);
        let parsed =
            Args::try_parse_from(["x", "sort", "--dataset", "germany", "--direct-io"]).unwrap();
        assert!(
            matches!(&parsed.check, Check::Sort(p) if p.direct_io && p.source.dataset == "germany")
        );
        let selections =
            Args::try_parse_from(["x", "all", "--osc-seq", "42", "--bbox=-1,2,3,4"]).unwrap();
        assert!(
            matches!(selections.check, Check::All(s) if s.selections.osc_seq == Some(42) && s.bbox.is_some())
        );
        crate::test_support::refused(
            Args::try_parse_from(["x", "all", "--osc", "fixture.osc", "--osc-seq", "42"]),
            "'--osc <OSC>' cannot be used with '--osc-seq <OSC_SEQ>'",
        );
        crate::test_support::refused(
            Args::try_parse_from(["x", "all", "--bbox=1,2,NaN,4"]),
            "bbox component 2 must be finite",
        );
        let parsed = Args::try_parse_from(["x", "all"]).unwrap();
        assert!(
            matches!(&parsed.check, Check::All(p) if p.pbf.source.dataset == "denmark" && p.pbf.source.variant == "indexed")
        );
        crate::test_support::refused(
            Args::try_parse_from(["x", "sort", "--input", "a.pbf", "--snapshot", "s"]),
            "'--input <INPUT>' cannot be used with '--snapshot <SNAPSHOT>'",
        );
    }

    #[test]
    fn cli_parses_diff_family() {
        for format in ["text", "osc"] {
            assert!(
                Args::try_parse_from([
                    "x",
                    "diff",
                    "--format",
                    format,
                    "--input",
                    "old.pbf",
                    "--osc",
                    "change.osc",
                    "--direct-io"
                ])
                .is_ok()
            );
        }
        crate::test_support::refused(
            Args::try_parse_from(["x", "diff", "--format", "json"]),
            "invalid value 'json' for '--format <FORMAT>'",
        );
        crate::test_support::refused(
            Args::try_parse_from(["x", "diff", "--osc", "a", "--osc-seq", "1"]),
            "'--osc <OSC>' cannot be used with '--osc-seq <OSC_SEQ>'",
        );
        crate::test_support::refused(
            Args::try_parse_from(["x", "diff", "--bbox=1,2,3,4"]),
            "unexpected argument '--bbox' found",
        );
        for name in ["apply-changes", "tags-filter"] {
            crate::test_support::refused(
                Args::try_parse_from(["x", name, "--bbox=1,2,3,4"]),
                "unexpected argument '--bbox' found",
            );
        }
        let parsed =
            Args::try_parse_from(["x", "renumber", "--start-id", "10,20,30", "--direct-io"])
                .unwrap();
        assert!(
            matches!(parsed.check, Check::Renumber(a) if a.start_id.as_deref() == Some("10,20,30") && a.pbf.direct_io)
        );
    }

    #[test]
    fn cli_parses_change_checks() {
        for args in [
            vec!["x", "merge-changes", "--osc", "a.osc", "--osc", "b.osc"],
            vec!["x", "merge-changes", "--osc-range", "10..12"],
            vec!["x", "tags-filter", "--input-kind", "osc", "--osc-seq", "10"],
        ] {
            assert!(Args::try_parse_from(args).is_ok());
        }
        for (args, why) in [
            (
                vec!["x", "merge-changes", "--osc", "a", "--osc-range", "1..2"],
                "'--osc <OSC>' cannot be used with '--osc-range <OSC_RANGE>'",
            ),
            (
                vec![
                    "x",
                    "merge-changes",
                    "--osc-seq",
                    "1",
                    "--osc-range",
                    "1..2",
                ],
                "'--osc-seq <OSC_SEQ>' cannot be used with '--osc-range <OSC_RANGE>'",
            ),
            (
                vec!["x", "merge-changes", "--direct-io"],
                "unexpected argument '--direct-io' found",
            ),
        ] {
            crate::test_support::refused(Args::try_parse_from(args), why);
        }
    }

    #[test]
    fn only_denmark_and_germany_are_verified_on_real_data() {
        assert!(eligible("denmark").is_ok());
        assert!(eligible("germany").is_ok());
        for name in ["planet", "europe", "north-america", "malta", ""] {
            crate::test_support::refused(eligible(name), "is not verified on real data");
        }
    }

    #[test]
    fn tools_are_found_only_as_absolute_executables() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = scratch.path().join("bin");
        std::fs::create_dir(&dir).unwrap();
        let osmium = dir.join("osmium");
        std::fs::write(&osmium, b"x").unwrap();
        let path = std::env::join_paths(["relative", dir.to_str().unwrap()]).unwrap();
        assert!(find_in_path("osmium", &path).is_none());
        std::fs::set_permissions(&osmium, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(find_in_path("osmium", &path).is_some());
    }
}
