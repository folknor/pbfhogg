//! Verify: cat — pbfhogg cat vs osmium cat for each element type.

use std::path::Path;

use crate::error::DevError;
use crate::output::verify_msg;
use crate::verify::VerifyHarness;

/// Cross-validate `pbfhogg cat` against `osmium cat` for node/way/relation types.
pub fn run(harness: &VerifyHarness, pbf: &Path) -> Result<(), DevError> {
    let outdir = harness.subdir("cat")?;

    let pbf_str = pbf.display().to_string();

    for elem_type in &["node", "way", "relation"] {
        verify_msg(&format!("=== verify cat -t {elem_type} ==="));

        // --- pbfhogg cat ---
        let pbfhogg_out = outdir.join(format!("pbfhogg-{elem_type}.osm.pbf"));
        let pbfhogg_out_str = pbfhogg_out.display().to_string();

        let captured =
            harness.run_pbfhogg(&["cat", &pbf_str, "-t", elem_type, "-o", &pbfhogg_out_str])?;
        harness.check_exit(&captured, "pbfhogg cat")?;

        // --- osmium cat ---
        let osmium_out = outdir.join(format!("osmium-{elem_type}.osm.pbf"));
        let osmium_out_str = osmium_out.display().to_string();

        let captured = harness.run_tool(
            "osmium",
            &["cat", &pbf_str, "-t", elem_type, "-o", &osmium_out_str, "--overwrite"],
        )?;
        harness.check_exit(&captured, "osmium cat")?;

        // --- Element counts ---
        harness.print_fileinfo("pbfhogg", &pbfhogg_out)?;
        harness.print_fileinfo("osmium", &osmium_out)?;

        // --- Diff ---
        let identical = harness.diff_pbfs(&pbfhogg_out, &osmium_out)?;
        if identical {
            verify_msg(&format!("  diff ({elem_type}): PASS (identical)"));
        } else {
            verify_msg(&format!("  diff ({elem_type}): FAIL (differences found)"));
        }

        // --- Sort feature comparison ---
        harness.compare_sort_feature(&pbfhogg_out, &osmium_out)?;
    }

    Ok(())
}
