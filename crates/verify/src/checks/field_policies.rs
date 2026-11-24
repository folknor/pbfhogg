//! Verify: field-policies - negative controls of the element comparison
//! itself ([`opl_compare`]) on a real slice of the input, one field class at
//! a time. It runs no pbfhogg; what it establishes is that every
//! [`Policy`] the other checks judge with reads exactly the field classes it
//! claims, as osmium decodes and writes real records.
//!
//! The slice: the first IDs of each kind's first data blob, read by the
//! verifier's own frame reader; `osmium getid -r` of the node and way IDs
//! (so every way brings its nodes), plain `osmium getid` of the relation IDs
//! (so a boundary does not drag its whole geometry in), and `osmium
//! add-locations-to-ways` over both, so ways carry real inline coordinates
//! and the header declares `LocationsOnWays`. That slice is decoded to OPL
//! with metadata and coordinates, and osmium writes the OPL back to PBF
//! twice: declared (`locations_on_ways=true`, the base) and undeclared. The
//! base must equal the slice under [`Policy::FULL`], so the roundtrip loses
//! nothing the controls rely on.
//!
//! Each control edits one record of the OPL in one field class, osmium
//! writes it back declared, and the base and the altered copy are compared
//! under FULL, NO_LOCATIONS, CONTENT and [`Policy::between`]. A policy that
//! claims the class must report exactly that one identity as differing and
//! nothing one-sided; a policy that does not must report the files
//! identical. Which field changed is not inferred from the count: the
//! record at that identity is read back from osmium's decoding of both
//! files and classified token by token over the whole record, and the
//! difference must be exactly the intended class. An edit osmium
//! normalised away is a control failure, never a skip; only a class with no
//! candidate record on the input is named as not exercised.
//!
//! [`Policy::between`] is held to all four declaration combinations of the
//! base and the undeclared copy, and behaviourally on that pair: under FULL
//! they differ in inline coordinates only (real coordinates against none),
//! and between them, in both orders, NO_LOCATIONS and CONTENT must report
//! identical.
//!
//! Visibility is not a control: libosmium writes the visible flag only to a
//! history file, so a non-history PBF cannot carry a change to it (which is
//! why a policy without metadata is sound only for non-history files of
//! visible objects). A control that fails means the comparator every other
//! check rests on is unsound, which is the verifier's own defect: the check
//! records every control, then ends in a verifier error, which stops a
//! suite.
//!
//! What each check passes as its policy is not established here.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::harness::Harness;
use crate::opl_compare::{self, Class, Group, Merged, Policy};
use crate::pbf_frames::{self, Origin};
use crate::reference::ToolRun;
use crate::verify_getid_refs::osmium_completed;

const KIND_LETTERS: [char; 3] = ['n', 'w', 'r'];

/// A field class of an OPL record, as the controls alter and the
/// classifier reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Field {
    Version,
    Visible,
    Timestamp,
    Changeset,
    Uid,
    User,
    Tags,
    /// A node's own `x` and `y`.
    NodeLocation,
    /// The IDs of a way's node references.
    WayRefs,
    /// The inline coordinates of a way's node references.
    WayLocations,
    MemberType,
    MemberRef,
    MemberRole,
    /// A way's node list or a relation's member list changed length: no
    /// control's edit, which all change an item in place.
    ListLength,
}

impl Field {
    fn is_metadata(self) -> bool {
        matches!(
            self,
            Self::Version
                | Self::Visible
                | Self::Timestamp
                | Self::Changeset
                | Self::Uid
                | Self::User
        )
    }

    /// Whether a comparison under `policy` must see a change to this class.
    fn read_by(self, policy: Policy) -> bool {
        if self.is_metadata() {
            policy.metadata
        } else if self == Self::WayLocations {
            policy.locations
        } else {
            true
        }
    }
}

/// One OPL record split into its fields: the object token and each
/// attribute by its letter. A repeated or unknown attribute, or a record
/// without its object token, is an error.
#[derive(Debug, PartialEq, Eq)]
struct Record<'a> {
    kind: usize,
    id: &'a str,
    fields: Vec<(u8, &'a str)>,
}

fn parse(line: &str) -> Result<Record<'_>, String> {
    let mut tokens = line.split(' ');
    let object = tokens.next().ok_or("an empty record")?;
    let kind = match object.as_bytes().first() {
        Some(b'n') => 0,
        Some(b'w') => 1,
        Some(b'r') => 2,
        _ => return Err(format!("{object:?} is not an object token")),
    };
    let allowed: &[u8] = match kind {
        0 => b"vdctiuTxy",
        1 => b"vdctiuTN",
        _ => b"vdctiuTM",
    };
    let mut fields: Vec<(u8, &str)> = Vec::new();
    for token in tokens {
        let (&letter, _) = token.as_bytes().split_first().ok_or("an empty field")?;
        if !allowed.contains(&letter) {
            return Err(format!("field {:?} unknown for this kind", letter as char));
        }
        if fields.iter().any(|(l, _)| *l == letter) {
            return Err(format!("field {:?} repeated", letter as char));
        }
        fields.push((letter, &token[1..]));
    }
    // A full decoding writes every field of its kind; a missing one means
    // the decoding was not the full one.
    if let Some(&missing) = allowed
        .iter()
        .find(|letter| !fields.iter().any(|(l, _)| l == *letter))
    {
        return Err(format!("field {:?} missing", missing as char));
    }
    Ok(Record {
        kind,
        id: &object[1..],
        fields,
    })
}

/// A list field's items; an empty field is an empty list.
fn items(value: &str) -> Vec<&str> {
    if value.is_empty() {
        Vec::new()
    } else {
        value.split(',').collect()
    }
}

/// A way node item `n<id>` or `n<id>x<lon>y<lat>`: its ID and its
/// coordinate suffix (empty when absent).
fn way_node(item: &str) -> Result<(&str, &str), String> {
    let rest = item
        .strip_prefix('n')
        .ok_or_else(|| format!("way node {item:?} has no n"))?;
    Ok(match rest.find('x') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, ""),
    })
}

/// Whether a parsed record is a way with at least one inline way-node
/// coordinate. libosmium writes an undefined location as a bare `xy`
/// suffix under its locations option, so that is no coordinate.
fn has_inline_coordinates(record: &Record<'_>) -> Result<bool, String> {
    let Some((_, value)) = record.fields.iter().find(|(l, _)| *l == b'N') else {
        return Ok(false);
    };
    for item in items(value) {
        let suffix = way_node(item)?.1;
        if !suffix.is_empty() && suffix != "xy" {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A member item `<type><id>@<role>`: type letter, ID and role.
fn member(item: &str) -> Result<(&str, &str, &str), String> {
    let at = item
        .find('@')
        .ok_or_else(|| format!("member {item:?} has no @"))?;
    if at == 0 {
        return Err(format!("member {item:?} has no type"));
    }
    Ok((&item[..1], &item[1..at], &item[at + 1..]))
}

/// Every field class in which two records of one identity differ, read
/// over the whole of both records, each of which must carry every field of
/// its kind; a list that changed length is [`Field::ListLength`].
fn classify(a: &str, b: &str) -> Result<BTreeSet<Field>, String> {
    let (ra, rb) = (parse(a)?, parse(b)?);
    if (ra.kind, ra.id) != (rb.kind, rb.id) {
        return Err(format!("records of different objects: {a:?} and {b:?}"));
    }
    let mut found = BTreeSet::new();
    fn get<'a>(r: &Record<'a>, letter: u8) -> Option<&'a str> {
        r.fields.iter().find(|(l, _)| *l == letter).map(|(_, v)| *v)
    }
    for letter in b"vdctiuTxyNM" {
        let (va, vb) = (get(&ra, *letter), get(&rb, *letter));
        if va == vb {
            continue;
        }
        match letter {
            b'v' => found.insert(Field::Version),
            b'd' => found.insert(Field::Visible),
            b'c' => found.insert(Field::Changeset),
            b't' => found.insert(Field::Timestamp),
            b'i' => found.insert(Field::Uid),
            b'u' => found.insert(Field::User),
            b'T' => found.insert(Field::Tags),
            b'x' | b'y' => found.insert(Field::NodeLocation),
            b'N' => {
                let (la, lb) = (items(va.unwrap_or("")), items(vb.unwrap_or("")));
                if la.len() != lb.len() {
                    found.insert(Field::ListLength);
                }
                for (x, y) in la.iter().zip(&lb) {
                    let ((ia, ca), (ib, cb)) = (way_node(x)?, way_node(y)?);
                    if ia != ib {
                        found.insert(Field::WayRefs);
                    }
                    if ca != cb {
                        found.insert(Field::WayLocations);
                    }
                }
                true
            }
            _ => {
                let (la, lb) = (items(va.unwrap_or("")), items(vb.unwrap_or("")));
                if la.len() != lb.len() {
                    found.insert(Field::ListLength);
                }
                for (x, y) in la.iter().zip(&lb) {
                    let ((ta, ia, ra), (tb, ib, rb)) = (member(x)?, member(y)?);
                    if ta != tb {
                        found.insert(Field::MemberType);
                    }
                    if ia != ib {
                        found.insert(Field::MemberRef);
                    }
                    if ra != rb {
                        found.insert(Field::MemberRole);
                    }
                }
                true
            }
        };
    }
    Ok(found)
}

/// A coordinate as OPL writes it, in decimicrodegrees.
fn decimicro(text: &str) -> Option<i64> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    if whole.is_empty()
        || fraction.len() > 7
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let mut value: i64 = whole.parse().ok()?;
    for i in 0..7 {
        let digit = fraction
            .as_bytes()
            .get(i)
            .map_or(0, |b| i64::from(b - b'0'));
        value = value.checked_mul(10)?.checked_add(digit)?;
    }
    Some(if negative { -value } else { value })
}

/// A coordinate one decimicrodegree away, toward zero from either bound.
fn nudge(text: &str) -> Option<String> {
    let value = decimicro(text)?;
    let moved = if value >= 0 { value - 1 } else { value + 1 };
    let moved = if value == 0 { 1 } else { moved };
    let sign = if moved < 0 { "-" } else { "" };
    let abs = moved.unsigned_abs();
    Some(format!(
        "{sign}{}.{:07}",
        abs / 10_000_000,
        abs % 10_000_000
    ))
}

/// Replace field `letter` of `line` by `value`.
fn with_field(line: &str, letter: u8, value: &str) -> String {
    line.split(' ')
        .enumerate()
        .map(|(i, token)| {
            if i > 0 && token.as_bytes().first() == Some(&letter) {
                format!("{}{value}", letter as char)
            } else {
                token.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn field(line: &str, letter: u8) -> Option<&str> {
    line.split(' ')
        .skip(1)
        .find(|t| t.as_bytes().first() == Some(&letter))
        .map(|t| &t[1..])
}

fn kind_of(line: &str) -> Option<u8> {
    line.as_bytes().first().copied()
}

/// The edit of one control: the altered record, or `None` when `line` is
/// no candidate for it.
fn edit(field_class: Field, line: &str) -> Option<String> {
    let number = |letter: u8, f: fn(i64) -> i64| {
        let value: i64 = field(line, letter)?.parse().ok()?;
        Some(with_field(line, letter, &f(value).to_string()))
    };
    match field_class {
        // Step down where there is room, so the value stays representable.
        Field::Version => number(b'v', |v| if v > 1 { v - 1 } else { v + 1 }),
        Field::Changeset => number(b'c', |c| if c > 1 { c - 1 } else { c + 1 }),
        // A uid is written as int32: step down where there is room.
        Field::Uid => number(b'i', |i| if i > 0 { i - 1 } else { i + 1 }),
        Field::Timestamp => {
            let current = field(line, b't')?;
            let other = if current == "2001-02-03T04:05:06Z" {
                "2001-02-03T04:05:07Z"
            } else {
                "2001-02-03T04:05:06Z"
            };
            Some(with_field(line, b't', other))
        }
        Field::User => Some(with_field(line, b'u', &format!("{}x", field(line, b'u')?))),
        Field::Tags => {
            // Shift a tag boundary: `k=vw` to `kv=w`, on a tag no escape
            // touches, so the record differs only in where the key ends.
            let tags = field(line, b'T')?;
            let mut pairs: Vec<String> = items(tags).into_iter().map(str::to_owned).collect();
            let at = pairs.iter().position(|p| {
                p.split_once('=').is_some_and(|(k, v)| {
                    !k.is_empty()
                        && !k.contains('%')
                        && !v.contains('%')
                        && v.len() >= 2
                        && v.as_bytes()[0].is_ascii_alphanumeric()
                })
            })?;
            let (k, v) = pairs[at].split_once('=')?;
            pairs[at] = format!("{k}{}={}", &v[..1], &v[1..]);
            Some(with_field(line, b'T', &pairs.join(",")))
        }
        Field::NodeLocation => {
            if kind_of(line)? != b'n' {
                return None;
            }
            let moved = nudge(field(line, b'x')?)?;
            Some(with_field(line, b'x', &moved))
        }
        Field::WayRefs | Field::WayLocations => {
            if kind_of(line)? != b'w' {
                return None;
            }
            let refs = items(field(line, b'N')?);
            let mut out: Vec<String> = refs.iter().map(|s| (*s).to_owned()).collect();
            if field_class == Field::WayRefs {
                let (id, coords) = way_node(refs.first()?).ok()?;
                let id: i64 = id.parse().ok()?;
                out[0] = format!("n{}{coords}", id.checked_add(1)?);
            } else {
                let at = refs
                    .iter()
                    .position(|r| way_node(r).is_ok_and(|(_, c)| c.len() > 2))?;
                let (id, coords) = way_node(refs[at]).ok()?;
                let (lon, lat) = coords.strip_prefix('x')?.split_once('y')?;
                out[at] = format!("n{id}x{}y{lat}", nudge(lon)?);
            }
            Some(with_field(line, b'N', &out.join(",")))
        }
        Field::MemberType | Field::MemberRef | Field::MemberRole => {
            let members = items(field(line, b'M')?);
            let (kind, id, role) = member(members.first()?).ok()?;
            let changed = match field_class {
                Field::MemberType => {
                    let other = match kind {
                        "n" => "w",
                        "w" => "r",
                        _ => "n",
                    };
                    format!("{other}{id}@{role}")
                }
                Field::MemberRef => {
                    let id: i64 = id.parse().ok()?;
                    format!("{kind}{}@{role}", id.checked_add(1)?)
                }
                _ => format!("{kind}{id}@{role}x"),
            };
            let mut out: Vec<String> = members.iter().map(|s| (*s).to_owned()).collect();
            out[0] = changed;
            Some(with_field(line, b'M', &out.join(",")))
        }
        Field::Visible | Field::ListLength => None,
    }
}

/// Every control: its name and the class it alters.
const CONTROLS: [(&str, Field); 13] = [
    ("version", Field::Version),
    ("timestamp", Field::Timestamp),
    ("changeset", Field::Changeset),
    ("uid", Field::Uid),
    ("user", Field::User),
    ("tag-value", Field::Tags),
    ("tag-boundary", Field::Tags),
    ("node-location", Field::NodeLocation),
    ("way-ref", Field::WayRefs),
    ("way-location", Field::WayLocations),
    ("member-type", Field::MemberType),
    ("member-ref", Field::MemberRef),
    ("member-role", Field::MemberRole),
];

/// The tag-value control appends to the tag field instead of shifting a
/// boundary.
fn control_edit(name: &str, class: Field, line: &str) -> Option<String> {
    if name == "tag-value" {
        let tags = field(line, b'T')?;
        if tags.is_empty() {
            return None;
        }
        return Some(with_field(line, b'T', &format!("{tags}x")));
    }
    edit(class, line)
}

fn policy_name(policy: Policy) -> &'static str {
    match (policy.metadata, policy.locations) {
        (true, true) => "full",
        (true, false) => "no-locations",
        (false, false) => "content",
        (false, true) => "locations-only",
    }
}

/// The first IDs of each kind's first data blob, by the verifier's own
/// frame reader.
fn first_ids(pbf: &Path, per_kind: usize) -> Result<[Vec<i64>; 3], Error> {
    let mut ids: [Vec<i64>; 3] = Default::default();
    // A kind is taken from the first blob it appears in, and only that one.
    let mut seen = [false; 3];
    let mut claims = pbf_frames::Claims::open(pbf, Origin::Other)?;
    while let Some(blob) = claims.next_blob()? {
        if seen.iter().all(|s| *s) {
            break;
        }
        if let Some(claim) = blob.claim
            && (seen[claim.kind] || claim.count == 0)
        {
            continue;
        }
        let elements =
            pbf_frames::blob_ids(pbf, Origin::Other, blob.payload_offset, blob.datasize)?;
        let mut here = [false; 3];
        for &(kind, id) in &elements {
            here[kind] = true;
            // osmium would read a negative ID argument as an option.
            if !seen[kind] && id >= 0 && ids[kind].len() < per_kind && !ids[kind].contains(&id) {
                ids[kind].push(id);
            }
        }
        for (kind, present) in here.into_iter().enumerate() {
            seen[kind] |= present;
        }
    }
    Ok(ids)
}

/// `osmium getid` of `ids` from `pbf` into `out`, following references when
/// `referenced`; a requested or referenced object not found is accepted.
fn getid(
    harness: &mut Harness,
    pbf: &Path,
    ids: &[String],
    referenced: bool,
    out: &Path,
) -> Result<(), Error> {
    let mut run = ToolRun::osmium(if referenced {
        "field-policies-getid-r.1"
    } else {
        "field-policies-getid.1"
    })
    .arg("getid")
    .input(pbf);
    for id in ids {
        run = run.arg(id.as_str());
    }
    let run = run
        .args(if referenced {
            &["-r", "-v", "-o"]
        } else {
            &["-v", "-o"]
        })
        .output(out)
        .arg("--overwrite")
        .accept_if(1, osmium_completed);
    harness.reference(&run, &out.with_extension("stdout"))?;
    Ok(())
}

/// `osmium cat` of the OPL file `opl` to the PBF `pbf`, declaring
/// `LocationsOnWays` when `declared`.
fn write_pbf(harness: &mut Harness, opl: &Path, pbf: &Path, declared: bool) -> Result<(), Error> {
    let format = if declared {
        "pbf,locations_on_ways=true"
    } else {
        "pbf"
    };
    let captured = harness.reference(
        &ToolRun::osmium("field-policies-write.1")
            .arg("cat")
            .input(opl)
            .args(&["-f", format, "-o"])
            .output(pbf)
            .arg("--overwrite")
            .uncached(),
        &pbf.with_extension("stdout"),
    )?;
    harness.require_success(&captured, "osmium cat to PBF")
}

fn read_lines(path: &Path) -> Result<Vec<String>, Error> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Failed(format!("cannot read OPL {}: {e}", path.display())))?;
    Ok(text.lines().map(str::to_owned).collect())
}

fn write_lines(path: &Path, lines: &[String]) -> Result<(), Error> {
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    for line in lines {
        out.write_all(line.as_bytes())?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}

/// osmium's decoding of `pbf` with metadata and inline coordinates into
/// `opl`, spelled here and not through [`Policy`]: the controls' records
/// and their isolation witnesses must not come from the policy under test,
/// or a FULL that lost a field would drop that field's controls with it.
/// Every record must carry every field of its kind.
fn literal_decode(harness: &mut Harness, pbf: &Path, opl: &Path) -> Result<Vec<String>, Error> {
    let captured = harness.reference(
        &ToolRun::osmium("field-policies-opl.1")
            .arg("cat")
            .input(pbf)
            .args(&["-f", "opl,locations_on_ways=true", "-o"])
            .output(opl)
            .arg("--overwrite")
            .uncached(),
        &opl.with_extension("stdout"),
    )?;
    harness.require_success(&captured, "osmium cat to OPL")?;
    let lines = read_lines(opl)?;
    for line in &lines {
        parse(line).map_err(|why| {
            Error::Failed(format!(
                "{}: record {}: {why}",
                opl.display(),
                identity(line)
            ))
        })?;
    }
    Ok(lines)
}

/// [`literal_decode`] of `pbf`, the OPL removed once read.
fn full_records(harness: &mut Harness, pbf: &Path) -> Result<Vec<String>, Error> {
    let opl = pbf.with_extension("full.opl");
    let lines = literal_decode(harness, pbf, &opl)?;
    std::fs::remove_file(&opl)?;
    Ok(lines)
}

/// The object token (`n12`) a record opens with.
fn identity(line: &str) -> &str {
    line.split(' ').next().unwrap_or("")
}

struct Compared {
    merged: Merged,
    groups: Vec<Group>,
}

fn compare(
    harness: &mut Harness,
    a: &Path,
    b: &Path,
    listing: &Path,
    policy: Policy,
) -> Result<Compared, Error> {
    let mut groups = Vec::new();
    // The merge itself is this check's subject, so never stored summaries.
    let merged = opl_compare::compare_merged(harness, a, b, listing, policy, |g| groups.push(g))?;
    Ok(Compared { merged, groups })
}

/// Whether a comparison reported exactly `(kind, id)` as differing and
/// nothing else.
fn sees_only(compared: &Compared, kind: usize, id: i64) -> bool {
    compared.merged.differs == 1
        && compared.merged.first_only == 0
        && compared.merged.second_only == 0
        && compared.groups
            == [Group {
                class: Class::Differs,
                kind,
                id,
            }]
}

/// One control (module doc): `Ok(false)` when the input has no candidate.
fn run_control(
    harness: &mut Harness,
    outdir: &Path,
    base: &Path,
    base_opl: &[String],
    base_full: &[String],
    name: &str,
    class: Field,
) -> Result<bool, Error> {
    let Some((at, altered_line)) = base_opl
        .iter()
        .enumerate()
        .find_map(|(i, line)| control_edit(name, class, line).map(|l| (i, l)))
    else {
        harness.msg(&format!("  {name}: no candidate record on this input"));
        return Ok(false);
    };
    let original = &base_opl[at];
    let token = identity(original).to_owned();
    let kind = match original.as_bytes()[0] {
        b'n' => 0,
        b'w' => 1,
        _ => 2,
    };
    let id: i64 = token[1..]
        .parse()
        .map_err(|_| Error::tool(format!("record {token} has no decimal ID")))?;
    harness.msg(&format!("--- {name}: {token} ---"));
    let mut lines = base_opl.to_vec();
    lines[at] = altered_line;
    let opl = outdir.join(format!("{name}.opl"));
    write_lines(&opl, &lines)?;
    let altered = outdir.join(format!("{name}.osm.pbf"));
    write_pbf(harness, &opl, &altered, true)?;

    let altered_full = full_records(harness, &altered)?;
    isolate(harness, name, class, &token, base_full, &altered_full);

    let between = Policy::between((base, Origin::Other), (&altered, Origin::Other))?;
    if between != Policy::FULL {
        harness.fail(format!(
            "{name}: between two LocationsOnWays files is {}, not full",
            policy_name(between)
        ));
    }
    judge_policies(
        harness,
        outdir,
        (base, &altered),
        name,
        class,
        (&token, kind, id),
        between,
    )
}

/// What osmium carried through, read independently of the comparison
/// under test: every record but `token` equal, and `token` differing in
/// exactly `class` over the whole record.
fn isolate(
    harness: &mut Harness,
    name: &str,
    class: Field,
    token: &str,
    base_full: &[String],
    altered_full: &[String],
) {
    let others_equal = base_full.len() == altered_full.len()
        && base_full
            .iter()
            .zip(altered_full)
            .all(|(a, b)| a == b || (identity(a) == token && identity(b) == token));
    if !others_equal {
        harness.msg(&format!("  {name}: a record besides {token} changed FAIL"));
        harness.fail(format!("{name} changes {token} alone"));
    }
    let find = |records: &[String]| records.iter().find(|line| identity(line) == token).cloned();
    match (find(base_full), find(altered_full)) {
        (Some(a), Some(b)) => match classify(&a, &b) {
            Ok(found) if found == BTreeSet::from([class]) => {
                harness.msg(&format!(
                    "  {name}: only {class:?} differs after osmium PASS"
                ));
            }
            Ok(found) => {
                harness.msg(&format!(
                    "  {name}: after osmium the record differs in {found:?}, not {class:?} only \
                     FAIL"
                ));
                harness.fail(format!("{name} alters {class:?} alone"));
            }
            Err(why) => {
                harness.msg(&format!("  {name}: unreadable record: {why} FAIL"));
                harness.fail(format!("{name} alters {class:?} alone"));
            }
        },
        _ => {
            harness.msg(&format!("  {name}: {token} is not in both decodings FAIL"));
            harness.fail(format!("{name} keeps {token} on both sides"));
        }
    }
}

/// The base against the altered copy under every policy: the identity
/// `(kind, id)`, spelled `token`, alone differing where the policy reads
/// `class`, the files identical where it does not.
fn judge_policies(
    harness: &mut Harness,
    outdir: &Path,
    (base, altered): (&Path, &Path),
    name: &str,
    class: Field,
    (token, kind, id): (&str, usize, i64),
    between: Policy,
) -> Result<bool, Error> {
    for (label, policy) in [
        ("full", Policy::FULL),
        ("no-locations", Policy::NO_LOCATIONS),
        ("content", Policy::CONTENT),
        ("between", between),
    ] {
        let listing = outdir.join(format!("{name}-{label}.txt"));
        let compared = compare(harness, base, altered, &listing, policy)?;
        let reads = class.read_by(policy);
        let ok = if reads {
            sees_only(&compared, kind, id)
        } else {
            compared.merged.identical() && compared.groups.is_empty()
        };
        let expect = if reads {
            format!("{token} alone differs")
        } else {
            "identical".to_owned()
        };
        harness.msg(&format!(
            "  {name} under {label}: {expect} {}",
            if ok { "PASS" } else { "FAIL" }
        ));
        if !ok {
            harness.replay_listing(&listing)?;
            harness.fail(format!(
                "{name} under {label} ({}): expected {expect}",
                policy_name(policy)
            ));
        }
    }
    Ok(true)
}

/// The between pair's independent witness: the identities whose records
/// differ in inline coordinates only (module doc), failing every record
/// that does not fit. `None` when any record failed, after failing each.
fn coordinate_witness(
    harness: &mut Harness,
    declared_full: &[String],
    undeclared_full: &[String],
) -> Option<BTreeSet<(usize, i64)>> {
    // The identities classified as differing in inline coordinates only;
    // the FULL comparison must report exactly these, not merely as many.
    // Every identity of the decoding is checked for uniqueness, so a
    // comparator that silently coalesced duplicate rows could not pass on a
    // witness that collapsed them too. The witness is independent of the
    // classification: a way the declared decoding gives inline coordinates
    // must lose all of them in the undeclared one, so a copy that kept some
    // cannot shrink or empty the expected set.
    let mut seen: BTreeSet<(usize, i64)> = BTreeSet::new();
    let mut failures: Vec<String> = Vec::new();
    let mut located: BTreeSet<(usize, i64)> = BTreeSet::new();
    for (a, b) in declared_full.iter().zip(undeclared_full) {
        let witness = parse(a).and_then(|ra| {
            let rb = parse(b)?;
            let id = ra
                .id
                .parse::<i64>()
                .map_err(|e| format!("id {:?}: {e}", ra.id))?;
            Ok((
                (ra.kind, id),
                has_inline_coordinates(&ra)?,
                has_inline_coordinates(&rb)?,
            ))
        });
        let (key, declared_located) = match witness {
            Ok((_, _, true)) => {
                failures.push(format!(
                    "{} keeps inline coordinates in the undeclared copy",
                    identity(a)
                ));
                continue;
            }
            Ok((key, declared_located, false)) => (key, declared_located),
            Err(why) => {
                failures.push(format!("{}: {why}", identity(a)));
                continue;
            }
        };
        if !seen.insert(key) {
            failures.push(format!(
                "{} appears twice in the declared decoding",
                identity(a)
            ));
        }
        if declared_located {
            located.insert(key);
        }
        match classify(a, b) {
            Ok(found) if found.is_empty() && !declared_located => {}
            Ok(found) if found == BTreeSet::from([Field::WayLocations]) && declared_located => {}
            Ok(found) => {
                failures.push(format!(
                    "{} differs in {found:?} without its declaration, not in inline \
                     coordinates only",
                    identity(a)
                ));
            }
            Err(why) => failures.push(format!("{}: {why}", identity(a))),
        }
    }
    if failures.is_empty() {
        return Some(located);
    }
    for failure in failures {
        harness.fail(failure);
    }
    None
}

/// [`Policy::between`] by declaration, and on a pair that differs in
/// inline coordinates only (module doc). `Ok(false)` when no way has a
/// located node.
fn run_between(
    harness: &mut Harness,
    outdir: &Path,
    declared: &Path,
    undeclared: &Path,
    declared_full: &[String],
) -> Result<bool, Error> {
    harness.msg("--- between: declaration combinations ---");
    for (a, b, want) in [
        (declared, declared, Policy::FULL),
        (declared, undeclared, Policy::NO_LOCATIONS),
        (undeclared, declared, Policy::NO_LOCATIONS),
        (undeclared, undeclared, Policy::NO_LOCATIONS),
    ] {
        let got = Policy::between((a, Origin::Other), (b, Origin::Other))?;
        let ok = got == want;
        harness.msg(&format!(
            "  between {} and {}: {} {}",
            a.display(),
            b.display(),
            policy_name(got),
            if ok { "PASS" } else { "FAIL" }
        ));
        if !ok {
            harness.fail(format!(
                "between {} and {} is {}, not {}",
                a.display(),
                b.display(),
                policy_name(got),
                policy_name(want)
            ));
        }
    }

    // The pair's FULL difference: every record classified, nothing but
    // inline coordinates.
    let undeclared_full = full_records(harness, undeclared)?;
    if declared_full.len() != undeclared_full.len() {
        harness.fail("the undeclared copy holds a different number of records");
        return Ok(true);
    }
    // An invalid witness has already failed the check; it is no skip.
    let Some(located) = coordinate_witness(harness, declared_full, &undeclared_full) else {
        return Ok(true);
    };
    if located.is_empty() {
        harness.msg("  between: no way has a located node on this input");
        return Ok(false);
    }
    let full = compare(
        harness,
        declared,
        undeclared,
        &outdir.join("between-full.txt"),
        Policy::FULL,
    )?;
    let reported: BTreeSet<(usize, i64)> = full
        .groups
        .iter()
        .filter(|g| g.class == Class::Differs)
        .map(|g| (g.kind, g.id))
        .collect();
    let ok = full.merged.differs == located.len() as u64
        && full.merged.first_only == 0
        && full.merged.second_only == 0
        && full.groups.len() == located.len()
        && reported == located;
    harness.msg(&format!(
        "  between: {} way(s) differ in inline coordinates only under full {}",
        located.len(),
        if ok { "PASS" } else { "FAIL" }
    ));
    if !ok {
        harness.fail(
            "the declared and undeclared copies do not differ under full in exactly the \
             ways classified as differing in inline coordinates",
        );
    }
    for (label, a, b) in [
        ("between", declared, undeclared),
        ("between-reversed", undeclared, declared),
    ] {
        let policy = Policy::between((a, Origin::Other), (b, Origin::Other))?;
        for (name, policy) in [
            (label, policy),
            ("no-locations", Policy::NO_LOCATIONS),
            ("content", Policy::CONTENT),
        ] {
            let listing = outdir.join(format!("{label}-{name}.txt"));
            let compared = compare(harness, a, b, &listing, policy)?;
            let ok = compared.merged.identical() && compared.groups.is_empty();
            harness.msg(&format!(
                "  {label} under {name}: identical {}",
                if ok { "PASS" } else { "FAIL" }
            ));
            if !ok {
                harness.replay_listing(&listing)?;
                harness.fail(format!(
                    "{label} under {name}: a coordinate-only difference was read"
                ));
            }
        }
    }
    Ok(true)
}

/// Run the field-policy controls (module doc). No outcome here says
/// anything about pbfhogg, so a failure to build or judge a control is a
/// verifier error; an interrupt, an unsettled launch and an environment
/// outcome keep their meaning.
pub fn run(harness: &mut Harness, pbf: &Path, _direct_io: bool) -> Result<Option<String>, Error> {
    match controls(harness, pbf) {
        Err(Error::Failed(why)) => Err(Error::tool(format!(
            "a field-policy control could not be built or judged, so the element comparison's \
             soundness is not established: {why}"
        ))),
        other => other,
    }
}

fn controls(harness: &mut Harness, pbf: &Path) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("field-policies")?;
    harness.msg("--- field-policies: a real slice of the input ---");
    let [nodes, ways, relations] = first_ids(pbf, crate::tunables::FIELD_POLICY_IDS)?;
    let spell = |kind: usize, ids: &[i64]| -> Vec<String> {
        ids.iter()
            .map(|id| format!("{}{id}", KIND_LETTERS[kind]))
            .collect()
    };
    let mut parts: Vec<PathBuf> = Vec::new();
    let referenced: Vec<String> = spell(0, &nodes)
        .into_iter()
        .chain(spell(1, &ways))
        .collect();
    if !referenced.is_empty() {
        let path = outdir.join("slice-ways.osm.pbf");
        getid(harness, pbf, &referenced, true, &path)?;
        parts.push(path);
    }
    if !relations.is_empty() {
        let path = outdir.join("slice-relations.osm.pbf");
        getid(harness, pbf, &spell(2, &relations), false, &path)?;
        parts.push(path);
    }
    if parts.is_empty() {
        return Err(Error::tool(format!(
            "{} holds no object to build a slice from",
            pbf.display()
        )));
    }
    let slice = outdir.join("slice.osm.pbf");
    let mut run = ToolRun::osmium("field-policies-altw.1")
        .arg("add-locations-to-ways")
        .args(&["-n", "--ignore-missing-nodes"]);
    for part in &parts {
        run = run.input(part);
    }
    let captured = harness.reference(
        &run.arg("-o").output(&slice).arg("--overwrite"),
        &outdir.join("slice-altw.stdout"),
    )?;
    harness.require_success(&captured, "osmium add-locations-to-ways")?;

    let slice_opl = outdir.join("slice.opl");
    let base_opl = literal_decode(harness, &slice, &slice_opl)?;
    harness.msg(&format!("  slice: {} records", base_opl.len()));
    let base = outdir.join("base.osm.pbf");
    write_pbf(harness, &slice_opl, &base, true)?;
    let undeclared = outdir.join("undeclared.osm.pbf");
    write_pbf(harness, &slice_opl, &undeclared, false)?;

    let roundtrip = compare(
        harness,
        &slice,
        &base,
        &outdir.join("roundtrip.txt"),
        Policy::FULL,
    )?;
    let lossless = roundtrip.merged.identical() && roundtrip.groups.is_empty();
    harness.msg(&format!(
        "  osmium OPL roundtrip of the slice is lossless under full {}",
        if lossless { "PASS" } else { "FAIL" }
    ));
    if !lossless {
        harness.fail("the slice's OPL roundtrip under full");
    }

    let base_full = full_records(harness, &base)?;
    let mut absent: Vec<&str> = Vec::new();
    for (name, class) in CONTROLS {
        if !run_control(harness, &outdir, &base, &base_opl, &base_full, name, class)? {
            absent.push(name);
        }
    }
    if !run_between(harness, &outdir, &base, &undeclared, &base_full)? {
        absent.push("between on inline coordinates");
    }

    if harness.failed() {
        return Err(Error::tool(
            "the field-policy controls do not establish that the element comparison reads the \
             field classes its policies claim; no verdict that rests on it can be trusted",
        ));
    }
    Ok(if absent.is_empty() {
        None
    } else {
        Some(format!(
            "not exercised on this input: {}",
            absent.join(", ")
        ))
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const NODE: &str =
        "n1 v2 dV c3 t2020-01-01T00:00:00Z i4 uann Thighway=bus_stop,name=Ab x12.5 y-55.0000001";
    const WAY: &str =
        "w5 v1 dV c3 t2020-01-01T00:00:00Z i4 uann Thighway=road Nn1x12.5y55.1,n2x1y2";
    const RELATION: &str = "r7 v1 dV c3 t2020-01-01T00:00:00Z i4 uann Ttype=route Mw5@outer,n1@";

    #[test]
    fn every_control_alters_exactly_its_class() {
        for (name, class) in CONTROLS {
            let edited: Vec<(String, String)> = [NODE, WAY, RELATION]
                .into_iter()
                .filter_map(|line| control_edit(name, class, line).map(|e| (line.to_owned(), e)))
                .collect();
            assert!(!edited.is_empty(), "{name} found no candidate");
            for (line, altered) in edited {
                assert_eq!(
                    classify(&line, &altered).unwrap(),
                    BTreeSet::from([class]),
                    "{name}: {line} -> {altered}"
                );
            }
        }
    }

    #[test]
    fn the_classifier_reads_the_whole_record() {
        assert!(classify(NODE, NODE).unwrap().is_empty());
        // A record without every field of its kind is no full decoding.
        assert!(classify(NODE, &NODE.replace(" c3", "")).is_err());
        // A list that changed length is no in-place edit.
        let shorter = WAY.replace(",n2x1y2", "");
        assert_eq!(
            classify(WAY, &shorter).unwrap(),
            BTreeSet::from([Field::ListLength])
        );
        let longer = RELATION.replace("n1@", "n1@,w9@x");
        assert_eq!(
            classify(RELATION, &longer).unwrap(),
            BTreeSet::from([Field::ListLength])
        );
        // Coordinates dropped from every way node.
        let bare = "w5 v1 dV c3 t2020-01-01T00:00:00Z i4 uann Thighway=road Nn1,n2";
        assert_eq!(
            classify(WAY, bare).unwrap(),
            BTreeSet::from([Field::WayLocations])
        );
        assert!(classify(NODE, WAY).is_err());
        assert!(classify("n1 v1 v2", "n1 v1 v2").is_err());
        assert!(classify("n1 N", "n1 N").is_err());
        assert!(classify("n1 q", "n1 q").is_err());
    }

    #[test]
    fn a_nudged_coordinate_is_one_decimicrodegree_away() {
        assert_eq!(decimicro("12.5"), Some(125_000_000));
        assert_eq!(decimicro("-55.0000001"), Some(-550_000_001));
        assert_eq!(decimicro("x"), None);
        assert_eq!(decimicro("1.00000001"), None);
        assert_eq!(nudge("12.5").unwrap(), "12.4999999");
        assert_eq!(nudge("-180").unwrap(), "-179.9999999");
        assert_eq!(nudge("0").unwrap(), "0.0000001");
    }

    #[test]
    fn a_tag_boundary_shift_avoids_escapes() {
        assert_eq!(
            edit(Field::Tags, "n1 Ta=%20%b,name=Ab").unwrap(),
            "n1 Ta=%20%b,nameA=b"
        );
        assert!(edit(Field::Tags, "n1 Ta=b,c=%20%x").is_none());
        assert!(edit(Field::Tags, "n1 T").is_none());
    }

    #[test]
    fn the_expected_matrix() {
        assert!(!Field::Uid.read_by(Policy::CONTENT));
        assert!(Field::Uid.read_by(Policy::NO_LOCATIONS));
        assert!(!Field::WayLocations.read_by(Policy::NO_LOCATIONS));
        assert!(Field::WayLocations.read_by(Policy::FULL));
        assert!(Field::MemberRole.read_by(Policy::CONTENT));
    }
}
