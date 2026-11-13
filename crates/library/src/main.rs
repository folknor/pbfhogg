//! Independent reader and supervisor for pbfhogg's library harness protocol.
//!
//! This binary deliberately has no dependency on pbfhogg or harness types.
mod artefact;
mod certification;
mod certify;
mod describe;
mod discovery;
mod identity;
mod json;
mod output;
mod process;
mod publication;
mod requirements;
mod run;
mod seed;
mod sidecar;
mod stream;

#[cfg_attr(test, allow(dead_code))]
// Lifecycle constants are used by the dedicated process test target.
mod tunables;

use clap::{Parser, Subcommand};
use pbfhogg_devkit::{Config, user_config_path};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "pbfhogg-library",
    about = "List and verify library harness scenarios"
)]
struct Args {
    /// Harness executable (otherwise BROKKR_WITH_EXE). Never builds a harness.
    #[arg(long, global = true)]
    harness: Option<PathBuf>,
    #[arg(long, global = true, default_value = "brokkr.toml")]
    config: PathBuf,
    /// Config host section (otherwise the kernel's hostname).
    #[arg(long, global = true)]
    host: Option<String>,
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    List,
    Verify {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        scenario: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        dataset: String,
        #[arg(long)]
        snapshot: Option<String>,
    },
}

/// Configuration/environment access stays at this executable boundary.
struct Boundary {
    config: Config,
    host: String,
    harness: PathBuf,
}
fn boundary(args: &Args) -> Result<Boundary, String> {
    let override_path = std::env::var_os("BROKKR_USER_CONFIG");
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");
    let harness_env = std::env::var_os("BROKKR_WITH_EXE");
    let user = user_config_path(override_path.as_deref(), xdg.as_deref(), home.as_deref());
    let config = Config::load(&args.config, user.as_deref()).map_err(|e| e.to_string())?;
    let host = match &args.host {
        Some(host) => host.clone(),
        None => hostname()?,
    };
    let harness = args
        .harness
        .clone()
        .or_else(|| harness_env.map(PathBuf::from))
        .ok_or("supply --harness PATH or BROKKR_WITH_EXE (brokkr run --with pbfhogg-harness)")?;
    let harness = executable(&harness)?;
    Ok(Boundary {
        config,
        host,
        harness,
    })
}
fn hostname() -> Result<String, String> {
    // gethostname truncation is refused rather than selecting a wrong host.
    let mut name = [0_u8; 256];
    // SAFETY: writable buffer with its correct capacity.
    if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } == -1 {
        return Err(format!("hostname: {}", std::io::Error::last_os_error()));
    }
    let end = name
        .iter()
        .position(|b| *b == 0)
        .ok_or("hostname is too long")?;
    String::from_utf8(name[..end].to_vec()).map_err(|e| format!("hostname: {e}"))
}
fn executable(path: &Path) -> Result<PathBuf, String> {
    let path =
        std::fs::canonicalize(path).map_err(|e| format!("harness {}: {e}", path.display()))?;
    let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(format!(
            "harness {} is not an executable file",
            path.display()
        ));
    }
    Ok(path)
}
fn listing(doc: &describe::Describe, out: &mut impl std::io::Write) -> std::io::Result<()> {
    for scenario in &doc.scenarios {
        writeln!(
            out,
            "{}: {} (deadline {}s, hotpath {})",
            scenario.id, scenario.description, scenario.deadline_s, scenario.hotpath
        )?;
        for call in &scenario.calls {
            writeln!(
                out,
                "  call {}: {}; stream {:?}",
                call.id, call.label, call.stream
            )?;
        }
        for role in &scenario.roles {
            writeln!(
                out,
                "  role {}: {} {:?}",
                role.id,
                role.kind.name(),
                role.source
            )?;
        }
        for assertion in &scenario.assertions {
            writeln!(
                out,
                "  assertion {}: call {:?}; covers {:?}",
                assertion.id, assertion.call, assertion.covers
            )?;
        }
    }
    Ok(())
}
fn execute(args: &Args) -> Result<(), String> {
    let selected = boundary(args)?;
    process::subreaper().map_err(|e| format!("enable child subreaper: {e}"))?;
    match &args.command {
        Action::List => {
            let doc = discovery::load(&selected.harness)?;
            output::write(|out| listing(&doc, out)).map_err(|e| e.to_string())
        }
        Action::Verify {
            scenario,
            all,
            dataset,
            snapshot,
        } => {
            if *all {
                return certification::verify(
                    &selected.config,
                    &selected.host,
                    &selected.harness,
                    dataset,
                    snapshot.as_deref(),
                );
            }
            let doc = discovery::load(&selected.harness)?;
            let scenario = doc
                .scenarios
                .iter()
                .find(|s| Some(&s.id) == scenario.as_ref())
                .ok_or("unknown scenario")?;
            let mut inputs = Vec::new();
            for role in &scenario.roles {
                if let describe::RoleSource::Dataset { variant } = &role.source {
                    inputs.push(
                        selected
                            .config
                            .resolve_input(
                                &selected.host,
                                dataset,
                                snapshot.as_deref(),
                                &role.id,
                                variant,
                            )
                            .map_err(|e| e.to_string())?,
                    );
                }
            }
            let root = selected
                .config
                .scratch_root(&selected.host)
                .map_err(|e| e.to_string())?
                .join("tools");
            let report = run::verify(&selected.harness, scenario, &inputs, &root)?;
            if report.failures.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "{}; run kept at {}{}",
                    report.failures.join("; "),
                    report.run_dir.display(),
                    if report.unestablished {
                        "; no verdict issued"
                    } else {
                        ""
                    }
                ))
            }
        }
    }
}
fn main() -> std::process::ExitCode {
    let args = Args::parse();
    match execute(&args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            output::error(&error);
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use clap::CommandFactory;
    #[test]
    fn cli_requires_one_selection_and_dataset() {
        Args::command().debug_assert();
        for args in [
            vec!["x", "verify"],
            vec!["x", "verify", "s"],
            vec!["x", "verify", "s", "--all", "--dataset", "d"],
        ] {
            assert!(Args::try_parse_from(args).is_err());
        }
        for args in [
            vec!["x", "list"],
            vec!["x", "verify", "s", "--dataset", "d"],
            vec!["x", "verify", "--all", "--dataset", "d", "--snapshot", "k"],
        ] {
            assert!(Args::try_parse_from(args).is_ok());
        }
    }
    #[test]
    fn harness_must_be_an_executable_regular_file() {
        let scratch = pbfhogg_test_scratch::scratch();
        let path = scratch.path().join("worker");
        assert!(executable(&path).is_err());
        std::fs::write(&path, b"x").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(executable(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(executable(&path).is_ok());
        assert!(executable(scratch.path()).is_err());
    }
}
