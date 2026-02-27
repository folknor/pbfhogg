//! Verify: merge — 4-tool comparison: pbfhogg, osmium, osmosis, osmconvert.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::error::DevError;
use crate::output;
use crate::output::verify_msg;
use crate::tools::OsmosisTools;
use crate::verify::{self, VerifyHarness};

/// Cross-validate `pbfhogg merge` against osmium, osmosis, and osmconvert.
pub fn run(
    harness: &VerifyHarness,
    pbf: &Path,
    osc: &Path,
    osmosis: Option<&OsmosisTools>,
) -> Result<(), DevError> {
    let outdir = harness.subdir("merge")?;

    verify_msg("=== verify merge ===");
    verify_msg(&format!("  base: {}", pbf.display()));
    verify_msg(&format!("  diff: {}", osc.display()));

    let pbf_str = pbf.display().to_string();
    let osc_str = osc.display().to_string();

    // --- pbfhogg merge ---
    let pbfhogg_out = outdir.join("pbfhogg.osm.pbf");
    let pbfhogg_out_str = pbfhogg_out.display().to_string();

    verify_msg("--- pbfhogg merge ---");
    let captured =
        harness.run_pbfhogg(&["merge", &pbf_str, &osc_str, "-o", &pbfhogg_out_str])?;
    harness.check_exit(&captured, "pbfhogg merge")?;

    // --- osmium apply-changes ---
    let osmium_out = outdir.join("osmium.osm.pbf");
    let osmium_out_str = osmium_out.display().to_string();

    verify_msg("--- osmium apply-changes ---");
    let captured = harness.run_tool(
        "osmium",
        &["apply-changes", &pbf_str, &osc_str, "-o", &osmium_out_str, "--overwrite"],
    )?;
    harness.check_exit(&captured, "osmium apply-changes")?;

    // --- osmosis (optional) ---
    let osmosis_out = outdir.join("osmosis.osm.pbf");
    if let Some(tools) = osmosis {
        verify_msg("--- osmosis --apply-change ---");
        let osmosis_out_str = osmosis_out.display().to_string();
        match run_osmosis(
            &tools.osmosis,
            &tools.java_home,
            &[
                "--read-xml-change",
                &format!("file={osc_str}"),
                "--read-pbf",
                &format!("file={pbf_str}"),
                "--apply-change",
                "--write-pbf",
                &format!("file={osmosis_out_str}"),
            ],
            &harness.workspace_root,
        ) {
            Ok(captured) => {
                if let Err(e) = harness.check_exit(&captured, "osmosis") {
                    verify_msg(&format!("  osmosis failed: {e}"));
                }
            }
            Err(e) => {
                verify_msg(&format!("  osmosis skipped: {e}"));
            }
        }
    }

    // --- osmconvert (optional) ---
    let osmconvert_out = outdir.join("osmconvert.osm.pbf");
    if verify::which_exists("osmconvert") {
        verify_msg("--- osmconvert ---");
        let osmconvert_out_str = osmconvert_out.display().to_string();
        let out_arg = format!("-o={osmconvert_out_str}");
        match harness.run_tool("osmconvert", &[&pbf_str, &osc_str, &out_arg]) {
            Ok(captured) => {
                if let Err(e) = harness.check_exit(&captured, "osmconvert") {
                    verify_msg(&format!("  osmconvert failed: {e}"));
                }
            }
            Err(e) => {
                verify_msg(&format!("  osmconvert skipped: {e}"));
            }
        }
    }

    // --- Element counts ---
    verify_msg("=== element counts ===");
    harness.print_fileinfo("pbfhogg", &pbfhogg_out)?;
    harness.print_fileinfo("osmium", &osmium_out)?;
    if osmosis_out.exists() {
        harness.print_fileinfo("osmosis", &osmosis_out)?;
    }
    if osmconvert_out.exists() {
        harness.print_fileinfo("osmconvert", &osmconvert_out)?;
    }

    // --- Sort check ---
    harness.check_sorted("pbfhogg merge", &pbfhogg_out)?;

    Ok(())
}

/// Run osmosis with `JAVA_HOME` set, returning a `CapturedOutput`.
fn run_osmosis(
    osmosis: &Path,
    java_home: &Path,
    args: &[&str],
    cwd: &Path,
) -> Result<output::CapturedOutput, DevError> {
    let start = Instant::now();
    let result = Command::new(osmosis.display().to_string())
        .args(args)
        .env("JAVA_HOME", java_home.display().to_string())
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| DevError::Subprocess {
            program: "osmosis".into(),
            code: None,
            stderr: e.to_string(),
        })?;

    Ok(output::CapturedOutput {
        status: result.status,
        stdout: result.stdout,
        stderr: result.stderr,
        elapsed: start.elapsed(),
    })
}
