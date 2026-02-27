//! Benchmark: extract strategies (simple/complete/smart) with bbox.
//!
//! Runs the pre-built binary as an external subprocess for each strategy,
//! timing each invocation via the harness.

use std::path::Path;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::output;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub const ALL_STRATEGIES: &[&str] = &["simple", "complete", "smart"];

// ---------------------------------------------------------------------------
// Parse helpers
// ---------------------------------------------------------------------------

pub fn parse_strategies(input: &str) -> Result<Vec<&'static str>, DevError> {
    let mut out = Vec::new();
    for part in input.split(',') {
        let part = part.trim();
        let found = ALL_STRATEGIES.iter().find(|&&s| s == part);
        match found {
            Some(&s) => out.push(s),
            None => return Err(DevError::Config(format!("unknown strategy: {part}"))),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Command argument builder
// ---------------------------------------------------------------------------

fn strategy_args(name: &str, pbf: &str, bbox: &str) -> Vec<String> {
    match name {
        "simple" => vec![
            "extract".into(), pbf.into(), "--simple".into(),
            "-b".into(), bbox.into(),
            "-o".into(), "/dev/null".into(),
        ],
        "complete" => vec![
            "extract".into(), pbf.into(),
            "-b".into(), bbox.into(),
            "-o".into(), "/dev/null".into(),
        ],
        "smart" => vec![
            "extract".into(), pbf.into(), "--smart".into(),
            "-b".into(), bbox.into(),
            "-o".into(), "/dev/null".into(),
        ],
        // parse_strategies validates the name, so this branch is unreachable
        // in normal usage. Return empty args to satisfy the match.
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    bbox: &str,
    strategies: &[&str],
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

    for &name in strategies {
        output::bench_msg(&format!("strategy: {name}"));

        let args = strategy_args(name, pbf_str, bbox);
        let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();

        let config = BenchConfig {
            command: "bench extract".into(),
            variant: Some(name.into()),
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: None,
            cargo_profile: "release".into(),
            runs,
        };

        harness.run_external(&config, binary, &args_refs, workspace_root)?;
    }

    Ok(())
}
