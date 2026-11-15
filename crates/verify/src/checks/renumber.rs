//! Verify: renumber - `pbfhogg renumber` vs `osmium renumber`.
//!
//! Cross-validates pbfhogg's renumber output against osmium. The two tools
//! name orphan references (a way ref or relation member whose target is not
//! in the input) differently, a documented deviation (pbfhogg
//! `DEVIATIONS.md`, "renumber: orphan-reference handling"): pbfhogg writes
//! `start + count + old`, osmium `start + count + i` in first-encounter
//! order. Both put every orphan at or above `start + count` of its kind,
//! where no output object lives. Each difference is proved to be that and
//! nothing else, record by record; there is no tolerance.
//!
//! The comparison is the verifier's merge of osmium's decodings of both
//! outputs (`crate::opl_compare`, metadata included), so both must be in
//! strict order and every object must pair by kind and ID. It passes when:
//! - every object pairs, and each kind's objects are exactly the IDs
//!   `start, start + 1, ...` on both sides, which fixes each kind's live
//!   range `start .. start + count`
//! - no node differs
//! - a differing way or relation differs only in the ref IDs of its node or
//!   member list: same fields, same list length, same member types and
//!   encoded roles or coordinate suffixes
//! - every reference, in every record, alike or not, is either the same
//!   live ID on both sides or at or above its kind's live range on both
//! - the orphan names correspond one to one per kind across the whole file:
//!   one osmium orphan is one pbfhogg orphan, and the reverse
//!
//! That proves the two outputs the same reference graph up to the naming of
//! orphans. It does not prove pbfhogg's shift formula: that would need the
//! input's references, which the comparison does not read.
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
use crate::reference::ToolRun;

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
/// `node,way,relation`; 1 when absent.
fn start_ids(start_id: Option<&str>) -> Result<[i128; 3], Error> {
    let Some(value) = start_id else {
        return Ok([1; 3]);
    };
    let parse = |part: &str| {
        part.trim()
            .parse::<i64>()
            .map(i128::from)
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
    /// passed a kind, its live range's exclusive end.
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
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let id = std::str::from_utf8(&rest[..digits]).ok()?.parse().ok()?;
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
        self.next[pair.kind] += 1;
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
        let (wp, wo) = (i128::from(p), i128::from(o));
        let name = KINDS[kind];
        if p == o && (start..end).contains(&wp) {
            return Ok(());
        }
        if wp < end || wo < end {
            return Err(format!(
                "{name} ref pbfhogg {p} osmium {o} is not one live {name} on both sides nor an \
                 orphan on both (live {name}s {start}..{end})"
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

    // --- The element comparison, classified by kind ---------------------
    let policy = Policy::between(&osmium_out, &pbfhogg_out)?;
    let mut classified = Classified::default();
    let mut attribution = Attribution::new(start_ids(start_id)?);
    let merged = opl_compare::compare_paired(
        harness,
        &osmium_out,
        &pbfhogg_out,
        &diff_log,
        policy,
        |g| classified.add(g),
        |p| attribution.pair(p),
    )?;
    let (unexplained, orphan_records, orphans) = attribution.finish();

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
            Ok(Some(
                "every object at its renumbered ID and identical as osmium decodes it, except \
                 orphan reference IDs, which lie past the live range on both sides and \
                 correspond one to one; pbfhogg's orphan shift formula is not checked against \
                 the input"
                    .into(),
            ))
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
        // Two nodes, so node orphans start at 3; pbfhogg names old node 9
        // 3 + 9 = 12, osmium names it 3.
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
        // An equal ref below the start names nothing.
        assert!(attribute(&with(&[("w1 v1 Nn0", "w1 v1 Nn0")])).0.is_some());
        // A relation member naming a relation is judged once every relation
        // is known: when r1 is read r2 is not yet, but it is live, so the
        // two sides must name it alike.
        let renamed = with(&[("r1 v1 Mr2@", "r1 v1 Mr7@"), ("r2 v1 M", "r2 v1 M")]);
        assert!(attribute(&renamed).0.is_some());
        let alike = with(&[("r1 v1 Mr2@", "r1 v1 Mr2@"), ("r2 v1 M", "r2 v1 M")]);
        assert_eq!(attribute(&alike).0, None);
    }

    #[test]
    fn start_ids_take_one_value_or_three() {
        assert_eq!(start_ids(None).unwrap(), [1; 3]);
        assert_eq!(start_ids(Some("10")).unwrap(), [10; 3]);
        assert_eq!(start_ids(Some("10,20,30")).unwrap(), [10, 20, 30]);
        assert!(start_ids(Some("1,2")).is_err());
        assert!(start_ids(Some("x")).is_err());
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
