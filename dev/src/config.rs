use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::DevError;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct DevConfig {
    pub datasets: HashMap<String, Dataset>,
    pub hosts: HashMap<String, HostConfig>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Dataset {
    pub pbf: String,
    pub osc: Option<String>,
    pub sha256_pbf: Option<String>,
    pub sha256_osc: Option<String>,
    pub origin: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct HostConfig {
    pub data: Option<String>,
    pub scratch: Option<String>,
    pub target: Option<String>,
    pub port: Option<u16>,
    pub drives: Option<DriveConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DriveConfig {
    pub source: Option<String>,
    pub data: Option<String>,
    pub scratch: Option<String>,
    pub target: Option<String>,
}

#[allow(dead_code)]
pub struct ResolvedPaths {
    pub hostname: String,
    pub data_dir: PathBuf,
    pub scratch_dir: PathBuf,
    pub target_dir: PathBuf,
    pub drives: Option<DriveConfig>,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Load `dev.toml` from the workspace root directory.
pub fn load(workspace_root: &Path) -> Result<DevConfig, DevError> {
    let path = workspace_root.join("dev.toml");
    let text = std::fs::read_to_string(&path).map_err(|e| {
        DevError::Config(format!("{}: {e}", path.display()))
    })?;

    let root: toml::Value = text.parse()?;

    let table = root
        .as_table()
        .ok_or_else(|| DevError::Config("dev.toml root is not a table".into()))?;

    let datasets = parse_datasets(table)?;
    let hosts = parse_hosts(table)?;

    Ok(DevConfig { datasets, hosts })
}

/// Extract the `[datasets]` section and deserialize each entry.
fn parse_datasets(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<HashMap<String, Dataset>, DevError> {
    let Some(datasets_val) = table.get("datasets") else {
        return Ok(HashMap::new());
    };

    let datasets_table = datasets_val
        .as_table()
        .ok_or_else(|| DevError::Config("[datasets] is not a table".into()))?;

    let mut out = HashMap::with_capacity(datasets_table.len());
    for (name, value) in datasets_table {
        let ds: Dataset = value.clone().try_into().map_err(|e: toml::de::Error| {
            DevError::Config(format!("datasets.{name}: {e}"))
        })?;
        out.insert(name.clone(), ds);
    }
    Ok(out)
}

/// Every top-level key that is a table and is not `datasets` is treated as a
/// hostname section.
fn parse_hosts(
    table: &toml::map::Map<String, toml::Value>,
) -> Result<HashMap<String, HostConfig>, DevError> {
    let mut out = HashMap::new();
    for (key, value) in table {
        if key == "datasets" {
            continue;
        }
        if !value.is_table() {
            continue;
        }
        let hc: HostConfig = value.clone().try_into().map_err(|e: toml::de::Error| {
            DevError::Config(format!("{key}: {e}"))
        })?;
        out.insert(key.clone(), hc);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Hostname
// ---------------------------------------------------------------------------

/// Get the current hostname via `libc::gethostname()`.
pub fn hostname() -> Result<String, DevError> {
    let mut buf = [0u8; 256];
    let ret = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if ret != 0 {
        return Err(DevError::Config("gethostname failed".into()));
    }

    let len = buf
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| DevError::Config("hostname not null-terminated".into()))?;

    String::from_utf8(buf[..len].to_vec())
        .map_err(|e| DevError::Config(format!("hostname is not utf-8: {e}")))
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Resolve host-specific paths from config, with defaults for unknown hosts.
///
/// - `workspace_root`: the root of the cargo workspace
/// - `target_dir`: from cargo metadata (resolved elsewhere)
pub fn resolve_paths(
    config: &DevConfig,
    hostname: &str,
    workspace_root: &Path,
    target_dir: &Path,
) -> ResolvedPaths {
    let host = config.hosts.get(hostname);

    let data_rel = host
        .and_then(|h| h.data.as_deref())
        .unwrap_or("data");

    let scratch_rel = host
        .and_then(|h| h.scratch.as_deref())
        .unwrap_or("data/scratch");

    let data_dir = resolve_relative(workspace_root, data_rel);
    let scratch_dir = resolve_relative(workspace_root, scratch_rel);

    let target_dir = match host.and_then(|h| h.target.as_deref()) {
        Some(t) => resolve_relative(workspace_root, t),
        None => target_dir.to_path_buf(),
    };

    let drives = host.and_then(|h| h.drives.clone());

    ResolvedPaths {
        hostname: hostname.to_owned(),
        data_dir,
        scratch_dir,
        target_dir,
        drives,
    }
}

/// Resolve a potentially relative path against a base directory.
/// Absolute paths are returned as-is.
fn resolve_relative(base: &Path, rel: &str) -> PathBuf {
    let p = Path::new(rel);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}
