use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::error::DevError;
use crate::output;

// ---------------------------------------------------------------------------
// Build configuration
// ---------------------------------------------------------------------------

pub struct BuildConfig {
    pub package: &'static str,
    pub features: &'static [&'static str],
    pub default_features: bool,
    pub profile: &'static str,
}

impl BuildConfig {
    pub fn release_cli() -> Self {
        Self {
            package: "pbfhogg-cli",
            features: &[],
            default_features: true,
            profile: "release",
        }
    }
}

// ---------------------------------------------------------------------------
// Workspace info
// ---------------------------------------------------------------------------

pub struct WorkspaceInfo {
    pub workspace_root: PathBuf,
    pub target_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// cargo metadata
// ---------------------------------------------------------------------------

/// Resolve workspace root and target directory via `cargo metadata`.
pub fn cargo_metadata() -> Result<WorkspaceInfo, DevError> {
    let captured = output::run_captured(
        "cargo",
        &["metadata", "--format-version", "1", "--no-deps"],
        Path::new("."),
    )?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Build(format!(
            "cargo metadata failed: {stderr}"
        )));
    }

    let stdout = String::from_utf8_lossy(&captured.stdout);
    let val: serde_json::Value = serde_json::from_str(&stdout)?;

    let workspace_root = extract_string(&val, "workspace_root")?;
    let target_dir = extract_string(&val, "target_directory")?;

    Ok(WorkspaceInfo {
        workspace_root: PathBuf::from(workspace_root),
        target_dir: PathBuf::from(target_dir),
    })
}

/// Extract a string field from a JSON value, returning a `DevError::Build` on
/// missing or wrong type.
fn extract_string<'a>(
    val: &'a serde_json::Value,
    key: &str,
) -> Result<&'a str, DevError> {
    val.get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            DevError::Build(format!(
                "cargo metadata missing \"{key}\" field"
            ))
        })
}

// ---------------------------------------------------------------------------
// cargo build
// ---------------------------------------------------------------------------

/// Build a crate and return the path to the compiled binary.
///
/// Parses `--message-format=json` output to find the `"executable"` field.
pub fn cargo_build(
    config: &BuildConfig,
    workspace_root: &Path,
) -> Result<PathBuf, DevError> {
    let args = build_args(config);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    output::build_msg(&format!(
        "cargo {}", arg_refs.join(" ")
    ));

    let start = Instant::now();
    let captured = output::run_captured("cargo", &arg_refs, workspace_root)?;
    let elapsed = start.elapsed();

    if !captured.status.success() {
        dump_build_stderr(&captured.stderr);
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Build(format!(
            "cargo build failed: {stderr}"
        )));
    }

    let executable = find_executable(&captured.stdout)?;

    output::build_msg(&format!(
        "done in {:.1}s -> {}",
        elapsed.as_secs_f64(),
        executable.display(),
    ));

    Ok(executable)
}

/// Assemble the argument list for `cargo build`.
fn build_args(config: &BuildConfig) -> Vec<String> {
    let mut args = Vec::with_capacity(10);
    args.push("build".into());
    args.push("-p".into());
    args.push(config.package.into());

    if config.profile == "release" {
        args.push("--release".into());
    } else {
        args.push("--profile".into());
        args.push(config.profile.into());
    }

    args.push("--message-format=json".into());

    if !config.default_features {
        args.push("--no-default-features".into());
    }

    if !config.features.is_empty() {
        args.push("--features".into());
        args.push(config.features.join(","));
    }

    args
}

/// Scan JSON lines from cargo output to find the last `"executable"` path.
fn find_executable(stdout: &[u8]) -> Result<PathBuf, DevError> {
    let text = String::from_utf8_lossy(stdout);
    let mut last_exe: Option<PathBuf> = None;

    for line in text.lines() {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(exe) = val.get("executable").and_then(serde_json::Value::as_str) {
            last_exe = Some(PathBuf::from(exe));
        }
    }

    last_exe.ok_or_else(|| {
        DevError::Build("no executable in cargo output".into())
    })
}

/// Print captured stderr through the error output channel.
fn dump_build_stderr(stderr: &[u8]) {
    let text = String::from_utf8_lossy(stderr);
    if !text.is_empty() {
        output::error(&text);
    }
}
