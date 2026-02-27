//! Benchmark: Planetiler Java PBF read performance.
//!
//! Runs the Planetiler PBF read benchmark as a Java subprocess.
//! Planetiler handles its own best-of-N timing internally, outputting
//! `---` delimited key=value blocks to stderr for each mode.

use std::path::Path;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, BenchResult};
use crate::output;
use crate::tools;

// ---------------------------------------------------------------------------
// Parsed result from Java benchmark output
// ---------------------------------------------------------------------------

struct ParsedResult {
    mode: String,
    elapsed_ms: i64,
    extra: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Output parsing
// ---------------------------------------------------------------------------

/// Parse `---` delimited key=value blocks from Planetiler benchmark stderr.
///
/// Each block looks like:
/// ```text
/// ---
/// tool=planetiler
/// mode=sequential
/// elapsed_ms=1234
/// nodes=123456
/// ways=78901
/// relations=2345
/// file_mb=465
/// ```
fn parse_planetiler_output(stderr: &str) -> Vec<ParsedResult> {
    let mut results = Vec::new();

    for block in stderr.split("---") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }

        let mut mode: Option<String> = None;
        let mut elapsed_ms: Option<i64> = None;
        let mut nodes: Option<i64> = None;
        let mut ways: Option<i64> = None;
        let mut relations: Option<i64> = None;

        for line in block.lines() {
            let line = line.trim();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };

            match key {
                "mode" => mode = Some(value.to_owned()),
                "elapsed_ms" => elapsed_ms = value.parse().ok(),
                "nodes" => nodes = value.parse().ok(),
                "ways" => ways = value.parse().ok(),
                "relations" => relations = value.parse().ok(),
                _ => {}
            }
        }

        if let (Some(mode), Some(elapsed_ms)) = (mode, elapsed_ms) {
            let extra = serde_json::json!({
                "nodes": nodes.unwrap_or(0),
                "ways": ways.unwrap_or(0),
                "relations": relations.unwrap_or(0),
            });

            results.push(ParsedResult {
                mode,
                elapsed_ms,
                extra,
            });
        }
    }

    results
}

// ---------------------------------------------------------------------------
// Subprocess runner
// ---------------------------------------------------------------------------

/// Run the Java benchmark subprocess and parse results from stderr.
fn run_planetiler_subprocess(
    java: &Path,
    classpath: &str,
    pbf_path: &Path,
    heap_mb: i64,
    runs: usize,
    workspace_root: &Path,
) -> Result<Vec<ParsedResult>, DevError> {
    let heap_arg = format!("-Xmx{heap_mb}m");
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;
    let runs_str = runs.to_string();

    let java_str = java
        .to_str()
        .ok_or_else(|| DevError::Config("Java path is not valid UTF-8".into()))?;

    let args: Vec<&str> = vec![
        &heap_arg,
        "-cp",
        classpath,
        "BenchPbfRead",
        pbf_str,
        &runs_str,
    ];

    let captured = output::run_captured(java_str, &args, workspace_root)?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Subprocess {
            program: java_str.to_owned(),
            code: captured.status.code(),
            stderr: stderr.into_owned(),
        });
    }

    let stderr = String::from_utf8_lossy(&captured.stderr);
    Ok(parse_planetiler_output(&stderr))
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the Planetiler Java PBF read benchmark.
///
/// The `runs` parameter is passed to the Java process (which handles
/// best-of-N internally). Each mode result is recorded separately via
/// the harness.
pub fn run(
    harness: &BenchHarness,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    data_dir: &Path,
    workspace_root: &Path,
) -> Result<(), DevError> {
    let pt = tools::ensure_planetiler(data_dir, workspace_root)?;

    let heap_mb = std::cmp::max((file_mb as i64) * 2, 2048);
    let classpath = format!("{}:{}", pt.planetiler_jar.display(), pt.bench_class_dir.display());

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    output::bench_msg("running planetiler benchmark");

    let results = run_planetiler_subprocess(
        &pt.java,
        &classpath,
        pbf_path,
        heap_mb,
        runs,
        workspace_root,
    )?;

    if results.is_empty() {
        return Err(DevError::Build(
            "no results from planetiler benchmark".into(),
        ));
    }

    for result in &results {
        let config = BenchConfig {
            command: "bench planetiler".into(),
            variant: Some(result.mode.clone()),
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: None,
            cargo_profile: "release".into(),
            runs: 1,
        };

        harness.run_internal(&config, |_| {
            Ok(BenchResult {
                elapsed_ms: result.elapsed_ms,
                extra: Some(result.extra.clone()),
            })
        })?;
    }

    Ok(())
}
