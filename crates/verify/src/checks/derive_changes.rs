//! Verify: diff --format osc - roundtrip validation via pbfhogg and osmium.

use std::path::Path;

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::{Harness, Verdict};
use crate::osc;
use crate::reference::ToolRun;
use crate::verify_input_order;

/// `pbfhogg diff --format osc OLD NEW -o OUT`, which writes run scratch on
/// every path, kept in the check's own directory. pbfhogg takes
/// `--direct-io` on indexed and unindexed inputs alike.
fn osc_diff(old: &Path, new: &Path, out: &Path, direct_io: bool) -> PbfArgv {
    PbfArgv::new("diff")
        .arg("--format")
        .arg("osc")
        .input(old)
        .input(new)
        .output(out)
        .scratch()
        .arg_if(direct_io, "--direct-io")
}

/// Cross-validate `pbfhogg diff --format osc` against `osmium derive-changes`.
///
/// Creates a "new" PBF by applying changes, derives changes from old->new with
/// both tools, then roundtrips each derived OSC back through apply-changes and compares.
/// Finally derives again from the pair stripped of indexdata and requires the
/// indexed OSC's content.
///
/// `direct_io` reaches both derives, the command under test. The
/// apply-changes runs build the fixture and the roundtrip oracle, so they run
/// buffered: "new" is judged only through roundtrips downstream of itself,
/// so an O_DIRECT fault in building it would pass, and one in the roundtrip
/// would fail the derive for a fault that is not its own. `verify merge`
/// holds apply-changes under `--direct-io` to osmium instead.
///
/// Input without the sorted claim takes [`refused`] instead.
#[allow(clippy::too_many_lines)]
pub fn run(
    harness: &mut Harness,
    pbf: &Path,
    osc: &Path,
    direct_io: bool,
) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("derive-changes")?;

    harness.msg("=== verify diff --format osc ===");
    harness.msg(&format!("  old: {}", pbf.display()));
    harness.msg(&format!(
        "  osc: {} (used to create 'new' via apply-changes)",
        osc.display()
    ));

    if !verify_input_order::input_header(harness, pbf, &outdir)?.optional_sorted() {
        return refused(harness, &outdir, pbf, direct_io);
    }

    // Step 1: Create "new" PBF by applying the OSC.
    let new_pbf = outdir.join("new.osm.pbf");

    harness.msg("--- creating 'new' PBF via apply-changes ---");
    let captured = harness.pbfhogg_discard(
        PbfArgv::new("apply-changes")
            .input(pbf)
            .input(osc)
            .output(&new_pbf),
    )?;
    harness.require_success(&captured, "pbfhogg apply-changes")?;

    // Step 2: Derive changes with both tools.
    let pbfhogg_osc = outdir.join("pbfhogg.osc.gz");

    harness.msg("--- pbfhogg diff --format osc ---");
    let captured = harness.pbfhogg_discard(osc_diff(pbf, &new_pbf, &pbfhogg_osc, direct_io))?;
    // Like a text diff, it exits 1 when the inputs differ - the normal case
    // here - and still writes its OSC; 2 and above are real failures.
    harness.require_codes(&captured, "pbfhogg diff --format osc", &[0, 1])?;

    let osmium_osc = outdir.join("osmium.osc.gz");

    harness.msg("--- osmium derive-changes ---");
    let captured = harness.reference(
        &ToolRun::osmium("derive-changes.1")
            .arg("derive-changes")
            .input(pbf)
            .input(&new_pbf)
            .arg("-o")
            .output(&osmium_osc)
            .arg("--overwrite"),
        &outdir.join("osmium-derive-changes.stdout"),
    )?;
    harness.require_success(&captured, "osmium derive-changes")?;

    // Step 3: Report OSC file sizes.
    harness.msg("=== OSC file sizes ===");
    if let Ok(meta) = std::fs::metadata(&pbfhogg_osc) {
        harness.msg(&format!("  pbfhogg: {} bytes", meta.len()));
    }
    if let Ok(meta) = std::fs::metadata(&osmium_osc) {
        harness.msg(&format!("  osmium:  {} bytes", meta.len()));
    }

    // Step 4: Roundtrip - apply each derived OSC back to old.
    let rt_pbfhogg = outdir.join("roundtrip-pbfhogg.osm.pbf");

    harness.msg("--- roundtrip: apply pbfhogg OSC ---");
    let captured = harness.pbfhogg_discard(
        PbfArgv::new("apply-changes")
            .input(pbf)
            .input(&pbfhogg_osc)
            .output(&rt_pbfhogg),
    )?;
    harness.require_success(&captured, "pbfhogg apply-changes (roundtrip)")?;

    let rt_osmium = outdir.join("roundtrip-osmium.osm.pbf");

    harness.msg("--- roundtrip: apply osmium OSC ---");
    let captured = harness.reference(
        &ToolRun::osmium("derive-changes-roundtrip.1")
            .arg("apply-changes")
            .input(pbf)
            .input(&osmium_osc)
            .arg("-o")
            .output(&rt_osmium)
            .arg("--overwrite"),
        &outdir.join("osmium-roundtrip.stdout"),
    )?;
    harness.require_success(&captured, "osmium apply-changes (roundtrip)")?;

    // pbfhogg diff is a merge-join and refuses an input that does not declare
    // Sort.Type_then_ID. osmium's apply-changes output is content-sorted but
    // does not stamp the feature, so normalise it through `pbfhogg sort` - a
    // passthrough plus flag stamp, content unchanged - before comparing.
    // `--force` because osmium writes no blob-level indexdata.
    let rt_osmium_sorted = outdir.join("roundtrip-osmium-sorted.osm.pbf");
    harness.msg("--- pbfhogg sort (normalise osmium roundtrip for merge-join diff) ---");
    let captured = harness.pbfhogg_discard(
        PbfArgv::new("sort")
            .arg("--force")
            .input(&rt_osmium)
            .output(&rt_osmium_sorted),
    )?;
    harness.require_success(&captured, "pbfhogg sort (normalise osmium roundtrip)")?;

    // Element counts. `new` and `roundtrip-pbfhogg` are summarised by their
    // sort checks below.
    harness.msg("=== element counts ===");
    harness.print_inspect("roundtrip-osmium", &rt_osmium)?;

    for (label, listing, a, b) in [
        (
            "pbfhogg roundtrip vs new",
            "pbfhogg-vs-new",
            &rt_pbfhogg,
            &new_pbf,
        ),
        (
            "osmium roundtrip vs new",
            "osmium-vs-new",
            &rt_osmium_sorted,
            &new_pbf,
        ),
        (
            "pbfhogg roundtrip vs osmium roundtrip",
            "pbfhogg-vs-osmium",
            &rt_pbfhogg,
            &rt_osmium_sorted,
        ),
    ] {
        harness.msg(&format!("=== diff: {label} ==="));
        let diff =
            harness.diff_pbfs(a, b, &outdir.join(format!("roundtrip-{listing}.diff.txt")))?;
        if diff.is_pass() {
            harness.msg("  PASS (identical)");
        } else {
            harness.msg("  FAIL (differences found)");
        }
        harness.record(&format!("diff {label}"), diff);
    }

    let parity = unindexed_osc_matches(harness, &outdir, pbf, &new_pbf, &pbfhogg_osc, direct_io)?;
    harness.record("unindexed OSC vs indexed OSC", parity);
    let order = harness.check_sorted("new", &new_pbf)?;
    harness.record("new order", order);
    let order = harness.check_sorted("roundtrip-pbfhogg", &rt_pbfhogg)?;
    harness.record("roundtrip-pbfhogg order", order);

    Ok(Some("element-content roundtrips against new and osmium; indexed/unindexed OSC decompressed bytes identical; metadata not independently judged".into()))
}

/// The check on input whose header lacks `Sort.Type_then_ID` among its
/// optional features, which `diff --format osc` refuses on either side by
/// contract (and the apply-changes that builds "new" refuses too): the derive
/// runs twice, the input as OLD and then as NEW against an osmium-sorted copy
/// of it, each held to the refusal naming the input, each with an output of
/// its own (`verify_input_order`). A mixed indexed pair, so no comparison
/// engine is exercised; both runs are made whatever the first one shows.
fn refused(
    harness: &mut Harness,
    outdir: &Path,
    pbf: &Path,
    direct_io: bool,
) -> Result<Option<String>, Error> {
    let copy = verify_input_order::sorted_counterpart(harness, pbf, outdir)?;
    let claim = Some(verify_input_order::refusal_claim(
        "pbfhogg diff --format osc",
        ", as OLD and as NEW, the other side an osmium-sorted copy of it",
        direct_io,
    ));
    for (side, old, new) in [("old", pbf, copy.as_path()), ("new", copy.as_path(), pbf)] {
        let out = outdir.join(format!("refused-as-{side}.osc.gz"));
        let label = format!("diff --format osc (input as {})", side.to_uppercase());
        verify_input_order::expect_unsorted_refusal(
            harness,
            osc_diff(old, new, &out, direct_io),
            &verify_input_order::Refusal {
                label: &label,
                operand: pbf,
                answers: &[0, 1],
                sink: None,
            },
        )?;
    }
    Ok(claim)
}

/// Rerun the derive on the same pair stripped of indexdata and require the
/// indexed run's OSC exactly.
///
/// pbfhogg diff has a separate code path for inputs without indexdata, meant
/// to write the same OSC, full metadata included. apply-changes refuses an
/// unindexed base, so a raw variant cannot drive this check; stripping both
/// sides of the pair verified above can. The indexed OSC is cross-checked
/// against osmium by the roundtrips, so matching it holds the unindexed path
/// to osmium too.
fn unindexed_osc_matches(
    harness: &mut Harness,
    outdir: &Path,
    old: &Path,
    new: &Path,
    indexed_osc: &Path,
    direct_io: bool,
) -> Result<Verdict, Error> {
    let old_raw = outdir.join("old-unindexed.osm.pbf");
    let new_raw = outdir.join("new-unindexed.osm.pbf");
    harness.msg("--- stripping indexdata from old and new ---");
    harness.strip_indexdata(old, &old_raw)?;
    harness.strip_indexdata(new, &new_raw)?;

    let raw_osc = outdir.join("pbfhogg-unindexed.osc.gz");
    harness.msg("--- pbfhogg diff --format osc (unindexed inputs) ---");
    let captured = harness.pbfhogg_discard(osc_diff(&old_raw, &new_raw, &raw_osc, direct_io))?;
    harness.require_codes(
        &captured,
        "pbfhogg diff --format osc (unindexed inputs)",
        &[0, 1],
    )?;

    harness.msg("=== diff: unindexed OSC vs indexed OSC ===");
    match osc::first_content_difference(&raw_osc, indexed_osc)? {
        None => {
            harness.msg("  PASS (identical content)");
            Ok(Verdict::Pass)
        }
        Some(offset) => {
            harness.msg(&format!(
                "  FAIL (decompressed content first differs at byte {offset})"
            ));
            Ok(Verdict::Fail)
        }
    }
}
