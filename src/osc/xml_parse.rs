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
//! [`drive_osc`] owns the *framing*: gzip sniffing, section tracking,
//! element nesting, self-closing elements, child routing, and the
//! end-of-document completeness check. An [`OscSink`] owns the *data*:
//! what an element turns into once the framing has identified it.

use std::cell::Cell;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek};
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use flate2::read::MultiGzDecoder;
use quick_xml::Reader;
use quick_xml::events::Event;
use quick_xml::events::attributes::Attribute;

use crate::read::elements::MemberType;

use super::ParseResult;
use super::compact::{
    ArenaMeta, META_FLAG_HAS_USER, META_FLAG_HAS_VERSION, META_FLAG_HIDDEN, META_FLAG_PRESENT,
    member_type_to_byte,
};
use super::parse::CompactDiffOverlay;

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

/// Which OSM object kind an element describes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ElemKind {
    Node,
    Way,
    Relation,
}

impl ElemKind {
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Way => "way",
            Self::Relation => "relation",
        }
    }
}

/// Where a driven OSC document's elements go.
///
/// The driver guarantees the call sequence: exactly one `begin_element`
/// per element, followed by zero or more `tag` / `nd` / `member` calls
/// (already filtered to the kinds that can carry them), followed by
/// exactly one `end_element`. A self-closing element gets `begin_element`
/// immediately followed by `end_element`.
pub(crate) trait OscSink {
    fn begin_element(
        &mut self,
        action: OscAction,
        kind: ElemKind,
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

pub(crate) fn parse_i64_attr(
    e: &quick_xml::events::BytesStart<'_>,
    name: &str,
) -> ParseResult<i64> {
    for attr_result in e.attributes() {
        let attr = attr_result?;
        if attr.key.as_ref() == name {
            return Ok(attr.value.parse::<i64>()?);
        }
    }
    Err(format!("missing attribute '{name}'").into())
}

fn parse_f64_attr(e: &quick_xml::events::BytesStart<'_>, name: &str) -> ParseResult<f64> {
    for attr_result in e.attributes() {
        let attr = attr_result?;
        if attr.key.as_ref() == name {
            return Ok(attr.value.parse::<f64>()?);
        }
    }
    Err(format!("missing attribute '{name}'").into())
}

pub(crate) fn parse_str_attr(
    e: &quick_xml::events::BytesStart<'_>,
    name: &str,
) -> ParseResult<String> {
    for attr_result in e.attributes() {
        let attr = attr_result?;
        if attr.key.as_ref() == name {
            let val = attr.normalized_value(quick_xml::XmlVersion::Implicit1_0)?;
            return Ok(val.into_owned());
        }
    }
    Err(format!("missing attribute '{name}'").into())
}

// Deliberately treat malformed optional roles as absent. Refusing them would
// change the accepted OSC input contract; all sinks retain their old fallback.
pub(crate) fn parse_str_attr_optional(
    e: &quick_xml::events::BytesStart<'_>,
    name: &str,
) -> Option<String> {
    for attr in e.attributes().flatten() {
        if attr.key.as_ref() == name {
            return attr
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(std::borrow::Cow::into_owned);
        }
    }
    None
}

/// Convert an OSC XML member type string to the crate's `MemberType`.
pub(crate) fn parse_member_type(s: &str) -> ParseResult<MemberType> {
    match s {
        "node" => Ok(MemberType::Node),
        "way" => Ok(MemberType::Way),
        "relation" => Ok(MemberType::Relation),
        other => Err(format!("unknown relation member type: '{other}'").into()),
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
        let attr = attr_result?;
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
    T::Err: std::fmt::Display,
{
    let text = attr.value.as_ref();
    text.parse::<T>()
        .map_err(|err| -> Box<dyn std::error::Error> {
            format!(
                "OSC element metadata attribute '{}' has unparsable value '{text}' ({err}); \
             refusing to record it as 0, because an absent or broken value is not the \
             value zero and the version-regression gate compares these numbers",
                field.name()
            )
            .into()
        })
}

// ---------------------------------------------------------------------------
// Input framing: gzip sniff
// ---------------------------------------------------------------------------

/// Wraps an `io::Read` and accumulates wall time spent in `read()` calls,
/// so the gzip-decompress portion of a parse can be attributed separately
/// via a sidecar counter.
struct TimedRead<R: io::Read> {
    inner: R,
    total: Rc<Cell<Duration>>,
}

impl<R: io::Read> io::Read for TimedRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let start = Instant::now();
        let result = self.inner.read(buf);
        self.total.set(self.total.get() + start.elapsed());
        result
    }
}

/// Open an OSC file, transparently handling both gzip-compressed and
/// plain XML.
///
/// `apply-changes` used to wrap every input in `MultiGzDecoder`
/// unconditionally and fail on a plain `.osc` with a raw gzip error,
/// while `merge-changes` sniffed the magic and handled both. The sniff
/// belongs to the shared parser, not to one of its two callers.
///
/// Returns the reader and the shared cell accumulating decompress time.
pub(crate) type OscReader = Reader<BufReader<Box<dyn io::Read>>>;

pub(crate) fn open_osc_reader(path: &Path) -> ParseResult<(OscReader, Rc<Cell<Duration>>)> {
    let mut file = File::open(path)?;
    // A file shorter than two bytes cannot be gzip and cannot be XML
    // either; read what is there and let the XML parser produce the
    // complaint, rather than failing here with an EOF error that names
    // nothing.
    let mut magic = [0u8; 2];
    let mut got = 0usize;
    while got < 2 {
        match file.read(&mut magic[got..])? {
            0 => break,
            n => got += n,
        }
    }
    file.seek(io::SeekFrom::Start(0))?;

    let total = Rc::new(Cell::new(Duration::ZERO));
    let inner: Box<dyn io::Read> = if got == 2 && magic == [0x1f, 0x8b] {
        Box::new(TimedRead {
            inner: MultiGzDecoder::new(file),
            total: Rc::clone(&total),
        })
    } else {
        Box::new(TimedRead {
            inner: file,
            total: Rc::clone(&total),
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

fn kind_for(name: &str) -> Option<ElemKind> {
    match name {
        "node" => Some(ElemKind::Node),
        "way" => Some(ElemKind::Way),
        "relation" => Some(ElemKind::Relation),
        _ => None,
    }
}

struct DriveState {
    section: Option<OscAction>,
    open: Option<ElemKind>,
    saw_root_end: bool,
}

/// The root element name, whose close proves the document is whole.
const ROOT: &str = "osmChange";

/// Route a `tag` / `nd` / `member` child to the sink, if one can carry it.
fn route_child<S: OscSink>(
    name: &str,
    e: &quick_xml::events::BytesStart<'_>,
    state: &DriveState,
    sink: &mut S,
) -> ParseResult<()> {
    let Some(kind) = state.open else {
        return Ok(());
    };
    match name {
        "tag" => sink.tag(e),
        "nd" if kind == ElemKind::Way => sink.nd(e),
        "member" if kind == ElemKind::Relation => sink.member(e),
        _ => Ok(()),
    }
}

fn handle_open<S: OscSink>(
    e: &quick_xml::events::BytesStart<'_>,
    is_empty: bool,
    state: &mut DriveState,
    sink: &mut S,
) -> ParseResult<()> {
    let name = e.name();
    let name = name.as_ref();

    // `<osmChange version="0.6"/>` is a complete, valid, empty change
    // document - it opens and closes in one event, so it never produces
    // an `End` and the completeness check below would otherwise call it
    // truncated.
    if is_empty && name == ROOT {
        state.saw_root_end = true;
        return Ok(());
    }

    if let Some(action) = action_for(name) {
        // A self-closing `<create/>` opens and closes in one event.
        state.section = if is_empty { None } else { Some(action) };
        return Ok(());
    }

    if let Some(kind) = kind_for(name) {
        // An element outside a section is malformed OSC. The compact
        // parser used to treat it as a create and put it in the overlay;
        // merge-changes errored for self-closing elements and silently
        // dropped non-self-closing ones. All three answers were
        // different, and only refusing is defensible - we cannot know
        // whether the producer meant to create, modify or delete it.
        let action = state.section.ok_or_else(|| -> Box<dyn std::error::Error> {
            format!(
                "OSC {} {} appears outside any <create>, <modify> or <delete> section; \
                 refusing to guess which change it was meant to be",
                kind.tag(),
                parse_i64_attr(e, "id").map_or_else(|_| "(no id)".to_string(), |id| id.to_string())
            )
            .into()
        })?;
        sink.begin_element(action, kind, e)?;
        if is_empty {
            sink.end_element()?;
        } else {
            state.open = Some(kind);
        }
        return Ok(());
    }

    route_child(name, e, state, sink)
}

/// Drive one OSC document from `reader` into `sink`.
pub(crate) fn drive_osc<R: io::BufRead, S: OscSink>(
    reader: &mut Reader<R>,
    sink: &mut S,
) -> ParseResult<()> {
    let mut state = DriveState {
        section: None,
        open: None,
        saw_root_end: false,
    };
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => handle_open(e, false, &mut state, sink)?,
            Ok(Event::Empty(ref e)) => handle_open(e, true, &mut state, sink)?,
            Ok(Event::End(ref e)) => {
                let name = e.name();
                let name = name.as_ref();
                if action_for(name).is_some() {
                    state.section = None;
                } else if kind_for(name).is_some() && state.open.is_some() {
                    sink.end_element()?;
                    state.open = None;
                } else if name == ROOT {
                    state.saw_root_end = true;
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(Box::new(e)),
        }
        buf.clear();
    }

    // A `.osc.gz` truncated at a gzip member boundary decompresses
    // cleanly, quick-xml reports `Eof`, and every caller happily treated
    // the partial document as the whole change stream - apply-changes
    // wrote a partially-applied PBF and reported success. Only the
    // closing tag proves the document is whole.
    if !state.saw_root_end {
        return Err(
            "OSC input ended without a closing </osmChange> tag: the file is \
                    truncated (a .osc.gz cut at a gzip member boundary decompresses \
                    cleanly, so this is the only way to notice). Applying it would \
                    produce a partially-applied result reported as success."
                .into(),
        );
    }

    Ok(())
}

/// Open `path`, drive it into `sink`, and emit the decompress counter.
pub(crate) fn parse_osc_path<S: OscSink>(path: &Path, sink: &mut S) -> ParseResult<()> {
    let (mut reader, decompress_total) = open_osc_reader(path)?;
    let result = drive_osc(&mut reader, sink);
    // Deliberately emit this historical name once per file, without accumulation,
    // from every sink. Changing attribution or totals breaks brokkr run comparisons.
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    crate::debug::emit_counter(
        "merge_changes_decompress_ns",
        decompress_total.get().as_nanos() as i64,
    );
    result
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
    let degrees = parse_f64_attr(e, name).map_err(|err| -> Box<dyn std::error::Error> {
        format!(
            "OSC node {id}: {name} attribute is missing or unparsable ({err}). A created or \
             modified node must carry usable coordinates; storing it at 0/0 would silently \
             move it to the Gulf of Guinea."
        )
        .into()
    })?;
    // Two independent checks. The geographic one comes first because it is
    // the one a caller can reason about: lat="100" is not a place, and an
    // i32 happily holds 1_000_000_000 decimicrodegrees, so the
    // representation check below would wave it through. Such a node would
    // enter the overlay as an ordinary node and could propagate into
    // LocationsOnWays, where nothing downstream re-validates it.
    let limit = if name == "lat" { 90.0_f64 } else { 180.0_f64 };
    if !(degrees.is_finite() && degrees.abs() <= limit) {
        return Err(format!(
            "OSC node {id}: {name} value {degrees} is outside the valid geographic range \
             -{limit} to {limit}"
        )
        .into());
    }
    let scaled = degrees * 1e7;
    if !scaled.is_finite() || scaled < f64::from(i32::MIN) || scaled > f64::from(i32::MAX) {
        return Err(format!(
            "OSC node {id}: {name} value {degrees} is outside the range representable in \
             decimicrodegrees"
        )
        .into());
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
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)?
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
                meta.timestamp = crate::commands::parse_rfc3339_utc(text).map_err(
                    |err| -> Box<dyn std::error::Error> {
                        format!(
                            "OSC element metadata attribute 'timestamp' has unparsable \
                             value '{text}' ({err}); refusing to record it as the epoch"
                        )
                        .into()
                    },
                )?;
            }
            MetaField::Changeset => meta.changeset = meta_int::<i64>(field, attr)?,
            MetaField::Uid => meta.uid = meta_int::<i32>(field, attr)?,
            MetaField::User => {
                user = Some(
                    attr.normalized_value(quick_xml::XmlVersion::Implicit1_0)?
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
    kind: ElemKind,
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
        kind: ElemKind,
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
                ElemKind::Node => self.overlay.delete_node(id, &meta)?,
                ElemKind::Way => self.overlay.delete_way(id, &meta)?,
                ElemKind::Relation => self.overlay.delete_relation(id, &meta)?,
            }
            self.staging = None;
            return Ok(());
        }

        // Deliberately leave the delete pending until insertion succeeds. Clearing it
        // here would let a rejected stale re-create erase the accepted deletion.
        let meta = arena_meta(e, self.overlay)?;
        let (lat, lon) = if kind == ElemKind::Node {
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
            ElemKind::Node => self
                .overlay
                .push_node(st.id, st.lat, st.lon, &tags, &st.meta),
            ElemKind::Way => self.overlay.push_way(st.id, &st.refs, &tags, &st.meta),
            ElemKind::Relation => self
                .overlay
                .push_relation(st.id, &st.members, &tags, &st.meta),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owned_sinks_pass_metadata_without_native_validation() -> crate::BoxResult<()> {
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
        let owned = parse_owned_metadata(&element)?.ok_or("metadata missing")?;
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
    fn absent_and_malformed_roles_both_default_to_empty() {
        let absent = quick_xml::events::BytesStart::new("member");
        let malformed = quick_xml::events::BytesStart::from_content("member role=broken", 6);
        assert_eq!(
            parse_str_attr_optional(&absent, "role").unwrap_or_default(),
            ""
        );
        assert_eq!(
            parse_str_attr_optional(&malformed, "role").unwrap_or_default(),
            ""
        );
        assert_eq!(parse_str_attr(&malformed, "role").unwrap_or_default(), "");
        assert!(parse_member_type("unknown").is_err());
    }

    #[test]
    fn failed_recreation_keeps_delete_and_success_clears_it() -> crate::BoxResult<()> {
        use crate::osc::OscElementKind;
        let mut overlay = CompactDiffOverlay::new();
        let deletion =
            quick_xml::events::BytesStart::from_content("node id=\"7\" version=\"5\"", 4);
        CompactSink::new(&mut overlay).begin_element(
            OscAction::Delete,
            ElemKind::Node,
            &deletion,
        )?;
        let stale = quick_xml::events::BytesStart::from_content(
            "node id=\"7\" version=\"4\" lat=\"1\" lon=\"2\"",
            4,
        );
        let mut sink = CompactSink::new(&mut overlay);
        sink.begin_element(OscAction::Create, ElemKind::Node, &stale)?;
        assert!(sink.end_element().is_err());
        assert!(overlay.is_deleted(OscElementKind::Node, 7));
        assert!(!overlay.has_node(7));
        let current = quick_xml::events::BytesStart::from_content(
            "node id=\"7\" version=\"6\" lat=\"1\" lon=\"2\"",
            4,
        );
        let mut sink = CompactSink::new(&mut overlay);
        sink.begin_element(OscAction::Create, ElemKind::Node, &current)?;
        sink.end_element()?;
        assert!(!overlay.is_deleted(OscElementKind::Node, 7));
        assert!(overlay.has_node(7));
        Ok(())
    }
}
