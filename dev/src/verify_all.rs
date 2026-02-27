//! Verify: all — run all verify commands sequentially.

use std::path::Path;

use crate::error::DevError;
use crate::output::verify_msg;
use crate::verify::VerifyHarness;
use crate::{
    tools, verify_add_locations, verify_cat, verify_check_refs, verify_derive_changes, verify_diff,
    verify_extract, verify_getid_removeid, verify_merge, verify_sort, verify_tags_filter,
};

/// Run all verify commands sequentially.
///
/// Each command is wrapped so that a failure is logged but does not prevent
/// the remaining commands from running. Returns `Ok(())` unconditionally —
/// individual failures are reported inline via `verify_msg`.
pub fn run(
    harness: &VerifyHarness,
    pbf: &Path,
    osc: Option<&Path>,
    bbox: Option<&str>,
    data_dir: &Path,
    workspace_root: &Path,
) -> Result<(), DevError> {
    let mut passed: u32 = 0;
    let mut failed: u32 = 0;
    let mut skipped: u32 = 0;

    // Helper: run one verify command, track pass/fail.
    let mut run_one = |name: &str, result: Result<(), DevError>| {
        match result {
            Ok(()) => {
                verify_msg(&format!("{name}: PASS"));
                passed += 1;
            }
            Err(e) => {
                verify_msg(&format!("{name} failed: {e}"));
                failed += 1;
            }
        }
    };

    // Helper: log a skipped command.
    let mut skip = |name: &str, reason: &str| {
        verify_msg(&format!("{name}: SKIPPED ({reason})"));
        skipped += 1;
    };

    // 1. sort
    verify_msg("========== sort ==========");
    run_one("sort", verify_sort::run(harness, pbf));

    // 2. cat
    verify_msg("========== cat ==========");
    run_one("cat", verify_cat::run(harness, pbf));

    // 3. extract
    verify_msg("========== extract ==========");
    if let Some(b) = bbox {
        run_one("extract", verify_extract::run(harness, pbf, b));
    } else {
        skip("extract", "no --bbox provided");
    }

    // 4. tags-filter
    verify_msg("========== tags-filter ==========");
    run_one("tags-filter", verify_tags_filter::run(harness, pbf));

    // 5. getid-removeid
    verify_msg("========== getid-removeid ==========");
    run_one("getid-removeid", verify_getid_removeid::run(harness, pbf));

    // 6. add-locations-to-ways
    verify_msg("========== add-locations-to-ways ==========");
    run_one(
        "add-locations-to-ways",
        verify_add_locations::run(harness, pbf),
    );

    // 7. check-refs
    verify_msg("========== check-refs ==========");
    run_one("check-refs", verify_check_refs::run(harness, pbf));

    // 8. merge
    verify_msg("========== merge ==========");
    if let Some(osc_path) = osc {
        // Best-effort osmosis setup — merge works without it.
        let osmosis = match tools::ensure_osmosis(data_dir, workspace_root) {
            Ok(tools) => Some(tools),
            Err(e) => {
                verify_msg(&format!("osmosis not available (non-fatal): {e}"));
                None
            }
        };
        run_one(
            "merge",
            verify_merge::run(harness, pbf, osc_path, osmosis.as_ref()),
        );
    } else {
        skip("merge", "no --osc provided");
    }

    // 9. derive-changes
    verify_msg("========== derive-changes ==========");
    if let Some(osc_path) = osc {
        run_one(
            "derive-changes",
            verify_derive_changes::run(harness, pbf, osc_path),
        );
    } else {
        skip("derive-changes", "no --osc provided");
    }

    // 10. diff
    verify_msg("========== diff ==========");
    if let Some(osc_path) = osc {
        run_one("diff", verify_diff::run(harness, pbf, osc_path));
    } else {
        skip("diff", "no --osc provided");
    }

    // Summary
    let total = passed + failed + skipped;
    verify_msg(&format!(
        "===== all done: {passed} passed, {failed} failed, {skipped} skipped out of {total} ====="
    ));

    Ok(())
}
