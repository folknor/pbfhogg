//! `verify diff`'s judgement of one pbfhogg `diff -v` record (its header line
//! and its detail lines) against the two objects osmium read for that
//! identity.
//!
//! The rendering is pbfhogg's documented text contract (its CLI reference,
//! `diff`, the `-v` paragraph): a header `-`, `+` or `*`, type letter, ID and
//! version (`v<old> -> v<new>` when a modification changed a present version);
//! for a modification, a node's `coordinates:` line, a way's ref and a
//! relation's member edit script, then the tags, keyed (`-`, `+`, `~`, `tags:
//! order changed`) or, with a duplicate key on either side, an ordered `tag`
//! edit script. Every text value is a double-quoted literal escaped by the
//! documented rule and every coordinate seven decimals with trailing zeros
//! dropped; both are compared as text against the spelling brokkr builds from
//! osmium's decoded values, so the lexical contract is checked, not only the
//! values.
//!
//! What the judgement can and cannot establish:
//!
//! - Each detail component must account for its whole old-to-new change and
//!   print nothing when unchanged. An edit script must list its removals,
//!   then its insertions, each strictly ascending, in range and naming the
//!   item at that position; the kept items must agree; it must be a shortest
//!   script (the edit distance is recomputed, bounded by the printed length);
//!   and it must hold at most 1000 edits. Which of several shortest scripts
//!   pbfhogg prints is its algorithm's choice and is not checked.
//! - A `replaced (details omitted)` line must name exactly the ranges left
//!   after trimming the common prefix and suffix. It is required when the
//!   shortest script exceeds 1000 edits; below that it is pbfhogg's search
//!   budget that allows it, which is not checked ([`Coverage::budget_replaced`]).
//! - OPL writes a missing version and version 0 alike (`v0`), so a version 0
//!   on either side admits every spelling consistent with either reading
//!   ([`Coverage::version_zero`]).
//! - A member pbfhogg prints with an unknown type (`unknown(raw)/id`) has no
//!   OPL counterpart: the record cannot be compared, an error rather than a
//!   verdict.

use std::collections::HashSet;

use crate::error::Error;
use crate::osc::OscKind;

use crate::verify_diff_opl::{Object, kind_char};

/// pbfhogg's edit-script cap: a longer shortest script prints as a
/// replacement.
const MAX_EDITS: usize = 1000;

/// What osmium's content says pbfhogg must print for one identity.
pub(super) enum Expected<'a> {
    Created(&'a Object),
    Deleted(&'a Object),
    Modified(&'a Object, &'a Object),
}

/// Which renderings the judged records exercised, and the qualifications a
/// PASS carries.
#[derive(Debug, Default)]
pub(super) struct Coverage {
    pub created: bool,
    pub deleted: bool,
    pub version_change: bool,
    pub same_version: bool,
    pub coordinates: bool,
    pub tag_removed: bool,
    pub tag_added: bool,
    pub tag_changed: bool,
    pub tag_order: bool,
    pub duplicate_key_tags: bool,
    pub refs: bool,
    pub members: bool,
    pub replaced: bool,
    /// A version 0 was matched against either spelling.
    pub version_zero: bool,
    /// A replacement line was accepted whose shortest script is within the
    /// edit cap, so only pbfhogg's search budget allows it.
    pub budget_replaced: bool,
}

impl Coverage {
    /// The renderings no judged record exercised.
    pub(super) fn missing(&self) -> Vec<&'static str> {
        [
            (self.created, "created"),
            (self.deleted, "deleted"),
            (self.version_change, "modified with a version change"),
            (self.same_version, "modified at one version"),
            (self.coordinates, "coordinates detail"),
            (self.tag_removed, "removed tag"),
            (self.tag_added, "added tag"),
            (self.tag_changed, "changed tag"),
            (self.tag_order, "tag order change"),
            (self.duplicate_key_tags, "duplicate-key tag script"),
            (self.refs, "way ref script"),
            (self.members, "member script"),
            (self.replaced, "replaced detail"),
        ]
        .into_iter()
        .filter_map(|(seen, name)| (!seen).then_some(name))
        .collect()
    }
}

/// Judge one record: `Ok(None)` when it renders `expected` exactly,
/// `Ok(Some(why))` when it does not, `Err` when it cannot be compared.
pub(super) fn judge(
    expected: &Expected<'_>,
    header: &str,
    detail: &[String],
    coverage: &mut Coverage,
) -> Result<Option<String>, Error> {
    let (headers, object) = match expected {
        Expected::Created(new) => (side_headers('+', new, coverage), *new),
        Expected::Deleted(old) => (side_headers('-', old, coverage), *old),
        Expected::Modified(old, new) => (modified_headers(old, new, coverage), *new),
    };
    if !headers.iter().any(|h| h == header) {
        return Ok(Some(format!(
            "header {header:?}, osmium's content gives {}",
            headers.join(" or ")
        )));
    }
    match expected {
        Expected::Created(_) => coverage.created = true,
        Expected::Deleted(_) => coverage.deleted = true,
        Expected::Modified(old, new) => {
            if old.version == new.version {
                coverage.same_version = true;
            } else {
                coverage.version_change = true;
            }
            return judge_detail(old, new, detail, coverage);
        }
    }
    Ok((!detail.is_empty()).then(|| {
        format!(
            "{} {}{} carries detail lines; only a modification has detail",
            if matches!(expected, Expected::Created(_)) {
                "created"
            } else {
                "deleted"
            },
            kind_char(object.kind),
            object.id
        )
    }))
}

/// The spellings of an OPL version: a present version, or for `v0` both a
/// missing version and version 0.
fn version_spellings(version: u64, coverage: &mut Coverage) -> Vec<Option<u64>> {
    if version == 0 {
        coverage.version_zero = true;
        vec![None, Some(0)]
    } else {
        vec![Some(version)]
    }
}

fn side_headers(prefix: char, object: &Object, coverage: &mut Coverage) -> Vec<String> {
    let base = format!("{prefix}{}{}", kind_char(object.kind), object.id);
    version_spellings(object.version, coverage)
        .into_iter()
        .map(|v| match v {
            Some(v) => format!("{base} v{v}"),
            None => base.clone(),
        })
        .collect()
}

/// pbfhogg's modified header (`write_modified_line`) for every consistent
/// pair of version readings.
fn modified_headers(old: &Object, new: &Object, coverage: &mut Coverage) -> Vec<String> {
    let base = format!("*{}{}", kind_char(new.kind), new.id);
    let mut headers = Vec::new();
    for o in version_spellings(old.version, coverage) {
        for n in version_spellings(new.version, coverage) {
            let header = match (o, n) {
                (Some(o), Some(n)) if o != n => format!("{base} v{o} -> v{n}"),
                (_, Some(v)) | (Some(v), None) => format!("{base} v{v}"),
                (None, None) => base.clone(),
            };
            if !headers.contains(&header) {
                headers.push(header);
            }
        }
    }
    headers
}

fn judge_detail(
    old: &Object,
    new: &Object,
    detail: &[String],
    coverage: &mut Coverage,
) -> Result<Option<String>, Error> {
    let mut at = 0;
    match new.kind {
        OscKind::Node => {
            if old.location != new.location {
                coverage.coordinates = true;
                let expected = coordinates_line(old.location, new.location);
                if detail.first() != Some(&expected) {
                    return Ok(Some(format!(
                        "coordinates detail {:?}, osmium's content gives {expected:?}",
                        detail.first()
                    )));
                }
                at = 1;
            }
        }
        OscKind::Way => {
            let spell = |refs: &[i64]| refs.iter().map(|r| format!("n{r}")).collect::<Vec<_>>();
            let component = Sequence {
                item: "ref",
                list: "refs",
                old: &spell(&old.refs),
                new: &spell(&new.refs),
            };
            let mut seen = false;
            let verdict =
                component.judge(detail, &mut at, &mut seen, &mut replaced_flags(coverage))?;
            coverage.refs |= seen;
            if verdict.is_some() {
                return Ok(verdict);
            }
        }
        OscKind::Relation => {
            let spell = |object: &Object| {
                object
                    .members
                    .iter()
                    .map(|m| format!("{}/{} {}", kind_name(m.kind), m.id, quote(&m.role)))
                    .collect::<Vec<_>>()
            };
            let component = Sequence {
                item: "member",
                list: "members",
                old: &spell(old),
                new: &spell(new),
            };
            let mut seen = false;
            let verdict =
                component.judge(detail, &mut at, &mut seen, &mut replaced_flags(coverage))?;
            coverage.members |= seen;
            if verdict.is_some() {
                return Ok(verdict);
            }
        }
    }
    if let Some(why) = judge_tags(&old.tags, &new.tags, detail, &mut at, coverage)? {
        return Ok(Some(why));
    }
    if let Some(extra) = detail.get(at) {
        return Ok(Some(format!(
            "detail line {extra:?} accounts for no change osmium's content shows"
        )));
    }
    Ok(None)
}

/// The two replacement flags a sequence component may set.
struct ReplacedFlags<'a> {
    replaced: &'a mut bool,
    budget: &'a mut bool,
}

fn replaced_flags(coverage: &mut Coverage) -> ReplacedFlags<'_> {
    ReplacedFlags {
        replaced: &mut coverage.replaced,
        budget: &mut coverage.budget_replaced,
    }
}

fn judge_tags(
    old: &[(String, String)],
    new: &[(String, String)],
    detail: &[String],
    at: &mut usize,
    coverage: &mut Coverage,
) -> Result<Option<String>, Error> {
    let unique = |tags: &[(String, String)]| {
        let mut keys = HashSet::new();
        tags.iter().all(|(k, _)| keys.insert(k.as_str()))
    };
    if !unique(old) || !unique(new) {
        let spell = |tags: &[(String, String)]| {
            tags.iter()
                .map(|(k, v)| format!("{}={}", quote(k), quote(v)))
                .collect::<Vec<_>>()
        };
        let component = Sequence {
            item: "tag",
            list: "tags",
            old: &spell(old),
            new: &spell(new),
        };
        let mut seen = false;
        let verdict = component.judge(detail, at, &mut seen, &mut replaced_flags(coverage))?;
        coverage.duplicate_key_tags |= seen;
        return Ok(verdict);
    }
    let expected = keyed_tag_lines(old, new, coverage);
    let rest = &detail[(*at).min(detail.len())..];
    let shown = rest.len().min(expected.len());
    if rest[..shown] != expected[..shown] || rest.len() < expected.len() {
        let position = rest
            .iter()
            .zip(&expected)
            .position(|(a, b)| a != b)
            .unwrap_or(shown);
        return Ok(Some(format!(
            "tag detail line {} is {:?}, osmium's content gives {:?}",
            position + 1,
            rest.get(position),
            expected.get(position)
        )));
    }
    *at += expected.len();
    Ok(None)
}

/// The keyed tag detail pbfhogg prints for unique keys on both sides:
/// removed keys in old order, added keys in new order, changed values in new
/// order, then one line when the keys present on both sides changed order.
fn keyed_tag_lines(
    old: &[(String, String)],
    new: &[(String, String)],
    coverage: &mut Coverage,
) -> Vec<String> {
    let in_new = |k: &str| new.iter().find(|(nk, _)| nk == k).map(|(_, v)| v);
    let in_old = |k: &str| old.iter().find(|(ok, _)| ok == k).map(|(_, v)| v);
    let mut lines = Vec::new();
    for (k, v) in old {
        if in_new(k).is_none() {
            coverage.tag_removed = true;
            lines.push(format!("  -{}={}", quote(k), quote(v)));
        }
    }
    for (k, v) in new {
        if in_old(k).is_none() {
            coverage.tag_added = true;
            lines.push(format!("  +{}={}", quote(k), quote(v)));
        }
    }
    for (k, v) in new {
        if let Some(was) = in_old(k)
            && was != v
        {
            coverage.tag_changed = true;
            lines.push(format!("  ~{}: {} -> {}", quote(k), quote(was), quote(v)));
        }
    }
    let common_old = old
        .iter()
        .filter(|(k, _)| in_new(k).is_some())
        .map(|(k, _)| k);
    let common_new = new
        .iter()
        .filter(|(k, _)| in_old(k).is_some())
        .map(|(k, _)| k);
    if !common_old.eq(common_new) {
        coverage.tag_order = true;
        lines.push("  tags: order changed".to_owned());
    }
    lines
}

/// One ordered component (refs, members, duplicate-key tags), its items
/// already spelled as pbfhogg prints them, so equal spellings are equal
/// items.
struct Sequence<'a> {
    item: &'static str,
    list: &'static str,
    old: &'a [String],
    new: &'a [String],
}

impl Sequence<'_> {
    /// Consume this component's lines from `detail` at `at` and judge them.
    fn judge(
        &self,
        detail: &[String],
        at: &mut usize,
        seen: &mut bool,
        flags: &mut ReplacedFlags<'_>,
    ) -> Result<Option<String>, Error> {
        let removal = format!("  -{} @", self.item);
        let insertion = format!("  +{} @", self.item);
        let replacement = format!("  {}: ", self.list);
        let start = *at;
        while detail.get(*at).is_some_and(|line| {
            line.starts_with(&removal)
                || line.starts_with(&insertion)
                || line.starts_with(&replacement)
        }) {
            *at += 1;
        }
        let lines = &detail[start..*at];
        if self.old == self.new {
            return Ok(lines
                .first()
                .map(|line| format!("{} detail {line:?} for unchanged {}", self.item, self.list)));
        }
        *seen = true;
        if lines.is_empty() {
            return Ok(Some(format!(
                "no {} detail, while osmium's content shows the {} changed",
                self.item, self.list
            )));
        }
        if let [line] = lines
            && line.starts_with(&replacement)
        {
            *flags.replaced = true;
            let expected = self.replacement_line();
            if *line != expected {
                return Ok(Some(format!(
                    "replacement {line:?}, osmium's content gives {expected:?}"
                )));
            }
            if shortest_edit(self.old, self.new, MAX_EDITS).is_some() {
                *flags.budget = true;
            }
            return Ok(None);
        }
        self.judge_script(lines, &removal, &insertion)
    }

    fn replacement_line(&self) -> String {
        let (n, m) = (self.old.len(), self.new.len());
        let prefix = self
            .old
            .iter()
            .zip(self.new)
            .take_while(|(a, b)| a == b)
            .count();
        let suffix = self.old[prefix..]
            .iter()
            .rev()
            .zip(self.new[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        format!(
            "  {}: {n} -> {m}, old @{prefix}..{} -> new @{prefix}..{} replaced (details omitted)",
            self.list,
            n - suffix,
            m - suffix
        )
    }

    fn judge_script(
        &self,
        lines: &[String],
        removal: &str,
        insertion: &str,
    ) -> Result<Option<String>, Error> {
        let mut removed = Vec::new();
        let mut inserted = Vec::new();
        for line in lines {
            let (into, list, rest) = if let Some(rest) = line.strip_prefix(removal) {
                if !inserted.is_empty() {
                    return Ok(Some(format!("removal {line:?} after an insertion")));
                }
                (&mut removed, self.old, rest)
            } else if let Some(rest) = line.strip_prefix(insertion) {
                (&mut inserted, self.new, rest)
            } else {
                return Ok(Some(format!("{line:?} mixed into an edit script")));
            };
            let Some((index, value)) = rest.split_once(' ') else {
                return Ok(Some(format!("{line:?} is not an edit line")));
            };
            if value.starts_with("unknown(") {
                return Err(Error::Failed(format!(
                    "pbfhogg prints a member of unknown type ({line:?}), which osmium's OPL \
                     cannot carry; diff cannot be compared on this input"
                )));
            }
            let Some(index) = index
                .parse::<usize>()
                .ok()
                .filter(|i| i.to_string() == index)
            else {
                return Ok(Some(format!("{line:?} has no position")));
            };
            if into.last().is_some_and(|&last| index <= last) {
                return Ok(Some(format!("{line:?} does not ascend")));
            }
            if list.get(index).map(String::as_str) != Some(value) {
                return Ok(Some(format!(
                    "{line:?} names {value}, osmium's content has {:?} at @{index}",
                    list.get(index)
                )));
            }
            into.push(index);
        }
        let edits = removed.len() + inserted.len();
        if edits > MAX_EDITS {
            return Ok(Some(format!(
                "{edits} {} edits itemised, past the {MAX_EDITS} that print as a replacement",
                self.item
            )));
        }
        let kept = |list: &[String], dropped: &[usize]| {
            list.iter()
                .enumerate()
                .filter(|(i, _)| dropped.binary_search(i).is_err())
                .map(|(_, v)| v.clone())
                .collect::<Vec<_>>()
        };
        if kept(self.old, &removed) != kept(self.new, &inserted) {
            return Ok(Some(format!(
                "the {} script does not turn the old {} into the new",
                self.item, self.list
            )));
        }
        if let Some(shortest) = shortest_edit(self.old, self.new, edits)
            && shortest < edits
        {
            return Ok(Some(format!(
                "the {} script has {edits} edits where {shortest} suffice",
                self.item
            )));
        }
        Ok(None)
    }
}

/// The number of removals plus insertions in a shortest edit script from
/// `a` to `b`, or `None` when it exceeds `cap` (Myers' greedy search, its
/// work bounded by the cap).
pub(super) fn shortest_edit<T: Eq>(a: &[T], b: &[T], cap: usize) -> Option<usize> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let (a, b) = (&a[prefix..], &b[prefix..]);
    let suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (a, b) = (&a[..a.len() - suffix], &b[..b.len() - suffix]);
    if a.is_empty() || b.is_empty() {
        let d = a.len() + b.len();
        return (d <= cap).then_some(d);
    }
    let (n, m) = (a.len(), b.len());
    let limit = (n + m).min(cap);
    // v[k + offset] is the furthest x on diagonal k = x - y.
    let offset = limit + 1;
    let mut v = vec![0_usize; 2 * limit + 3];
    for d in 0..=limit {
        for step in 0..=d {
            // k runs -d, -d + 2, ..., d; index = k + offset.
            let index = offset + 2 * step - d;
            let mut x = if step == 0 || (step != d && v[index - 1] < v[index + 1]) {
                v[index + 1]
            } else {
                v[index - 1] + 1
            };
            // y = x - k = x - (index - offset).
            let mut y = (x + offset).checked_sub(index)?;
            while x < n && y < m && a[x] == b[y] {
                x += 1;
                y += 1;
            }
            v[index] = x;
            if x >= n && y >= m {
                return Some(d);
            }
        }
    }
    None
}

fn kind_name(kind: OscKind) -> &'static str {
    match kind {
        OscKind::Node => "node",
        OscKind::Way => "way",
        OscKind::Relation => "relation",
    }
}

/// pbfhogg's coordinates detail line, latitude first.
fn coordinates_line(old: Option<(i64, i64)>, new: Option<(i64, i64)>) -> String {
    let spell = |location: Option<(i64, i64)>| match location {
        Some((lat, lon)) => format!("({}, {})", coordinate(lat), coordinate(lon)),
        None => "(?, ?)".to_owned(),
    };
    format!("  coordinates: {} -> {}", spell(old), spell(new))
}

/// A decimicrodegree count as pbfhogg prints it: seven decimals with
/// trailing zeros, then a bare point, dropped.
pub(super) fn coordinate(value: i64) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let magnitude = value.unsigned_abs();
    let (whole, fraction) = (magnitude / 10_000_000, magnitude % 10_000_000);
    if fraction == 0 {
        return format!("{sign}{whole}");
    }
    let digits = format!("{fraction:07}");
    format!("{sign}{whole}.{}", digits.trim_end_matches('0'))
}

/// A string as pbfhogg's detail prints it: a double-quoted literal that is
/// also a JSON string, `"` and `\` backslashed, newline, carriage return and
/// tab as `\n`, `\r`, `\t`, and every other control character, U+2028,
/// U+2029 and the bidi controls as `\u` with four lowercase hex digits.
pub(super) fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}'
            | '\u{2029}'
            | '\u{061C}'
            | '\u{200E}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}' => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::verify_diff_opl::parse_object;

    fn object(line: &str) -> Object {
        parse_object(line.as_bytes()).unwrap()
    }

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|&s| s.to_owned()).collect()
    }

    fn verdict(old: &str, new: &str, header: &str, detail: &[&str]) -> Option<String> {
        let (old, new) = (object(old), object(new));
        let mut coverage = Coverage::default();
        judge(
            &Expected::Modified(&old, &new),
            header,
            &lines(detail),
            &mut coverage,
        )
        .unwrap()
    }

    #[test]
    fn spelling_follows_the_documented_rules() {
        assert_eq!(
            quote("a\"b\\c\nd\u{7}\u{85}\u{202e}\u{e9}"),
            "\"a\\\"b\\\\c\\nd\\u0007\\u0085\\u202e\u{e9}\""
        );
        for (value, text) in [
            (0, "0"),
            (125_000_000, "12.5"),
            (-1, "-0.0000001"),
            (-1_800_000_000, "-180"),
            (551_234_567, "55.1234567"),
        ] {
            assert_eq!(coordinate(value), text);
        }
    }

    #[test]
    fn headers_follow_versions_and_their_ambiguity() {
        assert_eq!(
            verdict(
                "n1 v1 T x1 y2",
                "n1 v2 T x1 y3",
                "*n1 v1 -> v2",
                &["  coordinates: (2, 1) -> (3, 1)"]
            ),
            None
        );
        assert_eq!(
            verdict(
                "n1 v1 T x1 y2",
                "n1 v1 T x1 y3",
                "*n1 v1",
                &["  coordinates: (2, 1) -> (3, 1)"]
            ),
            None
        );
        assert!(
            verdict(
                "n1 v1 T x1 y2",
                "n1 v2 T x1 y3",
                "*n1 v2",
                &["  coordinates: (2, 1) -> (3, 1)"]
            )
            .is_some()
        );
        // v0 on the old side: a missing version prints the new one alone.
        for header in ["*n1 v5", "*n1 v0 -> v5"] {
            assert_eq!(
                verdict(
                    "n1 v0 T x1 y2",
                    "n1 v5 T x1 y3",
                    header,
                    &["  coordinates: (2, 1) -> (3, 1)"]
                ),
                None,
                "{header}"
            );
        }
        for header in ["*n1", "*n1 v0"] {
            assert_eq!(
                verdict(
                    "n1 v0 T x1 y2",
                    "n1 v0 T x1 y3",
                    header,
                    &["  coordinates: (2, 1) -> (3, 1)"]
                ),
                None,
                "{header}"
            );
        }
    }

    #[test]
    fn keyed_tags_are_fully_determined() {
        let old = "n1 v1 Ta=1,b=2,c=3 x0 y0";
        let new = "n1 v2 Tc=3,b=9,d=%22% x0 y0";
        let detail = [
            "  -\"a\"=\"1\"",
            "  +\"d\"=\"\\\"\"",
            "  ~\"b\": \"2\" -> \"9\"",
            "  tags: order changed",
        ];
        assert_eq!(verdict(old, new, "*n1 v1 -> v2", &detail), None);
        assert!(verdict(old, new, "*n1 v1 -> v2", &detail[..3]).is_some());
        assert!(
            verdict(
                old,
                new,
                "*n1 v1 -> v2",
                &[detail[1], detail[0], detail[2], detail[3]]
            )
            .is_some()
        );
        // An unchanged component prints nothing.
        assert!(
            verdict(
                "n1 v1 Ta=1 x0 y0",
                "n1 v2 Ta=2 x0 y1",
                "*n1 v1 -> v2",
                &["  ~\"a\": \"1\" -> \"2\""]
            )
            .is_some()
        );
    }

    #[test]
    fn edit_scripts_must_be_faithful_and_shortest() {
        let old = "w1 v1 T Nn1,n2,n3";
        let new = "w1 v2 T Nn2,n3,n1";
        let header = "*w1 v1 -> v2";
        assert_eq!(
            verdict(old, new, header, &["  -ref @0 n1", "  +ref @2 n1"]),
            None
        );
        // Wrong value, wrong position, not shortest, insertion first.
        assert!(verdict(old, new, header, &["  -ref @0 n2", "  +ref @2 n1"]).is_some());
        assert!(verdict(old, new, header, &["  -ref @1 n2", "  +ref @2 n1"]).is_some());
        assert!(
            verdict(
                old,
                new,
                header,
                &[
                    "  -ref @0 n1",
                    "  -ref @1 n2",
                    "  +ref @1 n2",
                    "  +ref @2 n1"
                ]
            )
            .is_some()
        );
        assert!(verdict(old, new, header, &["  +ref @2 n1", "  -ref @0 n1"]).is_some());
        // Missing script for a changed list.
        assert!(verdict(old, new, header, &[]).is_some());
        // The replacement names the trimmed middle exactly.
        assert_eq!(
            verdict(
                old,
                new,
                header,
                &["  refs: 3 -> 3, old @0..3 -> new @0..3 replaced (details omitted)"]
            ),
            None
        );
        assert!(
            verdict(
                old,
                new,
                header,
                &["  refs: 3 -> 3, old @0..2 -> new @0..3 replaced (details omitted)"]
            )
            .is_some()
        );
    }

    #[test]
    fn members_and_duplicate_key_tags_are_scripts() {
        let old = "r1 v1 Ta=1,a=2 Mw3@outer,n4@";
        let new = "r1 v2 Ta=1 Mn4@,w3@outer";
        let detail = [
            "  -member @0 way/3 \"outer\"",
            "  +member @1 way/3 \"outer\"",
            "  -tag @1 \"a\"=\"2\"",
        ];
        assert_eq!(verdict(old, new, "*r1 v1 -> v2", &detail), None);
        let (old, new) = (object(old), object(new));
        let mut coverage = Coverage::default();
        let unknown = lines(&["  -member @0 unknown(9)/3 \"outer\""]);
        assert!(
            judge(
                &Expected::Modified(&old, &new),
                "*r1 v1 -> v2",
                &unknown,
                &mut coverage
            )
            .is_err()
        );
    }

    #[test]
    fn created_and_deleted_carry_no_detail() {
        let node = object("n7 v3 T x0 y0");
        let mut coverage = Coverage::default();
        assert_eq!(
            judge(&Expected::Created(&node), "+n7 v3", &[], &mut coverage).unwrap(),
            None
        );
        assert!(
            judge(&Expected::Deleted(&node), "+n7 v3", &[], &mut coverage)
                .unwrap()
                .is_some()
        );
        assert!(
            judge(
                &Expected::Deleted(&node),
                "-n7 v3",
                &lines(&["  x"]),
                &mut coverage
            )
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn shortest_edit_counts_removals_plus_insertions() {
        let s = |t: &str| t.chars().collect::<Vec<_>>();
        assert_eq!(shortest_edit(&s("abc"), &s("abc"), 10), Some(0));
        assert_eq!(shortest_edit(&s("abc"), &s("bca"), 10), Some(2));
        assert_eq!(shortest_edit(&s("abcabba"), &s("cbabac"), 10), Some(5));
        assert_eq!(shortest_edit(&s("abcabba"), &s("cbabac"), 4), None);
        assert_eq!(shortest_edit(&s(""), &s("xyz"), 3), Some(3));
        assert_eq!(shortest_edit(&s(""), &s("xyz"), 2), None);
    }
}
