//! Verify: renumber - `pbfhogg renumber` vs `osmium renumber`.
//!
//! Cross-validates pbfhogg's renumber output against osmium. The two tools
//! name orphan references (a way ref or relation member whose target is not
//! in the input) differently, a documented deviation (pbfhogg
//! `DEVIATIONS.md`, "renumber: orphan-reference handling"): pbfhogg maps the
//! absent IDs one to one onto the IDs outside the live range, osmium
//! numbers them on past the live range in first-encounter order. Both keep
//! every orphan outside its kind's live range, where no output object
//! lives. Each difference is proved to be that and nothing else, record by
//! record; there is no tolerance.
//!
//! The comparison is the verifier's merge of osmium's decodings of both
//! outputs (`crate::opl_compare`, metadata included), so both must be in
//! strict order and every object must pair by kind and ID. It passes when:
//! - every object pairs, and each kind's objects are exactly the IDs
//!   `start, start + 1, ...` on both sides (`start, start - 1, ...` from a
//!   negative start; a start of 0 is 1, as both tools read it), which fixes
//!   each kind's live range
//! - no node differs
//! - a differing way or relation differs only in the ref IDs of its node or
//!   member list: same fields, same list length, same member types and
//!   encoded roles or coordinate suffixes
//! - every reference, in every record, alike or not, is either the same
//!   live ID on both sides or outside its kind's live range on both
//! - the orphan names correspond one to one per kind across the whole file:
//!   one osmium orphan is one pbfhogg orphan, and the reverse
//!
//! That proves the two outputs the same reference graph up to the naming of
//! orphans. It does not prove pbfhogg's orphan mapping: that would need the
//! input's references, which the comparison does not read. Starts at the
//! ends of the ID range are not an osmium oracle (osmium refuses
//! `i64::MIN` and `i64::MAX` as IDs and its arithmetic overflows near
//! them); pbfhogg's own tests cover them.
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
use crate::opl_compare::{self, Class, Merged, Policy};
use crate::reference::{Origin, ToolRun};

/// Differing identities per kind (node, way, relation) and class.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Classified {
    one_sided: [u64; 3],
    differs: [u64; 3],
}

impl Classified {
    fn add(&mut self, group: opl_compare::Group) {
        match group.class {
            Class::FirstOnly | Class::SecondOnly => self.one_sided[group.kind] += 1,
            Class::Differs => self.differs[group.kind] += 1,
        }
    }
}

const KINDS: [&str; 3] = ["node", "way", "relation"];

/// The per-kind start IDs `--start-id` names: one value, or
/// `node,way,relation`; 1 when absent, and 0 read as 1, as both tools do.
fn start_ids(start_id: Option<&str>) -> Result<[i128; 3], Error> {
    let Some(value) = start_id else {
        return Ok([1; 3]);
    };
    let parse = |part: &str| {
        part.trim()
            .parse::<i64>()
            .map(|s| if s == 0 { 1 } else { i128::from(s) })
            .map_err(|_| Error::Failed(format!("renumber: invalid --start-id {value}")))
    };
    let parts: Vec<&str> = value.split(',').collect();
    match parts[..] {
        [one] => Ok([parse(one)?; 3]),
        [n, w, r] => Ok([parse(n)?, parse(w)?, parse(r)?]),
        _ => Err(Error::Failed(format!(
            "renumber: --start-id {value} is neither one value nor three"
        ))),
    }
}

/// The record-by-record proof that the outputs differ only in how they
/// name orphan references (see the module documentation). osmium is the
/// first side. The first failure is kept and later records are skipped.
#[derive(Debug, Default)]
struct Attribution {
    start: [i128; 3],
    /// The ID the next object of each kind must have; once the streams have
    /// passed a kind, its live range's exclusive end (below the range when
    /// the kind counts down).
    next: [i128; 3],
    /// The kind of the record being read: a kind below it has its final
    /// live range.
    current: usize,
    /// References to a kind whose range is not final yet, as (kind,
    /// pbfhogg, osmium): relation members naming relations.
    deferred: Vec<(usize, i64, i64)>,
    /// Orphan names per referenced kind, pbfhogg to osmium and back.
    forward: [std::collections::HashMap<i64, i64>; 3],
    reverse: [std::collections::HashMap<i64, i64>; 3],
    /// Records per kind that differ, and differ only in orphan names.
    orphan_records: [u64; 3],
    failure: Option<String>,
}

/// One item of an OPL node or member list: its type letter, ref ID and
/// the encoded remainder (an `@role`, or a way node's coordinates).
fn split_item(item: &[u8]) -> Option<(u8, i64, &[u8])> {
    let (&letter, rest) = item.split_first()?;
    let sign = usize::from(rest.first() == Some(&b'-'));
    let digits = rest[sign..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    if digits == 0 {
        return None;
    }
    // Accumulated toward the sign, so `i64::MIN` parses and anything past
    // either end is `None`, as `str::parse` reads it.
    let mut id: i64 = 0;
    for &b in &rest[sign..sign + digits] {
        let d = i64::from(b - b'0');
        id = id.checked_mul(10)?;
        id = if sign == 1 {
            id.checked_sub(d)?
        } else {
            id.checked_add(d)?
        };
    }
    let digits = sign + digits;
    Some((letter, id, &rest[digits..]))
}

fn item_kind(letter: u8) -> Option<usize> {
    match letter {
        b'n' => Some(0),
        b'w' => Some(1),
        b'r' => Some(2),
        _ => None,
    }
}

impl Attribution {
    fn new(start: [i128; 3]) -> Self {
        Self {
            start,
            next: start,
            ..Self::default()
        }
    }

    /// +1 for a kind counting up from a positive start, -1 counting down.
    fn step(&self, kind: usize) -> i128 {
        if self.start[kind] > 0 { 1 } else { -1 }
    }

    /// Whether `id` is one of `kind`'s live IDs, between its start and the
    /// next ID, whichever way the kind counts.
    fn live(&self, kind: usize, id: i128) -> bool {
        let (start, next) = (self.start[kind], self.next[kind]);
        if start > 0 {
            (start..next).contains(&id)
        } else {
            (next + 1..=start).contains(&id)
        }
    }

    fn fail(&mut self, why: String) {
        if self.failure.is_none() {
            self.failure = Some(why);
        }
    }

    /// One paired object (see [`opl_compare::Pair`]).
    fn pair(&mut self, pair: opl_compare::Pair<'_>) {
        if self.failure.is_some() {
            return;
        }
        let name = KINDS[pair.kind];
        self.current = pair.kind;
        if i128::from(pair.id) != self.next[pair.kind] {
            self.fail(format!(
                "{name} {} is not the next renumbered {name} ID {}",
                pair.id, self.next[pair.kind]
            ));
            return;
        }
        self.next[pair.kind] += self.step(pair.kind);
        if pair.kind == 0 {
            if pair.first != pair.second {
                self.fail(format!("node {} differs", pair.id));
            }
            return;
        }
        match self.record(pair) {
            Ok(renamed) => {
                if pair.first != pair.second && renamed {
                    self.orphan_records[pair.kind] += 1;
                }
            }
            Err(why) => self.fail(format!("{name} {} differs: {why}", pair.id)),
        }
    }

    /// Compare a way or relation field by field; whether a ref differed.
    fn record(&mut self, pair: opl_compare::Pair<'_>) -> Result<bool, String> {
        let list = if pair.kind == 1 { b'N' } else { b'M' };
        let mut a = pair.first.split(|&b| b == b' ');
        let mut b = pair.second.split(|&b| b == b' ');
        let mut renamed = false;
        loop {
            match (a.next(), b.next()) {
                (None, None) => return Ok(renamed),
                (Some(x), Some(y)) if x.first() == Some(&list) && y.first() == Some(&list) => {
                    renamed |= self.list(&x[1..], &y[1..])?;
                }
                (Some(x), Some(y)) if x == y => {}
                _ => return Err("a field other than its node or member refs".into()),
            }
        }
    }

    fn list(&mut self, a: &[u8], b: &[u8]) -> Result<bool, String> {
        if a.is_empty() || b.is_empty() {
            return if a == b {
                Ok(false)
            } else {
                Err("the reference lists differ in length".into())
            };
        }
        let mut renamed = false;
        let mut ia = a.split(|&c| c == b',');
        let mut ib = b.split(|&c| c == b',');
        loop {
            let (x, y) = match (ia.next(), ib.next()) {
                (None, None) => return Ok(renamed),
                (Some(x), Some(y)) => (x, y),
                _ => return Err("the reference lists differ in length".into()),
            };
            let (Some((lx, px, rx)), Some((ly, py, ry))) = (split_item(x), split_item(y)) else {
                return Err("a reference that is not a typed decimal ID".into());
            };
            let kind = item_kind(lx).ok_or("a reference of no known type")?;
            if lx != ly || rx != ry {
                return Err("a reference type, role or coordinate".into());
            }
            renamed |= px != py;
            if kind < self.current {
                self.reference(kind, py, px)?;
            } else {
                self.deferred.push((kind, py, px));
            }
        }
    }

    /// One reference, pbfhogg's ID `p` and osmium's `o`, once its kind's
    /// live range is final.
    fn reference(&mut self, kind: usize, p: i64, o: i64) -> Result<(), String> {
        let (start, end) = (self.start[kind], self.next[kind]);
        let (lp, lo) = (
            self.live(kind, i128::from(p)),
            self.live(kind, i128::from(o)),
        );
        let name = KINDS[kind];
        if p == o && lp {
            return Ok(());
        }
        if lp || lo {
            return Err(format!(
                "{name} ref pbfhogg {p} osmium {o} is not one live {name} on both sides nor an \
                 orphan on both (live {name}s from {start}, next {end})"
            ));
        }
        if *self.forward[kind].entry(p).or_insert(o) != o
            || *self.reverse[kind].entry(o).or_insert(p) != p
        {
            return Err(format!(
                "orphan {name} pbfhogg {p} osmium {o} names another orphan on the other side"
            ));
        }
        Ok(())
    }

    /// Classify the deferred references, now that every range is final;
    /// the first failure, if any.
    fn finish(mut self) -> (Option<String>, [u64; 3], [usize; 3]) {
        self.current = 3;
        for (kind, p, o) in std::mem::take(&mut self.deferred) {
            if self.failure.is_some() {
                break;
            }
            if let Err(why) = self.reference(kind, p, o) {
                self.fail(format!("a relation member: {why}"));
            }
        }
        let orphans = [0, 1, 2].map(|k| self.forward[k].len());
        (self.failure, self.orphan_records, orphans)
    }
}

/// The derived-key kind of a [`CanonicalSummary`]; bump its number with any
/// change to the canonical encoding below.
const CANONICAL_KIND: &str = "renumber-canonical 1";

/// One renumbered output's decoding with its orphan names abstracted away:
/// stored across runs beside the plain decoding summary, so a warm run whose
/// two outputs decode as before decides from two of these without decoding
/// or merging (see [`Canonical`] for what equality of two proves). An
/// observation of one file, never a verdict: the verdict is decided again
/// from both, every run.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
struct CanonicalSummary {
    counts: [u64; 3],
    /// Per kind, the first and last object ID, for a nonempty kind.
    ends: [Option<(i64, i64)>; 3],
    /// Distinct orphan IDs referenced, per referenced kind.
    orphans: [u64; 3],
    /// XXH3-128 of the canonical stream.
    digest: String,
}

/// The canonical reading of one strictly ordered renumber output, record by
/// record. Each kind's live range is the file's own: its objects must be
/// consecutive IDs (both ends nonzero and of one sign, `|last - first| + 1`
/// objects), which with strict order means exactly the IDs between the ends.
/// Nodes, and every field but a way's `N` list or a relation's `M` list, are
/// hashed verbatim. Each list item keeps its type letter and encoded
/// remainder; its ID becomes `L` and the parsed decimal when inside the
/// referenced kind's range, else `O` and the orphan's first-occurrence
/// ordinal in that kind, one table per referenced kind over the whole
/// stream, equal records included. A reference to a kind whose range is not
/// final yet (relation members naming relations) becomes `D`, and its ID is
/// replayed in a counted trailer once every range is final.
///
/// Two such streams with equal digests, counts and ends, and ends at the
/// expected start, are the attribution's predicate: equal nodes and fields,
/// objects at the renumbered IDs, live references alike, every other
/// reference outside its range on both sides, and the orphan names one to
/// one both ways (equal ordinals at two positions mean the same orphan on
/// both sides or a different one on both). It cannot recover how many raw
/// records differ, nor the listing. Any ID not in canonical decimal, an
/// unknown list item, or a kind that is not consecutive makes the reading
/// ineligible, and the check then walks.
struct Canonical {
    hasher: xxhash_rust::xxh3::Xxh3,
    counts: [u64; 3],
    ends: [Option<(i64, i64)>; 3],
    orphans: [std::collections::HashMap<i64, u64>; 3],
    deferred: Vec<(usize, i64)>,
    /// The kind of the record being read.
    current: usize,
    /// Each kind's [`range`](Self::range), fixed once the stream passed it.
    finals: [Option<Option<(i128, i128)>>; 3],
    eligible: bool,
    /// Canonical bytes not hashed yet.
    buf: Vec<u8>,
}

impl Canonical {
    fn new() -> Self {
        Self {
            hasher: xxhash_rust::xxh3::Xxh3::new(),
            counts: [0; 3],
            ends: [None; 3],
            orphans: Default::default(),
            deferred: Vec::new(),
            current: 0,
            finals: [Some(None); 3],
            eligible: true,
            buf: Vec::new(),
        }
    }

    /// The inclusive live range of `kind`, when its objects so far are
    /// consecutive; `Some(None)` for a kind with none; `None` when not.
    fn range(&self, kind: usize) -> Option<Option<(i128, i128)>> {
        let Some((first, last)) = self.ends[kind] else {
            return Some(None);
        };
        let (first, last) = (i128::from(first), i128::from(last));
        let one_sign = (first > 0 && last > 0) || (first < 0 && last < 0);
        let span = (last - first).unsigned_abs() + 1;
        (one_sign && span == u128::from(self.counts[kind]))
            .then_some(Some((first.min(last), first.max(last))))
    }

    /// Make `kind` the kind being read, fixing the range of every kind
    /// below it.
    fn advance(&mut self, kind: usize) {
        if kind > self.current {
            for k in self.current..kind {
                self.finals[k] = self.range(k);
            }
            self.current = kind;
        }
    }

    /// The canonical encoding of a reference to `kind`'s `id`, appended to
    /// `out`, once `kind`'s range is final.
    fn encode(&mut self, kind: usize, id: i64, out: &mut Vec<u8>) {
        let Some(range) = self.finals[kind] else {
            self.eligible = false;
            return;
        };
        if range.is_some_and(|(lo, hi)| (lo..=hi).contains(&i128::from(id))) {
            out.push(b'L');
            push_decimal(out, id.unsigned_abs(), id < 0);
        } else {
            let table = &mut self.orphans[kind];
            let next = u64::try_from(table.len()).unwrap_or(u64::MAX);
            let ordinal = *table.entry(id).or_insert(next);
            out.push(b'O');
            push_decimal(out, ordinal, false);
        }
    }

    /// One record of the stream, in its strict order.
    fn record(&mut self, kind: usize, id: i64, line: &[u8]) {
        if !self.eligible {
            return;
        }
        self.advance(kind);
        self.counts[kind] += 1;
        self.ends[kind] = Some(match self.ends[kind] {
            None => (id, id),
            Some((first, _)) => (first, id),
        });
        let mut out = std::mem::take(&mut self.buf);
        if kind == 0 {
            out.extend_from_slice(line);
            out.push(b'\n');
            self.buffered(out);
            return;
        }
        let list = if kind == 1 { b'N' } else { b'M' };
        for (i, field) in line.split(|&b| b == b' ').enumerate() {
            if i > 0 {
                out.push(b' ');
            }
            match field.split_first() {
                Some((&letter, items)) if letter == list => {
                    out.push(letter);
                    if !items.is_empty() {
                        self.items(items, &mut out);
                    }
                }
                _ => out.extend_from_slice(field),
            }
        }
        out.push(b'\n');
        self.buffered(out);
    }

    /// Keep the pending canonical bytes, hashing them once a read buffer's
    /// worth has gathered: one hash update per line would cost more than
    /// the encoding.
    fn buffered(&mut self, mut out: Vec<u8>) {
        if out.len() >= crate::tunables::OPL_READ_BUFFER {
            self.hasher.update(&out);
            out.clear();
        }
        self.buf = out;
    }

    fn items(&mut self, items: &[u8], out: &mut Vec<u8>) {
        for (i, item) in items.split(|&c| c == b',').enumerate() {
            if i > 0 {
                out.push(b',');
            }
            let Some((letter, id, rest)) = split_item(item) else {
                self.eligible = false;
                return;
            };
            let digits = &item[1..item.len() - rest.len()];
            let (Some(kind), true) = (item_kind(letter), canonical_decimal(digits)) else {
                self.eligible = false;
                return;
            };
            out.push(letter);
            if kind < self.current {
                self.encode(kind, id, out);
            } else {
                out.push(b'D');
                self.deferred.push((kind, id));
            }
            out.extend_from_slice(rest);
        }
    }

    /// The summary, when the whole reading is eligible.
    fn finish(mut self) -> Option<CanonicalSummary> {
        self.advance(3);
        self.hasher.update(&self.buf);
        let deferred = std::mem::take(&mut self.deferred);
        self.hasher
            .update(format!("\0deferred {}\n", deferred.len()).as_bytes());
        let mut out = Vec::new();
        for (kind, id) in deferred {
            out.clear();
            out.push(b"nwr"[kind]);
            self.encode(kind, id, &mut out);
            out.push(b'\n');
            self.hasher.update(&out);
        }
        if self.finals.contains(&None) {
            self.eligible = false;
        }
        self.eligible.then(|| CanonicalSummary {
            counts: self.counts,
            ends: self.ends,
            orphans: self
                .orphans
                .each_ref()
                .map(|t| u64::try_from(t.len()).unwrap_or(u64::MAX)),
            digest: format!("{:032x}", self.hasher.digest128()),
        })
    }
}

/// Whether `digits` (an optional `-` and ASCII digits, as [`split_item`]
/// accepted them) spell their value the one way `i64`'s `Display` does: no
/// leading zero and no `-0`.
fn canonical_decimal(digits: &[u8]) -> bool {
    let magnitude = digits.strip_prefix(b"-").unwrap_or(digits);
    match magnitude {
        [b'0'] => magnitude.len() == digits.len(),
        [b'0', ..] => false,
        _ => true,
    }
}

/// Append `value` in decimal, after a `-` when `negative`.
fn push_decimal(out: &mut Vec<u8>, mut value: u64, negative: bool) {
    let mut digits = [0_u8; 20];
    let mut at = digits.len();
    loop {
        at -= 1;
        // A remainder below ten fits a byte.
        digits[at] = b'0' + u8::try_from(value % 10).unwrap_or(0);
        value /= 10;
        if value == 0 {
            break;
        }
    }
    if negative {
        out.push(b'-');
    }
    out.extend_from_slice(&digits[at..]);
}

/// Whether two canonical readings decide the comparison: equal, and each
/// nonempty kind starting at its renumbered start and counting its way.
fn canonical_pass(a: &CanonicalSummary, b: &CanonicalSummary, start: [i128; 3]) -> bool {
    a == b
        && (0..3).all(|kind| match a.ends[kind] {
            None => true,
            Some((first, last)) => {
                let step = if start[kind] > 0 { 1 } else { -1 };
                let span = i128::from(a.counts[kind]) - 1;
                i128::from(first) == start[kind] && i128::from(last) == start[kind] + step * span
            }
        })
}

/// Why the comparison fails, or `None` when it passes. osmium's output is
/// the merge's first side; `attribution` is the record-by-record proof's
/// first failure.
fn judge(merged: &Merged, classified: &Classified, attribution: Option<&str>) -> Option<String> {
    if merged.first != merged.second {
        Some(format!(
            "element counts diverge (osmium={:?}, pbfhogg={:?})",
            merged.first, merged.second
        ))
    } else if classified.one_sided != [0; 3] {
        Some(format!(
            "objects only one output holds per kind (node, way, relation): {:?}",
            classified.one_sided
        ))
    } else {
        attribution.map(str::to_owned)
    }
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

/// The PASS claim.
const CLAIM: &str = "every object at its renumbered ID and identical as osmium decodes it, \
                     except orphan reference IDs, which lie outside the live range on both \
                     sides and correspond one to one; pbfhogg's orphan mapping is not checked \
                     against the input";

/// The comparison decided from both outputs' stored canonical readings
/// ([`CanonicalSummary`]), without decoding or merging: both looked up under
/// one tool binding, and [`canonical_pass`]. Anything else is a walk, so
/// every failure and every diagnostic is the walk's.
fn by_canonical(
    harness: &mut Harness,
    osmium_out: &Path,
    pbfhogg_out: &Path,
    policy: Policy,
    start: [i128; 3],
) -> Result<Decision, Error> {
    let Some(keys) = opl_compare::planned_keys(harness, &[osmium_out, pbfhogg_out], policy)? else {
        return Ok(Decision::Walk { publish: false });
    };
    let read = |h: &Harness, key: &crate::reference_cache::Key| {
        h.summary_of::<CanonicalSummary>(&key.derived(CANONICAL_KIND))
    };
    let (Some(a), Some(b)) = (read(harness, &keys[0]), read(harness, &keys[1])) else {
        return Ok(Decision::Walk { publish: true });
    };
    if !canonical_pass(&a, &b, start) {
        return Ok(Decision::Walk { publish: true });
    }
    harness.run.revalidate()?;
    harness.checkpoint()?;
    harness.msg(&format!(
        "  element counts: osmium={} pbfhogg={}; decided by equal canonical readings \
         (xxh3-128 {}), raw differences not counted; distinct orphans referenced: nodes {}, \
         ways {}, relations {}",
        fmt_num(a.counts.iter().sum()),
        fmt_num(b.counts.iter().sum()),
        a.digest,
        a.orphans[0],
        a.orphans[1],
        a.orphans[2],
    ));
    harness.msg("  result: PASS (decided by canonical readings)");
    Ok(Decision::Decided)
}

/// What [`by_canonical`] left the check to do.
enum Decision {
    /// Passed from the stored readings.
    Decided,
    /// Walk; `publish` when summaries are kept at all (not under
    /// `--fresh-reference`), so the walk's readings are worth making.
    Walk { publish: bool },
}

/// Cross-validate `pbfhogg renumber` against `osmium renumber`.
///
/// `direct_io` reaches the renumber run only.
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
        .input(pbf, Origin::Other)
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
    harness.judge_way_locations(
        "pbfhogg renumber",
        pbf,
        &pbfhogg_out,
        &captured,
        crate::harness::WayLocations::Kept,
    )?;

    let pbfhogg_size = fs::metadata(&pbfhogg_out).map(|m| m.len()).unwrap_or(0);
    harness.msg(&format!(
        "  pbfhogg renumber: {} output",
        fmt_size(pbfhogg_size),
    ));

    // --- The element comparison, classified by kind ---------------------
    let policy = Policy::between(
        (&osmium_out, crate::pbf_frames::Origin::Other),
        (&pbfhogg_out, crate::pbf_frames::Origin::Pbfhogg),
    )?;
    let start = start_ids(start_id)?;
    let publish = match by_canonical(harness, &osmium_out, &pbfhogg_out, policy, start)? {
        Decision::Decided => return Ok(Some(CLAIM.into())),
        Decision::Walk { publish } => publish,
    };
    let mut classified = Classified::default();
    let mut attribution = Attribution::new(start);
    let mut canonical = [Canonical::new(), Canonical::new()];
    for reading in &mut canonical {
        // A reading nothing will store is not made.
        reading.eligible = publish;
    }
    let (merged, keys) = opl_compare::compare_paired(
        harness,
        (&osmium_out, Origin::Other),
        (&pbfhogg_out, Origin::Pbfhogg),
        &diff_log,
        policy,
        |g| classified.add(g),
        |p| {
            canonical[0].record(p.kind, p.id, p.first);
            canonical[1].record(p.kind, p.id, p.second);
            attribution.pair(p);
        },
    )?;
    let (unexplained, orphan_records, orphans) = attribution.finish();
    let passed = judge(&merged, &classified, unexplained.as_deref()).is_none();
    if passed {
        // Only a passing walk paired every record, so each reading saw its
        // whole stream; each is stored under the key its decoding was
        // sealed under, and never when the decode had none.
        for (key, reading) in keys.iter().zip(canonical) {
            if let (Some(key), Some(summary)) = (key, reading.finish()) {
                harness.publish_summary_of(&key.derived(CANONICAL_KIND), &summary)?;
            }
        }
    }

    let total: u64 = merged.first.iter().sum();
    harness.msg(&format!(
        "  element counts: osmium={} pbfhogg={}",
        fmt_num(total),
        fmt_num(merged.second.iter().sum()),
    ));
    harness.msg(&format!(
        "  same: {}, different: {} (nodes {}, ways {}, relations {}), one-sided: {:?}",
        fmt_num(merged.same),
        fmt_num(merged.differs),
        classified.differs[0],
        classified.differs[1],
        classified.differs[2],
        classified.one_sided,
    ));

    if verbose && !merged.identical() {
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

    harness.msg(&format!(
        "  differing only in orphan names: ways {}, relations {}; distinct orphans \
         referenced: nodes {}, ways {}, relations {}",
        orphan_records[1], orphan_records[2], orphans[0], orphans[1], orphans[2],
    ));

    match judge(&merged, &classified, unexplained.as_deref()) {
        Some(reason) => {
            harness.msg(&format!("  result: FAIL ({reason})"));
            harness.msg(&format!("  diff log preserved: {}", diff_log.display()));
            Err(Error::Failed(format!("renumber: {reason}")))
        }
        None => {
            if merged.differs == 0 {
                harness.msg("  result: PASS (identical)");
            } else {
                harness.msg(
                    "  result: PASS (every difference is an orphan name, see pbfhogg \
                     DEVIATIONS.md)",
                );
            }
            Ok(Some(CLAIM.into()))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn merged(first: [u64; 3], second: [u64; 3]) -> Merged {
        Merged {
            first,
            second,
            ..Merged::default()
        }
    }

    #[test]
    fn the_verdict_needs_paired_objects_and_a_proved_attribution() {
        let ok = Classified::default();
        assert_eq!(judge(&merged([5, 3, 10], [5, 3, 10]), &ok, None), None);
        assert!(judge(&merged([5, 3, 10], [5, 3, 9]), &ok, None).is_some());
        let one_sided = Classified {
            one_sided: [1, 0, 0],
            differs: [0; 3],
        };
        assert!(judge(&merged([5, 3, 10], [5, 3, 10]), &one_sided, None).is_some());
        assert!(judge(&merged([5, 3, 10], [5, 3, 10]), &ok, Some("x")).is_some());
    }

    /// Run the proof over paired records given as (osmium, pbfhogg) OPL
    /// lines, start 1; the first failure and the per-kind counts.
    fn attribute(records: &[(&str, &str)]) -> (Option<String>, [u64; 3], [usize; 3]) {
        let mut a = Attribution::new([1; 3]);
        for (first, second) in records {
            let kind = match first.as_bytes()[0] {
                b'n' => 0,
                b'w' => 1,
                _ => 2,
            };
            let id = first[1..first.find(' ').unwrap()].parse().unwrap();
            a.pair(opl_compare::Pair {
                kind,
                id,
                first: first.as_bytes(),
                second: second.as_bytes(),
            });
        }
        a.finish()
    }

    const NODES: [(&str, &str); 2] = [
        ("n1 v1 x1 y1", "n1 v1 x1 y1"),
        ("n2 v1 x1 y1", "n2 v1 x1 y1"),
    ];

    fn with(rest: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
        NODES.iter().chain(rest).copied().collect()
    }

    #[test]
    fn orphan_names_past_the_live_range_are_the_only_difference_allowed() {
        // Two nodes, so osmium names the first node orphan 3; pbfhogg's
        // name is its own, here 12.
        let (failure, records, orphans) = attribute(&with(&[
            ("w1 v1 Nn1,n3", "w1 v1 Nn1,n12"),
            ("w2 v1 Nn3,n2", "w2 v1 Nn12,n2"),
            ("r1 v1 Mn3@a,w1@,r2@", "r1 v1 Mn12@a,w1@,r2@"),
            ("r2 v1 Mr4@", "r2 v1 Mr9@"),
        ]));
        assert_eq!(failure, None);
        assert_eq!(records, [0, 2, 2]);
        assert_eq!(orphans, [1, 0, 1]);
    }

    #[test]
    fn every_other_difference_fails() {
        let bad: [&[(&str, &str)]; 9] = [
            // A node differs.
            &[("n3 v1 x1 y1", "n3 v1 x2 y1")],
            // A tag differs beside an orphan.
            &[("w1 v1 Ta=1 Nn3", "w1 v1 Ta=2 Nn12")],
            // A live ref differs.
            &[("w1 v1 Nn1", "w1 v1 Nn2")],
            // A live ref made an orphan on one side.
            &[("w1 v1 Nn2", "w1 v1 Nn12")],
            // A role differs.
            &[("r1 v1 Mn3@a", "r1 v1 Mn12@b")],
            // A member list differs in length.
            &[("r1 v1 Mn1@,n2@", "r1 v1 Mn1@")],
            // Two osmium orphans are one pbfhogg orphan.
            &[("w1 v1 Nn3,n4", "w1 v1 Nn12,n12")],
            // An equal orphan elsewhere conflicts with a renamed one.
            &[("w1 v1 Nn12", "w1 v1 Nn12"), ("w2 v1 Nn13", "w2 v1 Nn12")],
            // An object off its renumbered ID.
            &[("w2 v1 Nn1", "w2 v1 Nn1")],
        ];
        for case in bad {
            assert!(attribute(&with(case)).0.is_some(), "{case:?}");
        }
        // A ref below the start is an orphan on both sides: pbfhogg may
        // name an orphan below the live range.
        assert_eq!(attribute(&with(&[("w1 v1 Nn0", "w1 v1 Nn0")])).0, None);
        assert_eq!(attribute(&with(&[("w1 v1 Nn-4", "w1 v1 Nn3")])).0, None);
        // A relation member naming a relation is judged once every relation
        // is known: when r1 is read r2 is not yet, but it is live, so the
        // two sides must name it alike.
        let renamed = with(&[("r1 v1 Mr2@", "r1 v1 Mr7@"), ("r2 v1 M", "r2 v1 M")]);
        assert!(attribute(&renamed).0.is_some());
        let alike = with(&[("r1 v1 Mr2@", "r1 v1 Mr2@"), ("r2 v1 M", "r2 v1 M")]);
        assert_eq!(attribute(&alike).0, None);
    }

    /// Counting down from -1: objects run -1, -2, ...; osmium's orphans
    /// continue down, pbfhogg's may sit anywhere outside the live IDs.
    #[test]
    fn a_negative_start_counts_down() {
        let mut a = Attribution::new([-1; 3]);
        let records = [
            ("n-1 v1 x1 y1", "n-1 v1 x1 y1"),
            ("n-2 v1 x1 y1", "n-2 v1 x1 y1"),
            ("w-1 v1 Nn-1,n-3", "w-1 v1 Nn-1,n5"),
            ("w-2 v1 Nn-3,n-2", "w-2 v1 Nn5,n-2"),
        ];
        for (first, second) in records {
            let kind = usize::from(first.starts_with('w'));
            let id = first[1..first.find(' ').unwrap()].parse().unwrap();
            a.pair(opl_compare::Pair {
                kind,
                id,
                first: first.as_bytes(),
                second: second.as_bytes(),
            });
        }
        let (failure, records, orphans) = a.finish();
        assert_eq!(failure, None);
        assert_eq!(records, [0, 2, 0]);
        assert_eq!(orphans, [1, 0, 0]);
        // A live ID renamed to an orphan on one side fails.
        let mut a = Attribution::new([-1; 3]);
        for (first, second) in [
            ("n-1 v1 x1 y1", "n-1 v1 x1 y1"),
            ("w-1 v1 Nn-1", "w-1 v1 Nn5"),
        ] {
            let kind = usize::from(first.starts_with('w'));
            let id = first[1..first.find(' ').unwrap()].parse().unwrap();
            a.pair(opl_compare::Pair {
                kind,
                id,
                first: first.as_bytes(),
                second: second.as_bytes(),
            });
        }
        assert!(a.finish().0.is_some());
        assert_eq!(start_ids(Some("0,-5,7")).unwrap(), [1, -5, 7]);
    }

    /// Both canonical readings of paired (osmium, pbfhogg) records, and
    /// whether they decide a pass from `start`.
    fn canonical(records: &[(&str, &str)], start: [i128; 3]) -> bool {
        let mut sides = [Canonical::new(), Canonical::new()];
        for (first, second) in records {
            let kind = match first.as_bytes()[0] {
                b'n' => 0,
                b'w' => 1,
                _ => 2,
            };
            let id = first[1..first.find(' ').unwrap()].parse().unwrap();
            sides[0].record(kind, id, first.as_bytes());
            sides[1].record(kind, id, second.as_bytes());
        }
        let [a, b] = sides.map(Canonical::finish);
        matches!((a, b), (Some(a), Some(b)) if canonical_pass(&a, &b, start))
    }

    /// The canonical readings decide exactly what the attribution decides,
    /// over the attribution's own cases.
    #[test]
    fn canonical_readings_agree_with_the_attribution() {
        let good: [&[(&str, &str)]; 4] = [
            &[
                ("w1 v1 Nn1,n3", "w1 v1 Nn1,n12"),
                ("w2 v1 Nn3,n2", "w2 v1 Nn12,n2"),
                ("r1 v1 Mn3@a,w1@,r2@", "r1 v1 Mn12@a,w1@,r2@"),
                ("r2 v1 Mr4@", "r2 v1 Mr9@"),
            ],
            &[("w1 v1 Nn0", "w1 v1 Nn0")],
            &[("w1 v1 Nn-4", "w1 v1 Nn3")],
            &[("r1 v1 Mr2@", "r1 v1 Mr2@"), ("r2 v1 M", "r2 v1 M")],
        ];
        for case in good {
            let records = with(case);
            assert_eq!(attribute(&records).0, None, "{case:?}");
            assert!(canonical(&records, [1; 3]), "{case:?}");
        }
        let bad: [&[(&str, &str)]; 10] = [
            &[("n3 v1 x1 y1", "n3 v1 x2 y1")],
            &[("w1 v1 Ta=1 Nn3", "w1 v1 Ta=2 Nn12")],
            &[("w1 v1 Nn1", "w1 v1 Nn2")],
            &[("w1 v1 Nn2", "w1 v1 Nn12")],
            &[("r1 v1 Mn3@a", "r1 v1 Mn12@b")],
            &[("r1 v1 Mn1@,n2@", "r1 v1 Mn1@")],
            &[("w1 v1 Nn3,n4", "w1 v1 Nn12,n12")],
            &[("w1 v1 Nn12", "w1 v1 Nn12"), ("w2 v1 Nn13", "w2 v1 Nn12")],
            &[("w2 v1 Nn1", "w2 v1 Nn1")],
            &[("r1 v1 Mr2@", "r1 v1 Mr7@"), ("r2 v1 M", "r2 v1 M")],
        ];
        for case in bad {
            let records = with(case);
            assert!(attribute(&records).0.is_some(), "{case:?}");
            assert!(!canonical(&records, [1; 3]), "{case:?}");
        }
        // Counting down, and a start the outputs do not begin at.
        let down = [
            ("n-1 v1 x1 y1", "n-1 v1 x1 y1"),
            ("n-2 v1 x1 y1", "n-2 v1 x1 y1"),
            ("w-1 v1 Nn-1,n-3", "w-1 v1 Nn-1,n5"),
            ("w-2 v1 Nn-3,n-2", "w-2 v1 Nn5,n-2"),
        ];
        assert!(canonical(&down, [-1; 3]));
        assert!(!canonical(&down, [1; 3]));
        // A non-canonical decimal makes the reading ineligible.
        assert!(!canonical(&with(&[("w1 v1 Nn01", "w1 v1 Nn01")]), [1; 3]));
    }

    #[test]
    fn start_ids_take_one_value_or_three() {
        assert_eq!(start_ids(None).unwrap(), [1; 3]);
        assert_eq!(start_ids(Some("10")).unwrap(), [10; 3]);
        assert_eq!(start_ids(Some("10,20,30")).unwrap(), [10, 20, 30]);
        crate::test_support::refused(
            start_ids(Some("1,2")),
            "--start-id 1,2 is neither one value nor three",
        );
        crate::test_support::refused(start_ids(Some("x")), "invalid --start-id x");
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
