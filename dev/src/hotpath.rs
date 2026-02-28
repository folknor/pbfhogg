//! Hotpath profiling: function-level timing and allocation instrumentation.
//!
//! Consolidates `run-hotpath.sh`, `run-hotpath-alloc.sh`, and
//! `run-hotpath-germany.sh` into a single command.

use std::path::Path;
use std::time::Duration;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, BenchResult};
use crate::output::{self, CapturedOutput};

// ---------------------------------------------------------------------------
// Shared helper (used by hotpath.rs and profile.rs)
// ---------------------------------------------------------------------------

/// Run a hotpath-instrumented subprocess and extract the `[hotpath]` block.
///
/// Sets `HOTPATH_METRICS_SERVER_OFF=true` in the subprocess environment.
/// Does NOT check exit status — commands like `check-refs` may exit non-zero
/// when missing refs are found, which is expected.
pub fn run_hotpath_command(
    binary: &Path,
    args: &[&str],
    cwd: &Path,
) -> Result<CapturedOutput, DevError> {
    let program = binary.display().to_string();
    let captured = output::run_captured_with_env(
        &program,
        args,
        cwd,
        &[("HOTPATH_METRICS_SERVER_OFF", "true")],
    )?;

    let block = extract_hotpath_block(&captured.stdout);
    if !block.is_empty() {
        print!("{block}");
    }

    Ok(captured)
}

// ---------------------------------------------------------------------------
// Hotpath block extraction
// ---------------------------------------------------------------------------

/// Extract everything from the first `[hotpath]` line to end of stdout.
fn extract_hotpath_block(stdout: &[u8]) -> String {
    let text = String::from_utf8_lossy(stdout);
    let mut result = String::new();
    let mut found = false;

    for line in text.lines() {
        if !found {
            if line.starts_with("[hotpath]") {
                found = true;
                result.push_str(line);
                result.push('\n');
            }
        } else {
            result.push_str(line);
            result.push('\n');
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Elapsed conversion
// ---------------------------------------------------------------------------

fn elapsed_to_ms(duration: &Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

// ---------------------------------------------------------------------------
// Test suite definition
// ---------------------------------------------------------------------------

struct HotpathTest {
    label: &'static str,
    args: Vec<String>,
}

fn build_test_suite(
    binary_str: &str,
    pbf_str: &str,
    osc_str: &str,
    merged_str: &str,
    pbf_raw_str: Option<&str>,
) -> Vec<HotpathTest> {
    let mut tests = vec![
        HotpathTest {
            label: "tags-count",
            args: vec![
                binary_str.into(),
                "tags-count".into(),
                pbf_str.into(),
            ],
        },
        HotpathTest {
            label: "check-refs",
            args: vec![
                binary_str.into(),
                "check-refs".into(),
                pbf_str.into(),
            ],
        },
        HotpathTest {
            label: "cat",
            args: vec![
                binary_str.into(),
                "cat".into(),
                pbf_str.into(),
                "--type".into(),
                "node,way,relation".into(),
                "--compression".into(),
                "zlib".into(),
                "-o".into(),
                "/dev/null".into(),
            ],
        },
        HotpathTest {
            label: "merge",
            args: vec![
                binary_str.into(),
                "merge".into(),
                pbf_str.into(),
                osc_str.into(),
                "--compression".into(),
                "zlib".into(),
                "-o".into(),
                merged_str.into(),
            ],
        },
    ];

    if let Some(raw_str) = pbf_raw_str {
        tests.push(HotpathTest {
            label: "merge-no-indexdata",
            args: vec![
                binary_str.into(),
                "merge".into(),
                raw_str.into(),
                osc_str.into(),
                "--compression".into(),
                "zlib".into(),
                "-o".into(),
                merged_str.into(),
            ],
        });

        tests.push(HotpathTest {
            label: "merge-none",
            args: vec![
                binary_str.into(),
                "merge".into(),
                pbf_str.into(),
                osc_str.into(),
                "--compression".into(),
                "none".into(),
                "-o".into(),
                merged_str.into(),
            ],
        });
    }

    tests
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the hotpath profiling suite.
///
/// Runs each test `runs` times, recording results through the bench harness.
/// When `alloc` is true, the binary is expected to be built with `hotpath-alloc`
/// feature; the variant name gets a `/alloc` suffix.
#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_path: &Path,
    pbf_raw_path: Option<&Path>,
    osc_path: &Path,
    file_mb: f64,
    runs: usize,
    alloc: bool,
    scratch_dir: &Path,
    workspace_root: &Path,
) -> Result<(), DevError> {
    std::fs::create_dir_all(scratch_dir)?;
    let merged_path = scratch_dir.join("hotpath-merged.osm.pbf");

    let binary_str = binary
        .to_str()
        .ok_or_else(|| DevError::Config("binary path is not valid UTF-8".into()))?;
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;
    let osc_str = osc_path
        .to_str()
        .ok_or_else(|| DevError::Config("OSC path is not valid UTF-8".into()))?;
    let merged_str = merged_path
        .to_str()
        .ok_or_else(|| DevError::Config("merged path is not valid UTF-8".into()))?;
    let pbf_raw_str = pbf_raw_path
        .map(|p| {
            p.to_str()
                .ok_or_else(|| DevError::Config("raw PBF path is not valid UTF-8".into()))
        })
        .transpose()?;

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    let feature = if alloc { "hotpath-alloc" } else { "hotpath" };
    let tests = build_test_suite(binary_str, pbf_str, osc_str, merged_str, pbf_raw_str);

    for test in &tests {
        let variant_suffix = if alloc { "/alloc" } else { "" };
        let variant = format!("{}{variant_suffix}", test.label);

        let config = BenchConfig {
            command: "hotpath".into(),
            variant: Some(variant),
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: Some(feature.into()),
            cargo_profile: "release".into(),
            runs,
        };

        // Build args without the binary path (first element) for the subprocess.
        let subprocess_args: Vec<&str> = test.args[1..].iter().map(String::as_str).collect();

        harness.run_internal(&config, |_i| {
            output::hotpath_msg(test.label);
            let captured = run_hotpath_command(binary, &subprocess_args, workspace_root)?;
            let ms = elapsed_to_ms(&captured.elapsed);
            Ok(BenchResult {
                elapsed_ms: ms,
                extra: None,
            })
        })?;
    }

    // Clean up merged output file (ignore errors if it doesn't exist).
    let _ = std::fs::remove_file(&merged_path);

    Ok(())
}
