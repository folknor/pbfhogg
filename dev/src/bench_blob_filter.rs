//! Benchmark: compare indexed (with indexdata) vs raw (without) PBF performance.
//!
//! Runs 4 CLI commands against both an indexed PBF and a raw PBF,
//! measuring wall-clock time via external subprocess timing.

use std::path::Path;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::output;

// ---------------------------------------------------------------------------
// Command list
// ---------------------------------------------------------------------------

const COMMANDS: &[&str] = &["cat-way", "cat-relation", "tags-count-way", "node-stats"];

// ---------------------------------------------------------------------------
// Command argument builder
// ---------------------------------------------------------------------------

/// Build the CLI argument list for a given benchmark command.
fn command_args(name: &str, pbf: &str) -> Vec<String> {
    match name {
        "cat-way" => vec![
            "cat".into(), pbf.into(), "--type".into(), "way".into(),
            "-o".into(), "/dev/null".into(),
        ],
        "cat-relation" => vec![
            "cat".into(), pbf.into(), "--type".into(), "relation".into(),
            "-o".into(), "/dev/null".into(),
        ],
        "tags-count-way" => vec![
            "tags-count".into(), pbf.into(), "--type".into(), "way".into(),
            "--min-count".into(), "999999999".into(),
        ],
        "node-stats" => vec![
            "node-stats".into(), pbf.into(),
        ],
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the blob-filter benchmark for each command against indexed and raw PBFs.
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_indexed: &Path,
    pbf_raw: &Path,
    file_mb: f64,
    runs: usize,
    workspace_root: &Path,
) -> Result<(), DevError> {
    let indexed_str = pbf_indexed
        .to_str()
        .ok_or_else(|| DevError::Config("indexed PBF path is not valid UTF-8".into()))?;

    let raw_str = pbf_raw
        .to_str()
        .ok_or_else(|| DevError::Config("raw PBF path is not valid UTF-8".into()))?;

    let indexed_basename = pbf_indexed
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    let raw_basename = pbf_raw
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    let variants: &[(&str, &str, &str)] = &[
        ("indexed", indexed_str, &indexed_basename),
        ("raw", raw_str, &raw_basename),
    ];

    for &cmd in COMMANDS {
        for &(label_suffix, pbf_str, basename) in variants {
            let variant = format!("{cmd}+{label_suffix}");
            output::bench_msg(&format!("variant: {variant}"));

            let args = command_args(cmd, pbf_str);
            let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();

            let config = BenchConfig {
                command: "bench blob-filter".into(),
                variant: Some(variant),
                input_file: Some(basename.to_owned()),
                input_mb: Some(file_mb),
                cargo_features: None,
                cargo_profile: "release".into(),
                runs,
            };

            harness.run_external(&config, binary, &args_refs, workspace_root)?;
        }
    }

    Ok(())
}
