//! Verify: apply-changes - pbfhogg held to the verifier's own model of the
//! input OSC and base for every identity the OSC touches, and to osmium's
//! output for every other identity.
//!
//! The two tools resolve a touched identity by different documented rules
//! (DEVIATIONS.md, "Change stream"): pbfhogg by the last change in file
//! order, osmium by version, then valid timestamp, against the base too. So
//! osmium is no oracle for a touched identity; [`super::apply_model`] predicts
//! both tools' answers from the inputs, pbfhogg is judged against its
//! prediction, and osmium against its own where the model defines it. The
//! model also predicts pbfhogg's version-regression refusal, which the check
//! then holds pbfhogg to. An identity the OSC leaves alone is copied from the
//! base by both tools, so there osmium's output is the reference.
//!
//! osmosis is not run, on purpose. It deletes regardless of version, which
//! once made it the independent witness for deletion, but its output only
//! ever fed a printed element-count line, and equal counts prove no element
//! identity. Deletion is now judged from the input model, so the run (about
//! 37 s of a 58 s uncached denmark check on 2026-11-15) and the osmosis and
//! JDK provisioning behind it bought no verdict and were removed.
//!
//! osmconvert is not a reference here, on purpose. Run for real on
//! 2026-11-15 (bygg, upstream osmconvert 0.8.10), it cannot read a PBF whose
//! BlobHeaders carry indexdata: its reader accepts only an 11..17 byte
//! BlobHeader, so every PBF pbfhogg writes is refused ("format blob header
//! 880"), and some refusals exit 0 with an empty output. The check runs on
//! the indexed variant, so osmconvert never produced anything to look at.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use super::apply_model::{Key, Model, OsmiumExpect};
use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::WayLocations::{Dropped, Kept};
use crate::harness::{Harness, Verdict};
use crate::opl_compare::{self, Class, Group, Policy, parse_key, rank};
use crate::osc::{self, OscKind};
use crate::output;
use crate::reference::ToolRun;
use crate::verify_element_diff::PerType;
use crate::verify_input_order;

/// The header feature `--locations-on-ways` requires of its base.
const LOCATIONS_ON_WAYS: &str = "LocationsOnWays";

/// The fields every comparison here reads. Inline way coordinates are left
/// out in both modes: plain mode drops them with the declaration
/// (`Harness::judge_way_locations` holds that), and in locations mode
/// osmium's are known stale and the OSC's ways carry none, so
/// `pbfhogg check --locations` judges them instead.
const POLICY: Policy = Policy::NO_LOCATIONS;

/// Cross-validate `pbfhogg apply-changes` against the change model and
/// osmium.
///
/// With `locations_on_ways`, both tools run with `--locations-on-ways` and
/// pbfhogg's output must pass `pbfhogg check --locations`, the only check
/// that catches a stale coordinate - a way the diff leaves untouched under a
/// node it moves. osmium's output is checked the same way but only
/// reported: its location index can itself write stale coordinates (several
/// versions of one node, unsorted lookups), so it is not the oracle.
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

    let model = read_model(harness, pbf, osc, &outdir)?;
    if let Some(why) = model.refusal() {
        return expect_version_refusal(harness, pbfhogg_args, &pbfhogg_out, &why);
    }

    harness.msg("--- pbfhogg apply-changes ---");
    let captured = harness.pbfhogg_discard(pbfhogg_args)?;
    harness.require_success(&captured, "pbfhogg apply-changes")?;
    let kept = if locations_on_ways { Kept } else { Dropped };
    harness.judge_way_locations("pbfhogg apply-changes", pbf, &pbfhogg_out, &captured, kept)?;

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

    // --- Element counts ---
    // pbfhogg's output is summarised by its sort check below.
    harness.msg("=== element counts ===");
    harness.print_fileinfo("osmium", &osmium_out)?;

    // --- Sort check ---
    // Its own decode, not folded into the element judgment's strict merge:
    // the inline-location check and the expected stream's construction come
    // between, and an error in either would lose this verdict.
    let order = harness.check_sorted("pbfhogg apply-changes", &pbfhogg_out)?;
    harness.record("pbfhogg apply-changes output order", order);

    // --- Inline locations (-l only) ---
    // Before the element judgment, which reports its failure as an `Err` and
    // so would otherwise keep this verdict from being recorded.
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

    judge_elements(
        harness,
        &model,
        &pbfhogg_out,
        &osmium_out,
        &outdir,
        locations_on_ways,
    )?;

    Ok(Some(if locations_on_ways {
        "touched identities as the change model predicts; untouched identities equal to osmium; osmium held to its precedence model where cursor-independent; output strictly ordered; inline locations pass pbfhogg check --locations; osmium locations informational"
    } else {
        "touched identities as the change model predicts; untouched identities equal to osmium; osmium held to its precedence model; output strictly ordered"
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
    header: &crate::pbf_frames::Header,
    args: PbfArgv,
    direct_io: bool,
    locations_on_ways: bool,
) -> Result<Option<String>, Error> {
    if locations_on_ways && !header.optional_feature(LOCATIONS_ON_WAYS) {
        return Err(Error::Failed(format!(
            "{} declares neither {} nor {LOCATIONS_ON_WAYS}, two refusals of apply-changes \
             --locations-on-ways, so neither can be held alone",
            pbf.display(),
            crate::pbf_frames::SORT_TYPE_THEN_ID
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

/// The change model of `osc` over `pbf`: the verifier's OSC scan joined with
/// osmium's decoding of the same file, and the base records of the touched
/// identities (`osmium getid -H -I`, which takes its IDs from the OSC).
fn read_model(
    harness: &mut Harness,
    pbf: &Path,
    osc: &Path,
    outdir: &Path,
) -> Result<Model, Error> {
    harness.msg("=== change model ===");
    let mut changes = Vec::new();
    osc::scan_osc_changes(osc, |change| changes.push(change))?;
    let osc_opl = outdir.join("input-osc.opl");
    opl_compare::decode(harness, osc, &osc_opl, POLICY)?;
    let mut model = Model::build(&changes, opl_lines(&osc_opl)?)?;
    std::fs::remove_file(&osc_opl)?;
    let occurrences = changes.len();
    drop(changes);
    if occurrences > 0 {
        let base_opl = outdir.join("base-touched.opl");
        let getid = ToolRun::osmium("apply-changes-base.1")
            .arg("getid")
            .input(pbf)
            .args(&["-H", "-I"])
            .input(osc)
            .args(&["-f", &POLICY.format(), "-o"])
            .output(&base_opl)
            .arg("--overwrite");
        let captured = harness.reference(&getid, &outdir.join("base-touched.stdout"))?;
        harness.require_success(&captured, "osmium getid -H")?;
        for line in opl_lines(&base_opl)? {
            model.add_base(line?)?;
        }
    }
    harness.msg(&format!(
        "  {occurrences} OSC occurrences over {} identities, {} held by the base",
        model.identities(),
        model.base_records()
    ));
    Ok(model)
}

/// The lines of an OPL file, each without its one terminating LF. A last
/// line without its LF is refused as truncation, as every other verifier OPL
/// reader refuses it: osmium's own OPL parser accepts one, so a cut-short
/// file would otherwise read as complete, and `write_expected` would even
/// re-terminate it. The iterator ends after its first error.
fn opl_lines(path: &Path) -> Result<impl Iterator<Item = Result<Vec<u8>, Error>>, Error> {
    let file = File::open(path)
        .map_err(|e| Error::Failed(format!("cannot open {}: {e}", path.display())))?;
    let mut reader = BufReader::new(file);
    let path = path.to_path_buf();
    let mut number = 0u64;
    let mut done = false;
    Ok(std::iter::from_fn(move || {
        if done {
            return None;
        }
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line) {
            Err(e) => {
                done = true;
                Some(Err(Error::from(e)))
            }
            Ok(0) => {
                done = true;
                None
            }
            Ok(_) => {
                number += 1;
                if line.pop() == Some(b'\n') {
                    Some(Ok(line))
                } else {
                    done = true;
                    Some(Err(Error::Failed(format!(
                        "OPL {} line {number}: no terminating newline (truncated?)",
                        path.display()
                    ))))
                }
            }
        }
    }))
}

/// Hold `args` to the version-regression refusal the model predicts: exit 2
/// with the regression's `--allow-version-regression` advice, and nothing
/// published at `out`. No reference runs.
fn expect_version_refusal(
    harness: &mut Harness,
    args: PbfArgv,
    out: &Path,
    why: &str,
) -> Result<Option<String>, Error> {
    harness.msg(&format!(
        "--- pbfhogg apply-changes (expected to refuse: {why}) ---"
    ));
    let captured = harness.pbfhogg_discard(args)?;
    if std::fs::exists(out)? {
        harness.msg(&format!("  published {} FAIL", out.display()));
        harness.fail(format!(
            "pbfhogg apply-changes published {} on a version regression",
            out.display()
        ));
    }
    let refused = captured.code() == Some(2)
        && captured
            .stderr_text()
            .contains("--allow-version-regression");
    harness.msg(&format!(
        "  exit {:?}: {}",
        captured.code(),
        if refused {
            "refused the regression PASS"
        } else {
            "did not refuse the regression FAIL"
        }
    ));
    harness.record(
        "pbfhogg apply-changes refuses the version regression",
        Verdict::from_pass(refused),
    );
    Ok(Some(format!(
        "version regression refused with its advice and nothing published ({why})"
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

/// Judge pbfhogg's output, as osmium decodes it, against the expected
/// stream: osmium's output for the untouched identities with the model's
/// records for the touched ones. Then judge osmium's touched records
/// against its own model. `Err(Error::Failed(...))` if pbfhogg differs; an
/// osmium departure from its model is recorded as a failed comparison.
fn judge_elements(
    harness: &mut Harness,
    model: &Model,
    pbfhogg_out: &Path,
    osmium_out: &Path,
    outdir: &Path,
    locations_on_ways: bool,
) -> Result<(), Error> {
    harness.msg("=== pbfhogg vs the change model and osmium ===");
    let expectations = model.expectations(locations_on_ways);
    let osmium_opl = outdir.join("osmium.opl");
    opl_compare::decode(harness, osmium_out, &osmium_opl, POLICY)?;
    let expected = outdir.join("expected.opl");
    let osmium_touched = write_expected(&osmium_opl, &expected, model, &expectations)?;
    std::fs::remove_file(&osmium_opl)?;

    let mut report = Report::default();
    let listing = outdir.join("pbfhogg-vs-expected.diff.txt");
    opl_compare::compare_with_opl_each(harness, pbfhogg_out, &expected, &listing, POLICY, |g| {
        report.classify(g, model.touches((g.kind, g.id)));
    })?;
    std::fs::remove_file(&expected)?;
    report.judge_osmium(&expectations, &osmium_touched);

    report.print(harness);
    if report.osmium_off_model.total() > 0 {
        harness.fail(format!(
            "osmium apply-changes departs from the verifier's precedence model on {} \
             identities",
            report.osmium_off_model.total()
        ));
    }
    let failures = report.pbfhogg_failures();
    if failures > 0 {
        return Err(Error::Failed(format!(
            "pbfhogg apply-changes: {failures} unexplained difference entries (see element \
             diff above)"
        )));
    }
    harness.msg("  pbfhogg output is as expected PASS");
    Ok(())
}

/// Write the expected pbfhogg stream to `expected`: `osmium_opl`'s untouched
/// records, held to strict object order, merged with the model's live
/// touched records. Every osmium record of a touched identity is returned
/// instead, read in any order and any multiplicity, for the osmium judgment.
fn write_expected(
    osmium_opl: &Path,
    expected: &Path,
    model: &Model,
    expectations: &[(Key, Option<&[u8]>, OsmiumExpect)],
) -> Result<HashMap<Key, Vec<Vec<u8>>>, Error> {
    let mut out = BufWriter::new(File::create(expected)?);
    let mut live = expectations
        .iter()
        .filter_map(|&(key, pbfhogg, _)| pbfhogg.map(|line| (key, line)))
        .peekable();
    let mut touched: HashMap<Key, Vec<Vec<u8>>> = HashMap::new();
    let mut last: Option<Key> = None;
    for line in opl_lines(osmium_opl)? {
        let line = line?;
        let key = parse_key(&line)
            .map_err(|why| Error::Failed(format!("osmium apply-changes output: {why}")))?;
        if model.touches(key) {
            touched.entry(key).or_default().push(line);
            continue;
        }
        if last.is_some_and(|last| rank(key) <= rank(last)) {
            return Err(Error::Failed(
                "osmium apply-changes output: an untouched object at or below the previous \
                 one: not in strict kind and ID order"
                    .into(),
            ));
        }
        last = Some(key);
        while let Some(&(k, record)) = live.peek() {
            if rank(k) >= rank(key) {
                break;
            }
            out.write_all(record)?;
            out.write_all(b"\n")?;
            live.next();
        }
        out.write_all(&line)?;
        out.write_all(b"\n")?;
    }
    for (_, record) in live {
        out.write_all(record)?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(touched)
}

const KINDS: [OscKind; 3] = [OscKind::Node, OscKind::Way, OscKind::Relation];

/// Outcome classes, each a count per kind with bounded ID samples.
#[derive(Default)]
struct Report {
    /// Touched: pbfhogg holds a record where the model expects none (a
    /// final delete it kept).
    held_removed: PerType,
    /// Touched: pbfhogg lacks the change's final record.
    missing_final: PerType,
    /// Touched: pbfhogg's record is not the change's final record.
    not_final: PerType,
    /// Untouched: only pbfhogg holds it.
    pbfhogg_only: PerType,
    /// Untouched: only osmium holds it.
    osmium_only: PerType,
    /// Untouched: both hold it, unequal.
    content_mismatch: PerType,
    /// Touched: osmium wrote something other than its model's answer.
    osmium_off_model: PerType,
    /// Touched, informational: the two models disagree and osmium wrote its
    /// own answer.
    precedence_differs: PerType,
    /// Touched, informational: the model does not decide osmium's answer.
    osmium_undecided: PerType,
}

impl Report {
    fn classify(&mut self, group: Group, touched: bool) {
        let class = match (group.class, touched) {
            (Class::FirstOnly, true) => &mut self.held_removed,
            (Class::SecondOnly, true) => &mut self.missing_final,
            (Class::Differs, true) => &mut self.not_final,
            (Class::FirstOnly, false) => &mut self.pbfhogg_only,
            (Class::SecondOnly, false) => &mut self.osmium_only,
            (Class::Differs, false) => &mut self.content_mismatch,
        };
        class.add(KINDS[group.kind], group.id);
    }

    /// Hold osmium's touched records (`seen`) to its model: no record where
    /// it is expected to write none, exactly one equal record where it is.
    fn judge_osmium(
        &mut self,
        expectations: &[(Key, Option<&[u8]>, OsmiumExpect)],
        seen: &HashMap<Key, Vec<Vec<u8>>>,
    ) {
        for (key, pbfhogg, osmium) in expectations {
            let kind = KINDS[key.0];
            let records = seen.get(key).map_or(&[][..], Vec::as_slice);
            let (matches, agrees) = match osmium {
                OsmiumExpect::Undefined => {
                    self.osmium_undecided.add(kind, key.1);
                    continue;
                }
                OsmiumExpect::Absent => (records.is_empty(), pbfhogg.is_none()),
                OsmiumExpect::Record(line) => (
                    matches!(records, [one] if one == line),
                    *pbfhogg == Some(line.as_slice()),
                ),
            };
            if !matches {
                self.osmium_off_model.add(kind, key.1);
            } else if !agrees {
                self.precedence_differs.add(kind, key.1);
            }
        }
    }

    fn pbfhogg_failures(&self) -> u64 {
        [
            &self.held_removed,
            &self.missing_final,
            &self.not_final,
            &self.pbfhogg_only,
            &self.osmium_only,
            &self.content_mismatch,
        ]
        .iter()
        .map(|class| class.total())
        .sum()
    }

    fn print(&self, harness: &mut Harness) {
        let mut note = |label: &str, class: &PerType| {
            if class.total() > 0 {
                harness.msg(&format!(
                    "  {label}: {} (informational)",
                    class.counts_line()
                ));
            }
        };
        note(
            "osmium answers by its own precedence where pbfhogg's differs",
            &self.precedence_differs,
        );
        note(
            "osmium's answer left undecided (--locations-on-ways cursor)",
            &self.osmium_undecided,
        );
        let mut fail =
            |label: &str, class: &PerType| class.print("FAIL ", label, |line| harness.msg(line));
        fail(
            "touched, held where the change removes it",
            &self.held_removed,
        );
        fail(
            "touched, the change's final record missing",
            &self.missing_final,
        );
        fail("touched, not the change's final record", &self.not_final);
        fail("untouched, pbfhogg-only", &self.pbfhogg_only);
        fail("untouched, osmium-only", &self.osmium_only);
        fail("untouched, content differs", &self.content_mismatch);
        fail(
            "touched, osmium departs from its precedence model",
            &self.osmium_off_model,
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn opl_lines_refuse_a_last_line_without_its_newline() {
        let dir = pbfhogg_test_scratch::scratch();
        let path = dir.path().join("lines.opl");
        std::fs::write(&path, "n1 v1\n\nn2 v1\r\n").unwrap();
        let lines: Vec<Vec<u8>> = opl_lines(&path).unwrap().map(Result::unwrap).collect();
        assert_eq!(
            lines,
            vec![b"n1 v1".to_vec(), Vec::new(), b"n2 v1\r".to_vec()]
        );

        std::fs::write(&path, "n1 v1\nn2 v1").unwrap();
        let mut lines = opl_lines(&path).unwrap();
        assert_eq!(lines.next().unwrap().unwrap(), b"n1 v1");
        let err = lines.next().unwrap().unwrap_err().to_string();
        assert!(err.contains("line 2: no terminating newline"), "{err}");
        assert!(err.contains("lines.opl"), "{err}");
        assert!(lines.next().is_none());

        std::fs::write(&path, "").unwrap();
        assert!(opl_lines(&path).unwrap().next().is_none());
    }

    fn group(class: Class, id: i64) -> Group {
        Group { class, kind: 0, id }
    }

    #[test]
    fn touched_and_untouched_differences_land_in_their_own_classes() {
        let mut report = Report::default();
        report.classify(group(Class::FirstOnly, 1), true);
        report.classify(group(Class::SecondOnly, 2), true);
        report.classify(group(Class::Differs, 3), true);
        report.classify(group(Class::FirstOnly, 4), false);
        report.classify(group(Class::SecondOnly, 5), false);
        report.classify(group(Class::Differs, 6), false);
        for (class, id) in [
            (&report.held_removed, 1),
            (&report.missing_final, 2),
            (&report.not_final, 3),
            (&report.pbfhogg_only, 4),
            (&report.osmium_only, 5),
            (&report.content_mismatch, 6),
        ] {
            assert_eq!(class.kinds[0].ids(), vec![id]);
        }
        assert_eq!(report.pbfhogg_failures(), 6);
    }

    #[test]
    fn osmium_is_held_to_its_model_by_multiplicity() {
        let base = b"n1 v5".to_vec();
        let expectations = vec![
            // Agreeing absence, osmium writes nothing.
            ((0, 1), None, OsmiumExpect::Absent),
            // Precedence differs, osmium writes its own answer.
            ((0, 2), None, OsmiumExpect::Record(base.clone())),
            // osmium writes the record twice.
            ((0, 3), None, OsmiumExpect::Record(base.clone())),
            // Undecided.
            ((0, 4), None, OsmiumExpect::Undefined),
            // osmium writes something where it should write nothing.
            ((0, 5), None, OsmiumExpect::Absent),
        ];
        let seen = HashMap::from([
            ((0, 2), vec![base.clone()]),
            ((0, 3), vec![base.clone(), base.clone()]),
            ((0, 5), vec![base.clone()]),
        ]);
        let mut report = Report::default();
        report.judge_osmium(&expectations, &seen);
        assert_eq!(report.precedence_differs.kinds[0].ids(), vec![2]);
        assert_eq!(report.osmium_off_model.kinds[0].ids(), vec![3, 5]);
        assert_eq!(report.osmium_undecided.kinds[0].ids(), vec![4]);
        assert_eq!(report.pbfhogg_failures(), 0);
    }

    #[test]
    fn a_stale_summary_carries_the_reports_mismatch_count() {
        let report = "Elements: 52494246 nodes, 6616927 ways, 60836789 way node refs\n\
                      Refs with a node record: 60836196 verified, 563 mismatched (538 nodes)\n";
        assert_eq!(stale_summary(report), "STALE (563 mismatched (538 nodes))");
        assert_eq!(stale_summary("something else\n"), "STALE");
    }
}
