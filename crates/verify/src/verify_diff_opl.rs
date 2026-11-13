//! Strict streaming readers for the OPL osmium writes in `verify diff`, read
//! for the content pbfhogg's `diff` compares: [`DiffGroups`] over
//! `osmium diff -c -f opl` (every line `-` or `+` and an object), grouped by
//! identity, and [`ObjectReader`] over a plain OPL file (`osmium getid -f
//! opl`).
//!
//! An object is read in full: kind and signed ID, version, a node's location
//! in decimicrodegrees, the tags in order with osmium's `%hex%` escapes
//! decoded, a way's node references in order (any inline location dropped)
//! and a relation's members in order, each as type, signed reference and
//! decoded role. Content is what pbfhogg's `crates/lib/src/content.rs` defines as
//! element content: location and tags for a node, tags and references for a
//! way, tags and members for a relation; metadata never.
//!
//! The readers are strict because what they yield decides a verdict: a line
//! without its newline, a field they do not know, a field twice, a required
//! field missing, a value they cannot read, a deleted (history) object, and
//! identities out of order are each an error, never a record skipped.
//! Identities ascend in osmium's object order, which is pbfhogg's canonical
//! one: kind (node, way, relation), then ID zero, the negative IDs by
//! magnitude and the positive ones by magnitude ([`order_key`]).

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::osc::OscKind;

/// One relation member: its type, signed reference and decoded role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Member {
    pub kind: OscKind,
    pub id: i64,
    pub role: String,
}

/// One OSM object as osmium's OPL carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Object {
    pub kind: OscKind,
    pub id: i64,
    /// OPL's `v`: 0 both for an object without a version and for one with
    /// version 0, which OPL cannot tell apart.
    pub version: u64,
    /// A node's `(lat, lon)` in decimicrodegrees; `None` for ways and
    /// relations.
    pub location: Option<(i64, i64)>,
    pub tags: Vec<(String, String)>,
    pub refs: Vec<i64>,
    pub members: Vec<Member>,
}

impl Object {
    /// pbfhogg's content equality: location, tags, references and members,
    /// each in order; never metadata.
    pub(super) fn same_content(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.location == other.location
            && self.tags == other.tags
            && self.refs == other.refs
            && self.members == other.members
    }
}

/// The canonical position of an identity: kind, then ID zero, the negative
/// IDs by magnitude, the positive IDs by magnitude. libosmium orders objects
/// so (type, `id > 0`, absolute ID) and pbfhogg's `osm_id_cmp` the same way.
pub(super) fn order_key(kind: OscKind, id: i64) -> (usize, bool, u64) {
    (kind.index(), id > 0, id.unsigned_abs())
}

/// The OPL type letter of a kind, which is also pbfhogg's.
pub(super) fn kind_char(kind: OscKind) -> char {
    match kind {
        OscKind::Node => 'n',
        OscKind::Way => 'w',
        OscKind::Relation => 'r',
    }
}

fn kind_of(byte: u8) -> Option<OscKind> {
    match byte {
        b'n' => Some(OscKind::Node),
        b'w' => Some(OscKind::Way),
        b'r' => Some(OscKind::Relation),
        _ => None,
    }
}

/// A line reader that refuses a last line without its newline.
struct Lines {
    path: PathBuf,
    reader: BufReader<File>,
    line: Vec<u8>,
    number: u64,
}

impl Lines {
    fn open(path: &Path) -> Result<Self, Error> {
        let file = File::open(path)
            .map_err(|e| Error::Failed(format!("cannot open OPL {}: {e}", path.display())))?;
        Ok(Self {
            path: path.to_path_buf(),
            reader: BufReader::new(file),
            line: Vec::new(),
            number: 0,
        })
    }

    /// The next line without its newline, or `None` at the end.
    fn next(&mut self) -> Result<Option<&[u8]>, Error> {
        self.line.clear();
        let read = self
            .reader
            .read_until(b'\n', &mut self.line)
            .map_err(|e| Error::Failed(format!("cannot read OPL {}: {e}", self.path.display())))?;
        if read == 0 {
            return Ok(None);
        }
        self.number += 1;
        match self.line.strip_suffix(b"\n") {
            Some(body) => Ok(Some(body)),
            None => Err(self.error("no terminating newline (truncated?)")),
        }
    }

    fn error(&self, why: &str) -> Error {
        Error::Failed(format!(
            "OPL {} line {}: {why}",
            self.path.display(),
            self.number
        ))
    }
}

/// One identity of an `osmium diff -f opl` listing: the object of the first
/// input (`-`) and of the second (`+`), at least one present.
#[derive(Debug)]
pub(super) struct Group {
    pub kind: OscKind,
    pub id: i64,
    pub old: Option<Object>,
    pub new: Option<Object>,
}

/// The groups of an `osmium diff --suppress-common -f opl` listing, in
/// order. osmium prints an identity on both sides as a `-` and a `+` line
/// (in its object order, so a version going backwards prints `+` first) and
/// a one-sided one as one line. A third line for an identity, a common (` `)
/// line, or an identity out of order is an error.
pub(super) struct DiffGroups {
    lines: Lines,
    /// The line read past the end of the previous group.
    pending: Option<(u8, Object)>,
    last: Option<(usize, bool, u64)>,
    /// `-` and `+` lines read so far.
    pub minus: u64,
    pub plus: u64,
}

impl DiffGroups {
    pub(super) fn open(path: &Path) -> Result<Self, Error> {
        Ok(Self {
            lines: Lines::open(path)?,
            pending: None,
            last: None,
            minus: 0,
            plus: 0,
        })
    }

    fn read_line(&mut self) -> Result<Option<(u8, Object)>, Error> {
        let Some(line) = self.lines.next()? else {
            return Ok(None);
        };
        let (side, body) = match line.split_first() {
            Some((&side @ (b'-' | b'+'), body)) => (side, body),
            _ => {
                return Err(self.lines.error(
                    "not a - or + line (a common line means --suppress-common was not applied)",
                ));
            }
        };
        let object = parse_object(body).map_err(|why| self.lines.error(&why))?;
        if side == b'-' {
            self.minus += 1;
        } else {
            self.plus += 1;
        }
        Ok(Some((side, object)))
    }

    pub(super) fn next_group(&mut self) -> Result<Option<Group>, Error> {
        let first = match self.pending.take() {
            Some(line) => line,
            None => match self.read_line()? {
                Some(line) => line,
                None => return Ok(None),
            },
        };
        let key = order_key(first.1.kind, first.1.id);
        if self.last.is_some_and(|last| key <= last) {
            return Err(self.lines.error(&format!(
                "{}{} does not follow the previous identity in object order",
                kind_char(first.1.kind),
                first.1.id
            )));
        }
        self.last = Some(key);
        let mut group = Group {
            kind: first.1.kind,
            id: first.1.id,
            old: None,
            new: None,
        };
        place(&mut group, first);
        loop {
            let Some(line) = self.read_line()? else {
                return Ok(Some(group));
            };
            if line.1.kind != group.kind || line.1.id != group.id {
                self.pending = Some(line);
                return Ok(Some(group));
            }
            if !place(&mut group, line) {
                return Err(self.lines.error(&format!(
                    "{}{} appears twice on one side: each input holds one object per identity",
                    kind_char(group.kind),
                    group.id
                )));
            }
        }
    }
}

/// Put a listing line's object on its side of `group`; false when that side
/// already held one.
fn place(group: &mut Group, (side, object): (u8, Object)) -> bool {
    let slot = if side == b'-' {
        &mut group.old
    } else {
        &mut group.new
    };
    slot.replace(object).is_none()
}

/// The objects of a plain OPL file, identities strictly ascending in object
/// order.
pub(super) struct ObjectReader {
    lines: Lines,
    last: Option<(usize, bool, u64)>,
}

impl ObjectReader {
    pub(super) fn open(path: &Path) -> Result<Self, Error> {
        Ok(Self {
            lines: Lines::open(path)?,
            last: None,
        })
    }

    pub(super) fn next_object(&mut self) -> Result<Option<Object>, Error> {
        let Some(line) = self.lines.next()? else {
            return Ok(None);
        };
        let object = parse_object(line).map_err(|why| self.lines.error(&why))?;
        let key = order_key(object.kind, object.id);
        if self.last.is_some_and(|last| key <= last) {
            return Err(self.lines.error(&format!(
                "{}{} does not follow the previous object in object order",
                kind_char(object.kind),
                object.id
            )));
        }
        self.last = Some(key);
        Ok(Some(object))
    }
}

/// One OPL object line (no prefix, no newline).
pub(super) fn parse_object(line: &[u8]) -> Result<Object, String> {
    let line = std::str::from_utf8(line).map_err(|e| format!("not UTF-8: {e}"))?;
    let mut fields = line.split(' ');
    let first = fields.next().unwrap_or_default();
    let (kind, id) = first
        .as_bytes()
        .split_first()
        .and_then(|(&k, _)| kind_of(k))
        .zip(first.get(1..).and_then(parse_signed))
        .ok_or_else(|| format!("{first:?} is not an object type and ID"))?;
    let name = format!("{}{id}", kind_char(kind));
    let mut version = None;
    let mut seen = Vec::new();
    let (mut tags, mut x, mut y, mut refs, mut members) = (None, None, None, None, None);
    for field in fields {
        let Some(letter) = field.chars().next() else {
            return Err(format!("{name}: an empty field"));
        };
        let value = &field[letter.len_utf8()..];
        if seen.contains(&letter) {
            return Err(format!("{name}: field {letter} twice"));
        }
        seen.push(letter);
        match letter {
            'v' => {
                version = Some(
                    parse_unsigned(value).ok_or_else(|| format!("{name}: version {value:?}"))?,
                );
            }
            'd' => {
                if value != "V" {
                    return Err(format!(
                        "{name}: visibility {value:?}; history files are not compared"
                    ));
                }
            }
            // Metadata: not content.
            'c' | 't' | 'i' | 'u' => {}
            'T' => tags = Some(parse_tags(value).map_err(|why| format!("{name}: {why}"))?),
            'x' => x = Some(value),
            'y' => y = Some(value),
            'N' => refs = Some(parse_refs(value).map_err(|why| format!("{name}: {why}"))?),
            'M' => members = Some(parse_members(value).map_err(|why| format!("{name}: {why}"))?),
            other => return Err(format!("{name}: unknown field {other:?}")),
        }
    }
    let version = version.ok_or_else(|| format!("{name}: no version field"))?;
    let tags = tags.ok_or_else(|| format!("{name}: no tag field"))?;
    let location = match (kind, x, y) {
        (OscKind::Node, Some(x), Some(y)) => {
            let lon = parse_decimicro(x).ok_or_else(|| format!("{name}: longitude {x:?}"))?;
            let lat = parse_decimicro(y).ok_or_else(|| format!("{name}: latitude {y:?}"))?;
            Some((lat, lon))
        }
        (OscKind::Node, _, _) => return Err(format!("{name}: a node without x and y")),
        (_, None, None) => None,
        _ => return Err(format!("{name}: a location on a non-node")),
    };
    let refs = match (kind, refs) {
        (OscKind::Way, Some(refs)) => refs,
        (OscKind::Way, None) => return Err(format!("{name}: a way without a node field")),
        (_, None) => Vec::new(),
        _ => return Err(format!("{name}: a node field on a non-way")),
    };
    let members = match (kind, members) {
        (OscKind::Relation, Some(members)) => members,
        (OscKind::Relation, None) => {
            return Err(format!("{name}: a relation without a member field"));
        }
        (_, None) => Vec::new(),
        _ => return Err(format!("{name}: a member field on a non-relation")),
    };
    Ok(Object {
        kind,
        id,
        version,
        location,
        tags,
        refs,
        members,
    })
}

fn parse_unsigned(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// A decimal integer with an optional minus sign.
fn parse_signed(text: &str) -> Option<i64> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// A coordinate as osmium writes it, `-`, digits, and up to seven decimals,
/// as an exact decimicrodegree count. An empty value (an invalid location)
/// and anything outside [-180, 180] is refused.
pub(super) fn parse_decimicro(text: &str) -> Option<i64> {
    let (negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (whole, fraction) = match unsigned.split_once('.') {
        Some((whole, fraction)) if !fraction.is_empty() => (whole, fraction),
        Some(_) => return None,
        None => (unsigned, ""),
    };
    if whole.is_empty()
        || fraction.len() > 7
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
        || whole.len() > 3
    {
        return None;
    }
    let whole: i64 = whole.parse().ok()?;
    let mut fraction_value: i64 = 0;
    for position in 0..7 {
        let digit = fraction.as_bytes().get(position).map_or(0, |d| d - b'0');
        fraction_value = fraction_value * 10 + i64::from(digit);
    }
    let value = whole * 10_000_000 + fraction_value;
    let value = if negative { -value } else { value };
    (value.abs() <= 1_800_000_000).then_some(value)
}

/// osmium's escape: a character outside its plain set is written as `%`, its
/// code point in hex, `%`.
pub(super) fn decode(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('%') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let end = after
            .find('%')
            .ok_or_else(|| format!("unterminated escape in {text:?}"))?;
        let hex = &after[..end];
        let point = (!hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| u32::from_str_radix(hex, 16).ok())
            .flatten()
            .and_then(char::from_u32)
            .ok_or_else(|| format!("bad escape %{hex}% in {text:?}"))?;
        out.push(point);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The `T` field: comma-separated `key=value` pairs, both escaped, or
/// nothing.
fn parse_tags(value: &str) -> Result<Vec<(String, String)>, String> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|pair| {
            let (k, v) = pair
                .split_once('=')
                .filter(|(_, v)| !v.contains('='))
                .ok_or_else(|| format!("tag {pair:?} is not key=value"))?;
            Ok((decode(k)?, decode(v)?))
        })
        .collect()
}

/// The `N` field: comma-separated `n<ref>` tokens, each perhaps followed by
/// an inline location, which is not content and is dropped.
fn parse_refs(value: &str) -> Result<Vec<i64>, String> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|token| {
            let rest = token
                .strip_prefix('n')
                .ok_or_else(|| format!("node ref {token:?}"))?;
            let (reference, location) = match rest.split_once('x') {
                Some((reference, location)) => (reference, Some(location)),
                None => (rest, None),
            };
            // An inline location is `x<lon>y<lat>`, both empty when invalid;
            // it is not content, but it must read.
            let location_reads = location.is_none_or(|location| {
                location.split_once('y').is_some_and(|(x, y)| {
                    (x.is_empty() && y.is_empty())
                        || (parse_decimicro(x).is_some() && parse_decimicro(y).is_some())
                })
            });
            parse_signed(reference)
                .filter(|_| location_reads)
                .ok_or_else(|| format!("node ref {token:?}"))
        })
        .collect()
}

/// The `M` field: comma-separated `<type><ref>@<role>` tokens, the role
/// escaped, or nothing.
fn parse_members(value: &str) -> Result<Vec<Member>, String> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|token| {
            let bad = || format!("member {token:?} is not <type><ref>@<role>");
            let (head, role) = token.split_once('@').ok_or_else(bad)?;
            let kind = head
                .as_bytes()
                .first()
                .copied()
                .and_then(kind_of)
                .ok_or_else(bad)?;
            let id = head.get(1..).and_then(parse_signed).ok_or_else(bad)?;
            Ok(Member {
                kind,
                id,
                role: decode(role)?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    #[test]
    fn objects_carry_their_full_content() {
        let node = parse_object(
            b"n-5 v3 dV c1 t2020-01-01T00:00:00Z i1 ua%20%b Tname=a%2c%b,k=%25% x-12.5 y55.1234567",
        )
        .unwrap();
        assert_eq!((node.kind, node.id, node.version), (OscKind::Node, -5, 3));
        assert_eq!(node.location, Some((551_234_567, -125_000_000)));
        assert_eq!(
            node.tags,
            vec![
                ("name".to_owned(), "a,b".to_owned()),
                ("k".to_owned(), "%".to_owned())
            ]
        );
        let way = parse_object(b"w7 v0 dV c0 t i0 u T Nn1x1y2,n-2,n3xy").unwrap();
        assert_eq!(way.refs, vec![1, -2, 3]);
        assert!(way.tags.is_empty());
        let relation = parse_object(b"r9 v1 T Mw3@outer,n4@,r-1@a%40%b").unwrap();
        assert_eq!(
            relation.members,
            vec![
                Member {
                    kind: OscKind::Way,
                    id: 3,
                    role: "outer".into()
                },
                Member {
                    kind: OscKind::Node,
                    id: 4,
                    role: String::new()
                },
                Member {
                    kind: OscKind::Relation,
                    id: -1,
                    role: "a@b".into()
                },
            ]
        );
    }

    #[test]
    fn anything_it_cannot_account_for_is_an_error() {
        for (line, why) in [
            ("n1 v1 T x1", "without x and y"),
            ("n1 v1 T x1 y", "latitude"),
            ("n1 T x1 y1", "no version"),
            ("n1 v1 x1 y1", "no tag field"),
            ("n1 v1 v2 T x1 y1", "twice"),
            ("n1 v1 T x1 y1 Q", "unknown field"),
            ("n1 v1 dD T x1 y1", "history"),
            ("n1 v1 T x1  y1", "empty field"),
            ("w1 v1 T", "without a node field"),
            ("w1 v1 T Nw1", "node ref"),
            ("w1 v1 T Nn1xgarbage", "node ref"),
            ("w1 v1 T Nn1x1", "node ref"),
            ("w1 v1 T Nn1x1y", "node ref"),
            ("r1 v1 T", "without a member field"),
            ("r1 v1 T Mq1@", "member"),
            ("w1 v1 T N x1 y1", "location on a non-node"),
            ("n1 v1 Ta x1 y1", "key=value"),
            ("n1 v1 Ta=%zz% x1 y1", "bad escape"),
            ("n1 v1 Ta=%41 x1 y1", "unterminated"),
            ("x1 v1 T", "object type"),
            ("n1 v1 T x181 y0", "longitude"),
            ("n1 v1 T x1.12345678 y0", "longitude"),
        ] {
            let err = parse_object(line.as_bytes()).unwrap_err();
            assert!(err.contains(why), "{line}: {err}");
        }
    }

    #[test]
    fn coordinates_read_as_exact_decimicrodegrees() {
        for (text, value) in [
            ("0", 0),
            ("1", 10_000_000),
            ("-0.0000001", -1),
            ("12.5", 125_000_000),
            ("-179.9999999", -1_799_999_999),
            ("180", 1_800_000_000),
        ] {
            assert_eq!(parse_decimicro(text), Some(value), "{text}");
        }
        for text in ["", "-", "1.", ".5", "+1", "1e5", "0001.0"] {
            assert_eq!(parse_decimicro(text), None, "{text}");
        }
    }

    #[test]
    fn identities_order_as_osmium_and_pbfhogg_do() {
        let mut keys = vec![
            order_key(OscKind::Way, 1),
            order_key(OscKind::Node, 2),
            order_key(OscKind::Node, -3),
            order_key(OscKind::Node, 0),
            order_key(OscKind::Node, -1),
            order_key(OscKind::Node, 1),
        ];
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                order_key(OscKind::Node, 0),
                order_key(OscKind::Node, -1),
                order_key(OscKind::Node, -3),
                order_key(OscKind::Node, 1),
                order_key(OscKind::Node, 2),
                order_key(OscKind::Way, 1),
            ]
        );
    }

    #[test]
    fn diff_groups_pair_sides_and_refuse_repeats_and_disorder() {
        let dir = pbfhogg_test_scratch::scratch();
        let path = dir.path().join("diff.opl");
        std::fs::write(
            &path,
            "-n2 v1 T x1 y2\n+n2 v2 T x1 y3\n+n6 v2 T x1 y2\n-n6 v3 T x1 y2\n-n7 v1 T x1 y1\n+w1 v1 T N\n",
        )
        .unwrap();
        let mut groups = DiffGroups::open(&path).unwrap();
        let mut seen = Vec::new();
        while let Some(g) = groups.next_group().unwrap() {
            seen.push((g.id, g.old.map(|o| o.version), g.new.map(|o| o.version)));
        }
        assert_eq!(
            seen,
            vec![
                (2, Some(1), Some(2)),
                (6, Some(3), Some(2)),
                (7, Some(1), None),
                (1, None, Some(1)),
            ]
        );
        assert_eq!((groups.minus, groups.plus), (3, 3));
        for (text, why) in [
            ("-n2 v1 T x1 y2\n-n2 v2 T x1 y2\n", "twice"),
            ("-n3 v1 T x1 y2\n-n2 v1 T x1 y2\n", "does not follow"),
            (" n3 v1 T x1 y2\n", "common line"),
            ("-n3 v1 T x1 y2", "newline"),
        ] {
            std::fs::write(&path, text).unwrap();
            let mut groups = DiffGroups::open(&path).unwrap();
            let err = loop {
                match groups.next_group() {
                    Ok(Some(_)) => {}
                    Ok(None) => panic!("{text:?} read without error"),
                    Err(e) => break e.to_string(),
                }
            };
            assert!(err.contains(why), "{text:?}: {err}");
        }
    }
}
