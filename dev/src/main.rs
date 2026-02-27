mod bench_read;
mod bench_write;
mod build;
mod config;
mod db;
mod env;
mod error;
mod git;
mod harness;
mod lockfile;
mod output;
#[allow(dead_code)]
mod preflight;

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
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Bench { bench } => cmd_bench(bench),
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
