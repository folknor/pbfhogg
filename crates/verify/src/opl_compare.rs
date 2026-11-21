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
use crate::reference::ToolRun;

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
    pub(crate) fn between(a: &Path, b: &Path) -> Result<Self, Error> {
        let declares = |p: &Path| -> Result<bool, Error> {
            Ok(crate::pbf_frames::header(p)?.optional_feature("LocationsOnWays"))
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

/// `osmium cat` of `pbf` to OPL under `policy`, into `opl`.
pub(crate) fn decode(
    harness: &mut Harness,
    pbf: &Path,
    opl: &Path,
    policy: Policy,
) -> Result<(), Error> {
    let format = policy.format();
    // A file decoded twice in one check gets a fresh decoding each time:
    // reference placement links into an absent name and refuses an existing
    // one, so a repeated decode to the same path needs the old pair gone.
    // Deliberately kept over fresh per-call paths: the verifier never
    // retries, the first decoding of a path is merged and removed before any
    // second one, and fresh paths would only leave more scratch behind. The
    // delete of an absent file costs one unlink.
    for stale in [opl.to_path_buf(), opl.with_extension("stdout")] {
        match std::fs::remove_file(&stale) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let captured = harness.reference(
        &ToolRun::osmium("opl-compare.1")
            .arg("cat")
            .input(pbf)
            .args(&["-f", &format, "-o"])
            .output(opl)
            .arg("--overwrite")
            .uncached(),
        &opl.with_extension("stdout"),
    )?;
    harness.require_success(&captured, "osmium cat to OPL")
}

/// Compare `first` with `second` under `policy`: both decoded to OPL beside
/// `listing`, merged, every [`Group`] handed to `each` in object order and
/// written to `listing`. The OPL pair is removed once merged; on an error it
/// stays for diagnosis. A stream out of strict order, an ID that is not a
/// signed 64-bit decimal or a line that is not an object is an error naming
/// the file.
pub(crate) fn compare(
    harness: &mut Harness,
    first: &Path,
    second: &Path,
    listing: &Path,
    policy: Policy,
    each: impl FnMut(Group),
) -> Result<Merged, Error> {
    compare_paired(harness, first, second, listing, policy, each, |_| {})
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
    first: &Path,
    second: &Path,
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
    first: &Path,
    second: &Path,
    listing: &Path,
    policy: Policy,
    each: impl FnMut(Group),
    paired: impl FnMut(Pair<'_>),
) -> Result<Merged, Error> {
    walk(harness, first, second, listing, policy, false, each, paired)
}

/// [`compare`], also handing every paired identity, alike or not, to
/// `paired` in object order with both records.
pub(crate) fn compare_paired(
    harness: &mut Harness,
    first: &Path,
    second: &Path,
    listing: &Path,
    policy: Policy,
    each: impl FnMut(Group),
    paired: impl FnMut(Pair<'_>),
) -> Result<Merged, Error> {
    walk(harness, first, second, listing, policy, true, each, paired)
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
    first: &Path,
    second: &Path,
    listing: &Path,
    policy: Policy,
    each: impl FnMut(Group),
) -> Result<Walked, Error> {
    let a = listing.with_extension("first.opl");
    let b = listing.with_extension("second.opl");
    decode(harness, first, &a, policy)?;
    decode(harness, second, &b, policy)?;
    let walked = harness.timed(MERGE_STEP, || merge_judged(&a, &b, listing, each))?;
    if let Ok(merged) = &walked.merged {
        harness.msg(&format!(
            "  OPL merge {}: same={} first-only={} second-only={} differs={}",
            listing.display(),
            merged.same,
            merged.first_only,
            merged.second_only,
            merged.differs
        ));
        for opl in [&a, &b] {
            std::fs::remove_file(opl)?;
        }
    }
    Ok(walked)
}

#[allow(clippy::too_many_arguments)]
fn walk(
    harness: &mut Harness,
    first: &Path,
    second: &Path,
    listing: &Path,
    policy: Policy,
    strict: bool,
    each: impl FnMut(Group),
    paired: impl FnMut(Pair<'_>),
) -> Result<Merged, Error> {
    let a = listing.with_extension("first.opl");
    let b = listing.with_extension("second.opl");
    decode(harness, first, &a, policy)?;
    decode(harness, second, &b, policy)?;
    let merged = harness.timed(MERGE_STEP, || merge(&a, &b, listing, strict, each, paired))?;
    harness.msg(&format!(
        "  OPL merge {}: same={} first-only={} second-only={} differs={}",
        listing.display(),
        merged.same,
        merged.first_only,
        merged.second_only,
        merged.differs
    ));
    // The listing holds every differing record from both sides, so the
    // decodings, each as large as its input, are not kept even then.
    for opl in [&a, &b] {
        std::fs::remove_file(opl)?;
    }
    Ok(merged)
}

/// Whether `pbf`'s objects strictly ascend in osmium's object order as
/// osmium decodes them, and its object count per kind: an OPL decoding
/// (no metadata, no inline coordinates) beside `opl`, scanned for its keys
/// and removed. `osmium fileinfo -e` cannot stand in: it refuses a file
/// whose ways lack their nodes (an invalid-location geometry error), which
/// every way-only or filtered output is.
pub(crate) fn order(
    harness: &mut Harness,
    pbf: &Path,
    opl: &Path,
) -> Result<(bool, [u64; 3]), Error> {
    decode(harness, pbf, opl, Policy::CONTENT)?;
    let scanned = harness.timed("opl order scan", || scan_order(opl))?;
    std::fs::remove_file(opl)?;
    Ok(scanned)
}

/// The key scan of [`order`] over one OPL file.
fn scan_order(opl: &Path) -> Result<(bool, [u64; 3]), Error> {
    let mut stream = Stream::open(opl)?;
    let mut ordered = true;
    let mut last = None;
    while let Some(key) = stream.read_record()? {
        if last.is_some_and(|last| rank(key) <= rank(last)) {
            ordered = false;
        }
        last = Some(key);
    }
    Ok((ordered, stream.counts))
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
/// its version. Returns how many records were retained; both decodings are
/// removed.
pub(crate) fn osmium_derive_roundtrip(
    harness: &mut Harness,
    old: &Path,
    new: &Path,
    expected: &Path,
) -> Result<u64, Error> {
    let a = expected.with_extension("old.opl");
    let b = expected.with_extension("new.opl");
    decode(harness, old, &a, Policy::CONTENT)?;
    decode(harness, new, &b, Policy::CONTENT)?;
    let retained = harness.timed("opl derive model", || {
        retained_by_osmium_derive(&a, &b, expected)
    })?;
    for opl in [&a, &b] {
        std::fs::remove_file(opl)?;
    }
    Ok(retained)
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
/// written (`expected`); the decoding is removed once merged.
pub(crate) fn compare_with_opl(
    harness: &mut Harness,
    pbf: &Path,
    expected: &Path,
    listing: &Path,
    policy: Policy,
) -> Result<Merged, Error> {
    compare_with_opl_each(harness, pbf, expected, listing, policy, |_| {})
}

/// [`compare_with_opl`], handing every [`Group`] to `each` in object order.
pub(crate) fn compare_with_opl_each(
    harness: &mut Harness,
    pbf: &Path,
    expected: &Path,
    listing: &Path,
    policy: Policy,
    each: impl FnMut(Group),
) -> Result<Merged, Error> {
    let a = listing.with_extension("first.opl");
    decode(harness, pbf, &a, policy)?;
    let merged = harness.timed(MERGE_STEP, || {
        merge(&a, expected, listing, true, each, |_| {})
    })?;
    harness.msg(&format!(
        "  OPL merge {}: same={} first-only={} second-only={} differs={}",
        listing.display(),
        merged.same,
        merged.first_only,
        merged.second_only,
        merged.differs
    ));
    std::fs::remove_file(&a)?;
    Ok(merged)
}

/// [`compare_sequence`] with the first side an OPL file already written
/// (`expected`) and the second `pbf` decoded under `policy`; the decoding is
/// removed once merged, `expected` is kept.
pub(crate) fn compare_sequence_with_opl(
    harness: &mut Harness,
    expected: &Path,
    pbf: &Path,
    listing: &Path,
    policy: Policy,
) -> Result<Merged, Error> {
    let b = listing.with_extension("second.opl");
    decode(harness, pbf, &b, policy)?;
    let merged = harness.timed(MERGE_STEP, || {
        merge(expected, &b, listing, false, |_| {}, |_| {})
    })?;
    harness.msg(&format!(
        "  OPL merge {}: same={} first-only={} second-only={} differs={}",
        listing.display(),
        merged.same,
        merged.first_only,
        merged.second_only,
        merged.differs
    ));
    std::fs::remove_file(&b)?;
    Ok(merged)
}

/// One OPL stream: its current record and the key it last held.
struct Stream {
    path: PathBuf,
    reader: BufReader<File>,
    line: Vec<u8>,
    number: u64,
    key: Option<(usize, i64)>,
    counts: [u64; 3],
    /// The line of the first record at or below its predecessor, once a
    /// strict read has met one.
    stepped: Option<u64>,
}

impl Stream {
    fn open(path: &Path) -> Result<Self, Error> {
        let file = File::open(path)
            .map_err(|e| Error::Failed(format!("cannot open {}: {e}", path.display())))?;
        Ok(Self {
            path: path.to_path_buf(),
            reader: BufReader::with_capacity(crate::tunables::OPL_READ_BUFFER, file),
            line: Vec::new(),
            number: 0,
            key: None,
            counts: [0; 3],
            stepped: None,
        })
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
) -> Result<Merged, Error> {
    let mut a = Stream::open(first)?;
    let mut b = Stream::open(second)?;
    let mut out = BufWriter::new(File::create(listing)?);
    let mut merged = Merged::default();
    pair_streams(&mut a, &mut b, &mut out, strict, &mut merged, each, paired)?;
    out.flush()?;
    merged.first = a.counts;
    merged.second = b.counts;
    Ok(merged)
}

/// The strict [`merge`] of [`compare_judged`]: a step-down is a judgment of
/// both sides, its cursor error saved, rather than an early return.
fn merge_judged(
    first: &Path,
    second: &Path,
    listing: &Path,
    each: impl FnMut(Group),
) -> Result<Walked, Error> {
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
            Ok(Walked {
                scans: [scan(a.counts), scan(b.counts)],
                merged: Ok(merged),
                listing_note: None,
            })
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
            Ok(Walked {
                scans: [a.drain(), b.drain()],
                merged: Err(e),
                listing_note,
            })
        }
        Err(e) => Err(e),
    }
}

/// The pairing loop of [`merge`] over two open streams.
fn pair_streams(
    a: &mut Stream,
    b: &mut Stream,
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
        let merged = merge(&pa, &pb, &pl, strict, |g| groups.push(g), |_| {})?;
        Ok((merged, groups, std::fs::read_to_string(&pl).unwrap()))
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
        merge_judged(&pa, &pb, &pl, |_| {})
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
