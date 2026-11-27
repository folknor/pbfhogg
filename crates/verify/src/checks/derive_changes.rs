//! Verify: diff --format osc - roundtrip validation via pbfhogg and osmium.

use std::path::Path;

use crate::argv::PbfArgv;
use crate::diff_summary;
use crate::error::Error;
use crate::harness::{Harness, Role, Verdict};
use crate::opl_compare::{self, Policy};
use crate::osc;
use crate::process::Ended;
use crate::reference::{Origin, ToolRun};
use crate::verify_input_order;

/// `pbfhogg diff --format osc OLD NEW -o OUT`, which writes run scratch on
/// every path, kept in the check's own directory. pbfhogg takes
/// `--direct-io` on indexed and unindexed inputs alike. A fixed worker count
/// makes the fallback note mean the same on every host
/// ([`diff_summary::JOBS`]).
fn osc_diff(old: &Path, new: &Path, out: &Path, direct_io: bool) -> PbfArgv {
    PbfArgv::new("diff")
        .arg("--format")
        .arg("osc")
        .arg("--jobs")
        .arg(diff_summary::JOBS)
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
    let captured = harness
        .pbfhogg_discard(crate::argv::apply_changes(pbf, osc, &new_pbf).memoized_output())?;
    harness.require_output(&captured, "pbfhogg apply-changes", &new_pbf)?;

    // Step 2: Derive changes with both tools. The two pbfhogg derives, the
    // indexed one and its rerun on the pair stripped of indexdata, take
    // about the same time and read only settled files, so they run as one
    // launch group after the strips, a group of their own. Only the indexed
    // derive is judged here. The strips' outcome and the unindexed derive's
    // exit are held to the parity point at the end, where they were judged
    // when they ran last, so their failure cannot suppress the roundtrip
    // findings; with a failed strip the unindexed derive is not launched. A
    // group abort (a failed start, an overflow, an adoption, a moved input)
    // still ends the check where it happens.
    let pbfhogg_osc = outdir.join("pbfhogg.osc.gz");
    let old_raw = outdir.join("old-unindexed.osm.pbf");
    let new_raw = outdir.join("new-unindexed.osm.pbf");
    let raw_osc = outdir.join("pbfhogg-unindexed.osc.gz");

    harness.msg("--- stripping indexdata from old and new ---");
    let mut ended = harness
        .pbfhogg_group(vec![
            (
                crate::argv::strip_indexdata(pbf, &old_raw),
                outdir.join("old-unindexed.stdout"),
            ),
            (
                crate::argv::strip_indexdata(&new_pbf, &new_raw),
                outdir.join("new-unindexed.stdout"),
            ),
        ])?
        .into_iter();
    let mut next = || {
        ended
            .next()
            .ok_or_else(|| Error::tool("a pbfhogg launch group ended short"))
    };
    let strip_old = next()?;
    let strip_new = next()?;
    // Old before new, the order they once ran in.
    let strips = harness
        .require_output(&strip_old, "pbfhogg degrade --strip-indexdata", &old_raw)
        .and_then(|()| {
            harness.require_output(&strip_new, "pbfhogg degrade --strip-indexdata", &new_raw)
        });

    harness.msg("--- pbfhogg diff --format osc, indexed and unindexed inputs ---");
    let mut launches = vec![(
        osc_diff(pbf, &new_pbf, &pbfhogg_osc, direct_io),
        outdir.join("pbfhogg-osc.stdout"),
    )];
    if strips.is_ok() {
        launches.push((
            osc_diff(&old_raw, &new_raw, &raw_osc, direct_io),
            outdir.join("pbfhogg-unindexed-osc.stdout"),
        ));
    }
    let mut ended = harness.pbfhogg_group(launches)?.into_iter();
    let captured = ended
        .next()
        .ok_or_else(|| Error::tool("a pbfhogg launch group ended short"))?;
    let raw_derive = strips.map(|()| ended.next());
    // Like a text diff, it exits 1 when the inputs differ - the normal case
    // here - and still writes its OSC; 2 and above are real failures.
    harness.require_codes(&captured, "pbfhogg diff --format osc", &[0, 1])?;
    // The unindexed rerun matches this OSC whichever engine either run
    // took, so each run's route is judged from its own diagnostics.
    diff_summary::record_fallback_note(
        harness,
        "indexed derive",
        false,
        diff_summary::fallback_note(&captured.stderr_text()),
    );

    let osmium_osc = outdir.join("osmium.osc.gz");

    harness.msg("--- osmium derive-changes ---");
    let captured = harness.reference(
        &ToolRun::osmium("derive-changes.1")
            .arg("derive-changes")
            .input(pbf, Origin::Other)
            .input(&new_pbf, Origin::Pbfhogg)
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
            .input(pbf, Origin::Other)
            .input(&osmium_osc, Origin::Other)
            .arg("-o")
            .output(&rt_osmium)
            .arg("--overwrite"),
        &outdir.join("osmium-roundtrip.stdout"),
    )?;
    harness.require_success(&captured, "osmium apply-changes (roundtrip)")?;

    // What pbfhogg's OSC means is decided by osmium too: applied by osmium,
    // it must give new's content. pbfhogg's own application of it above is
    // kept as an interoperability comparison, but on its own a writer bug
    // and an apply bug that cancel would pass.
    let rt_cross = outdir.join("roundtrip-pbfhogg-osc-by-osmium.osm.pbf");
    harness.msg("--- roundtrip: apply pbfhogg OSC with osmium ---");
    let captured = harness.reference(
        &ToolRun::osmium("derive-changes-cross.1")
            .arg("apply-changes")
            .input(pbf, Origin::Other)
            .input(&pbfhogg_osc, Origin::Pbfhogg)
            .arg("-o")
            .output(&rt_cross)
            .arg("--overwrite"),
        &outdir.join("osmium-cross-roundtrip.stdout"),
    )?;
    harness.require_success(&captured, "osmium apply-changes (pbfhogg OSC)")?;

    // Content only: pbfhogg's OSC is content-based by design (metadata and
    // inline way coordinates are not content), so an object whose version
    // moved without a content change is not in it.
    // The first comparison, pbfhogg's roundtrip against new, also judges the
    // order of both from its decodings; a separate order decode after these
    // strict comparisons could only pass.
    let mut verdicts = Vec::with_capacity(4);
    for (index, (label, listing, a, b)) in [
        (
            "pbfhogg roundtrip vs new",
            "pbfhogg-vs-new",
            (rt_pbfhogg.as_path(), Origin::Pbfhogg),
            (new_pbf.as_path(), Origin::Pbfhogg),
        ),
        (
            "pbfhogg OSC applied by osmium vs new",
            "pbfhogg-osc-by-osmium-vs-new",
            (rt_cross.as_path(), Origin::Other),
            (new_pbf.as_path(), Origin::Pbfhogg),
        ),
        (
            "osmium roundtrip vs new",
            "osmium-vs-new",
            (rt_osmium.as_path(), Origin::Other),
            (new_pbf.as_path(), Origin::Pbfhogg),
        ),
        (
            "pbfhogg roundtrip vs osmium roundtrip",
            "pbfhogg-vs-osmium",
            (rt_pbfhogg.as_path(), Origin::Pbfhogg),
            (rt_osmium.as_path(), Origin::Other),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        harness.msg(&format!("=== diff: {label} ==="));
        let (role_a, role_b) = if index == 0 {
            (
                Role::Judged {
                    label: "roundtrip-pbfhogg",
                    record: "roundtrip-pbfhogg order",
                },
                Role::Judged {
                    label: "new",
                    record: "new order",
                },
            )
        } else {
            (Role::Required, Role::Required)
        };
        let (diff, _) = harness.same_elements_judged(
            (a, role_a),
            (b, role_b),
            &outdir.join(format!("roundtrip-{listing}.diff.txt")),
            Policy::CONTENT,
        )?;
        if diff.is_pass() {
            harness.msg("  PASS (identical)");
        } else {
            harness.msg("  FAIL (differences found)");
        }
        verdicts.push((label, diff));
    }
    // The two comparisons with osmium's own roundtrip judge osmium's derive
    // too, which omits some deletes (`opl_compare::osmium_derive_roundtrip`).
    // Only when osmium's roundtrip differs from new, it is held instead to
    // new plus exactly the old records that omission retains, unchanged;
    // pbfhogg's roundtrip against it then passes only where pbfhogg's equals
    // new. An osmium roundtrip equal to new is never re-judged.
    if !verdicts[2].1.is_pass() {
        let pbfhogg_ok = verdicts[0].1.is_pass();
        if let Some(osmium_ok) =
            osmium_omission_explains(harness, &outdir, pbf, &new_pbf, &rt_osmium)?
        {
            verdicts[2].1 = Verdict::from_pass(osmium_ok);
            verdicts[3].1 = Verdict::from_pass(osmium_ok && pbfhogg_ok);
        }
    }
    for (label, verdict) in verdicts {
        harness.record(&format!("diff {label}"), verdict);
    }

    let raw_derive =
        raw_derive?.ok_or_else(|| Error::tool("a pbfhogg launch group ended short"))?;
    let parity = unindexed_osc_matches(harness, &raw_derive, &raw_osc, &pbfhogg_osc)?;
    harness.record("unindexed OSC vs indexed OSC", parity);

    Ok(Some("element-content roundtrips against new and osmium, pbfhogg's OSC applied by both tools; indexed/unindexed OSC decompressed bytes identical; the fallback note confirms which engine the indexdata predicate selects, not engine entry; metadata, visibility and inline coordinates not judged".into()))
}

/// Whether osmium's roundtrip is new plus the old records its derive keeps
/// by comparing IDs without kinds, each unchanged; `None` when that model
/// retains nothing, so the failure stands as found. A pass is reported as a
/// residue consistent with the omission: the snapshots alone cannot tell an
/// omitted delete from an ignored one.
fn osmium_omission_explains(
    harness: &mut Harness,
    outdir: &Path,
    old: &Path,
    new: &Path,
    rt_osmium: &Path,
) -> Result<Option<bool>, Error> {
    harness.msg("=== osmium derive-changes same-ID omission model ===");
    let expected = outdir.join("osmium-derive-model.opl");
    let retained = opl_compare::osmium_derive_roundtrip(
        harness,
        (old, Origin::Other),
        (new, Origin::Pbfhogg),
        &expected,
    )?;
    if retained == 0 {
        harness.msg("  the model retains no old object: the failures stand");
        std::fs::remove_file(&expected)?;
        return Ok(None);
    }
    let merged = opl_compare::compare_with_opl(
        harness,
        (rt_osmium, Origin::Other),
        &expected,
        &outdir.join("roundtrip-osmium-vs-model.diff.txt"),
        Policy::CONTENT,
    )?;
    std::fs::remove_file(&expected)?;
    let ok = merged.identical();
    if ok {
        harness.msg(&format!(
            "  osmium roundtrip is new plus {retained} unchanged old object(s) osmium writes no \
             delete for (an old-only object whose ID the next new object holds under a later \
             kind; DEVIATIONS.md derive-changes): attributed to osmium"
        ));
    } else {
        harness.msg("  FAIL: osmium roundtrip is not new plus the retained old objects");
        harness.replay_listing(&outdir.join("roundtrip-osmium-vs-model.diff.txt"))?;
    }
    Ok(Some(ok))
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

/// Judge the derive rerun on the same pair stripped of indexdata, requiring
/// the indexed run's OSC exactly.
///
/// pbfhogg diff has a separate code path for inputs without indexdata, meant
/// to write the same OSC, full metadata included. apply-changes refuses an
/// unindexed base, so a raw variant cannot drive this check; stripping both
/// sides of the pair verified above can. The indexed OSC is cross-checked
/// against osmium by the roundtrips, so matching it holds the unindexed path
/// to osmium too.
fn unindexed_osc_matches(
    harness: &mut Harness,
    captured: &Ended,
    raw_osc: &Path,
    indexed_osc: &Path,
) -> Result<Verdict, Error> {
    harness.require_codes(
        captured,
        "pbfhogg diff --format osc (unindexed inputs)",
        &[0, 1],
    )?;
    diff_summary::record_fallback_note(
        harness,
        "unindexed derive",
        true,
        diff_summary::fallback_note(&captured.stderr_text()),
    );

    harness.msg("=== diff: unindexed OSC vs indexed OSC ===");
    match osc::first_content_difference(raw_osc, indexed_osc)? {
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
