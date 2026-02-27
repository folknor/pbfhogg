//! Verify: tags-filter — pbfhogg tags-filter vs osmium tags-filter.

use std::path::Path;

use crate::error::DevError;
use crate::output::verify_msg;
use crate::verify::VerifyHarness;

/// Expression and output label pairs for the three test cases.
const EXPRESSIONS: &[(&str, &str)] = &[
    ("highway=primary", "highway-R"),
    ("amenity=restaurant", "amenity-R"),
    ("w/highway=primary", "w-highway-R"),
];

/// Run tags-filter cross-validation: pbfhogg vs osmium, 3 expressions with `-R`.
pub fn run(harness: &VerifyHarness, pbf: &Path) -> Result<(), DevError> {
    let outdir = harness.subdir("tags-filter")?;
    let pbf_str = pbf.display().to_string();

    for (expr, label) in EXPRESSIONS {
        verify_msg(&format!("--- tags-filter {expr} -R ---"));

        // pbfhogg: tags-filter <pbf> -R <expr> -o <out>
        let pbfhogg_out = outdir.join(format!("pbfhogg-{label}.osm.pbf"));
        let pbfhogg_out_str = pbfhogg_out.display().to_string();
        let captured = harness.run_pbfhogg(&[
            "tags-filter",
            &pbf_str,
            "-R",
            expr,
            "-o",
            &pbfhogg_out_str,
        ])?;
        harness.check_exit(&captured, "pbfhogg tags-filter")?;

        // osmium: tags-filter <pbf> <expr> -R -o <out> --overwrite
        let osmium_out = outdir.join(format!("osmium-{label}.osm.pbf"));
        let osmium_out_str = osmium_out.display().to_string();
        let captured = harness.run_tool("osmium", &[
            "tags-filter",
            &pbf_str,
            expr,
            "-R",
            "-o",
            &osmium_out_str,
            "--overwrite",
        ])?;
        harness.check_exit(&captured, "osmium tags-filter")?;

        // Print fileinfo for both outputs.
        harness.print_fileinfo("pbfhogg", &pbfhogg_out)?;
        harness.print_fileinfo("osmium", &osmium_out)?;

        // Diff and report.
        let identical = harness.diff_pbfs(&pbfhogg_out, &osmium_out)?;
        if identical {
            verify_msg(&format!("  {label}: PASS (identical)"));
        } else {
            verify_msg(&format!("  {label}: FAIL (differences found)"));
        }

        // Compare sort feature flags.
        harness.compare_sort_feature(&pbfhogg_out, &osmium_out)?;
    }

    Ok(())
}
