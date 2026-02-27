//! Verify: derive-changes — roundtrip validation via pbfhogg and osmium.

use std::path::Path;

use crate::error::DevError;
use crate::output::verify_msg;
use crate::verify::VerifyHarness;

/// Cross-validate `pbfhogg derive-changes` against `osmium derive-changes`.
///
/// Creates a "new" PBF by merging, derives changes from old->new with both
/// tools, then roundtrips each derived OSC back through merge and compares.
pub fn run(harness: &VerifyHarness, pbf: &Path, osc: &Path) -> Result<(), DevError> {
    let outdir = harness.subdir("derive-changes")?;

    verify_msg("=== verify derive-changes ===");
    verify_msg(&format!("  old: {}", pbf.display()));
    verify_msg(&format!("  osc: {} (used to create 'new' via merge)", osc.display()));

    let pbf_str = pbf.display().to_string();
    let osc_str = osc.display().to_string();

    // Step 1: Create "new" PBF by applying the OSC.
    let new_pbf = outdir.join("new.osm.pbf");
    let new_pbf_str = new_pbf.display().to_string();

    verify_msg("--- creating 'new' PBF via merge ---");
    let captured =
        harness.run_pbfhogg(&["merge", &pbf_str, &osc_str, "-o", &new_pbf_str])?;
    harness.check_exit(&captured, "pbfhogg merge")?;

    // Step 2: Derive changes with both tools.
    let pbfhogg_osc = outdir.join("pbfhogg.osc.gz");
    let pbfhogg_osc_str = pbfhogg_osc.display().to_string();

    verify_msg("--- pbfhogg derive-changes ---");
    let captured = harness.run_pbfhogg(&[
        "derive-changes", &pbf_str, &new_pbf_str, "-o", &pbfhogg_osc_str,
    ])?;
    harness.check_exit(&captured, "pbfhogg derive-changes")?;

    let osmium_osc = outdir.join("osmium.osc.gz");
    let osmium_osc_str = osmium_osc.display().to_string();

    verify_msg("--- osmium derive-changes ---");
    let captured = harness.run_tool(
        "osmium",
        &["derive-changes", &pbf_str, &new_pbf_str, "-o", &osmium_osc_str, "--overwrite"],
    )?;
    harness.check_exit(&captured, "osmium derive-changes")?;

    // Step 3: Report OSC file sizes.
    verify_msg("=== OSC file sizes ===");
    if let Ok(meta) = std::fs::metadata(&pbfhogg_osc) {
        verify_msg(&format!("  pbfhogg: {} bytes", meta.len()));
    }
    if let Ok(meta) = std::fs::metadata(&osmium_osc) {
        verify_msg(&format!("  osmium:  {} bytes", meta.len()));
    }

    // Step 4: Roundtrip — apply each derived OSC back to old.
    let rt_pbfhogg = outdir.join("roundtrip-pbfhogg.osm.pbf");
    let rt_pbfhogg_str = rt_pbfhogg.display().to_string();

    verify_msg("--- roundtrip: apply pbfhogg OSC ---");
    let captured = harness.run_pbfhogg(&[
        "merge", &pbf_str, &pbfhogg_osc_str, "-o", &rt_pbfhogg_str,
    ])?;
    harness.check_exit(&captured, "pbfhogg merge (roundtrip)")?;

    let rt_osmium = outdir.join("roundtrip-osmium.osm.pbf");
    let rt_osmium_str = rt_osmium.display().to_string();

    verify_msg("--- roundtrip: apply osmium OSC ---");
    let captured = harness.run_tool(
        "osmium",
        &["apply-changes", &pbf_str, &osmium_osc_str, "-o", &rt_osmium_str, "--overwrite"],
    )?;
    harness.check_exit(&captured, "osmium apply-changes (roundtrip)")?;

    // Element counts.
    verify_msg("=== element counts ===");
    harness.print_fileinfo("new", &new_pbf)?;
    harness.print_fileinfo("roundtrip-pbfhogg", &rt_pbfhogg)?;
    harness.print_fileinfo("roundtrip-osmium", &rt_osmium)?;

    // Diffs (informational — differences are expected).
    verify_msg("=== diff: pbfhogg roundtrip vs new ===");
    let identical = harness.diff_pbfs(&rt_pbfhogg, &new_pbf)?;
    if identical {
        verify_msg("  PASS (identical)");
    } else {
        verify_msg("  differences found (see above)");
    }

    verify_msg("=== diff: osmium roundtrip vs new ===");
    let identical = harness.diff_pbfs(&rt_osmium, &new_pbf)?;
    if identical {
        verify_msg("  PASS (identical)");
    } else {
        verify_msg("  differences found (see above)");
    }

    verify_msg("=== diff: pbfhogg roundtrip vs osmium roundtrip ===");
    let identical = harness.diff_pbfs(&rt_pbfhogg, &rt_osmium)?;
    if identical {
        verify_msg("  PASS (identical)");
    } else {
        verify_msg("  differences found (see above)");
    }

    // Sort checks.
    harness.check_sorted("new", &new_pbf)?;
    harness.check_sorted("roundtrip-pbfhogg", &rt_pbfhogg)?;

    Ok(())
}
