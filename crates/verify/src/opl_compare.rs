//! The element comparison every check that once asked `pbfhogg diff` uses:
//! osmium decodes each side to OPL (`osmium cat -f opl,...`), and the
//! verifier merges the two record streams itself.
//!
//! The merge reads both OPL files once, line by line, keyed by kind and ID
//! in osmium's object order (kind, then ID zero, negative IDs by magnitude,
//! positive IDs by magnitude). [`compare`] requires each stream to ascend
//! strictly in that order, so the pass also certifies, as osmium decodes
//! them, that neither side repeats an object or is out of order: an
//! identical duplicate cannot hide as common. [`compare_sequence`] walks the
//! two streams the same way without that requirement, as osmium diff walks
//! its inputs. A record is the whole OPL line, compared byte for byte. Every identity in only one stream, or in both with unequal
//! lines, is handed to the caller as a [`Group`] and written to a listing
//! (`-` first, `+` second), so classification is the caller's.
//!
//! This judges osmium-decoded records under a [`Policy`], not wire fields:
//! libosmium fills absent metadata with its defaults on both sides,
//! timestamps are whole seconds, coordinates are at 1e-7 degrees, and its
//! OPL writer stops a string at an embedded NUL. OPL escapes the separators
//! (`=`, `,`, space, `@`, `%`), so tag boundaries are part of what is
//! compared, unlike osmium diff's checksum. With `metadata` off the OPL
//! carries no version, timestamp, changeset, user or visibility, which is
//! sound only for non-history files of visible objects; with `locations`
//! off it carries no inline way coordinate.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::harness::Harness;
use crate::pbf_frames::Origin;
use crate::reference::ToolRun;

/// A file osmium decodes and who wrote it, which decides how a failed
/// decode is judged ([`crate::reference::RunArg::Input`]).
pub(crate) type Operand<'a> = (&'a Path, Origin);
use crate::reference_cache::{Key, Summary};

/// What a comparison reads from each object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Policy {
    /// Version, timestamp, changeset, user and visibility.
    pub metadata: bool,
    /// Inline way-node coordinates (`locations_on_ways=true`).
    pub locations: bool,
}

impl Policy {
    /// Every field osmium decodes.
    pub(crate) const FULL: Self = Self {
        metadata: true,
        locations: true,
    };
    /// Metadata, no inline coordinates.
    pub(crate) const NO_LOCATIONS: Self = Self {
        metadata: true,
        locations: false,
    };
    /// Element content only: tags, coordinates of nodes, refs and members.
    pub(crate) const CONTENT: Self = Self {
        metadata: false,
        locations: false,
    };

    /// The `osmium cat -f` value that decodes under this policy.
    pub(crate) fn format(self) -> String {
        let mut format = String::from("opl");
        if !self.metadata {
            format.push_str(",add_metadata=false");
        }
        if self.locations {
            format.push_str(",locations_on_ways=true");
        }
        format
    }

    /// The comparison of two files: inline coordinates are read when both
    /// headers declare `LocationsOnWays`. A command may drop the declaration
    /// with its coordinates (a re-encode warns `header.way_metadata_loss`),
    /// and a reference tool may write none; where coordinates are a
    /// command's promise, the check holds the declaration itself. Deciding
    /// from the input's declaration instead would fail every documented
    /// drop (sort rewriting a way blob re-encodes it without coordinates),
    /// so a lost declaration is caught by `Harness::judge_way_locations`,
    /// which holds each output's declaration to its command's header rule.
    /// Each file comes with its origin, which decides what a header the
    /// verifier's reader refuses says ([`crate::pbf_frames::Origin`]).
    pub(crate) fn between(
        a: (&Path, crate::pbf_frames::Origin),
        b: (&Path, crate::pbf_frames::Origin),
    ) -> Result<Self, Error> {
        let declares = |(p, origin): (&Path, crate::pbf_frames::Origin)| -> Result<bool, Error> {
            Ok(crate::pbf_frames::header(p, origin)?.optional_feature("LocationsOnWays"))
        };
        Ok(if declares(a)? && declares(b)? {
            Self::FULL
        } else {
            Self::NO_LOCATIONS
        })
    }
}

/// How an identity differs between the two sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    FirstOnly,
    SecondOnly,
    Differs,
}

/// One identity the two sides do not hold alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Group {
    pub class: Class,
    /// Node 0, way 1, relation 2.
    pub kind: usize,
    pub id: i64,
}

/// One identity both sides hold, alike or not, with both encoded records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pair<'a> {
    /// Node 0, way 1, relation 2.
    pub kind: usize,
    pub id: i64,
    pub first: &'a [u8],
    pub second: &'a [u8],
}

/// What a merge counted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Merged {
    /// Objects per kind on each side.
    pub first: [u64; 3],
    pub second: [u64; 3],
    pub same: u64,
    pub first_only: u64,
    pub second_only: u64,
    pub differs: u64,
}

impl Merged {
    pub(crate) fn identical(&self) -> bool {
        self.first_only == 0 && self.second_only == 0 && self.differs == 0
    }
}

/// The step label of every merge walk, timed apart from the decodes it reads.
const MERGE_STEP: &str = "opl merge";

/// What one decoding was made from: the file's identity stamp (device,
/// inode, size, mtime, the run's trusted input identity) and its name's
/// format text. osmium picks its reader from the name, so the same inode
/// under `x.osc` and `x.osc.gz`, or `pbf` and `data`, is not one source:
/// the text is everything from the first dot, or the whole name when it has
/// none. Keyed by stamp, not content digest: no fresh output is hashed, and
/// cache-placed reference artifacts, hard links of one inode in several
/// checks, still meet. Byte-identical files at distinct inodes decode apart.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Source {
    stamp: (u64, u64, u64, i64, i64),
    format: std::ffi::OsString,
}

impl Source {
    fn of(pbf: &Path) -> Result<Self, Error> {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(pbf)
            .map_err(|e| Error::Environment(format!("{}: {e}", pbf.display())))?;
        let name = pbf.file_name().unwrap_or_default().as_bytes();
        let format = name
            .iter()
            .position(|&b| b == b'.')
            .map_or(name, |i| &name[i..]);
        Ok(Self {
            stamp: (
                meta.dev(),
                meta.ino(),
                meta.size(),
                meta.mtime(),
                meta.mtime_nsec(),
            ),
            format: std::ffi::OsStr::from_bytes(format).to_os_string(),
        })
    }
}

struct Entry {
    source: Source,
    policy: Policy,
    path: PathBuf,
    bytes: u64,
    /// Calls currently reading it; a pinned entry is never evicted.
    pins: u32,
    used: u64,
    /// Read by a call that failed: the error names this file, so it is
    /// never evicted or discarded and stays in the kept run directory.
    kept: bool,
    /// The persistent summary key this decoding was made under: planned
    /// before its launch and observed again, unchanged, once the group
    /// settled. `None` when no identity bound it; such a decoding serves
    /// merges but never a stored summary.
    key: Option<Key>,
}

impl Entry {
    fn idle(&self) -> bool {
        self.pins == 0 && !self.kept
    }
}

/// The run's OPL decodings, reused across calls and checks while their
/// source's stamp holds, plus each source's order and counts once some
/// decoding of it was read whole for its keys. Every policy writes the
/// object key (kind and ID) the same, so a scan read under one policy
/// answers order and counts under any; record bytes are never projected
/// across policies. Retention is bounded: idle (unpinned) decodings are
/// evicted least recently used down to `tunables::DECODING_BUDGET` bytes
/// after every release, and before every new decode down to the budget less
/// that decode's estimate (all idle ones when the estimate alone exceeds
/// it). The budget bounds retained idle bytes, not the filesystem's
/// headroom for the decode in flight or for other work.
#[derive(Default)]
pub(crate) struct Decodings {
    entries: Vec<Entry>,
    scans: Vec<(Source, Scan)>,
    tick: u64,
    made: u64,
}

impl Decodings {
    fn idle_bytes(&self) -> u64 {
        self.entries
            .iter()
            .filter(|e| e.idle())
            .map(|e| e.bytes)
            .sum()
    }

    /// Evict idle entries, least recently used first, until the idle bytes
    /// are at most `limit`.
    fn trim(&mut self, limit: u64) -> Result<(), Error> {
        while self.idle_bytes() > limit {
            let Some(oldest) = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.idle())
                .min_by_key(|(_, e)| e.used)
                .map(|(i, _)| i)
            else {
                break;
            };
            self.evict(oldest)?;
        }
        Ok(())
    }

    fn evict(&mut self, index: usize) -> Result<(), Error> {
        // Unlinked first: an entry whose file stays is still accounted.
        std::fs::remove_file(&self.entries[index].path)?;
        self.entries.swap_remove(index);
        Ok(())
    }

    /// Unpin `paths`, marking them kept when `keep`. An entry larger than
    /// the budget goes as soon as it is idle, before the least recently used
    /// trim, so it never flushes smaller entries only to be evicted itself.
    fn release(&mut self, paths: &[PathBuf], keep: bool) -> Result<(), Error> {
        for path in paths {
            if let Some(e) = self.entries.iter_mut().find(|e| &e.path == path) {
                e.pins = e.pins.saturating_sub(1);
                e.kept |= keep;
            }
        }
        while let Some(i) = self
            .entries
            .iter()
            .position(|e| e.idle() && e.bytes > crate::tunables::DECODING_BUDGET)
        {
            self.evict(i)?;
        }
        self.trim(crate::tunables::DECODING_BUDGET)
    }

    /// Record `scan` as the order and counts of `source`.
    fn record(&mut self, source: Source, scan: Scan) {
        if !self.scans.iter().any(|(s, _)| *s == source) {
            self.scans.push((source, scan));
        }
    }

    /// The persistent key of the decoding at `path`, if it has one.
    fn key_of(&self, path: &Path) -> Option<Key> {
        self.entries
            .iter()
            .find(|e| e.path == path)
            .and_then(|e| e.key.clone())
    }

    /// Record `scan` as the order and counts of the source `path` decodes.
    fn publish(&mut self, path: &Path, scan: Scan) {
        let Some(source) = self
            .entries
            .iter()
            .find(|e| e.path == path)
            .map(|e| e.source.clone())
        else {
            return;
        };
        if !self.scans.iter().any(|(s, _)| *s == source) {
            self.scans.push((source, scan));
        }
    }

    /// Remove every retained decoding at the end of the run, except those a
    /// failed call kept as evidence.
    pub(crate) fn discard(&mut self) {
        for entry in self.entries.drain(..).filter(|e| !e.kept) {
            if let Err(e) = std::fs::remove_file(&entry.path) {
                crate::output::error(&format!(
                    "cannot remove the decoding {}: {e}",
                    entry.path.display()
                ));
            }
        }
    }
}

/// `osmium cat` of `pbf` to OPL under `policy`, as a pinned store entry:
/// the run's retained decoding of the same source and policy when there is
/// one, else a fresh decode. A reuse is still an observation of `pbf`: it
/// is watched, every watched input revalidated and the lifecycle
/// checkpointed, as a launch would.
/// The pinned retained decoding of `pbf` under `policy`, if any. The input
/// is validated before the pin is taken, so a failed validation leaves no
/// pin behind.
fn acquire_hit(
    harness: &mut Harness,
    pbf: &Path,
    policy: Policy,
) -> Result<Option<PathBuf>, Error> {
    // Watched before the stat that picks the entry: a replacement between
    // the two fails the revalidation below instead of being blessed.
    harness.run.watch(pbf)?;
    let source = Source::of(pbf)?;
    let Some(path) = harness
        .run
        .decodings
        .entries
        .iter()
        .find(|e| e.source == source && e.policy == policy)
        .map(|e| e.path.clone())
    else {
        return Ok(None);
    };
    harness.run.revalidate()?;
    harness.checkpoint()?;
    harness.run.decodings.tick += 1;
    let tick = harness.run.decodings.tick;
    if let Some(e) = harness
        .run
        .decodings
        .entries
        .iter_mut()
        .find(|e| e.path == path)
    {
        e.pins += 1;
        e.used = tick;
    }
    harness.msg(&format!(
        "  {}: reusing the run's OPL decoding {}",
        pbf.display(),
        path.display()
    ));
    Ok(Some(path))
}

/// The decode a persistent summary key describes: `osmium cat` of `pbf`
/// under `policy`, its output named `decoding.opl`. The launch writes a
/// numbered name in the run's store instead; osmium picks the OPL writer
/// from `-f`, never from the output's name, so both decode the same bytes.
/// It is never launched and the origin is not keyed, so the origin it
/// declares judges nothing.
fn key_run(pbf: &Path, policy: Policy) -> ToolRun {
    ToolRun::osmium("opl-compare.1")
        .arg("cat")
        .input(pbf, Origin::Other)
        .args(&["-f", &policy.format(), "-o"])
        .output(Path::new("decoding.opl"))
        .arg("--overwrite")
        .uncached()
}

/// Give each settled decoding at `opls` its planned key when the identity,
/// observed again without a reprobe, still gives the same full key text.
fn seal_keys(
    harness: &mut Harness,
    planned: Option<Vec<Key>>,
    key_runs: &[ToolRun],
    opls: &[&Path],
) -> Result<(), Error> {
    let Some(planned) = planned else {
        return Ok(());
    };
    let Some(again) = harness.summary_keys(key_runs, false)? else {
        return Ok(());
    };
    for ((planned, again), opl) in planned.into_iter().zip(again).zip(opls) {
        if planned == again
            && let Some(e) = harness
                .run
                .decodings
                .entries
                .iter_mut()
                .find(|e| e.path == *opl)
        {
            e.key = Some(planned);
        }
    }
    Ok(())
}

/// Publish `summary` for the decoding at `decoding`, under the key its
/// bytes were made under; a decoding with no key publishes nothing.
fn publish_summary(harness: &mut Harness, decoding: &Path, summary: &Summary) -> Result<(), Error> {
    match harness.run.decodings.key_of(decoding) {
        Some(key) => harness.publish_summary(&key, summary),
        None => Ok(()),
    }
}

/// The summary of the decoding of `pbf` under `policy`, made (or reused
/// from the run's store) and scanned whole, when its key is `key`: the key
/// the caller looked up under, so both summaries of a comparison name one
/// tool binding. `None` when the decoding has another key or none, or when
/// the scan meets a read or parse error; the full merge then reports it.
fn scanned(
    harness: &mut Harness,
    pbf: Operand<'_>,
    policy: Policy,
    key: &Key,
) -> Result<Option<Summary>, Error> {
    decoded(harness, pbf, policy, |harness, path| {
        if harness.run.decodings.key_of(path).as_ref() != Some(key) {
            return Ok(None);
        }
        let Ok((ordered, counts, summary)) = harness.timed("opl summary scan", || scan_order(path))
        else {
            return Ok(None);
        };
        harness
            .run
            .decodings
            .publish(path, Scan { ordered, counts });
        publish_summary(harness, path, &summary)?;
        Ok(Some(summary))
    })
}

/// A strict comparison of `first` and `second` under `policy` decided from
/// their decodings' stored summaries, without a merge: both summaries under
/// one tool binding, both strictly ordered, with equal counts and equal
/// whole-stream digests. With one summary stored, the other side alone is
/// decoded and scanned. Any other outcome is `None` and the caller merges as
/// before, so a difference, a disorder or a decoding error is always the
/// merge's to report. A pass writes an empty `listing`.
fn by_summaries(
    harness: &mut Harness,
    first_side: Operand<'_>,
    second_side: Operand<'_>,
    listing: &Path,
    policy: Policy,
) -> Result<Option<Merged>, Error> {
    let (first, second) = (first_side.0, second_side.0);
    harness.run.watch(first)?;
    harness.run.watch(second)?;
    let runs = [key_run(first, policy), key_run(second, policy)];
    let Some(keys) = harness.summary_keys(&runs, true)? else {
        return Ok(None);
    };
    let (a, b) = match (harness.summary(&keys[0]), harness.summary(&keys[1])) {
        (Some(a), Some(b)) => (a, b),
        (Some(a), None) if a.ordered => match scanned(harness, second_side, policy, &keys[1])? {
            Some(b) => (a, b),
            None => return Ok(None),
        },
        (None, Some(b)) if b.ordered => match scanned(harness, first_side, policy, &keys[0])? {
            Some(a) => (a, b),
            None => return Ok(None),
        },
        _ => return Ok(None),
    };
    if !(a.ordered && b.ordered && a == b) {
        return Ok(None);
    }
    harness.run.revalidate()?;
    harness.checkpoint()?;
    File::create(listing)?;
    for (pbf, s) in [(first, &a), (second, &b)] {
        let source = Source::of(pbf)?;
        harness.run.decodings.record(
            source,
            Scan {
                ordered: true,
                counts: s.counts,
            },
        );
    }
    let merged = Merged {
        first: a.counts,
        second: b.counts,
        same: a.counts.iter().sum(),
        ..Merged::default()
    };
    harness.msg(&format!(
        "  OPL comparison {}: decided by equal decoding summaries (xxh3-128 {}), same={}",
        listing.display(),
        a.digest,
        merged.same
    ));
    Ok(Some(merged))
}

/// Decode every operand of `sides` whose slot is still empty, as one launch
/// group: the osmium decodes run together, so a comparison's decode wall is
/// its slowest operand's, not their sum. Operands of one source and policy
/// (one file twice, or hard links of one inode) share a single decode, with
/// a pin per slot. Every operand's path is watched, aliases included, and
/// each must still have the stamp its decode was planned under once the
/// group has settled. Each decode is registered and its slot filled as soon
/// as it is judged, so an earlier operand's decoding is held, and kept as
/// evidence, when a later one fails.
fn acquire_missing(
    harness: &mut Harness,
    sides: &[(Operand<'_>, Policy)],
    slots: &mut [Option<PathBuf>],
) -> Result<(), Error> {
    // Each unique missing (source, policy), with the slots it fills. One
    // source is one file, written by one producer, so its operands share
    // the origin the first of them names.
    let mut plan: Vec<(Source, Policy, Operand<'_>, Vec<usize>)> = Vec::new();
    // Each missing operand's own planned source. The watch comes first, so
    // a replacement between it and the stat makes the next revalidation
    // fail rather than bless the new file.
    let mut planned: Vec<(usize, Source)> = Vec::new();
    for (i, (((pbf, origin), policy), slot)) in sides.iter().zip(slots.iter()).enumerate() {
        if slot.is_some() {
            continue;
        }
        harness.run.watch(pbf)?;
        let source = Source::of(pbf)?;
        planned.push((i, source.clone()));
        match plan
            .iter_mut()
            .find(|(s, p, _, _)| *s == source && p == policy)
        {
            Some((_, _, _, fills)) => fills.push(i),
            None => plan.push((source, *policy, (pbf, *origin), vec![i])),
        }
    }
    if plan.is_empty() {
        return Ok(());
    }
    let estimate = plan.iter().fold(0_u64, |sum, (source, ..)| {
        sum.saturating_add(
            source
                .stamp
                .2
                .saturating_mul(crate::tunables::OPL_EXPANSION),
        )
    });
    harness
        .run
        .decodings
        .trim(crate::tunables::DECODING_BUDGET.saturating_sub(estimate))?;
    // Each decode's persistent key, planned before the launch under one
    // identity observation; it is kept below only if the same observation,
    // made again once the group settled, still gives it.
    let key_runs: Vec<ToolRun> = plan
        .iter()
        .map(|(_, policy, (pbf, _), _)| key_run(pbf, *policy))
        .collect();
    let planned_keys = harness.summary_keys(&key_runs, true)?;
    let dir = harness.run.dir.join("decodings");
    std::fs::create_dir_all(&dir)?;
    let mut runs = Vec::with_capacity(plan.len());
    for (_, policy, (pbf, origin), _) in &plan {
        harness.run.decodings.made += 1;
        let opl = dir.join(format!("{}.opl", harness.run.decodings.made));
        let run = ToolRun::osmium("opl-compare.1")
            .arg("cat")
            .input(pbf, *origin)
            .args(&["-f", &policy.format(), "-o"])
            .output(&opl)
            .arg("--overwrite")
            .uncached();
        runs.push((run, opl.with_extension("stdout"), opl));
    }
    let group: Vec<(&ToolRun, &Path)> = runs
        .iter()
        .map(|(run, sink, _)| (run, sink.as_path()))
        .collect();
    let captured = harness.reference_group(&group)?;
    for ((source, policy, (pbf, _), fills), (captured, (_, sink, opl))) in
        plan.into_iter().zip(captured.into_iter().zip(runs.iter()))
    {
        let captured = captured?;
        harness.require_success(&captured, "osmium cat to OPL")?;
        // A successful decode's stdout is no evidence; a failed one's stays.
        std::fs::remove_file(sink)?;
        // The stamp the decode was keyed under must still be the file's.
        if Source::of(pbf)? != source {
            return Err(Error::Environment(format!(
                "{} changed while osmium decoded it",
                pbf.display()
            )));
        }
        let bytes = std::fs::metadata(opl)?.len();
        harness.run.decodings.tick += 1;
        let tick = harness.run.decodings.tick;
        harness.run.decodings.entries.push(Entry {
            source,
            policy,
            path: opl.clone(),
            bytes,
            pins: u32::try_from(fills.len()).map_err(|e| Error::tool(e.to_string()))?,
            used: tick,
            kept: false,
            key: None,
        });
        for i in fills {
            slots[i] = Some(opl.clone());
        }
    }
    let opls: Vec<&Path> = runs.iter().map(|(_, _, opl)| opl.as_path()).collect();
    seal_keys(harness, planned_keys, &key_runs, &opls)?;
    // Aliases: every operand, not only the one osmium read, must still be
    // the source its slot's decoding was planned from.
    for (i, source) in planned {
        let pbf = sides[i].0.0;
        if Source::of(pbf)? != source {
            return Err(Error::Environment(format!(
                "{} changed while osmium decoded it",
                pbf.display()
            )));
        }
    }
    harness.run.revalidate()
}

/// Run `work` over pinned decodings of `sides` (each a file and its
/// policy), releasing them after. Retained decodings are pinned first, in
/// side order, and only then are the missing ones made, together as one
/// launch group, so no operand is evicted to make room for another. When an acquire or `work` fails, each
/// decoding acquired is kept: never evicted or discarded, so the path an
/// error names stays in the kept run directory, and the failure stays the
/// one raised. `work` returns whether its outcome, though `Ok`, is a failure
/// whose decodings are kept too.
fn with_decodings<T>(
    harness: &mut Harness,
    sides: &[(Operand<'_>, Policy)],
    work: impl FnOnce(&mut Harness, &[PathBuf]) -> Result<(T, bool), Error>,
) -> Result<T, Error> {
    let mut slots: Vec<Option<PathBuf>> = vec![None; sides.len()];
    let mut outcome = Ok(None);
    for (((pbf, _), policy), slot) in sides.iter().zip(&mut slots) {
        match acquire_hit(harness, pbf, *policy) {
            Ok(path) => *slot = path,
            Err(e) => {
                outcome = Err(e);
                break;
            }
        }
    }
    if outcome.is_ok()
        && let Err(e) = acquire_missing(harness, sides, &mut slots)
    {
        outcome = Err(e);
    }
    let held: Vec<PathBuf> = slots.iter().flatten().cloned().collect();
    // A stop during placement or staging cleanup is seen before the walk.
    if outcome.is_ok()
        && let Err(e) = harness.checkpoint()
    {
        outcome = Err(e);
    }
    if outcome.is_ok() {
        outcome = work(harness, &held).map(Some);
    }
    let keep = match &outcome {
        Ok(Some((_, failed))) => *failed,
        _ => true,
    };
    if keep {
        for path in &held {
            harness.msg(&format!("  decoding kept: {}", path.display()));
        }
    }
    let released = harness.run.decodings.release(&held, keep);
    if let (Err(cleanup), Err(_) | Ok(Some((_, true)))) = (&released, &outcome) {
        // The comparison's failure stays the one raised.
        harness.msg(&format!("  decoding cleanup failed: {cleanup}"));
    }
    let (value, _) = outcome?.ok_or_else(|| Error::tool("decodings lost their outcome"))?;
    released?;
    Ok(value)
}

/// `read` over the OPL decoding of `pbf` under `policy`, from the run's
/// store; kept on failure.
pub(crate) fn decoded<T>(
    harness: &mut Harness,
    pbf: Operand<'_>,
    policy: Policy,
    read: impl FnOnce(&mut Harness, &Path) -> Result<T, Error>,
) -> Result<T, Error> {
    with_decodings(harness, &[(pbf, policy)], |h, held| {
        Ok((read(h, &held[0])?, false))
    })
}

/// Compare `first` with `second` under `policy`: both decoded to OPL beside
/// `listing`, merged, every [`Group`] handed to `each` in object order and
/// written to `listing`. The decodings are the run's ([`Decodings`]); on an
/// error both are kept in the run's store for diagnosis. A stream out of strict order, an ID that is not a
/// signed 64-bit decimal or a line that is not an object is an error naming
/// the file.
pub(crate) fn compare(
    harness: &mut Harness,
    first: Operand<'_>,
    second: Operand<'_>,
    listing: &Path,
    policy: Policy,
    each: impl FnMut(Group),
) -> Result<Merged, Error> {
    if let Some(merged) = by_summaries(harness, first, second, listing, policy)? {
        return Ok(merged);
    }
    walk(harness, first, second, listing, policy, true, each, |_| {})
}

/// [`compare`] always by the full merge, never from stored summaries: for
/// a check whose subject is the merge itself (field-policies).
pub(crate) fn compare_merged(
    harness: &mut Harness,
    first: Operand<'_>,
    second: Operand<'_>,
    listing: &Path,
    policy: Policy,
    each: impl FnMut(Group),
) -> Result<Merged, Error> {
    walk(harness, first, second, listing, policy, true, each, |_| {})
}

/// [`compare`] for streams in any order: the merge walk advances the side
/// whose current key is lower and pairs equal keys, but no stream is held to
/// ascend. No first-only, second-only or differing record then means the two
/// record sequences are equal element by element, whatever their order (the
/// walk consumed one equal line from each side at every step); only
/// first-only records mean the second sequence is a subsequence of the
/// first. Neither holds for a key-ordered reading of the counts beyond that.
pub(crate) fn compare_sequence(
    harness: &mut Harness,
    first: Operand<'_>,
    second: Operand<'_>,
    listing: &Path,
    policy: Policy,
    each: impl FnMut(Group),
) -> Result<Merged, Error> {
    compare_sequence_paired(harness, first, second, listing, policy, each, |_| {})
}

/// [`compare_sequence`], also handing every paired identity, alike or not,
/// to `paired` in walk order with both records.
pub(crate) fn compare_sequence_paired(
    harness: &mut Harness,
    first: Operand<'_>,
    second: Operand<'_>,
    listing: &Path,
    policy: Policy,
    each: impl FnMut(Group),
    paired: impl FnMut(Pair<'_>),
) -> Result<Merged, Error> {
    walk(harness, first, second, listing, policy, false, each, paired)
}

/// [`compare`], also handing every paired identity, alike or not, to
/// `paired` in object order with both records. Never decided from plain
/// summaries: the caller's subject is the pairs. Also returns each side's
/// decoding key, when its decode was sealed under one (never under
/// `--fresh-reference`), so the caller may publish its own reading of the
/// decodings it walked ([`Harness::publish_summary_of`]).
pub(crate) fn compare_paired(
    harness: &mut Harness,
    first: Operand<'_>,
    second: Operand<'_>,
    listing: &Path,
    policy: Policy,
    each: impl FnMut(Group),
    paired: impl FnMut(Pair<'_>),
) -> Result<(Merged, [Option<Key>; 2]), Error> {
    let mut keys = [None, None];
    let merged = walk_keyed(
        harness,
        first,
        second,
        listing,
        policy,
        true,
        each,
        paired,
        Some(&mut keys),
    )?;
    Ok((merged, keys))
}

/// The persistent keys the decodings of `files` under `policy` would be
/// sealed under, planned now under one tool-identity observation, each file
/// watched first: what a check looks its own stored readings up under before
/// deciding whether to decode at all. `None` under `--fresh-reference` or
/// when the identity is unavailable.
pub(crate) fn planned_keys(
    harness: &mut Harness,
    files: &[&Path],
    policy: Policy,
) -> Result<Option<Vec<Key>>, Error> {
    for file in files {
        harness.run.watch(file)?;
    }
    let runs: Vec<ToolRun> = files.iter().map(|f| key_run(f, policy)).collect();
    harness.summary_keys(&runs, true)
}

/// One side's order as a judged walk read it: whether its objects strictly
/// ascend in object order, and its object count per kind, both over the
/// whole stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Scan {
    pub ordered: bool,
    pub counts: [u64; 3],
}

/// What a judged walk ([`compare_judged`]) found.
#[derive(Debug)]
pub(crate) struct Walked {
    /// Each side's order and counts. An `Err` is a read or parse error met
    /// while draining that side after the other stepped down: that side has
    /// no whole-stream judgment.
    pub scans: [Result<Scan, Error>; 2],
    /// The comparison, or on a step-down the error the strict walk raises at
    /// the cursor that stepped down, exactly as [`compare`] raises it. The
    /// listing is then a prefix and both OPL decodings are kept.
    pub merged: Result<Merged, Error>,
    /// A failure to flush the prefix listing after a step-down, for the
    /// detail; the cursor error stays the one raised.
    pub listing_note: Option<String>,
}

/// [`compare`], also judging each side's order from the same decodings, so
/// no caller pays a separate decode for an order verdict. A clean merge
/// gives both sides ordered with the merge's counts. At the first step-down
/// the walk saves the error [`compare`] would raise at that cursor, stops
/// pairing (no further `each`, no further listing lines), and drains each
/// side to its end for its order and counts. A read or parse error before
/// any step-down is raised as [`compare`] raises it, with no judgment.
pub(crate) fn compare_judged(
    harness: &mut Harness,
    first: Operand<'_>,
    second: Operand<'_>,
    listing: &Path,
    policy: Policy,
    each: impl FnMut(Group),
) -> Result<Walked, Error> {
    if let Some(merged) = by_summaries(harness, first, second, listing, policy)? {
        let scan = |counts| {
            Ok(Scan {
                ordered: true,
                counts,
            })
        };
        return Ok(Walked {
            scans: [scan(merged.first), scan(merged.second)],
            merged: Ok(merged),
            listing_note: None,
        });
    }
    let sides = [(first, policy), (second, policy)];
    with_decodings(harness, &sides, |harness, held| {
        let (a, b) = (&held[0], &held[1]);
        let (walked, summaries) =
            harness.timed(MERGE_STEP, || merge_judged(a, b, listing, each))?;
        for (path, scan) in [a, b].into_iter().zip(&walked.scans) {
            if let Ok(scan) = scan {
                harness.run.decodings.publish(path, *scan);
            }
        }
        for (path, summary) in [a, b].into_iter().zip(&summaries) {
            if let Some(summary) = summary {
                publish_summary(harness, path, summary)?;
            }
        }
        if let Ok(merged) = &walked.merged {
            log_merge(harness, listing, merged);
        }
        let failed = walked.merged.is_err();
        Ok((walked, failed))
    })
}

fn log_merge(harness: &mut Harness, listing: &Path, merged: &Merged) {
    harness.msg(&format!(
        "  OPL merge {}: same={} first-only={} second-only={} differs={}",
        listing.display(),
        merged.same,
        merged.first_only,
        merged.second_only,
        merged.differs
    ));
}

#[allow(clippy::too_many_arguments)]
fn walk(
    harness: &mut Harness,
    first: Operand<'_>,
    second: Operand<'_>,
    listing: &Path,
    policy: Policy,
    strict: bool,
    each: impl FnMut(Group),
    paired: impl FnMut(Pair<'_>),
) -> Result<Merged, Error> {
    walk_keyed(
        harness, first, second, listing, policy, strict, each, paired, None,
    )
}

/// [`walk`], also filling `keys` with each decoding's sealed key.
#[allow(clippy::too_many_arguments)]
fn walk_keyed(
    harness: &mut Harness,
    first: Operand<'_>,
    second: Operand<'_>,
    listing: &Path,
    policy: Policy,
    strict: bool,
    each: impl FnMut(Group),
    paired: impl FnMut(Pair<'_>),
    keys: Option<&mut [Option<Key>; 2]>,
) -> Result<Merged, Error> {
    let sides = [(first, policy), (second, policy)];
    // The listing holds every differing record from both sides, so a
    // completed merge keeps no decoding beside it, even when they differ.
    with_decodings(harness, &sides, |harness, held| {
        let (a, b) = (&held[0], &held[1]);
        let (merged, summaries) =
            harness.timed(MERGE_STEP, || merge(a, b, listing, strict, each, paired))?;
        if let Some(keys) = keys {
            *keys = [
                harness.run.decodings.key_of(a),
                harness.run.decodings.key_of(b),
            ];
        }
        for (path, summary) in [a, b].into_iter().zip(&summaries) {
            publish_summary(harness, path, summary)?;
        }
        if strict {
            // A completed strict merge read both sides whole, ascending.
            for (path, counts) in [(a, merged.first), (b, merged.second)] {
                harness.run.decodings.publish(
                    path,
                    Scan {
                        ordered: true,
                        counts,
                    },
                );
            }
        }
        log_merge(harness, listing, &merged);
        Ok((merged, false))
    })
}

/// Whether `pbf`'s objects strictly ascend in osmium's object order as
/// osmium decodes them, and its object count per kind: the run's recorded
/// scan of `pbf` when some decoding of it was already read whole for its
/// keys (under any policy, since every policy writes the key alike), else a
/// key scan of a retained decoding under any policy, else of a new one with
/// no metadata and no inline coordinates; on failure that decoding is kept.
/// `osmium fileinfo -e` cannot stand in: it refuses a file whose ways lack
/// their nodes (an invalid-location geometry error), which every way-only or
/// filtered output is.
pub(crate) fn order(harness: &mut Harness, pbf: Operand<'_>) -> Result<(bool, [u64; 3]), Error> {
    order_as(harness, pbf, Policy::CONTENT)
}

/// [`order`], decoding under `cold` when no scan, stored summary or
/// retained decoding of `pbf` answers: for a caller that decodes `pbf` under
/// `cold` next, which then reuses the run's decoding while it is retained.
pub(crate) fn order_as(
    harness: &mut Harness,
    operand: Operand<'_>,
    cold: Policy,
) -> Result<(bool, [u64; 3]), Error> {
    let pbf = operand.0;
    // Watched before the stat that picks a scan: a replacement between the
    // two fails the revalidation instead of being blessed.
    harness.run.watch(pbf)?;
    let source = Source::of(pbf)?;
    if let Some(scan) = harness
        .run
        .decodings
        .scans
        .iter()
        .find(|(s, _)| *s == source)
        .map(|(_, scan)| *scan)
    {
        harness.run.revalidate()?;
        harness.checkpoint()?;
        harness.msg(&format!(
            "  {}: order and counts from the run's earlier reading of its decoding",
            pbf.display()
        ));
        return Ok((scan.ordered, scan.counts));
    }
    // A stored summary of any policy: every policy writes the object keys
    // alike, so a successful whole decoding under one answers order and
    // counts under all.
    let runs = [Policy::CONTENT, Policy::NO_LOCATIONS, Policy::FULL].map(|p| key_run(pbf, p));
    if let Some(keys) = harness.summary_keys(&runs, true)?
        && let Some(stored) = keys.iter().find_map(|k| harness.summary(k))
    {
        harness.run.revalidate()?;
        harness.checkpoint()?;
        let scan = Scan {
            ordered: stored.ordered,
            counts: stored.counts,
        };
        harness.run.decodings.record(source, scan);
        harness.msg(&format!(
            "  {}: order and counts from a stored summary of its decoding",
            pbf.display()
        ));
        return Ok((scan.ordered, scan.counts));
    }
    let policy = harness
        .run
        .decodings
        .entries
        .iter()
        .find(|e| e.source == source)
        .map_or(cold, |e| e.policy);
    decoded(harness, operand, policy, |harness, path| {
        let (ordered, counts, summary) = harness.timed("opl order scan", || scan_order(path))?;
        harness
            .run
            .decodings
            .publish(path, Scan { ordered, counts });
        publish_summary(harness, path, &summary)?;
        Ok((ordered, counts))
    })
}

/// The key scan of [`order`] over one OPL file, with its summary.
fn scan_order(opl: &Path) -> Result<(bool, [u64; 3], Summary), Error> {
    let mut stream = Stream::open(opl)?;
    while stream.read_record()?.is_some() {}
    let summary = stream.summary();
    Ok((summary.ordered, stream.counts, summary))
}

/// What `osmium derive-changes` from `old` to `new` gives once applied back
/// to `old`, under [`Policy::CONTENT`], written to `expected` as OPL: new's
/// records plus every old record osmium writes no delete for. Its merge
/// (`command_derive_changes.cpp`) skips the delete of an old-only object
/// when the new stream's current object has the same ID, comparing IDs
/// without kinds, so an old-only identity is retained, unchanged, exactly
/// when the next new identity in object order (paired or new-only, none at
/// the end of new) has its ID and a later kind. Every old-only identity
/// before that new one is tested on its own: old `n7 n8 w7` against a next
/// new `r7` retains `n7` and `w7` and deletes `n8`. Version and timestamp
/// cannot move the cursor for an old-only identity, since every new identity
/// below it is consumed first and every one above sorts after it whatever
/// its version. Returns how many records were retained; the decodings come
/// from the run's store and are kept there on failure.
pub(crate) fn osmium_derive_roundtrip(
    harness: &mut Harness,
    old: Operand<'_>,
    new: Operand<'_>,
    expected: &Path,
) -> Result<u64, Error> {
    let sides = [(old, Policy::CONTENT), (new, Policy::CONTENT)];
    with_decodings(harness, &sides, |harness, held| {
        let retained = harness.timed("opl derive model", || {
            retained_by_osmium_derive(&held[0], &held[1], expected)
        })?;
        Ok((retained, false))
    })
}

/// The walk of [`osmium_derive_roundtrip`] over two strictly ordered OPL
/// files.
fn retained_by_osmium_derive(old: &Path, new: &Path, expected: &Path) -> Result<u64, Error> {
    let mut a = Stream::open(old)?;
    let mut b = Stream::open(new)?;
    let mut out = BufWriter::new(File::create(expected)?);
    // An old-only record is decided by the new record current beside it,
    // the next new one in object order, which is written after it.
    let mut retained = 0;
    let mut ka = a.next()?;
    let mut kb = b.next()?;
    loop {
        match (ka, kb) {
            (None, None) => break,
            (Some(x), y) if y.is_none_or(|y| rank(x) < rank(y)) => {
                if y.is_some_and(|y| y.1 == x.1) {
                    out.write_all(&a.line)?;
                    out.write_all(b"\n")?;
                    retained += 1;
                }
                ka = a.next()?;
            }
            (_, Some(y)) => {
                out.write_all(&b.line)?;
                out.write_all(b"\n")?;
                if ka == Some(y) {
                    ka = a.next()?;
                }
                kb = b.next()?;
            }
            (Some(_), None) => return Err(Error::tool("derive model lost its key")),
        }
    }
    out.flush()?;
    Ok(retained)
}

/// [`compare`] of `pbf`, decoded under `policy`, with an OPL file already
/// written (`expected`); the decoding is the run's, kept on failure.
pub(crate) fn compare_with_opl(
    harness: &mut Harness,
    pbf: Operand<'_>,
    expected: &Path,
    listing: &Path,
    policy: Policy,
) -> Result<Merged, Error> {
    compare_with_opl_each(harness, pbf, expected, listing, policy, |_| {})
}

/// [`compare_with_opl`], handing every [`Group`] to `each` in object order.
pub(crate) fn compare_with_opl_each(
    harness: &mut Harness,
    pbf: Operand<'_>,
    expected: &Path,
    listing: &Path,
    policy: Policy,
    each: impl FnMut(Group),
) -> Result<Merged, Error> {
    decoded(harness, pbf, policy, |harness, a| {
        let (merged, [summary, _]) = harness.timed(MERGE_STEP, || {
            merge(a, expected, listing, true, each, |_| {})
        })?;
        publish_summary(harness, a, &summary)?;
        harness.run.decodings.publish(
            a,
            Scan {
                ordered: true,
                counts: merged.first,
            },
        );
        log_merge(harness, listing, &merged);
        Ok(merged)
    })
}

/// The persistent summary key the run's retained decoding at `path` was
/// sealed under, if it has one.
pub(crate) fn decoding_key(harness: &Harness, path: &Path) -> Option<Key> {
    harness.run.decodings.key_of(path)
}

/// What a verifier-written expected OPL file was made from and holds: the
/// PBF whose decoding it projects, that decoding's sealed summary key (so
/// one tool binding names both sides of a summary decision), and the
/// written file's object counts per kind and XXH3-128 of its whole byte
/// stream, newlines included.
#[derive(Debug, Clone)]
pub(crate) struct Basis {
    pub source: PathBuf,
    pub key: Key,
    pub counts: [u64; 3],
    pub digest: String,
}

/// The OPL stream a caller predicts an output decodes to, made by walking
/// the expected file a [`Basis`] describes: its per-kind counts and
/// XXH3-128, and the merge's result had that stream been merged against
/// the expected file, whose listing the prediction wrote.
#[derive(Debug, Clone)]
pub(crate) struct Predicted {
    pub counts: [u64; 3],
    pub digest: String,
    pub merged: Merged,
}

/// The prediction that the output decodes to the expected file itself:
/// its counts and digest, every record paired equal, the listing empty.
pub(crate) fn unchanged(basis: &Basis) -> impl FnOnce(&Path) -> Result<Option<Predicted>, Error> {
    let (counts, digest) = (basis.counts, basis.digest.clone());
    move |listing| {
        File::create(listing)?;
        Ok(Some(Predicted {
            counts,
            digest,
            merged: Merged {
                first: counts,
                second: counts,
                same: counts.iter().sum(),
                ..Merged::default()
            },
        }))
    }
}

/// A sequence walk of the expected file `basis` describes against `pbf`
/// decided from `pbf`'s stored decoding summary, with no decode and no
/// merge: the source's key, observed again with the output's in one tool
/// binding, must still be the one the expected file was made under, and
/// the output's stored counts and digest must equal those of the stream
/// `predict` names. `predict` runs only once that summary is found; it
/// writes the listing the merge would write for its stream to the staged
/// path it is handed, which is renamed over `listing` only after a match,
/// the revalidation and the checkpoint (a miss removes it), and returns
/// `None` when it cannot predict. Byte-identical OPL streams merge to the
/// same result, so a matching prediction's [`Merged`] and listing are the
/// merge's; XXH3-128 equality stands for byte equality under the same
/// accepted assumption as [`by_summaries`]. Any other outcome is `None`
/// and the caller decodes and merges. With `strict` the output's stored
/// summary must also be ordered, so a summary that contradicts byte
/// equality with a strictly ascending expected stream goes to the strict
/// merge instead.
fn sequence_by_summary(
    harness: &mut Harness,
    basis: &Basis,
    pbf: &Path,
    listing: &Path,
    policy: Policy,
    strict: bool,
    predict: impl FnOnce(&Path) -> Result<Option<Predicted>, Error>,
) -> Result<Option<Merged>, Error> {
    harness.run.watch(&basis.source)?;
    harness.run.watch(pbf)?;
    let runs = [key_run(&basis.source, policy), key_run(pbf, policy)];
    let Some(keys) = harness.summary_keys(&runs, true)? else {
        return Ok(None);
    };
    if keys[0] != basis.key {
        return Ok(None);
    }
    let Some(out) = harness.summary(&keys[1]) else {
        return Ok(None);
    };
    if strict && !out.ordered {
        return Ok(None);
    }
    let mut staged = listing.as_os_str().to_owned();
    staged.push(".predicted");
    let staged = PathBuf::from(staged);
    let predicted = match predict(&staged)? {
        Some(p) if p.counts == out.counts && p.digest == out.digest => p,
        _ => {
            match std::fs::remove_file(&staged) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            return Ok(None);
        }
    };
    harness.run.revalidate()?;
    harness.checkpoint()?;
    std::fs::rename(&staged, listing)?;
    harness.run.decodings.record(
        Source::of(pbf)?,
        Scan {
            ordered: out.ordered,
            counts: out.counts,
        },
    );
    let merged = predicted.merged;
    harness.msg(&format!(
        "  OPL comparison {}: decided by the predicted stream's digest and the output's stored \
         decoding summary (xxh3-128 {}), same={} first_only={} second_only={}",
        listing.display(),
        out.digest,
        merged.same,
        merged.first_only,
        merged.second_only
    ));
    Ok(Some(merged))
}

/// [`compare_sequence`] with the first side an OPL file already written
/// (`expected`) and the second `pbf` decoded under `policy`; the decoding is
/// the run's, kept on failure, and `expected` is kept. With `basis`, an
/// output whose stored summary matches the stream `predict` names is
/// decided without a decode ([`sequence_by_summary`]).
pub(crate) fn compare_sequence_with_opl(
    harness: &mut Harness,
    expected: &Path,
    basis: Option<&Basis>,
    pbf: Operand<'_>,
    listing: &Path,
    policy: Policy,
    predict: impl FnOnce(&Path) -> Result<Option<Predicted>, Error>,
) -> Result<Merged, Error> {
    if let Some(basis) = basis
        && let Some(merged) =
            sequence_by_summary(harness, basis, pbf.0, listing, policy, false, predict)?
    {
        return Ok(merged);
    }
    decoded(harness, pbf, policy, |harness, b| {
        let (merged, [_, summary]) = harness.timed(MERGE_STEP, || {
            merge(expected, b, listing, false, |_| {}, |_| {})
        })?;
        publish_summary(harness, b, &summary)?;
        log_merge(harness, listing, &merged);
        Ok(merged)
    })
}

/// A strict comparison of `pbf`, decoded under `policy`, with an expected
/// OPL stream that is never written: `projection` yields its bytes, and
/// `basis` describes them (counts and XXH3-128 of the whole stream, the
/// sealed key of the decoding it projects). The caller guarantees the
/// stream strictly ascends: then an output whose stored summary is ordered
/// and has the basis's counts and digest is byte-identical to it, so it is
/// decided without a decode ([`sequence_by_summary`], strict), and the
/// listing is written empty. Otherwise `pbf` is decoded and strictly merged
/// against `projection()`, `pbf` first; `name` labels the projected side in
/// errors and should name the evidence it is made from. The projection's
/// digest is never published as any summary.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compare_with_projection_each<R: std::io::Read>(
    harness: &mut Harness,
    pbf: Operand<'_>,
    basis: Option<&Basis>,
    name: &Path,
    projection: impl FnOnce() -> Result<R, Error>,
    listing: &Path,
    policy: Policy,
    each: impl FnMut(Group),
) -> Result<Merged, Error> {
    if let Some(basis) = basis
        && let Some(merged) = sequence_by_summary(
            harness,
            basis,
            pbf.0,
            listing,
            policy,
            true,
            unchanged(basis),
        )?
    {
        return Ok(merged);
    }
    decoded(harness, pbf, policy, |harness, a| {
        let first = Stream::open(a)?;
        let second = Stream::from_reader(projection()?, name);
        let (merged, [summary, _]) = harness.timed(MERGE_STEP, || {
            merge_streams(first, second, listing, true, each, |_| {})
        })?;
        publish_summary(harness, a, &summary)?;
        harness.run.decodings.publish(
            a,
            Scan {
                ordered: true,
                counts: merged.first,
            },
        );
        log_merge(harness, listing, &merged);
        Ok(merged)
    })
}

/// The file beneath a [`Stream`]'s buffer, digesting every byte as the
/// buffer fills: hashed in buffer-sized reads rather than per line, so a
/// stream read to its end has the digest of the whole file.
struct Hashing<R = File> {
    file: R,
    hasher: xxhash_rust::xxh3::Xxh3,
}

impl<R: std::io::Read> std::io::Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = std::io::Read::read(&mut self.file, buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

/// One OPL stream: its current record and the key it last held. Its bytes
/// come from a file, or from any reader that yields a whole OPL stream
/// (`path` then names what that reader projects, for messages).
struct Stream<R = File> {
    path: PathBuf,
    reader: BufReader<Hashing<R>>,
    line: Vec<u8>,
    number: u64,
    key: Option<(usize, i64)>,
    counts: [u64; 3],
    /// The line of the first record at or below its predecessor, once a
    /// strict read has met one.
    stepped: Option<u64>,
    /// Whether any record so far was at or below its predecessor, whatever
    /// the read's strictness; with the reader's digest, a stream read to its
    /// end yields its [`Summary`].
    seen: Option<(usize, i64)>,
    disordered: bool,
}

impl Stream {
    fn open(path: &Path) -> Result<Self, Error> {
        let file = File::open(path)
            .map_err(|e| Error::Failed(format!("cannot open {}: {e}", path.display())))?;
        Ok(Stream::from_reader(file, path))
    }
}

impl<R: std::io::Read> Stream<R> {
    fn from_reader(file: R, path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            reader: BufReader::with_capacity(
                crate::tunables::OPL_READ_BUFFER,
                Hashing {
                    file,
                    hasher: xxhash_rust::xxh3::Xxh3::new(),
                },
            ),
            line: Vec::new(),
            number: 0,
            key: None,
            counts: [0; 3],
            stepped: None,
            seen: None,
            disordered: false,
        }
    }

    /// The summary of a stream read to its end without error.
    fn summary(&self) -> Summary {
        Summary {
            ordered: !self.disordered,
            counts: self.counts,
            digest: format!("{:032x}", self.reader.get_ref().hasher.digest128()),
        }
    }

    fn error(&self, why: &str) -> Error {
        Error::Failed(format!(
            "OPL {} line {}: {why}",
            self.path.display(),
            self.number
        ))
    }

    /// Advance to the next record, requiring strict order; `None` at the
    /// end.
    fn next(&mut self) -> Result<Option<(usize, i64)>, Error> {
        let Some(key) = self.read_record()? else {
            return Ok(None);
        };
        if self.key.is_some_and(|last| rank(key) <= rank(last)) {
            self.stepped = Some(self.number);
            return Err(self.error(
                "an object at or below the previous one: the stream is not in strict kind and ID \
                 order",
            ));
        }
        self.key = Some(key);
        Ok(Some(key))
    }

    /// Advance to the next record, in [`next`](Self::next)'s order when
    /// `strict`, else whatever its order.
    fn advance(&mut self, strict: bool) -> Result<Option<(usize, i64)>, Error> {
        if strict {
            self.next()
        } else {
            self.read_record()
        }
    }

    /// Read the rest of the stream after a step-down, for its order and
    /// counts: the first step-down is kept, later ones only confirm it.
    fn drain(&mut self) -> Result<Scan, Error> {
        while let Some(key) = self.read_record()? {
            if self.stepped.is_none() && self.key.is_some_and(|last| rank(key) <= rank(last)) {
                self.stepped = Some(self.number);
            }
            self.key = Some(key);
        }
        Ok(Scan {
            ordered: self.stepped.is_none(),
            counts: self.counts,
        })
    }

    /// Advance to the next record and count it, whatever its order.
    fn read_record(&mut self) -> Result<Option<(usize, i64)>, Error> {
        self.line.clear();
        let read = self
            .reader
            .read_until(b'\n', &mut self.line)
            .map_err(|e| Error::Failed(format!("cannot read {}: {e}", self.path.display())))?;
        if read == 0 {
            return Ok(None);
        }
        self.number += 1;
        if self.line.pop() != Some(b'\n') {
            return Err(self.error("no terminating newline (truncated?)"));
        }
        let key = parse_key(&self.line).map_err(|why| self.error(why))?;
        self.counts[key.0] += 1;
        if self.seen.is_some_and(|last| rank(key) <= rank(last)) {
            self.disordered = true;
        }
        self.seen = Some(key);
        Ok(Some(key))
    }
}

/// osmium's object order of a kind and ID: kind, then ID zero, the
/// negative IDs by magnitude, the positive IDs by magnitude.
pub(crate) fn rank((kind, id): (usize, i64)) -> (usize, bool, u64) {
    (kind, id > 0, id.unsigned_abs())
}

/// The kind and signed ID an OPL object line opens with.
pub(crate) fn parse_key(line: &[u8]) -> Result<(usize, i64), &'static str> {
    let (&letter, rest) = line.split_first().ok_or("an empty line")?;
    let kind = match letter {
        b'n' => 0,
        b'w' => 1,
        b'r' => 2,
        _ => return Err("a line that is not a node, way or relation"),
    };
    let end = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
    let digits = &rest[..end];
    let magnitude = digits.strip_prefix(b"-").unwrap_or(digits);
    if magnitude.is_empty() || !magnitude.iter().all(u8::is_ascii_digit) {
        return Err("an object ID that is not a decimal");
    }
    let id = std::str::from_utf8(digits)
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or("an object ID out of range")?;
    Ok((kind, id))
}

/// Merge two OPL files (see the module documentation), writing each
/// differing identity's lines to `listing` and handing every paired
/// identity to `paired`; `strict` holds each stream to strict order.
fn merge(
    first: &Path,
    second: &Path,
    listing: &Path,
    strict: bool,
    each: impl FnMut(Group),
    paired: impl FnMut(Pair<'_>),
) -> Result<(Merged, [Summary; 2]), Error> {
    merge_streams(
        Stream::open(first)?,
        Stream::open(second)?,
        listing,
        strict,
        each,
        paired,
    )
}

/// The non-strict sequence [`merge`] of two OPL files, for tests that hold
/// a prediction to what the merge itself writes and counts: the merge's
/// result and the second side's summary.
#[cfg(test)]
pub(crate) fn sequence_merge(
    first: &Path,
    second: &Path,
    listing: &Path,
) -> Result<(Merged, [u64; 3], String), Error> {
    let (merged, [_, summary]) = merge(first, second, listing, false, |_| {}, |_| {})?;
    Ok((merged, summary.counts, summary.digest))
}

/// [`merge`] over two open streams.
fn merge_streams<A: std::io::Read, B: std::io::Read>(
    mut a: Stream<A>,
    mut b: Stream<B>,
    listing: &Path,
    strict: bool,
    each: impl FnMut(Group),
    paired: impl FnMut(Pair<'_>),
) -> Result<(Merged, [Summary; 2]), Error> {
    let mut out = BufWriter::new(File::create(listing)?);
    let mut merged = Merged::default();
    pair_streams(&mut a, &mut b, &mut out, strict, &mut merged, each, paired)?;
    out.flush()?;
    merged.first = a.counts;
    merged.second = b.counts;
    // A completed merge read both streams to their ends.
    Ok((merged, [a.summary(), b.summary()]))
}

/// The strict [`merge`] of [`compare_judged`]: a step-down is a judgment of
/// both sides, its cursor error saved, rather than an early return.
fn merge_judged(
    first: &Path,
    second: &Path,
    listing: &Path,
    each: impl FnMut(Group),
) -> Result<(Walked, [Option<Summary>; 2]), Error> {
    let mut a = Stream::open(first)?;
    let mut b = Stream::open(second)?;
    let mut out = BufWriter::new(File::create(listing)?);
    let mut merged = Merged::default();
    match pair_streams(&mut a, &mut b, &mut out, true, &mut merged, each, |_| {}) {
        Ok(()) => {
            out.flush()?;
            merged.first = a.counts;
            merged.second = b.counts;
            let scan = |counts| {
                Ok(Scan {
                    ordered: true,
                    counts,
                })
            };
            Ok((
                Walked {
                    scans: [scan(a.counts), scan(b.counts)],
                    merged: Ok(merged),
                    listing_note: None,
                },
                [Some(a.summary()), Some(b.summary())],
            ))
        }
        // Only a strict read that stepped down sets `stepped`, and it
        // returns at once, so this error is that cursor's.
        Err(e) if a.stepped.is_some() || b.stepped.is_some() => {
            let listing_note = out.flush().err().map(|f| {
                format!(
                    "  the prefix listing {} could not be flushed: {f}",
                    listing.display()
                )
            });
            let scans = [a.drain(), b.drain()];
            // A side drained whole has a summary, disordered or not.
            let summaries = [
                scans[0].is_ok().then(|| a.summary()),
                scans[1].is_ok().then(|| b.summary()),
            ];
            Ok((
                Walked {
                    scans,
                    merged: Err(e),
                    listing_note,
                },
                summaries,
            ))
        }
        Err(e) => Err(e),
    }
}

/// The pairing loop of [`merge`] over two open streams.
fn pair_streams<A: std::io::Read, B: std::io::Read>(
    a: &mut Stream<A>,
    b: &mut Stream<B>,
    out: &mut BufWriter<File>,
    strict: bool,
    merged: &mut Merged,
    mut each: impl FnMut(Group),
    mut paired: impl FnMut(Pair<'_>),
) -> Result<(), Error> {
    let mut ka = a.advance(strict)?;
    let mut kb = b.advance(strict)?;
    loop {
        let class = match (ka, kb) {
            (None, None) => break,
            (Some(x), Some(y)) if x == y => {
                paired(Pair {
                    kind: x.0,
                    id: x.1,
                    first: &a.line,
                    second: &b.line,
                });
                if a.line == b.line {
                    merged.same += 1;
                    ka = a.advance(strict)?;
                    kb = b.advance(strict)?;
                    continue;
                }
                Class::Differs
            }
            (Some(x), Some(y)) if rank(x) < rank(y) => Class::FirstOnly,
            (Some(_), None) => Class::FirstOnly,
            _ => Class::SecondOnly,
        };
        let (kind, id) = match class {
            Class::SecondOnly => kb,
            _ => ka,
        }
        .ok_or_else(|| Error::tool("merge lost its key"))?;
        if class != Class::SecondOnly {
            out.write_all(b"-")?;
            out.write_all(&a.line)?;
            out.write_all(b"\n")?;
        }
        if class != Class::FirstOnly {
            out.write_all(b"+")?;
            out.write_all(&b.line)?;
            out.write_all(b"\n")?;
        }
        each(Group { class, kind, id });
        match class {
            Class::FirstOnly => {
                merged.first_only += 1;
                ka = a.advance(strict)?;
            }
            Class::SecondOnly => {
                merged.second_only += 1;
                kb = b.advance(strict)?;
            }
            Class::Differs => {
                merged.differs += 1;
                ka = a.advance(strict)?;
                kb = b.advance(strict)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::unwrap_in_result)]
    use super::*;

    fn run(a: &str, b: &str) -> Result<(Merged, Vec<Group>, String), Error> {
        walk_test(a, b, true)
    }

    fn walk_test(a: &str, b: &str, strict: bool) -> Result<(Merged, Vec<Group>, String), Error> {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = scratch.path();
        let (pa, pb, pl) = (dir.join("a.opl"), dir.join("b.opl"), dir.join("l.txt"));
        std::fs::write(&pa, a).unwrap();
        std::fs::write(&pb, b).unwrap();
        let mut groups = Vec::new();
        let (merged, _) = merge(&pa, &pb, &pl, strict, |g| groups.push(g), |_| {})?;
        Ok((merged, groups, std::fs::read_to_string(&pl).unwrap()))
    }

    fn summaries(a: &str, b: &str, strict: bool) -> [Summary; 2] {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = scratch.path();
        let (pa, pb, pl) = (dir.join("a.opl"), dir.join("b.opl"), dir.join("l.txt"));
        std::fs::write(&pa, a).unwrap();
        std::fs::write(&pb, b).unwrap();
        merge(&pa, &pb, &pl, strict, |_| {}, |_| {}).unwrap().1
    }

    #[test]
    fn a_summary_digests_the_whole_stream_and_judges_its_order() {
        let text = "n1 v1 Ta=bc x1 y1\nw5 v1 T Nn1\n";
        let [a, b] = summaries(text, text, true);
        assert_eq!(a, b);
        assert!(a.ordered);
        assert_eq!(a.counts, [1, 1, 0]);
        assert_eq!(
            a.digest,
            format!("{:032x}", xxhash_rust::xxh3::xxh3_128(text.as_bytes()))
        );
        // Records the merge calls different give different digests.
        let [a, b] = summaries("n1 v1 Ta=bc x1 y1\n", "n1 v1 Tab=c x1 y1\n", true);
        assert!(a.digest != b.digest);
        // A sequence walk reads a disordered stream whole and says so.
        let [a, _] = summaries("n2 v1\nn1 v1\n", "n2 v1\nn1 v1\n", false);
        assert!(!a.ordered);
        assert_eq!(a.counts, [2, 0, 0]);
    }

    #[test]
    fn identical_streams_are_identical_and_counted() {
        let text = "n1 v1 Ta=b x1 y1\nn2 v1 T x1 y2\nw5 v1 T Nn1,n2\nr7 v1 T Mw5@\n";
        let (merged, groups, listing) = run(text, text).unwrap();
        assert!(merged.identical());
        assert_eq!(merged.same, 4);
        assert_eq!(merged.first, [2, 1, 1]);
        assert!(groups.is_empty());
        assert!(listing.is_empty());
    }

    #[test]
    fn a_shifted_tag_boundary_differs() {
        // osmium diff's checksum feeds tags without lengths, so these two
        // collide there; the OPL records do not.
        let (merged, groups, _) = run("n1 v1 Ta=bc x1 y1\n", "n1 v1 Tab=c x1 y1\n").unwrap();
        assert_eq!(merged.differs, 1);
        assert_eq!(
            groups,
            [Group {
                class: Class::Differs,
                kind: 0,
                id: 1
            }]
        );
    }

    #[test]
    fn one_sided_identities_are_classified_in_object_order() {
        let (merged, groups, listing) =
            run("n1 v1\nn3 v1\nw2 v1\n", "n2 v1\nn3 v1\nr1 v1\n").unwrap();
        assert_eq!(
            (merged.first_only, merged.second_only, merged.same),
            (2, 2, 1)
        );
        let classes: Vec<_> = groups.iter().map(|g| (g.class, g.kind, g.id)).collect();
        assert_eq!(
            classes,
            [
                (Class::FirstOnly, 0, 1),
                (Class::SecondOnly, 0, 2),
                (Class::FirstOnly, 1, 2),
                (Class::SecondOnly, 2, 1),
            ]
        );
        assert_eq!(listing, "-n1 v1\n+n2 v1\n-w2 v1\n+r1 v1\n");
    }

    #[test]
    fn repeats_disorder_and_garbage_are_errors() {
        let ok = "n1 v1\n";
        // An identical duplicate on both sides cannot pass as common.
        assert!(run("n1 v1\nn1 v1\n", "n1 v1\nn1 v1\n").is_err());
        assert!(run("n2 v1\nn1 v1\n", ok).is_err());
        assert!(run("w1 v1\nn5 v1\n", ok).is_err());
        assert!(run(ok, "n- v1\n").is_err());
        assert!(run(ok, "c1 k1\n").is_err());
        assert!(run(ok, "n1 v1").is_err());
        assert!(
            run("n0 v1\nn1 v1\n", "n0 v1\nn1 v1\n")
                .unwrap()
                .0
                .identical()
        );
    }

    fn judged(a: &str, b: &str) -> Result<Walked, Error> {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = scratch.path();
        let (pa, pb, pl) = (dir.join("a.opl"), dir.join("b.opl"), dir.join("l.txt"));
        std::fs::write(&pa, a).unwrap();
        std::fs::write(&pb, b).unwrap();
        merge_judged(&pa, &pb, &pl, |_| {}).map(|(walked, _)| walked)
    }

    #[test]
    fn a_judged_walk_judges_both_sides_and_keeps_the_cursor_error() {
        let ordered = "n1 v1\nn2 v1\nw1 v1\n";
        let walked = judged(ordered, ordered).unwrap();
        let scan = Scan {
            ordered: true,
            counts: [2, 1, 0],
        };
        assert_eq!(walked.scans[0].as_ref().unwrap(), &scan);
        assert_eq!(walked.merged.unwrap().first, [2, 1, 0]);

        // The second side steps down at its line 3, before the first side's
        // line 3 is read: that cursor's error is the one raised, and both
        // sides are still judged whole.
        let walked = judged("n1 v1\nn10 v1\nn9 v1\nw1 v1\n", "n1 v1\nn3 v1\nn2 v1\n").unwrap();
        let err = walked.merged.unwrap_err().to_string();
        assert!(err.contains("b.opl line 3"), "{err}");
        let first = walked.scans[0].as_ref().unwrap();
        assert_eq!((first.ordered, first.counts), (false, [3, 1, 0]));
        let second = walked.scans[1].as_ref().unwrap();
        assert_eq!((second.ordered, second.counts), (false, [3, 0, 0]));

        // A repeat on one side; the other side is ordered, and a parse error
        // in a drained side is that side's alone.
        let walked = judged("n1 v1\nn1 v1\n", "n1 v1\nn2 v1\nx\n").unwrap();
        assert!(walked.merged.is_err());
        assert!(!walked.scans[0].as_ref().unwrap().ordered);
        assert!(walked.scans[1].is_err());

        // A read error before any step-down is raised, not judged.
        assert!(judged("n1 v1", "n1 v1\n").is_err());
    }

    #[test]
    fn negative_ids_order_as_osmium_orders_them() {
        // Zero, then negatives by magnitude, then positives.
        let text = "n0 v1\nn-1 v1\nn-2 v1\nn1 v1\n";
        assert!(run(text, text).unwrap().0.identical());
        assert!(run("n-2 v1\nn-1 v1\n", "n-2 v1\n").is_err());
        let (merged, _, listing) = run("n-1 v1\nn1 v1\n", "n-1 v2\nn1 v1\n").unwrap();
        assert_eq!(merged.differs, 1);
        assert_eq!(listing, "-n-1 v1\n+n-1 v2\n");
    }

    #[test]
    fn a_sequence_walk_takes_any_order_and_pairs_only_equal_sequences() {
        let text = "n3 v1\nn1 v1\nn2 v1\n";
        assert!(walk_test(text, text, false).unwrap().0.identical());
        // A within-kind reorder is not the same sequence.
        assert!(
            !walk_test(text, "n1 v1\nn3 v1\nn2 v1\n", false)
                .unwrap()
                .0
                .identical()
        );
        // A move as osmium diff reports it: one object out, then in.
        let (merged, groups, listing) =
            walk_test("n1 v1\nn2 v1\nn3 v1\n", "n2 v1\nn1 v1\nn3 v1\n", false).unwrap();
        assert_eq!(
            (merged.first_only, merged.second_only, merged.same),
            (1, 1, 2)
        );
        assert_eq!(groups.len(), 2);
        assert_eq!(listing, "-n1 v1\n+n1 v1\n");
        // A shifted tag boundary differs here too.
        let (merged, _, _) = walk_test("n1 v1 Ta=bc\n", "n1 v1 Tab=c\n", false).unwrap();
        assert_eq!(merged.differs, 1);
    }

    fn derive_model(old: &str, new: &str) -> (u64, String) {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = scratch.path();
        let (pa, pb, pe) = (dir.join("a.opl"), dir.join("b.opl"), dir.join("e.opl"));
        std::fs::write(&pa, old).unwrap();
        std::fs::write(&pb, new).unwrap();
        let retained = retained_by_osmium_derive(&pa, &pb, &pe).unwrap();
        (retained, std::fs::read_to_string(&pe).unwrap())
    }

    #[test]
    fn osmium_derive_retains_an_old_only_object_whose_id_the_next_new_object_holds() {
        // The kind-change pair itself.
        assert_eq!(
            derive_model("n1 x1\nn7 x2\n", "n1 x1\nw7 Nn1\n"),
            (1, "n1 x1\nn7 x2\nw7 Nn1\n".into())
        );
        // Every old-only record before the cursor is tested on its own.
        assert_eq!(
            derive_model("n7 x1\nn8 x1\nw7 N\n", "r7 M\n"),
            (2, "n7 x1\nw7 N\nr7 M\n".into())
        );
        // A different next ID, a paired cursor of another ID, or the end of
        // new: deleted.
        assert_eq!(
            derive_model("n5 x1\nn7 x1\n", "n7 x2\nw9 N\n"),
            (0, "n7 x2\nw9 N\n".into())
        );
        assert_eq!(
            derive_model("n1 x1\nw7 N\n", "n1 x1\n"),
            (0, "n1 x1\n".into())
        );
        // A paired cursor with the old-only ID also retains it.
        assert_eq!(
            derive_model("n7 x1\nw7 N\n", "w7 Nn1\n"),
            (1, "n7 x1\nw7 Nn1\n".into())
        );
    }

    #[test]
    fn the_policy_spells_osmiums_opl_options() {
        assert_eq!(Policy::FULL.format(), "opl,locations_on_ways=true");
        assert_eq!(Policy::NO_LOCATIONS.format(), "opl");
        assert_eq!(Policy::CONTENT.format(), "opl,add_metadata=false");
    }
}
