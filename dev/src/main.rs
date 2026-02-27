mod build;
mod config;
mod db;
mod env;
mod error;
#[allow(dead_code)]
mod git;
#[allow(dead_code)]
mod harness;
#[allow(dead_code)]
mod lockfile;
mod output;
#[allow(dead_code)]
mod preflight;

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

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
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
