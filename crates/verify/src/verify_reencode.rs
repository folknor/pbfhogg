//! What `verify repack` and `verify degrade` share: the reading of their
//! input, the references osmium builds from it, the element comparison, the
//! block-layout rules and the readers they are judged with.
//!
//! **Elements.** osmium diff walks both files as a merge on object order and
//! calls an equal-key pair same only when a checksum over visible,
//! timestamp, tags, changeset, uid, user and the node location, way node
//! references with their coordinates, or relation members agrees. So a
//! summary of no left, right or different object says the two object
//! sequences are identical element by element, whatever their order (barring
//! a checksum collision; tags are fed without lengths). A moved or dropped
//! object is printed instead of checksummed, so its lines are compared
//! byte for byte, way coordinates included (`opl,locations_on_ways=true`).
//! The reference is the input when its kinds are grouped in order, else a
//! stable partition osmium builds (each kind's objects in input order); for
//! a comparison that must not see way coordinates (degrade drops them) it is
//! written without them.
//!
//! **Layout.** Every output blob's indexdata is first held to its payload by
//! `pbfhogg check --index`, a decoder independent of the writer's
//! `BlockBuilder`, and then read by the header-only claims walk
//! ([`super::pbf_frames::Claims`]) and compared, in constant memory, with
//! what the documented rule predicts from the input's claims (proven the same
//! way): [`RepackBuilder`] for repack, [`split`] for degrade's decode path
//! without an unsort mode, [`continuous`] for `--unsort`.
//!
//! **Domain.** One object per kind and ID: an input whose header requires
//! `HistoricalInformation` is refused, and so is one with a negative ID
//! (pbfhogg rejects negative input IDs project-wide). A run pbfhogg refuses
//! because a block would be too large for the format is recorded as refused,
//! never verified ([`Ran::TooLarge`]).

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use super::pbf_frames::{self, BlobClaim, ClaimedBlob, Claims, Framing};
use super::verify_fileinfo::{FileInfo, fileinfo};
use crate::argv::PbfArgv;
use crate::diff_summary::{OsmiumSummary, parse_osmium_summary};
use crate::error::Error;
use crate::harness::{Harness, Verdict};
use crate::reference::ToolRun;

/// pbfhogg's documented default `--elements-per-blob`, for repack and for
/// degrade's decode path: the cap a run that omits the flag is held to.
pub(super) const DEFAULT_CAP: u64 = 8000;

pub(super) const KINDS: [&str; 3] = ["node", "way", "relation"];

const LOCATIONS_ON_WAYS: &str = "LocationsOnWays";
const HISTORY: &str = "HistoricalInformation";

/// The advice pbfhogg's CLI appends to a blob-too-large refusal of an
/// oversized data blob, and to nothing else.
const TOO_LARGE_ADVICE: &str = "; lower --elements-per-blob";

/// What the arms read from their input before running anything.
pub(super) struct Input {
    pub path: PathBuf,
    pub info: FileInfo,
    /// `Sort.Type_then_ID` among the header's optional features.
    pub sorted_claim: bool,
    /// `LocationsOnWays` among the header's optional features.
    pub locations: bool,
    /// Kinds grouped in order (`inspect`'s `ordering.standard`).
    pub standard: bool,
    /// Every data blob carries indexdata, and `check --index` found every
    /// claim true: its claims can feed a layout prediction.
    pub indexed: bool,
}

impl Input {
    pub fn total(&self) -> u64 {
        self.info.counts.iter().sum()
    }
}

/// Read the input: osmium's counts and order, the header claims, the framing
/// and, on an indexed input, `check --index`. A negative ID, a history
/// header, or a claim its payload contradicts is an error.
pub(super) fn read_input(harness: &mut Harness, outdir: &Path, pbf: &Path) -> Result<Input, Error> {
    let info = fileinfo(harness, pbf, &outdir.join("input.fileinfo.json"))?;
    if let Some((kind, id)) = info.negative_id() {
        return Err(Error::Failed(format!(
            "{} holds {} ID {id}: pbfhogg rejects negative input IDs project-wide",
            pbf.display(),
            KINDS[kind]
        )));
    }
    if Claims::open(pbf)?
        .required_features()
        .iter()
        .any(|f| f == HISTORY)
    {
        return Err(Error::Failed(format!(
            "{} declares {HISTORY}: the comparison needs one object per kind and ID, and \
             osmium's plain references drop deleted objects' visibility",
            pbf.display()
        )));
    }
    let header = super::verify_input_order::input_header(harness, pbf, outdir)?;
    let indexed = matches!(pbf_frames::framing(pbf)?, Framing::Indexed { .. });
    if indexed {
        let index = check_index(harness, outdir, "input", pbf)?;
        if !index.passed {
            return Err(Error::Failed(format!(
                "pbfhogg check --index finds {} claims of {} false: its blob layout cannot be \
                 predicted from them",
                index.total_violations,
                pbf.display()
            )));
        }
    }
    let input = Input {
        path: pbf.to_path_buf(),
        info,
        sorted_claim: header.optional_sorted(),
        locations: header.optional_feature(LOCATIONS_ON_WAYS),
        standard: header.standard_order(),
        indexed,
    };
    harness.msg(&format!(
        "  input: {}, {}indexed, kinds {}grouped in order, sort claim {}, LocationsOnWays {}",
        if input.info.strictly_ordered() {
            "in strict order"
        } else {
            "not in strict order"
        },
        if input.indexed { "" } else { "not " },
        if input.standard { "" } else { "not " },
        input.sorted_claim,
        input.locations
    ));
    Ok(input)
}

// ---------------------------------------------------------------------------
// check --index
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct IndexReport {
    index: IndexSection,
}

/// The part of `check --index --json` the arms read.
#[derive(serde::Deserialize)]
pub(super) struct IndexSection {
    pub blobs: u64,
    pub blobs_with_indexdata: u64,
    pub passed: bool,
    pub total_violations: u64,
    violations: Vec<IndexViolation>,
}

#[derive(serde::Deserialize)]
struct IndexViolation {
    detail: String,
}

/// `pbfhogg check --index --json` of `pbf`, the report at
/// `<stem>.check-index.json` in `outdir`. Exit 0 must come with a pass and 1
/// with a failure; anything else is an error. Runs buffered: a comparator.
pub(super) fn check_index(
    harness: &mut Harness,
    outdir: &Path,
    stem: &str,
    pbf: &Path,
) -> Result<IndexSection, Error> {
    let report = outdir.join(format!("{stem}.check-index.json"));
    let captured = harness.pbfhogg(
        PbfArgv::new("check")
            .arg("--index")
            .arg("--json")
            .input(pbf),
        &report,
    )?;
    if !matches!(captured.code(), Some(0 | 1)) {
        harness.require_success(&captured, "pbfhogg check --index")?;
    }
    let file = File::open(&report)
        .map_err(|e| Error::Failed(format!("cannot open {}: {e}", report.display())))?;
    let section = serde_json::from_reader::<_, IndexReport>(BufReader::new(file))
        .map_err(|e| {
            Error::Failed(format!(
                "cannot read the check --index report {}: {e}",
                report.display()
            ))
        })?
        .index;
    let agrees = match captured.code() {
        Some(0) => section.passed && section.total_violations == 0,
        Some(1) => !section.passed && section.total_violations > 0,
        _ => false,
    };
    if !agrees {
        return Err(Error::Failed(format!(
            "pbfhogg check --index exited {:?} with a report that says passed {} with {} \
             violations ({})",
            captured.code(),
            section.passed,
            section.total_violations,
            report.display()
        )));
    }
    for violation in section.violations.iter().take(10) {
        harness.msg(&format!("  {stem}: check --index: {}", violation.detail));
    }
    Ok(section)
}

/// Record that every blob of `output` carries indexdata its payload bears
/// out, through [`check_index`].
pub(super) fn judge_index(
    harness: &mut Harness,
    outdir: &Path,
    label: &str,
    output: &Path,
) -> Result<(), Error> {
    let stem = label.replace(' ', "-");
    let index = check_index(harness, outdir, &stem, output)?;
    let ok = index.passed && index.blobs_with_indexdata == index.blobs;
    harness.msg(&format!(
        "  {label}: check --index: {} of {} blobs indexed, {} violations {}",
        index.blobs_with_indexdata,
        index.blobs,
        index.total_violations,
        if ok { "PASS" } else { "FAIL" }
    ));
    harness.record(
        &format!("{label} indexdata against its blobs"),
        Verdict::from_pass(ok),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Running the command under test
// ---------------------------------------------------------------------------

/// How a run of the command under test ended.
pub(super) enum Ran {
    Written,
    /// pbfhogg refused because a block would be too large for the format
    /// (exit 2, its CLI's element-cap advice, nothing published; the line is
    /// in the detail). Whether the block really was too large is not
    /// established.
    TooLarge,
}

/// Run `argv` (repack or degrade); any ending but success or a too-large
/// refusal is an error naming `label`.
pub(super) fn reencode(harness: &mut Harness, argv: PbfArgv, label: &str) -> Result<Ran, Error> {
    let outputs = argv.writes().to_vec();
    let captured = harness.pbfhogg_discard(argv)?;
    if captured.code() == Some(2) {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        if let Some(line) = stderr.lines().find(|l| l.contains(TOO_LARGE_ADVICE)) {
            let mut published = false;
            for output in &outputs {
                match output.symlink_metadata() {
                    Ok(_) => published = true,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
            if !published {
                harness.msg(&format!("  {label}: refused as too large: {line}"));
                return Ok(Ran::TooLarge);
            }
        }
    }
    harness.require_success(&captured, label)?;
    Ok(Ran::Written)
}

/// Record the output header's claims: `Sort.Type_then_ID` and
/// `LocationsOnWays` among its optional features as `sorted` and
/// `locations` say.
pub(super) fn judge_header(
    harness: &mut Harness,
    label: &str,
    output: &Path,
    sorted: bool,
    locations: bool,
) -> Result<(), Error> {
    let header = super::inspect_report::inspect(harness, output, false)?;
    for (feature, want, got) in [
        (
            crate::inspect_report::SORT_TYPE_THEN_ID,
            sorted,
            header.optional_sorted(),
        ),
        (
            LOCATIONS_ON_WAYS,
            locations,
            header.optional_feature(LOCATIONS_ON_WAYS),
        ),
    ] {
        let ok = want == got;
        harness.msg(&format!(
            "  {label}: header {} {feature}, {} {}",
            if got { "claims" } else { "does not claim" },
            if want { "as it must" } else { "as it must not" },
            if ok { "PASS" } else { "FAIL" }
        ));
        harness.record(&format!("{label} header {feature}"), Verdict::from_pass(ok));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// References
// ---------------------------------------------------------------------------

/// The files the arms compare an output's elements with, built by osmium
/// from the input once each and kept for the check.
pub(super) struct References<'a> {
    outdir: &'a Path,
    input: &'a Input,
    with_locations: Option<PathBuf>,
    without_locations: Option<PathBuf>,
}

impl<'a> References<'a> {
    pub fn new(outdir: &'a Path, input: &'a Input) -> Self {
        Self {
            outdir,
            input,
            with_locations: None,
            without_locations: None,
        }
    }

    /// The input's objects with each kind in input order and the kinds
    /// grouped nodes, ways, relations, way coordinates kept when
    /// `locations` (and present), else dropped. The input itself when it is
    /// that already.
    pub fn get(&mut self, harness: &mut Harness, locations: bool) -> Result<PathBuf, Error> {
        let keep = locations || !self.input.locations;
        let slot = if keep {
            &self.with_locations
        } else {
            &self.without_locations
        };
        if let Some(path) = slot {
            return Ok(path.clone());
        }
        let path = if self.input.standard && keep {
            self.input.path.clone()
        } else if self.input.standard {
            harness.msg("--- reference: the input without way coordinates (osmium cat) ---");
            let out = self.outdir.join("reference-plain.osm.pbf");
            Self::osmium_cat(
                harness,
                "reencode-plain.1",
                &[&self.input.path],
                &[],
                &out,
                false,
                "osmium cat (reference)",
            )?;
            out
        } else {
            self.partition(harness, keep && self.input.locations)?
        };
        if keep {
            self.with_locations = Some(path.clone());
        } else {
            self.without_locations = Some(path.clone());
        }
        Ok(path)
    }

    /// A stable partition of the input by kind: `osmium cat -t <kind>` per
    /// kind, then `osmium cat` of the three in kind order. osmium cat drains
    /// each input in read order before the next, so each kind keeps its input
    /// order.
    fn partition(&self, harness: &mut Harness, locations: bool) -> Result<PathBuf, Error> {
        harness.msg(&format!(
            "--- reference: the input partitioned by kind (osmium cat -t){} ---",
            if locations {
                ", way coordinates kept"
            } else {
                ""
            }
        ));
        let suffix = if locations { "-locations" } else { "" };
        let mut parts = Vec::new();
        for kind in KINDS {
            let part = self
                .outdir
                .join(format!("reference-{kind}{suffix}.osm.pbf"));
            Self::osmium_cat(
                harness,
                "reencode-partition.1",
                &[&self.input.path],
                &["-t", kind],
                &part,
                locations,
                "osmium cat -t (reference)",
            )?;
            parts.push(part);
        }
        let out = self
            .outdir
            .join(format!("reference-partition{suffix}.osm.pbf"));
        let refs: Vec<&Path> = parts.iter().map(PathBuf::as_path).collect();
        Self::osmium_cat(
            harness,
            "reencode-partition-join.1",
            &refs,
            &[],
            &out,
            locations,
            "osmium cat (reference partition)",
        )?;
        Ok(out)
    }

    fn osmium_cat(
        harness: &mut Harness,
        id: &str,
        inputs: &[&Path],
        args: &[&str],
        out: &Path,
        locations: bool,
        label: &str,
    ) -> Result<(), Error> {
        let mut run = ToolRun::osmium(id).arg("cat");
        for input in inputs {
            run = run.input(input);
        }
        run = run.args(args);
        if locations {
            run = run.args(&["-f", "pbf,locations_on_ways=true"]);
        }
        let run = run.arg("-o").output(out).arg("--overwrite");
        let captured = harness.reference(&run, &out.with_extension("stdout"))?;
        harness.require_success(&captured, label)
    }
}

// ---------------------------------------------------------------------------
// The element comparison
// ---------------------------------------------------------------------------

/// One osmium diff of a reference against an output.
pub(super) struct Compared {
    pub summary: OsmiumSummary,
    pub listing: PathBuf,
}

impl Compared {
    pub fn identical(&self) -> bool {
        self.summary.left == 0 && self.summary.right == 0 && self.summary.different == 0
    }
}

/// `osmium diff --suppress-common --output-format opl,locations_on_ways=true
/// <reference> <output> --summary`, its listing at `<stem>.diff.opl`. Exit 0
/// or 1, agreeing with the summary; anything else is an error.
pub(super) fn compare(
    harness: &mut Harness,
    outdir: &Path,
    stem: &str,
    reference: &Path,
    output: &Path,
) -> Result<Compared, Error> {
    let listing = outdir.join(format!("{stem}.diff.opl"));
    let captured = harness.reference(
        &ToolRun::osmium("reencode-diff.1")
            .args(&[
                "diff",
                "--suppress-common",
                "--output-format",
                "opl,locations_on_ways=true",
            ])
            .input(reference)
            .input(output)
            .arg("--summary")
            .accept(&[1]),
        &listing,
    )?;
    let stderr = String::from_utf8_lossy(&captured.stderr);
    let summary = parse_osmium_summary(&stderr).ok_or_else(|| {
        Error::Failed(format!(
            "osmium diff's summary is not one `Summary: left=N right=N same=N different=N` \
             line: {:?}",
            stderr.trim()
        ))
    })?;
    let compared = Compared { summary, listing };
    if (captured.code() == Some(0)) != compared.identical() {
        return Err(Error::Failed(format!(
            "osmium diff exited {:?} with the summary {summary:?}",
            captured.code()
        )));
    }
    harness.msg(&format!(
        "  osmium diff {stem}: left={} right={} same={} different={}",
        summary.left, summary.right, summary.same, summary.different
    ));
    Ok(compared)
}

/// Record that `compared` found the two sequences identical, replaying the
/// listing's head when not.
pub(super) fn record_identical(
    harness: &mut Harness,
    label: &str,
    compared: &Compared,
) -> Result<(), Error> {
    let ok = compared.identical();
    harness.msg(&format!(
        "  {label}: element sequence {} {}",
        if ok {
            "identical to the reference"
        } else {
            "differs from the reference"
        },
        if ok { "PASS" } else { "FAIL" }
    ));
    if !ok {
        harness.replay_listing(&compared.listing)?;
    }
    harness.record(&format!("{label} elements"), Verdict::from_pass(ok));
    Ok(())
}

/// One identity of an osmium diff listing: its kind, ID and the raw object
/// line on each side, at least one present.
#[derive(Debug)]
pub(super) struct Group {
    pub kind: usize,
    pub id: i64,
    pub minus: Option<Vec<u8>>,
    pub plus: Option<Vec<u8>>,
}

/// The groups of an `osmium diff --suppress-common -f opl` listing, in
/// object order, each object kept as the bytes osmium wrote. Strict: a line
/// that is not `-` or `+` and an object, an identity out of order, a side
/// twice for one identity, or a last line without its newline is an error.
/// [`finish`](Self::finish) holds the line counts to the summary.
pub(super) struct Listing {
    path: PathBuf,
    reader: BufReader<File>,
    line: Vec<u8>,
    number: u64,
    pending: Option<Line>,
    last: Option<(usize, bool, u64)>,
    minus: u64,
    plus: u64,
}

/// A listing line: its side (`-` or `+`), kind, ID and the object's bytes.
type Line = (u8, usize, i64, Vec<u8>);

fn order_key(kind: usize, id: i64) -> (usize, bool, u64) {
    (kind, id > 0, id.unsigned_abs())
}

impl Listing {
    pub fn open(path: &Path) -> Result<Self, Error> {
        let file = File::open(path)
            .map_err(|e| Error::Failed(format!("cannot open {}: {e}", path.display())))?;
        Ok(Self {
            path: path.to_path_buf(),
            reader: BufReader::new(file),
            line: Vec::new(),
            number: 0,
            pending: None,
            last: None,
            minus: 0,
            plus: 0,
        })
    }

    fn error(&self, why: &str) -> Error {
        Error::Failed(format!(
            "osmium diff listing {} line {}: {why}",
            self.path.display(),
            self.number
        ))
    }

    fn read_line(&mut self) -> Result<Option<Line>, Error> {
        self.line.clear();
        let read = self
            .reader
            .read_until(b'\n', &mut self.line)
            .map_err(|e| Error::Failed(format!("cannot read {}: {e}", self.path.display())))?;
        if read == 0 {
            return Ok(None);
        }
        self.number += 1;
        let Some(body) = self.line.strip_suffix(b"\n") else {
            return Err(self.error("no terminating newline (truncated?)"));
        };
        let parsed = parse_line(body).map_err(|why| self.error(why))?;
        if parsed.0 == b'-' {
            self.minus += 1;
        } else {
            self.plus += 1;
        }
        Ok(Some(parsed))
    }

    pub fn next_group(&mut self) -> Result<Option<Group>, Error> {
        let first = match self.pending.take() {
            Some(line) => line,
            None => match self.read_line()? {
                Some(line) => line,
                None => return Ok(None),
            },
        };
        let key = order_key(first.1, first.2);
        if self.last.is_some_and(|last| key <= last) {
            return Err(self.error("an identity out of object order"));
        }
        self.last = Some(key);
        let mut group = Group {
            kind: first.1,
            id: first.2,
            minus: None,
            plus: None,
        };
        place(&mut group, first.0, first.3);
        loop {
            let Some(line) = self.read_line()? else {
                return Ok(Some(group));
            };
            if line.1 != group.kind || line.2 != group.id {
                self.pending = Some(line);
                return Ok(Some(group));
            }
            if !place(&mut group, line.0, line.3) {
                return Err(self.error("one identity twice on one side"));
            }
        }
    }

    /// After the last group: the listing's `-` lines must number `left +
    /// different` and its `+` lines `right + different`, or it was not read
    /// whole.
    pub fn finish(self, summary: &OsmiumSummary) -> Result<(), Error> {
        if self.minus != summary.left + summary.different
            || self.plus != summary.right + summary.different
        {
            return Err(Error::Failed(format!(
                "osmium diff listing {} has {} - and {} + lines against the summary {summary:?}",
                self.path.display(),
                self.minus,
                self.plus
            )));
        }
        Ok(())
    }
}

fn place(group: &mut Group, side: u8, body: Vec<u8>) -> bool {
    let slot = if side == b'-' {
        &mut group.minus
    } else {
        &mut group.plus
    };
    slot.replace(body).is_none()
}

/// Read a listing line into its side, kind, ID and the object's bytes after
/// the side.
fn parse_line(line: &[u8]) -> Result<Line, &'static str> {
    let (&side, body) = line.split_first().ok_or("an empty line")?;
    if side != b'-' && side != b'+' {
        return Err("not a - or + line (a common line means --suppress-common was not applied)");
    }
    let (&letter, rest) = body.split_first().ok_or("a line without an object")?;
    let kind = match letter {
        b'n' => 0,
        b'w' => 1,
        b'r' => 2,
        _ => return Err("an object that is not a node, way or relation"),
    };
    let end = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
    let id = std::str::from_utf8(&rest[..end])
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or("an object ID that does not read")?;
    Ok((side, kind, id, body.to_vec()))
}

// ---------------------------------------------------------------------------
// Layout rules
// ---------------------------------------------------------------------------

/// The blocks a layout predicts are emitted one at a time as element counts,
/// to a sink that may fail.
pub(super) type Emit<'e> = dyn FnMut(u64) -> Result<(), Error> + 'e;

/// Which layout shapes a repack prediction went through: the ones its
/// coverage claim names when no case did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Witnesses {
    /// A shrink blob's nonzero remainder followed by a nonempty grow blob of
    /// its kind (the remainder must not take its elements).
    pub remainder_then_grow: bool,
    /// A nonempty grow payload entering a nonempty builder.
    pub coalesced: bool,
}

/// repack's rule for one kind, fed that kind's input blobs in file order:
/// a blob with at least `cap` elements (a shrink blob) flushes the builder,
/// then is written as `count / cap` full blocks and its remainder on its
/// own; a smaller one (a grow blob) goes element by element into the
/// builder, which flushes at `cap`. The kind ends with a flush.
pub(super) struct RepackBuilder {
    cap: u64,
    held: u64,
    after_remainder: bool,
    pub witnesses: Witnesses,
}

impl RepackBuilder {
    pub fn new(cap: u64) -> Self {
        Self {
            cap,
            held: 0,
            after_remainder: false,
            witnesses: Witnesses::default(),
        }
    }

    pub fn blob(&mut self, count: u64, emit: &mut Emit<'_>) -> Result<(), Error> {
        if count == 0 {
            return Ok(());
        }
        if count >= self.cap {
            if self.held > 0 {
                emit(self.held)?;
                self.held = 0;
            }
            for _ in 0..count / self.cap {
                emit(self.cap)?;
            }
            let rest = count % self.cap;
            if rest > 0 {
                emit(rest)?;
            }
            self.after_remainder = rest > 0;
        } else {
            self.witnesses.coalesced |= self.held > 0;
            self.witnesses.remainder_then_grow |= self.after_remainder;
            self.after_remainder = false;
            self.held += count;
            while self.held >= self.cap {
                emit(self.cap)?;
                self.held -= self.cap;
            }
        }
        Ok(())
    }

    /// End the kind: flush what the builder holds.
    pub fn finish(&mut self, emit: &mut Emit<'_>) -> Result<(), Error> {
        if self.held > 0 {
            emit(self.held)?;
        }
        self.held = 0;
        self.after_remainder = false;
        Ok(())
    }
}

/// degrade's decode path without an unsort mode, per input blob: `count /
/// cap` full blocks, then the remainder; nothing for an empty survivor set.
pub(super) fn split(cap: u64, count: u64, emit: &mut Emit<'_>) -> Result<(), Error> {
    for _ in 0..count / cap {
        emit(cap)?;
    }
    if !count.is_multiple_of(cap) {
        emit(count % cap)?;
    }
    Ok(())
}

/// `--unsort`: each kind packed continuously, full blocks then the rest.
pub(super) fn continuous(cap: u64, total: u64, emit: &mut Emit<'_>) -> Result<(), Error> {
    split(cap, total, emit)
}

/// Feed `f` the claim of every blob of `kind` in `path`, in file order.
/// Every blob must carry a claim.
pub(super) fn each_claim(
    path: &Path,
    kind: usize,
    mut f: impl FnMut(BlobClaim) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut claims = Claims::open(path)?;
    while let Some(blob) = claims.next_blob()? {
        let claim = blob.claim.ok_or_else(|| {
            Error::Failed(format!(
                "data blob {} of {} carries no indexdata",
                blob.index,
                path.display()
            ))
        })?;
        if claim.kind == kind {
            f(claim)?;
        }
    }
    Ok(())
}

/// An output's blobs compared, in order, with a predicted layout: the first
/// disagreement is kept and the rest ignored.
pub(super) struct LayoutCheck {
    claims: Claims,
    position: u64,
    pub mismatch: Option<String>,
}

impl LayoutCheck {
    pub fn open(output: &Path) -> Result<Self, Error> {
        Ok(Self {
            claims: Claims::open(output)?,
            position: 0,
            mismatch: None,
        })
    }

    /// The next output blob must claim `count` elements of `kind`.
    pub fn expect(&mut self, kind: usize, count: u64) -> Result<(), Error> {
        if self.mismatch.is_some() {
            return Ok(());
        }
        let at = self.position;
        self.position += 1;
        self.mismatch = match self.claims.next_blob()? {
            None => Some(format!(
                "the output ends where block {at} ({count} {}s) was predicted",
                KINDS[kind]
            )),
            Some(ClaimedBlob { claim: None, .. }) => {
                Some(format!("output blob {at} carries no indexdata"))
            }
            Some(ClaimedBlob {
                claim: Some(claim), ..
            }) if claim.kind != kind || claim.count != count => Some(format!(
                "output blob {at} holds {} {}s where {count} {}s were predicted",
                claim.count, KINDS[claim.kind], KINDS[kind]
            )),
            Some(_) => None,
        };
        Ok(())
    }

    /// After the last predicted block: the output must hold no more blobs.
    /// Returns the first disagreement.
    pub fn finish(mut self) -> Result<Option<String>, Error> {
        if self.mismatch.is_none() && self.claims.next_blob()?.is_some() {
            self.mismatch = Some(format!(
                "the output holds more than the {} predicted blocks",
                self.position
            ));
        }
        Ok(self.mismatch)
    }
}

/// What one pass over an output's claims finds, per kind and overall.
#[derive(Debug, Default)]
pub(super) struct OutputBlobs {
    pub kinds: [KindBlobs; 3],
    /// The first blob over the cap, or with no element.
    pub bad_size: Option<String>,
    /// A blob claiming a kind after a later kind's blob.
    pub kind_step_back: Option<String>,
}

#[derive(Debug, Default)]
pub(super) struct KindBlobs {
    pub blobs: u64,
    pub elements: u64,
    /// Adjacent pairs of this kind's blobs whose ID ranges overlap.
    pub overlaps: u64,
    /// The position among this kind's blobs of the first such pair's first
    /// blob.
    pub first_overlap: Option<u64>,
    pub first: Option<(ClaimedBlob, BlobClaim)>,
    pub second: Option<(ClaimedBlob, BlobClaim)>,
    last_max: Option<i64>,
}

/// One pass over `output`'s claims (every blob must carry one).
pub(super) fn output_blobs(output: &Path, cap: u64) -> Result<OutputBlobs, Error> {
    let mut out = OutputBlobs::default();
    let mut highest: Option<usize> = None;
    let mut claims = Claims::open(output)?;
    while let Some(blob) = claims.next_blob()? {
        let claim = blob.claim.ok_or_else(|| {
            Error::Failed(format!(
                "data blob {} of {} carries no indexdata",
                blob.index,
                output.display()
            ))
        })?;
        if out.bad_size.is_none() && (claim.count == 0 || claim.count > cap) {
            out.bad_size = Some(format!(
                "blob {} holds {} {}s, cap {cap}",
                blob.index, claim.count, KINDS[claim.kind]
            ));
        }
        if out.kind_step_back.is_none() && highest.is_some_and(|h| claim.kind < h) {
            out.kind_step_back = Some(format!(
                "blob {} holds {}s after a later kind's blob",
                blob.index, KINDS[claim.kind]
            ));
        }
        highest = Some(highest.map_or(claim.kind, |h| h.max(claim.kind)));
        let kind = &mut out.kinds[claim.kind];
        if kind.last_max.is_some_and(|max| claim.min_id <= max) {
            kind.overlaps += 1;
            kind.first_overlap.get_or_insert(kind.blobs - 1);
        }
        kind.last_max = Some(claim.max_id);
        match kind.blobs {
            0 => kind.first = Some((blob, claim)),
            1 => kind.second = Some((blob, claim)),
            _ => {}
        }
        kind.blobs += 1;
        kind.elements += claim.count;
    }
    Ok(out)
}

/// The elements of the blob `located` in `output`, as IDs, each required to
/// be of `kind` and to number as its claim says: the decoded sequence and the
/// claims then describe the same blob.
pub(super) fn located_ids(
    output: &Path,
    located: &(ClaimedBlob, BlobClaim),
    kind: usize,
) -> Result<Vec<i64>, Error> {
    let (blob, claim) = located;
    let elements = pbf_frames::blob_ids(output, blob.payload_offset, blob.datasize)?;
    if elements.len() as u64 != claim.count || elements.iter().any(|&(k, _)| k != kind) {
        return Err(Error::Failed(format!(
            "data blob {} of {} decodes to {} elements, not the {} {}s its claim checked by \
             check --index says",
            blob.index,
            output.display(),
            elements.len(),
            claim.count,
            KINDS[kind]
        )));
    }
    Ok(elements.into_iter().map(|(_, id)| id).collect())
}

/// Record the claims pass's cap and kind-order verdicts.
pub(super) fn record_blobs(harness: &mut Harness, label: &str, blobs: &OutputBlobs) {
    for (what, problem) in [
        ("block sizes", &blobs.bad_size),
        ("kind order", &blobs.kind_step_back),
    ] {
        if let Some(why) = problem {
            harness.msg(&format!("  {label}: {why} FAIL"));
            harness.fail(format!("{label} {what}: {why}"));
        }
    }
}

/// Record a layout prediction's outcome.
pub(super) fn record_layout(harness: &mut Harness, label: &str, mismatch: Option<String>) {
    match mismatch {
        Some(why) => {
            harness.msg(&format!("  {label}: layout: {why} FAIL"));
            harness.fail(format!("{label} block layout: {why}"));
        }
        None => harness.msg(&format!("  {label}: layout as predicted PASS")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn repack(cap: u64, counts: &[u64]) -> (Vec<u64>, Witnesses) {
        let mut blocks = Vec::new();
        let mut builder = RepackBuilder::new(cap);
        let mut emit = |n| {
            blocks.push(n);
            Ok(())
        };
        for &c in counts {
            builder.blob(c, &mut emit).unwrap();
        }
        builder.finish(&mut emit).unwrap();
        (blocks, builder.witnesses)
    }

    #[test]
    fn a_shrink_blobs_remainder_never_takes_the_grow_blobs_after_it() {
        // cap 3000 over 8000-blobs and a short last one: 3000, 3000, 2000
        // per shrink blob, the 1000-blob on its own.
        let (blocks, w) = repack(3000, &[8000, 8000, 1000]);
        assert_eq!(blocks, [3000, 3000, 2000, 3000, 3000, 2000, 1000]);
        assert!(w.remainder_then_grow);
        assert!(!w.coalesced);
    }

    #[test]
    fn grow_blobs_coalesce_and_split_at_the_cap() {
        let (blocks, w) = repack(64000, &[8000; 9]);
        assert_eq!(blocks, [64000, 8000]);
        assert!(w.coalesced);
        // A grow run is flushed before a shrink blob.
        let (blocks, _) = repack(5, &[3, 3, 7, 2]);
        assert_eq!(blocks, [5, 1, 5, 2, 2]);
        // An exact multiple leaves no remainder to witness, and an empty
        // blob neither grows nor witnesses.
        let (blocks, w) = repack(4, &[8, 0, 1]);
        assert_eq!(blocks, [4, 4, 1]);
        assert!(!w.remainder_then_grow);
    }

    #[test]
    fn decode_path_blobs_split_alone_and_unsort_packs_the_kind() {
        let mut blocks = Vec::new();
        let mut emit = |n| {
            blocks.push(n);
            Ok(())
        };
        split(8000, 17000, &mut emit).unwrap();
        split(8000, 0, &mut emit).unwrap();
        continuous(8000, 8000, &mut emit).unwrap();
        assert_eq!(blocks, [8000, 8000, 1000, 8000]);
    }

    #[test]
    fn listing_lines_read_side_kind_and_id_and_keep_the_object() {
        let (side, kind, id, body) = parse_line(b"-w-12 v1 dV Nn1x1y2").unwrap();
        assert_eq!((side, kind, id), (b'-', 1, -12));
        assert_eq!(body, b"w-12 v1 dV Nn1x1y2");
        assert!(parse_line(b" n1 v1").is_err());
        assert!(parse_line(b"+c1 v1").is_err());
        assert!(parse_line(b"+nx v1").is_err());
    }

    #[test]
    fn a_listing_groups_both_sides_and_is_held_to_its_summary() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = scratch.path();
        let path = dir.join("l.opl");
        std::fs::write(&path, "-n2 v1 T x1 y1\n+n2 v1 T x1 y1\n-w5 v2 T Nn1\n").unwrap();
        let mut listing = Listing::open(&path).unwrap();
        let first = listing.next_group().unwrap().unwrap();
        assert_eq!((first.kind, first.id), (0, 2));
        assert_eq!(first.minus, first.plus);
        let second = listing.next_group().unwrap().unwrap();
        assert!(second.plus.is_none());
        assert!(listing.next_group().unwrap().is_none());
        let summary = OsmiumSummary {
            left: 2,
            right: 1,
            same: 0,
            different: 0,
        };
        listing.finish(&summary).unwrap();
        std::fs::write(&path, "-n2 v1\n-n1 v1\n").unwrap();
        let mut listing = Listing::open(&path).unwrap();
        listing.next_group().unwrap();
        assert!(listing.next_group().is_err());
        std::fs::write(&path, "-n2 v1").unwrap();
        assert!(Listing::open(&path).unwrap().next_group().is_err());
    }
}
