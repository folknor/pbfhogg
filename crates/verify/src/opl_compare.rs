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

    fn format(self) -> String {
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

/// `osmium cat` of `pbf` to OPL under `policy`, into `opl`.
pub(crate) fn decode(
    harness: &mut Harness,
    pbf: &Path,
    opl: &Path,
    policy: Policy,
) -> Result<(), Error> {
    let format = policy.format();
    // A file decoded twice in one check gets a fresh decoding each time.
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
    walk(harness, first, second, listing, policy, false, each, |_| {})
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
    let merged = merge(&a, &b, listing, strict, each, paired)?;
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
    let mut stream = Stream::open(opl)?;
    let mut ordered = true;
    let mut last = None;
    while let Some(key) = stream.read_record()? {
        if last.is_some_and(|last| rank(key) <= rank(last)) {
            ordered = false;
        }
        last = Some(key);
    }
    let counts = stream.counts;
    drop(stream);
    std::fs::remove_file(opl)?;
    Ok((ordered, counts))
}

/// One OPL stream: its current record and the key it last held.
struct Stream {
    path: PathBuf,
    reader: BufReader<File>,
    line: Vec<u8>,
    number: u64,
    key: Option<(usize, i64)>,
    counts: [u64; 3],
}

impl Stream {
    fn open(path: &Path) -> Result<Self, Error> {
        let file = File::open(path)
            .map_err(|e| Error::Failed(format!("cannot open {}: {e}", path.display())))?;
        Ok(Self {
            path: path.to_path_buf(),
            reader: BufReader::new(file),
            line: Vec::new(),
            number: 0,
            key: None,
            counts: [0; 3],
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
fn rank((kind, id): (usize, i64)) -> (usize, bool, u64) {
    (kind, id > 0, id.unsigned_abs())
}

/// The kind and signed ID an OPL object line opens with.
fn parse_key(line: &[u8]) -> Result<(usize, i64), &'static str> {
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
    mut each: impl FnMut(Group),
    mut paired: impl FnMut(Pair<'_>),
) -> Result<Merged, Error> {
    let mut a = Stream::open(first)?;
    let mut b = Stream::open(second)?;
    let mut out = BufWriter::new(File::create(listing)?);
    let mut merged = Merged::default();
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
    out.flush()?;
    merged.first = a.counts;
    merged.second = b.counts;
    Ok(merged)
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

    #[test]
    fn the_policy_spells_osmiums_opl_options() {
        assert_eq!(Policy::FULL.format(), "opl,locations_on_ways=true");
        assert_eq!(Policy::NO_LOCATIONS.format(), "opl");
        assert_eq!(Policy::CONTENT.format(), "opl,add_metadata=false");
    }
}
