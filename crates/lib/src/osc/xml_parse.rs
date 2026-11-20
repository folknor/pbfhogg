//! The single OSC (.osc) XML state machine.
//!
//! There used to be three of these: `handle_start_event_compact` here,
//! and `merge_changes::handle_start_like` / `handle_start_like_streaming`,
//! the latter two byte-for-byte identical apart from where the finished
//! element went. They disagreed - on metadata leniency, on whether an
//! element outside a `<create>` / `<modify>` / `<delete>` section was an
//! error or a silent create, on whether a missing `lat` meant Null Island
//! or a parse failure, and on whether an uncompressed `.osc` could be read
//! at all. Two code paths answering the same question differently is the
//! recurring defect in this codebase; the answer is one machine.
//!
//! [`drive_osc`] owns the *framing*: gzip sniffing, the element hierarchy
//! (root, sections, objects, their children; anything misplaced is
//! refused), self-closing elements, child routing, and the end-of-document
//! completeness check. Text, comments and processing instructions are
//! ignored wherever they appear, except that every event is held to XML
//! 1.0's `Char` production ([`check_event_chars`]); beyond that this is a
//! hierarchy check, not XML validation beyond what quick-xml does. An
//! [`OscSink`] owns the *data*:
//! what an element turns into once the framing has identified it.

use std::cell::Cell;
use std::io::{self, BufReader, Read};
use std::path::Path;
use std::rc::Rc;

use flate2::read::MultiGzDecoder;
use quick_xml::Reader;
use quick_xml::events::Event;
use quick_xml::events::attributes::Attribute;

use crate::ElementKind;
use crate::coord_fmt::deg_to_e7;
use crate::coord_pack::{validate_lat, validate_lon};
use crate::read::elements::MemberType;

use super::ParseResult;
use super::compact::{
    ArenaMeta, META_FLAG_HAS_CHANGESET, META_FLAG_HAS_TIMESTAMP, META_FLAG_HAS_UID,
    META_FLAG_HAS_USER, META_FLAG_HAS_VERSION, META_FLAG_HAS_VISIBLE, META_FLAG_HIDDEN,
    META_FLAG_PRESENT, member_type_to_byte,
};
use super::parse::CompactDiffOverlay;
use crate::error::Error;

// ---------------------------------------------------------------------------
// Shared vocabulary
// ---------------------------------------------------------------------------

/// Which `<create>` / `<modify>` / `<delete>` section an element sits in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum OscAction {
    Create,
    Modify,
    Delete,
}

impl OscAction {
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Modify => "modify",
            Self::Delete => "delete",
        }
    }
}

/// What a sink does with the children (`tag`, `nd`, `member`) of an object
/// in `<delete>`. Every sink states it; there is no default, so a sink that
/// re-emits deletes cannot lose their bodies by omission.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DeleteChildren {
    /// Route them to the sink like a live object's children. The
    /// re-emitting sinks (merge-changes, tags-filter OSC) and the lexical
    /// recorder that feeds them: a delete in a Geofabrik diff carries the
    /// object's last state, and osmium writes it back out.
    Route,
    /// Check them with the parsers a routed child gets, then drop them. A
    /// sink whose answer a delete's body cannot change (apply-changes'
    /// overlay keeps a versioned tombstone).
    CheckAndDrop,
}

/// Where a driven OSC document's elements go.
///
/// The driver guarantees the call sequence: exactly one `begin_element`
/// per element, followed by zero or more `tag` / `nd` / `member` calls
/// (already filtered to the kinds that can carry them), followed by
/// exactly one `end_element`. A child of an object in `<delete>` is routed
/// or checked and dropped as the sink's [`OscSink::DELETE_CHILDREN`] says,
/// by the driver and by [`LexBatch::replay`] alike; either way it is read
/// with the same parsers ([`parse_tag`], [`parse_nd`], [`parse_member`]),
/// which is what makes every command refuse the same malformed delete. A
/// self-closing element gets `begin_element`
/// immediately followed by `end_element`. The sequence is balanced on
/// success; when the driver fails (a misplaced element, truncation, a sink
/// error) the last element may be left begun and never ended.
pub(crate) trait OscSink {
    /// Whether this sink receives the children of deleted objects.
    const DELETE_CHILDREN: DeleteChildren;

    /// Whether `end_element` emits the finished element (serializes or
    /// writes it), so that a failure there is the output's and not a fault
    /// of the input. Every other callback only interprets the input, and
    /// the driver locates its failures with the input file.
    const END_ELEMENT_EMITS: bool = false;

    fn begin_element(
        &mut self,
        action: OscAction,
        kind: ElementKind,
        e: &quick_xml::events::BytesStart<'_>,
    ) -> ParseResult<()>;
    fn tag(&mut self, e: &quick_xml::events::BytesStart<'_>) -> ParseResult<()>;
    fn nd(&mut self, e: &quick_xml::events::BytesStart<'_>) -> ParseResult<()>;
    fn member(&mut self, e: &quick_xml::events::BytesStart<'_>) -> ParseResult<()>;
    fn end_element(&mut self) -> ParseResult<()>;
}

// ---------------------------------------------------------------------------
// Attribute parsing helpers (shared by every sink)
// ---------------------------------------------------------------------------

/// Classify an XML-level failure (a malformed attribute, an unreadable or
/// ill-formed document) as invalid OSC input, keeping the XML error as the
/// cause.
fn xml_err(source: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::invalid_data_with("malformed OSC XML", source)
}

/// Classify a failure of the XML reader itself. A failed read of the
/// underlying stream (EIO, a corrupt gzip member) is an I/O failure and keeps
/// its `io::ErrorKind`; only syntax and encoding errors are invalid input.
fn xml_read_err(source: quick_xml::Error) -> Error {
    match source {
        quick_xml::Error::Io(io_err) => Error::from(io::Error::new(io_err.kind(), io_err)),
        other => xml_err(other),
    }
}

fn missing_attr(name: &str) -> Error {
    Error::invalid_data(format!("missing attribute '{name}'"))
}

/// Parse a typed attribute value as XML defines it: after attribute-value
/// normalization, so a character reference (`id="&#49;"`) reads as the
/// character it names. The raw text is tried first and normalized only when
/// that fails, which keeps normal input at one parse with no normalization
/// scan. That shortcut is exact only for parsers that refuse `&`, tab, LF and
/// CR, because XML 1.0 normalization changes nothing else: every `parse`
/// passed here (`i32`, `i64` and `f64` `FromStr`, `parse_rfc3339_utc`) does.
/// On failure the error names the normalized text; a reference that does not
/// resolve is malformed XML.
fn parse_decoded<T, E>(
    attr: &Attribute<'_>,
    parse: impl Fn(&str) -> std::result::Result<T, E>,
) -> ParseResult<std::result::Result<T, (String, E)>> {
    let raw = attr.value.as_ref();
    let err = match parse(raw) {
        Ok(v) => return Ok(Ok(v)),
        Err(err) => err,
    };
    if !raw.as_bytes().contains(&b'&') {
        return Ok(Err((raw.to_owned(), err)));
    }
    let text = attr
        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
        .map_err(xml_err)?;
    Ok(parse(&text).map_err(|err| (text.into_owned(), err)))
}

pub(crate) fn parse_i64_attr(
    e: &quick_xml::events::BytesStart<'_>,
    name: &str,
) -> ParseResult<i64> {
    for attr_result in e.attributes() {
        let attr = attr_result.map_err(xml_err)?;
        if attr.key.as_ref() == name {
            return parse_decoded(&attr, str::parse::<i64>)?.map_err(|(text, err)| {
                Error::invalid_data_with(
                    format!("attribute '{name}' is not a valid integer: '{text}'"),
                    err,
                )
            });
        }
    }
    Err(missing_attr(name))
}

/// The longest coordinate token the `f64` path decides alone. Up to this
/// length a nonzero mantissa shifts the decimal order by at most 31 places,
/// so Rust's exponent-accumulation cap can only bite where both the true and
/// the parsed value overflow or underflow alike; every other such token
/// parses correctly rounded. Real coordinates are under 16 bytes.
const COORD_FAST_MAX_LEN: usize = 32;

/// Read a coordinate attribute: its `f64` degrees (which the geographic check
/// judges, as before) and, where the `f64` product cannot be trusted to round
/// like the decimal text, the exact decimicrodegree value of that text.
///
/// libosmium rounds the decimal digits themselves, half away from zero, so
/// `39.00704275` is `390070428`, while `39.00704275 * 1e7` in `f64` is
/// `390070427.49999994`. A correctly rounded parse of `|deg| <= 180` and the
/// multiply together err by under `2.7e-7` units, and rounding only changes
/// at half-units, so outside a `1e-6` window around `.5` the `f64` answer is
/// the exact one; seven-decimal input never enters it. The inner `Option`
/// is `None` when the exact value does not fit.
#[allow(clippy::type_complexity)]
fn parse_coord_attr(
    e: &quick_xml::events::BytesStart<'_>,
    name: &str,
) -> ParseResult<(f64, Option<Option<i64>>)> {
    let parse = |s: &str| {
        let deg = s.parse::<f64>()?;
        let frac = deg_to_e7(deg).abs().fract();
        let exact = if s.len() > COORD_FAST_MAX_LEN || (frac - 0.5).abs() <= 1e-6 {
            Some(exact_decimal_e7(s))
        } else {
            None
        };
        Ok::<_, std::num::ParseFloatError>((deg, exact))
    };
    for attr_result in e.attributes() {
        let attr = attr_result.map_err(xml_err)?;
        if attr.key.as_ref() == name {
            return parse_decoded(&attr, parse)?.map_err(|(text, err)| {
                Error::invalid_data_with(
                    format!("attribute '{name}' is not a valid number: '{text}'"),
                    err,
                )
            });
        }
    }
    Err(missing_attr(name))
}

/// The decimicrodegree value of a decimal spelling `f64` `FromStr` accepted
/// (sign, digits with an optional point, optional exponent), rounded half
/// away from zero on the exact decimal value. `None` if the magnitude reaches
/// `1e11` units or the text is not such a spelling (`inf`, `nan`). Only the
/// retained digits and the first dropped one are read; exponent arithmetic
/// saturates, so an arbitrarily long token is still read exactly.
#[cold]
fn exact_decimal_e7(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let (neg, body) = match b.first() {
        Some(b'-') => (true, &b[1..]),
        Some(b'+') => (false, &b[1..]),
        _ => (false, b),
    };
    let mant_end = body
        .iter()
        .position(|c| *c == b'e' || *c == b'E')
        .unwrap_or(body.len());
    let (mant, exp_part) = body.split_at(mant_end);
    let exp: i64 = match exp_part.split_first() {
        None => 0,
        Some((_, rest)) => {
            let (eneg, digits) = match rest.first() {
                Some(b'-') => (true, &rest[1..]),
                Some(b'+') => (false, &rest[1..]),
                _ => (false, rest),
            };
            if digits.is_empty() {
                return None;
            }
            let mut v: i64 = 0;
            for c in digits {
                if !c.is_ascii_digit() {
                    return None;
                }
                v = v.saturating_mul(10).saturating_add(i64::from(c - b'0'));
            }
            if eneg { -v } else { v }
        }
    };
    let int_digits = mant.iter().position(|c| *c == b'.').unwrap_or(mant.len());
    // Decimal position of the first mantissa digit after scaling by 1e7.
    let mut pos = i64::try_from(int_digits)
        .unwrap_or(i64::MAX)
        .saturating_add(exp)
        .saturating_add(6);
    let mut acc: i64 = 0;
    let mut round_up = false;
    let mut any_digit = false;
    for c in mant {
        if *c == b'.' {
            continue;
        }
        if !c.is_ascii_digit() {
            return None;
        }
        any_digit = true;
        let d = i64::from(c - b'0');
        if pos >= 0 {
            if d != 0 {
                if pos > 10 {
                    return None;
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let unit = 10_i64.pow(pos as u32);
                acc = acc.checked_add(d * unit)?;
            }
        } else if pos == -1 {
            round_up = d >= 5;
            break;
        } else {
            break;
        }
        pos = pos.saturating_sub(1);
    }
    if !any_digit {
        return None;
    }
    if acc >= 100_000_000_000 {
        return None;
    }
    let v = acc + i64::from(round_up);
    Some(if neg { -v } else { v })
}

pub(crate) fn parse_str_attr(
    e: &quick_xml::events::BytesStart<'_>,
    name: &str,
) -> ParseResult<String> {
    let mut out = String::new();
    append_str_attr(e, name, &mut out)?;
    Ok(out)
}

/// [`parse_str_attr`], appending the normalized value to `out` instead of
/// allocating it: the same lookup and the same refusals.
pub(crate) fn append_str_attr(
    e: &quick_xml::events::BytesStart<'_>,
    name: &str,
    out: &mut String,
) -> ParseResult<()> {
    for attr_result in e.attributes() {
        let attr = attr_result.map_err(xml_err)?;
        if attr.key.as_ref() == name {
            let val = attr
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map_err(xml_err)?;
            out.push_str(&val);
            return Ok(());
        }
    }
    Err(missing_attr(name))
}

// A malformed attribute is an error even when the attribute is optional.
// Until 2026-10-26 a malformed optional `role` read as absent and an empty
// role was written; that silently altered the data. The accepted OSC contract
// changed (maintainer ruling, review audit) to match the PBF re-encoding
// paths, the OSC arena and libosmium, whose expat parser rejects malformed XML
// outright. Absent stays `Ok(false)`; only a malformed attribute is refused.
#[cfg(test)]
pub(crate) fn parse_str_attr_optional(
    e: &quick_xml::events::BytesStart<'_>,
    name: &str,
) -> ParseResult<Option<String>> {
    let mut out = String::new();
    Ok(append_str_attr_optional(e, name, &mut out)?.then_some(out))
}

/// An optional attribute's normalized value, appended to `out`;
/// `Ok(false)` when absent, refused when malformed.
pub(crate) fn append_str_attr_optional(
    e: &quick_xml::events::BytesStart<'_>,
    name: &str,
    out: &mut String,
) -> ParseResult<bool> {
    for attr_result in e.attributes() {
        let attr = attr_result.map_err(|err| {
            Error::invalid_data_with(
                format!(
                    "malformed attribute on <{}> while reading '{name}'",
                    e.name().into_inner()
                ),
                err,
            )
        })?;
        if attr.key.as_ref() == name {
            let val = attr
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map_err(|err| {
                    Error::invalid_data_with(
                        format!(
                            "malformed '{name}' attribute on <{}>",
                            e.name().into_inner()
                        ),
                        err,
                    )
                })?;
            out.push_str(&val);
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Object children - ONE interpretation, shared by every sink and the driver
// ---------------------------------------------------------------------------

/// A `<tag>`: its required `k` and `v`.
pub(crate) fn parse_tag(e: &quick_xml::events::BytesStart<'_>) -> ParseResult<(String, String)> {
    let k = parse_str_attr(e, "k")?;
    let v = parse_str_attr(e, "v")?;
    Ok((k, v))
}

/// An `<nd>`: its required `ref`.
pub(crate) fn parse_nd(e: &quick_xml::events::BytesStart<'_>) -> ParseResult<i64> {
    parse_i64_attr(e, "ref")
}

/// A `<member>`, read in one fixed order (type, ref, role) so every sink
/// reports the same error when several attributes are bad.
pub(crate) struct ParsedMember {
    pub(crate) kind: ElementKind,
    pub(crate) ref_id: i64,
    /// Empty when absent; a malformed role is refused (see
    /// [`append_str_attr_optional`]).
    pub(crate) role: String,
}

impl ParsedMember {
    #[cfg(test)]
    pub(crate) fn member_id(&self) -> crate::MemberId {
        match self.kind {
            ElementKind::Node => crate::MemberId::Node(self.ref_id),
            ElementKind::Way => crate::MemberId::Way(self.ref_id),
            ElementKind::Relation => crate::MemberId::Relation(self.ref_id),
        }
    }

    pub(crate) fn member_type(&self) -> MemberType {
        match self.kind {
            ElementKind::Node => MemberType::Node,
            ElementKind::Way => MemberType::Way,
            ElementKind::Relation => MemberType::Relation,
        }
    }
}

pub(crate) fn parse_member(e: &quick_xml::events::BytesStart<'_>) -> ParseResult<ParsedMember> {
    let mut role = String::new();
    let (kind, ref_id) = parse_member_into(e, &mut String::new(), &mut role)?;
    Ok(ParsedMember { kind, ref_id, role })
}

/// [`parse_member`] without allocating: the type text goes through
/// `type_scratch` (cleared first), the role is appended to `role_out`
/// (nothing when absent). The same order and the same refusals.
pub(crate) fn parse_member_into(
    e: &quick_xml::events::BytesStart<'_>,
    type_scratch: &mut String,
    role_out: &mut String,
) -> ParseResult<(ElementKind, i64)> {
    type_scratch.clear();
    append_str_attr(e, "type", type_scratch)?;
    let Some(kind) = ElementKind::from_name(type_scratch) else {
        return Err(Error::invalid_data(format!(
            "unknown relation member type: '{type_scratch}'"
        )));
    };
    let ref_id = parse_i64_attr(e, "ref")?;
    append_str_attr_optional(e, "role", role_out)?;
    Ok((kind, ref_id))
}

// ---------------------------------------------------------------------------
// Metadata scanning - ONE leniency decision, shared by every sink
// ---------------------------------------------------------------------------

/// One of the six OSM element metadata attributes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MetaField {
    Version,
    Timestamp,
    Changeset,
    Uid,
    User,
    Visible,
}

impl MetaField {
    fn name(self) -> &'static str {
        match self {
            Self::Version => "version",
            Self::Timestamp => "timestamp",
            Self::Changeset => "changeset",
            Self::Uid => "uid",
            Self::User => "user",
            Self::Visible => "visible",
        }
    }
}

/// Scan an element's metadata attributes, invoking `f` once per attribute
/// that is actually present. Returns whether the element carried any
/// metadata at all.
///
/// # The leniency this codifies
///
/// **Presence, not truthiness, and not version-gating.** An element that
/// carries `timestamp` but no `version` has metadata, and every attribute
/// it did carry is preserved. `merge_changes::parse_metadata` used to
/// return `None` the moment `version` was absent, throwing away
/// timestamp, changeset, uid, user and visible along with it - so the
/// same input file produced different metadata depending on which command
/// read it, and `merge-changes` silently stripped metadata that
/// `apply-changes` preserved. Presence wins because dropping data the
/// producer emitted is never the safer default, and because it is the
/// same rule `osc::write::push_info_attrs` was fixed to on the emit side.
///
/// **Absent is `None`; present-but-unparsable is an error.** The old
/// compact parser folded `version="abc"` into `version = 0`, which is
/// the absent-versus-zero collapse this codebase keeps having to undo -
/// and it is now load-bearing, because the version-regression gate in
/// `CompactDiffOverlay` compares versions. A garbage version reading as
/// 0 would make every subsequent version look like a step forward.
pub(crate) fn scan_meta_attrs<F>(
    e: &quick_xml::events::BytesStart<'_>,
    mut f: F,
) -> ParseResult<bool>
where
    F: FnMut(MetaField, &Attribute<'_>) -> ParseResult<()>,
{
    let mut any = false;
    for attr_result in e.attributes() {
        let attr = attr_result.map_err(xml_err)?;
        let field = match attr.key.as_ref() {
            "version" => MetaField::Version,
            "timestamp" => MetaField::Timestamp,
            "changeset" => MetaField::Changeset,
            "uid" => MetaField::Uid,
            "user" => MetaField::User,
            // An empty `visible` carries no visibility and is the attribute
            // absent, for every sink, so a change claims the same visibility
            // applied directly and after re-emission (which, skipping it
            // here, never writes it). It is
            // judged after the iterator has validated the attribute (a
            // duplicate still fails) and does not count as metadata.
            // libosmium refuses it; pbfhogg does no strict true/false
            // validation (DEVIATIONS.md). An empty `user` stays a claim: it
            // can name a blank user alongside a uid.
            "visible" if attr.value.is_empty() => continue,
            "visible" => MetaField::Visible,
            _ => continue,
        };
        any = true;
        f(field, &attr)?;
    }
    Ok(any)
}

/// Parse a metadata attribute's value as an integer, or fail naming the
/// attribute and the text that could not be read. Shared so that both
/// sinks apply the same strictness.
pub(crate) fn meta_int<T: std::str::FromStr>(
    field: MetaField,
    attr: &Attribute<'_>,
) -> ParseResult<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    parse_decoded(attr, str::parse::<T>)?.map_err(|(text, err)| {
        Error::invalid_data_with(
            format!(
                "OSC element metadata attribute '{}' has unparsable value '{text}'; \
                 refusing to record it as 0, because an absent or broken value is not the \
                 value zero and the version-regression gate compares these numbers",
                field.name()
            ),
            err,
        )
    })
}

// ---------------------------------------------------------------------------
// Input framing: gzip sniff
// ---------------------------------------------------------------------------

/// Wraps an `io::Read` and accumulates wall time spent in `read()` calls,
/// for the `merge_changes_decompress_ns` sidecar counter. Despite that
/// historical name, what it times is the input reader, not the codec: for
/// gzip input it is file reads plus decompression, for plain XML it is file
/// reads alone. It is neither isolated codec time nor the full parse wall
/// (XML parsing and sink work happen outside these calls). The name and
/// attribution are kept so the counter stays comparable with recorded runs.
///
/// It is also where an OSC parse observes the invocation's cancellation
/// token: once per `read()` of the input (one buffer refill of the XML
/// reader), never per XML event.
struct TimedRead<R: io::Read> {
    inner: R,
    /// Nanoseconds spent in `read()`, summed. Saturates rather than wraps:
    /// the sum would need centuries of cumulative read time to reach
    /// `i64::MAX` (where the emitted counter used to wrap negative) or
    /// `u64::MAX`.
    total: Rc<Cell<u64>>,
    cancel: crate::host::Invocation,
}

impl<R: io::Read> io::Read for TimedRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.cancel.check().map_err(io::Error::from)?;
        let start = crate::counters::Stopwatch::start();
        let result = self.inner.read(buf);
        self.total
            .set(self.total.get().saturating_add(start.elapsed_ns()));
        result
    }
}

pub(crate) type OscReader = Reader<BufReader<Box<dyn io::Read>>>;

/// Open an OSC file, transparently handling both gzip-compressed and
/// plain XML.
///
/// `apply-changes` used to wrap every input in `MultiGzDecoder`
/// unconditionally and fail on a plain `.osc` with a raw gzip error,
/// while `merge-changes` sniffed the magic and handled both. The sniff
/// belongs to the shared parser, not to one of its two callers.
///
/// Returns the reader and the shared cell accumulating time spent in the
/// input reader's `read()` calls (see `TimedRead`: file reads plus gzip
/// decompression, or file reads alone for plain XML).
pub(crate) fn open_osc_reader(
    path: &Path,
    cancel: &crate::host::Recorder,
) -> ParseResult<(OscReader, Rc<Cell<u64>>)> {
    let mut file = crate::read::input::open(path)?;
    // The input is read front to back exactly once and never rewound, so
    // it may be a stream (a FIFO, `/dev/stdin`): the sniffed prefix is
    // replayed ahead of the rest of the file rather than sought back over.
    // Three bytes, not the two the gzip magic needs, so that plain XML's
    // first buffer fill still holds a whole UTF-8 BOM for quick-xml.
    //
    // A file shorter than that is read as far as it goes and the XML
    // parser produces the complaint, rather than failing here with an EOF
    // error that names nothing.
    let read_error =
        |e: io::Error| Error::from(e).context(format!("failed to read {}", path.display()));
    let mut prefix = [0u8; 3];
    let mut got = 0usize;
    while got < prefix.len() {
        match file.read(&mut prefix[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(read_error(e)),
        }
    }
    let gzip = got >= 2 && prefix[..2] == [0x1f, 0x8b];
    let file = io::Cursor::new(prefix).take(got as u64).chain(file);

    let total = Rc::new(Cell::new(0_u64));
    let inner: Box<dyn io::Read> = if gzip {
        Box::new(TimedRead {
            inner: MultiGzDecoder::new(file),
            total: Rc::clone(&total),
            cancel: cancel.handle().clone(),
        })
    } else {
        Box::new(TimedRead {
            inner: file,
            total: Rc::clone(&total),
            cancel: cancel.handle().clone(),
        })
    };
    let mut reader = Reader::from_reader(BufReader::new(inner));
    reader.config_mut().trim_text(true);
    Ok((reader, total))
}

// ---------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------

fn action_for(name: &str) -> Option<OscAction> {
    match name {
        "create" => Some(OscAction::Create),
        "modify" => Some(OscAction::Modify),
        "delete" => Some(OscAction::Delete),
        _ => None,
    }
}

/// Where in the document hierarchy the driver stands. Every level has a
/// fixed parent, so the hierarchy needs no stack: the section action and
/// the open element kind sit in [`DriveState`]'s fixed slots while a
/// deeper level is open. quick-xml's default `check_end_names` (with
/// unmatched ends refused) guarantees every `End` closes the innermost
/// open `Start`, which is what makes popping by level sound.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Level {
    /// Before the root element.
    Doc,
    /// Inside `<osmChange>`.
    Root,
    /// An unrecognised element directly under the root (`<bounds>`, for
    /// instance): tolerated as a leaf, ignored, and refused if it has
    /// element children.
    Other,
    /// Inside `<create>` / `<modify>` / `<delete>`.
    Section,
    /// Inside a `<node>` / `<way>` / `<relation>`.
    Element,
    /// Inside a non-self-closing child of an element (`<tag>`, `<nd>`,
    /// `<member>`, `<bbox>` / `<bounds>`), which may not have element
    /// children.
    Leaf,
    /// The root has closed; the document is whole.
    Done,
}

struct DriveState {
    level: Level,
    section: OscAction,
    open: ElementKind,
}

/// The root element name, whose close proves the document is whole.
const ROOT: &str = "osmChange";

/// Refuse an element the OSC hierarchy does not allow where it appears.
/// The grammar follows libosmium's section and object-child structure
/// (`xml_input_format.hpp`): sections only under the root, only objects in
/// sections, only `tag`, `nd` (ways), `member` (relations) and `bbox` /
/// `bounds` (ways, relations) in objects, and no element children below
/// that. Before this, a nested object replaced the sink's current element
/// and an unknown wrapper's descendants were routed into the enclosing
/// object - silent wrong answers, not tolerance.
#[cold]
fn misplaced(name: &str, parent: &str) -> Error {
    Error::invalid_data(format!(
        "OSC element <{name}> is not allowed {parent}; refusing a document whose \
         structure the change cannot be read from"
    ))
}

/// Which object child a routed callback is.
#[derive(Clone, Copy)]
enum Child {
    Tag,
    Nd,
    Member,
}

/// Hand one object child to `sink`, or, for a child of a deleted object
/// and a [`DeleteChildren::CheckAndDrop`] sink, interpret it with the
/// parser the sink would have used and drop it. The one place the delete
/// policy is applied, for the driver and for lexical replay.
fn route_child<S: OscSink>(
    child: Child,
    e: &quick_xml::events::BytesStart<'_>,
    section: OscAction,
    sink: &mut S,
) -> ParseResult<()> {
    let route = section != OscAction::Delete || S::DELETE_CHILDREN == DeleteChildren::Route;
    match (child, route) {
        (Child::Tag, true) => sink.tag(e),
        (Child::Tag, false) => parse_tag(e).map(drop),
        (Child::Nd, true) => sink.nd(e),
        (Child::Nd, false) => parse_nd(e).map(drop),
        (Child::Member, true) => sink.member(e),
        (Child::Member, false) => parse_member(e).map(drop),
    }
}

/// Route an object's child (`level == Element`) to the sink, or refuse it.
fn handle_element_child<S: OscSink>(
    name: &str,
    e: &quick_xml::events::BytesStart<'_>,
    section: OscAction,
    kind: ElementKind,
    sink: &mut S,
) -> ParseResult<()> {
    match (name, kind) {
        ("tag", _) => route_child(Child::Tag, e, section, sink),
        ("nd", ElementKind::Way) => route_child(Child::Nd, e, section, sink),
        ("member", ElementKind::Relation) => route_child(Child::Member, e, section, sink),
        ("bbox" | "bounds", ElementKind::Way | ElementKind::Relation) => Ok(()),
        _ => Err(misplaced(name, &format!("inside <{}>", kind.as_str()))),
    }
}

/// Locate a failure of reading or interpreting the OSC input at `path`. A
/// cancellation stays unlocated, as does an error that already names a
/// location. There is no byte offset: the parser's position counts
/// decompressed bytes of a `.osc.gz`, not bytes of the file.
#[cold]
fn input_fault(path: &Path, err: Error) -> Error {
    err.locate_blob(crate::error::Location::in_file(path))
}

/// `sink.end_element()`, its failure located in the input unless the sink
/// emits the finished element there (see [`OscSink::END_ELEMENT_EMITS`]).
fn end_element<S: OscSink>(sink: &mut S, path: &Path) -> ParseResult<()> {
    let result = sink.end_element();
    if S::END_ELEMENT_EMITS {
        result
    } else {
        result.map_err(|e| input_fault(path, e))
    }
}

fn handle_open<S: OscSink>(
    e: &quick_xml::events::BytesStart<'_>,
    is_empty: bool,
    state: &mut DriveState,
    sink: &mut S,
    path: &Path,
) -> ParseResult<()> {
    let name = e.name();
    let name = name.as_ref();

    // A self-closing element is admitted by exactly the rules a `Start`
    // is, then closes in the same event, so the level never moves for it.
    match state.level {
        Level::Element => {
            // The hot arm: every tag / nd / member of every object.
            handle_element_child(name, e, state.section, state.open, sink)
                .map_err(|e| input_fault(path, e))?;
            if !is_empty {
                state.level = Level::Leaf;
            }
        }
        Level::Section => {
            let Some(kind) = ElementKind::from_name(name) else {
                return Err(input_fault(
                    path,
                    misplaced(name, &format!("inside <{}>", state.section.tag())),
                ));
            };
            sink.begin_element(state.section, kind, e)
                .map_err(|e| input_fault(path, e))?;
            if is_empty {
                end_element(sink, path)?;
            } else {
                state.open = kind;
                state.level = Level::Element;
            }
        }
        Level::Root => {
            if let Some(action) = action_for(name) {
                if !is_empty {
                    state.section = action;
                    state.level = Level::Section;
                }
            } else if let Some(kind) = ElementKind::from_name(name) {
                // An element outside a section is malformed OSC. The
                // compact parser used to treat it as a create and put it in
                // the overlay; merge-changes errored for self-closing
                // elements and silently dropped non-self-closing ones. All
                // three answers were different, and only refusing is
                // defensible - we cannot know whether the producer meant to
                // create, modify or delete it.
                return Err(input_fault(
                    path,
                    Error::invalid_data(format!(
                        "OSC {} {} appears outside any <create>, <modify> or <delete> \
                         section; refusing to guess which change it was meant to be",
                        kind.as_str(),
                        parse_i64_attr(e, "id")
                            .map_or_else(|_| "(no id)".to_string(), |id| id.to_string())
                    )),
                ));
            } else if name == ROOT {
                return Err(input_fault(path, misplaced(name, "inside <osmChange>")));
            } else if !is_empty {
                state.level = Level::Other;
            }
        }
        Level::Doc => {
            if name != ROOT {
                return Err(input_fault(
                    path,
                    misplaced(name, "as the document root (expected <osmChange>)"),
                ));
            }
            // `<osmChange version="0.6"/>` is a complete, valid, empty
            // change document.
            state.level = if is_empty { Level::Done } else { Level::Root };
        }
        Level::Leaf => {
            return Err(input_fault(
                path,
                misplaced(name, "inside a <tag>, <nd>, <member> or <bbox>"),
            ));
        }
        Level::Other => {
            return Err(input_fault(
                path,
                misplaced(name, "inside an unrecognised element"),
            ));
        }
        Level::Done => {
            return Err(input_fault(path, misplaced(name, "after </osmChange>")));
        }
    }
    Ok(())
}

/// Close the innermost open element. quick-xml has already matched the
/// name against its `Start`, so the level alone says what closes.
fn handle_close<S: OscSink>(state: &mut DriveState, sink: &mut S, path: &Path) -> ParseResult<()> {
    state.level = match state.level {
        Level::Leaf => Level::Element,
        Level::Element => {
            end_element(sink, path)?;
            Level::Section
        }
        Level::Section | Level::Other => Level::Root,
        Level::Root => Level::Done,
        // Unreachable with matched end names: nothing is open.
        Level::Doc | Level::Done => state.level,
    };
    Ok(())
}

// ---------------------------------------------------------------------------
// XML 1.0 Char on input
// ---------------------------------------------------------------------------

/// Whether `cp` is in XML 1.0's `Char` production: tab, LF, CR,
/// U+0020-U+D7FF, U+E000-U+FFFD, U+10000-U+10FFFF.
fn is_xml_char(cp: u32) -> bool {
    matches!(cp, 0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x1_0000..=0x10_FFFF)
}

#[cold]
fn forbidden_char(cp: u32, how: &str) -> Error {
    Error::invalid_data(format!(
        "malformed OSC XML: the document holds U+{cp:04X} {how}, which XML 1.0 forbids \
         anywhere in a document; refusing it as a conforming XML reader (libosmium's \
         Expat) does"
    ))
}

/// Decode the numeric character reference whose body (between `&#` and
/// `;`, `x` included) is `body`, and refuse it unless it is a `Char`.
/// Spelling follows XML 1.0's `CharRef`: decimal digits or lowercase `x`
/// and hex digits, at least one, any number of leading zeros.
fn check_char_ref(body: &[u8]) -> ParseResult<()> {
    let (digits, radix) = match body.split_first() {
        Some((b'x', rest)) => (rest, 16),
        _ => (body, 10),
    };
    let mut cp: u32 = 0;
    for &d in digits {
        let v = char::from(d)
            .to_digit(radix)
            .ok_or_else(|| malformed_char_ref(body))?;
        // Saturate past the Unicode range: still refused, never wrapped.
        cp = cp.saturating_mul(radix).saturating_add(v).min(0x11_0000);
    }
    if digits.is_empty() {
        return Err(malformed_char_ref(body));
    }
    if is_xml_char(cp) {
        Ok(())
    } else if cp > 0x10_FFFF {
        Err(Error::invalid_data(format!(
            "malformed OSC XML: character reference &#{}; exceeds U+10FFFF",
            String::from_utf8_lossy(body)
        )))
    } else {
        Err(forbidden_char(cp, "as a character reference"))
    }
}

#[cold]
fn malformed_char_ref(body: &[u8]) -> Error {
    Error::invalid_data(format!(
        "malformed OSC XML: malformed character reference &#{};",
        String::from_utf8_lossy(body)
    ))
}

/// Refuse a literal scalar XML 1.0 forbids. `bytes` is valid UTF-8 (every
/// quick-xml event payload is a `str`), so a byte below 0x20 is an ASCII
/// scalar and `EF BF BE` / `EF BF BF` are exactly U+FFFE / U+FFFF. When
/// `refs` is set, every `&#...;` is also decoded and checked: in a start
/// or empty tag a raw `&` can only begin a reference in a quoted attribute
/// value, so this covers every attribute, the ones no sink reads included,
/// while `&amp;#1;` (a `&` followed by `amp;`) stays literal text.
///
/// The fast path tests eight bytes at a time for any byte below 0x20, any
/// 0xEF and (with `refs`) any `&`; only from the first word holding a
/// candidate on is the event walked byte by byte. A tab, LF or CR is a
/// candidate the walk then passes, which costs nothing on OSC as written:
/// its whitespace sits between tags, where quick-xml trims it away.
fn check_chars(bytes: &[u8], refs: bool) -> ParseResult<()> {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    // Exact "some byte is zero" / "some byte is below n" word tests.
    let has_zero = |v: u64| v.wrapping_sub(LO) & !v & HI;
    // Without `refs`, repeat the 0xEF test rather than match nothing.
    let amp = LO * if refs { u64::from(b'&') } else { 0xEF };
    let (words, rest) = bytes.as_chunks::<8>();
    let mut start = 0;
    for word in words {
        let w = u64::from_le_bytes(*word);
        let below_space = w.wrapping_sub(LO * 0x20) & !w & HI;
        if below_space | has_zero(w ^ (LO * 0xEF)) | has_zero(w ^ amp) != 0 {
            return check_chars_slow(bytes, start, refs);
        }
        start += 8;
    }
    if rest
        .iter()
        .any(|&b| b < 0x20 || b == 0xEF || (refs && b == b'&'))
    {
        return check_chars_slow(bytes, start, refs);
    }
    Ok(())
}

#[cold]
fn check_chars_slow(bytes: &[u8], from: usize, refs: bool) -> ParseResult<()> {
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b @ 0..0x20 if !matches!(b, b'\t' | b'\n' | b'\r') => {
                return Err(forbidden_char(u32::from(b), "literally"));
            }
            0xEF if bytes.get(i + 1) == Some(&0xBF)
                && matches!(bytes.get(i + 2), Some(0xBE | 0xBF)) =>
            {
                let cp = if bytes[i + 2] == 0xBE { 0xFFFE } else { 0xFFFF };
                return Err(forbidden_char(cp, "literally"));
            }
            b'&' if refs && bytes.get(i + 1) == Some(&b'#') => {
                let body = &bytes[i + 2..];
                let end = body
                    .iter()
                    .position(|&b| b == b';')
                    .ok_or_else(|| malformed_char_ref(body))?;
                check_char_ref(&body[..end])?;
                i += 2 + end;
            }
            _ => {}
        }
        i += 1;
    }
    Ok(())
}

/// Whether a DOCTYPE payload opens an internal subset: a `[` outside the
/// quoted literals of its external identifier (`SYSTEM "https://[::1]/x"`
/// is legal and has none).
fn has_internal_subset(payload: &[u8]) -> bool {
    let mut quote = None;
    for &b in payload {
        match (quote, b) {
            (None, b'"' | b'\'') => quote = Some(b),
            (Some(q), _) if b == q => quote = None,
            (None, b'[') => return true,
            _ => {}
        }
    }
    false
}

/// Hold one event to XML 1.0's `Char` production. The driver ignores most
/// events, but a document carrying a forbidden character is not XML, and
/// libosmium's Expat refuses all of it before any OSM interpretation; so
/// does this, on the reader thread every route shares, before the event is
/// dispatched - so no filter predicate or simplify dedupe decides whether
/// the refusal happens. Literal scalars are checked in every event's
/// payload (quick-xml's payloads omit only delimiters, the BOM and XML
/// whitespace); character references are decoded where XML gives them
/// meaning: attribute values and text (`GeneralRef`). Inside comments,
/// CDATA and processing instructions `&#1;` is legal literal text.
///
/// A DOCTYPE with an internal subset is refused outright: its entity
/// values and attribute defaults would need a second grammar to check,
/// neither pbfhogg's nor osmium's OSC writers produce one, and libosmium
/// refuses entity declarations. An
/// external identifier is inert - nothing loads it.
fn check_event_chars(ev: &Event<'_>) -> ParseResult<()> {
    match ev {
        Event::Start(e) | Event::Empty(e) => check_chars(e.as_bytes(), true),
        Event::GeneralRef(r) => {
            let name = r.as_bytes();
            match name.split_first() {
                Some((b'#', body)) => check_char_ref(body),
                _ => check_chars(name, false),
            }
        }
        Event::DocType(d) => {
            let payload = d.as_bytes();
            if has_internal_subset(payload) {
                return Err(Error::invalid_data(
                    "malformed OSC XML: the document's DOCTYPE carries an internal \
                     subset; OSC input refuses DTD declarations",
                ));
            }
            check_chars(payload, false)
        }
        other => check_chars(other.as_bytes(), false),
    }
}

/// Drive one OSC document read from `path` through `reader` into `sink`.
///
/// Every failure of reading or interpreting the input is located with
/// `path`; a failure the sink raises while emitting a finished element is
/// output's, and passes through unlocated (it names its own destination).
fn drive_osc<R: io::BufRead, S: OscSink>(
    reader: &mut Reader<R>,
    sink: &mut S,
    path: &Path,
) -> ParseResult<()> {
    let mut state = DriveState {
        level: Level::Doc,
        section: OscAction::Create,
        open: ElementKind::Node,
    };
    let mut buf = Vec::new();

    loop {
        let event = reader.read_event_into(&mut buf);
        if let Ok(ev) = &event {
            check_event_chars(ev).map_err(|e| input_fault(path, e))?;
        }
        match event {
            Ok(Event::Start(ref e)) => handle_open(e, false, &mut state, sink, path)?,
            Ok(Event::Empty(ref e)) => handle_open(e, true, &mut state, sink, path)?,
            Ok(Event::End(_)) => handle_close(&mut state, sink, path)?,
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => {
                // A read the input's cancellation check refused reaches here
                // wrapped in quick-xml's `Io(Arc<io::Error>)`, which the
                // carrier walk behind `Error::is_cancelled` cannot see
                // through: recognise that refusal and report the
                // cancellation itself, typed. Only that refusal - a syntax
                // error or a real read failure racing a cancellation stays
                // what it is.
                if let quick_xml::Error::Io(io_err) = &e
                    && crate::error::io_carries_cancellation(io_err)
                {
                    return Err(crate::error::new_cancelled());
                }
                return Err(input_fault(path, xml_read_err(e)));
            }
        }
        buf.clear();
    }

    // A `.osc.gz` truncated at a gzip member boundary decompresses
    // cleanly, quick-xml reports `Eof`, and every caller happily treated
    // the partial document as the whole change stream - apply-changes
    // wrote a partially-applied PBF and reported success. Only the
    // closing tag proves the document is whole.
    if state.level != Level::Done {
        return Err(input_fault(
            path,
            Error::invalid_data(
                "OSC input ended without a closing </osmChange> tag: the file is \
                 truncated (a .osc.gz cut at a gzip member boundary decompresses \
                 cleanly, so this is the only way to notice). Applying it would \
                 produce a partially-applied result reported as success.",
            ),
        ));
    }

    Ok(())
}

/// Open `path`, drive it into `sink`, and emit the input-read counter
/// (historically named `merge_changes_decompress_ns`; see `TimedRead`).
/// `cancel` is observed at every read of the input (see `TimedRead`).
pub(crate) fn parse_osc_path<S: OscSink>(
    path: &Path,
    sink: &mut S,
    cancel: &crate::host::Recorder,
) -> ParseResult<()> {
    let (mut reader, decompress_total) = open_osc_reader(path, cancel)?;
    let result = drive_osc(&mut reader, sink, path);
    // Deliberately emit this historical name once per file, without accumulation,
    // from every sink. Changing attribution or totals breaks brokkr run comparisons.
    cancel.counter("merge_changes_decompress_ns", decompress_total.get());
    result
}

// ---------------------------------------------------------------------------
// Lexical batches: the driver's callbacks, recorded for replay elsewhere
//
// A reader thread drives the document (tokenizing, framing, every structural
// check above) and records each callback's raw tag content instead of
// interpreting it. A worker then replays a batch into a real sink. No XML is
// parsed out of its document context: the replayed tags are exactly the ones
// the driver framed, and attribute decoding needs nothing from the document
// beyond the tag itself (predefined entities and character references only).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum LexOp {
    Begin(OscAction, ElementKind),
    Tag,
    Nd,
    Member,
    End,
}

struct LexEvent {
    op: LexOp,
    start: usize,
    end: usize,
    name_len: usize,
}

/// A run of recorded driver callbacks, whole elements only.
#[derive(Default)]
pub(crate) struct LexBatch {
    text: String,
    events: Vec<LexEvent>,
}

impl LexBatch {
    /// Recorded tag bytes, the unit batches are cut and budgeted by.
    pub(crate) fn bytes(&self) -> usize {
        self.text.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    fn push(&mut self, op: LexOp, e: Option<&quick_xml::events::BytesStart<'_>>) {
        let start = self.text.len();
        let name_len = e.map_or(0, |e| {
            self.text.push_str(e);
            e.name().into_inner().len()
        });
        self.events.push(LexEvent {
            op,
            start,
            end: self.text.len(),
            name_len,
        });
    }

    /// Replay the recorded callbacks into `sink`, locating failures exactly
    /// as [`drive_osc`] does: interpreting the input is the input's fault,
    /// an emitting `end_element` is the output's.
    /// A delete's children are routed by `S`'s [`OscSink::DELETE_CHILDREN`],
    /// as the driver routes them.
    pub(crate) fn replay<S: OscSink>(&self, sink: &mut S, path: &Path) -> ParseResult<()> {
        let mut section = OscAction::Create;
        for ev in &self.events {
            let e = quick_xml::events::BytesStart::from_content(
                &self.text[ev.start..ev.end],
                ev.name_len,
            );
            match ev.op {
                LexOp::Begin(action, kind) => {
                    section = action;
                    sink.begin_element(action, kind, &e)
                }
                LexOp::Tag => route_child(Child::Tag, &e, section, sink),
                LexOp::Nd => route_child(Child::Nd, &e, section, sink),
                LexOp::Member => route_child(Child::Member, &e, section, sink),
                LexOp::End => {
                    end_element(sink, path)?;
                    continue;
                }
            }
            .map_err(|err| input_fault(path, err))?;
        }
        Ok(())
    }
}

/// Sink that records callbacks into [`LexBatch`]es and hands each batch to
/// `seal` once it reaches `target` bytes, always at an element end. Call
/// [`LexSink::finish`] after a successful drive to seal the remainder.
/// `halted` is asked at every callback; once it answers true the sink
/// refuses, so a reader stops recording promptly when its consumer has
/// given up, even inside one huge element.
pub(crate) struct LexSink<F, H> {
    batch: LexBatch,
    target: usize,
    seal: F,
    halted: H,
}

impl<F: FnMut(LexBatch) -> ParseResult<()>, H: Fn() -> bool> LexSink<F, H> {
    pub(crate) fn new(target: usize, seal: F, halted: H) -> Self {
        Self {
            batch: LexBatch::default(),
            target,
            seal,
            halted,
        }
    }

    fn record(
        &mut self,
        op: LexOp,
        e: Option<&quick_xml::events::BytesStart<'_>>,
    ) -> ParseResult<()> {
        if (self.halted)() {
            return Err(Error::internal(
                "OSC reading stopped after a failure elsewhere",
            ));
        }
        self.batch.push(op, e);
        Ok(())
    }

    pub(crate) fn finish(mut self) -> ParseResult<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        (self.seal)(std::mem::take(&mut self.batch))
    }
}

impl<F: FnMut(LexBatch) -> ParseResult<()>, H: Fn() -> bool> OscSink for LexSink<F, H> {
    /// Recorded for the replaying sink, which applies its own policy.
    const DELETE_CHILDREN: DeleteChildren = DeleteChildren::Route;
    /// Sealing hands the batch on (and may wait for room); its failure is
    /// not a fault of the input.
    const END_ELEMENT_EMITS: bool = true;

    fn begin_element(
        &mut self,
        action: OscAction,
        kind: ElementKind,
        e: &quick_xml::events::BytesStart<'_>,
    ) -> ParseResult<()> {
        self.record(LexOp::Begin(action, kind), Some(e))
    }

    fn tag(&mut self, e: &quick_xml::events::BytesStart<'_>) -> ParseResult<()> {
        self.record(LexOp::Tag, Some(e))
    }

    fn nd(&mut self, e: &quick_xml::events::BytesStart<'_>) -> ParseResult<()> {
        self.record(LexOp::Nd, Some(e))
    }

    fn member(&mut self, e: &quick_xml::events::BytesStart<'_>) -> ParseResult<()> {
        self.record(LexOp::Member, Some(e))
    }

    fn end_element(&mut self) -> ParseResult<()> {
        self.record(LexOp::End, None)?;
        if self.batch.bytes() >= self.target {
            (self.seal)(std::mem::take(&mut self.batch))?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The compact-overlay sink (apply-changes)
// ---------------------------------------------------------------------------

/// Convert one OSC coordinate attribute to decimicrodegrees.
///
/// Geometry is not metadata, so the leniency `scan_meta_attrs` documents
/// does not extend here. A create/modify `<node>` whose `lat`/`lon` is
/// missing, empty or unparsable used to land at exactly (0, 0) - and 0/0
/// is a real coordinate in the Gulf of Guinea, so the result was
/// indistinguishable from a genuine node there. Worse, under
/// `--locations-on-ways` that (0, 0) propagates into every way referencing
/// the node. Out-of-range and non-finite values were equally silent: `as
/// i32` saturates `lat="1e30"` to `i32::MAX` and maps `NaN` to 0.
pub(crate) fn coord_to_decimicro(
    e: &quick_xml::events::BytesStart<'_>,
    name: &str,
    id: i64,
) -> ParseResult<i32> {
    let (degrees, exact) = parse_coord_attr(e, name).map_err(|err| {
        err.context(format!(
            "OSC node {id}: {name} attribute is missing or unparsable (a created or \
             modified node must carry usable coordinates; storing it at 0/0 would silently \
             move it to the Gulf of Guinea)"
        ))
    })?;
    // Two independent checks. The geographic one comes first because it is
    // the one a caller can reason about: lat="100" is not a place, and an
    // i32 happily holds 1_000_000_000 decimicrodegrees, so the
    // representation check below would wave it through. Such a node would
    // enter the overlay as an ordinary node and could propagate into
    // LocationsOnWays, where nothing downstream re-validates it. The range
    // rule itself is `coord_pack`'s; only the OSC node context is added here.
    let validate = if name == "lat" {
        validate_lat
    } else {
        validate_lon
    };
    validate(name, degrees)
        .map_err(|err| Error::invalid_data_from(err).context(format!("OSC node {id}")))?;
    if let Some(exact) = exact {
        let limit: i64 = if name == "lat" {
            900_000_000
        } else {
            1_800_000_000
        };
        return match exact {
            Some(v) if v.abs() <= limit => {
                // Bounded by the geographic limit just checked, so it fits.
                #[allow(clippy::cast_possible_truncation)]
                Ok(v as i32)
            }
            _ => Err(Error::invalid_data(format!(
                "OSC node {id}: {name} value is outside the geographic range once read exactly"
            ))),
        };
    }
    let scaled = deg_to_e7(degrees);
    if !scaled.is_finite() || scaled < f64::from(i32::MIN) || scaled > f64::from(i32::MAX) {
        return Err(Error::invalid_data(format!(
            "OSC node {id}: {name} value {degrees} is outside the range representable in \
             decimicrodegrees"
        )));
    }
    // The bounds check above rules out every value `as i32` would saturate
    // or map to 0, so the cast is exact.
    #[allow(clippy::cast_possible_truncation)]
    Ok(scaled.round() as i32)
}

/// The location a `<delete>` node carries, for the sinks that re-emit it.
///
/// A delete needs no location, but a Geofabrik diff gives every deleted node
/// its last one, and a re-emitted delete keeps what it was given. Neither
/// `lat` nor `lon` present: no location. Both present: each read as a live
/// node's is ([`coord_to_decimicro`]), refusing a malformed or out-of-range
/// value. Only one present: refused, since writing half a location (or
/// dropping the half that came) would change the change silently. An empty
/// attribute is present, not absent, and refuses as unparsable.
pub(crate) fn delete_coords(
    e: &quick_xml::events::BytesStart<'_>,
    id: i64,
) -> ParseResult<Option<(i32, i32)>> {
    let mut lat = false;
    let mut lon = false;
    for attr_result in e.attributes() {
        let attr = attr_result.map_err(xml_err)?;
        match attr.key.as_ref() {
            "lat" => lat = true,
            "lon" => lon = true,
            _ => {}
        }
    }
    match (lat, lon) {
        (false, false) => Ok(None),
        (true, true) => {
            let read = |name| {
                coord_to_decimicro(e, name, id).map_err(|err| {
                    err.context(format!(
                        "OSC deleted node {id}: a location given on a delete is re-emitted, \
                         so it must be usable"
                    ))
                })
            };
            Ok(Some((read("lat")?, read("lon")?)))
        }
        (true, false) | (false, true) => Err(Error::invalid_data(format!(
            "OSC deleted node {id} carries {} without {}; refusing to re-emit half a location",
            if lat { "lat" } else { "lon" },
            if lat { "lon" } else { "lat" },
        ))),
    }
}

/// Read an element's metadata into the owned representation used by the
/// two re-emitting sinks (`merge-changes` and `tags-filter --input-kind
/// osc`).
///
/// Leniency is `scan_meta_attrs`': presence, not version-gating. The
/// `merge-changes` copy used to open with `let version =
/// parse_i32_attr_optional(e, "version")?;` and return `None` when
/// `version` was absent - throwing away timestamp, changeset, uid, user
/// and visible along with it, so `merge-changes` silently stripped
/// metadata that `apply-changes` preserved from the very same file.
///
/// It lives here rather than in either command because both re-emit
/// paths need it and the whole point of this module is that there is one
/// answer per question. `tags-filter --input-kind osc` had no metadata
/// handling at all: it hardcoded `metadata: None` on every element it
/// kept, so filtering a change stream silently erased version,
/// timestamp, changeset, uid, user and visible from every surviving
/// change - the same data loss, in the sink nobody had looked at.
// Deliberately normalize XML text without validating timestamp, changeset, uid
// or visible. These re-emitting sinks pass metadata through; numeric validation
// would reject previously accepted input and add work to every changed element.
#[cfg(test)]
pub(crate) fn parse_owned_metadata(
    e: &quick_xml::events::BytesStart<'_>,
) -> ParseResult<Option<crate::osc::write::OwnedMetadata>> {
    let mut meta = crate::osc::write::OwnedMetadata {
        version: None,
        timestamp: String::new(),
        changeset: String::new(),
        uid: String::new(),
        user: String::new(),
        visible: String::new(),
        present: 0,
    };
    let present = scan_owned_metadata_into(e, &mut meta)?;
    Ok(if present { Some(meta) } else { None })
}

/// [`parse_owned_metadata`] into caller-owned storage, every field cleared
/// first, so a caller that reuses it allocates nothing once its strings
/// have grown. Returns whether the element carried any metadata.
pub(crate) fn scan_owned_metadata_into(
    e: &quick_xml::events::BytesStart<'_>,
    meta: &mut crate::osc::write::OwnedMetadata,
) -> ParseResult<bool> {
    use crate::osc::write::OwnedMetadata as M;
    meta.version = None;
    meta.present = 0;
    for text in [
        &mut meta.timestamp,
        &mut meta.changeset,
        &mut meta.uid,
        &mut meta.user,
        &mut meta.visible,
    ] {
        text.clear();
    }
    scan_meta_attrs(e, |field, attr| {
        // `version` is parsed rather than copied verbatim, because the
        // version-precedence gate compares it and because a non-numeric
        // version is not a version. The rest ride through as the
        // producer wrote them: this is a re-emit path, and reformatting
        // a timestamp or a uid would change bytes for no reason.
        if field == MetaField::Version {
            meta.version = Some(meta_int::<i32>(field, attr)?);
            return Ok(());
        }
        let text = attr
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(xml_err)?;
        // Presence is recorded apart from the text: an exactly empty
        // attribute is present and is re-emitted as written (an empty
        // `visible` never reaches here; `scan_meta_attrs` reads it as
        // absent). Applying the re-emitted change then sees what applying
        // the original saw: `user=""` stays a blank-user claim, and
        // `uid=""`, `changeset=""` or `timestamp=""` is refused by apply's
        // native parse either way, as libosmium refuses them on every route.
        let (slot, bit) = match field {
            MetaField::Timestamp => (&mut meta.timestamp, M::PRESENT_TIMESTAMP),
            MetaField::Changeset => (&mut meta.changeset, M::PRESENT_CHANGESET),
            MetaField::Uid => (&mut meta.uid, M::PRESENT_UID),
            MetaField::User => (&mut meta.user, M::PRESENT_USER),
            MetaField::Visible => (&mut meta.visible, M::PRESENT_VISIBLE),
            MetaField::Version => unreachable!("handled above"),
        };
        meta.present |= bit;
        // A repeated attribute keeps its last value, as the owned scan did.
        slot.clear();
        slot.push_str(&text);
        Ok(())
    })
}

/// Read an element's metadata into the arena representation.
// Deliberately keep the overlay's native numeric parsing and false-only visible
// test. Sharing owned-sink validation would change accepted input and its cost.
// The visible test does compare the decoded value, though: the re-emitters
// (merge-changes, OSC tags-filter) write the decoded text, so a raw comparison
// would make `visible="&#102;alse"` visible here and hidden once re-emitted.
fn arena_meta(
    e: &quick_xml::events::BytesStart<'_>,
    overlay: &mut CompactDiffOverlay,
) -> ParseResult<ArenaMeta> {
    let mut meta = ArenaMeta::default();
    let mut user: Option<String> = None;
    let present = scan_meta_attrs(e, |field, attr| {
        match field {
            MetaField::Version => {
                meta.version = meta_int::<i32>(field, attr)?;
                meta.flags |= META_FLAG_HAS_VERSION;
            }
            MetaField::Timestamp => {
                meta.flags |= META_FLAG_HAS_TIMESTAMP;
                meta.timestamp = parse_decoded(attr, crate::commands::parse_rfc3339_utc)?.map_err(
                    |(text, err)| {
                        Error::invalid_data_with(
                            format!(
                                "OSC element metadata attribute 'timestamp' has unparsable \
                             value '{text}'; refusing to record it as the epoch"
                            ),
                            err,
                        )
                    },
                )?;
            }
            MetaField::Changeset => {
                meta.changeset = meta_int::<i64>(field, attr)?;
                meta.flags |= META_FLAG_HAS_CHANGESET;
            }
            MetaField::Uid => {
                meta.uid = meta_int::<i32>(field, attr)?;
                meta.flags |= META_FLAG_HAS_UID;
            }
            MetaField::User => {
                user = Some(
                    attr.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map_err(xml_err)?
                        .into_owned(),
                );
            }
            MetaField::Visible => {
                meta.flags |= META_FLAG_HAS_VISIBLE;
                let text = attr
                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                    .map_err(xml_err)?;
                if text == "false" {
                    meta.flags |= META_FLAG_HIDDEN;
                }
            }
        }
        Ok(())
    })?;
    if present {
        meta.flags |= META_FLAG_PRESENT;
    }
    if let Some(name) = user {
        meta.user_id = overlay.intern(&name)?;
        meta.flags |= META_FLAG_HAS_USER;
    }
    Ok(meta)
}

/// What a [`CompactSink`] makes of a `<create>` or `<modify>` element that
/// says `visible="false"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HiddenUpserts {
    /// Stage it as an upsert carrying the hidden bit: the public overlay
    /// reports what the file says.
    Keep,
    /// Record it as a versioned delete: apply-changes writes a snapshot, and
    /// in OSM a record whose visibility is false is the deleted state of its
    /// object, so the object is absent if this change wins. osmium
    /// apply-changes does the same in its snapshot mode (it writes the
    /// winning record only when it is visible). The version claim goes
    /// through the delete path, so the stream and base version gates still
    /// see it. The body is parsed like an upsert's and dropped; a node's
    /// coordinates are not required.
    AsDelete,
}

/// Sink that appends into a [`CompactDiffOverlay`].
pub(crate) struct CompactSink<'a> {
    overlay: &'a mut CompactDiffOverlay,
    hidden: HiddenUpserts,
    /// `None` while no create/modify element is being staged - which is
    /// also the state during a `<delete>` element, whose children the
    /// driver checks and drops (`DeleteChildren::CheckAndDrop`), and during
    /// a hidden upsert recorded as a delete (`dropping`).
    staging: Option<Staging>,
    /// A hidden create or modify recorded as a delete is open: the driver
    /// still routes its children, which are parsed and dropped.
    dropping: bool,
}

struct Staging {
    kind: ElementKind,
    id: i64,
    lat: i32,
    lon: i32,
    meta: ArenaMeta,
    tag_keys: Vec<u32>,
    tag_values: Vec<String>,
    refs: Vec<i64>,
    members: Vec<(i64, u8, u32)>,
}

impl<'a> CompactSink<'a> {
    pub(crate) fn new(overlay: &'a mut CompactDiffOverlay, hidden: HiddenUpserts) -> Self {
        Self {
            overlay,
            hidden,
            staging: None,
            dropping: false,
        }
    }

    /// Record a versioned delete of `kind` `id` with `meta`'s claim.
    fn delete(&mut self, kind: ElementKind, id: i64, meta: &ArenaMeta) -> ParseResult<()> {
        match kind {
            ElementKind::Node => self.overlay.delete_node(id, meta),
            ElementKind::Way => self.overlay.delete_way(id, meta),
            ElementKind::Relation => self.overlay.delete_relation(id, meta),
        }
    }

    /// The element a child belongs to. The driver routes children only of
    /// created and modified objects, which always stage one.
    fn staged(&mut self) -> ParseResult<&mut Staging> {
        self.staging
            .as_mut()
            .ok_or_else(|| Error::internal("OSC overlay: an object child with no staged object"))
    }
}

impl OscSink for CompactSink<'_> {
    /// A delete becomes a versioned tombstone; its body cannot change that.
    const DELETE_CHILDREN: DeleteChildren = DeleteChildren::CheckAndDrop;

    fn begin_element(
        &mut self,
        action: OscAction,
        kind: ElementKind,
        e: &quick_xml::events::BytesStart<'_>,
    ) -> ParseResult<()> {
        let id = parse_i64_attr(e, "id")?;

        if action == OscAction::Delete {
            // A delete's metadata is read for the same reason a
            // create's is: its `version` is a claim about which state of
            // the object this change supersedes, and the overlay records
            // it as a versioned tombstone so a later stale change cannot
            // resurrect an earlier version through the gap a bare
            // index removal used to leave.
            //
            // Only the version claim is kept, yet the user name is
            // interned here like a create's: one metadata reader for every
            // action. It costs nothing measurable - at planet (daily diff
            // seq 4913, bygg) the strings only deletes interned were 48 of
            // 13,692, 537 bytes.
            let meta = arena_meta(e, self.overlay)?;
            self.delete(kind, id, &meta)?;
            self.staging = None;
            self.dropping = false;
            return Ok(());
        }

        // Deliberately leave the delete pending until insertion succeeds. Clearing it
        // here would let a rejected stale re-create erase the accepted deletion.
        let meta = arena_meta(e, self.overlay)?;
        if self.hidden == HiddenUpserts::AsDelete && meta.flags & META_FLAG_HIDDEN != 0 {
            self.delete(kind, id, &meta)?;
            self.staging = None;
            self.dropping = true;
            return Ok(());
        }
        self.dropping = false;
        let (lat, lon) = if kind == ElementKind::Node {
            (
                coord_to_decimicro(e, "lat", id)?,
                coord_to_decimicro(e, "lon", id)?,
            )
        } else {
            (0, 0)
        };

        self.staging = Some(Staging {
            kind,
            id,
            lat,
            lon,
            meta,
            tag_keys: Vec::new(),
            tag_values: Vec::new(),
            refs: Vec::new(),
            members: Vec::new(),
        });
        Ok(())
    }

    fn tag(&mut self, e: &quick_xml::events::BytesStart<'_>) -> ParseResult<()> {
        let (k, v) = parse_tag(e)?;
        if self.dropping {
            return Ok(());
        }
        let key_id = self.overlay.intern(&k)?;
        let st = self.staged()?;
        st.tag_keys.push(key_id);
        st.tag_values.push(v);
        Ok(())
    }

    fn nd(&mut self, e: &quick_xml::events::BytesStart<'_>) -> ParseResult<()> {
        let ref_id = parse_nd(e)?;
        if self.dropping {
            return Ok(());
        }
        self.staged()?.refs.push(ref_id);
        Ok(())
    }

    fn member(&mut self, e: &quick_xml::events::BytesStart<'_>) -> ParseResult<()> {
        let member = parse_member(e)?;
        let type_byte = member_type_to_byte(member.member_type())?;
        if self.dropping {
            return Ok(());
        }
        let role_id = self.overlay.intern(&member.role)?;
        self.staged()?
            .members
            .push((member.ref_id, type_byte, role_id));
        Ok(())
    }

    fn end_element(&mut self) -> ParseResult<()> {
        self.dropping = false;
        let Some(st) = self.staging.take() else {
            return Ok(());
        };
        let tags: Vec<(u32, &str)> = st
            .tag_keys
            .iter()
            .zip(st.tag_values.iter())
            .map(|(&k, v)| (k, v.as_str()))
            .collect();
        match st.kind {
            ElementKind::Node => self
                .overlay
                .push_node(st.id, st.lat, st.lon, &tags, &st.meta),
            ElementKind::Way => self.overlay.push_way(st.id, &st.refs, &tags, &st.meta),
            ElementKind::Relation => {
                self.overlay
                    .push_relation(st.id, &st.members, &tags, &st.meta)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coord(attrs: &str, name: &str) -> crate::Result<i32> {
        let element = quick_xml::events::BytesStart::from_content(format!("node {attrs}"), 4);
        coord_to_decimicro(&element, name, 1)
    }

    #[test]
    fn coordinates_round_the_decimal_text_like_libosmium() -> crate::Result<()> {
        // f64 gives 390070427.49999994; the decimal is an exact tie.
        assert_eq!(coord("lat=\"39.00704275\"", "lat")?, 390_070_428);
        assert_eq!(coord("lat=\"-39.00704275\"", "lat")?, -390_070_428);
        assert_eq!(coord("lat=\"39.007042749999\"", "lat")?, 390_070_427);
        assert_eq!(coord("lat=\"&#51;9.00704275\"", "lat")?, 390_070_428);
        assert_eq!(coord("lon=\"12.3456789\"", "lon")?, 123_456_789);
        assert_eq!(coord("lon=\"0.000000001e2\"", "lon")?, 1);
        assert_eq!(coord("lon=\"-0.00000005\"", "lon")?, -1);
        // Long tokens are read exactly, including against the f64 exponent cap.
        let long = format!("lat=\"0.{}1e5\"", "0".repeat(40));
        assert_eq!(coord(&long, "lat")?, 0);
        let long = format!("lat=\"90.{}1\"", "0".repeat(40));
        assert_eq!(coord(&long, "lat")?, 900_000_000);
        let smuggled = format!("lat=\"0.{}1e1000000\"", "0".repeat(999_999));
        assert!(coord(&smuggled, "lat").is_ok_and(|v| v == 10_000_000));
        let smuggled = format!("lat=\"0.{}1e1000002\"", "0".repeat(999_999));
        assert!(coord(&smuggled, "lat").is_err());
        Ok(())
    }

    #[test]
    fn exact_decimal_rounds_half_away_from_zero() {
        assert_eq!(exact_decimal_e7("1.00000005"), Some(10_000_001));
        assert_eq!(exact_decimal_e7("1.00000004999"), Some(10_000_000));
        assert_eq!(exact_decimal_e7("+.5e-7"), Some(1));
        assert_eq!(exact_decimal_e7("180."), Some(1_800_000_000));
        assert_eq!(exact_decimal_e7("1e5"), None);
        assert_eq!(exact_decimal_e7("inf"), None);
        assert_eq!(exact_decimal_e7("1e-99999999999999999999"), Some(0));
    }

    #[test]
    fn owned_sinks_pass_metadata_without_native_validation() -> crate::Result<()> {
        for attributes in [
            "timestamp=\"yesterday\"",
            "changeset=\"no\"",
            "uid=\"2147483648\"",
            "visible=\"False\"",
        ] {
            let element =
                quick_xml::events::BytesStart::from_content(format!("node {attributes}"), 4);
            assert!(parse_owned_metadata(&element)?.is_some(), "{attributes}");
        }
        let element = quick_xml::events::BytesStart::from_content("node version=\"no\"", 4);
        assert!(parse_owned_metadata(&element).is_err());
        assert!(arena_meta(&element, &mut CompactDiffOverlay::new()).is_err());
        let element = quick_xml::events::BytesStart::from_content(
            "node uid=\"&#48;\" visible=\"False\" timestamp=\"a\tb\"",
            4,
        );
        let owned =
            parse_owned_metadata(&element)?.ok_or_else(|| Error::internal("metadata missing"))?;
        assert_eq!(owned.uid, "0");
        assert_eq!(owned.timestamp, "a b");
        let visible = quick_xml::events::BytesStart::from_content("node visible=\"False\"", 4);
        assert_eq!(
            arena_meta(&visible, &mut CompactDiffOverlay::new())?.flags & META_FLAG_HIDDEN,
            0
        );
        Ok(())
    }

    #[test]
    fn absent_role_is_none_and_malformed_role_is_refused() -> crate::Result<()> {
        let absent = quick_xml::events::BytesStart::new("member");
        let malformed = quick_xml::events::BytesStart::from_content("member role=broken", 6);
        assert_eq!(parse_str_attr_optional(&absent, "role")?, None);
        let err = parse_str_attr_optional(&malformed, "role")
            .err()
            .ok_or_else(|| Error::internal("malformed role accepted"))?;
        assert!(err.to_string().contains("member"), "{err}");
        assert!(parse_str_attr(&malformed, "role").is_err());
        let unknown = quick_xml::events::BytesStart::from_content(
            "member type=\"unknown\" ref=\"1\" role=\"r\"",
            6,
        );
        assert!(parse_member(&unknown).is_err());
        Ok(())
    }

    #[test]
    fn failed_recreation_keeps_delete_and_success_clears_it() -> crate::Result<()> {
        let mut overlay = CompactDiffOverlay::new();
        let deletion =
            quick_xml::events::BytesStart::from_content("node id=\"7\" version=\"5\"", 4);
        CompactSink::new(&mut overlay, HiddenUpserts::Keep).begin_element(
            OscAction::Delete,
            ElementKind::Node,
            &deletion,
        )?;
        let stale = quick_xml::events::BytesStart::from_content(
            "node id=\"7\" version=\"4\" lat=\"1\" lon=\"2\"",
            4,
        );
        let mut sink = CompactSink::new(&mut overlay, HiddenUpserts::Keep);
        sink.begin_element(OscAction::Create, ElementKind::Node, &stale)?;
        assert!(sink.end_element().is_err());
        assert!(overlay.is_deleted(ElementKind::Node, 7));
        assert!(!overlay.has_node(7));
        let current = quick_xml::events::BytesStart::from_content(
            "node id=\"7\" version=\"6\" lat=\"1\" lon=\"2\"",
            4,
        );
        let mut sink = CompactSink::new(&mut overlay, HiddenUpserts::Keep);
        sink.begin_element(OscAction::Create, ElementKind::Node, &current)?;
        sink.end_element()?;
        assert!(!overlay.is_deleted(ElementKind::Node, 7));
        assert!(overlay.has_node(7));
        Ok(())
    }

    #[test]
    fn a_hidden_upsert_is_a_delete_only_for_apply() -> crate::Result<()> {
        // No coordinates: a hidden node recorded as a delete does not need them.
        let hidden = quick_xml::events::BytesStart::from_content(
            "node id=\"7\" version=\"5\" visible=\"false\"",
            4,
        );
        let tag = quick_xml::events::BytesStart::from_content("tag k=\"a\" v=\"b\"", 3);
        let mut overlay = CompactDiffOverlay::new();
        let mut sink = CompactSink::new(&mut overlay, HiddenUpserts::AsDelete);
        sink.begin_element(OscAction::Modify, ElementKind::Node, &hidden)?;
        sink.tag(&tag)?;
        sink.end_element()?;
        assert!(overlay.is_deleted(ElementKind::Node, 7));
        assert!(!overlay.has_node(7));

        let mut overlay = CompactDiffOverlay::new();
        let mut sink = CompactSink::new(&mut overlay, HiddenUpserts::Keep);
        assert!(
            sink.begin_element(OscAction::Modify, ElementKind::Node, &hidden)
                .is_err(),
            "the public overlay still stages a hidden node, which needs coordinates"
        );
        Ok(())
    }
}
