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
//! ignored wherever they appear; this is a hierarchy check, not XML
//! validation beyond what quick-xml does. An [`OscSink`] owns the *data*:
//! what an element turns into once the framing has identified it.

use std::cell::Cell;
use std::io::{self, BufReader, Read, Seek};
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
    ArenaMeta, META_FLAG_HAS_USER, META_FLAG_HAS_VERSION, META_FLAG_HIDDEN, META_FLAG_PRESENT,
    member_type_to_byte,
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

/// Where a driven OSC document's elements go.
///
/// The driver guarantees the call sequence: exactly one `begin_element`
/// per element, followed by zero or more `tag` / `nd` / `member` calls
/// (already filtered to the kinds that can carry them), followed by
/// exactly one `end_element`. A self-closing element gets `begin_element`
/// immediately followed by `end_element`. The sequence is balanced on
/// success; when the driver fails (a misplaced element, truncation, a sink
/// error) the last element may be left begun and never ended.
pub(crate) trait OscSink {
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

pub(crate) fn parse_i64_attr(
    e: &quick_xml::events::BytesStart<'_>,
    name: &str,
) -> ParseResult<i64> {
    for attr_result in e.attributes() {
        let attr = attr_result.map_err(xml_err)?;
        if attr.key.as_ref() == name {
            return attr.value.parse::<i64>().map_err(|err| {
                Error::invalid_data_with(format!("attribute '{name}' is not a valid integer"), err)
            });
        }
    }
    Err(missing_attr(name))
}

fn parse_f64_attr(e: &quick_xml::events::BytesStart<'_>, name: &str) -> ParseResult<f64> {
    for attr_result in e.attributes() {
        let attr = attr_result.map_err(xml_err)?;
        if attr.key.as_ref() == name {
            return attr.value.parse::<f64>().map_err(|err| {
                Error::invalid_data_with(format!("attribute '{name}' is not a valid number"), err)
            });
        }
    }
    Err(missing_attr(name))
}

pub(crate) fn parse_str_attr(
    e: &quick_xml::events::BytesStart<'_>,
    name: &str,
) -> ParseResult<String> {
    for attr_result in e.attributes() {
        let attr = attr_result.map_err(xml_err)?;
        if attr.key.as_ref() == name {
            let val = attr
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map_err(xml_err)?;
            return Ok(val.into_owned());
        }
    }
    Err(missing_attr(name))
}

// A malformed attribute is an error even when the attribute is optional.
// Until 2026-10-26 a malformed optional `role` read as absent and an empty
// role was written; that silently altered the data. The accepted OSC contract
// changed (maintainer ruling, review audit) to match the PBF re-encoding
// paths, the OSC arena and libosmium, whose expat parser rejects malformed XML
// outright. Absent stays `Ok(None)`; only a malformed attribute is refused.
pub(crate) fn parse_str_attr_optional(
    e: &quick_xml::events::BytesStart<'_>,
    name: &str,
) -> ParseResult<Option<String>> {
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
            return Ok(Some(val.into_owned()));
        }
    }
    Ok(None)
}

/// Convert an OSC XML member type string to the crate's `MemberType`.
pub(crate) fn parse_member_type(s: &str) -> ParseResult<MemberType> {
    match ElementKind::from_name(s) {
        Some(ElementKind::Node) => Ok(MemberType::Node),
        Some(ElementKind::Way) => Ok(MemberType::Way),
        Some(ElementKind::Relation) => Ok(MemberType::Relation),
        None => Err(Error::invalid_data(format!(
            "unknown relation member type: '{s}'"
        ))),
    }
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
    let text = attr.value.as_ref();
    text.parse::<T>().map_err(|err| {
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
    // A file shorter than two bytes cannot be gzip and cannot be XML
    // either; read what is there and let the XML parser produce the
    // complaint, rather than failing here with an EOF error that names
    // nothing.
    let read_error =
        |e: io::Error| Error::from(e).context(format!("failed to read {}", path.display()));
    let mut magic = [0u8; 2];
    let mut got = 0usize;
    while got < 2 {
        match file.read(&mut magic[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(read_error(e)),
        }
    }
    file.seek(io::SeekFrom::Start(0)).map_err(read_error)?;

    let total = Rc::new(Cell::new(0_u64));
    let inner: Box<dyn io::Read> = if got == 2 && magic == [0x1f, 0x8b] {
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

/// Route an object's child (`level == Element`) to the sink, or refuse it.
fn handle_element_child<S: OscSink>(
    name: &str,
    e: &quick_xml::events::BytesStart<'_>,
    kind: ElementKind,
    sink: &mut S,
) -> ParseResult<()> {
    match (name, kind) {
        ("tag", _) => sink.tag(e),
        ("nd", ElementKind::Way) => sink.nd(e),
        ("member", ElementKind::Relation) => sink.member(e),
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
            handle_element_child(name, e, state.open, sink).map_err(|e| input_fault(path, e))?;
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
        match reader.read_event_into(&mut buf) {
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
    pub(crate) fn replay<S: OscSink>(&self, sink: &mut S, path: &Path) -> ParseResult<()> {
        for ev in &self.events {
            let e = quick_xml::events::BytesStart::from_content(
                &self.text[ev.start..ev.end],
                ev.name_len,
            );
            match ev.op {
                LexOp::Begin(action, kind) => sink.begin_element(action, kind, &e),
                LexOp::Tag => sink.tag(&e),
                LexOp::Nd => sink.nd(&e),
                LexOp::Member => sink.member(&e),
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
    let degrees = parse_f64_attr(e, name).map_err(|err| {
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
    };
    let present = scan_meta_attrs(e, |field, attr| {
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
            .map_err(xml_err)?
            .into_owned();
        match field {
            MetaField::Timestamp => meta.timestamp = text,
            MetaField::Changeset => meta.changeset = text,
            MetaField::Uid => meta.uid = text,
            MetaField::User => meta.user = text,
            MetaField::Visible => meta.visible = text,
            MetaField::Version => unreachable!("handled above"),
        }
        Ok(())
    })?;
    Ok(if present { Some(meta) } else { None })
}

/// Read an element's metadata into the arena representation.
// Deliberately keep the overlay's native numeric parsing and false-only visible
// test. Sharing owned-sink validation would change accepted input and its cost.
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
                let text = attr.value.as_ref();
                meta.timestamp = crate::commands::parse_rfc3339_utc(text).map_err(|err| {
                    Error::invalid_data_with(
                        format!(
                            "OSC element metadata attribute 'timestamp' has unparsable \
                             value '{text}'; refusing to record it as the epoch"
                        ),
                        err,
                    )
                })?;
            }
            MetaField::Changeset => meta.changeset = meta_int::<i64>(field, attr)?,
            MetaField::Uid => meta.uid = meta_int::<i32>(field, attr)?,
            MetaField::User => {
                user = Some(
                    attr.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map_err(xml_err)?
                        .into_owned(),
                );
            }
            MetaField::Visible => {
                if attr.value.as_ref() == "false" {
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

/// Sink that appends into a [`CompactDiffOverlay`].
pub(crate) struct CompactSink<'a> {
    overlay: &'a mut CompactDiffOverlay,
    /// `None` while no create/modify element is being staged - which is
    /// also the state during a `<delete>` element, whose children carry
    /// nothing the overlay needs.
    staging: Option<Staging>,
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
    pub(crate) fn new(overlay: &'a mut CompactDiffOverlay) -> Self {
        Self {
            overlay,
            staging: None,
        }
    }
}

impl OscSink for CompactSink<'_> {
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
            let meta = arena_meta(e, self.overlay)?;
            match kind {
                ElementKind::Node => self.overlay.delete_node(id, &meta)?,
                ElementKind::Way => self.overlay.delete_way(id, &meta)?,
                ElementKind::Relation => self.overlay.delete_relation(id, &meta)?,
            }
            self.staging = None;
            return Ok(());
        }

        // Deliberately leave the delete pending until insertion succeeds. Clearing it
        // here would let a rejected stale re-create erase the accepted deletion.
        let meta = arena_meta(e, self.overlay)?;
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
        if self.staging.is_none() {
            return Ok(());
        }
        let k = parse_str_attr(e, "k")?;
        let v = parse_str_attr(e, "v")?;
        let key_id = self.overlay.intern(&k)?;
        if let Some(st) = self.staging.as_mut() {
            st.tag_keys.push(key_id);
            st.tag_values.push(v);
        }
        Ok(())
    }

    fn nd(&mut self, e: &quick_xml::events::BytesStart<'_>) -> ParseResult<()> {
        let ref_id = parse_i64_attr(e, "ref")?;
        if let Some(st) = self.staging.as_mut() {
            st.refs.push(ref_id);
        }
        Ok(())
    }

    fn member(&mut self, e: &quick_xml::events::BytesStart<'_>) -> ParseResult<()> {
        if self.staging.is_none() {
            return Ok(());
        }
        let member_type = parse_member_type(&parse_str_attr(e, "type")?)?;
        let ref_id = parse_i64_attr(e, "ref")?;
        // Deliberately replace an unreadable role with empty text, as the owned
        // sinks do. Propagating this error would refuse previously accepted OSC.
        let role = parse_str_attr(e, "role").unwrap_or_default();
        let type_byte = member_type_to_byte(member_type)?;
        let role_id = self.overlay.intern(&role)?;
        if let Some(st) = self.staging.as_mut() {
            st.members.push((ref_id, type_byte, role_id));
        }
        Ok(())
    }

    fn end_element(&mut self) -> ParseResult<()> {
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
        assert!(parse_member_type("unknown").is_err());
        Ok(())
    }

    #[test]
    fn failed_recreation_keeps_delete_and_success_clears_it() -> crate::Result<()> {
        let mut overlay = CompactDiffOverlay::new();
        let deletion =
            quick_xml::events::BytesStart::from_content("node id=\"7\" version=\"5\"", 4);
        CompactSink::new(&mut overlay).begin_element(
            OscAction::Delete,
            ElementKind::Node,
            &deletion,
        )?;
        let stale = quick_xml::events::BytesStart::from_content(
            "node id=\"7\" version=\"4\" lat=\"1\" lon=\"2\"",
            4,
        );
        let mut sink = CompactSink::new(&mut overlay);
        sink.begin_element(OscAction::Create, ElementKind::Node, &stale)?;
        assert!(sink.end_element().is_err());
        assert!(overlay.is_deleted(ElementKind::Node, 7));
        assert!(!overlay.has_node(7));
        let current = quick_xml::events::BytesStart::from_content(
            "node id=\"7\" version=\"6\" lat=\"1\" lon=\"2\"",
            4,
        );
        let mut sink = CompactSink::new(&mut overlay);
        sink.begin_element(OscAction::Create, ElementKind::Node, &current)?;
        sink.end_element()?;
        assert!(!overlay.is_deleted(ElementKind::Node, 7));
        assert!(overlay.has_node(7));
        Ok(())
    }
}
