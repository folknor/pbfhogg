//! Verify: getid/removeid — pbfhogg getid vs osmium getid, plus removeid complement test.

use std::path::Path;

use crate::error::DevError;
use crate::output::verify_msg;
use crate::verify::VerifyHarness;

/// Element IDs known to exist in Denmark PBFs.
const IDS: &[&str] = &[
    "n115722", "n115723", "n115724",
    "w2080", "w2081", "w2082",
    "r174", "r213", "r339",
];

/// Run getid/removeid cross-validation: pbfhogg getid vs osmium getid,
/// then pbfhogg removeid complement test.
pub fn run(harness: &VerifyHarness, pbf: &Path) -> Result<(), DevError> {
    let outdir = harness.subdir("getid-removeid")?;
    let pbf_str = pbf.display().to_string();

    verify_msg("--- getid: pbfhogg vs osmium ---");

    // pbfhogg getid <pbf> -o <out> <ids...>
    let pbfhogg_getid = outdir.join("pbfhogg-getid.osm.pbf");
    let pbfhogg_getid_str = pbfhogg_getid.display().to_string();
    let mut pbfhogg_args: Vec<&str> = vec!["getid", &pbf_str, "-o", &pbfhogg_getid_str];
    pbfhogg_args.extend_from_slice(IDS);
    let captured = harness.run_pbfhogg(&pbfhogg_args)?;
    harness.check_exit(&captured, "pbfhogg getid")?;

    // osmium getid <pbf> <ids...> -o <out> --overwrite
    let osmium_getid = outdir.join("osmium-getid.osm.pbf");
    let osmium_getid_str = osmium_getid.display().to_string();
    let mut osmium_args: Vec<&str> = vec!["getid", &pbf_str];
    osmium_args.extend_from_slice(IDS);
    osmium_args.extend_from_slice(&["-o", &osmium_getid_str, "--overwrite"]);
    let captured = harness.run_tool("osmium", &osmium_args)?;
    harness.check_exit(&captured, "osmium getid")?;

    // Print fileinfo for both getid outputs.
    harness.print_fileinfo("pbfhogg getid", &pbfhogg_getid)?;
    harness.print_fileinfo("osmium getid", &osmium_getid)?;

    // Diff and report.
    let identical = harness.diff_pbfs(&pbfhogg_getid, &osmium_getid)?;
    if identical {
        verify_msg("  getid: PASS (identical)");
    } else {
        verify_msg("  getid: FAIL (differences found)");
    }

    // Compare sort feature flags.
    harness.compare_sort_feature(&pbfhogg_getid, &osmium_getid)?;

    // --- removeid: complement test ---
    verify_msg("--- removeid: complement test ---");

    let pbfhogg_removeid = outdir.join("pbfhogg-removeid.osm.pbf");
    let pbfhogg_removeid_str = pbfhogg_removeid.display().to_string();
    let mut removeid_args: Vec<&str> = vec!["removeid", &pbf_str, "-o", &pbfhogg_removeid_str];
    removeid_args.extend_from_slice(IDS);
    let captured = harness.run_pbfhogg(&removeid_args)?;
    harness.check_exit(&captured, "pbfhogg removeid")?;

    // Print fileinfo for original, getid, and removeid (complement validation).
    harness.print_fileinfo("original", pbf)?;
    harness.print_fileinfo("getid", &pbfhogg_getid)?;
    harness.print_fileinfo("removeid", &pbfhogg_removeid)?;

    Ok(())
}
