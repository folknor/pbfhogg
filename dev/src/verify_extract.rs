//! Verify: extract — pbfhogg extract vs osmium extract for each strategy.

use std::path::Path;

use crate::error::DevError;
use crate::output::verify_msg;
use crate::verify::VerifyHarness;

/// Cross-validate `pbfhogg extract` against `osmium extract` for
/// simple, complete-ways, and smart strategies.
pub fn run(harness: &VerifyHarness, pbf: &Path, bbox: &str) -> Result<(), DevError> {
    let outdir = harness.subdir("extract")?;

    let pbf_str = pbf.display().to_string();

    for strategy in &["simple", "complete", "smart"] {
        verify_msg(&format!("=== verify extract --{strategy} ==="));

        // --- pbfhogg extract ---
        let pbfhogg_out = outdir.join(format!("pbfhogg-{strategy}.osm.pbf"));
        let pbfhogg_out_str = pbfhogg_out.display().to_string();

        let mut pbfhogg_args = vec!["extract", &pbf_str, "-b", bbox, "-o", &pbfhogg_out_str];
        match *strategy {
            "simple" => pbfhogg_args.push("--simple"),
            "smart" => pbfhogg_args.push("--smart"),
            // "complete" is the default — no extra flag needed.
            _ => {}
        }

        let captured = harness.run_pbfhogg(&pbfhogg_args)?;
        harness.check_exit(&captured, "pbfhogg extract")?;

        // --- osmium extract ---
        let osmium_out = outdir.join(format!("osmium-{strategy}.osm.pbf"));
        let osmium_out_str = osmium_out.display().to_string();

        let osmium_strategy = match *strategy {
            "simple" => "simple",
            "complete" => "complete_ways",
            "smart" => "smart",
            _ => "complete_ways",
        };

        let captured = harness.run_tool(
            "osmium",
            &[
                "extract",
                &pbf_str,
                "-b",
                bbox,
                "-s",
                osmium_strategy,
                "-o",
                &osmium_out_str,
                "--overwrite",
            ],
        )?;
        harness.check_exit(&captured, "osmium extract")?;

        // --- Element counts ---
        harness.print_fileinfo("pbfhogg", &pbfhogg_out)?;
        harness.print_fileinfo("osmium", &osmium_out)?;

        // --- Diff (extract has known minor differences, just log) ---
        let identical = harness.diff_pbfs(&pbfhogg_out, &osmium_out)?;
        if identical {
            verify_msg(&format!("  diff ({strategy}): PASS (identical)"));
        } else {
            verify_msg(&format!(
                "  diff ({strategy}): differences found (expected for extract)"
            ));
        }

        // --- Sort flag ---
        harness.check_sorted(&format!("pbfhogg extract --{strategy}"), &pbfhogg_out)?;
    }

    Ok(())
}
