//! Input order: the header claim pbfhogg's commands branch on, the
//! osmium-sorted copy a check reads beside it, and the refusal a command that
//! rejects claimless input is held to.
//!
//! pbfhogg reads `Sort.Type_then_ID` from the header's optional features
//! (`HeaderBlock::is_sorted`), and so does every check here. On input without
//! it a check takes one of two routes, by what the command it verifies
//! promises for such input:
//!
//! - A command that takes unsorted input (cat, extract, multi-extract,
//!   tags-filter, getid, getparents, add-locations-to-ways with the sparse
//!   index, check --refs) reads it as it is; every osmium run reads an osmium-sorted copy,
//!   and pbfhogg's outputs are compared through osmium-sorted copies
//!   ([`InputOrder`], [`compare_outputs`]).
//! - A command whose documented contract refuses it (apply-changes, diff in
//!   both formats, renumber, add-locations-to-ways with the external index) is
//!   held to that refusal on the input itself ([`expect_unsorted_refusal`]):
//!   exit 2, the advice pbfhogg's CLI gives every `Unsorted` refusal naming
//!   the claimless operand as the argv spelled it, and nothing published. No
//!   reference comparison is made. Which of pbfhogg's order checks raised the
//!   refusal (the header gate, or a later one finding the content out of
//!   order) is not established: the property held is that such input is
//!   refused rather than accepted, which pbfhogg's general apply-changes path
//!   once did, silently dropping creates.
//!
//! A header that claims the flag over disordered content takes the ordinary
//! path in every check, where a refusal of it is an error.

#![allow(
    dead_code,
    reason = "shared verifier helpers consumed by the remaining family slices"
)]

use std::path::{Path, PathBuf};

use super::inspect_report::{self, InspectSummary};
use crate::argv::PbfArgv;
use crate::checks::sort;
use crate::error::Error;
use crate::harness::{Harness, Verdict};
use crate::inspect_report::SORT_TYPE_THEN_ID;

/// The input header's report, written to the check's directory, never beside
/// the input.
pub(crate) fn input_header(
    harness: &mut Harness,
    pbf: &Path,
    outdir: &Path,
) -> Result<InspectSummary, Error> {
    inspect_report::inspect_into(harness, pbf, &outdir.join("input.inspect.json"), false)
}

/// An osmium-sorted copy of `pbf` in `outdir/osmium-sorted-input/`. The
/// sort's artifact name and argv are verify sort's, so one cached run of the
/// same input serves every check.
pub(crate) fn osmium_sorted_copy(
    harness: &mut Harness,
    pbf: &Path,
    outdir: &Path,
) -> Result<PathBuf, Error> {
    let dir = outdir.join("osmium-sorted-input");
    std::fs::create_dir_all(&dir)?;
    let copy = dir.join(sort::OSMIUM_SORTED);
    sort::osmium_sort(harness, pbf, &copy, &dir.join("osmium.sink"))?;
    Ok(copy)
}

/// Where the check's osmium runs read the input, and whether pbfhogg reads
/// it as sorted.
pub(crate) struct InputOrder {
    pub sorted: bool,
    /// The input itself when sorted, else an osmium-sorted copy of it.
    pub reference_input: PathBuf,
}

impl InputOrder {
    /// Read the input header's claim (its report in the check's directory)
    /// and, on unsorted input, write the osmium-sorted copy.
    pub(crate) fn of(harness: &mut Harness, pbf: &Path, outdir: &Path) -> Result<Self, Error> {
        let sorted = input_header(harness, pbf, outdir)?.optional_sorted();
        Self::from_claim(harness, pbf, outdir, sorted)
    }

    /// [`of`](Self::of) for a check that has already read the claim, as
    /// one that branches on it before any reference runs.
    pub(crate) fn from_claim(
        harness: &mut Harness,
        pbf: &Path,
        outdir: &Path,
        sorted: bool,
    ) -> Result<Self, Error> {
        if sorted {
            return Ok(Self {
                sorted,
                reference_input: pbf.to_path_buf(),
            });
        }
        harness.msg(&format!(
            "  input header does not declare {SORT_TYPE_THEN_ID} as an optional feature: osmium \
             reads an osmium-sorted copy, pbfhogg the input as it is"
        ));
        Ok(Self {
            sorted,
            reference_input: osmium_sorted_copy(harness, pbf, outdir)?,
        })
    }
}

/// Compare pbfhogg's output `ours` of `pbfhogg <label>` with osmium's
/// `theirs` of `<osmium_label>`: osmium's must be in strict order or the
/// comparison cannot be made (an error); pbfhogg's must be in strict order
/// on sorted input, and on unsorted input must not claim `Sort.Type_then_ID`
/// and is compared through an osmium-sorted copy whose strict order proves
/// no repeated ID; then the element diff (content and tags; metadata left
/// out), and on sorted input the sort feature comparison.
pub(crate) fn compare_outputs(
    harness: &mut Harness,
    order: &InputOrder,
    label: &str,
    osmium_label: &str,
    ours: &Path,
    theirs: &Path,
) -> Result<(), Error> {
    if !harness.check_sorted(osmium_label, theirs)?.is_pass() {
        return Err(Error::Failed(format!(
            "{osmium_label} output is not in strict order, so it cannot be compared with \
             pbfhogg's by ID"
        )));
    }
    let pbfhogg_label = format!("pbfhogg {label}");
    let compared = if order.sorted {
        let verdict = harness.check_sorted(&pbfhogg_label, ours)?;
        harness.record(&format!("{pbfhogg_label} output order"), verdict);
        ours.to_path_buf()
    } else {
        let claims = harness.has_sort_flag(ours)?;
        harness.msg(&format!(
            "  {pbfhogg_label}: header {} {SORT_TYPE_THEN_ID} on unsorted input {}",
            if claims { "claims" } else { "does not claim" },
            if claims { "FAIL" } else { "PASS" }
        ));
        harness.record(
            &format!("{pbfhogg_label} output claims {SORT_TYPE_THEN_ID} on unsorted input"),
            Verdict::from_pass(!claims),
        );
        let copy = ours.with_extension("sorted.pbf");
        sort::osmium_sort(harness, ours, &copy, &ours.with_extension("sorted.sink"))?;
        let verdict = harness.check_sorted(&format!("{pbfhogg_label} (osmium-sorted)"), &copy)?;
        harness.record(&format!("{pbfhogg_label} output repeats an ID"), verdict);
        copy
    };
    let diff = harness.diff_pbfs(
        &compared,
        theirs,
        &crate::harness::listing_beside(&compared),
    )?;
    if diff.is_pass() {
        harness.msg(&format!("  {label}: PASS (identical)"));
    } else {
        harness.msg(&format!("  {label}: FAIL (differences found)"));
    }
    harness.record(&format!("{label} diff"), diff);
    if order.sorted {
        let feature = harness.compare_sort_feature(ours, theirs)?;
        harness.record(&format!("{label} sort feature"), feature);
    }
    Ok(())
}

/// The claim part a passing check on unsorted input carries when every
/// pbfhogg output it judges went through [`compare_outputs`].
pub(crate) const CLAIM_UNSORTED: &str = "on unsorted input, against osmium on an osmium-sorted \
                                         copy, pbfhogg's outputs compared through \
                                         osmium-sorted copies";

/// What a passing refusal check establishes: `command` refused the input,
/// `runs` saying how it ran when not once on the input alone. Under
/// `--direct-io` a refusal proves no O_DIRECT read either.
pub(crate) fn refusal_claim(command: &str, runs: &str, direct_io: bool) -> String {
    format!(
        "on input without {SORT_TYPE_THEN_ID} among its optional features, refusal only, \
         nothing compared with osmium: {}",
        refusal_terms(command, runs, direct_io)
    )
}

/// What one held refusal establishes, for a claim that also states other
/// work: [`refusal_claim`] without its opening.
pub(crate) fn refusal_terms(command: &str, runs: &str, direct_io: bool) -> String {
    format!(
        "{command} refused it as Unsorted, naming it, and published nothing{runs}; which of \
         pbfhogg's order checks refused is not established{}",
        if direct_io {
            ", nor that any O_DIRECT read ran"
        } else {
            ""
        }
    )
}

/// The osmium-sorted copy a two-sided diff refusal pairs the input with,
/// required to carry the claim pbfhogg reads (from `pbfhogg inspect`, not the
/// command under test), or the other side's gate cannot be held.
pub(crate) fn sorted_counterpart(
    harness: &mut Harness,
    pbf: &Path,
    outdir: &Path,
) -> Result<PathBuf, Error> {
    let copy = osmium_sorted_copy(harness, pbf, outdir)?;
    if !inspect_report::inspect(harness, &copy, false)?.optional_sorted() {
        return Err(Error::Failed(format!(
            "the osmium-sorted copy {} does not declare {SORT_TYPE_THEN_ID} among its optional \
             features, so it cannot stand as the sorted side of a refused diff",
            copy.display()
        )));
    }
    Ok(copy)
}

/// One run expected to refuse its claimless operand.
pub(crate) struct Refusal<'a> {
    /// The run's name in findings, after `pbfhogg `.
    pub label: &'a str,
    /// The operand without the claim. The advice must name it as the argv
    /// spelled it.
    pub operand: &'a Path,
    /// The exits by which the command answers, so accepted the input: 0, and
    /// 1 for a diff that found differences.
    pub answers: &'static [i32],
    /// Where a text diff's sink goes; it must stay empty.
    pub sink: Option<&'a Path>,
}

/// Run `argv` and hold it to pbfhogg's documented refusal of `refusal`'s
/// operand: an answering exit is a failed finding (it accepted input it
/// refuses), and so is an output under any name the argv writes, or a
/// listing on a text diff's sink, whatever the run's ending; exit 2 must
/// carry exactly one `Unsorted` advice, naming the operand (one naming
/// another file is a failed finding). Exit 2 without that advice, any other
/// exit, a signal, or a failed capture is an error: the run failed for
/// another reason. Publication is checked before any such error is raised,
/// so a published output stays on record.
pub(crate) fn expect_unsorted_refusal(
    harness: &mut Harness,
    argv: PbfArgv,
    refusal: &Refusal<'_>,
) -> Result<(), Error> {
    let label = format!("pbfhogg {}", refusal.label);
    let operand = refusal
        .operand
        .to_str()
        .ok_or_else(|| Error::tool("non-UTF-8 refusal operand"))?;
    let outputs = argv.writes().to_vec();
    harness.msg(&format!("--- {label} (expected to refuse {operand}) ---"));
    let run = match refusal.sink {
        Some(sink) => harness.pbfhogg(argv, sink),
        None => harness.pbfhogg_discard(argv),
    };
    if matches!(run, Err(Error::Interrupted)) {
        return Err(Error::Interrupted);
    }
    for output in &outputs {
        if exists(output)? {
            harness.msg(&format!("  {label}: published {} FAIL", output.display()));
            harness.fail(format!(
                "{label} published {} on input it refuses",
                output.display()
            ));
        }
    }
    if let Some(sink) = refusal.sink {
        match std::fs::metadata(sink) {
            Ok(meta) if meta.len() > 0 => {
                harness.msg(&format!(
                    "  {label}: wrote a listing to {} FAIL",
                    sink.display()
                ));
                harness.fail(format!("{label} wrote a listing on input it refuses"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let captured = run?;
    match captured.code() {
        Some(code) if refusal.answers.contains(&code) => {
            harness.msg(&format!("  {label}: exit {code}, accepted the input FAIL"));
            harness.fail(format!(
                "{label} accepted input without {SORT_TYPE_THEN_ID} (exit {code}), which it \
                 refuses by contract"
            ));
            Ok(())
        }
        Some(2) => {
            let stderr = String::from_utf8_lossy(&captured.stderr);
            match advice_operands(&stderr).as_slice() {
                [named] if *named == operand => {
                    harness.msg(&format!("  {label}: refused as Unsorted, naming it PASS"));
                    Ok(())
                }
                [named] => {
                    harness.msg(&format!(
                        "  {label}: refused as Unsorted naming {named}, not {operand} FAIL"
                    ));
                    harness.fail(format!(
                        "{label} refused naming {named}, not the input without the claim, \
                         {operand}"
                    ));
                    Ok(())
                }
                [] => Err(Error::Failed(format!(
                    "{label} exited 2 without the advice pbfhogg gives every Unsorted refusal, \
                     so it failed for another reason: {stderr}"
                ))),
                _ => Err(Error::Failed(format!(
                    "{label} exited 2 with several Unsorted advice commands, naming no one \
                     operand: {stderr}"
                ))),
            }
        }
        _ => match harness.require_success(&captured, &label) {
            Err(e) => Err(e),
            Ok(()) => Err(Error::Failed(format!(
                "{label} exited 0, which {} does not count as an answer",
                refusal.label
            ))),
        },
    }
}

/// Whether anything, a dangling link included, is at `path`. Only
/// `NotFound` proves absence.
fn exists(path: &Path) -> Result<bool, Error> {
    match path.symlink_metadata() {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// The advice command pbfhogg's CLI appends to every `Unsorted` refusal,
/// around the refused input as a shell word.
const ADVICE_HEAD: &str = "pbfhogg sort --distrust-metadata ";
const ADVICE_TAIL: &str = " -o sorted.osm.pbf";

/// The operand of every `Unsorted` advice command in `stderr`, decoded.
///
/// pbfhogg writes the operand plain when every character is ASCII
/// alphanumeric or one of `._-/+:`, else in single quotes with each `'`
/// written `'\''` (its `shell_word`); a quoted word may hold a newline, so the
/// command is read across lines. An occurrence that does not parse to the
/// whole command is not advice.
fn advice_operands(stderr: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = stderr;
    while let Some(at) = rest.find(ADVICE_HEAD) {
        let after = &rest[at + ADVICE_HEAD.len()..];
        if let Some((word, tail)) = shell_word(after)
            && tail.starts_with(ADVICE_TAIL)
        {
            found.push(word);
        }
        rest = after;
    }
    found
}

/// Decode one shell word as pbfhogg writes it, returning it and what follows.
fn shell_word(text: &str) -> Option<(String, &str)> {
    let Some(mut quoted) = text.strip_prefix('\'') else {
        let end = text
            .find(|c: char| {
                !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | '+' | ':'))
            })
            .unwrap_or(text.len());
        return (end > 0).then(|| (text[..end].to_owned(), &text[end..]));
    };
    let mut word = String::new();
    loop {
        let close = quoted.find('\'')?;
        word.push_str(&quoted[..close]);
        let after = &quoted[close + 1..];
        match after.strip_prefix("\\''") {
            Some(next) => {
                word.push('\'');
                quoted = next;
            }
            None => return Some((word, after)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{advice_operands, refusal_claim};

    fn advice(word: &str) -> String {
        format!(
            "Error: apply-changes requires a sorted base PBF (Sort.Type_then_ID). (in x)\n\nSort \
             the input first, decoding every blob rather than believing the header's sort claim \
             and the blobs' indexdata:\n\n  pbfhogg sort --distrust-metadata {word} -o \
             sorted.osm.pbf\n"
        )
    }

    #[test]
    fn a_plain_operand_is_read_whole() {
        assert_eq!(
            advice_operands(&advice("/data/denmark-unsorted.osm.pbf")),
            ["/data/denmark-unsorted.osm.pbf"]
        );
    }

    #[test]
    fn a_quoted_operand_is_decoded_across_quotes_and_lines() {
        assert_eq!(advice_operands(&advice("'a b.pbf'")), ["a b.pbf"]);
        assert_eq!(advice_operands(&advice("'it'\\''s.pbf'")), ["it's.pbf"]);
        assert_eq!(
            advice_operands(&advice("'two\nlines.pbf'")),
            ["two\nlines.pbf"]
        );
    }

    #[test]
    fn text_that_is_not_the_whole_command_is_not_advice() {
        assert!(advice_operands("Error: unreadable input\n").is_empty());
        assert!(advice_operands("pbfhogg sort --distrust-metadata x.pbf -o other.pbf").is_empty());
        assert!(
            advice_operands("pbfhogg sort --distrust-metadata 'open -o sorted.osm.pbf").is_empty()
        );
        assert!(advice_operands("pbfhogg sort --distrust-metadata  -o sorted.osm.pbf").is_empty());
    }

    #[test]
    fn every_advice_is_returned() {
        let twice = format!("{}{}", advice("a.pbf"), advice("b.pbf"));
        assert_eq!(advice_operands(&twice), ["a.pbf", "b.pbf"]);
    }

    #[test]
    fn the_claim_names_what_direct_io_does_not_prove() {
        assert!(!refusal_claim("pbfhogg x", "", false).contains("O_DIRECT"));
        assert!(refusal_claim("pbfhogg x", "", true).contains("O_DIRECT"));
        assert!(refusal_claim("pbfhogg x", ", twice", false).contains(
            "pbfhogg x refused it as Unsorted, naming it, and published nothing, twice;"
        ));
    }
}
