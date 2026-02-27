mod build;
mod config;
mod env;
mod error;
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
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Check { args } => cmd_check(&args),
        Command::Env => cmd_env(),
        Command::Run { args } => cmd_run(&args),
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
