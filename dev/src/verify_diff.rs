//! Verify: diff — pbfhogg diff vs osmium diff summary comparison.

use std::fs;
use std::path::Path;

use crate::error::DevError;
use crate::output::verify_msg;
use crate::verify::VerifyHarness;

/// Cross-validate `pbfhogg diff` against `osmium diff --summary`.
///
/// Creates a "new" PBF by merging, then diffs old vs new with both tools
/// and compares their summary output and line counts.
pub fn run(harness: &VerifyHarness, pbf: &Path, osc: &Path) -> Result<(), DevError> {
    let outdir = harness.subdir("diff")?;

    verify_msg("=== verify diff ===");
    verify_msg(&format!("  old: {}", pbf.display()));
    verify_msg(&format!("  osc: {} (used to create 'new' via merge)", osc.display()));

    let pbf_str = pbf.display().to_string();
    let osc_str = osc.display().to_string();

    // Create "new" PBF by applying the OSC.
    let new_pbf = outdir.join("new.osm.pbf");
    let new_pbf_str = new_pbf.display().to_string();

    verify_msg("--- creating 'new' PBF via merge ---");
    let captured =
        harness.run_pbfhogg(&["merge", &pbf_str, &osc_str, "-o", &new_pbf_str])?;
    harness.check_exit(&captured, "pbfhogg merge")?;

    // pbfhogg diff — exits non-zero when differences exist, so do NOT check_exit.
    verify_msg("--- pbfhogg diff ---");
    let captured = harness.run_pbfhogg(&["diff", "-c", &pbf_str, &new_pbf_str])?;

    let pbfhogg_diff_path = outdir.join("pbfhogg-diff.txt");
    let pbfhogg_summary_path = outdir.join("pbfhogg-summary.txt");
    fs::write(&pbfhogg_diff_path, &captured.stdout)?;
    fs::write(&pbfhogg_summary_path, &captured.stderr)?;

    // osmium diff — exits non-zero when differences exist, so do NOT check_exit.
    verify_msg("--- osmium diff ---");
    let captured =
        harness.run_tool("osmium", &["diff", &pbf_str, &new_pbf_str, "--summary"])?;

    let osmium_diff_path = outdir.join("osmium-diff.txt");
    let osmium_summary_path = outdir.join("osmium-summary.txt");
    fs::write(&osmium_diff_path, &captured.stdout)?;
    fs::write(&osmium_summary_path, &captured.stderr)?;

    // Print summaries (from stderr).
    verify_msg("=== pbfhogg diff summary ===");
    let pbfhogg_summary_bytes = fs::read(&pbfhogg_summary_path)?;
    let pbfhogg_summary = String::from_utf8_lossy(&pbfhogg_summary_bytes);
    for line in pbfhogg_summary.lines() {
        verify_msg(&format!("  {line}"));
    }

    verify_msg("=== osmium diff summary ===");
    let osmium_summary_bytes = fs::read(&osmium_summary_path)?;
    let osmium_summary = String::from_utf8_lossy(&osmium_summary_bytes);
    for line in osmium_summary.lines() {
        verify_msg(&format!("  {line}"));
    }

    // Line counts from stdout files.
    let pbfhogg_diff_bytes = fs::read(&pbfhogg_diff_path)?;
    let pbfhogg_diff = String::from_utf8_lossy(&pbfhogg_diff_bytes);
    let osmium_diff_bytes = fs::read(&osmium_diff_path)?;
    let osmium_diff = String::from_utf8_lossy(&osmium_diff_bytes);

    let pbfhogg_lines = pbfhogg_diff.lines().count();
    let osmium_lines = osmium_diff.lines().count();

    verify_msg("=== output line counts ===");
    verify_msg(&format!("  pbfhogg: {pbfhogg_lines} lines"));
    verify_msg(&format!("  osmium:  {osmium_lines} lines"));

    Ok(())
}
