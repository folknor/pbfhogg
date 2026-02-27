//! Benchmark: merge a base PBF with an OSC diff file.
//!
//! Variant matrix:
//! - `buffered+{comp}` — standard buffered I/O (always run)
//! - `uring+{comp}` — io_uring writer (if `--uring`)
//! - `uring+sqpoll+{comp}` — io_uring with SQ polling (if `--uring`)

use std::path::Path;
use std::time::Instant;

use pbfhogg::commands::merge;
use pbfhogg::writer::Compression;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, BenchResult};
use crate::output;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse a comma-separated list of compression specs into `(label, Compression)` pairs.
///
/// Accepted formats: `none`, `zlib`, `zlib:N`, `zstd`, `zstd:N`.
/// Defaults: zlib level 6, zstd level 3.
pub fn parse_compressions(
    input: &str,
) -> Result<Vec<(String, Compression)>, DevError> {
    let mut result = Vec::new();
    for token in input.split(',') {
        let trimmed = token.trim();
        let (name, level_str) = match trimmed.split_once(':') {
            Some((n, l)) => (n, Some(l)),
            None => (trimmed, None),
        };
        let comp = match name {
            "none" => Compression::None,
            "zlib" => {
                let level = parse_zlib_level(level_str, 6)?;
                Compression::Zlib(level)
            }
            "zstd" => {
                let level = parse_zstd_level(level_str, 3)?;
                Compression::Zstd(level)
            }
            _ => return Err(DevError::Config(format!("unknown compression: {trimmed}"))),
        };
        let label = match comp {
            Compression::None => "none".to_owned(),
            Compression::Zlib(l) => format!("zlib:{l}"),
            Compression::Zstd(l) => format!("zstd:{l}"),
            _ => trimmed.to_owned(),
        };
        result.push((label, comp));
    }
    Ok(result)
}

/// Check that RLIMIT_MEMLOCK is sufficient for io_uring registered buffers.
///
/// Returns `Ok(())` if the limit is at least 16 MB, or `Err(DevError::Preflight)`
/// with a human-readable message explaining how to fix it.
pub fn check_uring_preflight() -> Result<(), DevError> {
    let mut rlim: libc::rlimit = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut rlim) };
    if ret != 0 {
        return Err(DevError::Preflight(vec![
            "could not read RLIMIT_MEMLOCK".into(),
        ]));
    }
    let limit_mb = rlim.rlim_cur / (1024 * 1024);
    if limit_mb < 16 {
        return Err(DevError::Preflight(vec![format!(
            "RLIMIT_MEMLOCK is {limit_mb} MB, need >= 16 MB (try: ulimit -l 65536)"
        )]));
    }
    Ok(())
}

/// Run the merge benchmark for each compression mode and I/O variant.
pub fn run(
    harness: &BenchHarness,
    pbf_path: &Path,
    osc_path: &Path,
    file_mb: f64,
    runs: usize,
    compressions: &[(String, Compression)],
    uring: bool,
    scratch_dir: &Path,
) -> Result<(), DevError> {
    std::fs::create_dir_all(scratch_dir)?;

    let output_path = scratch_dir.join("bench-merge-output.osm.pbf");
    let basename = pbf_basename(pbf_path);

    for (label, comp) in compressions {
        run_variant(
            harness,
            pbf_path,
            osc_path,
            &output_path,
            &basename,
            file_mb,
            runs,
            label,
            *comp,
            "buffered",
            false,
            false,
        )?;

        if uring {
            run_variant(
                harness,
                pbf_path,
                osc_path,
                &output_path,
                &basename,
                file_mb,
                runs,
                label,
                *comp,
                "uring",
                true,
                false,
            )?;

            run_variant(
                harness,
                pbf_path,
                osc_path,
                &output_path,
                &basename,
                file_mb,
                runs,
                label,
                *comp,
                "uring+sqpoll",
                true,
                true,
            )?;
        }
    }

    // Clean up the output file (ignore errors if it doesn't exist).
    let _ = std::fs::remove_file(&output_path);

    Ok(())
}

// ---------------------------------------------------------------------------
// Variant runner
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run_variant(
    harness: &BenchHarness,
    pbf_path: &Path,
    osc_path: &Path,
    output_path: &Path,
    basename: &str,
    file_mb: f64,
    runs: usize,
    comp_label: &str,
    comp: Compression,
    mode: &str,
    io_uring: bool,
    sqpoll: bool,
) -> Result<BenchResult, DevError> {
    let variant = format!("{mode}+{comp_label}");
    output::bench_msg(&format!("variant: {variant}"));

    let config = build_config(&variant, basename, file_mb, runs);

    harness.run_internal(&config, |_i| {
        bench_merge(pbf_path, osc_path, output_path, comp, io_uring, sqpoll)
    })
}

// ---------------------------------------------------------------------------
// Core benchmark function
// ---------------------------------------------------------------------------

fn bench_merge(
    pbf_path: &Path,
    osc_path: &Path,
    output_path: &Path,
    comp: Compression,
    io_uring: bool,
    sqpoll: bool,
) -> Result<BenchResult, DevError> {
    // Remove stale output from a previous run (ignore errors).
    let _ = std::fs::remove_file(output_path);

    let start = Instant::now();
    let stats = merge::merge(
        pbf_path,
        osc_path,
        output_path,
        comp,
        false, // direct_io
        io_uring,
        sqpoll,
    )
    .map_err(|e| DevError::Build(format!("merge: {e}")))?;
    let elapsed_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);

    let output_mb = file_size_mb(output_path);

    Ok(BenchResult {
        elapsed_ms,
        extra: Some(serde_json::json!({
            "base_nodes": stats.base_nodes,
            "base_ways": stats.base_ways,
            "base_relations": stats.base_relations,
            "diff_nodes": stats.diff_nodes,
            "diff_ways": stats.diff_ways,
            "diff_relations": stats.diff_relations,
            "blobs_passthrough": stats.blobs_passthrough,
            "blobs_rewritten": stats.blobs_rewritten,
            "output_mb": output_mb,
        })),
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a `BenchConfig` for a merge benchmark variant.
fn build_config(variant: &str, basename: &str, file_mb: f64, runs: usize) -> BenchConfig {
    BenchConfig {
        command: "bench merge".into(),
        variant: Some(variant.into()),
        input_file: Some(basename.into()),
        input_mb: Some(file_mb),
        cargo_features: Some("zlib-ng".into()),
        cargo_profile: "release".into(),
        runs,
    }
}

/// Extract the file basename from a path.
fn pbf_basename(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned()
}

/// Get file size in MB (decimal, consistent with bench scripts).
fn file_size_mb(path: &Path) -> f64 {
    std::fs::metadata(path)
        .map(|m| m.len() as f64 / 1_000_000.0)
        .unwrap_or(0.0)
}

/// Parse an optional zlib level string, falling back to a default.
fn parse_zlib_level(level_str: Option<&str>, default: u32) -> Result<u32, DevError> {
    match level_str {
        Some(s) => s
            .parse()
            .map_err(|_| DevError::Config(format!("invalid compression level: {s}"))),
        None => Ok(default),
    }
}

/// Parse an optional zstd level string, falling back to a default.
fn parse_zstd_level(level_str: Option<&str>, default: i32) -> Result<i32, DevError> {
    match level_str {
        Some(s) => s
            .parse()
            .map_err(|_| DevError::Config(format!("invalid compression level: {s}"))),
        None => Ok(default),
    }
}
