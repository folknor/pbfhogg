//! Combined benchmark suite: runs all pbfhogg benchmarks plus external baselines.

use std::collections::HashMap;
use std::path::Path;

use crate::build;
use crate::config::{DevConfig, ResolvedPaths};
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, BenchResult};
use crate::output;
use crate::{bench_commands, bench_merge, bench_planetiler, bench_read, bench_write};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the full benchmark suite: read, write, merge, commands, plus external baselines.
///
/// External baselines (osmpbf, osmium, planetiler) are best-effort — errors
/// skip the baseline without failing the whole run.
#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    dev_config: &DevConfig,
    paths: &ResolvedPaths,
    workspace_root: &Path,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    dataset: &str,
) -> Result<(), DevError> {
    // 1. bench read — all 5 modes
    output::bench_msg("=== bench read ===");
    bench_read::run(harness, pbf_path, file_mb, runs, bench_read::ALL_MODES)?;

    // 2. bench write — default compressions
    output::bench_msg("=== bench write ===");
    let compressions = bench_write::parse_compressions("none,zlib:6,zstd:3")?;
    bench_write::run(harness, pbf_path, file_mb, runs, &compressions)?;

    // 3. bench merge — if dataset has OSC
    let ds = dev_config.datasets.get(dataset);
    if let Some(osc_file) = ds.and_then(|d| d.osc.as_ref()) {
        let osc_path = paths.data_dir.join(osc_file);
        if osc_path.exists() {
            output::bench_msg("=== bench merge ===");
            let merge_compressions = bench_merge::parse_compressions("zlib,none")?;
            match bench_merge::run(
                harness,
                pbf_path,
                &osc_path,
                file_mb,
                runs,
                &merge_compressions,
                false,
                &paths.scratch_dir,
            ) {
                Ok(()) => {}
                Err(e) => output::bench_msg(&format!("merge skipped: {e}")),
            }
        }
    }

    // 4. bench commands — all 14
    output::bench_msg("=== bench commands ===");
    let binary = build::cargo_build(&build::BuildConfig::release_cli(), workspace_root)?;
    bench_commands::run(
        harness,
        &binary,
        pbf_path,
        file_mb,
        runs,
        bench_commands::ALL_COMMANDS,
        workspace_root,
    )?;

    // 5. osmpbf baseline — build and run
    output::bench_msg("=== osmpbf baseline ===");
    let manifest = workspace_root.join("bench/osmpbf-baseline/Cargo.toml");
    if manifest.exists() {
        match run_osmpbf_baseline(harness, &manifest, pbf_path, file_mb, runs, workspace_root) {
            Ok(()) => {}
            Err(e) => output::bench_msg(&format!("osmpbf baseline skipped: {e}")),
        }
    }

    // 6. osmium — if available
    output::bench_msg("=== osmium baseline ===");
    if which_exists("osmium") {
        run_osmium_baseline(harness, pbf_path, file_mb, runs, workspace_root)?;
    }

    // 7. planetiler — if available
    output::bench_msg("=== planetiler baseline ===");
    match bench_planetiler::run(harness, pbf_path, file_mb, runs, &paths.data_dir, workspace_root)
    {
        Ok(()) => {}
        Err(e) => output::bench_msg(&format!("planetiler skipped: {e}")),
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// osmpbf baseline
// ---------------------------------------------------------------------------

fn run_osmpbf_baseline(
    harness: &BenchHarness,
    manifest: &Path,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    workspace_root: &Path,
) -> Result<(), DevError> {
    let manifest_str = manifest.display().to_string();

    // Build the baseline binary.
    let captured = output::run_captured(
        "cargo",
        &[
            "build",
            "--release",
            "--manifest-path",
            &manifest_str,
            "--message-format=json",
        ],
        workspace_root,
    )?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Build(format!(
            "osmpbf-baseline build failed: {stderr}"
        )));
    }

    let binary = build::find_executable(&captured.stdout)?;

    // Run the baseline binary: {binary} {pbf} {runs}
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;
    let runs_str = runs.to_string();
    let binary_str = binary.display().to_string();

    let captured = output::run_captured(
        &binary_str,
        &[pbf_str, &runs_str],
        workspace_root,
    )?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Subprocess {
            program: binary_str,
            code: captured.status.code(),
            stderr: stderr.into_owned(),
        });
    }

    // Parse stderr for --- delimited blocks.
    let stderr = String::from_utf8_lossy(&captured.stderr);
    let blocks = parse_stderr_blocks(&stderr);

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    for block in &blocks {
        let mode = match block.get("mode") {
            Some(m) => m.clone(),
            None => continue,
        };

        let elapsed_ms: i64 = match block.get("elapsed_ms").and_then(|v| v.parse().ok()) {
            Some(ms) => ms,
            None => continue,
        };

        let variant = format!("osmpbf/{mode}");
        let extra = build_extra_from_block(block);

        let config = BenchConfig {
            command: "bench baseline".into(),
            variant: Some(variant),
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: None,
            cargo_profile: "release".into(),
            runs: 1,
        };

        harness.run_internal(&config, |_i| {
            Ok(BenchResult {
                elapsed_ms,
                extra: extra.clone(),
            })
        })?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// osmium baseline
// ---------------------------------------------------------------------------

fn run_osmium_baseline(
    harness: &BenchHarness,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    workspace_root: &Path,
) -> Result<(), DevError> {
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    let config = BenchConfig {
        command: "bench baseline".into(),
        variant: Some("osmium/cat-opl".into()),
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "release".into(),
        runs,
    };

    harness.run_external(
        &config,
        Path::new("osmium"),
        &["cat", pbf_str, "-o", "/dev/null", "-f", "opl", "--overwrite"],
        workspace_root,
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check whether an executable exists on PATH.
fn which_exists(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_or(false, |s| s.success())
}

/// Parse `---` delimited blocks from stderr output.
///
/// Both the osmpbf-baseline and planetiler benchmark emit results as blocks
/// separated by `---` lines, with `key=value` pairs on each line within a block.
fn parse_stderr_blocks(stderr: &str) -> Vec<HashMap<String, String>> {
    let mut blocks = Vec::new();
    let mut current: Option<HashMap<String, String>> = None;

    for line in stderr.lines() {
        let trimmed = line.trim();
        if trimmed == "---" {
            // Start a new block, pushing the previous one if it exists.
            if let Some(block) = current.take() {
                if !block.is_empty() {
                    blocks.push(block);
                }
            }
            current = Some(HashMap::new());
            continue;
        }

        if let Some(ref mut block) = current {
            if let Some((key, value)) = trimmed.split_once('=') {
                block.insert(key.to_owned(), value.to_owned());
            }
        }
    }

    // Push the final block.
    if let Some(block) = current {
        if !block.is_empty() {
            blocks.push(block);
        }
    }

    blocks
}

/// Build a JSON extra object from a parsed block, including relevant fields.
fn build_extra_from_block(block: &HashMap<String, String>) -> Option<serde_json::Value> {
    let mut map = serde_json::Map::new();

    for (key, value) in block {
        // Skip keys already used as primary fields.
        if key == "mode" || key == "elapsed_ms" {
            continue;
        }
        // Try to parse as number, fall back to string.
        if let Ok(n) = value.parse::<i64>() {
            map.insert(key.clone(), serde_json::Value::Number(n.into()));
        } else if let Ok(n) = value.parse::<f64>() {
            if let Some(num) = serde_json::Number::from_f64(n) {
                map.insert(key.clone(), serde_json::Value::Number(num));
            } else {
                map.insert(key.clone(), serde_json::Value::String(value.clone()));
            }
        } else {
            map.insert(key.clone(), serde_json::Value::String(value.clone()));
        }
    }

    if map.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(map))
    }
}
