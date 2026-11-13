//! Verify: apply-changes - strict pbfhogg/osmium comparison, with optional
//! osmosis and osmconvert outputs shown for additional context.

use std::path::{Path, PathBuf};

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::Harness;
use crate::osc::{self, OscAction, OscKind};
use crate::output;
use crate::reference::{Program, ToolRun};
use crate::verify_element_diff::{self as element_diff, PerType};
use crate::verify_input_order;

/// The header feature `--locations-on-ways` requires of its base.
const LOCATIONS_ON_WAYS: &str = "LocationsOnWays";

/// Cross-validate `pbfhogg apply-changes` against osmium; print optional
/// osmosis and osmconvert output for additional context.
///
/// With `locations_on_ways`, both tools run with `--locations-on-ways` and
/// pbfhogg's output must pass `pbfhogg check --locations`. The element diff
/// cannot see inline coordinates (`pbfhogg diff` treats them as derived
/// data, not content), so that check is the only one that catches a stale
/// coordinate - a way the diff leaves untouched under a node it moves.
/// osmium's output is checked the same way but only reported: its location
/// index can itself write stale coordinates (several versions of one node,
/// unsorted lookups), so it is not the oracle. osmosis and osmconvert, which
/// write no inline locations, are not run.
///
/// On a base whose header lacks `Sort.Type_then_ID` among its optional
/// features, apply-changes is held to its documented refusal instead and no
/// reference runs (`super::verify_input_order`).
pub fn run(
    harness: &mut Harness,
    pbf: &Path,
    osc: &Path,
    direct_io: bool,
    locations_on_ways: bool,
) -> Result<Option<String>, Error> {
    let outdir = harness.subdir(if locations_on_ways {
        "apply-changes-l"
    } else {
        "apply-changes"
    })?;

    harness.msg("=== verify apply-changes ===");
    harness.msg(&format!("  base: {}", pbf.display()));
    harness.msg(&format!("  diff: {}", osc.display()));
    if locations_on_ways {
        harness.msg("  mode: --locations-on-ways");
    }

    // --- pbfhogg apply-changes ---
    let pbfhogg_out = outdir.join("pbfhogg.osm.pbf");
    let pbfhogg_args = PbfArgv::new("apply-changes")
        .input(pbf)
        .input(osc)
        .output(&pbfhogg_out)
        .arg_if(direct_io, "--direct-io")
        .arg_if(locations_on_ways, "--locations-on-ways");

    // A base without the sorted claim is one apply-changes refuses by
    // contract, as osmium requires sorted input: the run is held to that
    // refusal and nothing is compared (`verify_input_order`).
    let header = verify_input_order::input_header(harness, pbf, &outdir)?;
    if !header.optional_sorted() {
        return refused(
            harness,
            pbf,
            &header,
            pbfhogg_args,
            direct_io,
            locations_on_ways,
        );
    }

    harness.msg("--- pbfhogg apply-changes ---");
    let captured = harness.pbfhogg_discard(pbfhogg_args)?;
    harness.require_success(&captured, "pbfhogg apply-changes")?;

    // --- osmium apply-changes ---
    let osmium_out = outdir.join("osmium.osm.pbf");

    harness.msg("--- osmium apply-changes ---");
    let mut osmium_run = ToolRun::osmium("apply-changes.1")
        .arg("apply-changes")
        .input(pbf)
        .input(osc)
        .arg("-o")
        .output(&osmium_out)
        .arg("--overwrite");
    if locations_on_ways {
        osmium_run = osmium_run.arg("--locations-on-ways");
    }
    let captured = harness.reference(&osmium_run, &outdir.join("osmium.stdout"))?;
    harness.require_success(&captured, "osmium apply-changes")?;

    // --- osmosis / osmconvert (optional context) ---
    let context_outputs = if locations_on_ways {
        Vec::new()
    } else {
        run_context_tools(harness, pbf, osc, &outdir)?
    };

    // --- Element counts ---
    // pbfhogg's output is summarised by its sort check below.
    harness.msg("=== element counts ===");
    harness.print_inspect("osmium", &osmium_out)?;
    for (label, path) in &context_outputs {
        harness.print_inspect(label, path)?;
    }

    // --- Sort check ---
    let order = harness.check_sorted("pbfhogg apply-changes", &pbfhogg_out)?;
    harness.record("pbfhogg apply-changes output order", order);

    // --- Inline locations (-l only) ---
    // Before the element diff, which reports its failure as an `Err` and so
    // would otherwise keep this verdict from being recorded.
    if locations_on_ways {
        harness.msg("=== inline locations (pbfhogg check --locations) ===");
        let (pbfhogg_verdict, _) = harness.check_locations("pbfhogg", &pbfhogg_out, false)?;
        harness.record("pbfhogg apply-changes inline locations", pbfhogg_verdict);
        let (osmium, report) = harness.check_locations("osmium", &osmium_out, false)?;
        // One line on the summary channel, which prints on a pass too, so a
        // change in osmium's stale set stays visible run to run; the report
        // itself goes beside the outputs and into the detail.
        let report_file = outdir.join("osmium-locations.txt");
        std::fs::write(&report_file, &report)?;
        for line in report.lines().filter(|l| !l.trim().is_empty()) {
            harness.msg(&format!("    {line}"));
        }
        output::note(&format!(
            "  osmium apply-changes --locations-on-ways inline locations: {} \
             (informational: osmium's location index is not an oracle); report in {}",
            if osmium.is_pass() {
                "consistent".to_owned()
            } else {
                stale_summary(&report)
            },
            report_file.display()
        ));
    }

    // pbfhogg diff (the deep-compare below) is a merge-join and requires both
    // inputs to declare the Sort.Type_then_ID optional feature. pbfhogg's
    // apply-changes output does; osmium's is content-sorted but doesn't stamp
    // the feature, so pbfhogg diff otherwise dies with "New PBF is not sorted".
    // Normalise osmium's output through `pbfhogg sort` first - a passthrough +
    // flag stamp on already-sorted content - so the compare sees two sorted
    // inputs. Content is unchanged, so the element diff is unaffected.
    let osmium_sorted = outdir.join("osmium-sorted.osm.pbf");
    harness.msg("--- pbfhogg sort (normalise osmium output for merge-join diff) ---");
    // `--force`: osmium's output never carries blob-level indexdata, which
    // pbfhogg sort requires by default. With --force it decodes payloads to
    // scan IDs (slower pass 1) but produces content-identical output, so the
    // cross-check's independence is untouched.
    let captured = harness.pbfhogg_discard(
        PbfArgv::new("sort")
            .arg("--force")
            .input(&osmium_out)
            .output(&osmium_sorted),
    )?;
    harness.require_success(&captured, "pbfhogg sort (normalise osmium output)")?;

    // --- Strict pbfhogg-vs-osmium element diff (with delete-set tolerance) ---
    //
    // Cross-validation needs more than equal element counts. We diff
    // the two outputs element-by-element and exempt the one
    // legitimate semantic difference: osmium does version-based
    // deletes while pbfhogg/osmosis/osmconvert delete unconditionally,
    // so osmium-only elements whose IDs appear in the input OSC's
    // `<delete>` section aren't bugs - both behaviours are spec-allowed.
    // Anything else (osmium-only IDs not in the delete set, pbfhogg-only
    // elements, or content-level `<modify>` differences) fails verify.
    verify_pbfhogg_vs_osmium(harness, osc, &pbfhogg_out, &osmium_sorted, &outdir)?;

    Ok(Some(if locations_on_ways {
        "strict element content with delete-set tolerance; output strictly ordered; inline locations pass pbfhogg check --locations; osmium locations informational"
    } else {
        "strict element content with delete-set tolerance; output strictly ordered; osmosis and osmconvert context only"
    }.into()))
}

/// The check on a base whose header lacks `Sort.Type_then_ID` among its
/// optional features: `args`, the apply-changes run [`run`] would make, held
/// to its refusal of the base.
///
/// With `-l` pbfhogg refuses a base without `LocationsOnWays` too, so the
/// base must meet that prerequisite: the check then does not depend on which
/// of the two refusals pbfhogg raises first.
fn refused(
    harness: &mut Harness,
    pbf: &Path,
    header: &crate::inspect_report::InspectSummary,
    args: PbfArgv,
    direct_io: bool,
    locations_on_ways: bool,
) -> Result<Option<String>, Error> {
    if locations_on_ways && !header.optional_feature(LOCATIONS_ON_WAYS) {
        return Err(Error::Failed(format!(
            "{} declares neither {} nor {LOCATIONS_ON_WAYS}, two refusals of apply-changes \
             --locations-on-ways, so neither can be held alone",
            pbf.display(),
            crate::inspect_report::SORT_TYPE_THEN_ID
        )));
    }
    verify_input_order::expect_unsorted_refusal(
        harness,
        args,
        &verify_input_order::Refusal {
            label: "apply-changes",
            operand: pbf,
            answers: &[0],
            sink: None,
        },
    )?;
    Ok(Some(verify_input_order::refusal_claim(
        "pbfhogg apply-changes",
        "",
        direct_io,
    )))
}

/// `STALE (563 mismatched (538 nodes))`: the verdict with the report's
/// mismatch count, echoed from its "Refs with a node record" line when it has
/// one, so a change in the stale set shows in the summary line itself.
fn stale_summary(report: &str) -> String {
    let count = report
        .lines()
        .find(|l| l.contains("Refs with a node record"))
        .and_then(|l| l.split(", ").nth(1))
        .map(str::trim);
    match count {
        Some(count) => format!("STALE ({count})"),
        None => "STALE".to_owned(),
    }
}

/// Run the optional osmosis and osmconvert merges, shown for context only.
/// Returns the `(label, output)` of each tool that completed successfully:
/// production is tracked from each run's outcome, not inferred from a file
/// being present. A failed run's partial output is still removed.
///
/// Both run through the reference-run path (stdout to a file, death with
/// brokkr, SIGKILL classified). No comparison depends on these tools, so an
/// environment kill of one does not change the check's verdict; it is
/// reported on the summary channel, where a passing check still shows it.
fn run_context_tools(
    harness: &mut Harness,
    pbf: &Path,
    osc: &Path,
    outdir: &Path,
) -> Result<Vec<(&'static str, PathBuf)>, Error> {
    let mut produced = Vec::new();
    for (program, available, label, prefix) in [
        (Program::Osmosis, harness.has_osmosis(), "osmosis", "file="),
        (
            Program::Osmconvert,
            harness.has_osmconvert(),
            "osmconvert",
            "-o=",
        ),
    ] {
        if !available {
            continue;
        }
        let output = outdir.join(format!("{label}.osm.pbf"));
        let run = if program == Program::Osmosis {
            ToolRun::new("apply-changes-osmosis.1", program)
                .arg("--read-xml-change")
                .input_as("file=", osc)
                .arg("--read-pbf")
                .input_as("file=", pbf)
                .args(&["--apply-change", "--write-pbf"])
                .output_as(prefix, &output)
        } else {
            ToolRun::new("apply-changes-osmconvert.1", program)
                .input(pbf)
                .input(osc)
                .output_as(prefix, &output)
        };
        let result = harness
            .reference(&run, &outdir.join(format!("{label}.stdout")))
            .and_then(|ended| harness.require_success(&ended, label));
        match result {
            Ok(()) => produced.push((label, output)),
            Err(e @ (Error::Interrupted | Error::Unsettled(_))) => return Err(e),
            Err(e) => {
                output::note(&format!(
                    "{label} context only, no verdict depends on it: {e}"
                ));
                if output.exists() {
                    std::fs::remove_file(output)?;
                }
            }
        }
    }

    Ok(produced)
}

/// Run `pbfhogg diff --format osc` on `(pbfhogg_out, osmium_out)`,
/// then categorise each diff entry against the input OSC's delete
/// set. Returns `Err(Error::Failed(...))` if any unexplained
/// difference remains.
fn verify_pbfhogg_vs_osmium(
    harness: &mut Harness,
    input_osc: &Path,
    pbfhogg_out: &Path,
    osmium_out: &Path,
    outdir: &Path,
) -> Result<(), Error> {
    harness.msg("=== pbfhogg vs osmium element diff ===");

    let applied = DeleteSet::read(input_osc)?;
    harness.msg(&format!(
        "  input OSC delete set: {} nodes, {} ways, {} relations",
        applied.ids[0].len(),
        applied.ids[1].len(),
        applied.ids[2].len(),
    ));

    let diff_path = outdir.join("pbfhogg-vs-osmium.osc");

    // pbfhogg diff old new: <delete> blocks = old-only, <create>
    // blocks = new-only, <modify> blocks = same id different content.
    // We pass pbfhogg as `old` and osmium as `new` so:
    //   diff.created_* = osmium-only (carve-out candidates)
    //   diff.deleted_* = pbfhogg-only (always failures)
    //   diff.modified_* = content mismatches (always failures)
    // Classified as it streams: only counters and bounded samples are kept,
    // so a regression the size of the planet costs no more than a pass. The
    // verdict waits for the scan to return, which proves the document whole
    // and its entries in agreement with the diff's exit status.
    let mut report = MergeDiffReport::default();
    element_diff::scan_osc_diff(
        harness,
        pbfhogg_out,
        osmium_out,
        &diff_path,
        |action, kind, id| report.classify(action, kind, id, &applied),
    )?;
    drop(applied);
    if report.is_empty() {
        harness.msg("  pbfhogg and osmium outputs are element-identical PASS");
        return Ok(());
    }
    report.print(harness);

    if report.is_failure() {
        return Err(Error::Failed(format!(
            "pbfhogg vs osmium: {} unexplained difference entries \
             (see element diff above)",
            report.total_failures()
        )));
    }
    Ok(())
}

/// The input OSC's `<delete>` IDs per kind (node, way, relation), sorted and
/// deduplicated for binary search: the carve-out's membership test.
///
/// Held in memory, O(delete entries in the input change): 8-byte IDs plus
/// vector capacity while it is read. That is the one input-sized structure
/// here, and the step before it dominates it: osmium apply-changes, which must
/// have succeeded first, holds every change in the file as a full object.
/// Nothing sized by the PBF or by the difference is retained.
struct DeleteSet {
    ids: [Vec<i64>; 3],
}

impl DeleteSet {
    fn read(osc_path: &Path) -> Result<Self, Error> {
        let mut ids: [Vec<i64>; 3] = Default::default();
        osc::scan_osc_file(osc_path, |action, kind, id| {
            if action == OscAction::Delete {
                ids[kind.index()].push(id);
            }
        })?;
        Ok(Self::from_unsorted(ids))
    }

    fn from_unsorted(mut ids: [Vec<i64>; 3]) -> Self {
        for kind in &mut ids {
            kind.sort_unstable();
            kind.dedup();
            kind.shrink_to_fit();
        }
        Self { ids }
    }

    fn contains(&self, kind: OscKind, id: i64) -> bool {
        self.ids[kind.index()].binary_search(&id).is_ok()
    }
}

/// Outcome classes for the per-element diff, each a count of diff entries
/// per kind plus the smallest few distinct IDs
/// ([`element_diff::MAX_REPORTED_IDS`]), so the
/// report is bounded and deterministic whatever the diff's size or order.
#[derive(Default)]
struct MergeDiffReport {
    /// osmium-only IDs accounted for by the input OSC's delete set.
    /// These are *expected* differences - osmium's version-based
    /// deletes vs pbfhogg's unconditional deletes.
    osmium_only_explained: PerType,
    /// osmium-only IDs not in the input OSC delete set. These are
    /// pbfhogg dropping elements it shouldn't have.
    osmium_only_unexplained: PerType,
    /// pbfhogg-only IDs (osmium dropped, pbfhogg kept). Always a
    /// failure - means osmium did something pbfhogg didn't.
    pbfhogg_only: PerType,
    /// IDs present in both outputs but with different content.
    content_mismatch: PerType,
}

impl MergeDiffReport {
    /// Bucket one diff entry. pbfhogg is the diff's old side and osmium its
    /// new side, so `<create>` is osmium-only (exempt when the input deleted
    /// it), `<delete>` pbfhogg-only and `<modify>` a content mismatch.
    fn classify(&mut self, action: OscAction, kind: OscKind, id: i64, applied: &DeleteSet) {
        let class = match action {
            OscAction::Create if applied.contains(kind, id) => &mut self.osmium_only_explained,
            OscAction::Create => &mut self.osmium_only_unexplained,
            OscAction::Delete => &mut self.pbfhogg_only,
            OscAction::Modify => &mut self.content_mismatch,
        };
        class.add(kind, id);
    }

    fn is_empty(&self) -> bool {
        self.osmium_only_explained.total() == 0 && self.total_failures() == 0
    }

    fn total_failures(&self) -> u64 {
        self.osmium_only_unexplained.total()
            + self.pbfhogg_only.total()
            + self.content_mismatch.total()
    }

    fn is_failure(&self) -> bool {
        self.total_failures() > 0
    }

    fn print(&self, harness: &mut Harness) {
        if self.osmium_only_explained.total() > 0 {
            harness.msg(&format!(
                "  osmium-only explained by delete set: {} OK",
                self.osmium_only_explained.counts_line()
            ));
        }
        let mut fail =
            |label: &str, class: &PerType| class.print("FAIL ", label, |line| harness.msg(line));
        fail(
            "osmium-only NOT in delete set",
            &self.osmium_only_unexplained,
        );
        fail("pbfhogg-only (osmium dropped)", &self.pbfhogg_only);
        fail("content differs", &self.content_mismatch);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn deletes(nodes: &[i64], ways: &[i64], relations: &[i64]) -> DeleteSet {
        DeleteSet::from_unsorted([nodes.to_vec(), ways.to_vec(), relations.to_vec()])
    }

    fn classify_all(entries: &[(OscAction, OscKind, i64)], applied: &DeleteSet) -> MergeDiffReport {
        let mut report = MergeDiffReport::default();
        for &(action, kind, id) in entries {
            report.classify(action, kind, id, applied);
        }
        report
    }

    use OscAction::{Create, Delete, Modify};
    use OscKind::{Node, Relation, Way};

    #[test]
    fn delete_set_is_sorted_deduplicated_and_searchable() {
        let set = deletes(&[9, 1, 9, 4], &[], &[7]);
        assert_eq!(set.ids[0], vec![1, 4, 9]);
        assert!(set.contains(Node, 4));
        assert!(!set.contains(Way, 4));
        assert!(set.contains(Relation, 7));
    }

    #[test]
    fn classify_buckets_each_action() {
        let applied = deletes(&[10], &[], &[]);
        let report = classify_all(
            &[
                (Create, Node, 10),
                (Create, Node, 20),
                (Create, Way, 30),
                (Modify, Node, 40),
                (Delete, Way, 50),
            ],
            &applied,
        );
        assert_eq!(report.osmium_only_explained.kinds[0].ids(), vec![10]);
        assert_eq!(report.osmium_only_unexplained.kinds[0].ids(), vec![20]);
        assert_eq!(report.osmium_only_unexplained.kinds[1].ids(), vec![30]);
        assert_eq!(report.content_mismatch.kinds[0].ids(), vec![40]);
        assert_eq!(report.pbfhogg_only.kinds[1].ids(), vec![50]);
        assert!(report.is_failure());
        assert_eq!(report.total_failures(), 4);
    }

    #[test]
    fn classify_clean_when_all_explained() {
        let applied = deletes(&[1, 2, 3], &[10], &[100]);
        let report = classify_all(
            &[
                (Create, Node, 1),
                (Create, Node, 2),
                (Create, Node, 3),
                (Create, Way, 10),
                (Create, Relation, 100),
            ],
            &applied,
        );
        assert!(!report.is_failure());
        assert!(!report.is_empty());
        assert_eq!(report.osmium_only_explained.kinds[0].ids(), vec![1, 2, 3]);
        assert_eq!(report.osmium_only_explained.total(), 5);
    }

    #[test]
    fn pbfhogg_only_and_modify_always_fail() {
        // pbfhogg-only entries fail even when the input requested the delete:
        // osmium deleted them too, so pbfhogg should not be holding them.
        let applied = deletes(&[7, 8], &[], &[]);
        let report = classify_all(
            &[(Delete, Node, 7), (Delete, Node, 8), (Modify, Node, 42)],
            &applied,
        );
        assert_eq!(report.pbfhogg_only.kinds[0].ids(), vec![7, 8]);
        assert_eq!(report.content_mismatch.kinds[0].ids(), vec![42]);
        assert_eq!(report.total_failures(), 3);
    }

    #[test]
    fn a_huge_regression_keeps_exact_counts_and_bounded_samples() {
        // The bug: every differing ID was copied into report vectors, so the
        // failure path held a planet-sized regression in memory.
        let applied = deletes(&[], &[], &[]);
        let mut report = MergeDiffReport::default();
        for id in (0..100_000).rev() {
            report.classify(Create, Node, id, &applied);
        }
        let sampled = &report.osmium_only_unexplained.kinds[0];
        assert_eq!(sampled.count, 100_000);
        assert_eq!(sampled.ids(), (0..10).collect::<Vec<i64>>());
        assert_eq!(element_diff::MAX_REPORTED_IDS, 10);
        assert_eq!(report.total_failures(), 100_000);
    }

    #[test]
    fn a_stale_summary_carries_the_reports_mismatch_count() {
        let report = "Elements: 52494246 nodes, 6616927 ways, 60836789 way node refs\n\
                      Refs with a node record: 60836196 verified, 563 mismatched (538 nodes)\n";
        assert_eq!(stale_summary(report), "STALE (563 mismatched (538 nodes))");
        assert_eq!(stale_summary("something else\n"), "STALE");
    }

    #[test]
    fn an_empty_report_is_identical() {
        assert!(MergeDiffReport::default().is_empty());
    }
}
