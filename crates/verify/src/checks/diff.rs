//! Verify: diff - pbfhogg's text diff, listing and detail, held to the
//! content osmium reads from the same pair.
//!
//! The pair is the input and pbfhogg `apply-changes` of it with the
//! dataset's OSC. pbfhogg's `diff --suppress-common` runs four times, the
//! command under test at every combination of indexed or stripped inputs and
//! with or without `--verbose`. The indexed verbose listing is judged record
//! by record against osmium's content; the stripped verbose listing must
//! equal it byte for byte; each plain listing must equal its header lines;
//! every run's summary must carry the expected class and common counts and
//! its exit status must say whether anything differs.
//!
//! The expected listing comes from osmium, never from pbfhogg. `osmium diff
//! --suppress-common --output-format opl` lists, with full content, every
//! object one side lacks and every pair whose version, timestamp or osmium
//! checksum differs. osmium's checksum feeds tags without lengths, so a pair
//! whose content differs can still be "same" to it (`a=bc` against `ab=c`,
//! or a replayed equal-version change that reorders tags). Every identity
//! the input OSC touches, and every one pbfhogg reports modified, that the
//! osmium listing lacks is therefore fetched from both inputs with `osmium
//! getid --output-format opl` and judged on its content too. pbfhogg's content
//! is compared as its `crates/lib/src/content.rs` defines it (location, tags, refs,
//! members, in order; metadata never), so a version or metadata change alone
//! is no difference.
//!
//! Domain assumption, not proved here: `apply-changes` preserves the diff
//! content of every identity the OSC does not touch. An untouched object the
//! builder changed while keeping osmium's checksum, and that pbfhogg's diff
//! then wrongly calls common, would pass; `verify merge` is the check that
//! holds `apply-changes` to osmium. Detail judgement limits are in
//! [`crate::verify_diff_detail`].
//!
//! Input whose header lacks `Sort.Type_then_ID` among its optional features
//! is input pbfhogg diff refuses on either side; the check then holds it to
//! that refusal and compares nothing (`refused`).

use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write as _};
use std::path::{Path, PathBuf};

use crate::argv::PbfArgv;
use crate::diff_summary::{Counts, parse_osmium_summary, parse_pbfhogg_summary};
use crate::error::Error;
use crate::harness::{Harness, Verdict};
use crate::osc::OscKind;
use crate::reference::ToolRun;
use crate::stream;
use crate::verify_diff_detail::{self as detail, Coverage, Expected};
use crate::verify_diff_opl::{DiffGroups, Object, ObjectReader, kind_char, order_key};
use crate::verify_input_order;

/// Listing mismatches printed in full before the rest are only counted.
use crate::tunables::DIFF_SHOWN_MISMATCHES as SHOWN_MISMATCHES;

/// `pbfhogg diff -c OLD NEW [-v]`, a text diff to stdout. It writes no
/// scratch, and pbfhogg refuses `--tmp-dir` on it. pbfhogg takes
/// `--direct-io` on indexed and unindexed inputs alike.
fn text_diff(old: &Path, new: &Path, verbose: bool, direct_io: bool) -> PbfArgv {
    PbfArgv::new("diff")
        .arg("--suppress-common")
        .arg_if(verbose, "--verbose")
        .input(old)
        .input(new)
        .arg_if(direct_io, "--direct-io")
}

/// One pbfhogg diff run: where its listing went, its exit code and the
/// summary its stderr carried.
struct DiffRun {
    label: &'static str,
    listing: PathBuf,
    code: Option<i32>,
    summary: Option<Counts>,
}

/// Cross-validate `pbfhogg diff` against osmium's reading of the same pair.
///
/// `direct_io` reaches all four pbfhogg diffs, the command under test. The
/// apply-changes that builds "new" runs buffered: nothing compares "new"
/// with osmium's apply-changes, so an O_DIRECT fault there would hand both
/// tools the same wrong pair and pass; `verify merge` holds apply-changes
/// under `--direct-io` to osmium instead.
pub fn run(
    harness: &mut Harness,
    pbf: &Path,
    osc: &Path,
    direct_io: bool,
) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("diff")?;

    harness.msg("=== verify diff ===");
    harness.msg(&format!("  old: {}", pbf.display()));
    harness.msg(&format!(
        "  osc: {} (used to create 'new' via apply-changes)",
        osc.display()
    ));

    if !verify_input_order::input_header(harness, pbf, &outdir)?.optional_sorted() {
        return refused(harness, &outdir, pbf, direct_io);
    }

    let new_pbf = outdir.join("new.osm.pbf");
    harness.msg("--- creating 'new' PBF via apply-changes ---");
    let captured = harness.pbfhogg_discard(
        PbfArgv::new("apply-changes")
            .input(pbf)
            .input(osc)
            .output(&new_pbf),
    )?;
    harness.require_success(&captured, "pbfhogg apply-changes")?;

    let touched = touched_identities(osc)?;
    harness.msg(&format!("  the OSC touches {} identities", touched.len()));

    let runs = DiffRuns::run(harness, &outdir, (pbf, &new_pbf), direct_io)?;
    let judgement = against_osmium(
        harness,
        &outdir,
        (pbf, &new_pbf),
        &touched,
        &runs.indexed_verbose.listing,
    )?;

    harness.record(
        "indexed verbose listing vs osmium content",
        Verdict::from_pass(judgement.mismatches == 0),
    );
    runs.check(harness, judgement.expected)?;
    Ok(with_claim(&judgement.coverage))
}

/// The check on input whose header lacks `Sort.Type_then_ID` among its
/// optional features, which the text diff refuses on either side by contract
/// (and the apply-changes that builds "new" refuses too): the plain diff runs
/// twice, the input as OLD and then as NEW against an osmium-sorted copy of
/// it, each held to the refusal naming the input with an empty stdout
/// (`verify_input_order`). A mixed indexed pair, so no comparison engine is
/// exercised; both runs are made whatever the first one shows.
fn refused(
    harness: &mut Harness,
    outdir: &Path,
    pbf: &Path,
    direct_io: bool,
) -> Result<Option<String>, Error> {
    let copy = verify_input_order::sorted_counterpart(harness, pbf, outdir)?;
    let claim = Some(verify_input_order::refusal_claim(
        "the plain pbfhogg text diff",
        ", as OLD and as NEW, the other side an osmium-sorted copy of it",
        direct_io,
    ));
    for (side, old, new) in [("old", pbf, copy.as_path()), ("new", copy.as_path(), pbf)] {
        let sink = outdir.join(format!("refused-as-{side}.txt"));
        let label = format!("diff (input as {})", side.to_uppercase());
        verify_input_order::expect_unsorted_refusal(
            harness,
            text_diff(old, new, false, direct_io),
            &verify_input_order::Refusal {
                label: &label,
                operand: pbf,
                answers: &[0, 1],
                sink: Some(&sink),
            },
        )?;
    }
    Ok(claim)
}

/// The four pbfhogg diffs of the pair: indexed and stripped of indexdata,
/// each with and without `--verbose`.
struct DiffRuns {
    indexed_verbose: DiffRun,
    indexed_plain: DiffRun,
    unindexed_verbose: DiffRun,
    unindexed_plain: DiffRun,
}

impl DiffRuns {
    /// Listings go to files, never memory: their size is the size of the
    /// change set, which at planet scale is not bounded by any capture.
    fn run(
        harness: &mut Harness,
        outdir: &Path,
        (old, new): (&Path, &Path),
        direct_io: bool,
    ) -> Result<Self, Error> {
        let mut diff = |label, stem, pair, verbose| {
            pbfhogg_diff(harness, outdir, label, stem, pair, verbose, direct_io)
        };
        let indexed_verbose = diff("indexed verbose", "pbfhogg-diff-verbose", (old, new), true)?;
        let indexed_plain = diff("indexed plain", "pbfhogg-diff", (old, new), false)?;
        // pbfhogg diff has a separate code path for inputs without indexdata.
        // apply-changes refuses an unindexed base, so a raw variant cannot
        // drive it; stripping both sides of the pair can.
        let old_raw = outdir.join("old-unindexed.osm.pbf");
        let new_raw = outdir.join("new-unindexed.osm.pbf");
        harness.msg("--- stripping indexdata from old and new ---");
        harness.strip_indexdata(old, &old_raw)?;
        harness.strip_indexdata(new, &new_raw)?;
        let raw = (old_raw.as_path(), new_raw.as_path());
        Ok(Self {
            indexed_verbose,
            indexed_plain,
            unindexed_verbose: pbfhogg_diff(
                harness,
                outdir,
                "unindexed verbose",
                "pbfhogg-unindexed-diff-verbose",
                raw,
                true,
                direct_io,
            )?,
            unindexed_plain: pbfhogg_diff(
                harness,
                outdir,
                "unindexed plain",
                "pbfhogg-unindexed-diff",
                raw,
                false,
                direct_io,
            )?,
        })
    }

    /// Every run's summary and exit against the expected counts, the stripped
    /// verbose listing against the indexed one byte for byte, and each plain
    /// listing against the indexed verbose listing's headers.
    fn check(&self, harness: &mut Harness, expected: Counts) -> Result<(), Error> {
        let expected_code =
            i32::from((expected.created + expected.modified + expected.deleted) > 0);
        for run in [
            &self.indexed_verbose,
            &self.indexed_plain,
            &self.unindexed_verbose,
            &self.unindexed_plain,
        ] {
            let ok = run.summary == Some(expected) && run.code == Some(expected_code);
            harness.msg(&format!(
                "  {}: exit {:?}, summary {:?}: {}",
                run.label,
                run.code,
                run.summary,
                if ok { "PASS" } else { "FAIL" }
            ));
            harness.record(
                &format!("{} summary and exit vs osmium content", run.label),
                Verdict::from_pass(ok),
            );
        }

        harness.msg("=== listings across modes ===");
        let verbose = &self.indexed_verbose.listing;
        let differs = stream::first_difference(&self.unindexed_verbose.listing, verbose)?;
        if let Some(at) = differs {
            harness.msg(&format!(
                "  unindexed verbose: FAIL (first differs at byte {at})"
            ));
        }
        harness.record(
            "unindexed verbose listing vs indexed",
            Verdict::from_pass(differs.is_none()),
        );
        for run in [&self.indexed_plain, &self.unindexed_plain] {
            let differs = header_projection_difference(verbose, &run.listing)?;
            if let Some(line) = differs {
                harness.msg(&format!(
                    "  {}: FAIL (line {line} differs from the verbose listing's headers)",
                    run.label
                ));
            }
            harness.record(
                &format!("{} listing vs indexed verbose headers", run.label),
                Verdict::from_pass(differs.is_none()),
            );
        }
        Ok(())
    }
}

/// The findings with a claim naming what the judged records did not
/// exercise and the qualifications the judgement accepted.
fn with_claim(coverage: &Coverage) -> Option<String> {
    let mut claim = Vec::new();
    let missing = coverage.missing();
    if !missing.is_empty() {
        claim.push(format!("not exercised: {}", missing.join(", ")));
    }
    if coverage.version_zero {
        claim.push(
            "version 0 in osmium's OPL, which cannot say whether a version is present, \
             matched against either spelling"
                .to_owned(),
        );
    }
    if coverage.budget_replaced {
        claim.push(
            "a replaced detail within the 1000-edit cap accepted on pbfhogg's search \
             budget, not checked"
                .to_owned(),
        );
    }
    if claim.is_empty() {
        None
    } else {
        Some(claim.join("; "))
    }
}

/// What the judgement against osmium established.
struct Judgement {
    mismatches: u64,
    expected: Counts,
    coverage: Coverage,
}

/// Judge pbfhogg's indexed verbose listing against osmium's content: the
/// osmium diff listing, then the identities it withholds fetched from both
/// inputs. Returns the expected counts the summaries are held to.
fn against_osmium(
    harness: &mut Harness,
    outdir: &Path,
    (pbf, new_pbf): (&Path, &Path),
    touched: &[(OscKind, i64)],
    listing: &Path,
) -> Result<Judgement, Error> {
    harness.msg("--- osmium diff (OPL) ---");
    let osmium_listing = outdir.join("osmium-diff.opl");
    let captured = harness.reference(
        &ToolRun::osmium("diff-opl.1")
            .args(&["diff", "--suppress-common", "--output-format", "opl"])
            .input(pbf)
            .input(new_pbf)
            .arg("--summary")
            .accept(&[1]),
        &osmium_listing,
    )?;
    // osmium diff exits 1 when the inputs differ by its own measure, which
    // counts metadata; anything else means its listing cannot be trusted.
    harness.require_codes(&captured, "osmium diff", &[0, 1])?;
    fs::write(outdir.join("osmium-summary.txt"), &captured.stderr)?;
    let osmium =
        parse_osmium_summary(&String::from_utf8_lossy(&captured.stderr)).ok_or_else(|| {
            Error::Failed(format!(
                "diff verify: osmium's summary is not one `Summary: left=N right=N same=N \
                 different=N` line: {:?}",
                String::from_utf8_lossy(&captured.stderr).trim()
            ))
        })?;

    // Identities whose content the osmium listing withholds.
    let candidates = candidates(&osmium_listing, listing, touched)?;
    harness.msg(&format!(
        "  {} identities to fetch from both inputs (touched or reported modified, \
         osmium-same)",
        candidates.len()
    ));
    let fetched = if candidates.is_empty() {
        None
    } else {
        let ids = outdir.join("candidates.txt");
        write_ids(&ids, &candidates)?;
        Some((
            fetch(harness, outdir, "old", pbf, &ids, candidates.len())?,
            fetch(harness, outdir, "new", new_pbf, &ids, candidates.len())?,
        ))
    };

    harness.msg("=== listing vs osmium content ===");
    let mut coverage = Coverage::default();
    let judged = judge_listing(
        &osmium_listing,
        &candidates,
        fetched.as_ref().map(|(o, n)| (o, n)),
        listing,
        &mut coverage,
    )?;
    for line in &judged.shown {
        harness.msg(&format!("  {line}"));
    }
    if judged.mismatches > judged.shown.len() as u64 {
        harness.msg(&format!(
            "  ... and {} more",
            judged.mismatches - judged.shown.len() as u64
        ));
    }
    harness.msg(&format!(
        "  {} ({} records judged; listing in {})",
        if judged.mismatches == 0 {
            "PASS"
        } else {
            "FAIL"
        },
        (judged.expected.created + judged.expected.modified + judged.expected.deleted),
        listing.display()
    ));

    // osmium's own bookkeeping: each one-sided object and each side of a
    // checksum-different pair is one line.
    if judged.minus != osmium.left + osmium.different
        || judged.plus != osmium.right + osmium.different
    {
        return Err(Error::Failed(format!(
            "diff verify: osmium's listing has {} - and {} + lines, its summary left={} \
             right={} different={}: the listing was not read whole",
            judged.minus, judged.plus, osmium.left, osmium.right, osmium.different
        )));
    }
    let expected = expected_counts(&judged, osmium.same)?;
    harness.msg(&format!(
        "  expected: {} created, {} modified, {} deleted, {} common",
        expected.created, expected.modified, expected.deleted, expected.common
    ));
    Ok(Judgement {
        mismatches: judged.mismatches,
        expected,
        coverage,
    })
}

/// Run one pbfhogg text diff to `<stem>.txt`, its stderr to
/// `<stem>-summary.txt`. Exit 1 means the inputs differ; 2 and above mean
/// the run did not complete, and its missing summary must not be read.
fn pbfhogg_diff(
    harness: &mut Harness,
    outdir: &Path,
    label: &'static str,
    stem: &str,
    (old, new): (&Path, &Path),
    verbose: bool,
    direct_io: bool,
) -> Result<DiffRun, Error> {
    harness.msg(&format!("--- pbfhogg diff ({label}) ---"));
    let listing = outdir.join(format!("{stem}.txt"));
    let captured = harness.pbfhogg(text_diff(old, new, verbose, direct_io), &listing)?;
    harness.require_codes(&captured, &format!("pbfhogg diff ({label})"), &[0, 1])?;
    fs::write(outdir.join(format!("{stem}-summary.txt")), &captured.stderr)?;
    let stderr = String::from_utf8_lossy(&captured.stderr);
    for line in stderr.lines() {
        harness.msg(&format!("  {line}"));
    }
    Ok(DiffRun {
        label,
        listing,
        code: captured.code(),
        summary: parse_pbfhogg_summary(&stderr),
    })
}

/// Every (kind, ID) the input OSC names in an action block, sorted in object
/// order and deduplicated. Read strictly: an entry the scan cannot account
/// for is an error, since an identity missing here would leave its content
/// unjudged wherever osmium's checksum calls the pair same.
fn touched_identities(osc: &Path) -> Result<Vec<(OscKind, i64)>, Error> {
    let mut ids = Vec::new();
    crate::osc::scan_osc_file_strict(osc, |_, kind, id| ids.push((kind, id)))?;
    sort_identities(&mut ids);
    Ok(ids)
}

fn sort_identities(ids: &mut Vec<(OscKind, i64)>) {
    ids.sort_unstable_by_key(|&(kind, id)| order_key(kind, id));
    ids.dedup();
}

/// The identities to fetch: those the OSC touches and those pbfhogg reports
/// modified, less every one the osmium listing carries.
fn candidates(
    osmium_listing: &Path,
    pbfhogg_listing: &Path,
    touched: &[(OscKind, i64)],
) -> Result<Vec<(OscKind, i64)>, Error> {
    let mut ids = touched.to_vec();
    let mut records = Listing::open(pbfhogg_listing)?;
    while let Some(record) = records.next_record()? {
        if record.header.starts_with('*') {
            ids.push((record.kind, record.id));
        }
    }
    sort_identities(&mut ids);
    let mut groups = DiffGroups::open(osmium_listing)?;
    let mut listed = Vec::new();
    while let Some(group) = groups.next_group()? {
        listed.push(order_key(group.kind, group.id));
    }
    ids.retain(|&(kind, id)| listed.binary_search(&order_key(kind, id)).is_err());
    if let Some(&(kind, id)) = ids.iter().find(|&&(_, id)| id < 0) {
        return Err(Error::Failed(format!(
            "{}{id} must be fetched from both inputs, and osmium getid matches an ID by its \
             absolute value; diff cannot be compared on an input whose unlisted changes carry \
             negative IDs",
            kind_char(kind)
        )));
    }
    Ok(ids)
}

fn write_ids(path: &Path, ids: &[(OscKind, i64)]) -> Result<(), Error> {
    let mut out = BufWriter::new(File::create(path)?);
    for &(kind, id) in ids {
        writeln!(out, "{}{id}", kind_char(kind))?;
    }
    out.flush()?;
    Ok(())
}

/// One side's candidate objects, as osmium getid writes them in OPL.
struct Fetched {
    path: PathBuf,
    /// The objects osmium reported it did not find.
    missing: u64,
}

fn fetch(
    harness: &mut Harness,
    outdir: &Path,
    side: &str,
    input: &Path,
    ids: &Path,
    requested: usize,
) -> Result<Fetched, Error> {
    let path = outdir.join(format!("osmium-candidates-{side}.opl"));
    let captured = harness.reference(
        &ToolRun::osmium(format!("diff-candidates-{side}.1"))
            .arg("getid")
            .input(input)
            .arg("--id-file")
            .input(ids)
            .args(&["--output-format", "opl", "--verbose", "--output"])
            .output(&path)
            .arg("--overwrite")
            .accept_if(1, crate::verify_getid_refs::osmium_completed),
        &outdir.join(format!("osmium-candidates-{side}.stdout")),
    )?;
    harness.require_codes(&captured, &format!("osmium getid ({side})"), &[0, 1])?;
    let missing = match captured.code() {
        Some(0) => 0,
        Some(1) => crate::verify_getid_refs::osmium_missing(&captured.stderr).ok_or_else(|| {
            Error::Failed(format!(
                "osmium getid ({side}) exited 1 without completing: its verbose output does not \
                 end in a missing-object count and Done."
            ))
        })?,
        other => {
            return Err(Error::Failed(format!(
                "osmium getid ({side}) ended with {other:?}"
            )));
        }
    };
    harness.msg(&format!(
        "  osmium getid ({side}): {requested} requested, {missing} not found"
    ));
    Ok(Fetched { path, missing })
}

/// What the judgement found.
#[derive(Debug, Default)]
struct Judged {
    /// The counts osmium's content gives (common is filled in later).
    expected: Counts,
    /// Both-sided osmium groups whose content is equal (E).
    equal_listed: u64,
    /// Fetched candidates whose content differs (K).
    different_fetched: u64,
    mismatches: u64,
    shown: Vec<String>,
    /// osmium listing lines by side.
    minus: u64,
    plus: u64,
}

impl Judged {
    fn mismatch(&mut self, what: String) {
        self.mismatches += 1;
        if self.shown.len() < SHOWN_MISMATCHES {
            self.shown.push(what);
        }
    }
}

/// pbfhogg's common count from osmium's: osmium's `same` pairs, less the
/// fetched ones whose content differs, plus the listed pairs whose content
/// is equal.
fn expected_counts(judged: &Judged, same: u64) -> Result<Counts, Error> {
    let common = same
        .checked_sub(judged.different_fetched)
        .and_then(|c| c.checked_add(judged.equal_listed))
        .ok_or_else(|| {
            Error::Failed(format!(
                "diff verify: {} fetched pairs differ in content, more than osmium's same={same}",
                judged.different_fetched
            ))
        })?;
    Ok(Counts {
        common,
        ..judged.expected
    })
}

/// One side's fetched objects read alongside the candidate list.
struct FetchedSide<'a> {
    side: &'static str,
    fetched: &'a Fetched,
    reader: ObjectReader,
    head: Option<Object>,
    found: u64,
}

impl<'a> FetchedSide<'a> {
    fn open(side: &'static str, fetched: &'a Fetched) -> Result<Self, Error> {
        let mut reader = ObjectReader::open(&fetched.path)?;
        let head = reader.next_object()?;
        Ok(Self {
            side,
            fetched,
            reader,
            head,
            found: 0,
        })
    }

    fn unrequested(object: &Object) -> Error {
        Error::Failed(format!(
            "osmium getid returned {}{}, which was not requested",
            kind_char(object.kind),
            object.id
        ))
    }

    /// The fetched object for `want`, or `None` when osmium did not find it;
    /// an object before it was not requested.
    fn take(&mut self, want: (OscKind, i64)) -> Result<Option<Object>, Error> {
        let want = order_key(want.0, want.1);
        match &self.head {
            Some(object) if order_key(object.kind, object.id) < want => {
                Err(Self::unrequested(object))
            }
            Some(object) if order_key(object.kind, object.id) == want => {
                self.found += 1;
                let taken = self.head.take();
                self.head = self.reader.next_object()?;
                Ok(taken)
            }
            _ => Ok(None),
        }
    }

    /// Nothing left over, and every request found or reported missing.
    fn finish(&self, requested: u64) -> Result<(), Error> {
        if let Some(extra) = &self.head {
            return Err(Self::unrequested(extra));
        }
        let (side, found, missing) = (self.side, self.found, self.fetched.missing);
        if found + missing != requested {
            return Err(Error::Failed(format!(
                "osmium getid ({side}) found {found} and reported {missing} missing of \
                 {requested} requested"
            )));
        }
        Ok(())
    }
}

/// Where an identity's two objects came from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The osmium diff listing.
    Listed,
    /// Fetched with osmium getid.
    Fetched,
    /// Neither: osmium calls the pair same and nothing named it.
    Unlisted,
}

impl Judged {
    /// Judge one identity: what its objects say pbfhogg must print against
    /// what pbfhogg printed.
    fn identity(
        &mut self,
        (old, new): (Option<&Object>, Option<&Object>),
        source: Source,
        record: Option<&Record>,
        coverage: &mut Coverage,
    ) -> Result<(), Error> {
        let expected = match (old, new) {
            (Some(o), Some(n)) if o.same_content(n) => {
                if source == Source::Listed {
                    self.equal_listed += 1;
                }
                None
            }
            (Some(o), Some(n)) => {
                if source == Source::Fetched {
                    self.different_fetched += 1;
                }
                self.expected.modified += 1;
                Some(Expected::Modified(o, n))
            }
            (Some(o), None) => {
                self.expected.deleted += 1;
                Some(Expected::Deleted(o))
            }
            (None, Some(n)) => {
                self.expected.created += 1;
                Some(Expected::Created(n))
            }
            (None, None) => None,
        };
        match (expected, record) {
            (None, None) => {}
            (None, Some(r)) => self.mismatch(format!(
                "{:?}: osmium's content shows no change for {}{}",
                r.header,
                kind_char(r.kind),
                r.id
            )),
            (Some(e), None) => {
                let (class, object) = match e {
                    Expected::Created(n) => ("created", n),
                    Expected::Deleted(o) => ("deleted", o),
                    Expected::Modified(_, n) => ("modified", n),
                };
                self.mismatch(format!(
                    "{}{}: pbfhogg lists nothing, osmium's content gives {class}",
                    kind_char(object.kind),
                    object.id
                ));
            }
            (Some(e), Some(r)) => {
                if let Some(why) = detail::judge(&e, &r.header, &r.detail, coverage)? {
                    self.mismatch(format!("{}{}: {why}", kind_char(r.kind), r.id));
                }
            }
        }
        Ok(())
    }
}

/// Judge pbfhogg's verbose listing against osmium's content, all streams
/// merged in object order: the osmium listing's groups, the fetched
/// candidates and pbfhogg's records.
fn judge_listing(
    osmium_listing: &Path,
    candidates: &[(OscKind, i64)],
    fetched: Option<(&Fetched, &Fetched)>,
    pbfhogg_listing: &Path,
    coverage: &mut Coverage,
) -> Result<Judged, Error> {
    let mut judged = Judged::default();
    let mut groups = DiffGroups::open(osmium_listing)?;
    let mut group = groups.next_group()?;
    let mut sides = match fetched {
        Some((old, new)) => Some((
            FetchedSide::open("old", old)?,
            FetchedSide::open("new", new)?,
        )),
        None => None,
    };
    let mut pending = candidates.iter().copied().peekable();
    let mut records = Listing::open(pbfhogg_listing)?;
    let mut record = records.next_record()?;
    loop {
        let keys = [
            group.as_ref().map(|g| order_key(g.kind, g.id)),
            pending.peek().map(|&(kind, id)| order_key(kind, id)),
            record.as_ref().map(|r| order_key(r.kind, r.id)),
        ];
        let Some(key) = keys.iter().flatten().min().copied() else {
            break;
        };
        let (old, new, source) = if keys[0] == Some(key)
            && let Some(g) = group.take()
        {
            group = groups.next_group()?;
            (g.old, g.new, Source::Listed)
        } else if keys[1] == Some(key)
            && let Some(want) = pending.next()
        {
            let Some((old_side, new_side)) = sides.as_mut() else {
                return Err(Error::Failed(
                    "diff verify: candidates without fetched objects".into(),
                ));
            };
            let (old, new) = (old_side.take(want)?, new_side.take(want)?);
            if old.is_some() != new.is_some() {
                return Err(Error::Failed(format!(
                    "osmium getid found {}{} in one input only, where osmium diff lists no \
                     change for it",
                    kind_char(want.0),
                    want.1
                )));
            }
            (old, new, Source::Fetched)
        } else {
            (None, None, Source::Unlisted)
        };
        let this = if keys[2] == Some(key) {
            let taken = record.take();
            record = records.next_record()?;
            taken
        } else {
            None
        };
        judged.identity(
            (old.as_ref(), new.as_ref()),
            source,
            this.as_ref(),
            coverage,
        )?;
    }
    if let Some((old_side, new_side)) = &sides {
        let requested = candidates.len() as u64;
        old_side.finish(requested)?;
        new_side.finish(requested)?;
    }
    judged.minus = groups.minus;
    judged.plus = groups.plus;
    Ok(judged)
}

/// One record of pbfhogg's verbose listing: its header line and the detail
/// lines under it.
struct Record {
    kind: OscKind,
    id: i64,
    header: String,
    detail: Vec<String>,
}

/// pbfhogg's text listing read record by record, strictly: a header is
/// `-`, `+` or `*`, a type letter and a signed ID, then nothing or a space;
/// a detail line starts with two spaces and follows a header; identities
/// strictly ascend in object order. Anything else is an error.
struct Listing {
    path: PathBuf,
    reader: BufReader<File>,
    pending: Option<String>,
    last: Option<(usize, bool, u64)>,
    number: u64,
}

impl Listing {
    fn open(path: &Path) -> Result<Self, Error> {
        let file = File::open(path)
            .map_err(|e| Error::Failed(format!("cannot open {}: {e}", path.display())))?;
        Ok(Self {
            path: path.to_path_buf(),
            reader: BufReader::new(file),
            pending: None,
            last: None,
            number: 0,
        })
    }

    fn error(&self, why: &str) -> Error {
        Error::Failed(format!(
            "pbfhogg listing {} line {}: {why}",
            self.path.display(),
            self.number
        ))
    }

    fn line(&mut self) -> Result<Option<String>, Error> {
        if let Some(line) = self.pending.take() {
            return Ok(Some(line));
        }
        let mut bytes = Vec::new();
        let read = self
            .reader
            .read_until(b'\n', &mut bytes)
            .map_err(|e| Error::Failed(format!("cannot read {}: {e}", self.path.display())))?;
        if read == 0 {
            return Ok(None);
        }
        self.number += 1;
        if bytes.pop() != Some(b'\n') {
            return Err(self.error("no terminating newline"));
        }
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|e| self.error(&format!("not UTF-8: {e}")))
    }

    fn next_record(&mut self) -> Result<Option<Record>, Error> {
        let Some(header) = self.line()? else {
            return Ok(None);
        };
        let (kind, id) = parse_header(&header)
            .ok_or_else(|| self.error(&format!("{header:?} is not a header line")))?;
        let key = order_key(kind, id);
        if self.last.is_some_and(|last| key <= last) {
            return Err(self.error(&format!("{header:?} out of object order")));
        }
        self.last = Some(key);
        let mut detail = Vec::new();
        while let Some(line) = self.line()? {
            if line.starts_with("  ") {
                detail.push(line);
            } else {
                self.pending = Some(line);
                break;
            }
        }
        Ok(Some(Record {
            kind,
            id,
            header,
            detail,
        }))
    }
}

fn parse_header(line: &str) -> Option<(OscKind, i64)> {
    let rest = line.strip_prefix(['-', '+', '*'])?;
    let kind = match rest.as_bytes().first()? {
        b'n' => OscKind::Node,
        b'w' => OscKind::Way,
        b'r' => OscKind::Relation,
        _ => return None,
    };
    let rest = &rest[1..];
    let digits = rest.split(' ').next()?;
    let unsigned = digits.strip_prefix('-').unwrap_or(digits);
    if unsigned.is_empty() || !unsigned.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((kind, digits.parse().ok()?))
}

/// The first line (1-based) at which `plain` differs from `verbose` with
/// its detail lines (two leading spaces) removed, or `None` when equal.
fn header_projection_difference(verbose: &Path, plain: &Path) -> Result<Option<u64>, Error> {
    let open = |path: &Path| {
        File::open(path)
            .map(BufReader::new)
            .map_err(|e| Error::Failed(format!("cannot open {}: {e}", path.display())))
    };
    let read = |reader: &mut BufReader<File>, buf: &mut Vec<u8>, path: &Path| {
        buf.clear();
        reader
            .read_until(b'\n', buf)
            .map_err(|e| Error::Failed(format!("cannot read {}: {e}", path.display())))
    };
    let (mut v, mut p) = (open(verbose)?, open(plain)?);
    let (mut vbuf, mut pbuf) = (Vec::new(), Vec::new());
    let mut line = 0;
    loop {
        // The next verbose header line, skipping detail.
        loop {
            if read(&mut v, &mut vbuf, verbose)? == 0 || !vbuf.starts_with(b"  ") {
                break;
            }
        }
        read(&mut p, &mut pbuf, plain)?;
        line += 1;
        if vbuf != pbuf {
            return Ok(Some(line));
        }
        if vbuf.is_empty() {
            return Ok(None);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn pbfhogg_summaries_parse_strictly() {
        assert_eq!(
            parse_pbfhogg_summary(
                "note: something\n10321 differences: 5589 created, 3489 modified, 1243 deleted (59147550 common)\n"
            ),
            Some(Counts {
                created: 5589,
                modified: 3489,
                deleted: 1243,
                common: 59_147_550
            })
        );
        assert_eq!(
            parse_pbfhogg_summary("Files are identical (59152282 common elements)"),
            Some(Counts {
                common: 59_152_282,
                ..Counts::default()
            })
        );
        for text in [
            "10 differences: 5 created, 3 modified, 1 deleted (5 common)",
            "Files are identical (1 common elements)\nFiles are identical (1 common elements)",
            "0 differences: 0 created, 0 modified, 0 deleted (5 common)",
            "",
        ] {
            assert_eq!(parse_pbfhogg_summary(text), None, "{text}");
        }
    }

    #[test]
    fn osmium_summaries_parse_strictly() {
        assert_eq!(
            parse_osmium_summary("Summary: left=4 right=4 same=1 different=3\n"),
            Some(crate::diff_summary::OsmiumSummary {
                left: 4,
                right: 4,
                same: 1,
                different: 3
            })
        );
        assert_eq!(parse_osmium_summary("Summary: left=4 right=4 same=1"), None);
        assert_eq!(
            parse_osmium_summary("Summary: left=4 right=4 same=1 different=3 x=1"),
            None
        );
        assert_eq!(parse_osmium_summary(""), None);
    }

    #[test]
    fn headers_name_kind_and_signed_id() {
        assert_eq!(parse_header("*n2 v1 -> v2"), Some((OscKind::Node, 2)));
        assert_eq!(parse_header("-w-7 v1"), Some((OscKind::Way, -7)));
        assert_eq!(parse_header("+r9"), Some((OscKind::Relation, 9)));
        for line in ["  x", "n1 v1", "*x1", "*n", "*n1x"] {
            assert_eq!(parse_header(line), None, "{line}");
        }
    }

    #[test]
    fn plain_listings_must_equal_the_verbose_headers() {
        let dir = pbfhogg_test_scratch::scratch();
        let verbose = dir.path().join("verbose.txt");
        let plain = dir.path().join("plain.txt");
        fs::write(
            &verbose,
            "*n2 v1 -> v2\n  ~\"a\": \"1\" -> \"2\"\n-n5 v1\n*w1 v1\n  -ref @0 n1\n",
        )
        .unwrap();
        fs::write(&plain, "*n2 v1 -> v2\n-n5 v1\n*w1 v1\n").unwrap();
        assert_eq!(
            header_projection_difference(&verbose, &plain).unwrap(),
            None
        );
        fs::write(&plain, "*n2 v1 -> v2\n-n5 v1\n").unwrap();
        assert_eq!(
            header_projection_difference(&verbose, &plain).unwrap(),
            Some(3)
        );
        fs::write(
            &plain,
            "*n2 v1 -> v2\n  ~\"a\": \"1\" -> \"2\"\n-n5 v1\n*w1 v1\n",
        )
        .unwrap();
        assert_eq!(
            header_projection_difference(&verbose, &plain).unwrap(),
            Some(2)
        );
    }

    /// The listing judged end to end on the small pair from which the
    /// reference behaviour was established: osmium diff -c -f opl of it and
    /// pbfhogg diff -c -v of it.
    #[test]
    fn listing_judgement_matches_content_and_flags_faults() {
        let dir = pbfhogg_test_scratch::scratch();
        let osmium = dir.path().join("osmium.opl");
        fs::write(
            &osmium,
            "-n2 v1 dV c1 t2020-01-01T00:00:00Z i1 ua Tname=x x1 y2\n\
             +n2 v2 dV c2 t2020-01-02T00:00:00Z i1 ua Tname=y x1 y2\n\
             -n3 v1 dV c1 t2020-01-01T00:00:00Z i1 ua T x1 y2\n\
             +n3 v1 dV c1 t2020-01-01T00:00:00Z i1 ua T x1 y3\n\
             -n4 v2 dV c1 t2020-01-01T00:00:00Z i1 ua T x1 y2\n\
             +n4 v3 dV c2 t2020-01-02T00:00:00Z i1 ua T x1 y2\n\
             -n5 v1 dV c1 t2020-01-01T00:00:00Z i1 ua T x1 y2\n\
             +n8 v1 dV c1 t2020-01-01T00:00:00Z i1 ua T x1 y2\n",
        )
        .unwrap();
        let listing = dir.path().join("pbfhogg.txt");
        let judge = |text: &str| {
            fs::write(&listing, text).unwrap();
            let mut coverage = Coverage::default();
            judge_listing(&osmium, &[], None, &listing, &mut coverage).unwrap()
        };
        let good = "*n2 v1 -> v2\n  ~\"name\": \"x\" -> \"y\"\n*n3 v1\n  coordinates: (2, 1) -> (3, 1)\n-n5 v1\n+n8 v1\n";
        let judged = judge(good);
        assert_eq!(judged.mismatches, 0, "{:?}", judged.shown);
        assert_eq!(
            (
                judged.expected.created,
                judged.expected.modified,
                judged.expected.deleted
            ),
            (1, 2, 1)
        );
        assert_eq!((judged.equal_listed, judged.minus, judged.plus), (1, 4, 4));
        // A version-only change reported as modified, a missing record and a
        // wrong detail each fail.
        assert_eq!(
            judge("*n2 v1 -> v2\n  ~\"name\": \"x\" -> \"y\"\n*n3 v1\n  coordinates: (2, 1) -> (3, 1)\n*n4 v2 -> v3\n-n5 v1\n+n8 v1\n").mismatches,
            1
        );
        assert_eq!(
            judge("*n3 v1\n  coordinates: (2, 1) -> (3, 1)\n-n5 v1\n+n8 v1\n").mismatches,
            1
        );
        assert_eq!(
            judge("*n2 v1 -> v2\n  ~\"name\": \"x\" -> \"z\"\n*n3 v1\n  coordinates: (2, 1) -> (3, 1)\n-n5 v1\n+n8 v1\n").mismatches,
            1
        );
    }

    /// A pair osmium calls same is fetched and judged on its content.
    #[test]
    fn fetched_candidates_are_judged_on_content() {
        let dir = pbfhogg_test_scratch::scratch();
        let osmium = dir.path().join("osmium.opl");
        fs::write(&osmium, "").unwrap();
        let old = dir.path().join("old.opl");
        let new = dir.path().join("new.opl");
        // n1: a=bc against ab=c, the same to osmium's checksum; n2 equal; n3 absent.
        fs::write(&old, "n1 v1 Ta=bc x1 y2\nn2 v1 Tk=v x1 y2\n").unwrap();
        fs::write(&new, "n1 v1 Tab=c x1 y2\nn2 v1 Tk=v x1 y2\n").unwrap();
        let candidates = [(OscKind::Node, 1), (OscKind::Node, 2), (OscKind::Node, 3)];
        let fetched = |path: &Path| Fetched {
            path: path.to_path_buf(),
            missing: 1,
        };
        let (fo, fnew) = (fetched(&old), fetched(&new));
        let listing = dir.path().join("pbfhogg.txt");
        let run = |text: &str| {
            fs::write(&listing, text).unwrap();
            let mut coverage = Coverage::default();
            judge_listing(
                &osmium,
                &candidates,
                Some((&fo, &fnew)),
                &listing,
                &mut coverage,
            )
        };
        let judged = run("*n1 v1\n  -\"a\"=\"bc\"\n  +\"ab\"=\"c\"\n").unwrap();
        assert_eq!(judged.mismatches, 0, "{:?}", judged.shown);
        assert_eq!(judged.different_fetched, 1);
        assert_eq!(expected_counts(&judged, 5).unwrap().common, 4);
        assert_eq!(run("").unwrap().mismatches, 1);
        assert_eq!(
            run("*n1 v1\n  -\"a\"=\"bc\"\n  +\"ab\"=\"c\"\n*n2 v1\n")
                .unwrap()
                .mismatches,
            1
        );
        // A miscount of what osmium did not find is an error.
        let wrong = Fetched {
            path: old.clone(),
            missing: 0,
        };
        fs::write(&listing, "*n1 v1\n  -\"a\"=\"bc\"\n  +\"ab\"=\"c\"\n").unwrap();
        let mut coverage = Coverage::default();
        assert!(
            judge_listing(
                &osmium,
                &candidates,
                Some((&wrong, &fnew)),
                &listing,
                &mut coverage
            )
            .is_err()
        );
    }
}
