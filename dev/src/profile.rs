//! Two-pass profiling: timing instrumentation followed by allocation tracking.
//!
//! Replaces `profile-region.sh`. Builds the CLI binary twice — once with
//! `--features hotpath` for timing, once with `--features hotpath-alloc` for
//! allocation metrics.

use std::path::Path;

use crate::build;
use crate::error::DevError;
use crate::hotpath;
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn run(
    pbf_path: &Path,
    pbf_raw_path: Option<&Path>,
    osc_path: &Path,
    dataset_name: &str,
    file_mb: f64,
    scratch_dir: &Path,
    workspace_root: &Path,
) -> Result<(), DevError> {
    let _lock = crate::lockfile::acquire(scratch_dir)?;

    // Compute display sizes.
    let raw_mb: Option<f64> = pbf_raw_path.and_then(|p| {
        std::fs::metadata(p).ok().map(|m| m.len() as f64 / 1_000_000.0)
    });

    output::hotpath_msg(&format!("=== {dataset_name} ({file_mb:.0} MB) ==="));

    // -----------------------------------------------------------------------
    // TIMING PASS
    // -----------------------------------------------------------------------

    output::hotpath_msg("=== TIMING PASS ===");

    let binary = build::cargo_build(
        &build::BuildConfig::release_cli_with_features(&["hotpath"]),
        workspace_root,
    )?;

    std::fs::create_dir_all(scratch_dir)?;
    let merged = scratch_dir.join("profile-merged.osm.pbf");

    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;
    let osc_str = osc_path
        .to_str()
        .ok_or_else(|| DevError::Config("OSC path is not valid UTF-8".into()))?;
    let merged_str = merged
        .to_str()
        .ok_or_else(|| DevError::Config("merged path is not valid UTF-8".into()))?;

    // --- tags-count ---
    run_test(&binary, "tags-count", &["tags-count", pbf_str], workspace_root)?;

    // --- check-refs ---
    run_test(&binary, "check-refs", &["check-refs", pbf_str], workspace_root)?;

    // --- cat --type ---
    run_test(
        &binary,
        "cat --type",
        &["cat", pbf_str, "--type", "node,way,relation", "--compression", "zlib", "-o", "/dev/null"],
        workspace_root,
    )?;

    // --- merge: no indexdata, zlib ---
    match pbf_raw_path {
        Some(raw_path) => {
            let pbf_raw_str = raw_path
                .to_str()
                .ok_or_else(|| DevError::Config("raw PBF path is not valid UTF-8".into()))?;

            let raw_mb_display = raw_mb
                .map(|mb| format!(" ({mb:.0} MB)"))
                .unwrap_or_default();

            run_test(
                &binary,
                &format!("merge: no indexdata, zlib{raw_mb_display}"),
                &["merge", pbf_raw_str, osc_str, "--compression", "zlib", "-o", merged_str],
                workspace_root,
            )?;
        }
        None => {
            output::hotpath_msg("--- merge: no indexdata, zlib --- (skipped, no raw PBF)");
            println!();
        }
    }

    // --- merge: indexdata, zlib ---
    run_test(
        &binary,
        "merge: indexdata, zlib",
        &["merge", pbf_str, osc_str, "--compression", "zlib", "-o", merged_str],
        workspace_root,
    )?;

    // --- merge: indexdata, none ---
    run_test(
        &binary,
        "merge: indexdata, none",
        &["merge", pbf_str, osc_str, "--compression", "none", "-o", merged_str],
        workspace_root,
    )?;

    // -----------------------------------------------------------------------
    // ALLOCATION PASS
    // -----------------------------------------------------------------------

    output::hotpath_msg("=== ALLOCATION PASS ===");

    let binary = build::cargo_build(
        &build::BuildConfig::release_cli_with_features(&["hotpath-alloc"]),
        workspace_root,
    )?;

    // --- cat --type (alloc) ---
    run_test(
        &binary,
        "cat --type (alloc)",
        &["cat", pbf_str, "--type", "node,way,relation", "--compression", "zlib", "-o", "/dev/null"],
        workspace_root,
    )?;

    // --- merge: indexdata, none (alloc) ---
    run_test(
        &binary,
        "merge: indexdata, none (alloc)",
        &["merge", pbf_str, osc_str, "--compression", "none", "-o", merged_str],
        workspace_root,
    )?;

    // -----------------------------------------------------------------------
    // Done
    // -----------------------------------------------------------------------

    output::hotpath_msg(&format!("=== {dataset_name} COMPLETE ==="));

    let _ = std::fs::remove_file(&merged);

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Print a label, run a hotpath-instrumented command, and print an empty line.
fn run_test(
    binary: &Path,
    label: &str,
    args: &[&str],
    workspace_root: &Path,
) -> Result<(), DevError> {
    output::hotpath_msg(&format!("--- {label} ---"));
    hotpath::run_hotpath_command(binary, args, workspace_root)?;
    println!();
    Ok(())
}
