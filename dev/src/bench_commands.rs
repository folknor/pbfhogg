//! Benchmark: run pbfhogg CLI commands and measure wall-clock time.
//!
//! Each command maps to a set of CLI arguments. The harness runs the
//! pre-built binary as an external subprocess, timing each invocation.

use std::path::Path;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::output;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// All benchmarkable CLI commands.
pub const ALL_COMMANDS: &[&str] = &[
    "cat-way",
    "cat-relation",
    "tags-count",
    "tags-count-way",
    "tags-filter-way",
    "tags-filter-amenity",
    "tags-filter-twopass",
    "getid",
    "removeid",
    "add-locations-to-ways",
    "extract-simple",
    "extract-complete",
    "extract-smart",
    "node-stats",
];

// ---------------------------------------------------------------------------
// Parse helpers
// ---------------------------------------------------------------------------

/// Parse a command name into one or more benchmark command names.
///
/// If `input` is `"all"`, returns all commands. Otherwise, looks up the
/// single command by name. Returns an error for unknown names.
pub fn parse_command(input: &str) -> Result<Vec<&'static str>, DevError> {
    if input == "all" {
        return Ok(ALL_COMMANDS.to_vec());
    }

    for &cmd in ALL_COMMANDS {
        if cmd == input {
            return Ok(vec![cmd]);
        }
    }

    Err(DevError::Config(format!("unknown command: {input}")))
}

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
        "tags-count" => vec![
            "tags-count".into(), pbf.into(),
            "--min-count".into(), "999999999".into(),
        ],
        "tags-count-way" => vec![
            "tags-count".into(), pbf.into(), "--type".into(), "way".into(),
            "--min-count".into(), "999999999".into(),
        ],
        "tags-filter-way" => vec![
            "tags-filter".into(), pbf.into(),
            "-R".into(), "w/highway=primary".into(),
            "-o".into(), "/dev/null".into(),
        ],
        "tags-filter-amenity" => vec![
            "tags-filter".into(), pbf.into(),
            "-R".into(), "amenity=restaurant".into(),
            "-o".into(), "/dev/null".into(),
        ],
        "tags-filter-twopass" => vec![
            "tags-filter".into(), pbf.into(),
            "highway=primary".into(),
            "-o".into(), "/dev/null".into(),
        ],
        "getid" => vec![
            "getid".into(), pbf.into(),
            "n115722".into(), "n115723".into(), "n115724".into(),
            "w2080".into(), "w2081".into(), "w2082".into(),
            "r174".into(), "r213".into(), "r339".into(),
            "-o".into(), "/dev/null".into(),
        ],
        "removeid" => vec![
            "removeid".into(), pbf.into(),
            "n115722".into(), "n115723".into(), "n115724".into(),
            "w2080".into(), "w2081".into(), "w2082".into(),
            "r174".into(), "r213".into(), "r339".into(),
            "-o".into(), "/dev/null".into(),
        ],
        "add-locations-to-ways" => vec![
            "add-locations-to-ways".into(), pbf.into(),
            "-o".into(), "/dev/null".into(),
        ],
        "extract-simple" => vec![
            "extract".into(), pbf.into(), "--simple".into(),
            "-b".into(), "12.4,55.6,12.7,55.8".into(),
            "-o".into(), "/dev/null".into(),
        ],
        "extract-complete" => vec![
            "extract".into(), pbf.into(),
            "-b".into(), "12.4,55.6,12.7,55.8".into(),
            "-o".into(), "/dev/null".into(),
        ],
        "extract-smart" => vec![
            "extract".into(), pbf.into(), "--smart".into(),
            "-b".into(), "12.4,55.6,12.7,55.8".into(),
            "-o".into(), "/dev/null".into(),
        ],
        "node-stats" => vec![
            "node-stats".into(), pbf.into(),
        ],
        // parse_command validates the name, so this branch is unreachable
        // in normal usage. Return empty args to satisfy the match.
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the command benchmark for each requested command name.
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    commands: &[&str],
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

    for &name in commands {
        output::bench_msg(&format!("command: {name}"));

        let args = command_args(name, pbf_str);
        let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();

        let config = BenchConfig {
            command: "bench commands".into(),
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
