//! Verify: check --ids - pbfhogg `check --ids` and `check --ids --full`
//! against osmium's reading of the same file and the verifier's reading of its
//! framing.
//!
//! osmium has no ID check of its own, so the expectation is derived per case:
//! `check --ids` passes exactly when the decoded objects strictly ascend by
//! kind and then ID in canonical order (`osmium fileinfo -e`: ordered and no
//! ID repeated, [`crate::verify_fileinfo`]), and, on an indexed file, the kind
//! every data blob claims never decreases, an empty blob's claim included,
//! since there pbfhogg judges type order from the claims, not the elements
//! (the header-only [`crate::pbf_frames::framing`]). `--full` adds duplicate
//! detail, which cannot change the verdict on a file without negative IDs: a
//! repeat already breaks the strict ascent. Each mode, on each case, must
//! exit as its report says (0 with a pass and no violation, 1 with a failure
//! and at least one), answer as expected, count every kind as osmium does
//! (the guard against a reader that skips objects), and say it read
//! indexdata exactly when the framing has it. The violations themselves are
//! not compared.
//!
//! Cases: the input; when osmium reads it in strict order, three copies the
//! check derives (the input stripped of indexdata when it has any, `degrade
//! --unsort` for a step down across blobs on an indexed file, and `degrade
//! --unsort-intra --strip-indexdata` for one inside a blob on an unindexed
//! file), each required to keep the input's counts and to be out of order
//! exactly when its degradation fires on those counts; and five fixed
//! fixtures, written as OPL and converted by osmium (unindexed) and then
//! rewritten by pbfhogg cat (indexed): a kind inversion and an adjacent
//! repeated ID in both forms, and an ordered file with an empty blob claiming
//! nodes appended after its way blob. Each fixture must show the property it
//! exists for, read from osmium and the framing, never inferred from how it
//! was built.
//!
//! Domain: no negative ID (pbfhogg rejects negative input IDs project-wide,
//! and `--full` reports each one as untrackable), and true kind claims: a
//! blob whose decoded kind contradicts its claim stops pbfhogg's check
//! without an answer, which is an error here.

use std::path::{Path, PathBuf};

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::{Harness, Verdict};
use crate::pbf_frames::{self, Framing};
use crate::reference::ToolRun;
use crate::verify_fileinfo::{FileInfo, fileinfo};

/// The block cap `degrade --unsort` is given, so the check knows when it
/// fires: on a kind with more objects than this.
const UNSORT_CAP: u64 = 8000;

const KINDS: [&str; 3] = ["nodes", "ways", "relations"];

/// One file the check judges, with what osmium and the framing say of it.
struct Case {
    label: String,
    path: PathBuf,
    info: FileInfo,
    framing: Framing,
}

impl Case {
    fn read(harness: &mut Harness, outdir: &Path, label: &str, path: &Path) -> Result<Self, Error> {
        let stem = label.replace(' ', "-");
        let info = fileinfo(harness, path, &outdir.join(format!("{stem}.fileinfo.json")))?;
        let framing = pbf_frames::framing(path)?;
        Ok(Self {
            label: label.to_owned(),
            path: path.to_path_buf(),
            info,
            framing,
        })
    }

    fn indexed(&self) -> bool {
        matches!(self.framing, Framing::Indexed { .. })
    }

    /// What `check --ids` must answer.
    fn expected_pass(&self) -> bool {
        self.info.strictly_ordered()
            && !matches!(
                self.framing,
                Framing::Indexed {
                    claims_in_kind_order: false
                }
            )
    }
}

/// The bounded part of `check --ids --json`.
#[derive(serde::Deserialize)]
struct Report {
    ids: Ids,
}

#[derive(serde::Deserialize)]
struct Ids {
    counts: Counts,
    indexed: bool,
    passed: bool,
    total_violations: u64,
    violations: Vec<serde::de::IgnoredAny>,
}

#[derive(serde::Deserialize)]
struct Counts {
    nodes: u64,
    ways: u64,
    relations: u64,
}

fn parse_report(reader: impl std::io::Read) -> serde_json::Result<Ids> {
    serde_json::from_reader::<_, Report>(reader).map(|r| r.ids)
}

/// Whether exit `code` agrees with the report: 0 with a pass and no
/// violation, 1 with a failure and at least one.
fn exit_agrees(code: i32, ids: &Ids) -> bool {
    match code {
        0 => ids.passed && ids.total_violations == 0 && ids.violations.is_empty(),
        1 => !ids.passed && ids.total_violations > 0 && !ids.violations.is_empty(),
        _ => false,
    }
}

/// Run both modes on `case` and record what they must agree with.
fn compare(
    harness: &mut Harness,
    outdir: &Path,
    case: &Case,
    direct_io: bool,
) -> Result<(), Error> {
    let expected = case.expected_pass();
    harness.msg(&format!(
        "--- check --ids: {} ({}, expected to {}) ---",
        case.label,
        if case.indexed() {
            "indexed"
        } else {
            "unindexed"
        },
        if expected { "pass" } else { "fail" }
    ));
    for full in [false, true] {
        let mode = if full {
            "check --ids --full"
        } else {
            "check --ids"
        };
        let label = format!("{} {mode}", case.label);
        let report = outdir.join(format!(
            "{}-{}.json",
            case.label.replace(' ', "-"),
            if full { "full" } else { "streaming" }
        ));
        let captured = harness.pbfhogg(
            PbfArgv::new("check")
                .arg("--ids")
                .arg_if(full, "--full")
                .arg("--json")
                .arg_if(direct_io, "--direct-io")
                .input(&case.path),
            &report,
        )?;
        if !matches!(captured.code(), Some(0 | 1)) {
            harness.require_success(&captured, &format!("pbfhogg {mode}"))?;
        }
        let code = captured.code().ok_or_else(|| {
            Error::Failed(format!("pbfhogg {mode} answered without an exit code"))
        })?;
        let file = std::fs::File::open(&report)
            .map_err(|e| Error::Failed(format!("cannot open {}: {e}", report.display())))?;
        let ids = match parse_report(std::io::BufReader::new(file)) {
            Ok(ids) => ids,
            Err(e) => {
                harness.msg(&format!(
                    "  {label}: report {} does not read: {e} FAIL",
                    report.display()
                ));
                harness.fail(format!("{label} wrote a report that does not read"));
                continue;
            }
        };
        let agrees = exit_agrees(code, &ids);
        harness.msg(&format!(
            "  {label}: exit {code}, passed {}, {} violations: exit {} its report {}",
            ids.passed,
            ids.total_violations,
            if agrees { "agrees with" } else { "contradicts" },
            if agrees { "PASS" } else { "FAIL" }
        ));
        harness.record(
            &format!("{label} exit agrees with its report"),
            Verdict::from_pass(agrees),
        );
        let verdict = ids.passed == expected;
        harness.msg(&format!(
            "  {label}: {} where osmium and the framing say it should {} {}",
            if ids.passed { "passed" } else { "failed" },
            if expected { "pass" } else { "fail" },
            if verdict { "PASS" } else { "FAIL" }
        ));
        harness.record(&format!("{label} verdict"), Verdict::from_pass(verdict));
        let ours = [ids.counts.nodes, ids.counts.ways, ids.counts.relations];
        for (kind, (ours, theirs)) in ours.into_iter().zip(case.info.counts).enumerate() {
            let same = ours == theirs;
            if !same {
                harness.msg(&format!(
                    "  {label}: {ours} {} where osmium counts {theirs} FAIL",
                    KINDS[kind]
                ));
            }
            harness.record(
                &format!("{label} {} count", KINDS[kind]),
                Verdict::from_pass(same),
            );
        }
        let same = ids.indexed == case.indexed();
        if !same {
            harness.msg(&format!(
                "  {label}: reports indexed {} where the framing {} indexdata FAIL",
                ids.indexed,
                if case.indexed() {
                    "carries"
                } else {
                    "carries no"
                }
            ));
        }
        harness.record(&format!("{label} indexed"), Verdict::from_pass(same));
    }
    Ok(())
}

/// A derived case: `path`, written from the input by a pbfhogg fixture
/// builder, must keep the input's counts, be framed as `indexed` says, and
/// be out of order exactly when `disordered`.
fn derived(
    harness: &mut Harness,
    outdir: &Path,
    input: &Case,
    label: &str,
    path: &Path,
    indexed: bool,
    disordered: bool,
) -> Result<Case, Error> {
    let case = Case::read(harness, outdir, label, path)?;
    let why = if case.info.counts != input.info.counts {
        Some(format!(
            "holds {:?} objects per kind where the input holds {:?}",
            case.info.counts, input.info.counts
        ))
    } else if case.indexed() != indexed {
        Some(format!(
            "is {}indexed",
            if case.indexed() { "" } else { "not " }
        ))
    } else if case.info.strictly_ordered() == disordered {
        Some(if disordered {
            "is in order although its degradation fires on these counts".to_owned()
        } else {
            "is out of order although nothing should have changed its order".to_owned()
        })
    } else {
        None
    };
    match why {
        Some(why) => Err(Error::Failed(format!(
            "derived case {label} ({}) {why}, so it cannot stand for the case it is built for",
            path.display()
        ))),
        None => Ok(case),
    }
}

/// Which of the four kinds of case real data could give (indexed or not, in
/// order or not) none of the cases from the input, `(indexed, expected
/// pass)` each, was: one claim part per kind missing. The fixtures do not
/// count: they stand for their own shapes, not for the data.
fn coverage_gaps(cases: impl Iterator<Item = (bool, bool)>) -> Vec<String> {
    let seen: Vec<(bool, bool)> = cases.collect();
    [(true, true), (false, true), (true, false), (false, false)]
        .into_iter()
        .filter(|kind| !seen.contains(kind))
        .map(|(indexed, pass)| {
            format!(
                "no {} {} case came from the input",
                if indexed { "indexed" } else { "unindexed" },
                if pass { "in-order" } else { "out-of-order" }
            )
        })
        .collect()
}

/// The fixed fixtures, as OPL. Tags are empty, metadata is a version.
const INVERSION: &str = "w10 v1 T Nn1,n2\nn1 v1 T x1 y1\nn2 v1 T x1.1 y1.1\n";
const REPEAT: &str = "n1 v1 T x1 y1\nn1 v1 T x1 y1\nw10 v1 T Nn1\n";
const ORDERED: &str = "n1 v1 T x1 y1\nn2 v1 T x1.1 y1.1\nw10 v1 T Nn1,n2\n";

/// What a fixture must show to be the case it is built for.
#[derive(Clone, Copy)]
enum Shape {
    Inversion,
    Repeat,
    EmptyClaim,
}

impl Shape {
    fn holds(self, case: &Case) -> bool {
        match self {
            Self::Inversion => !case.info.ordered,
            Self::Repeat => case.info.ordered && case.info.multiple_versions == Some(true),
            Self::EmptyClaim => {
                case.info.strictly_ordered()
                    && case.framing
                        == Framing::Indexed {
                            claims_in_kind_order: false,
                        }
            }
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Inversion => "a way before nodes",
            Self::Repeat => "an adjacent repeated ID",
            Self::EmptyClaim => "objects in order with an empty node claim after a way blob",
        }
    }
}

fn fixture_case(
    harness: &mut Harness,
    outdir: &Path,
    label: &str,
    path: &Path,
    shape: Shape,
    indexed: bool,
) -> Result<Case, Error> {
    let case = Case::read(harness, outdir, label, path)?;
    if !shape.holds(&case) || case.indexed() != indexed {
        return Err(Error::Failed(format!(
            "fixture {label} ({}) does not show {}{}, so it cannot stand for that case",
            path.display(),
            shape.name(),
            if indexed { " indexed" } else { " unindexed" }
        )));
    }
    Ok(case)
}

/// Write `opl` as `<stem>.opl`, convert it with osmium to `<stem>.osm.pbf`
/// (no indexdata), and rewrite that with pbfhogg cat to
/// `<stem>-indexed.osm.pbf`. Returns both PBFs.
fn build_fixture(
    harness: &mut Harness,
    outdir: &Path,
    stem: &str,
    opl: &str,
) -> Result<(PathBuf, PathBuf), Error> {
    let source = outdir.join(format!("{stem}.opl"));
    std::fs::write(&source, opl)?;
    let unindexed = outdir.join(format!("{stem}.osm.pbf"));
    let captured = harness.reference(
        &ToolRun::osmium("check-ids-fixture.1")
            .arg("cat")
            .input(&source)
            .arg("-o")
            .output(&unindexed)
            .arg("--overwrite"),
        &outdir.join(format!("{stem}.stdout")),
    )?;
    harness.require_success(&captured, "osmium cat (fixture)")?;
    let indexed = outdir.join(format!("{stem}-indexed.osm.pbf"));
    let captured =
        harness.pbfhogg_discard(PbfArgv::new("cat").input(&unindexed).output(&indexed))?;
    harness.require_success(&captured, "pbfhogg cat (fixture)")?;
    Ok((unindexed, indexed))
}

/// The copies of an input osmium reads in strict order: stripped of
/// indexdata (when it has any), `degrade --unsort` and `degrade
/// --unsort-intra --strip-indexdata`, each held to the shape it is built for
/// ([`derived`]). pbfhogg builds them buffered.
fn derive_cases(
    harness: &mut Harness,
    outdir: &Path,
    pbf: &Path,
    input: &Case,
) -> Result<Vec<Case>, Error> {
    harness.msg("--- check --ids: deriving cases from the input ---");
    let mut cases = Vec::new();
    if input.indexed() {
        let stripped = outdir.join("stripped.osm.pbf");
        harness.strip_indexdata(pbf, &stripped)?;
        cases.push(derived(
            harness, outdir, input, "stripped", &stripped, false, false,
        )?);
    }
    let counts = input.info.counts;
    let unsort = outdir.join("unsort.osm.pbf");
    let captured = harness.pbfhogg_discard(
        PbfArgv::new("degrade")
            .arg("--unsort")
            .arg("--elements-per-blob")
            .arg(UNSORT_CAP.to_string())
            .arg("--force")
            .input(pbf)
            .output(&unsort),
    )?;
    harness.require_success(&captured, "pbfhogg degrade --unsort")?;
    let fires = counts.iter().any(|&n| n > UNSORT_CAP);
    cases.push(derived(
        harness, outdir, input, "unsort", &unsort, true, fires,
    )?);
    let intra = outdir.join("unsort-intra.osm.pbf");
    let captured = harness.pbfhogg_discard(
        PbfArgv::new("degrade")
            .arg("--unsort-intra")
            .arg("--strip-indexdata")
            .arg("--force")
            .input(pbf)
            .output(&intra),
    )?;
    harness.require_success(
        &captured,
        "pbfhogg degrade --unsort-intra --strip-indexdata",
    )?;
    let fires = counts.iter().any(|&n| n >= 2);
    cases.push(derived(
        harness,
        outdir,
        input,
        "unsort intra",
        &intra,
        false,
        fires,
    )?);
    Ok(cases)
}

/// Run check --ids cross-validation: pbfhogg check --ids, plain and --full,
/// against osmium fileinfo and the framing, on the input, its derived
/// copies and the fixed fixtures (module doc).
pub fn run(harness: &mut Harness, pbf: &Path, direct_io: bool) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("check-ids")?;
    let mut gaps: Vec<String> = Vec::new();

    harness.msg("--- check --ids: reading the input ---");
    let input = Case::read(harness, &outdir, "input", pbf)?;
    if let Some((kind, id)) = input.info.negative_id() {
        return Err(Error::Failed(format!(
            "{} holds {} ID {id}: pbfhogg rejects negative input IDs project-wide and check \
             --ids --full reports each as untrackable, so check --ids cannot be compared on it",
            pbf.display(),
            ["node", "way", "relation"][kind]
        )));
    }

    let mut cases = vec![];
    if input.info.counts.iter().all(|&n| n == 0) {
        // Stripping or degrading a file with no object changes nothing a
        // derived case could stand for.
        gaps.push("the input holds no object, so no case was derived from it".to_owned());
    } else if input.info.strictly_ordered() {
        cases = derive_cases(harness, &outdir, pbf, &input)?;
    } else {
        harness.msg("  osmium reads the input out of strict order: no case is derived from it");
    }
    gaps.extend(coverage_gaps(
        std::iter::once(&input)
            .chain(&cases)
            .map(|case| (case.indexed(), case.expected_pass())),
    ));

    harness.msg("--- check --ids: building the fixed fixtures ---");
    let (inversion, inversion_indexed) = build_fixture(harness, &outdir, "inversion", INVERSION)?;
    let (repeat, repeat_indexed) = build_fixture(harness, &outdir, "repeat", REPEAT)?;
    let (_, empty_claim) = build_fixture(harness, &outdir, "empty-claim", ORDERED)?;
    pbf_frames::append_empty_claim(&empty_claim, 0)?;
    for (label, path, shape, indexed) in [
        ("inversion", &inversion, Shape::Inversion, false),
        (
            "inversion indexed",
            &inversion_indexed,
            Shape::Inversion,
            true,
        ),
        ("repeat", &repeat, Shape::Repeat, false),
        ("repeat indexed", &repeat_indexed, Shape::Repeat, true),
        ("empty claim", &empty_claim, Shape::EmptyClaim, true),
    ] {
        cases.push(fixture_case(harness, &outdir, label, path, shape, indexed)?);
    }

    compare(harness, &outdir, &input, direct_io)?;
    for case in &cases {
        compare(harness, &outdir, case, direct_io)?;
    }

    let mut claim = vec![
        "verdict, exit and per-kind counts against osmium fileinfo and the framing's kind claims; \
         the violations listed and --full's duplicate detail are not compared"
            .to_owned(),
    ];
    claim.extend(gaps);
    Ok(Some(claim.join("; ")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn case(ordered: bool, repeated: Option<bool>, framing: Framing) -> Case {
        Case {
            label: "x".into(),
            path: PathBuf::from("x.osm.pbf"),
            info: FileInfo {
                ordered,
                multiple_versions: repeated,
                counts: [2, 1, 0],
                min_ids: [1, 10, 0],
                max_ids: [2, 10, 0],
            },
            framing,
        }
    }

    const IN_ORDER: Framing = Framing::Indexed {
        claims_in_kind_order: true,
    };
    const OUT_OF_ORDER: Framing = Framing::Indexed {
        claims_in_kind_order: false,
    };

    #[test]
    fn the_expectation_needs_strict_order_and_ordered_claims() {
        assert!(case(true, Some(false), IN_ORDER).expected_pass());
        assert!(case(true, Some(false), Framing::Unindexed).expected_pass());
        assert!(!case(true, Some(false), OUT_OF_ORDER).expected_pass());
        assert!(!case(true, Some(true), IN_ORDER).expected_pass());
        assert!(!case(false, None, Framing::Unindexed).expected_pass());
    }

    #[test]
    fn coverage_names_each_kind_of_case_no_input_case_was() {
        let all = [(true, true), (false, true), (true, false), (false, false)];
        assert!(coverage_gaps(all.into_iter()).is_empty());
        assert_eq!(
            coverage_gaps([(false, true), (true, false), (false, false)].into_iter()),
            vec!["no indexed in-order case came from the input"]
        );
        assert_eq!(coverage_gaps([(true, false)].into_iter()).len(), 3);
    }

    #[test]
    fn each_fixture_shape_is_read_from_the_case() {
        assert!(Shape::Inversion.holds(&case(false, None, Framing::Unindexed)));
        assert!(!Shape::Inversion.holds(&case(true, Some(false), Framing::Unindexed)));
        assert!(Shape::Repeat.holds(&case(true, Some(true), IN_ORDER)));
        assert!(!Shape::Repeat.holds(&case(false, None, IN_ORDER)));
        assert!(Shape::EmptyClaim.holds(&case(true, Some(false), OUT_OF_ORDER)));
        assert!(!Shape::EmptyClaim.holds(&case(true, Some(false), IN_ORDER)));
        assert!(!Shape::EmptyClaim.holds(&case(false, None, OUT_OF_ORDER)));
    }

    const PASSED: &str = r#"{"ids":{"counts":{"nodes":2,"relations":0,"ways":1},"file":"x",
        "header_sorted":true,"indexed":true,"passed":true,"total_violations":0,"violations":[]}}"#;
    const FAILED: &str = r#"{"ids":{"counts":{"nodes":2,"relations":0,"ways":1},"file":"x",
        "header_sorted":false,"indexed":false,"passed":false,"total_violations":2,
        "violations":[{"elem_type":"node","id":1,"prev_id":1,"type":"non_monotonic"},
                      {"elem_type":"node","id":1,"type":"duplicate"}]}}"#;

    #[test]
    fn the_exit_must_agree_with_the_report() {
        let passed = parse_report(PASSED.as_bytes()).unwrap();
        let failed = parse_report(FAILED.as_bytes()).unwrap();
        assert!(exit_agrees(0, &passed));
        assert!(!exit_agrees(1, &passed));
        assert!(exit_agrees(1, &failed));
        assert!(!exit_agrees(0, &failed));
        assert!(!exit_agrees(2, &failed));
        assert_eq!(failed.violations.len(), 2);
        // A pass that counts violations, or a failure listing none, agrees
        // with no exit.
        let odd = parse_report(
            PASSED
                .replace(r#""total_violations":0"#, r#""total_violations":3"#)
                .as_bytes(),
        )
        .unwrap();
        assert!(!exit_agrees(0, &odd));
        let empty = parse_report(
            FAILED
                .replace(
                    r#"[{"elem_type":"node","id":1,"prev_id":1,"type":"non_monotonic"},
                      {"elem_type":"node","id":1,"type":"duplicate"}]"#,
                    "[]",
                )
                .as_bytes(),
        )
        .unwrap();
        assert!(!exit_agrees(1, &empty));
        assert!(parse_report(r#"{"ids":{"passed":true}}"#.as_bytes()).is_err());
    }
}
