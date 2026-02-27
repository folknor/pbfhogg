//! Verify: sort — pbfhogg sort vs osmium sort.

use std::path::Path;

use crate::error::DevError;
use crate::output::verify_msg;
use crate::verify::VerifyHarness;

/// Cross-validate `pbfhogg sort` against `osmium sort`.
pub fn run(harness: &VerifyHarness, pbf: &Path) -> Result<(), DevError> {
    let outdir = harness.subdir("sort")?;

    verify_msg("=== verify sort ===");

    // --- pbfhogg sort ---
    let pbf_str = pbf.display().to_string();
    let pbfhogg_out = outdir.join("pbfhogg.osm.pbf");
    let pbfhogg_out_str = pbfhogg_out.display().to_string();

    let captured = harness.run_pbfhogg(&["sort", &pbf_str, "-o", &pbfhogg_out_str])?;
    harness.check_exit(&captured, "pbfhogg sort")?;

    // --- osmium sort ---
    let osmium_out = outdir.join("osmium.osm.pbf");
    let osmium_out_str = osmium_out.display().to_string();

    let captured =
        harness.run_tool("osmium", &["sort", &pbf_str, "-o", &osmium_out_str, "--overwrite"])?;
    harness.check_exit(&captured, "osmium sort")?;

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

    // --- Sort flag ---
    harness.check_sorted("pbfhogg sort", &pbfhogg_out)?;

    Ok(())
}
