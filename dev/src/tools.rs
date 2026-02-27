//! External tool download and cache management.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::DevError;
use crate::output;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub struct PlanetilerTools {
    pub java: PathBuf,
    pub planetiler_jar: PathBuf,
    pub bench_class_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const JDK_MAJOR: u32 = 25;

// ---------------------------------------------------------------------------
// Top-level entry point
// ---------------------------------------------------------------------------

/// Ensure JDK + Planetiler JAR + compiled benchmark class are ready.
pub fn ensure_planetiler(
    data_dir: &Path,
    workspace_root: &Path,
) -> Result<PlanetilerTools, DevError> {
    check_curl()?;

    let java = ensure_jdk(data_dir)?;
    let javac = data_dir.join("jdk/bin/javac");
    let planetiler_jar = ensure_planetiler_jar(data_dir)?;
    let bench_class_dir = compile_bench(data_dir, workspace_root, &javac, &planetiler_jar)?;

    Ok(PlanetilerTools {
        java,
        planetiler_jar,
        bench_class_dir,
    })
}

// ---------------------------------------------------------------------------
// curl preflight
// ---------------------------------------------------------------------------

fn check_curl() -> Result<(), DevError> {
    let result = std::process::Command::new("which")
        .arg("curl")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match result {
        Ok(status) if status.success() => Ok(()),
        _ => Err(DevError::Preflight(vec![
            "'curl' not found in PATH (required for tool downloads)".into(),
        ])),
    }
}

// ---------------------------------------------------------------------------
// JDK
// ---------------------------------------------------------------------------

fn ensure_jdk(data_dir: &Path) -> Result<PathBuf, DevError> {
    let jdk_dir = data_dir.join("jdk");
    let version_file = data_dir.join(".jdk-version");
    let java = jdk_dir.join("bin/java");

    let arch = detect_arch()?;
    let os = detect_os()?;
    let api_url = format!(
        "https://api.adoptium.net/v3/assets/latest/{JDK_MAJOR}/hotspot\
         ?architecture={arch}&image_type=jdk&os={os}&vendor=eclipse"
    );

    let api_body = run_curl(&["-sfL", &api_url], Path::new("."))?;
    let api_json: serde_json::Value = serde_json::from_slice(&api_body)?;

    let first = api_json
        .as_array()
        .and_then(|arr| arr.first())
        .ok_or_else(|| {
            DevError::Config("adoptium API returned empty response".into())
        })?;

    let release_name = first
        .get("release_name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            DevError::Config("adoptium API missing release_name".into())
        })?;

    let download_url = first
        .get("binary")
        .and_then(|b| b.get("package"))
        .and_then(|p| p.get("link"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            DevError::Config("adoptium API missing binary.package.link".into())
        })?;

    // Check cached version.
    if java.exists() {
        if let Ok(cached) = fs::read_to_string(&version_file) {
            if cached.trim() == release_name {
                return Ok(java);
            }
        }
    }

    // Download.
    let tarball = data_dir.join("jdk-download.tar.gz");
    let tarball_str = tarball.display().to_string();
    output::bench_msg(&format!("downloading JDK {release_name}"));
    run_curl(
        &["-fsSL", "-o", &tarball_str, download_url],
        Path::new("."),
    )?;

    // Remove old JDK dir and recreate.
    if jdk_dir.exists() {
        fs::remove_dir_all(&jdk_dir)?;
    }
    fs::create_dir_all(&jdk_dir)?;

    // Extract.
    let jdk_dir_str = jdk_dir.display().to_string();
    let captured = output::run_captured(
        "tar",
        &["xzf", &tarball_str, "-C", &jdk_dir_str, "--strip-components=1"],
        Path::new("."),
    )?;
    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Subprocess {
            program: "tar".into(),
            code: captured.status.code(),
            stderr: stderr.into_owned(),
        });
    }

    // Write version file.
    fs::write(&version_file, release_name)?;

    // Clean up tarball.
    let _ = fs::remove_file(&tarball);

    output::bench_msg(&format!("installed JDK {release_name}"));
    Ok(java)
}

// ---------------------------------------------------------------------------
// Planetiler JAR
// ---------------------------------------------------------------------------

fn ensure_planetiler_jar(data_dir: &Path) -> Result<PathBuf, DevError> {
    let jar_path = data_dir.join("planetiler.jar");
    let version_file = data_dir.join(".planetiler-version");

    let api_url =
        "https://api.github.com/repos/onthegomap/planetiler/releases/latest";

    let api_body = run_curl(&["-sfL", api_url], Path::new("."))?;
    let api_json: serde_json::Value = serde_json::from_slice(&api_body)?;

    let tag_name = api_json
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            DevError::Config("github API missing tag_name".into())
        })?;

    let assets = api_json
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            DevError::Config("github API missing assets array".into())
        })?;

    let download_url = assets
        .iter()
        .find(|a| {
            a.get("name")
                .and_then(serde_json::Value::as_str)
                .map_or(false, |n| n == "planetiler.jar")
        })
        .and_then(|a| a.get("browser_download_url"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            DevError::Config(
                "github API: no planetiler.jar asset found in release".into(),
            )
        })?;

    // Check cached version.
    if jar_path.exists() {
        if let Ok(cached) = fs::read_to_string(&version_file) {
            if cached.trim() == tag_name {
                return Ok(jar_path);
            }
        }
    }

    // Download.
    let jar_str = jar_path.display().to_string();
    output::bench_msg(&format!("downloading Planetiler {tag_name}"));
    run_curl(
        &["-fsSL", "-o", &jar_str, download_url],
        Path::new("."),
    )?;

    // Write version file.
    fs::write(&version_file, tag_name)?;

    output::bench_msg(&format!("installed Planetiler {tag_name}"));
    Ok(jar_path)
}

// ---------------------------------------------------------------------------
// Compile benchmark class
// ---------------------------------------------------------------------------

fn compile_bench(
    data_dir: &Path,
    workspace_root: &Path,
    javac: &Path,
    planetiler_jar: &Path,
) -> Result<PathBuf, DevError> {
    let bench_src = workspace_root
        .join("bench/planetiler-baseline/BenchPbfRead.java");
    let class_dir = data_dir.join("planetiler-bench-classes");
    let class_file = class_dir.join("BenchPbfRead.class");

    // Check if recompilation is needed.
    if class_file.exists() {
        if let Some(false) = needs_recompile(&class_file, &bench_src, planetiler_jar) {
            return Ok(class_dir);
        }
    }

    fs::create_dir_all(&class_dir)?;

    let javac_str = javac.display().to_string();
    let jar_str = planetiler_jar.display().to_string();
    let class_dir_str = class_dir.display().to_string();
    let bench_src_str = bench_src.display().to_string();

    let captured = output::run_captured(
        &javac_str,
        &["-proc:none", "-cp", &jar_str, "-d", &class_dir_str, &bench_src_str],
        workspace_root,
    )?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Subprocess {
            program: "javac".into(),
            code: captured.status.code(),
            stderr: stderr.into_owned(),
        });
    }

    output::bench_msg("compiled planetiler benchmark");
    Ok(class_dir)
}

/// Returns `Some(true)` if the class file is older than any source, `Some(false)`
/// if it is up to date, or `None` if timestamps could not be compared.
fn needs_recompile(
    class_file: &Path,
    bench_src: &Path,
    planetiler_jar: &Path,
) -> Option<bool> {
    let class_mtime = file_mtime(class_file)?;
    let src_mtime = file_mtime(bench_src)?;
    let jar_mtime = file_mtime(planetiler_jar)?;

    Some(src_mtime > class_mtime || jar_mtime > class_mtime)
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

// ---------------------------------------------------------------------------
// Helpers: architecture / OS detection
// ---------------------------------------------------------------------------

fn detect_arch() -> Result<&'static str, DevError> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x64"),
        "aarch64" => Ok("aarch64"),
        other => Err(DevError::Config(format!(
            "unsupported architecture: {other}"
        ))),
    }
}

fn detect_os() -> Result<&'static str, DevError> {
    match std::env::consts::OS {
        "linux" => Ok("linux"),
        "macos" => Ok("mac"),
        other => Err(DevError::Config(format!(
            "unsupported OS: {other}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Helpers: curl wrapper
// ---------------------------------------------------------------------------

/// Run curl with the given arguments, returning stdout bytes on success.
fn run_curl(args: &[&str], cwd: &Path) -> Result<Vec<u8>, DevError> {
    let captured = output::run_captured("curl", args, cwd)?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Subprocess {
            program: "curl".into(),
            code: captured.status.code(),
            stderr: stderr.into_owned(),
        });
    }

    Ok(captured.stdout)
}
