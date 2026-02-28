mod bench_all;
mod bench_allocator;
mod bench_blob_filter;
mod bench_commands;
mod bench_extract;
mod bench_merge;
mod bench_planetiler;
mod bench_read;
mod bench_write;
mod build;
mod config;
mod db;
mod download;
mod env;
mod error;
mod git;
mod harness;
mod hotpath;
mod lockfile;
mod output;
#[allow(dead_code)]
mod preflight;
mod profile;
mod tools;
mod verify;
mod verify_add_locations;
mod verify_all;
mod verify_cat;
mod verify_check_refs;
mod verify_derive_changes;
mod verify_diff;
mod verify_extract;
mod verify_getid_removeid;
mod verify_merge;
mod verify_sort;
mod verify_tags_filter;

use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand};

use error::DevError;

#[derive(Parser)]
#[command(name = "dev", about = "pbfhogg development tooling")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run clippy + tests
    Check {
        /// Extra arguments passed to cargo test
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show environment information
    Env,
    /// Build and run the pbfhogg CLI
    Run {
        /// Arguments passed to the pbfhogg binary
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run benchmarks
    Bench {
        #[command(subcommand)]
        bench: BenchCommand,
    },
    /// Cross-validate pbfhogg output against reference tools
    Verify {
        #[command(subcommand)]
        verify: VerifyCommand,
    },
    /// Query benchmark results
    Results {
        /// Show results for a specific commit (prefix match)
        #[arg(long)]
        commit: Option<String>,

        /// Compare two commits side-by-side
        #[arg(long, num_args = 2, value_names = ["COMMIT_A", "COMMIT_B"])]
        compare: Option<Vec<String>>,

        /// Filter by command name (e.g. "bench read", "bench merge")
        #[arg(long)]
        command: Option<String>,

        /// Filter by variant (e.g. "buffered+zlib", "pipelined")
        #[arg(long)]
        variant: Option<String>,

        /// Maximum number of results to show
        #[arg(long, short = 'n', default_value = "20")]
        limit: usize,
    },
    /// Run hotpath profiling (timing or allocation instrumentation)
    Hotpath {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,

        /// Explicit OSC diff file path (overrides --dataset)
        #[arg(long)]
        osc: Option<String>,

        /// Run allocation profiling instead of timing
        #[arg(long)]
        alloc: bool,

        /// Number of runs (default: 1 for profiling)
        #[arg(long, default_value = "1")]
        runs: usize,
    },
    /// Run two-pass profiling (timing + allocation) for a dataset
    Profile {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,

        /// Explicit OSC diff file path (overrides --dataset)
        #[arg(long)]
        osc: Option<String>,
    },
    /// Download a region dataset from Geofabrik
    Download {
        /// Region name (malta, greater-london, switzerland, norway, japan, denmark, germany, north-america)
        region: String,

        /// URL for the OSC diff file
        #[arg(long)]
        osc_url: Option<String>,
    },
    /// Clean build artifacts and scratch data
    Clean,
}

#[derive(Subcommand)]
enum BenchCommand {
    /// Benchmark PBF read performance (5 reader modes)
    Read {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,

        /// Number of runs, best-of-N (default: 3)
        #[arg(long, default_value = "3")]
        runs: usize,

        /// Comma-separated list of modes (default: all)
        #[arg(long)]
        modes: Option<String>,
    },
    /// Benchmark PBF write performance (sync + pipelined x compression)
    Write {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,

        /// Number of runs, best-of-N (default: 3)
        #[arg(long, default_value = "3")]
        runs: usize,

        /// Comma-separated compression modes (default: none,zlib:6,zstd:3)
        #[arg(long, default_value = "none,zlib:6,zstd:3")]
        compression: String,
    },
    /// Benchmark merge performance (I/O modes x compression)
    Merge {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,

        /// Explicit OSC diff file path (overrides --dataset)
        #[arg(long)]
        osc: Option<String>,

        /// Number of runs, best-of-N (default: 3)
        #[arg(long, default_value = "3")]
        runs: usize,

        /// Enable io_uring variants (with preflight checks)
        #[arg(long)]
        uring: bool,

        /// Comma-separated compression modes (default: zlib,none)
        #[arg(long, default_value = "zlib,none")]
        compression: String,
    },
    /// Benchmark CLI commands (external timing)
    Commands {
        /// Command to benchmark (or "all" for full suite)
        #[arg(default_value = "all")]
        command: String,

        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,

        /// Number of runs, best-of-N (default: 3)
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// Benchmark extract strategies (simple/complete/smart)
    Extract {
        /// Dataset name from dev.toml (default: japan)
        #[arg(long, default_value = "japan")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,

        /// Number of runs, best-of-N (default: 3)
        #[arg(long, default_value = "3")]
        runs: usize,

        /// Bounding box (left,bottom,right,top)
        #[arg(long)]
        bbox: Option<String>,

        /// Comma-separated strategies (default: simple,complete,smart)
        #[arg(long, default_value = "simple,complete,smart")]
        strategies: String,
    },
    /// Benchmark allocators (default/jemalloc/mimalloc) via check-refs
    Allocator {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,

        /// Number of runs, best-of-N (default: 3)
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// Benchmark indexed vs non-indexed PBF performance
    BlobFilter {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit indexed PBF path (overrides --dataset)
        #[arg(long)]
        pbf_indexed: Option<String>,

        /// Explicit non-indexed PBF path (overrides --dataset)
        #[arg(long)]
        pbf_raw: Option<String>,

        /// Number of runs, best-of-N (default: 3)
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// Benchmark Planetiler Java PBF read performance
    Planetiler {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,

        /// Number of runs, best-of-N (default: 3)
        #[arg(long, default_value = "3")]
        runs: usize,
    },
    /// Run full benchmark suite (read + write + merge + commands + baselines)
    All {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,

        /// Number of runs, best-of-N (default: 3)
        #[arg(long, default_value = "3")]
        runs: usize,
    },
}

#[derive(Subcommand)]
enum VerifyCommand {
    /// Cross-validate merge against osmium/osmosis/osmconvert
    Merge {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,

        /// Explicit OSC diff file path (overrides --dataset)
        #[arg(long)]
        osc: Option<String>,
    },
    /// Cross-validate sort against osmium sort
    Sort {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,
    },
    /// Cross-validate cat (type filters) against osmium cat
    Cat {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,
    },
    /// Cross-validate extract (bbox strategies) against osmium extract
    Extract {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,

        /// Bounding box (left,bottom,right,top)
        #[arg(long)]
        bbox: Option<String>,
    },
    /// Cross-validate derive-changes roundtrip against osmium
    DeriveChanges {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,

        /// Explicit OSC diff file path (overrides --dataset)
        #[arg(long)]
        osc: Option<String>,
    },
    /// Cross-validate diff summary against osmium diff
    Diff {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,

        /// Explicit OSC diff file path (overrides --dataset)
        #[arg(long)]
        osc: Option<String>,
    },
    /// Cross-validate add-locations-to-ways against osmium
    AddLocationsToWays {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,
    },
    /// Cross-validate tags-filter against osmium tags-filter
    TagsFilter {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,
    },
    /// Cross-validate getid/removeid against osmium getid
    GetidRemoveid {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,
    },
    /// Cross-validate check-refs against osmium check-refs
    CheckRefs {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,
    },
    /// Run all verify commands sequentially
    All {
        /// Dataset name from dev.toml (default: denmark)
        #[arg(long, default_value = "denmark")]
        dataset: String,

        /// Explicit PBF file path (overrides --dataset)
        #[arg(long)]
        pbf: Option<String>,

        /// Explicit OSC diff file path (overrides --dataset)
        #[arg(long)]
        osc: Option<String>,

        /// Bounding box (left,bottom,right,top)
        #[arg(long)]
        bbox: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Bench { bench } => cmd_bench(bench),
        Command::Verify { verify } => cmd_verify(verify),
        Command::Check { args } => cmd_check(&args),
        Command::Env => cmd_env(),
        Command::Run { args } => cmd_run(&args),
        Command::Results {
            commit,
            compare,
            command,
            variant,
            limit,
        } => cmd_results(commit, compare, command, variant, limit),
        Command::Hotpath {
            dataset,
            pbf,
            osc,
            alloc,
            runs,
        } => cmd_hotpath(dataset, pbf, osc, alloc, runs),
        Command::Profile { dataset, pbf, osc } => cmd_profile(dataset, pbf, osc),
        Command::Download { region, osc_url } => cmd_download(region, osc_url),
        Command::Clean => cmd_clean(),
    };
    if let Err(e) = result {
        output::error(&e.to_string());
        process::exit(1);
    }
}

fn cmd_check(extra_args: &[String]) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;

    run_clippy(&ws.workspace_root)?;
    run_tests(&ws.workspace_root, extra_args)?;

    output::result_msg("check passed");
    Ok(())
}

fn run_clippy(workspace_root: &std::path::Path) -> Result<(), DevError> {
    output::run_msg("cargo clippy --all-targets");

    let captured = output::run_captured(
        "cargo",
        &["clippy", "--all-targets"],
        workspace_root,
    )?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        output::error(&stderr);
        return Err(DevError::Build("clippy failed".into()));
    }

    Ok(())
}

fn run_tests(
    workspace_root: &std::path::Path,
    extra_args: &[String],
) -> Result<(), DevError> {
    let mut args = vec!["test"];
    let extra_refs: Vec<&str> = extra_args.iter().map(String::as_str).collect();
    args.extend_from_slice(&extra_refs);

    output::run_msg(&format!("cargo {}", args.join(" ")));

    let captured = output::run_captured("cargo", &args, workspace_root)?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        output::error(&stderr);
        return Err(DevError::Build("tests failed".into()));
    }

    Ok(())
}

fn cmd_env() -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(
        &dev_config,
        &hostname,
        &ws.workspace_root,
        &ws.target_dir,
    );

    let info = env::collect(&dev_config, &paths);
    env::print(&info);
    Ok(())
}

fn cmd_results(
    commit: Option<String>,
    compare: Option<Vec<String>>,
    command: Option<String>,
    variant: Option<String>,
    limit: usize,
) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let db_path = ws.workspace_root.join("dev/results.db");

    if !db_path.exists() {
        output::result_msg("no results yet (run a benchmark first)");
        return Ok(());
    }

    let results_db = db::ResultsDb::open(&db_path)?;

    if let Some(commits) = compare {
        let commit_a = commits.first().map_or("", String::as_str);
        let commit_b = commits.get(1).map_or("", String::as_str);
        let (rows_a, rows_b) = results_db.query_compare(commit_a, commit_b)?;
        let table = db::format_compare(commit_a, &rows_a, commit_b, &rows_b);
        println!("{table}");
    } else {
        let filter = db::QueryFilter {
            commit,
            command,
            variant,
            limit,
        };
        let rows = results_db.query(&filter)?;
        if rows.is_empty() {
            output::result_msg("no matching results");
        } else {
            let table = db::format_table(&rows);
            println!("{table}");
        }
    }

    Ok(())
}

fn cmd_bench(bench: BenchCommand) -> Result<(), DevError> {
    match bench {
        BenchCommand::Read {
            dataset,
            pbf,
            runs,
            modes,
        } => cmd_bench_read(dataset, pbf, runs, modes),
        BenchCommand::Write {
            dataset,
            pbf,
            runs,
            compression,
        } => cmd_bench_write(dataset, pbf, runs, compression),
        BenchCommand::Merge {
            dataset,
            pbf,
            osc,
            runs,
            uring,
            compression,
        } => cmd_bench_merge(dataset, pbf, osc, runs, uring, compression),
        BenchCommand::Commands {
            command,
            dataset,
            pbf,
            runs,
        } => cmd_bench_commands(command, dataset, pbf, runs),
        BenchCommand::Extract {
            dataset,
            pbf,
            runs,
            bbox,
            strategies,
        } => cmd_bench_extract(dataset, pbf, runs, bbox, strategies),
        BenchCommand::Allocator {
            dataset,
            pbf,
            runs,
        } => cmd_bench_allocator(dataset, pbf, runs),
        BenchCommand::BlobFilter {
            dataset,
            pbf_indexed,
            pbf_raw,
            runs,
        } => cmd_bench_blob_filter(dataset, pbf_indexed, pbf_raw, runs),
        BenchCommand::Planetiler {
            dataset,
            pbf,
            runs,
        } => cmd_bench_planetiler(dataset, pbf, runs),
        BenchCommand::All {
            dataset,
            pbf,
            runs,
        } => cmd_bench_all(dataset, pbf, runs),
    }
}

fn cmd_bench_read(
    dataset: String,
    pbf: Option<String>,
    runs: usize,
    modes_str: Option<String>,
) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(
        &dev_config,
        &hostname,
        &ws.workspace_root,
        &ws.target_dir,
    );

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;

    let modes = match modes_str {
        Some(ref s) => bench_read::parse_modes(s)?,
        None => bench_read::ALL_MODES.to_vec(),
    };

    let file_mb = file_size_mb(&pbf_path);

    let harness = harness::BenchHarness::new(&dev_config, &paths, &ws.workspace_root)?;
    bench_read::run(&harness, &pbf_path, file_mb, runs, &modes)
}

fn cmd_bench_write(
    dataset: String,
    pbf: Option<String>,
    runs: usize,
    compression_str: String,
) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(
        &dev_config,
        &hostname,
        &ws.workspace_root,
        &ws.target_dir,
    );

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let compressions = bench_write::parse_compressions(&compression_str)?;
    let file_mb = file_size_mb(&pbf_path);

    let harness = harness::BenchHarness::new(&dev_config, &paths, &ws.workspace_root)?;
    bench_write::run(&harness, &pbf_path, file_mb, runs, &compressions)
}

fn cmd_bench_merge(
    dataset: String,
    pbf: Option<String>,
    osc: Option<String>,
    runs: usize,
    uring: bool,
    compression_str: String,
) -> Result<(), DevError> {
    if uring {
        bench_merge::check_uring_preflight()?;
    }

    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(
        &dev_config,
        &hostname,
        &ws.workspace_root,
        &ws.target_dir,
    );

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let osc_path = resolve_osc_path(&osc, &dataset, &dev_config, &paths)?;
    let compressions = bench_merge::parse_compressions(&compression_str)?;
    let file_mb = file_size_mb(&pbf_path);

    let harness = harness::BenchHarness::new(&dev_config, &paths, &ws.workspace_root)?;
    bench_merge::run(
        &harness,
        &pbf_path,
        &osc_path,
        file_mb,
        runs,
        &compressions,
        uring,
        &paths.scratch_dir,
    )
}

fn cmd_bench_commands(
    command: String,
    dataset: String,
    pbf: Option<String>,
    runs: usize,
) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(
        &dev_config,
        &hostname,
        &ws.workspace_root,
        &ws.target_dir,
    );

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let commands = bench_commands::parse_command(&command)?;
    let file_mb = file_size_mb(&pbf_path);

    let binary = build::cargo_build(
        &build::BuildConfig::release_cli(),
        &ws.workspace_root,
    )?;

    let harness = harness::BenchHarness::new(&dev_config, &paths, &ws.workspace_root)?;
    bench_commands::run(
        &harness,
        &binary,
        &pbf_path,
        file_mb,
        runs,
        &commands,
        &ws.workspace_root,
    )
}

fn cmd_bench_extract(
    dataset: String,
    pbf: Option<String>,
    runs: usize,
    bbox: Option<String>,
    strategies_str: String,
) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(
        &dev_config,
        &hostname,
        &ws.workspace_root,
        &ws.target_dir,
    );

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let bbox = resolve_bbox(&bbox, &dataset, &dev_config)?;
    let strategies = bench_extract::parse_strategies(&strategies_str)?;
    let file_mb = file_size_mb(&pbf_path);

    let binary = build::cargo_build(
        &build::BuildConfig::release_cli(),
        &ws.workspace_root,
    )?;

    let harness = harness::BenchHarness::new(&dev_config, &paths, &ws.workspace_root)?;
    bench_extract::run(
        &harness,
        &binary,
        &pbf_path,
        file_mb,
        runs,
        &bbox,
        &strategies,
        &ws.workspace_root,
    )
}

fn cmd_bench_allocator(
    dataset: String,
    pbf: Option<String>,
    runs: usize,
) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(
        &dev_config,
        &hostname,
        &ws.workspace_root,
        &ws.target_dir,
    );

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let file_mb = file_size_mb(&pbf_path);

    let harness = harness::BenchHarness::new(&dev_config, &paths, &ws.workspace_root)?;
    bench_allocator::run(&harness, &pbf_path, file_mb, runs, &ws.workspace_root)
}

fn cmd_bench_blob_filter(
    dataset: String,
    pbf_indexed: Option<String>,
    pbf_raw: Option<String>,
    runs: usize,
) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(
        &dev_config,
        &hostname,
        &ws.workspace_root,
        &ws.target_dir,
    );

    let indexed_path = resolve_pbf_path(&pbf_indexed, &dataset, &dev_config, &paths)?;
    let raw_path = resolve_raw_pbf_path(&pbf_raw, &dataset, &dev_config, &paths)?;
    let file_mb = file_size_mb(&indexed_path);

    let binary = build::cargo_build(
        &build::BuildConfig::release_cli(),
        &ws.workspace_root,
    )?;

    let harness = harness::BenchHarness::new(&dev_config, &paths, &ws.workspace_root)?;
    bench_blob_filter::run(
        &harness,
        &binary,
        &indexed_path,
        &raw_path,
        file_mb,
        runs,
        &ws.workspace_root,
    )
}

fn cmd_bench_planetiler(
    dataset: String,
    pbf: Option<String>,
    runs: usize,
) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(
        &dev_config,
        &hostname,
        &ws.workspace_root,
        &ws.target_dir,
    );

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let file_mb = file_size_mb(&pbf_path);

    let harness = harness::BenchHarness::new(&dev_config, &paths, &ws.workspace_root)?;
    bench_planetiler::run(
        &harness,
        &pbf_path,
        file_mb,
        runs,
        &paths.data_dir,
        &ws.workspace_root,
    )
}

fn cmd_bench_all(
    dataset: String,
    pbf: Option<String>,
    runs: usize,
) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(
        &dev_config,
        &hostname,
        &ws.workspace_root,
        &ws.target_dir,
    );

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let file_mb = file_size_mb(&pbf_path);

    let harness = harness::BenchHarness::new(&dev_config, &paths, &ws.workspace_root)?;
    bench_all::run(
        &harness,
        &dev_config,
        &paths,
        &ws.workspace_root,
        &pbf_path,
        file_mb,
        runs,
        &dataset,
    )
}

fn cmd_verify(verify: VerifyCommand) -> Result<(), DevError> {
    match verify {
        VerifyCommand::Merge { dataset, pbf, osc } => cmd_verify_merge(dataset, pbf, osc),
        VerifyCommand::Sort { dataset, pbf } => cmd_verify_sort(dataset, pbf),
        VerifyCommand::Cat { dataset, pbf } => cmd_verify_cat(dataset, pbf),
        VerifyCommand::Extract { dataset, pbf, bbox } => cmd_verify_extract(dataset, pbf, bbox),
        VerifyCommand::DeriveChanges { dataset, pbf, osc } => {
            cmd_verify_derive_changes(dataset, pbf, osc)
        }
        VerifyCommand::Diff { dataset, pbf, osc } => cmd_verify_diff(dataset, pbf, osc),
        VerifyCommand::AddLocationsToWays { dataset, pbf } => {
            cmd_verify_add_locations(dataset, pbf)
        }
        VerifyCommand::TagsFilter { dataset, pbf } => cmd_verify_tags_filter(dataset, pbf),
        VerifyCommand::GetidRemoveid { dataset, pbf } => {
            cmd_verify_getid_removeid(dataset, pbf)
        }
        VerifyCommand::CheckRefs { dataset, pbf } => cmd_verify_check_refs(dataset, pbf),
        VerifyCommand::All {
            dataset,
            pbf,
            osc,
            bbox,
        } => cmd_verify_all(dataset, pbf, osc, bbox),
    }
}

fn cmd_verify_merge(
    dataset: String,
    pbf: Option<String>,
    osc: Option<String>,
) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(&dev_config, &hostname, &ws.workspace_root, &ws.target_dir);

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let osc_path = resolve_osc_path(&osc, &dataset, &dev_config, &paths)?;

    let osmosis = match tools::ensure_osmosis(&paths.data_dir, &ws.workspace_root) {
        Ok(tools) => Some(tools),
        Err(e) => {
            output::verify_msg(&format!("osmosis not available (non-fatal): {e}"));
            None
        }
    };

    let harness = verify::VerifyHarness::new(&paths, &ws.workspace_root, &ws.target_dir)?;
    verify_merge::run(&harness, &pbf_path, &osc_path, osmosis.as_ref())
}

fn cmd_verify_sort(dataset: String, pbf: Option<String>) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(&dev_config, &hostname, &ws.workspace_root, &ws.target_dir);

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;

    let harness = verify::VerifyHarness::new(&paths, &ws.workspace_root, &ws.target_dir)?;
    verify_sort::run(&harness, &pbf_path)
}

fn cmd_verify_cat(dataset: String, pbf: Option<String>) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(&dev_config, &hostname, &ws.workspace_root, &ws.target_dir);

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;

    let harness = verify::VerifyHarness::new(&paths, &ws.workspace_root, &ws.target_dir)?;
    verify_cat::run(&harness, &pbf_path)
}

fn cmd_verify_extract(
    dataset: String,
    pbf: Option<String>,
    bbox: Option<String>,
) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(&dev_config, &hostname, &ws.workspace_root, &ws.target_dir);

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let bbox = resolve_bbox(&bbox, &dataset, &dev_config)?;

    let harness = verify::VerifyHarness::new(&paths, &ws.workspace_root, &ws.target_dir)?;
    verify_extract::run(&harness, &pbf_path, &bbox)
}

fn cmd_verify_derive_changes(
    dataset: String,
    pbf: Option<String>,
    osc: Option<String>,
) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(&dev_config, &hostname, &ws.workspace_root, &ws.target_dir);

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let osc_path = resolve_osc_path(&osc, &dataset, &dev_config, &paths)?;

    let harness = verify::VerifyHarness::new(&paths, &ws.workspace_root, &ws.target_dir)?;
    verify_derive_changes::run(&harness, &pbf_path, &osc_path)
}

fn cmd_verify_diff(
    dataset: String,
    pbf: Option<String>,
    osc: Option<String>,
) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(&dev_config, &hostname, &ws.workspace_root, &ws.target_dir);

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let osc_path = resolve_osc_path(&osc, &dataset, &dev_config, &paths)?;

    let harness = verify::VerifyHarness::new(&paths, &ws.workspace_root, &ws.target_dir)?;
    verify_diff::run(&harness, &pbf_path, &osc_path)
}

fn cmd_verify_add_locations(dataset: String, pbf: Option<String>) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(&dev_config, &hostname, &ws.workspace_root, &ws.target_dir);

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;

    let harness = verify::VerifyHarness::new(&paths, &ws.workspace_root, &ws.target_dir)?;
    verify_add_locations::run(&harness, &pbf_path)
}

fn cmd_verify_tags_filter(dataset: String, pbf: Option<String>) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(&dev_config, &hostname, &ws.workspace_root, &ws.target_dir);

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;

    let harness = verify::VerifyHarness::new(&paths, &ws.workspace_root, &ws.target_dir)?;
    verify_tags_filter::run(&harness, &pbf_path)
}

fn cmd_verify_getid_removeid(dataset: String, pbf: Option<String>) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(&dev_config, &hostname, &ws.workspace_root, &ws.target_dir);

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;

    let harness = verify::VerifyHarness::new(&paths, &ws.workspace_root, &ws.target_dir)?;
    verify_getid_removeid::run(&harness, &pbf_path)
}

fn cmd_verify_check_refs(dataset: String, pbf: Option<String>) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(&dev_config, &hostname, &ws.workspace_root, &ws.target_dir);

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;

    let harness = verify::VerifyHarness::new(&paths, &ws.workspace_root, &ws.target_dir)?;
    verify_check_refs::run(&harness, &pbf_path)
}

fn cmd_verify_all(
    dataset: String,
    pbf: Option<String>,
    osc: Option<String>,
    bbox: Option<String>,
) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(&dev_config, &hostname, &ws.workspace_root, &ws.target_dir);

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;

    // OSC is optional for verify all — commands that need it are skipped.
    let osc_path = resolve_osc_path(&osc, &dataset, &dev_config, &paths).ok();

    // bbox is optional — extract is skipped if not available.
    let bbox_str = match bbox {
        Some(b) => Some(b),
        None => resolve_bbox(&None, &dataset, &dev_config).ok(),
    };

    let harness = verify::VerifyHarness::new(&paths, &ws.workspace_root, &ws.target_dir)?;
    verify_all::run(
        &harness,
        &pbf_path,
        osc_path.as_deref(),
        bbox_str.as_deref(),
        &paths.data_dir,
        &ws.workspace_root,
    )
}

/// Resolve the PBF path from --pbf or --dataset.
fn resolve_pbf_path(
    pbf: &Option<String>,
    dataset: &str,
    config: &config::DevConfig,
    paths: &config::ResolvedPaths,
) -> Result<PathBuf, DevError> {
    let path = match pbf {
        Some(p) => PathBuf::from(p),
        None => {
            let ds = config.datasets.get(dataset).ok_or_else(|| {
                DevError::Config(format!("unknown dataset: {dataset}"))
            })?;
            paths.data_dir.join(&ds.pbf)
        }
    };

    if !path.exists() {
        return Err(DevError::Config(format!(
            "PBF file not found: {}",
            path.display()
        )));
    }

    Ok(path)
}

/// Resolve the OSC path from --osc or --dataset.
fn resolve_osc_path(
    osc: &Option<String>,
    dataset: &str,
    config: &config::DevConfig,
    paths: &config::ResolvedPaths,
) -> Result<PathBuf, DevError> {
    let path = match osc {
        Some(p) => PathBuf::from(p),
        None => {
            let ds = config.datasets.get(dataset).ok_or_else(|| {
                DevError::Config(format!("unknown dataset: {dataset}"))
            })?;
            let osc_file = ds.osc.as_ref().ok_or_else(|| {
                DevError::Config(format!(
                    "dataset '{dataset}' has no osc file configured"
                ))
            })?;
            paths.data_dir.join(osc_file)
        }
    };

    if !path.exists() {
        return Err(DevError::Config(format!(
            "OSC file not found: {}",
            path.display()
        )));
    }

    Ok(path)
}

/// Resolve the bbox from --bbox or dataset config.
fn resolve_bbox(
    bbox: &Option<String>,
    dataset: &str,
    config: &config::DevConfig,
) -> Result<String, DevError> {
    if let Some(b) = bbox {
        return Ok(b.clone());
    }

    let ds = config.datasets.get(dataset).ok_or_else(|| {
        DevError::Config(format!("unknown dataset: {dataset}"))
    })?;

    ds.bbox.clone().ok_or_else(|| {
        DevError::Config(format!(
            "dataset '{dataset}' has no bbox configured (use --bbox)"
        ))
    })
}

/// Resolve the non-indexed (raw) PBF path from --pbf-raw or dataset config.
fn resolve_raw_pbf_path(
    pbf_raw: &Option<String>,
    dataset: &str,
    config: &config::DevConfig,
    paths: &config::ResolvedPaths,
) -> Result<PathBuf, DevError> {
    let path = match pbf_raw {
        Some(p) => PathBuf::from(p),
        None => {
            let ds = config.datasets.get(dataset).ok_or_else(|| {
                DevError::Config(format!("unknown dataset: {dataset}"))
            })?;
            let raw_file = ds.pbf_raw.as_ref().ok_or_else(|| {
                DevError::Config(format!(
                    "dataset '{dataset}' has no pbf_raw configured (use --pbf-raw)"
                ))
            })?;
            paths.data_dir.join(raw_file)
        }
    };

    if !path.exists() {
        return Err(DevError::Config(format!(
            "raw PBF file not found: {}",
            path.display()
        )));
    }

    Ok(path)
}

/// Get file size in MB (decimal, consistent with bench scripts).
fn file_size_mb(path: &std::path::Path) -> f64 {
    std::fs::metadata(path)
        .map(|m| m.len() as f64 / 1_000_000.0)
        .unwrap_or(0.0)
}

fn cmd_run(args: &[String]) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let binary = build::cargo_build(
        &build::BuildConfig::release_cli(),
        &ws.workspace_root,
    )?;

    output::run_msg(&format!(
        "{} {}",
        binary.display(),
        args.join(" "),
    ));

    let code = output::run_passthrough(&binary, args)?;
    if code != 0 {
        process::exit(code);
    }
    Ok(())
}

fn cmd_hotpath(
    dataset: String,
    pbf: Option<String>,
    osc: Option<String>,
    alloc: bool,
    runs: usize,
) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(
        &dev_config,
        &hostname,
        &ws.workspace_root,
        &ws.target_dir,
    );

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let osc_path = resolve_osc_path(&osc, &dataset, &dev_config, &paths)?;
    let pbf_raw_path = resolve_raw_pbf_path(&None, &dataset, &dev_config, &paths).ok();
    let file_mb = file_size_mb(&pbf_path);

    let feature = if alloc { "hotpath-alloc" } else { "hotpath" };
    let binary = build::cargo_build(
        &build::BuildConfig::release_cli_with_features(&[feature]),
        &ws.workspace_root,
    )?;

    let harness = harness::BenchHarness::new(&dev_config, &paths, &ws.workspace_root)?;
    hotpath::run(
        &harness,
        &binary,
        &pbf_path,
        pbf_raw_path.as_deref(),
        &osc_path,
        file_mb,
        runs,
        alloc,
        &paths.scratch_dir,
        &ws.workspace_root,
    )
}

fn cmd_profile(
    dataset: String,
    pbf: Option<String>,
    osc: Option<String>,
) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(
        &dev_config,
        &hostname,
        &ws.workspace_root,
        &ws.target_dir,
    );

    let pbf_path = resolve_pbf_path(&pbf, &dataset, &dev_config, &paths)?;
    let osc_path = resolve_osc_path(&osc, &dataset, &dev_config, &paths)?;
    let pbf_raw_path = resolve_raw_pbf_path(&None, &dataset, &dev_config, &paths).ok();
    let file_mb = file_size_mb(&pbf_path);

    profile::run(
        &pbf_path,
        pbf_raw_path.as_deref(),
        &osc_path,
        &dataset,
        file_mb,
        &paths.scratch_dir,
        &ws.workspace_root,
    )
}

fn cmd_download(region: String, osc_url: Option<String>) -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(
        &dev_config,
        &hostname,
        &ws.workspace_root,
        &ws.target_dir,
    );

    download::run(
        &region,
        osc_url.as_deref(),
        &paths.data_dir,
        &ws.workspace_root,
    )
}

fn cmd_clean() -> Result<(), DevError> {
    let ws = build::cargo_metadata()?;
    let hostname = config::hostname()?;
    let dev_config = config::load(&ws.workspace_root)?;
    let paths = config::resolve_paths(
        &dev_config,
        &hostname,
        &ws.workspace_root,
        &ws.target_dir,
    );

    // Clean verify output.
    let verify_dir = paths.target_dir.join("verify");
    if verify_dir.exists() {
        std::fs::remove_dir_all(&verify_dir)?;
        output::run_msg("removed verify output");
    }

    // Clean scratch temp files.
    if paths.scratch_dir.exists() {
        let mut removed = 0u32;
        if let Ok(entries) = std::fs::read_dir(&paths.scratch_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("pbf") {
                    let _ = std::fs::remove_file(&path);
                    removed += 1;
                }
            }
        }
        if removed > 0 {
            output::run_msg(&format!("removed {removed} scratch file(s)"));
        }
    }

    output::result_msg("clean done");
    Ok(())
}
