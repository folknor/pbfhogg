//! Strict streaming readers for OPL files osmium wrote: [`scan_relations`]
//! for relation records only (`osmium cat -t relation -f opl`), each
//! relation's ID and its typed member references in member order, and
//! [`scan_objects`] for a whole file, adding each object's kind, whether it
//! has tags, and a way's node references. Tag text, roles and metadata are
//! not read.
//!
//! OPL is one object per line, fields separated by one space, each field a
//! letter followed by its value; the member field is `M` followed by
//! comma-separated `<type><ref>@<role>` tokens, `n`, `w` or `r` for the type,
//! a way's node field `N` followed by comma-separated `n<ref>` tokens, and
//! the tag field `T` followed by comma-separated `key=value` pairs, empty
//! for an object without tags.
//! osmium escapes a space, comma, `@` and the other delimiters inside a value
//! as `%hex%`, so splitting on space and comma never cuts a value. The reader
//! is strict because what it yields decides a verdict: a record that is not a
//! relation, a line without its terminating newline (a truncated file), a
//! record with no member field or more than one, a member token it cannot
//! read, and an ID that does not strictly ascend are each an error, never a
//! record silently skipped. An `M` with nothing after it is a relation with
//! no members.

#![allow(
    dead_code,
    reason = "shared verifier helpers consumed by the remaining family slices"
)]

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::error::Error;
use crate::osc::OscKind;

/// The relations of `reference_input` (the input osmium reads) as OPL, at
/// `outdir/input-relations.opl`: `osmium cat -t relation -f opl`, one
/// declared reference run, so every check reading the relations of the same
/// file (`verify getid-removeid`, `verify getparents`) shares its cached
/// execution.
pub(crate) fn input_relations(
    harness: &mut crate::harness::Harness,
    reference_input: &Path,
    outdir: &Path,
) -> Result<std::path::PathBuf, Error> {
    let relations = outdir.join("input-relations.opl");
    let captured = harness.reference(
        &crate::reference::ToolRun::osmium("getid-relations.1")
            .arg("cat")
            .input(reference_input)
            .args(&["-t", "relation", "-f", "opl", "-o"])
            .output(&relations)
            .arg("--overwrite"),
        &outdir.join("input-relations.stdout"),
    )?;
    harness.require_success(&captured, "osmium cat -t relation")?;
    Ok(relations)
}

/// One relation record: its ID and its members as `(kind, ref)`.
pub(crate) struct OplRelation {
    pub id: i64,
    pub members: Vec<(OscKind, i64)>,
}

/// Hand every relation record of the OPL file at `path` to `each`, in file
/// order; returns the record count. IDs must strictly ascend.
pub(crate) fn scan_relations(
    path: &Path,
    mut each: impl FnMut(&OplRelation),
) -> Result<u64, Error> {
    let file = File::open(path)
        .map_err(|e| Error::Failed(format!("cannot open OPL {}: {e}", path.display())))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut records: u64 = 0;
    let mut last: Option<i64> = None;
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| Error::Failed(format!("cannot read OPL {}: {e}", path.display())))?;
        if read == 0 {
            return Ok(records);
        }
        let at = |why: String| {
            Error::Failed(format!(
                "OPL {} record {}: {why}",
                path.display(),
                records + 1
            ))
        };
        let Some(body) = line.strip_suffix(b"\n") else {
            return Err(at("no terminating newline (truncated?)".into()));
        };
        let relation = parse_relation(body).map_err(at)?;
        if last.is_some_and(|last| relation.id <= last) {
            return Err(at(format!(
                "relation {} does not ascend from {}",
                relation.id,
                last.unwrap_or_default()
            )));
        }
        last = Some(relation.id);
        records += 1;
        each(&relation);
    }
}

/// One object record of any kind: its kind and ID, whether it has tags, a
/// way's node references and a relation's members, each in file order.
pub(crate) struct OplObject {
    pub kind: OscKind,
    pub id: i64,
    pub tagged: bool,
    pub nodes: Vec<i64>,
    pub members: Vec<(OscKind, i64)>,
}

/// Hand every object record of the OPL file at `path` (`osmium cat -f opl`
/// of a whole file) to `each`, in file order; returns the record count.
/// Kinds must come nodes, then ways, then relations, and IDs strictly ascend
/// within each, so no object is repeated. A way must carry exactly one node
/// field and a relation exactly one member field, other kinds neither; at
/// most one tag field; an ID must not be negative.
pub(crate) fn scan_objects(path: &Path, mut each: impl FnMut(&OplObject)) -> Result<u64, Error> {
    let file = File::open(path)
        .map_err(|e| Error::Failed(format!("cannot open OPL {}: {e}", path.display())))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut records: u64 = 0;
    let mut last: Option<(usize, i64)> = None;
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| Error::Failed(format!("cannot read OPL {}: {e}", path.display())))?;
        if read == 0 {
            return Ok(records);
        }
        let at = |why: String| {
            Error::Failed(format!(
                "OPL {} record {}: {why}",
                path.display(),
                records + 1
            ))
        };
        let Some(body) = line.strip_suffix(b"\n") else {
            return Err(at("no terminating newline (truncated?)".into()));
        };
        let object = parse_object(body).map_err(at)?;
        let position = (object.kind.index(), object.id);
        if let Some(previous) = last
            && position <= previous
        {
            return Err(at(format!(
                "{} {} does not follow {} {} in type-then-ID order",
                kind_name(object.kind),
                object.id,
                KIND_NAMES[previous.0],
                previous.1
            )));
        }
        last = Some(position);
        records += 1;
        each(&object);
    }
}

const KIND_NAMES: [&str; 3] = ["node", "way", "relation"];

fn kind_name(kind: OscKind) -> &'static str {
    KIND_NAMES[kind.index()]
}

/// One OPL line (without its newline) as an object record of any kind.
fn parse_object(line: &[u8]) -> Result<OplObject, String> {
    let mut fields = line.split(|&b| b == b' ');
    let first = fields.next().unwrap_or_default();
    let (kind, digits) = match first.split_first() {
        Some((b'n', digits)) => (OscKind::Node, digits),
        Some((b'w', digits)) => (OscKind::Way, digits),
        Some((b'r', digits)) => (OscKind::Relation, digits),
        _ => {
            return Err(format!(
                "not an object record: {:?}",
                String::from_utf8_lossy(first)
            ));
        }
    };
    let name = kind_name(kind);
    let id = parse_id(digits).ok_or_else(|| {
        format!(
            "{name} id {:?} is not a non-negative integer",
            String::from_utf8_lossy(digits)
        )
    })?;
    let (mut tags, mut nodes, mut members) = (None, None, None);
    for field in fields {
        match field.split_first() {
            Some((b'T', value)) => {
                if tags.replace(!value.is_empty()).is_some() {
                    return Err(format!("{name} {id} has more than one tag field"));
                }
            }
            Some((b'N', list)) => {
                if kind != OscKind::Way {
                    return Err(format!("{name} {id} has a node field"));
                }
                let list = parse_way_nodes(list).map_err(|why| format!("{name} {id}: {why}"))?;
                if nodes.replace(list).is_some() {
                    return Err(format!("{name} {id} has more than one node field"));
                }
            }
            Some((b'M', list)) => {
                if kind != OscKind::Relation {
                    return Err(format!("{name} {id} has a member field"));
                }
                let list = parse_members(list).map_err(|why| format!("{name} {id}: {why}"))?;
                if members.replace(list).is_some() {
                    return Err(format!("{name} {id} has more than one member field"));
                }
            }
            _ => {}
        }
    }
    match kind {
        OscKind::Way if nodes.is_none() => return Err(format!("way {id} has no node field")),
        OscKind::Relation if members.is_none() => {
            return Err(format!("relation {id} has no member field"));
        }
        _ => {}
    }
    Ok(OplObject {
        kind,
        id,
        tagged: tags.unwrap_or(false),
        nodes: nodes.unwrap_or_default(),
        members: members.unwrap_or_default(),
    })
}

/// A way's `N` field value: comma-separated `n<ref>` tokens, each perhaps
/// followed by its location (`x<lon>y<lat>`), or nothing at all.
fn parse_way_nodes(list: &[u8]) -> Result<Vec<i64>, String> {
    if list.is_empty() {
        return Ok(Vec::new());
    }
    list.split(|&b| b == b',')
        .map(|token| {
            let bad = || {
                format!(
                    "node ref {:?} is not n<ref>",
                    String::from_utf8_lossy(token)
                )
            };
            let rest = token.strip_prefix(b"n").ok_or_else(bad)?;
            let end = rest.iter().position(|&b| b == b'x').unwrap_or(rest.len());
            parse_ref(&rest[..end]).ok_or_else(bad)
        })
        .collect()
}

/// One OPL line (without its newline) as a relation record.
fn parse_relation(line: &[u8]) -> Result<OplRelation, String> {
    let mut fields = line.split(|&b| b == b' ');
    let first = fields.next().unwrap_or_default();
    let id = match first.split_first() {
        Some((b'r', digits)) => parse_id(digits).ok_or_else(|| {
            format!(
                "relation id {:?} is not a non-negative integer",
                String::from_utf8_lossy(digits)
            )
        })?,
        _ => {
            return Err(format!(
                "not a relation record: {:?}",
                String::from_utf8_lossy(first)
            ));
        }
    };
    let mut members = None;
    for field in fields {
        if let Some(list) = field.strip_prefix(b"M") {
            if members.is_some() {
                return Err(format!("relation {id} has more than one member field"));
            }
            members = Some(parse_members(list).map_err(|why| format!("relation {id}: {why}"))?);
        }
    }
    let members = members.ok_or_else(|| format!("relation {id} has no member field"))?;
    Ok(OplRelation { id, members })
}

/// The `M` field's value: comma-separated `<type><ref>@<role>` tokens, or
/// nothing at all.
fn parse_members(list: &[u8]) -> Result<Vec<(OscKind, i64)>, String> {
    if list.is_empty() {
        return Ok(Vec::new());
    }
    list.split(|&b| b == b',')
        .map(|token| {
            let bad = || {
                format!(
                    "member {:?} is not <type><ref>@<role>",
                    String::from_utf8_lossy(token)
                )
            };
            let (kind, rest) = token.split_first().ok_or_else(bad)?;
            let kind = match kind {
                b'n' => OscKind::Node,
                b'w' => OscKind::Way,
                b'r' => OscKind::Relation,
                _ => return Err(bad()),
            };
            let at = rest.iter().position(|&b| b == b'@').ok_or_else(bad)?;
            let id = parse_ref(&rest[..at]).ok_or_else(bad)?;
            Ok((kind, id))
        })
        .collect()
}

/// A member reference: any decimal integer osmium writes, a minus sign
/// allowed, zero too. A dangling or odd reference is still a reference the
/// relation carries, and one that never matches (a relation member, or a node
/// or way ID nothing holds) must not make the relation unreadable.
fn parse_ref(text: &[u8]) -> Option<i64> {
    let digits = text.strip_prefix(b"-").unwrap_or(text);
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(text).ok()?.parse().ok()
}

/// A non-negative decimal ID: ASCII digits only, no sign, no leading zero
/// but in `0` itself (osmium writes an object with ID zero as it is).
fn parse_id(digits: &[u8]) -> Option<i64> {
    if digits.is_empty()
        || (digits[0] == b'0' && digits.len() > 1)
        || !digits.iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::unwrap_in_result, clippy::expect_used)]
    use super::*;

    type Record = (i64, Vec<(OscKind, i64)>);

    /// Write `text` to `file` in the calling test's own scratch `dir` and scan it.
    fn scan_text(dir: &Path, file: &str, text: &str) -> Result<Vec<Record>, Error> {
        let path = dir.join(file);
        std::fs::write(&path, text).unwrap();
        let mut seen = Vec::new();
        let count = scan_relations(&path, |r| seen.push((r.id, r.members.clone())))?;
        assert_eq!(count, seen.len() as u64);
        Ok(seen)
    }

    #[test]
    fn reads_ids_and_typed_members_in_order() {
        let scratch_dir = pbfhogg_test_scratch::scratch();
        let dir = scratch_dir.path().join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let seen = scan_text(
            &dir,
            "members.opl",
            "r5 v1 dV c1 t2020-01-01T00:00:00Z i1 ua%20%b Ttype=multipolygon,name=a%2c%b \
             Mw7@outer,n3@la%40%bel,r9@,w2@inner\nr6 v1 dV c0 t i0 u T M\nr8 M\
             n1@,r0@,r-2@\n",
        )
        .unwrap();
        assert_eq!(
            seen,
            vec![
                (
                    5,
                    vec![
                        (OscKind::Way, 7),
                        (OscKind::Node, 3),
                        (OscKind::Relation, 9),
                        (OscKind::Way, 2)
                    ]
                ),
                (6, vec![]),
                (
                    8,
                    vec![
                        (OscKind::Node, 1),
                        (OscKind::Relation, 0),
                        (OscKind::Relation, -2)
                    ]
                ),
            ]
        );
        assert!(scan_text(&dir, "empty.opl", "").unwrap().is_empty());
    }

    #[test]
    fn anything_it_cannot_account_for_is_an_error() {
        let scratch_dir = pbfhogg_test_scratch::scratch();
        let dir = scratch_dir.path().join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        for (case, (text, why)) in [
            ("r5 v1 Mw7@\nr6 v1 Mn1@", "terminating newline"),
            ("n5 v1 x1 y2\n", "not a relation"),
            ("r5 v1 T\n", "no member field"),
            ("r5 Mw1@ Mw2@\n", "more than one"),
            ("r5 Mw1\n", "member"),
            ("r5 Mx1@\n", "member"),
            ("r5 Mw@\n", "member"),
            ("r5 Mw+1@\n", "member"),
            ("r5 Mw-@\n", "member"),
            ("r5 Mw1x@\n", "member"),
            ("r5 Mw1@,\n", "member"),
            ("r-5 M\n", "non-negative integer"),
            ("r05 M\n", "non-negative integer"),
            ("r6 M\nr6 M\n", "does not ascend"),
            ("r6 M\nr5 M\n", "does not ascend"),
        ]
        .into_iter()
        .enumerate()
        {
            let err = scan_text(&dir, &format!("{case}.opl"), text)
                .unwrap_err()
                .to_string();
            assert!(err.contains(why), "{text:?}: {err}");
        }
    }

    type Object = (OscKind, i64, bool, Vec<i64>, Vec<(OscKind, i64)>);

    fn scan_object_text(dir: &Path, file: &str, text: &str) -> Result<Vec<Object>, Error> {
        let path = dir.join(file);
        std::fs::write(&path, text).unwrap();
        let mut seen = Vec::new();
        let count = scan_objects(&path, |o| {
            seen.push((o.kind, o.id, o.tagged, o.nodes.clone(), o.members.clone()));
        })?;
        assert_eq!(count, seen.len() as u64);
        Ok(seen)
    }

    #[test]
    fn objects_carry_kind_tags_nodes_and_members() {
        let scratch_dir = pbfhogg_test_scratch::scratch();
        let dir = scratch_dir.path().join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let seen = scan_object_text(
            &dir,
            "objects.opl",
            "n0 T x0 y0\nn1 v1 dV c1 t2020-01-01T00:00:00Z i1 ua Tname=a x1.5 y2.5\nn2 v1 T x1 y2\n\
             w3 v1 Thighway=road Nn1,n2,n-4\nw5 T Nn1x1.5y2.5,n2x1y2\nw6 N\n\
             r7 T Mw3@,r9@\n",
        )
        .unwrap();
        assert_eq!(
            seen,
            vec![
                (OscKind::Node, 0, false, vec![], vec![]),
                (OscKind::Node, 1, true, vec![], vec![]),
                (OscKind::Node, 2, false, vec![], vec![]),
                (OscKind::Way, 3, true, vec![1, 2, -4], vec![]),
                (OscKind::Way, 5, false, vec![1, 2], vec![]),
                (OscKind::Way, 6, false, vec![], vec![]),
                (
                    OscKind::Relation,
                    7,
                    false,
                    vec![],
                    vec![(OscKind::Way, 3), (OscKind::Relation, 9)]
                ),
            ]
        );
    }

    #[test]
    fn an_object_it_cannot_account_for_is_an_error() {
        let scratch_dir = pbfhogg_test_scratch::scratch();
        let dir = scratch_dir.path().join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        for (case, (text, why)) in [
            ("w1 N\nn2 T\n", "does not follow"),
            ("n2 T\nn2 T\n", "does not follow"),
            ("r1 M\nw1 N\n", "does not follow"),
            ("w1 T\n", "no node field"),
            ("r1 T\n", "no member field"),
            ("n1 Nn2\n", "has a node field"),
            ("w1 N Mw2@\n", "has a member field"),
            ("w1 Nn1 Nn2\n", "more than one node field"),
            ("n1 Ta=b Tc=d\n", "more than one tag field"),
            ("w1 Nw2\n", "node ref"),
            ("w1 Nn\n", "node ref"),
            ("w1 Nn1,\n", "node ref"),
            ("n00 T\n", "non-negative integer"),
            ("n-1 T\n", "non-negative integer"),
            ("x1 T\n", "not an object record"),
            ("n1 T", "terminating newline"),
        ]
        .into_iter()
        .enumerate()
        {
            let err = scan_object_text(&dir, &format!("{case}.opl"), text)
                .unwrap_err()
                .to_string();
            assert!(err.contains(why), "{text:?}: {err}");
        }
    }
}
