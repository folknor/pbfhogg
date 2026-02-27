//! Verify: add-locations-to-ways — pbfhogg vs osmium, plus optional dense index test.

use std::path::Path;

use crate::error::DevError;
use crate::output::verify_msg;
use crate::verify::VerifyHarness;

/// Cross-validate `pbfhogg add-locations-to-ways` against `osmium add-locations-to-ways`.
///
/// Also runs an optional dense index variant — if the dense allocation fails
/// (common on systems without `vm.overcommit_memory=1`), the failure is logged
/// and the function still returns `Ok`.
pub fn run(harness: &VerifyHarness, pbf: &Path) -> Result<(), DevError> {
    let outdir = harness.subdir("add-locations-to-ways")?;

    verify_msg("=== verify add-locations-to-ways ===");

    let pbf_str = pbf.display().to_string();

    // --- pbfhogg add-locations-to-ways (default hash index) ---
    let pbfhogg_out = outdir.join("pbfhogg.osm.pbf");
    let pbfhogg_out_str = pbfhogg_out.display().to_string();

    let captured =
        harness.run_pbfhogg(&["add-locations-to-ways", &pbf_str, "-o", &pbfhogg_out_str])?;
    harness.check_exit(&captured, "pbfhogg add-locations-to-ways")?;

    // --- osmium add-locations-to-ways ---
    let osmium_out = outdir.join("osmium.osm.pbf");
    let osmium_out_str = osmium_out.display().to_string();

    let captured = harness.run_tool(
        "osmium",
        &[
            "add-locations-to-ways",
            &pbf_str,
            "-o",
            &osmium_out_str,
            "--overwrite",
        ],
    )?;
    harness.check_exit(&captured, "osmium add-locations-to-ways")?;

    // --- Element counts ---
    harness.print_fileinfo("pbfhogg", &pbfhogg_out)?;
    harness.print_fileinfo("osmium", &osmium_out)?;

    // --- Diff ---
    let identical = harness.diff_pbfs(&pbfhogg_out, &osmium_out)?;
    if identical {
        verify_msg("  diff: PASS (identical)");
    } else {
        verify_msg("  diff: FAIL (differences found)");
    }

    // --- Sort feature comparison ---
    harness.compare_sort_feature(&pbfhogg_out, &osmium_out)?;

    // --- Optional dense index variant ---
    verify_msg("--- dense index variant ---");

    let dense_out = outdir.join("pbfhogg-dense.osm.pbf");
    let dense_out_str = dense_out.display().to_string();

    let dense_result = harness.run_pbfhogg(&[
        "add-locations-to-ways",
        &pbf_str,
        "-o",
        &dense_out_str,
        "--index-type",
        "dense",
    ])?;

    if dense_result.status.success() {
        let identical = harness.diff_pbfs(&pbfhogg_out, &dense_out)?;
        if identical {
            verify_msg("  diff (hash vs dense): PASS (identical)");
        } else {
            verify_msg("  diff (hash vs dense): FAIL (differences found)");
        }
    } else {
        verify_msg("  dense index skipped (allocation failed — expected on systems without vm.overcommit_memory=1)");
    }

    Ok(())
}
