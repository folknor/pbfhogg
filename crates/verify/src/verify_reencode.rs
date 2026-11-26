//! What `verify repack` and `verify degrade` share: the reading of their
//! input, the references osmium builds from it, the element comparison, the
//! block-layout rules and the readers they are judged with.
//!
//! **Elements.** osmium decodes the reference and the output to OPL with
//! metadata and inline way coordinates, and the verifier walks the two
//! record streams as osmium diff walks its inputs, but comparing whole lines
//! byte for byte ([`opl_compare::compare_sequence`]): OPL escapes its
//! separators, so a shifted tag boundary (`a=bc` against `ab=c`), which
//! osmium diff's checksum feeds identically, differs here. No first-only,
//! second-only or differing record says the two object sequences are
//! identical element by element, whatever their order; a removed or moved
//! object is listed with its line. The expected side is osmium's OPL
//! decoding of the input, partitioned stably by kind by the verifier (each
//! kind's objects in input order), with every way-node coordinate written
//! undefined where the command must drop them ([`References`]).
//!
//! **Layout.** Every output blob's indexdata is first held to its payload:
//! kind, count and ID range by the verifier's own decode of every blob
//! ([`super::pbf_frames::TruthWalk`]), bounding box and tag index by
//! `pbfhogg check --index` (independent of the writer's `BlockBuilder` but
//! not of pbfhogg); then the saved claims are compared with
//! what the documented rule predicts from the input's claims (proven the same
//! way): [`RepackBuilder`] for repack, [`split`] for degrade's decode path
//! without an unsort mode, [`continuous`] for `--unsort`.
//!
//! **Domain.** One object per kind and signed ID: an input whose header
//! requires `HistoricalInformation` is refused. Negative IDs are in the
//! domain; degrade's drop and unsort layout predictions, which read claimed
//! ranges numerically, do not run on them (see `checks/degrade.rs`). The
//! independent blob decode refuses a dense ID delta that overflows `i64`,
//! but way refs and relation members are decoded only by osmium on both
//! sides, so a wrapped ref delta at the `i64` extremes is certified only as
//! far as libosmium's signed arithmetic agrees with pbfhogg's wrapping
//! encoding. A run pbfhogg refuses
//! because a block would be too large for the format is recorded as refused,
//! never verified ([`Ran::TooLarge`]).

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::pbf_frames::{self, BlobClaim, ClaimedBlob, Claims, FrameAudit, Framing, Origin};
use super::verify_fileinfo::{FileInfo, fileinfo};
use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::{Harness, Verdict};
use crate::opl_compare::{self, Merged, Policy};

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
    /// Kinds grouped in order, known from osmium reading the input in
    /// kind and ID order (`objects_ordered`); an input with grouped kinds
    /// but IDs out of order reads as not. The OPL scan detects grouping
    /// independently before deciding whether it needs to write a partition.
    pub standard: bool,
    /// Every data blob carries indexdata, and both the verifier's own blob
    /// decode and `check --index` found every claim true: its claims can
    /// feed a layout prediction.
    pub indexed: bool,
    /// Validated claims bucketed by kind, in file order within each kind.
    pub claims: [Vec<BlobClaim>; 3],
}

impl Input {
    pub fn total(&self) -> u64 {
        self.info.counts.iter().sum()
    }
}

/// Read the input: osmium's counts and order, the header claims, the framing
/// and, on an indexed input, `check --index`. A history header, or a claim
/// its payload contradicts, is an error.
pub(super) fn read_input(harness: &mut Harness, outdir: &Path, pbf: &Path) -> Result<Input, Error> {
    let info = fileinfo(
        harness,
        (pbf, Origin::Other),
        &outdir.join("input.fileinfo.json"),
    )?;
    // Every refusal here is of the input, so a verifier error: the comparison
    // cannot be made on it, which says nothing about pbfhogg.
    if Claims::open(pbf, Origin::Other)?
        .required_features()
        .iter()
        .any(|f| f == HISTORY)
    {
        return Err(Error::Tool(format!(
            "{} declares {HISTORY}: the comparison needs one object per kind and ID",
            pbf.display()
        )));
    }
    let header = super::verify_input_order::input_header(harness, pbf, outdir)?;
    let indexed = matches!(
        pbf_frames::framing(pbf, Origin::Other)?,
        Framing::Indexed { .. }
    );
    let mut claims: [Vec<BlobClaim>; 3] = Default::default();
    if indexed {
        let audit = cached_audit(harness, pbf, Origin::Other, None)?;
        for claim in audit.claims.iter().filter_map(|blob| blob.claim) {
            claims[claim.kind].push(claim);
        }
        let truth = &audit.truth;
        if !truth.passed() {
            return Err(Error::Tool(format!(
                "the verifier's blob decode finds {} claims of {} false ({}): its blob layout \
                 cannot be predicted from them",
                truth.violations,
                pbf.display(),
                truth.shown.join("; ")
            )));
        }
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
        sorted_claim: header.optional_sorted(),
        locations: header.optional_feature(LOCATIONS_ON_WAYS),
        standard: info.ordered,
        info,
        indexed,
        claims,
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
/// out: kind, count and ID range by the verifier's own decode
/// ([`pbf_frames::TruthWalk`]), and bounding box and tag index, which
/// that decode does not read, by [`check_index`] (pbfhogg's own decoder,
/// so same-project evidence for those two).
fn judge_index(
    harness: &mut Harness,
    outdir: &Path,
    label: &str,
    output: &Path,
    truth: &pbf_frames::IndexTruth,
) -> Result<(), Error> {
    let ok = truth.passed() && truth.blobs_with_indexdata == truth.blobs;
    for line in &truth.shown {
        harness.msg(&format!("  {label}: blob decode: {line}"));
    }
    harness.msg(&format!(
        "  {label}: blob decode: {} of {} blobs indexed, {} violations {}",
        truth.blobs_with_indexdata,
        truth.blobs,
        truth.violations,
        if ok { "PASS" } else { "FAIL" }
    ));
    harness.record(
        &format!("{label} indexdata against the verifier's blob decode"),
        Verdict::from_pass(ok),
    );
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
fn judge_header(
    harness: &mut Harness,
    label: &str,
    header: &pbf_frames::Header,
    sorted: bool,
    locations: bool,
) {
    for (feature, want, got) in [
        (
            pbf_frames::SORT_TYPE_THEN_ID,
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
}

// ---------------------------------------------------------------------------
// References
// ---------------------------------------------------------------------------

/// Successful audits are memoized by the same file stamp as run watches.
#[derive(Default)]
pub(crate) struct FrameStore {
    entries: Vec<(crate::run::Stamp, Rc<FrameAudit>)>,
}

impl FrameStore {
    fn get(&self, stamp: Option<crate::run::Stamp>) -> Option<Rc<FrameAudit>> {
        self.entries
            .iter()
            .find(|(key, _)| Some(*key) == stamp)
            .map(|(_, audit)| Rc::clone(audit))
    }
}

fn cached_audit(
    harness: &mut Harness,
    path: &Path,
    origin: Origin,
    walk: Option<FrameIds>,
) -> Result<Rc<FrameAudit>, Error> {
    let stamp = walk
        .as_ref()
        .map_or_else(|| crate::run::stamp(path).ok(), |walk| walk.stamp);
    if let Some(audit) = harness.run.frames.get(stamp) {
        return Ok(audit);
    }
    let audit = Rc::new(match walk {
        Some(walk) => walk.finish()?,
        None => pbf_frames::TruthWalk::open(path, origin, false)?.finish()?,
    });
    if let Some(stamp) = stamp
        && crate::run::stamp(path).ok() == Some(stamp)
    {
        harness.run.frames.entries.push((stamp, Rc::clone(&audit)));
    }
    Ok(audit)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn judge_output(
    harness: &mut Harness,
    outdir: &Path,
    label: &str,
    output: &Path,
    sorted: bool,
    locations: bool,
    cap: u64,
    decoded: Option<FrameIds>,
) -> Result<OutputBlobs, Error> {
    let audit = match harness.run.frames.get(crate::run::stamp(output).ok()) {
        Some(audit) => {
            judge_header(harness, label, &audit.header, sorted, locations);
            audit
        }
        None => {
            let walk = match decoded {
                Some(walk) => walk,
                None => FrameIds::open(output, true)?,
            };
            judge_header(harness, label, walk.walk.header(), sorted, locations);
            cached_audit(harness, output, Origin::Pbfhogg, Some(walk))?
        }
    };
    judge_index(harness, outdir, label, output, &audit.truth)?;
    let blobs = output_blobs(output, cap, audit)?;
    record_blobs(harness, label, &blobs);
    Ok(blobs)
}

/// An expected record sequence: an OPL file the verifier prepared from
/// osmium's [`Policy::FULL`] decoding of the input.
#[derive(Clone)]
pub(super) struct Expected {
    pub path: PathBuf,
    /// The input's ways with at least one defined inline coordinate, as
    /// osmium decodes them (whatever the header declares).
    pub coordinate_ways: u64,
    /// The file's source, binding, counts and digest, when the input's
    /// decoding was sealed under a summary key.
    basis: Option<opl_compare::Basis>,
    summary: crate::reference_cache::Summary,
    stamp: Option<crate::run::Stamp>,
}

/// Expected OPL files for the life of the run. A check can keep its usual
/// artifact name by linking to the prepared file.
#[derive(Default)]
pub(crate) struct ReferenceStore {
    entries: std::collections::HashMap<(PathBuf, bool), Expected>,
}

/// The expected sequences the arms compare an output's elements with, each
/// built once per run for each input and coordinate policy.
///
/// No reference is a PBF osmium writes: a fresh osmium PBF write can fail
/// where pbfhogg succeeds (libosmium's writer packs records into blocks of
/// its own choosing, against the 32 MiB limit, not the input's), and its
/// plain writer drops `visible` without a history header. The expected file
/// is osmium's FULL OPL decoding of the input itself, so every field osmium
/// reads survives into it. Which ways carry coordinates is taken from that
/// decoding, never from the header's `LocationsOnWays`: libosmium decodes
/// the lat/lon arrays whatever the header says, and so does pbfhogg.
pub(super) struct References<'a> {
    outdir: &'a Path,
    input: &'a Input,
}

impl<'a> References<'a> {
    pub fn new(outdir: &'a Path, input: &'a Input) -> Self {
        Self { outdir, input }
    }

    /// The input's objects, each kind in input order and the kinds grouped
    /// nodes, ways, relations (a stable partition, the decoding itself on an
    /// input already grouped), every way-node coordinate written undefined
    /// (`n<id>xy`) when `strip`, as osmium decodes it otherwise.
    pub fn get(&self, harness: &mut Harness, strip: bool) -> Result<Expected, Error> {
        // Keep the original narration, which describes osmium's strict
        // order probe. A grouped decoding can still have out-of-order IDs.
        let stem = if strip {
            "expected-stripped"
        } else {
            "expected"
        };
        harness.msg(&format!(
            "--- reference: osmium's decoding of the input{}{} ---",
            if self.input.standard {
                ""
            } else {
                ", partitioned by kind"
            },
            if strip {
                ", way coordinates written undefined"
            } else {
                ""
            }
        ));
        let path = self.outdir.join(format!("{stem}.opl"));
        let cache_key = (self.input.path.clone(), strip);
        let expected = if let Some(cached) = harness.run.reencode_references.entries.get(&cache_key)
        {
            harness.run.revalidate()?;
            std::fs::hard_link(&cached.path, &path)?;
            Expected {
                path,
                ..cached.clone()
            }
        } else {
            let expected = self.build(harness, path, strip)?;
            harness
                .run
                .reencode_references
                .entries
                .insert(cache_key, expected.clone());
            expected
        };
        harness.msg(&format!(
            "  {} ways carry a defined coordinate in osmium's decoding",
            expected.coordinate_ways
        ));
        Ok(expected)
    }

    fn build(&self, harness: &mut Harness, path: PathBuf, strip: bool) -> Result<Expected, Error> {
        let (written, key) = opl_compare::decoded(
            harness,
            (&self.input.path, Origin::Other),
            Policy::FULL,
            |h, opl| {
                let written = partition(opl, &path, strip)?;
                if written.reused {
                    // The decoding store can evict on release. The check
                    // keeps a name for the same inode for the rest of the run.
                    std::fs::hard_link(opl, &path)?;
                }
                Ok((written, opl_compare::decoding_key(h, opl)))
            },
        )?;
        // A decoding sealed under a summary key lets an output's stored
        // summary decide its comparison; one with no key never does.
        let basis = key.map(|key| opl_compare::Basis {
            source: self.input.path.clone(),
            key,
            counts: written.counts,
            digest: written.digest.clone(),
        });
        Ok(Expected {
            stamp: crate::run::stamp(&path).ok(),
            path,
            coordinate_ways: written.coordinate_ways,
            basis,
            summary: crate::reference_cache::Summary {
                ordered: written.ordered,
                counts: written.counts,
                digest: written.digest,
            },
        })
    }
}

/// What [`partition`] wrote.
struct Partitioned {
    /// Ways of the decoding with at least one defined coordinate.
    coordinate_ways: u64,
    /// Objects per kind in the resulting sequence.
    counts: [u64; 3],
    /// XXH3-128 of the resulting sequence, newlines included, spelled as a
    /// decoding summary's digest is.
    digest: String,
    reused: bool,
    ordered: bool,
}

/// The expected file's writer, digesting every byte it is handed.
struct HashingWriter {
    file: File,
    hasher: xxhash_rust::xxh3::Xxh3,
}

impl Write for HashingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.file.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// One continuous byte range in the decoding or the projected-way spool.
struct Part {
    projected: bool,
    start: u64,
    end: u64,
}

/// A stable partition in one record scan. Unchanged byte ranges can be
/// copied as-is if the sequence needs rewriting. Until EOF, a late kind
/// regression or a projected way can still force a rewrite. Keep byte
/// ranges instead of buffering the decoding or spooling the already grouped
/// case. The rewrite copies those ranges without parsing them again.
fn partition(decoding: &Path, out: &Path, strip: bool) -> Result<Partitioned, Error> {
    let file = File::open(decoding)
        .map_err(|e| Error::Failed(format!("cannot open {}: {e}", decoding.display())))?;
    let mut reader = BufReader::with_capacity(crate::tunables::OPL_READ_BUFFER, file);
    let mut plan = PartitionPlan::default();
    let spool_path = out.with_extension("projected-ways");
    let mut spool = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        plan.push(decoding, &line, strip, &spool_path, &mut spool)?;
    }
    let reused = plan.grouped && spool.is_none();
    let digest = if reused {
        format!("{:032x}", plan.original.digest128())
    } else {
        let digest = write_partition(out, reader.into_inner(), spool, &plan.parts)?;
        if plan.projected_end != 0 {
            std::fs::remove_file(spool_path)?;
        }
        digest
    };
    Ok(Partitioned {
        coordinate_ways: plan.coordinate_ways,
        counts: plan.counts,
        digest,
        reused,
        ordered: plan.ordered,
    })
}

struct PartitionPlan {
    parts: [Vec<Part>; 3],
    counts: [u64; 3],
    original: xxhash_rust::xxh3::Xxh3,
    coordinate_ways: u64,
    source_end: u64,
    projected_end: u64,
    grouped: bool,
    last_kind: usize,
    last_ids: [Option<i64>; 3],
    ordered: bool,
}

impl Default for PartitionPlan {
    fn default() -> Self {
        Self {
            parts: Default::default(),
            counts: [0; 3],
            original: xxhash_rust::xxh3::Xxh3::new(),
            coordinate_ways: 0,
            source_end: 0,
            projected_end: 0,
            grouped: true,
            last_kind: 0,
            last_ids: [None; 3],
            ordered: true,
        }
    }
}

impl PartitionPlan {
    fn push(
        &mut self,
        decoding: &Path,
        line: &[u8],
        strip: bool,
        spool_path: &Path,
        spool: &mut Option<BufWriter<File>>,
    ) -> Result<(), Error> {
        let body = line.strip_suffix(b"\n").ok_or_else(|| {
            Error::Failed(format!(
                "{}: a last line without its newline (truncated?)",
                decoding.display()
            ))
        })?;
        let (kind, id) = opl_compare::parse_key(body)
            .map_err(|why| Error::Failed(format!("{}: {why}", decoding.display())))?;
        let source_start = self.source_end;
        self.source_end += line.len() as u64;
        self.counts[kind] += 1;
        if let Some(last) = self.last_ids[kind].replace(id) {
            self.ordered &= opl_compare::rank((kind, last)) < opl_compare::rank((kind, id));
        }
        self.grouped &= self.last_kind <= kind;
        self.last_kind = kind;
        let mut part = Part {
            projected: false,
            start: source_start,
            end: self.source_end,
        };
        if kind == 1 {
            let (projected, defined) = strip_way_coordinates(body);
            self.coordinate_ways += u64::from(defined > 0);
            if strip && projected.as_slice() != body {
                part = self.project_way(&projected, spool_path, spool)?;
            }
        }
        if self.grouped && spool.is_none() {
            self.original.update(line);
        }
        let parts = &mut self.parts[kind];
        if let Some(last) = parts.last_mut()
            && last.projected == part.projected
            && last.end == part.start
        {
            last.end = part.end;
        } else {
            parts.push(part);
        }
        Ok(())
    }

    fn project_way(
        &mut self,
        line: &[u8],
        spool_path: &Path,
        spool: &mut Option<BufWriter<File>>,
    ) -> Result<Part, Error> {
        if spool.is_none() {
            let file = File::options()
                .read(true)
                .write(true)
                .create_new(true)
                .open(spool_path)?;
            *spool = Some(BufWriter::new(file));
        }
        let writer = spool
            .as_mut()
            .ok_or_else(|| Error::tool("way projection lost its spool"))?;
        writer.write_all(line)?;
        writer.write_all(b"\n")?;
        let start = self.projected_end;
        self.projected_end += line.len() as u64 + 1;
        Ok(Part {
            projected: true,
            start,
            end: self.projected_end,
        })
    }
}

fn write_partition(
    out: &Path,
    mut source: File,
    spool: Option<BufWriter<File>>,
    parts: &[Vec<Part>; 3],
) -> Result<String, Error> {
    let mut projected = spool
        .map(BufWriter::into_inner)
        .transpose()
        .map_err(|e| Error::Failed(format!("cannot write {}: {}", out.display(), e.error())))?;
    let mut writer = BufWriter::new(HashingWriter {
        file: File::create(out)?,
        hasher: xxhash_rust::xxh3::Xxh3::new(),
    });
    for part in parts.iter().flatten() {
        let reader = if part.projected {
            projected
                .as_mut()
                .ok_or_else(|| Error::tool("way projection lost its spool"))?
        } else {
            &mut source
        };
        reader.seek(SeekFrom::Start(part.start))?;
        let len = part.end - part.start;
        if std::io::copy(&mut reader.take(len), &mut writer)? != len {
            return Err(Error::tool(
                "OPL partition source ended inside a record range",
            ));
        }
    }
    let hashing = writer
        .into_inner()
        .map_err(|e| Error::Failed(format!("cannot write {}: {}", out.display(), e.error())))?;
    Ok(format!("{:032x}", hashing.hasher.digest128()))
}

/// An OPL way line (osmium's writer with `locations_on_ways=true`) with every
/// node ref of its `N` field written undefined, and how many refs defined a
/// coordinate. libosmium writes every ref as `n<id>x<lon>y<lat>`, or
/// `n<id>xy` when undefined; the ID is a signed decimal, so the first `x` is
/// the separator. Fields are split on the raw space: OPL escapes a space
/// inside a tag, user or role, and only the way's node list is prefixed `N`.
/// Undefined and absent coordinates print alike, so an output carrying
/// all-undefined arrays reads as stripped: the comparison sees decoded
/// coordinates, not the wire arrays.
pub(super) fn strip_way_coordinates(line: &[u8]) -> (Vec<u8>, usize) {
    let mut out = Vec::with_capacity(line.len());
    let mut defined = 0;
    for (i, field) in line.split(|&b| b == b' ').enumerate() {
        if i != 0 {
            out.push(b' ');
        }
        let Some(refs) = field.strip_prefix(b"N").filter(|_| i != 0) else {
            out.extend_from_slice(field);
            continue;
        };
        out.push(b'N');
        for (j, node) in refs.split(|&b| b == b',').enumerate() {
            if j != 0 {
                out.push(b',');
            }
            match node.iter().position(|&b| b == b'x') {
                Some(x) => {
                    out.extend_from_slice(&node[..x]);
                    out.extend_from_slice(b"xy");
                    if &node[x + 1..] != b"y" {
                        defined += 1;
                    }
                }
                None => out.extend_from_slice(node),
            }
        }
    }
    (out, defined)
}

// ---------------------------------------------------------------------------
// The element comparison
// ---------------------------------------------------------------------------

/// One element comparison of a reference against an output.
pub(super) struct Compared {
    pub merged: Merged,
    pub listing: PathBuf,
}

impl Compared {
    pub fn identical(&self) -> bool {
        self.merged.identical()
    }
}

/// The expected records and the output's OPL records (metadata and inline
/// way coordinates) walked in file order
/// ([`opl_compare::compare_sequence_with_opl`]), the listing of every
/// unmatched or differing record at `<stem>.diff.opl`, `-` expected and `+`
/// output.
pub(super) fn compare(
    harness: &mut Harness,
    outdir: &Path,
    stem: &str,
    expected: &Expected,
    output: &Path,
    prediction: Prediction<'_>,
) -> Result<Compared, Error> {
    let listing = outdir.join(format!("{stem}.diff.opl"));
    let audited = matches!(&prediction, Prediction::Survivors(_))
        && harness
            .run
            .frames
            .get(crate::run::stamp(output).ok())
            .is_some();
    let equality = matches!(&prediction, Prediction::Unchanged)
        && expected.stamp.is_some()
        && crate::run::stamp(&expected.path).ok() == expected.stamp;
    let merged = opl_compare::compare_sequence_with_opl(
        harness,
        &expected.path,
        expected.basis.as_ref(),
        // Every output compared here is a pbfhogg run's.
        (output, Origin::Pbfhogg),
        &listing,
        Policy::FULL,
        equality.then_some(&expected.summary),
        |staged| {
            let Some(basis) = &expected.basis else {
                return Ok(None);
            };
            let whole = (basis.counts, basis.digest.as_str());
            match prediction {
                Prediction::Unchanged => opl_compare::unchanged(basis)(staged),
                Prediction::Swap(at) => predict(&expected.path, whole, staged, |kind, _, index| {
                    Ok(Some(if at[kind] == Some(index) {
                        Edit::SwapNext
                    } else {
                        Edit::Keep
                    }))
                }),
                Prediction::Survivors(_) if audited => Ok(None),
                Prediction::Survivors(decoded) => {
                    let Ok(mut survivors) = FrameIds::open(output, false) else {
                        return Ok(None);
                    };
                    let candidate = predict(&expected.path, whole, staged, |kind, id, _| {
                        Ok(match survivors.peek() {
                            Ok(next) if next == Some((kind, id)) => {
                                survivors.pending = None;
                                Some(Edit::Keep)
                            }
                            Ok(_) => Some(Edit::Remove),
                            // The candidate cannot be formed: the merge decides.
                            Err(_) => None,
                        })
                    });
                    *decoded = Some(survivors);
                    candidate
                }
            }
        },
    )?;
    Ok(Compared { merged, listing })
}

/// What stream a case predicts its output decodes to. A matching stored
/// summary can decide every prediction. A fresh summary can also decide
/// whole-stream equality. A difference goes to the merge.
pub(super) enum Prediction<'a> {
    /// The expected file itself.
    Unchanged,
    /// The expected file with, per kind holding `Some(i)`, the kind's
    /// records `i` and `i + 1` (from 0, in walk order) swapped: what an
    /// unsort mode documents.
    Swap([Option<u64>; 3]),
    /// The expected file less every record whose identity the output's
    /// blobs do not hold next, read by the verifier's own frame decoder:
    /// the candidate for a drop, whose selection is pbfhogg's.
    Survivors(&'a mut Option<FrameIds>),
}

/// What [`predict`]'s walker does with one expected record.
enum Edit {
    Keep,
    /// Listed `-`, not in the predicted stream.
    Remove,
    /// Held and written after the next record, which must be of its kind;
    /// listed `-` and `+` together, as the merge lists an adjacent swap of
    /// `a < q` (it reaches `a` against `q`, lists `-a`, pairs `q`, then
    /// meets `a` below the next expected record and lists `+a`).
    SwapNext,
}

/// Walk the expected file once, digesting the stream `decide` edits it into
/// and writing to `listing` exactly the lines the sequence merge of the
/// expected file against that stream would. The unedited bytes are also
/// digested and held to `basis`, so the walk read the file the basis
/// describes. `decide` sees each record's kind, ID and position within its
/// kind; it returning `None`, a malformed line, or a swap whose partner is
/// missing or of another kind, makes the whole prediction `None`.
fn predict(
    expected: &Path,
    (counts, digest): ([u64; 3], &str),
    listing: &Path,
    mut decide: impl FnMut(usize, i64, u64) -> Result<Option<Edit>, Error>,
) -> Result<Option<opl_compare::Predicted>, Error> {
    let mut reader =
        BufReader::with_capacity(crate::tunables::OPL_READ_BUFFER, File::open(expected)?);
    let mut out = std::io::BufWriter::new(File::create(listing)?);
    let mut whole = xxhash_rust::xxh3::Xxh3::new();
    let mut stream = xxhash_rust::xxh3::Xxh3::new();
    let mut first = [0u64; 3];
    let mut second = [0u64; 3];
    let mut merged = opl_compare::Merged::default();
    let mut held: Option<(usize, Vec<u8>)> = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        whole.update(&line);
        let Some(body) = line.strip_suffix(b"\n") else {
            return Ok(None);
        };
        let Ok((kind, id)) = opl_compare::parse_key(body) else {
            return Ok(None);
        };
        let index = first[kind];
        first[kind] += 1;
        if let Some((held_kind, moved)) = held.take() {
            if held_kind != kind {
                return Ok(None);
            }
            stream.update(&line);
            stream.update(&moved);
            second[kind] += 2;
            merged.same += 1;
            merged.first_only += 1;
            merged.second_only += 1;
            out.write_all(b"-")?;
            out.write_all(&moved)?;
            out.write_all(b"+")?;
            out.write_all(&moved)?;
            continue;
        }
        match decide(kind, id, index)? {
            None => return Ok(None),
            Some(Edit::Keep) => {
                stream.update(&line);
                second[kind] += 1;
                merged.same += 1;
            }
            Some(Edit::Remove) => {
                merged.first_only += 1;
                out.write_all(b"-")?;
                out.write_all(&line)?;
            }
            Some(Edit::SwapNext) => held = Some((kind, line.clone())),
        }
    }
    if held.is_some() || first != counts || format!("{:032x}", whole.digest128()) != digest {
        return Ok(None);
    }
    out.into_inner()
        .map_err(|e| Error::Failed(format!("cannot write {}: {}", listing.display(), e.error())))?
        .sync_all()?;
    merged.first = first;
    merged.second = second;
    Ok(Some(opl_compare::Predicted {
        counts: second,
        digest: format!("{:032x}", stream.digest128()),
        merged,
    }))
}

/// The identities an output's blobs hold, in file order, read by the
/// verifier's frame decoder one blob at a time, with one held for a peek.
pub(super) struct FrameIds {
    stamp: Option<crate::run::Stamp>,
    walk: pbf_frames::TruthWalk,
    truth_error: Option<Error>,
    blob: std::vec::IntoIter<(usize, i64)>,
    pending: Option<(usize, i64)>,
}

impl FrameIds {
    fn open(path: &Path, report_header: bool) -> Result<Self, Error> {
        Ok(Self {
            stamp: crate::run::stamp(path).ok(),
            walk: pbf_frames::TruthWalk::open(path, Origin::Pbfhogg, report_header)?,
            truth_error: None,
            blob: Vec::new().into_iter(),
            pending: None,
        })
    }

    /// The next identity, left in place; `None` past the last blob.
    fn peek(&mut self) -> Result<Option<(usize, i64)>, ()> {
        while self.pending.is_none() {
            if let Some(next) = self.blob.next() {
                self.pending = Some(next);
                break;
            }
            let (blob, ids) = match self.walk.next(true) {
                Ok(Some(decoded)) => decoded,
                Ok(None) => return Ok(None),
                Err(error) => {
                    self.truth_error = Some(error);
                    return Err(());
                }
            };
            match ids {
                Ok(ids) => {
                    self.walk.observe(blob, &ids);
                    self.blob = ids.ids.into_iter();
                }
                Err(error) => {
                    if blob.claim.is_some() {
                        self.truth_error = Some(error);
                    } else {
                        self.walk
                            .observe(blob, &pbf_frames::BlockObservation::default());
                    }
                    return Err(());
                }
            }
        }
        Ok(self.pending)
    }

    fn finish(self) -> Result<FrameAudit, Error> {
        if let Some(error) = self.truth_error {
            return Err(error);
        }
        self.walk.finish()
    }
}

/// [`compare`] of two pbfhogg outputs, both decoded by osmium.
pub(super) fn compare_outputs(
    harness: &mut Harness,
    outdir: &Path,
    stem: &str,
    first: &Path,
    second: &Path,
) -> Result<Compared, Error> {
    let listing = outdir.join(format!("{stem}.diff.opl"));
    let merged = opl_compare::compare_sequence(
        harness,
        (first, Origin::Pbfhogg),
        (second, Origin::Pbfhogg),
        &listing,
        Policy::FULL,
        |_| {},
    )?;
    Ok(Compared { merged, listing })
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

/// One identity of a comparison listing: its kind, ID and the raw object
/// line on each side, at least one present.
#[derive(Debug)]
pub(super) struct Group {
    pub kind: usize,
    pub id: i64,
    pub minus: Option<Vec<u8>>,
    pub plus: Option<Vec<u8>>,
}

/// The groups of a [`compare`] listing (`-` reference, `+` output), in
/// object order, each object kept as the bytes osmium wrote. Strict: a line
/// that is not `-` or `+` and an object, an identity out of order, a side
/// twice for one identity, or a last line without its newline is an error.
/// [`finish`](Self::finish) holds the line counts to the merge's.
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
            reader: BufReader::with_capacity(crate::tunables::OPL_READ_BUFFER, file),
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
            "comparison listing {} line {}: {why}",
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

    /// After the last group: the listing's `-` lines must number the
    /// first-only and differing records and its `+` lines the second-only
    /// and differing ones, or it was not read whole.
    pub fn finish(self, merged: &Merged) -> Result<(), Error> {
        if self.minus != merged.first_only + merged.differs
            || self.plus != merged.second_only + merged.differs
        {
            return Err(Error::Failed(format!(
                "comparison listing {} has {} - and {} + lines against the merge {merged:?}",
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

/// An output's blobs compared, in order, with a predicted layout: the first
/// disagreement is kept and the rest ignored.
pub(super) struct LayoutCheck<'a> {
    claims: std::slice::Iter<'a, ClaimedBlob>,
    position: u64,
    mismatch: Option<String>,
}

impl LayoutCheck<'_> {
    /// The next output blob must claim `count` elements of `kind`.
    pub fn expect(&mut self, kind: usize, count: u64) -> Result<(), Error> {
        if self.mismatch.is_some() {
            return Ok(());
        }
        let at = self.position;
        self.position += 1;
        self.mismatch = match self.claims.next() {
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
    pub fn finish(mut self) -> Option<String> {
        if self.mismatch.is_none() && self.claims.next().is_some() {
            self.mismatch = Some(format!(
                "the output holds more than the {} predicted blocks",
                self.position
            ));
        }
        self.mismatch
    }
}

/// What one pass over an output's claims finds, per kind and overall.
pub(super) struct OutputBlobs {
    audit: Rc<FrameAudit>,
    pub kinds: [KindBlobs; 3],
    /// The first blob over the cap, or with no element.
    pub bad_size: Option<String>,
    /// A blob claiming a kind after a later kind's blob.
    pub kind_step_back: Option<String>,
}

impl OutputBlobs {
    pub fn layout(&self) -> LayoutCheck<'_> {
        LayoutCheck {
            claims: self.audit.claims.iter(),
            position: 0,
            mismatch: None,
        }
    }

    pub fn ids(&self, kind: usize, position: usize) -> &[(usize, i64)] {
        &self.audit.witnesses[kind][position]
    }
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
fn output_blobs(output: &Path, cap: u64, audit: Rc<FrameAudit>) -> Result<OutputBlobs, Error> {
    let mut out = OutputBlobs {
        audit,
        kinds: Default::default(),
        bad_size: None,
        kind_step_back: None,
    };
    let mut highest: Option<usize> = None;
    for &blob in &out.audit.claims {
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
    elements: &[(usize, i64)],
) -> Result<Vec<i64>, Error> {
    let (blob, claim) = located;
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
    Ok(elements.iter().map(|&(_, id)| id).collect())
}

/// Record the claims pass's cap and kind-order verdicts.
fn record_blobs(harness: &mut Harness, label: &str, blobs: &OutputBlobs) {
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

    #[test]
    fn frame_audits_follow_the_inode_and_expire_when_the_file_changes() {
        let scratch = pbfhogg_test_scratch::scratch();
        let input = scratch.path().join("source.pbf");
        let alias = scratch.path().join("alias.pbf");
        std::fs::write(&input, b"old").unwrap();
        std::fs::hard_link(&input, &alias).unwrap();

        let audit = Rc::new(FrameAudit::default());
        let key = crate::run::stamp(&input).unwrap();
        let store = FrameStore {
            entries: vec![(key, Rc::clone(&audit))],
        };
        let reused = store.get(crate::run::stamp(&alias).ok()).unwrap();
        assert!(Rc::ptr_eq(&audit, &reused));

        std::fs::write(&alias, b"new contents").unwrap();
        assert!(store.get(crate::run::stamp(&input).ok()).is_none());
    }

    /// Predict `edit` of `expected`, merge `expected` against `actual`, and
    /// require the two to agree on everything the judges read: the result,
    /// the listing bytes, the counts and the digest.
    fn agrees(
        expected: &str,
        actual: &str,
        decide: impl FnMut(usize, i64, u64) -> Result<Option<Edit>, Error>,
    ) -> opl_compare::Merged {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = scratch.path();
        let (e, a) = (dir.join("e.opl"), dir.join("a.opl"));
        let (pl, ml) = (dir.join("p.txt"), dir.join("m.txt"));
        std::fs::write(&e, expected).unwrap();
        std::fs::write(&a, actual).unwrap();
        let (_, counts, digest) = opl_compare::sequence_merge(&e, &e, &ml).unwrap();
        let p = predict(&e, (counts, &digest), &pl, decide)
            .unwrap()
            .unwrap();
        let (merged, counts, digest) = opl_compare::sequence_merge(&e, &a, &ml).unwrap();
        assert_eq!(p.merged, merged);
        assert_eq!((p.counts, p.digest), (counts, digest));
        assert_eq!(std::fs::read(&pl).unwrap(), std::fs::read(&ml).unwrap());
        merged
    }

    #[test]
    fn a_predicted_swap_or_drop_writes_what_the_merge_writes() {
        let e = "n1 v1\nn2 v1\nn3 v1\nw1 v1\nw2 v1\nr7 v1\n";
        // Each kind's swap at its own position, the way swap at its end and
        // the next kind's first record right after it.
        let at = [Some(1), Some(0), None];
        let m = agrees(
            e,
            "n1 v1\nn3 v1\nn2 v1\nw2 v1\nw1 v1\nr7 v1\n",
            |k, _, i| {
                Ok(Some(if at[k] == Some(i) {
                    Edit::SwapNext
                } else {
                    Edit::Keep
                }))
            },
        );
        assert_eq!((m.first_only, m.second_only, m.same), (2, 2, 4));
        let gone = [(0, 1), (0, 3), (2, 7)];
        let m = agrees(e, "n2 v1\nw1 v1\nw2 v1\n", |k, id, _| {
            Ok(Some(if gone.contains(&(k, id)) {
                Edit::Remove
            } else {
                Edit::Keep
            }))
        });
        assert_eq!((m.first_only, m.second_only, m.same), (3, 0, 3));
    }

    #[test]
    fn a_prediction_refuses_an_unpaired_swap_or_a_file_unlike_its_basis() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = scratch.path();
        let (e, l) = (dir.join("e.opl"), dir.join("l.txt"));
        std::fs::write(&e, "n1 v1\nw1 v1\n").unwrap();
        let (_, counts, digest) = opl_compare::sequence_merge(&e, &e, &l).unwrap();
        let swap_last_node =
            |k: usize, _, _| Ok(Some(if k == 0 { Edit::SwapNext } else { Edit::Keep }));
        assert!(
            predict(&e, (counts, &digest), &l, swap_last_node)
                .unwrap()
                .is_none()
        );
        let keep = |_, _, _| Ok(Some(Edit::Keep));
        assert!(predict(&e, (counts, "0"), &l, keep).unwrap().is_none());
        assert!(predict(&e, (counts, &digest), &l, keep).unwrap().is_some());
    }

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
    fn stripping_rewrites_only_way_node_coordinates() {
        let (out, defined) = strip_way_coordinates(
            b"w7 v2 dV c3 t2020-01-01T00:00:00Z i4 uN%20x Thighway=x Nn-1x1.5y-2.25,n2xy",
        );
        assert_eq!(defined, 1);
        assert_eq!(
            out,
            b"w7 v2 dV c3 t2020-01-01T00:00:00Z i4 uN%20x Thighway=x Nn-1xy,n2xy".to_vec()
        );
        assert_eq!(strip_way_coordinates(b"w1 v1 N"), (b"w1 v1 N".to_vec(), 0));
        assert_eq!(strip_way_coordinates(b"w1 v1 Nn1x0y0").1, 1);
        assert_eq!(strip_way_coordinates(b"w1 v1 Nn1xy").1, 0);
    }

    #[test]
    fn the_partition_groups_kinds_in_file_order_and_counts_coordinate_ways() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = scratch.path();
        let (decoding, out) = (dir.join("d.opl"), dir.join("e.opl"));
        std::fs::write(
            &decoding,
            "w5 v1 Nn1x1y1\nn2 v1\nr1 v1 M\nn1 v1\nw3 v1 Nn2xy\n",
        )
        .unwrap();
        let stripped = partition(&decoding, &out, true).unwrap();
        assert_eq!(stripped.coordinate_ways, 1);
        assert_eq!(stripped.counts, [2, 2, 1]);
        let written = std::fs::read(&out).unwrap();
        assert_eq!(
            written,
            b"n2 v1\nn1 v1\nw5 v1 Nn1xy\nw3 v1 Nn2xy\nr1 v1 M\n"
        );
        // The digest is of the bytes written, as a decoding summary's is.
        assert_eq!(
            stripped.digest,
            format!("{:032x}", xxhash_rust::xxh3::xxh3_128(&written))
        );
        assert_eq!(
            partition(&decoding, &out, false).unwrap().coordinate_ways,
            1
        );
        assert!(
            std::fs::read_to_string(&out)
                .unwrap()
                .contains("w5 v1 Nn1x1y1\n")
        );
        std::fs::write(&decoding, "n1 v1").unwrap();
        crate::test_support::refused(
            partition(&decoding, &out, false),
            "d.opl: a last line without its newline",
        );
    }

    #[test]
    fn grouped_records_use_the_decoding_even_with_out_of_order_ids() {
        let scratch = pbfhogg_test_scratch::scratch();
        let (decoding, out) = (scratch.path().join("d.opl"), scratch.path().join("e.opl"));
        let bytes = b"n2 v1\nn1 v1\nw3 v1 Nn2xy\nr1 v1 M\n";
        std::fs::write(&decoding, bytes).unwrap();

        let unchanged = partition(&decoding, &out, true).unwrap();
        assert!(unchanged.reused);
        assert!(!out.exists());
        assert_eq!(unchanged.counts, [2, 1, 1]);
        assert_eq!(unchanged.coordinate_ways, 0);
        assert_eq!(
            unchanged.digest,
            format!("{:032x}", xxhash_rust::xxh3::xxh3_128(bytes))
        );
    }

    #[test]
    fn listing_lines_read_side_kind_and_id_and_keep_the_object() {
        let (side, kind, id, body) = parse_line(b"-w-12 v1 dV Nn1x1y2").unwrap();
        assert_eq!((side, kind, id), (b'-', 1, -12));
        assert_eq!(body, b"w-12 v1 dV Nn1x1y2");
        crate::test_support::refused(parse_line(b" n1 v1"), "not a - or + line");
        crate::test_support::refused(
            parse_line(b"+c1 v1"),
            "an object that is not a node, way or relation",
        );
        crate::test_support::refused(parse_line(b"+nx v1"), "an object ID that does not read");
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
        let merged = Merged {
            first_only: 2,
            second_only: 1,
            ..Merged::default()
        };
        listing.finish(&merged).unwrap();
        std::fs::write(&path, "-n2 v1\n-n1 v1\n").unwrap();
        let mut listing = Listing::open(&path).unwrap();
        listing.next_group().unwrap();
        crate::test_support::refused(
            listing.next_group(),
            "l.opl line 2: an identity out of object order",
        );
        std::fs::write(&path, "-n2 v1").unwrap();
        crate::test_support::refused(
            Listing::open(&path).unwrap().next_group(),
            "l.opl line 1: no terminating newline",
        );
    }
}
