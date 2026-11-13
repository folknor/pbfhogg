//! Verify: renumber - `pbfhogg renumber` vs `osmium renumber`.
//!
//! Cross-validates pbfhogg's renumber output against osmium. Unlike the other
//! verify subcommands, a small number of diffs is expected and treated as a
//! PASS: pbfhogg's orphan-reference handling in relation members is a
//! documented semantic deviation (see pbfhogg `DEVIATIONS.md`).
//!
//! Fail conditions:
//! - element counts diverge between osmium and pbfhogg outputs
//! - any node or way diffs appear
//! - relation diff count exceeds `0.10 * total_relations` (sanity threshold)
//!
//! Input whose header lacks `Sort.Type_then_ID` among its optional features
//! is input both tools refuse; pbfhogg's renumber is then held to its
//! documented refusal and nothing is compared (`crate::verify_input_order`).
//!
//! The run directory follows the verifier's shared retention policy.

use std::fs;
use std::path::Path;

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::Harness;
use crate::reference::ToolRun;

/// Summary counts parsed from the `Summary: left=... right=... same=... different=...`
/// line produced by `pbfhogg diff -s`.
#[derive(Debug, Default)]
struct DiffSummary {
    left: u64,
    right: u64,
    same: u64,
    different: u64,
}

/// Per-element-type diff-block counts, derived from the `*n<id>` / `*w<id>` /
/// `*r<id>` block headers in `pbfhogg diff -c -v` output.
#[derive(Debug, Default)]
struct TypeCounts {
    nodes: u64,
    ways: u64,
    relations: u64,
}

/// Parse the `Summary:` line emitted by `pbfhogg diff -s`.
///
/// Example: `Summary: left=59152282 right=59152282 same=59151976 different=306`.
fn parse_summary(text: &str) -> Option<DiffSummary> {
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("Summary:"))?;
    let (mut left, mut right, mut same, mut different) = (None, None, None, None);
    for field in line.split_whitespace() {
        if let Some(v) = field.strip_prefix("left=") {
            left = Some(v.parse().ok()?);
        } else if let Some(v) = field.strip_prefix("right=") {
            right = Some(v.parse().ok()?);
        } else if let Some(v) = field.strip_prefix("same=") {
            same = Some(v.parse().ok()?);
        } else if let Some(v) = field.strip_prefix("different=") {
            different = Some(v.parse().ok()?);
        }
    }
    // All four or nothing: a field missing from a partial summary must not
    // read as zero.
    Some(DiffSummary {
        left: left?,
        right: right?,
        same: same?,
        different: different?,
    })
}

/// Elements present on one side only: `left` and `right` each count `same +
/// different` plus their one-sided elements, which a renumber of the same
/// input by both tools must not produce.
fn one_sided(s: &DiffSummary) -> Option<(u64, u64)> {
    let paired = s.same.checked_add(s.different)?;
    let left_only = s.left.checked_sub(paired)?;
    let right_only = s.right.checked_sub(paired)?;
    Some((left_only, right_only))
}

/// Bytes of a line [`block_kind`] needs: `*`, the kind, an optional `-`, a
/// digit.
const BLOCK_HEAD: usize = 4;

/// The element kind of a diff block header line, or `None` for any other
/// line.
///
/// Block headers look like `*n<id>`, `*w<id>`, `*r<id>` at the start of a
/// line (the `*` marks a changed element), the ID possibly negative. Lines
/// that don't match are context, member lists, or tags and are ignored.
fn block_kind(head: &[u8]) -> Option<u8> {
    let (&star, rest) = head.split_first()?;
    let (&kind, rest) = rest.split_first()?;
    let rest = rest.strip_prefix(b"-").unwrap_or(rest);
    let digit = rest.first().is_some_and(u8::is_ascii_digit);
    (star == b'*' && digit && matches!(kind, b'n' | b'w' | b'r')).then_some(kind)
}

fn count_block(c: &mut TypeCounts, head: &[u8]) {
    match block_kind(head) {
        Some(b'n') => c.nodes += 1,
        Some(b'w') => c.ways += 1,
        Some(b'r') => c.relations += 1,
        _ => {}
    }
}

/// Count block headers per kind in the diff listing file, reading only the
/// first few bytes of each line through a fixed buffer.
fn categorize_listing(listing: &Path) -> Result<TypeCounts, Error> {
    let mut c = TypeCounts::default();
    crate::stream::for_each_line_head(listing, BLOCK_HEAD, |head| {
        count_block(&mut c, head);
    })?;
    Ok(c)
}

#[cfg(test)]
fn categorize_blocks(detail: &str) -> TypeCounts {
    let mut c = TypeCounts::default();
    for line in detail.lines() {
        let head = &line.as_bytes()[..line.len().min(BLOCK_HEAD)];
        count_block(&mut c, head);
    }
    c
}

/// Format an integer with comma thousand separators.
fn fmt_num(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Format a byte count as a short human-readable string.
fn fmt_size(bytes: u64) -> String {
    const KB: u64 = 1_000;
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;
    #[allow(clippy::cast_precision_loss)]
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Cross-validate `pbfhogg renumber` against `osmium renumber`.
///
/// `direct_io` reaches the renumber run only; the `pbfhogg diff` that
/// compares its output with osmium's is a comparator and reads buffered.
#[allow(clippy::too_many_lines)]
pub fn run(
    harness: &mut Harness,
    pbf: &Path,
    dataset: &str,
    start_id: Option<&str>,
    verbose: bool,
    direct_io: bool,
) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("renumber")?;

    harness.msg("--- renumber cross-validation ---");
    harness.msg(&format!("  dataset: {dataset}"));
    harness.msg(&format!("  input: {}", pbf.display()));
    if let Some(sid) = start_id {
        harness.msg(&format!("  start-id: {sid}"));
    }

    // --- Output paths ---------------------------------------------------
    let osmium_out = outdir.join(format!("osmium-renumber-{dataset}.osm.pbf"));
    let pbfhogg_out = outdir.join(format!("pbfhogg-renumber-{dataset}.osm.pbf"));
    let diff_log = outdir.join(format!("verify-renumber-{dataset}-diff.txt"));

    let mut pbfhogg_args = PbfArgv::new("renumber")
        .input(pbf)
        .output(&pbfhogg_out)
        .arg_if(direct_io, "--direct-io");
    if let Some(sid) = start_id {
        pbfhogg_args = pbfhogg_args.arg("--start-id").arg(sid);
    }

    // Input without the sorted claim is input renumber refuses by contract,
    // and osmium renumber requires sorted input: pbfhogg's run is held to
    // that refusal and osmium does not run (`verify_input_order`).
    if !crate::verify_input_order::input_header(harness, pbf, &outdir)?.optional_sorted() {
        let claim = Some(crate::verify_input_order::refusal_claim(
            "pbfhogg renumber",
            "",
            direct_io,
        ));
        crate::verify_input_order::expect_unsorted_refusal(
            harness,
            pbfhogg_args,
            &crate::verify_input_order::Refusal {
                label: "renumber",
                operand: pbf,
                answers: &[0],
                sink: None,
            },
        )?;
        return Ok(claim);
    }

    // --- osmium renumber ------------------------------------------------
    let mut osmium_run = ToolRun::osmium("renumber.1")
        .arg("renumber")
        .input(pbf)
        .arg("-o")
        .output(&osmium_out)
        .arg("--overwrite");
    if let Some(sid) = start_id {
        osmium_run = osmium_run.args(&["--start-id", sid]);
    }
    let captured = harness.reference(&osmium_run, &outdir.join("osmium-renumber.stdout"))?;
    harness.require_success(&captured, "osmium renumber")?;

    let osmium_size = fs::metadata(&osmium_out).map(|m| m.len()).unwrap_or(0);
    harness.msg(&format!(
        "  osmium renumber: {} output",
        fmt_size(osmium_size),
    ));

    // --- pbfhogg renumber -----------------------------------------------
    let captured = harness.pbfhogg_discard(pbfhogg_args)?;
    harness.require_success(&captured, "pbfhogg renumber")?;

    let pbfhogg_size = fs::metadata(&pbfhogg_out).map(|m| m.len()).unwrap_or(0);
    harness.msg(&format!(
        "  pbfhogg renumber: {} output",
        fmt_size(pbfhogg_size),
    ));

    // --- pbfhogg diff (summary + context + verbose) ---------------------
    // The listing goes to a file: with -v it prints every differing
    // element's members, which outgrew any in-memory capture at germany
    // scale. Exit 1 means "differences", 0 "identical"; anything else (a
    // signal, a refusal) means the listing and summary cannot be trusted.
    // A text diff writes no scratch and refuses `--tmp-dir`.
    let captured = harness.pbfhogg(
        PbfArgv::new("diff")
            // The one short brokkr forwards: stable across the pbfhogg
            // versions that have the option, whose long spelling changed
            // from `--summary` to `--osmium-summary` while `-s` kept its
            // meaning.
            .arg("-s")
            .arg("--suppress-common")
            .arg("--verbose")
            .input(&osmium_out)
            .input(&pbfhogg_out),
        &diff_log,
    )?;
    harness.require_codes(&captured, "pbfhogg diff", &[0, 1])?;
    let stderr = String::from_utf8_lossy(&captured.stderr).into_owned();
    // pbfhogg's -s summary is written to stderr.
    let summary = parse_summary(&stderr).ok_or_else(|| {
        Error::Failed(
            "renumber: could not parse a complete 'Summary:' line from pbfhogg diff stderr".into(),
        )
    })?;

    // --- Total relation count (for threshold) ---------------------------
    // An unreadable report is a hard error rather than a silent fallback:
    // without total_relations the sanity-threshold check is effectively
    // disabled, which would let arbitrarily large relation-only regressions
    // sneak through as PASS. The JSON reader refuses a missing count.
    let total_relations = crate::inspect_report::inspect(harness, &osmium_out, false)?
        .counts()
        .relations;

    // --- Report ---------------------------------------------------------
    let total = summary.left.max(summary.right);
    #[allow(clippy::cast_precision_loss)]
    let pct_same = if total > 0 {
        (summary.same as f64 / total as f64) * 100.0
    } else {
        0.0
    };
    #[allow(clippy::cast_precision_loss)]
    let pct_diff = if total > 0 {
        (summary.different as f64 / total as f64) * 100.0
    } else {
        0.0
    };

    if summary.left == summary.right {
        harness.msg(&format!(
            "  element counts: {} total on both sides (match)",
            fmt_num(summary.left),
        ));
    } else {
        harness.msg(&format!(
            "  element counts: osmium={} pbfhogg={} (MISMATCH)",
            fmt_num(summary.left),
            fmt_num(summary.right),
        ));
    }
    harness.msg(&format!(
        "  same: {} ({pct_same:.5}%)",
        fmt_num(summary.same),
    ));
    harness.msg(&format!(
        "  different: {} ({pct_diff:.5}%)",
        fmt_num(summary.different),
    ));

    // --- Categorize diff blocks by element type -------------------------
    let counts = categorize_listing(&diff_log)?;
    if summary.different > 0 {
        harness.msg(&format!(
            "    diff blocks: {} node(s), {} way(s), {} relation(s)",
            counts.nodes, counts.ways, counts.relations,
        ));
    }

    // --- Pass/fail decision ---------------------------------------------
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let threshold = ((total_relations as f64) * 0.10) as u64;

    let fail_reason: Option<String> = if summary.left != summary.right {
        Some(format!(
            "element counts diverge (osmium={}, pbfhogg={})",
            summary.left, summary.right,
        ))
    } else if one_sided(&summary) != Some((0, 0)) {
        Some(format!(
            "summary does not pair every element (left={} right={} same={} different={})",
            summary.left, summary.right, summary.same, summary.different,
        ))
    } else if counts.nodes + counts.ways + counts.relations != summary.different {
        // pbfhogg writes one block header per modified element, and the
        // check above rules out created and deleted ones, so every
        // difference must have been classified by kind; a gap means the
        // listing or its header format is not what this check reads.
        Some(format!(
            "classified {} diff block(s) but the summary reports {} difference(s)",
            counts.nodes + counts.ways + counts.relations,
            summary.different,
        ))
    } else if counts.nodes > 0 || counts.ways > 0 {
        Some(format!(
            "node/way diffs detected (nodes={}, ways={})",
            counts.nodes, counts.ways,
        ))
    } else if summary.different > threshold {
        Some(format!(
            "relation diffs {} exceed sanity threshold {} (10% of {} relations)",
            summary.different, threshold, total_relations,
        ))
    } else {
        None
    };

    if verbose && summary.different > 0 {
        harness.msg(&format!("  diff log: {}", diff_log.display()));
        let (lines, _) = crate::stream::preview(
            &diff_log,
            crate::tunables::PREVIEW_LINES,
            crate::tunables::PREVIEW_LINE_BYTES,
        )?;
        for line in lines {
            harness.msg(&format!("    {line}"));
        }
    }

    match fail_reason {
        Some(reason) => {
            harness.msg(&format!("  result: FAIL ({reason})"));
            harness.msg(&format!("  diff log preserved: {}", diff_log.display()));
            Err(Error::Failed(format!("renumber: {reason}")))
        }
        None => {
            if summary.different == 0 {
                harness.msg("  result: PASS (identical)");
            } else {
                harness.msg("  result: PASS (within expected delta)");
                harness.msg(&format!(
                    "    -> {} relation-member diff(s) attributed to orphan-reference handling",
                    summary.different,
                ));
                harness.msg("    -> documented deviation (see pbfhogg DEVIATIONS.md)");
            }
            Ok(Some("node and way content identical; relation differences within 10% tolerance; orphan handling attribution not independently proved".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::too_many_arguments,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::approx_constant,
        clippy::needless_pass_by_value,
        clippy::let_underscore_must_use,
        clippy::useless_vec
    )]
    use super::*;

    #[test]
    fn parse_summary_standard() {
        let text = "some noise\nSummary: left=59152282 right=59152282 same=59151976 different=306\ntrailing\n";
        let s = parse_summary(text).unwrap();
        assert_eq!(s.left, 59_152_282);
        assert_eq!(s.right, 59_152_282);
        assert_eq!(s.same, 59_151_976);
        assert_eq!(s.different, 306);
    }

    #[test]
    fn parse_summary_indented() {
        let text = "  Summary: left=1 right=2 same=3 different=4";
        let s = parse_summary(text).unwrap();
        assert_eq!(s.left, 1);
        assert_eq!(s.right, 2);
        assert_eq!(s.same, 3);
        assert_eq!(s.different, 4);
    }

    #[test]
    fn parse_summary_missing() {
        assert!(parse_summary("no summary here\n").is_none());
    }

    #[test]
    fn parse_summary_refuses_a_partial_line() {
        // A field missing from a cut-short summary must not read as zero.
        assert!(parse_summary("Summary: left=5 right=5 same=5").is_none());
    }

    #[test]
    fn one_sided_elements_are_found_from_the_summary() {
        let s = parse_summary("Summary: left=10 right=10 same=7 different=3").unwrap();
        assert_eq!(one_sided(&s), Some((0, 0)));
        // Equal totals can still hide one element on each side.
        let s = parse_summary("Summary: left=10 right=10 same=7 different=2").unwrap();
        assert_eq!(one_sided(&s), Some((1, 1)));
        let s = parse_summary("Summary: left=1 right=1 same=7 different=2").unwrap();
        assert_eq!(one_sided(&s), None);
    }

    #[test]
    fn categorize_listing_reads_signed_headers_from_a_file() {
        let dir = pbfhogg_test_scratch::scratch();
        let listing = dir.path().join("diff.txt");
        let long_member_line = format!("  -member {}\n", "w1 ".repeat(10_000));
        std::fs::write(
            &listing,
            format!("*r12\n{long_member_line}*r-3\n*n-\n*w9 tags\n"),
        )
        .unwrap();
        let c = categorize_listing(&listing).unwrap();
        assert_eq!((c.nodes, c.ways, c.relations), (0, 1, 2));
    }

    #[test]
    fn categorize_blocks_mixed() {
        let detail = "\
*n123 tag diff
  -v=a
  +v=b
*w456
  -member n1
  +member n2
*r789
  -member w10000000
  +member w20
*r790
  -member w10000001
  +member w21
";
        let c = categorize_blocks(detail);
        assert_eq!(c.nodes, 1);
        assert_eq!(c.ways, 1);
        assert_eq!(c.relations, 2);
    }

    #[test]
    fn categorize_blocks_ignores_non_headers() {
        let detail = "*not-a-header\n*n foo\n*nabc\nregular line\n";
        // "*not-a-header" - type char is 'n', next char is 'o' (not digit) -> skipped.
        // "*n foo" - next char is ' ' -> skipped.
        // "*nabc" - next char is 'a' -> skipped.
        let c = categorize_blocks(detail);
        assert_eq!(c.nodes, 0);
        assert_eq!(c.ways, 0);
        assert_eq!(c.relations, 0);
    }

    #[test]
    fn fmt_num_thousands() {
        assert_eq!(fmt_num(0), "0");
        assert_eq!(fmt_num(123), "123");
        assert_eq!(fmt_num(1234), "1,234");
        assert_eq!(fmt_num(59_152_282), "59,152,282");
    }

    #[test]
    fn fmt_size_units() {
        assert_eq!(fmt_size(500), "500 B");
        // Decimal units, matching the sizes the other tools print.
        assert_eq!(fmt_size(2_500), "2 KB");
        assert_eq!(fmt_size(5_500_000), "5 MB");
        assert_eq!(fmt_size(3_000_000_000), "3.0 GB");
        assert_eq!(fmt_size(3 * 1024 * 1024 * 1024), "3.2 GB");
    }
}
