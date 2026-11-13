//! Verify: check --refs - pbfhogg check --refs vs osmium check-refs.
//!
//! The two tools use different output formats, so we parse the missing-reference
//! counts structurally rather than comparing raw text. Known semantic differences:
//!
//! - Ways-only mode: pbfhogg skips relation blobs and reports "0 relations" in its
//!   element summary, while osmium always reports the full relation count. Both
//!   agree on the actual check result (missing node reference count).
//!
//! - With-relations mode: pbfhogg reports "Missing relation members: 706 (777
//!   references)" where 777 is the occurrence count that matches osmium's output.
//!   We extract the trailing number (the references count) for comparison.
//!
//! Each side's exit must agree with its report: exit 1 exactly when something
//! is missing. pbfhogg is the command under test, so a disagreement there is
//! a failed finding. osmium also exits 1 when an exception ends its run
//! before any count, so its exit 1 is accepted only on a complete summary
//! (`ToolRun::accept_if`, at publication, at lookup and here), and a report
//! that does not parse or contradicts its exit is an error: nothing can be
//! compared with it.
//!
//! `check --refs` takes input in any order; `osmium check-refs` refuses
//! out-of-order input, so on input without the sort claim it reads the
//! osmium-sorted copy (`verify_input_order`), whose elements are the input's.

use std::fs;
use std::path::Path;

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::{Harness, Verdict};
use crate::reference::ToolRun;
use crate::verify_input_order::InputOrder;

/// What a passing check establishes on input without the sort claim.
const CLAIM_UNSORTED: &str = "on unsorted input, osmium check-refs reads an osmium-sorted copy, \
                              pbfhogg the input as it is";

/// Parsed pbfhogg check --refs counts for structural comparison.
#[derive(Default)]
struct CheckRefCounts {
    /// Missing node references in ways (ways-only and with-relations modes).
    nodes_in_ways: Option<u64>,
    /// Missing node members in relations.
    nodes_in_relations: Option<u64>,
    /// Missing way members in relations.
    ways_in_relations: Option<u64>,
    /// Missing relation member references (occurrences). pbfhogg outputs
    /// "N (M references)" - we extract M. osmium outputs M directly.
    relations_in_relations: Option<u64>,
    /// The distinct missing relation members, the N of "N (M references)".
    /// It is N, not M, that sums into the total.
    relations_distinct: Option<u64>,
    /// The N of "Referential integrity: FAILED (N missing references)".
    total_missing: Option<u64>,
    /// Whether the output indicates all checks passed with no issues.
    integrity_ok: bool,
    /// A verdict or class line in an unknown shape, or one repeated.
    malformed: bool,
}

impl CheckRefCounts {
    /// A clean report: the OK verdict with no FAILED verdict and no class
    /// line beside it. Either beside it contradicts it, and a malformed
    /// report is neither clean nor dirty.
    fn clean(&self) -> Option<bool> {
        if self.malformed {
            return None;
        }
        let any_class = [
            self.nodes_in_ways,
            self.nodes_in_relations,
            self.ways_in_relations,
            self.relations_in_relations,
        ]
        .iter()
        .any(Option::is_some);
        match (self.integrity_ok, self.total_missing.is_some() || any_class) {
            (true, true) => None,
            (ok, _) => Some(ok),
        }
    }
}

/// pbfhogg's ways-only missing-node-in-ways count.
///
/// A clean file says `Referential integrity: OK` (0), a dirty one `Missing
/// node refs in ways: N` and `FAILED (N missing references)`; with only way
/// node refs checked the two must be equal. Anything else is `None`.
fn pbfhogg_ways_only(counts: &CheckRefCounts) -> Option<u64> {
    if counts.clean()? {
        return Some(0);
    }
    let in_ways = counts.nodes_in_ways?;
    (counts.nodes_in_relations.is_none()
        && counts.ways_in_relations.is_none()
        && counts.relations_in_relations.is_none()
        && counts.total_missing == Some(in_ways))
    .then_some(in_ways)
}

/// pbfhogg's with-relations counts as `[nodes in ways, nodes in relations,
/// ways in relations, relation member references]`.
///
/// A clean file prints only `Referential integrity: OK`, which is all zeros.
/// A dirty file omits the lines of classes with nothing missing, so an absent
/// class line is taken as 0 - but only when the classes present account for
/// the whole FAILED total. If they do not, some line went unrecognised and
/// this returns `None` rather than guess.
fn pbfhogg_with_relations(counts: &CheckRefCounts) -> Option<[u64; 4]> {
    if counts.clean()? {
        return Some([0; 4]);
    }
    let total = counts.total_missing?;
    let in_ways = counts.nodes_in_ways.unwrap_or(0);
    let nodes = counts.nodes_in_relations.unwrap_or(0);
    let ways = counts.ways_in_relations.unwrap_or(0);
    let rel_distinct = counts.relations_distinct.unwrap_or(0);
    let rel_refs = counts.relations_in_relations.unwrap_or(0);
    let accounted = in_ways
        .checked_add(nodes)?
        .checked_add(ways)?
        .checked_add(rel_distinct)?;
    (accounted == total).then_some([in_ways, nodes, ways, rel_refs])
}

/// Parse pbfhogg check --refs output.
///
/// Ways-only format:
/// ```text
/// Elements: 52489653 nodes, 6616526 ways, 0 relations
/// Referential integrity: OK
/// ```
///
/// With-relations format:
/// ```text
/// Elements: 52489653 nodes, 6616526 ways, 46103 relations
/// Missing way refs in relations: 32943
/// Missing node members in relations: 441
/// Missing relation members: 706 (777 references)
/// Referential integrity: FAILED (34090 missing references)
/// ```
///
/// Every line starting `Referential integrity` or `Missing ` must be one of
/// these shapes, each at most once; anything else marks the report
/// malformed, which leaves no count to compare.
fn parse_pbfhogg(text: &str) -> CheckRefCounts {
    let mut counts = CheckRefCounts::default();
    let mut ok_lines = 0;
    for line in text.lines().map(str::trim) {
        if !(line.starts_with("Referential integrity") || line.starts_with("Missing ")) {
            continue;
        }
        let parsed = if line == "Referential integrity: OK" {
            ok_lines += 1;
            counts.integrity_ok = true;
            ok_lines == 1
        } else if let Some(rest) = line.strip_prefix("Referential integrity: FAILED (") {
            rest.strip_suffix(" missing references)")
                .and_then(decimal)
                .is_some_and(|n| set_once(&mut counts.total_missing, n))
        } else if let Some(rest) = line.strip_prefix("Missing node refs in ways: ") {
            decimal(rest).is_some_and(|n| set_once(&mut counts.nodes_in_ways, n))
        } else if let Some(rest) = line.strip_prefix("Missing node members in relations: ") {
            decimal(rest).is_some_and(|n| set_once(&mut counts.nodes_in_relations, n))
        } else if let Some(rest) = line.strip_prefix("Missing way refs in relations: ") {
            decimal(rest).is_some_and(|n| set_once(&mut counts.ways_in_relations, n))
        } else if let Some(rest) = line.strip_prefix("Missing relation members: ") {
            let (distinct, refs) = match rest.split_once(" (") {
                Some((distinct, refs)) => (distinct, refs.strip_suffix(" references)")),
                None => (rest, Some(rest)),
            };
            match (decimal(distinct), refs.and_then(decimal)) {
                (Some(d), Some(r)) => {
                    set_once(&mut counts.relations_distinct, d)
                        && set_once(&mut counts.relations_in_relations, r)
                }
                _ => false,
            }
        } else {
            false
        };
        if !parsed {
            counts.malformed = true;
        }
    }
    counts
}

/// A plain decimal count.
fn decimal(s: &str) -> Option<u64> {
    if is_decimal(s) { s.parse().ok() } else { None }
}

/// Store `n` in a field not yet set; false when it already was.
fn set_once(field: &mut Option<u64>, n: u64) -> bool {
    field.replace(n).is_none()
}

/// pbfhogg's verdict line.
#[derive(Debug, PartialEq, Eq)]
enum Integrity {
    Ok,
    Failed(u64),
}

/// The one verdict line of a pbfhogg report: `Referential integrity: OK` or
/// `Referential integrity: FAILED (N missing references)`. None when there
/// is no such line, more than one, or one in another shape.
fn pbfhogg_integrity(text: &str) -> Option<Integrity> {
    let mut found = None;
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("Referential integrity: ") else {
            continue;
        };
        if found.is_some() {
            return None;
        }
        found = Some(if rest == "OK" {
            Integrity::Ok
        } else {
            let n = rest
                .strip_prefix("FAILED (")?
                .strip_suffix(" missing references)")?;
            if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Integrity::Failed(n.parse().ok()?)
        });
    }
    found
}

/// Whether pbfhogg's exit agrees with its verdict: 0 with OK, 1 with a
/// FAILED naming a positive count.
fn pbfhogg_exit_agrees(code: i32, integrity: Option<&Integrity>) -> bool {
    match (code, integrity) {
        (0, Some(Integrity::Ok)) => true,
        (1, Some(Integrity::Failed(n))) => *n > 0,
        _ => false,
    }
}

/// osmium check-refs's summary, which it prints to stderr only once its whole
/// read is done: `There are N nodes, M ways, and K relations in this file.`
/// followed by the count lines of its mode.
#[derive(Debug, PartialEq, Eq)]
struct OsmiumReport {
    /// The mode's counts in the order osmium prints them: nodes in ways,
    /// and with relations also nodes, ways and relations in relations.
    counts: Vec<u64>,
}

impl OsmiumReport {
    /// Whether osmium found anything missing, which is its exit 1.
    fn any_missing(&self) -> bool {
        self.counts.iter().any(|&n| n > 0)
    }
}

/// The count lines of each mode, whitespace collapsed.
const WAYS_ONLY_LINES: &[&str] = &["Nodes in ways missing"];
const WITH_RELATIONS_LINES: &[&str] = &[
    "Nodes in ways missing",
    "Nodes in relations missing",
    "Ways in relations missing",
    "Relations in relations missing",
];

/// Parse osmium check-refs's summary from its stderr.
///
/// Ways-only format:
/// ```text
/// There are 52489653 nodes, 6616526 ways, and 46103 relations in this file.
/// Nodes in ways missing: 0
/// ```
///
/// With-relations format:
/// ```text
/// There are 52489653 nodes, 6616526 ways, and 46103 relations in this file.
/// Nodes     in ways      missing: 0
/// Nodes     in relations missing: 441
/// Ways      in relations missing: 32943
/// Relations in relations missing: 777
/// ```
///
/// The summary must be the last lines, complete and in order; an earlier
/// line that reads as part of a summary makes the report ambiguous. Anything
/// else is `None`: the run did not complete (an exception, such as `Node IDs
/// out of order`, ends it before the summary) or printed something this
/// reader does not know.
fn osmium_report(stderr: &[u8], with_relations: bool) -> Option<OsmiumReport> {
    let text = std::str::from_utf8(stderr).ok()?;
    let labels = if with_relations {
        WITH_RELATIONS_LINES
    } else {
        WAYS_ONLY_LINES
    };
    let lines: Vec<String> = text
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect();
    let start = lines.len().checked_sub(labels.len() + 1)?;
    let (before, summary) = lines.split_at(start);
    let summary_like = |line: &str| {
        line.starts_with("There are ")
            || WITH_RELATIONS_LINES
                .iter()
                .any(|label| line.starts_with(&format!("{label}:")))
    };
    if before.iter().any(|l| summary_like(l)) {
        return None;
    }
    let totals = summary[0]
        .strip_prefix("There are ")?
        .strip_suffix(" relations in this file.")?;
    let (nodes, rest) = totals.split_once(" nodes, ")?;
    let (ways, relations) = rest.split_once(" ways, and ")?;
    if ![nodes, ways, relations].iter().all(|n| is_decimal(n)) {
        return None;
    }
    let mut counts = Vec::with_capacity(labels.len());
    for (line, label) in summary[1..].iter().zip(labels) {
        let n = line.strip_prefix(label)?.strip_prefix(": ")?;
        if !is_decimal(n) {
            return None;
        }
        counts.push(n.parse().ok()?);
    }
    Some(OsmiumReport { counts })
}

fn is_decimal(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// The completion conditions osmium's exit 1 is accepted on, per mode.
fn osmium_ways_only_completed(stderr: &[u8]) -> bool {
    osmium_report(stderr, false).is_some()
}

fn osmium_with_relations_completed(stderr: &[u8]) -> bool {
    osmium_report(stderr, true).is_some()
}

/// Run pbfhogg's side, save and print its report, and hold its exit to it:
/// exits 0 and 1 are answers, anything else an error. Returns the report and
/// whether its exit agrees with it.
fn run_pbfhogg_side(
    harness: &mut Harness,
    argv: PbfArgv,
    out_file: &Path,
    label: &str,
) -> Result<(String, Verdict), Error> {
    let captured = harness.pbfhogg(argv, out_file)?;
    if !matches!(captured.code(), Some(0 | 1)) {
        harness.require_success(&captured, label)?;
    }
    // check --refs reports to stdout; the sink belongs to the verifier.
    let text = crate::stream::read_report(out_file)?;
    harness.msg(&format!("  {label}:"));
    for line in text.lines() {
        harness.msg(&format!("    {line}"));
    }
    let code = captured
        .code()
        .ok_or_else(|| Error::Failed(format!("{label} answered without an exit code")))?;
    let agrees = pbfhogg_exit_agrees(code, pbfhogg_integrity(&text).as_ref());
    if !agrees {
        harness.msg(&format!(
            "  {label}: exit {code} disagrees with its report's one integrity verdict FAIL"
        ));
    }
    Ok((text, Verdict::from_pass(agrees)))
}

/// Run osmium's side and read its summary. Exit 1 is accepted only on a
/// complete summary (the run's `accept_if`, checked again here); the exit
/// must agree with the counts, and a summary that does not parse is an error.
fn run_osmium_side(
    harness: &mut Harness,
    run: &ToolRun,
    out_file: &Path,
    label: &str,
    with_relations: bool,
) -> Result<OsmiumReport, Error> {
    let sink = out_file.with_extension("stdout");
    let captured = harness.reference(run, &sink)?;
    // Without --show-ids osmium writes nothing to stdout; its report is the
    // stderr summary.
    let printed = fs::metadata(&sink)?.len();
    if printed > 0 {
        return Err(Error::Failed(format!(
            "{label} wrote {printed} bytes to stdout ({}), where without --show-ids it \
             writes nothing",
            sink.display()
        )));
    }
    let text = String::from_utf8_lossy(&captured.stderr).into_owned();
    fs::write(out_file, &text)?;
    harness.msg(&format!("  {label}:"));
    for line in text.lines() {
        harness.msg(&format!("    {line}"));
    }
    let report = osmium_report(&captured.stderr, with_relations).ok_or_else(|| {
        Error::Failed(format!(
            "{label} did not complete: its stderr does not end in the summary osmium \
             check-refs prints after a whole read, so nothing can be compared with it"
        ))
    })?;
    let code = captured.code();
    if code != Some(i32::from(report.any_missing())) {
        return Err(Error::Failed(format!(
            "{label} exited {code:?}, which contradicts its summary {:?}",
            report.counts
        )));
    }
    Ok(report)
}

/// Run check --refs cross-validation: pbfhogg check --refs vs osmium check-refs.
///
/// Two modes: ways-only (default) and with relations. Both tools exit 1 when
/// they find missing refs.
///
/// No `--direct-io`: pbfhogg refuses it under `check --refs` without `--ids`
/// or `--index` (exit 64, since pbfhogg 930ddfc9), since the ref check reads
/// through buffered parallel unaligned preads. Only revisions from fb8dd3c0
/// until 053def6a read it, through a sequential reader; brokkr run --with pbfhogg pbfhogg-verify -- runs the current
/// operation set whatever revision `--commit` builds, so O_DIRECT coverage of
/// those revisions' ref check is given up rather than reached by probing the
/// binary or retrying a refusal.
pub fn run(harness: &mut Harness, pbf: &Path) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("check-refs")?;
    let order = InputOrder::of(harness, pbf, &outdir)?;

    // --- Ways only ---
    harness.msg("--- check --refs (ways only) ---");
    let (pbfhogg_text, agrees) = run_pbfhogg_side(
        harness,
        PbfArgv::new("check").arg("--refs").input(pbf),
        &outdir.join("pbfhogg-ways.txt"),
        "pbfhogg (ways only)",
    )?;
    harness.record("pbfhogg (ways only) exit agrees with its report", agrees);
    let osmium = run_osmium_side(
        harness,
        &ToolRun::osmium("check-refs.1")
            .arg("check-refs")
            .input(&order.reference_input)
            .accept_if(1, osmium_ways_only_completed),
        &outdir.join("osmium-ways.txt"),
        "osmium (ways only)",
        false,
    )?;
    let ours = pbfhogg_ways_only(&parse_pbfhogg(&pbfhogg_text));
    let theirs = osmium.counts.first().copied();
    harness.msg(&format!(
        "  ways only counts: pbfhogg={ours:?}, osmium={theirs:?}"
    ));
    let verdict = compare_count("ways only", ours, theirs);
    harness.record("ways only", verdict);

    // --- With relations ---
    harness.msg("--- check --refs (with relations) ---");
    let (pbfhogg_text, agrees) = run_pbfhogg_side(
        harness,
        PbfArgv::new("check")
            .arg("--refs")
            .input(pbf)
            .arg("--check-relations"),
        &outdir.join("pbfhogg-all.txt"),
        "pbfhogg (with relations)",
    )?;
    harness.record(
        "pbfhogg (with relations) exit agrees with its report",
        agrees,
    );
    let osmium = run_osmium_side(
        harness,
        &ToolRun::osmium("check-refs.1")
            .args(&["check-refs", "-r"])
            .input(&order.reference_input)
            .accept_if(1, osmium_with_relations_completed),
        &outdir.join("osmium-all.txt"),
        "osmium (with relations)",
        true,
    )?;

    let pbf_rel = pbfhogg_with_relations(&parse_pbfhogg(&pbfhogg_text));
    if pbf_rel.is_none() {
        harness.msg(
            "  pbfhogg (with relations): per-class counts do not reconcile with the \
             FAILED total (or no verdict line) - output format changed?",
        );
    }
    for (i, label) in [
        "nodes in ways",
        "nodes in relations",
        "ways in relations",
        "relation members",
    ]
    .into_iter()
    .enumerate()
    {
        let ours = pbf_rel.map(|counts| counts[i]);
        let theirs = osmium.counts.get(i).copied();
        harness.msg(&format!(
            "  {label} counts: pbfhogg={ours:?}, osmium={theirs:?}"
        ));
        let verdict = compare_count(label, ours, theirs);
        harness.record(label, verdict);
    }

    Ok(if order.sorted {
        None
    } else {
        Some(CLAIM_UNSORTED.to_owned())
    })
}

fn compare_count(_label: &str, pbfhogg: Option<u64>, osmium: Option<u64>) -> Verdict {
    Verdict::from_pass(matches!((pbfhogg, osmium), (Some(p), Some(o)) if p == o))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const PBFHOGG_WAYS_OK: &str = "\
Elements: 52489653 nodes, 6616526 ways, 0 relations
Referential integrity: OK
";

    const PBFHOGG_RELATIONS_FAILED: &str = "\
Elements: 52489653 nodes, 6616526 ways, 46103 relations
Missing way refs in relations: 32943
Missing node members in relations: 441
Missing relation members: 706 (777 references)
Referential integrity: FAILED (34090 missing references)
";

    const OSMIUM_WAYS: &str = "\
There are 52489653 nodes, 6616526 ways, and 46103 relations in this file.
Nodes in ways missing: 0
";

    const OSMIUM_RELATIONS: &str = "\
There are 52489653 nodes, 6616526 ways, and 46103 relations in this file.
Nodes     in ways      missing: 0
Nodes     in relations missing: 441
Ways      in relations missing: 32943
Relations in relations missing: 777
";

    const OSMIUM_OUT_OF_ORDER: &str = "\
Node IDs out of order: 7658480
This command expects the input file to be ordered: First nodes in order of ID,
then ways in order of ID, then relations in order of ID.
";

    #[test]
    fn pbfhogg_ways_only_clean_is_zero() {
        let c = parse_pbfhogg(PBFHOGG_WAYS_OK);
        assert!(c.integrity_ok);
        assert_eq!(pbfhogg_ways_only(&c), Some(0));
    }

    #[test]
    fn pbfhogg_ways_only_failed_uses_its_count() {
        // A FAILED total with no class line names no count.
        let c = parse_pbfhogg("Referential integrity: FAILED (12 missing references)\n");
        assert_eq!(pbfhogg_ways_only(&c), None);
        // A class count the total does not equal.
        let c = parse_pbfhogg(
            "Missing node refs in ways: 12\nReferential integrity: FAILED (13 missing \
             references)\n",
        );
        assert_eq!(pbfhogg_ways_only(&c), None);
        // An OK verdict beside a class count contradicts it.
        let c = parse_pbfhogg("Missing node refs in ways: 5\nReferential integrity: OK\n");
        assert_eq!(pbfhogg_ways_only(&c), None);
        assert_eq!(pbfhogg_with_relations(&c), None);
        // A class line that does not parse, or one repeated, is malformed.
        for text in [
            "Missing node refs in ways: invalid\nReferential integrity: OK\n",
            "Missing node refs in ways: 5\nMissing node refs in ways: 5\nReferential \
             integrity: FAILED (5 missing references)\n",
            "Missing node refs in ways: +5\nReferential integrity: FAILED (5 missing \
             references)\n",
            "Referential integrity: OK\nReferential integrity: OK\n",
        ] {
            let c = parse_pbfhogg(text);
            assert!(c.malformed, "{text}");
            assert_eq!(pbfhogg_ways_only(&c), None, "{text}");
            assert_eq!(pbfhogg_with_relations(&c), None, "{text}");
        }
        let c = parse_pbfhogg(
            "Missing node refs in ways: 12\nReferential integrity: FAILED (12 missing \
             references)\n",
        );
        assert_eq!(c.nodes_in_ways, Some(12));
        assert_eq!(pbfhogg_ways_only(&c), Some(12));
    }

    #[test]
    fn pbfhogg_with_relations_sample_parses() {
        let c = parse_pbfhogg(PBFHOGG_RELATIONS_FAILED);
        assert_eq!(c.ways_in_relations, Some(32_943));
        assert_eq!(c.nodes_in_relations, Some(441));
        assert_eq!(c.relations_in_relations, Some(777));
        assert_eq!(c.relations_distinct, Some(706));
        assert_eq!(c.total_missing, Some(34_090));
        assert_eq!(pbfhogg_with_relations(&c), Some([0, 441, 32_943, 777]));
    }

    #[test]
    fn pbfhogg_with_relations_counts_node_refs_in_ways() {
        let text = "\
Missing node refs in ways: 3
Missing way refs in relations: 10
Referential integrity: FAILED (13 missing references)
";
        assert_eq!(
            pbfhogg_with_relations(&parse_pbfhogg(text)),
            Some([3, 0, 10, 0])
        );
    }

    #[test]
    fn pbfhogg_with_relations_clean_dataset_is_all_zero() {
        // The bug: a clean dataset prints only the OK line and used to read
        // as "could not parse counts".
        let text = "\
Elements: 52489653 nodes, 6616526 ways, 46103 relations
Referential integrity: OK
";
        let c = parse_pbfhogg(text);
        assert_eq!(pbfhogg_with_relations(&c), Some([0; 4]));
    }

    #[test]
    fn pbfhogg_with_relations_absent_class_is_zero_when_total_reconciles() {
        let text = "\
Missing way refs in relations: 10
Referential integrity: FAILED (10 missing references)
";
        let c = parse_pbfhogg(text);
        assert_eq!(pbfhogg_with_relations(&c), Some([0, 0, 10, 0]));
    }

    #[test]
    fn pbfhogg_with_relations_unreconciled_total_is_none() {
        // A class line the parser does not recognise leaves the total
        // unaccounted for - refuse rather than read the gap as zero.
        let text = "\
Missing way refs in relations: 10
Missing something new: 5
Referential integrity: FAILED (15 missing references)
";
        let c = parse_pbfhogg(text);
        assert_eq!(pbfhogg_with_relations(&c), None);
        // No verdict line at all is not a clean dataset either.
        assert_eq!(pbfhogg_with_relations(&parse_pbfhogg("garbage\n")), None);
    }

    #[test]
    fn pbfhogg_exit_must_agree_with_one_verdict() {
        let ok = pbfhogg_integrity(PBFHOGG_WAYS_OK);
        let failed = pbfhogg_integrity(PBFHOGG_RELATIONS_FAILED);
        assert_eq!(ok, Some(Integrity::Ok));
        assert_eq!(failed, Some(Integrity::Failed(34_090)));
        assert!(pbfhogg_exit_agrees(0, ok.as_ref()));
        assert!(pbfhogg_exit_agrees(1, failed.as_ref()));
        assert!(!pbfhogg_exit_agrees(1, ok.as_ref()));
        assert!(!pbfhogg_exit_agrees(0, failed.as_ref()));
        let zero = pbfhogg_integrity("Referential integrity: FAILED (0 missing references)\n");
        assert!(!pbfhogg_exit_agrees(1, zero.as_ref()));
        assert_eq!(
            pbfhogg_integrity("Referential integrity: OK\nReferential integrity: OK\n"),
            None
        );
        assert_eq!(pbfhogg_integrity("Referential integrity: fine\n"), None);
        assert!(!pbfhogg_exit_agrees(0, None));
    }

    #[test]
    fn osmium_samples_parse() {
        let w = osmium_report(OSMIUM_WAYS.as_bytes(), false).unwrap();
        assert_eq!(w.counts, [0]);
        assert!(!w.any_missing());
        let r = osmium_report(OSMIUM_RELATIONS.as_bytes(), true).unwrap();
        assert_eq!(r.counts, [0, 441, 32_943, 777]);
        assert!(r.any_missing());
    }

    #[test]
    fn an_incomplete_osmium_run_has_no_report() {
        assert_eq!(osmium_report(OSMIUM_OUT_OF_ORDER.as_bytes(), false), None);
        assert_eq!(osmium_report(OSMIUM_OUT_OF_ORDER.as_bytes(), true), None);
        assert!(!osmium_ways_only_completed(OSMIUM_OUT_OF_ORDER.as_bytes()));
        // The other mode's summary is not this mode's.
        assert_eq!(osmium_report(OSMIUM_WAYS.as_bytes(), true), None);
        assert_eq!(osmium_report(OSMIUM_RELATIONS.as_bytes(), false), None);
        // A summary cut short, or one repeated, is not a report.
        let cut = OSMIUM_RELATIONS.rsplit_once("Relations").unwrap().0;
        assert_eq!(osmium_report(cut.as_bytes(), true), None);
        let twice = format!("{OSMIUM_WAYS}{OSMIUM_WAYS}");
        assert_eq!(osmium_report(twice.as_bytes(), false), None);
        // A count that is not a plain decimal.
        let signed = OSMIUM_WAYS.replace("missing: 0", "missing: +0");
        assert_eq!(osmium_report(signed.as_bytes(), false), None);
    }

    #[test]
    fn osmium_report_ignores_earlier_unrelated_lines() {
        let warned = format!("Warning: something unrelated\n{OSMIUM_WAYS}");
        assert_eq!(osmium_report(warned.as_bytes(), false).unwrap().counts, [0]);
    }

    #[test]
    fn samples_agree_across_tools() {
        let p = pbfhogg_with_relations(&parse_pbfhogg(PBFHOGG_RELATIONS_FAILED)).unwrap();
        let o = osmium_report(OSMIUM_RELATIONS.as_bytes(), true).unwrap();
        assert_eq!(p.to_vec(), o.counts);
    }

    #[test]
    fn compare_count_verdicts() {
        assert_eq!(compare_count("x", Some(3), Some(3)), Verdict::Pass);
        assert_eq!(compare_count("x", Some(3), Some(4)), Verdict::Fail);
        assert_eq!(compare_count("x", None, Some(0)), Verdict::Fail);
    }
}
