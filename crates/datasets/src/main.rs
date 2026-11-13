//! Acquire and register development datasets.
//!
//! Run as `brokkr run --with pbfhogg pbfhogg-datasets -- download REGION`:
//! brokkr holds the machine lock for the whole run and hands the built
//! pbfhogg CLI in `BROKKR_WITH_EXE`. This program never calls brokkr and
//! links no pbfhogg.
mod date;
mod download;
mod layout;
mod lock;
mod output;
mod process;
mod promote;
mod source;
mod tunables;

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use pbfhogg_devkit::{Config, user_config_path};

#[derive(Parser)]
#[command(
    name = "pbfhogg-datasets",
    about = "Acquire and register development datasets"
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
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Register a completed cat, repack or degrade output as a snapshot.
    Promote {
        dataset: String,
        /// Completed output file. Copied, never consumed or modified.
        file: PathBuf,
        #[arg(long)]
        as_snapshot: String,
        #[arg(long)]
        replace_snapshot: bool,
        /// Command that produced FILE (not an instruction to execute it).
        #[arg(long, value_enum)]
        producer: promote::Producer,
        /// The degrade output was produced with --strip-indexdata.
        #[arg(long)]
        strip_indexdata: bool,
    },
    /// Download a dataset from Geofabrik or planet.openstreetmap.org
    Download {
        /// Region name, Geofabrik path or registered dataset (e.g. denmark,
        /// europe/france, asia/japan/kanto, planet)
        region: String,
        /// Acquire every diff after the selected raw sequence through N.
        #[arg(long)]
        osc_seq: Option<u64>,
        /// Register a new named snapshot beside an existing dataset.
        #[arg(long)]
        as_snapshot: Option<String>,
        /// Extend OSCs on an existing snapshot (requires --osc-seq).
        #[arg(long, requires = "osc_seq", conflicts_with_all = ["as_snapshot", "refresh"])]
        snapshot: Option<String>,
        /// Rotate the primary into a dated snapshot when upstream is newer.
        #[arg(long, conflicts_with = "as_snapshot")]
        refresh: bool,
        /// Rotate even without evidence of a newer upstream file.
        #[arg(long, requires = "refresh")]
        force: bool,
    },
}

/// Proxy and CA settings curl honours, passed through to it and nothing
/// else of this process's environment.
fn curl_env() -> Vec<(OsString, OsString)> {
    [
        ("http_proxy", std::env::var_os("http_proxy")),
        ("https_proxy", std::env::var_os("https_proxy")),
        ("HTTPS_PROXY", std::env::var_os("HTTPS_PROXY")),
        ("all_proxy", std::env::var_os("all_proxy")),
        ("ALL_PROXY", std::env::var_os("ALL_PROXY")),
        ("no_proxy", std::env::var_os("no_proxy")),
        ("NO_PROXY", std::env::var_os("NO_PROXY")),
        ("SSL_CERT_FILE", std::env::var_os("SSL_CERT_FILE")),
        ("SSL_CERT_DIR", std::env::var_os("SSL_CERT_DIR")),
        ("CURL_CA_BUNDLE", std::env::var_os("CURL_CA_BUNDLE")),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.map(|v| (OsString::from(name), v)))
    .collect()
}

/// Configuration and environment access stays at this executable boundary.
fn boundary(args: &Args) -> Result<(Config, String, Option<process::Tools>), String> {
    let override_path = std::env::var_os("BROKKR_USER_CONFIG");
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");
    let with_exe = std::env::var_os("BROKKR_WITH_EXE");
    let path = std::env::var_os("PATH");
    let curl_env = curl_env();
    let user = user_config_path(override_path.as_deref(), xdg.as_deref(), home.as_deref());
    let config = Config::load(&args.config, user.as_deref()).map_err(|e| e.to_string())?;
    let host = match &args.host {
        Some(host) => host.clone(),
        None => hostname()?,
    };
    if matches!(args.command, Action::Promote { .. }) {
        return Ok((config, host, None));
    }
    let pbfhogg = args
        .pbfhogg
        .clone()
        .or_else(|| with_exe.map(PathBuf::from))
        .ok_or("supply --pbfhogg PATH or BROKKR_WITH_EXE (brokkr run --with pbfhogg)")?;
    let pbfhogg = executable(&pbfhogg)?;
    let curl = path
        .as_deref()
        .and_then(|p| find_in_path("curl", p))
        .ok_or("'curl' not found in PATH (required for downloads)")?;
    Ok((
        config,
        host,
        Some(process::Tools {
            curl,
            curl_env,
            pbfhogg,
        }),
    ))
}

fn find_in_path(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| executable(candidate).is_ok())
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
    let path = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(format!("{} is not an executable file", path.display()));
    }
    Ok(path)
}

fn execute(args: &Args) -> Result<(), String> {
    let (config, host, tools) = boundary(args)?;
    match &args.command {
        Action::Promote {
            dataset,
            file,
            as_snapshot,
            replace_snapshot,
            producer,
            strip_indexdata,
        } => promote::run(&promote::Request {
            config: &config,
            host: &host,
            dataset,
            file,
            snapshot: as_snapshot,
            replace: *replace_snapshot,
            producer: *producer,
            strip_indexdata: *strip_indexdata,
        }),
        Action::Download {
            region,
            osc_seq,
            as_snapshot,
            snapshot,
            refresh,
            force,
        } => download::run(&download::Request {
            config: &config,
            host: &host,
            region,
            osc_seq: *osc_seq,
            as_snapshot: as_snapshot.as_deref(),
            snapshot: snapshot.as_deref(),
            refresh: *refresh,
            force: *force,
            tools: tools.as_ref().ok_or("download tools missing")?,
            today: date::CivilDate::today(),
        }),
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
    #![allow(clippy::unwrap_used)]
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_parses_download() {
        Args::command().debug_assert();
        assert!(Args::try_parse_from(["x", "download", "planet", "--snapshot", "s"]).is_err());
        for flag in ["--refresh", "--as-snapshot"] {
            let mut args = vec![
                "x",
                "download",
                "planet",
                "--snapshot",
                "s",
                "--osc-seq",
                "13",
                flag,
            ];
            if flag == "--as-snapshot" {
                args.push("new");
            }
            assert!(Args::try_parse_from(args).is_err());
        }
        assert!(
            Args::try_parse_from([
                "x",
                "download",
                "planet",
                "--snapshot",
                "s",
                "--osc-seq",
                "13"
            ])
            .is_ok()
        );
        assert!(Args::try_parse_from(["x", "download", "denmark", "--force"]).is_err());
        assert!(
            Args::try_parse_from([
                "x",
                "download",
                "denmark",
                "--refresh",
                "--as-snapshot",
                "s"
            ])
            .is_err()
        );
        assert!(Args::try_parse_from(["x", "download", "denmark", "--refresh", "--force"]).is_ok());
        assert!(Args::try_parse_from(["x", "download"]).is_err());
        assert!(Args::try_parse_from(["x", "download", "denmark", "--host", "h"]).is_ok());
        let parsed = Args::try_parse_from([
            "x",
            "download",
            "denmark",
            "--as-snapshot",
            "named",
            "--osc-seq",
            "13",
        ])
        .unwrap();
        assert!(
            matches!(parsed.command, Action::Download { as_snapshot: Some(key), osc_seq: Some(13), .. } if key == "named")
        );
    }

    #[test]
    fn cli_parses_promotion() {
        let parsed = Args::try_parse_from([
            "x",
            "promote",
            "tiny",
            "output.osm.pbf",
            "--as-snapshot",
            "named",
            "--producer",
            "degrade",
            "--strip-indexdata",
            "--replace-snapshot",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Action::Promote {
                producer: promote::Producer::Degrade,
                strip_indexdata: true,
                replace_snapshot: true,
                ..
            }
        ));
        assert!(
            Args::try_parse_from([
                "x",
                "promote",
                "tiny",
                "output.osm.pbf",
                "--as-snapshot",
                "named"
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from([
                "x",
                "promote",
                "tiny",
                "output.osm.pbf",
                "--as-snapshot",
                "named",
                "--producer",
                "sort"
            ])
            .is_err()
        );
    }

    #[test]
    fn curl_is_found_only_as_an_absolute_executable() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = scratch.path().join("bin");
        std::fs::create_dir(&dir).unwrap();
        let curl = dir.join("curl");
        std::fs::write(&curl, b"x").unwrap();
        let path = std::env::join_paths(["relative", dir.to_str().unwrap()]).unwrap();
        assert!(find_in_path("curl", &path).is_none());
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(find_in_path("curl", &path).is_some());
    }
}
