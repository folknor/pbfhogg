//! A change file as a listing of its changes, one record per change in
//! document order, for the verify arms over pbfhogg's OSC commands
//! (`verify merge-changes`, `verify tags-filter-osc`).
//!
//! A record joins two independent readings of the same change by position:
//! osmium's (`osmium cat -f opl`, one declared reference run, so it is
//! cached by the file's content), which decodes the object - tags,
//! coordinates, refs, members and the metadata OPL holds - and brokkr's
//! strict XML scan ([`crate::osc::scan_osc_changes`]), which gives what OPL
//! cannot: the action of the container the change sits in (osmium derives
//! create or modify from the version when it writes), and the metadata
//! attributes as written, absence apart from an empty or zero value. At
//! each position the two must name the same kind and ID and the XML version
//! must be the OPL one (`v0` for none); a count mismatch or a trailing OPL
//! line is an error too, so neither reading can drop a change the other
//! sees.
//!
//! Record format, one line each: the action letter (`c`, `m`, `d`), the
//! XML version (`-` when absent), the timestamp, changeset, uid, user and
//! visible attributes (`-` when absent, else `=` and the decoded value with
//! `%`, tab, line breaks and other control characters as `%XX`), then the
//! OPL line, all tab-separated. osmium escapes every delimiter inside OPL,
//! so a record is one line and a position is its line number.
//!
//! [`compare`] reads the expected side as pbfhogg's OSC writers render an
//! input change: an empty metadata string is written as no attribute, and
//! the projection is applied to the expected side only, so the actual output
//! must carry no empty attribute. Every change keeps its body, a delete's
//! included (tags, coordinates, refs, members, as osmium keeps them), and
//! both sides are osmium's decoding, so pbfhogg's normal form (attribute
//! order, coordinate spelling, tags written after children) is not compared
//! while tag, way-ref and member order are. A deleted node with no location
//! has empty `x` and `y` on both sides; one written as 0 0 differs.

#![allow(
    dead_code,
    reason = "shared verifier helpers consumed by the remaining family slices"
)]

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::harness::Harness;
use crate::osc::{self, OscAction, OscKind, OscMeta};
use crate::reference::ToolRun;

/// One change: the XML reading's action and metadata with osmium's OPL line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Change {
    pub action: OscAction,
    pub kind: OscKind,
    pub id: i64,
    pub meta: OscMeta,
    pub opl: String,
}

impl Change {
    /// Whether the OPL line carries any body field (tags, coordinates, way
    /// refs, members) with a value.
    pub(crate) fn has_body(&self) -> bool {
        opl_fields(&self.opl)
            .skip(1)
            .any(|field| is_body_field(field) && field.len() > 1)
    }

    /// Whether the OPL line carries the body field `letter` with a value.
    pub(crate) fn has_field(&self, letter: char) -> bool {
        opl_field(&self.opl, letter).is_some_and(|v| !v.is_empty())
    }

    /// The OPL `v` field (`0` when OPL prints none).
    pub(crate) fn opl_version(&self) -> Result<i64, Error> {
        opl_field(&self.opl, 'v')
            .map_or(Ok(0), str::parse)
            .map_err(|_| Error::Failed(format!("OPL record {:?} has no integer version", self.opl)))
    }

    /// The OPL timestamp in seconds since the epoch, `None` when OPL prints
    /// none (libosmium's invalid timestamp).
    pub(crate) fn opl_timestamp(&self) -> Result<Option<i64>, Error> {
        match opl_field(&self.opl, 't') {
            None | Some("") => Ok(None),
            Some(t) => parse_iso_seconds(t).map(Some).ok_or_else(|| {
                Error::Failed(format!(
                    "OPL record {:?} has a timestamp brokkr cannot read",
                    self.opl
                ))
            }),
        }
    }

    /// This change as pbfhogg's writers render it (see the module docs).
    fn as_pbfhogg_writes(&self) -> Self {
        let empty_is_absent = |v: &Option<String>| v.clone().filter(|s| !s.is_empty());
        let meta = OscMeta {
            version: self.meta.version,
            timestamp: empty_is_absent(&self.meta.timestamp),
            changeset: empty_is_absent(&self.meta.changeset),
            uid: empty_is_absent(&self.meta.uid),
            user: empty_is_absent(&self.meta.user),
            visible: empty_is_absent(&self.meta.visible),
        };
        Self {
            action: self.action,
            kind: self.kind,
            id: self.id,
            meta,
            opl: self.opl.clone(),
        }
    }

    /// The record line, newline included.
    pub(crate) fn record(&self) -> String {
        let mut line = String::with_capacity(self.opl.len() + 64);
        line.push(action_letter(self.action));
        line.push('\t');
        match self.meta.version {
            Some(v) => line.push_str(&v.to_string()),
            None => line.push('-'),
        }
        for value in [
            &self.meta.timestamp,
            &self.meta.changeset,
            &self.meta.uid,
            &self.meta.user,
            &self.meta.visible,
        ] {
            line.push('\t');
            match value {
                None => line.push('-'),
                Some(v) => {
                    line.push('=');
                    escape_into(&mut line, v);
                }
            }
        }
        line.push('\t');
        line.push_str(&self.opl);
        line.push('\n');
        line
    }

    /// A record line (without its newline) read back.
    fn parse(line: &str) -> Result<Self, String> {
        let mut fields = line.split('\t');
        let mut next = |what: &str| fields.next().ok_or_else(|| format!("record has no {what}"));
        let action = match next("action")? {
            "c" => OscAction::Create,
            "m" => OscAction::Modify,
            "d" => OscAction::Delete,
            other => return Err(format!("unknown action {other:?}")),
        };
        let version = match next("version")? {
            "-" => None,
            v => Some(v.parse().map_err(|_| format!("version {v:?}"))?),
        };
        let mut strings: [Option<String>; 5] = Default::default();
        for slot in &mut strings {
            let field = next("metadata field")?;
            *slot = match field {
                "-" => None,
                _ => Some(unescape(
                    field
                        .strip_prefix('=')
                        .ok_or_else(|| format!("metadata field {field:?}"))?,
                )?),
            };
        }
        let opl = next("OPL line")?.to_owned();
        if fields.next().is_some() {
            return Err("record has more fields than it should".into());
        }
        let (kind, id) = opl_identity(&opl)?;
        let [timestamp, changeset, uid, user, visible] = strings;
        Ok(Self {
            action,
            kind,
            id,
            meta: OscMeta {
                version,
                timestamp,
                changeset,
                uid,
                user,
                visible,
            },
            opl,
        })
    }
}

fn action_letter(action: OscAction) -> char {
    match action {
        OscAction::Create => 'c',
        OscAction::Modify => 'm',
        OscAction::Delete => 'd',
    }
}

/// The body fields of an OPL object line: tags, coordinates, way nodes,
/// relation members.
fn is_body_field(field: &str) -> bool {
    matches!(
        field.as_bytes().first(),
        Some(b'T' | b'x' | b'y' | b'N' | b'M')
    )
}

fn opl_fields(opl: &str) -> impl Iterator<Item = &str> {
    opl.split(' ')
}

/// The value of the OPL field with letter `letter`, past the identity.
fn opl_field(opl: &str, letter: char) -> Option<&str> {
    opl_fields(opl).skip(1).find_map(|f| f.strip_prefix(letter))
}

/// The kind and ID of an OPL object line.
pub(crate) fn opl_identity(opl: &str) -> Result<(OscKind, i64), String> {
    let first = opl.split(' ').next().unwrap_or_default();
    let (kind, digits) = match first.split_at_checked(1) {
        Some(("n", d)) => (OscKind::Node, d),
        Some(("w", d)) => (OscKind::Way, d),
        Some(("r", d)) => (OscKind::Relation, d),
        _ => return Err(format!("not an object record: {opl:?}")),
    };
    let id = digits
        .parse()
        .map_err(|_| format!("object id {digits:?} is not an integer"))?;
    Ok((kind, id))
}

/// `YYYY-MM-DDTHH:MM:SSZ`, the only form osmium's OPL writes, as seconds
/// since the epoch.
fn parse_iso_seconds(t: &str) -> Option<i64> {
    let b = t.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' {
        return None;
    }
    if b[16] != b':' || b[19] != b'Z' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| t.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Days from civil (Howard Hinnant's algorithm).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + s)
}

fn escape_into(out: &mut String, value: &str) {
    for c in value.chars() {
        if c == '%' || c.is_control() {
            for byte in c.to_string().bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(c);
        }
    }
}

fn unescape(field: &str) -> Result<String, String> {
    let mut bytes = Vec::with_capacity(field.len());
    let raw = field.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' {
            let hex = field
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| format!("bad escape in {field:?}"))?;
            bytes.push(hex);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

/// A change file's listing.
pub(crate) struct Listing {
    pub path: PathBuf,
    /// osmium's OPL of the file, the listing's content side.
    pub opl: PathBuf,
}

/// Write the listing of `osc` to `outdir/<stem>.changes`, from `osmium cat`
/// to `outdir/<stem>.opl` (run id `osc-opl.1`) and the strict XML scan.
pub(crate) fn listing(
    harness: &mut Harness,
    osc: &Path,
    outdir: &Path,
    stem: &str,
) -> Result<Listing, Error> {
    let opl = outdir.join(format!("{stem}.opl"));
    let captured = harness.reference(
        &ToolRun::osmium("osc-opl.1")
            .arg("cat")
            .input(osc)
            .args(&["-f", "opl", "-o"])
            .output(&opl)
            .arg("--overwrite"),
        &outdir.join(format!("{stem}.opl.stdout")),
    )?;
    harness.require_success(&captured, "osmium cat -f opl")?;
    let path = outdir.join(format!("{stem}.changes"));
    let changes = zip(osc, &opl, &path)?;
    harness.msg(&format!("  {}: {changes} changes", osc.display()));
    Ok(Listing { path, opl })
}

/// Join the XML scan of `osc` with its OPL `opl` into the listing `out`;
/// returns the change count.
fn zip(osc: &Path, opl: &Path, out: &Path) -> Result<u64, Error> {
    let mut lines = BufReader::new(File::open(opl)?);
    let mut writer = BufWriter::new(File::create(out)?);
    let mut count: u64 = 0;
    let mut failure: Option<String> = None;
    let mut line = String::new();
    osc::scan_osc_changes(osc, |change| {
        if failure.is_some() {
            return;
        }
        count += 1;
        line.clear();
        let step = (|| -> Result<(), String> {
            let read = lines.read_line(&mut line).map_err(|e| e.to_string())?;
            let opl_line = line.strip_suffix('\n').ok_or_else(|| {
                if read == 0 {
                    "osmium's OPL ends early".to_owned()
                } else {
                    "OPL line without its newline".to_owned()
                }
            })?;
            let (kind, id) = opl_identity(opl_line)?;
            if (kind, id) != (change.kind, change.id) {
                return Err(format!(
                    "the XML names {:?} {} where osmium's OPL has {opl_line:?}",
                    change.kind, change.id
                ));
            }
            let record = Change {
                action: change.action,
                kind,
                id,
                meta: change.meta,
                opl: opl_line.to_owned(),
            };
            let version = record.opl_version().map_err(|e| e.to_string())?;
            if record.meta.version.unwrap_or(0) != version {
                return Err(format!(
                    "the XML version {:?} is not osmium's v{version} in {opl_line:?}",
                    record.meta.version
                ));
            }
            writer
                .write_all(record.record().as_bytes())
                .map_err(|e| e.to_string())
        })();
        if let Err(why) = step {
            failure = Some(format!("change {count}: {why}"));
        }
    })?;
    if let Some(why) = failure {
        return Err(Error::Failed(format!(
            "{} and {} disagree: {why}",
            osc.display(),
            opl.display()
        )));
    }
    line.clear();
    if lines.read_line(&mut line)? > 0 {
        return Err(Error::Failed(format!(
            "{} holds more objects than the {count} changes brokkr reads in {}",
            opl.display(),
            osc.display()
        )));
    }
    writer.flush()?;
    Ok(count)
}

/// Reads a listing record by record, tracking each record's byte offset.
pub(crate) struct ListingReader {
    path: PathBuf,
    reader: BufReader<File>,
    line: String,
    offset: u64,
    records: u64,
}

impl ListingReader {
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        Ok(Self {
            path: path.to_path_buf(),
            reader: BufReader::new(File::open(path)?),
            line: String::new(),
            offset: 0,
            records: 0,
        })
    }

    /// The next record and its byte offset, or `None` at the end.
    pub(crate) fn next_record(&mut self) -> Result<Option<(u64, Change)>, Error> {
        self.line.clear();
        let read = self.reader.read_line(&mut self.line)?;
        if read == 0 {
            return Ok(None);
        }
        let at = self.offset;
        self.offset += read as u64;
        self.records += 1;
        let body = self.line.strip_suffix('\n').ok_or_else(|| {
            Error::Failed(format!("{} ends inside a record", self.path.display()))
        })?;
        let change = Change::parse(body).map_err(|why| {
            Error::Failed(format!(
                "{} record {}: {why}",
                self.path.display(),
                self.records
            ))
        })?;
        Ok(Some((at, change)))
    }
}

/// Random access to a listing's records by byte offset.
pub(crate) struct ListingFile {
    path: PathBuf,
    reader: BufReader<File>,
    line: String,
}

impl ListingFile {
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        Ok(Self {
            path: path.to_path_buf(),
            reader: BufReader::new(File::open(path)?),
            line: String::new(),
        })
    }

    pub(crate) fn at(&mut self, offset: u64) -> Result<Change, Error> {
        self.reader.seek(SeekFrom::Start(offset))?;
        self.line.clear();
        self.reader.read_line(&mut self.line)?;
        let body = self.line.strip_suffix('\n').ok_or_else(|| {
            Error::Failed(format!(
                "{} has no record at offset {offset}",
                self.path.display()
            ))
        })?;
        Change::parse(body)
            .map_err(|why| Error::Failed(format!("{} at {offset}: {why}", self.path.display())))
    }
}

/// Append the records of `listings`, in order, to the new file `out`.
pub(crate) fn concatenate(listings: &[&Path], out: &Path) -> Result<(), Error> {
    let mut writer = BufWriter::new(File::create(out)?);
    for path in listings {
        std::io::copy(&mut File::open(path)?, &mut writer)?;
    }
    writer.flush()?;
    Ok(())
}

/// Where two listings first part, if they do.
pub(crate) struct Comparison {
    pub expected: u64,
    pub actual: u64,
    /// The 1-based record number and the two records there (`None` past
    /// one side's end).
    pub first_difference: Option<(u64, Option<String>, Option<String>)>,
}

impl Comparison {
    pub(crate) fn is_equal(&self) -> bool {
        self.first_difference.is_none()
    }

    /// Print the outcome under `label`; on a difference, the record number
    /// and both records, each cut to a bounded length.
    pub(crate) fn report(&self, harness: &mut Harness, label: &str) {
        match &self.first_difference {
            None => harness.msg(&format!("  {label}: {} records equal PASS", self.expected)),
            Some((at, expected, actual)) => {
                harness.msg(&format!(
                    "  {label}: {} expected records, {} actual, first difference at record {at} FAIL",
                    self.expected, self.actual
                ));
                let show = |r: &Option<String>| match r {
                    None => "(none: the listing ends)".to_owned(),
                    Some(r) => bounded(r.trim_end()),
                };
                harness.msg(&format!("    expected: {}", show(expected)));
                harness.msg(&format!("    actual:   {}", show(actual)));
            }
        }
    }
}

fn bounded(s: &str) -> String {
    use crate::tunables::CHANGE_PREVIEW_CHARS as MAX;
    match s.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}...", &s[..cut]),
        None => s.to_owned(),
    }
}

/// Compare the listing `expected`, each record as pbfhogg's writers render
/// it (see the module docs), with the listing `actual` record for record;
/// both are read to their ends, so the counts are whole.
pub(crate) fn compare(expected: &Path, actual: &Path) -> Result<Comparison, Error> {
    let mut left = ListingReader::open(expected)?;
    let mut right = ListingReader::open(actual)?;
    let mut comparison = Comparison {
        expected: 0,
        actual: 0,
        first_difference: None,
    };
    loop {
        let l = left.next_record()?.map(|(_, c)| c.as_pbfhogg_writes());
        let r = right.next_record()?.map(|(_, c)| c);
        if l.is_none() && r.is_none() {
            return Ok(comparison);
        }
        comparison.expected += u64::from(l.is_some());
        comparison.actual += u64::from(r.is_some());
        if comparison.first_difference.is_none() && l != r {
            let at = comparison.expected.max(comparison.actual);
            comparison.first_difference = Some((at, l.map(|c| c.record()), r.map(|c| c.record())));
        }
    }
}

/// Compare two plain line files byte for byte, line by line (osmium OPL
/// outputs), with the same report shape as [`compare`].
pub(crate) fn compare_lines(expected: &Path, actual: &Path) -> Result<Comparison, Error> {
    let mut left = BufReader::new(File::open(expected)?);
    let mut right = BufReader::new(File::open(actual)?);
    let mut comparison = Comparison {
        expected: 0,
        actual: 0,
        first_difference: None,
    };
    let (mut l, mut r) = (String::new(), String::new());
    loop {
        l.clear();
        r.clear();
        let lr = left.read_line(&mut l)?;
        let rr = right.read_line(&mut r)?;
        if lr == 0 && rr == 0 {
            return Ok(comparison);
        }
        comparison.expected += u64::from(lr > 0);
        comparison.actual += u64::from(rr > 0);
        if comparison.first_difference.is_none() && (lr == 0 || rr == 0 || l != r) {
            let at = comparison.expected.max(comparison.actual);
            let some = |n: usize, s: &String| (n > 0).then(|| s.clone());
            comparison.first_difference = Some((at, some(lr, &l), some(rr, &r)));
        }
    }
}

/// Whether two change files carry the same decompressed content (gzip
/// framing not compared), with a message naming the first differing offset.
pub(crate) fn same_content(a: &Path, b: &Path) -> Result<Option<String>, Error> {
    Ok(osc::first_content_difference(a, b)?.map(|at| {
        format!(
            "{} and {} differ from decompressed byte {at}",
            a.display(),
            b.display()
        )
    }))
}

/// Whether a file exists, an unreadable answer being an error.
pub(crate) fn exists(path: &Path) -> Result<bool, Error> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn change(action: OscAction, opl: &str) -> Change {
        let (kind, id) = opl_identity(opl).unwrap();
        Change {
            action,
            kind,
            id,
            meta: OscMeta {
                version: Some(2),
                timestamp: Some("2020-01-01T00:00:00Z".into()),
                changeset: Some(String::new()),
                uid: None,
                user: Some("a\tb%c\nd".into()),
                visible: None,
            },
            opl: opl.into(),
        }
    }

    #[test]
    fn a_record_round_trips_with_control_characters_in_metadata() {
        let c = change(
            OscAction::Modify,
            "n5 v2 dV c0 t2020-01-01T00:00:00Z i0 u Ta=b x1 y2",
        );
        let line = c.record();
        assert_eq!(line.matches('\n').count(), 1);
        assert_eq!(Change::parse(line.trim_end_matches('\n')).unwrap(), c);
    }

    #[test]
    fn pbfhogg_writes_a_delete_with_its_body_and_an_empty_string_as_absent() {
        let d = change(
            OscAction::Delete,
            "w9 v2 dD c0 t2020-01-01T00:00:00Z i0 u Ta=b Nn1,n2",
        );
        assert!(d.has_body());
        assert!(d.has_field('N') && !d.has_field('M'));
        let written = d.as_pbfhogg_writes();
        assert_eq!(written.opl, d.opl);
        assert_eq!(written.meta.changeset, None);
        assert_eq!(written.meta.user, d.meta.user);
        let m = change(OscAction::Modify, "n1 v2 dV c0 t i0 u Ta=b x1 y2");
        assert_eq!(m.as_pbfhogg_writes().opl, m.opl);
    }

    #[test]
    fn opl_versions_and_timestamps_are_read() {
        let c = change(
            OscAction::Create,
            "n1 v7 dV c0 t1970-01-02T00:00:01Z i0 u T x y",
        );
        assert_eq!(c.opl_version().unwrap(), 7);
        assert_eq!(c.opl_timestamp().unwrap(), Some(86_401));
        let none = change(OscAction::Create, "n1 v0 dV c0 t i0 u T x y");
        assert_eq!(none.opl_timestamp().unwrap(), None);
        assert_eq!(parse_iso_seconds("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(parse_iso_seconds("2000-03-01 00:00:00Z"), None);
    }

    #[test]
    fn comparisons_name_the_first_differing_record_and_count_both_sides() {
        let scratch_dir = pbfhogg_test_scratch::scratch();
        let dir = scratch_dir.path().join("fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let a = change(OscAction::Delete, "n1 v2 dD c0 t i0 u Tx=y x1 y1");
        let mut b = a.as_pbfhogg_writes();
        let expected = dir.join("e.changes");
        let actual = dir.join("a.changes");
        std::fs::write(&expected, a.record()).unwrap();
        std::fs::write(&actual, b.record()).unwrap();
        assert!(compare(&expected, &actual).unwrap().is_equal());
        // The projection is the expected side's only: an actual empty
        // attribute differs.
        std::fs::write(&actual, a.record()).unwrap();
        assert!(!compare(&expected, &actual).unwrap().is_equal());
        // A delete that lost its body, or a missing location written as
        // 0 0, differs.
        for opl in ["n1 v2 dD c0 t i0 u T x y", "n1 v2 dD c0 t i0 u Tx=y x0 y0"] {
            let mut lost = b.clone();
            lost.opl = opl.into();
            std::fs::write(&actual, lost.record()).unwrap();
            assert!(!compare(&expected, &actual).unwrap().is_equal());
        }
        b.action = OscAction::Modify;
        std::fs::write(&actual, format!("{}{}", b.record(), b.record())).unwrap();
        let cmp = compare(&expected, &actual).unwrap();
        assert_eq!((cmp.expected, cmp.actual), (1, 2));
        assert_eq!(cmp.first_difference.unwrap().0, 1);
    }
}
