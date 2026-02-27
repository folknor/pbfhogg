//! Benchmark: compare allocators (default, jemalloc, mimalloc) via check-refs.

use std::path::Path;

use crate::build::{self, BuildConfig};
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::output;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// All benchmarkable allocators.
pub const ALL_ALLOCATORS: &[&str] = &["default", "jemalloc", "mimalloc"];

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the allocator benchmark: rebuild the CLI with each allocator and run
/// check-refs, measuring wall-clock time.
pub fn run(
    harness: &BenchHarness,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    workspace_root: &Path,
) -> Result<(), DevError> {
    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    for &name in ALL_ALLOCATORS {
        output::bench_msg(&format!("allocator: {name}"));

        let build_config = match name {
            "jemalloc" => BuildConfig::release_cli_with_features(&["jemalloc"]),
            "mimalloc" => BuildConfig::release_cli_with_features(&["mimalloc"]),
            _ => BuildConfig::release_cli(),
        };

        let binary = build::cargo_build(&build_config, workspace_root)?;

        let args: Vec<&str> = vec!["check-refs", pbf_str];

        let features_label = match name {
            "jemalloc" => Some("jemalloc".into()),
            "mimalloc" => Some("mimalloc".into()),
            _ => None,
        };

        let config = BenchConfig {
            command: "bench allocator".into(),
            variant: Some(name.into()),
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: features_label,
            cargo_profile: "release".into(),
            runs,
        };

        harness.run_external(&config, &binary, &args, workspace_root)?;
    }

    Ok(())
}
