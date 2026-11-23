//! XML-oriented owned element types for derive_changes, diff, merge_changes, and tags_filter_osc.
//!
//! Metadata fields are String-typed for direct XML attribute output.
//! See `crate::owned` for the PBF-oriented variant with native types.

// This module owns OSC element XML, not whole-document framing or transport.
// Every producer picks plain or gzip from the output name by the one rule in
// `crate::osc::output_is_gzip`; each keeps its own document framing, because
// their gzip member layouts differ (single stream, per-input members, ordered
// parallel chunks).

use quick_xml::Writer;
use quick_xml::events::{BytesEnd, BytesStart, Event};

use crate::coord_fmt::{format_coord, from_decimicro};
use crate::{ElementKind, MemberId};

/// Check that a user-text attribute value can be written as XML 1.0, and
/// say whether it holds a tab, newline or carriage return that must be
/// written as a character reference.
///
/// XML 1.0's `Char` production admits tab, LF, CR, U+0020-U+D7FF,
/// U+E000-U+FFFD and U+10000-U+10FFFF. A `str` holds no surrogate, so what
/// it can hold beyond that is the other C0 controls (NUL, U+0001 ...) and
/// U+FFFE / U+FFFF. Those cannot appear in an XML 1.0 document at all, not
/// even as character references, so a conforming reader (libosmium's Expat
/// among them) refuses the whole file. PBF strings are validated as UTF-8
/// only, so a PBF tag can carry one here. OSC input cannot: the shared
/// reader (`check_event_chars` in `osc/xml_parse.rs`) refuses the document
/// first, so for the re-emitters this is a second line, not the boundary.
/// Writing it would publish a file that is not XML; dropping or replacing
/// it would be a silent wrong answer. It is refused.
///
/// One pass over the bytes, replacing the whitespace scan the escape needed
/// anyway: a byte below 0x20 is an ASCII scalar, and in valid UTF-8 `EF BF
/// BE` / `EF BF BF` are exactly U+FFFE / U+FFFF. The scan continues past a
/// tab, LF or CR, since a forbidden scalar can follow one.
fn check_xml_chars(key: &str, value: &str) -> crate::Result<bool> {
    let bytes = value.as_bytes();
    // Fast path: almost every value has no byte below 0x20 and no 0xEF lead
    // byte, and this plain scan costs what the old whitespace-only scan did.
    // The exact scan below runs only when one is present.
    if !bytes.iter().any(|&b| b < 0x20 || b == 0xEF) {
        return Ok(false);
    }
    let mut needs_refs = false;
    for (i, &b) in bytes.iter().enumerate() {
        let forbidden = match b {
            b'\t' | b'\n' | b'\r' => {
                needs_refs = true;
                None
            }
            0..0x20 => Some(u32::from(b)),
            0xEF if bytes.get(i + 1) == Some(&0xBF) => match bytes.get(i + 2) {
                Some(&0xBE) => Some(0xFFFE),
                Some(&0xBF) => Some(0xFFFF),
                _ => None,
            },
            _ => None,
        };
        if let Some(cp) = forbidden {
            return Err(crate::Error::invalid_data(format!(
                "attribute {key} holds U+{cp:04X}, which XML 1.0 cannot carry even as a \
                 character reference; OSC output refuses it rather than write a file \
                 XML readers reject"
            )));
        }
    }
    Ok(needs_refs)
}

/// Name the element an attribute refusal belongs to, as `node 42`.
pub(crate) fn in_element(kind: ElementKind, id: i64) -> impl FnOnce(crate::Error) -> crate::Error {
    move |err| err.context(format!("{} {id}", kind.as_str()))
}

/// Push an attribute whose value is OSM user data (tag key/value, member
/// role, user name) and may therefore contain control characters.
///
/// XML 1.0 attribute-value normalization (spec section 3.3.3) replaces raw
/// tab, newline, and carriage-return characters in attribute values with
/// spaces at parse time, so a literal newline inside e.g. a multi-line
/// `inscription` tag value does not survive a write -> parse roundtrip.
/// Character references (`&#10;` etc.) are the only spec-conforming way to
/// preserve them; osmium's OSC writer does the same (`&#xA;`). quick-xml
/// 0.42's tuple `push_attribute` escapes through `escape_attribute`, which
/// writes the five markup characters and `&#9;` / `&#10;` / `&#13;`, the
/// same bytes [`append_attr_escaped`] writes. A character XML cannot carry
/// is refused ([`check_xml_chars`]).
fn push_attribute_escaped<'a>(
    elem: &mut BytesStart<'a>,
    key: &'a str,
    value: &str,
) -> crate::Result<()> {
    check_xml_chars(key, value)?;
    elem.push_attribute((key, value));
    Ok(())
}

/// Append ` key="value"` to a start tag's content, escaped as
/// `BytesStart::push_attribute((key, value))` escapes it.
pub(crate) fn append_attr(content: &mut String, key: &str, value: &str) {
    content.push(' ');
    content.push_str(key);
    content.push_str("=\"");
    content.push_str(&quick_xml::escape::escape(value));
    content.push('"');
}

/// Append ` key="n"`: an integer needs no escaping, and `to_string`'s
/// spelling is the writers'.
pub(crate) fn append_int_attr(content: &mut String, key: &str, n: i64) {
    use std::fmt::Write as _;
    content.push(' ');
    content.push_str(key);
    content.push_str("=\"");
    write!(content, "{n}").ok(); // ok-deliberate: writing to a String cannot fail
    content.push('"');
}

/// Append ` key="value"` as [`push_attribute_escaped`] writes it: escaped,
/// with newline, carriage return and tab as character references, and a
/// character XML cannot carry refused ([`check_xml_chars`]). This is the
/// OSC re-emitters' one user-text boundary: tags, roles and every metadata
/// text field.
pub(crate) fn append_attr_escaped(
    content: &mut String,
    key: &str,
    value: &str,
) -> crate::Result<()> {
    if !check_xml_chars(key, value)? {
        append_attr(content, key, value);
        return Ok(());
    }
    content.push(' ');
    content.push_str(key);
    content.push_str("=\"");
    for ch in quick_xml::escape::escape(value).chars() {
        match ch {
            '\n' => content.push_str("&#10;"),
            '\r' => content.push_str("&#13;"),
            '\t' => content.push_str("&#9;"),
            _ => content.push(ch),
        }
    }
    content.push('"');
    Ok(())
}

// ---------------------------------------------------------------------------
// Owned and borrowed element writers deliberately retain separate metadata
// adapters: owned attributes preserve lexical timestamp/number/visible strings,
// while borrowed Info uses typed, independently optional fields and DenseInfo
// reads presence from its values (`DenseNodeInfo::claims`). The owned writers
// print an OSC element's version as given, because OSC can spell absence and
// an explicit `version="0"` there was claimed by its input; the borrowed
// writers print PBF metadata, where 0 is how writers spell "no version", so
// they print only a claimed version. Every PBF element derive prints goes
// through the borrowed writers, whatever path found it; owned metadata
// built from a PBF element (derive's deletes, `version_only`) carries the
// raw version and its writer filters it. A common element trait must preserve
// those distinctions, attribute order, empty-element shape and scratch reuse;
// converting borrowed elements to owned strings would add per-element work to
// the measured diff/derive output path. Share the tag event loop below without
// changing either metadata representation or the emitted XML event sequence.
//
// Owned element types - Vec fields are not converted to Box<[T]> because these
// are low-volume types (derive_changes/diff output), not hot-path allocations.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(crate) struct OwnedMetadata {
    /// `None` when the source element carried no `version` attribute at
    /// all. Absent is not zero: emitting `version="0"` for an element
    /// whose producer never claimed a version invents data, and the
    /// version-precedence gates compare this field.
    pub(crate) version: Option<i32>,
    pub(crate) timestamp: String,
    pub(crate) changeset: String,
    pub(crate) uid: String,
    pub(crate) user: String,
    pub(crate) visible: String,
    /// Which of the text fields the source element carried, one
    /// `PRESENT_*` bit each. Presence, not emptiness, decides what is
    /// written: an explicit `user=""` names a blank user (apply records it
    /// as a PBF claim distinct from no user), and an explicit `uid=""` is
    /// re-emitted so that applying the re-emitted change refuses it exactly
    /// as applying the original does. A cleared field has its bit cleared.
    pub(crate) present: u8,
}

impl OwnedMetadata {
    pub(crate) const PRESENT_TIMESTAMP: u8 = 1;
    pub(crate) const PRESENT_CHANGESET: u8 = 1 << 1;
    pub(crate) const PRESENT_UID: u8 = 1 << 2;
    pub(crate) const PRESENT_USER: u8 = 1 << 3;
    pub(crate) const PRESENT_VISIBLE: u8 = 1 << 4;

    pub(crate) fn version_only(version: i32) -> Self {
        Self {
            version: Some(version),
            timestamp: String::new(),
            changeset: String::new(),
            uid: String::new(),
            user: String::new(),
            visible: String::new(),
            present: 0,
        }
    }

    fn has(&self, bit: u8) -> bool {
        self.present & bit != 0
    }

    /// The owned writers' metadata attributes. Production OSC re-emission
    /// writes [`Self::append_attrs`]; this stays as the oracle the byte
    /// sinks' differential tests compare with.
    #[cfg(test)]
    pub(crate) fn push_attrs(
        &self,
        elem: &mut quick_xml::events::BytesStart<'_>,
    ) -> crate::Result<()> {
        if let Some(version) = self.version {
            let v = version.to_string();
            elem.push_attribute(("version", v.as_str()));
        }
        for (bit, key, value) in self.text_fields() {
            if self.has(bit) {
                push_attribute_escaped(elem, key, value)?;
            }
        }
        Ok(())
    }

    /// The text fields in attribute order. Every one is the decoded
    /// (normalized) value its input carried, so a referenced newline, carriage
    /// return or tab is a real character in it and must be written back as a
    /// reference, or the next reader normalizes it to a space.
    fn text_fields(&self) -> [(u8, &'static str, &str); 5] {
        [
            (Self::PRESENT_TIMESTAMP, "timestamp", &self.timestamp),
            (Self::PRESENT_CHANGESET, "changeset", &self.changeset),
            (Self::PRESENT_UID, "uid", &self.uid),
            (Self::PRESENT_USER, "user", &self.user),
            (Self::PRESENT_VISIBLE, "visible", &self.visible),
        ]
    }

    /// The metadata as start tag content text, in the owned writers'
    /// attribute order and escaping (`push_attrs`).
    pub(crate) fn append_attrs(&self, content: &mut String) -> crate::Result<()> {
        if let Some(version) = self.version {
            append_int_attr(content, "version", i64::from(version));
        }
        for (bit, key, value) in self.text_fields() {
            if self.has(bit) {
                append_attr_escaped(content, key, value)?;
            }
        }
        Ok(())
    }
}

// The owned elements and their writers below are the byte contract the OSC
// re-emitting sinks (`merge_changes::emit::ElementBuf`) are held to by
// differential tests; no production path builds them any more.
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct OwnedNode {
    pub(crate) id: i64,
    /// Latitude then longitude in decimicrodegrees. Always present on a
    /// created or modified node; on a deleted one, present when its input
    /// gave it (`xml_parse::delete_coords`).
    pub(crate) location: Option<(i32, i32)>,
    pub(crate) tags: Vec<(String, String)>,
    pub(crate) metadata: Option<OwnedMetadata>,
}

#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct OwnedWay {
    pub(crate) id: i64,
    pub(crate) tags: Vec<(String, String)>,
    pub(crate) refs: Vec<i64>,
    pub(crate) metadata: Option<OwnedMetadata>,
}

#[derive(Clone, Debug)]
pub(crate) struct OwnedMember {
    pub(crate) id: MemberId,
    pub(crate) role: String,
}

#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct OwnedRelation {
    pub(crate) id: i64,
    pub(crate) tags: Vec<(String, String)>,
    pub(crate) members: Vec<OwnedMember>,
    pub(crate) metadata: Option<OwnedMetadata>,
}

// ---------------------------------------------------------------------------
// OSC XML element writing (shared by derive_changes and merge_changes)
// ---------------------------------------------------------------------------

/// The `type` attribute for one relation member.
///
/// `MemberId::Unknown` has no OSC spelling. Writing it as `type="node"` -
/// which both member writers used to do - produces a well-formed OSC file
/// in which the member points at a completely different object: ref 42 of
/// some unknown type becomes **node 42**, an unrelated element that very
/// likely exists. Nothing downstream can detect the substitution, so the
/// derived change stream silently corrupts referential integrity. This is
/// the OSC-side twin of the `BlockBuilder` poison flag on the PBF side, and
/// takes the same answer: refuse to write it.
pub(crate) fn member_type_str(id: MemberId, relation_id: i64) -> crate::Result<&'static str> {
    match id {
        MemberId::Node(_) => Ok(ElementKind::Node.as_str()),
        MemberId::Way(_) => Ok(ElementKind::Way.as_str()),
        MemberId::Relation(_) => Ok(ElementKind::Relation.as_str()),
        MemberId::Unknown(member_id, type_value) => Err(crate::Error::invalid_data(format!(
            "relation {relation_id} has a member (ref {member_id}) with unknown type value \
             {type_value}; OSC has no spelling for it, and writing it as a node member would \
             silently repoint the member at an unrelated object"
        ))),
    }
}

#[cfg(test)]
pub(crate) fn write_node_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    node: &OwnedNode,
) -> crate::Result<()> {
    let mut elem = BytesStart::new(ElementKind::Node.as_str());
    let id_str = node.id.to_string();
    elem.push_attribute(("id", id_str.as_str()));
    if let Some((lat, lon)) = node.location {
        let mut coord_buf = String::new();
        format_coord(&mut coord_buf, from_decimicro(lat));
        let lat_str = coord_buf.clone();
        format_coord(&mut coord_buf, from_decimicro(lon));
        elem.push_attribute(("lat", lat_str.as_str()));
        elem.push_attribute(("lon", coord_buf.as_str()));
    }
    let ctx = || in_element(ElementKind::Node, node.id);
    if let Some(meta) = &node.metadata {
        meta.push_attrs(&mut elem).map_err(ctx())?;
    }

    if node.tags.is_empty() {
        writer.write_event(Event::Empty(elem))?;
    } else {
        writer.write_event(Event::Start(elem))?;
        write_tags_xml(writer, &node.tags).map_err(ctx())?;
        writer.write_event(Event::End(BytesEnd::new(ElementKind::Node.as_str())))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn write_way_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    way: &OwnedWay,
) -> crate::Result<()> {
    let mut elem = BytesStart::new(ElementKind::Way.as_str());
    let id_str = way.id.to_string();
    elem.push_attribute(("id", id_str.as_str()));
    let ctx = || in_element(ElementKind::Way, way.id);
    if let Some(meta) = &way.metadata {
        meta.push_attrs(&mut elem).map_err(ctx())?;
    }

    if way.refs.is_empty() && way.tags.is_empty() {
        writer.write_event(Event::Empty(elem))?;
    } else {
        writer.write_event(Event::Start(elem))?;
        for r in &way.refs {
            let mut nd = BytesStart::new("nd");
            let r_str = r.to_string();
            nd.push_attribute(("ref", r_str.as_str()));
            writer.write_event(Event::Empty(nd))?;
        }
        write_tags_xml(writer, &way.tags).map_err(ctx())?;
        writer.write_event(Event::End(BytesEnd::new(ElementKind::Way.as_str())))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn write_relation_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    rel: &OwnedRelation,
) -> crate::Result<()> {
    let mut elem = BytesStart::new(ElementKind::Relation.as_str());
    let id_str = rel.id.to_string();
    elem.push_attribute(("id", id_str.as_str()));
    let ctx = || in_element(ElementKind::Relation, rel.id);
    if let Some(meta) = &rel.metadata {
        meta.push_attrs(&mut elem).map_err(ctx())?;
    }

    if rel.members.is_empty() && rel.tags.is_empty() {
        writer.write_event(Event::Empty(elem))?;
    } else {
        writer.write_event(Event::Start(elem))?;
        for m in &rel.members {
            let mut member = BytesStart::new("member");
            let type_str = member_type_str(m.id, rel.id)?;
            let member_id = m.id.id().to_string();
            member.push_attribute(("type", type_str));
            member.push_attribute(("ref", member_id.as_str()));
            push_attribute_escaped(&mut member, "role", m.role.as_str()).map_err(ctx())?;
            writer.write_event(Event::Empty(member))?;
        }
        write_tags_xml(writer, &rel.tags).map_err(ctx())?;
        writer.write_event(Event::End(BytesEnd::new(ElementKind::Relation.as_str())))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn write_tags_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    tags: &[(String, String)],
) -> crate::Result<()> {
    write_borrowed_tags_xml(writer, tags.iter().map(|(k, v)| (k.as_str(), v.as_str())))
}

/// The tag event loop. A block's tag strings always resolve (block
/// construction refuses an index past the string table), so the list is
/// never shortened.
fn write_borrowed_tags_xml<'a, W: std::io::Write>(
    writer: &mut Writer<W>,
    tags: impl Iterator<Item = (&'a str, &'a str)>,
) -> crate::Result<()> {
    for (k, v) in tags {
        let mut tag = BytesStart::new("tag");
        push_attribute_escaped(&mut tag, "k", k)?;
        push_attribute_escaped(&mut tag, "v", v)?;
        writer.write_event(Event::Empty(tag))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Borrowed element XML writing - zero-clone path for derive_changes
// ---------------------------------------------------------------------------

/// Emit the metadata attribute set from a borrowed element's info fields.
///
/// **Presence, not truthiness.** Each attribute is emitted exactly when the
/// source element carried it, which is why every parameter is an `Option`
/// rather than a value plus a `> 0` test. The previous shape gated on
/// `version > 0`, `changeset > 0` and `uid > 0`, which silently dropped
/// legitimate zero-valued metadata - `uid="0"` is the anonymous mapper that
/// old OSM data is full of - and, when every field happened to be zero,
/// shipped the element with no metadata attributes at all. The parse side
/// then set no `META_FLAG_PRESENT` and apply-changes wrote the element with
/// no info block, so `--format osc`'s documented lossless roundtrip did not
/// hold at exactly those margins.
///
/// `user` keeps its non-empty test: an empty user name is not
/// distinguishable from an absent one in the string table, and emitting
/// `user=""` would assert an anonymous edit that the source never claimed.
///
/// **Only what the OSC reader reads back.** A timestamp outside years 0000
/// to 9999 has no `YYYY-MM-DDTHH:MM:SSZ` spelling (`format_epoch_secs`
/// would write a five-digit or signed year), and pbfhogg's own OSC parser
/// refuses it, so derive would publish a file apply cannot read. It is
/// refused here instead; dropping the attribute would be a silent wrong
/// answer.
///
/// **Visibility: only `false` is written.** A created or modified element
/// whose metadata claims `visible=false` is printed with `visible="false"`,
/// which both pbfhogg's and osmium's apply read as the object's deleted
/// state (a versioned delete). Dropping it would publish a hidden record as
/// a visible upsert. An explicit `true` is not written: it is OSC's meaning
/// of an absent attribute, the dense read and re-encode rules treat an
/// absent `visible` column as `true` (and `BlockBuilder` writes the column
/// on every complete-metadata node), so printing it would put `visible="true"`
/// on every node of a pbfhogg-written file and print the same element
/// differently by representation.
#[allow(clippy::too_many_arguments)]
fn push_info_attrs(
    elem: &mut BytesStart<'_>,
    kind: ElementKind,
    id: i64,
    version: Option<i32>,
    milli_timestamp: Option<crate::Result<i64>>,
    changeset: Option<i64>,
    uid: Option<i32>,
    user: Option<&str>,
    visible: Option<bool>,
) -> crate::Result<()> {
    if let Some(version) = version {
        let v_str = version.to_string();
        elem.push_attribute(("version", v_str.as_str()));
    }
    // The claim is scaled exactly; an overflow is refused rather than
    // printed as the wrapped date.
    let milli_timestamp = milli_timestamp.transpose().map_err(in_element(kind, id))?;
    if let Some(milli_timestamp) = milli_timestamp {
        // Floor, not truncate, so a pre-1970 millisecond timestamp lands in
        // the right second; export formats the same way.
        let ts = crate::commands::format_rfc3339_utc(milli_timestamp.div_euclid(1000)).map_err(
            |err| {
                crate::Error::invalid_data_from(err).context(format!(
                    "{} {id}: its timestamp ({milli_timestamp} ms) cannot be written as OSC",
                    kind.as_str()
                ))
            },
        )?;
        elem.push_attribute(("timestamp", ts.as_str()));
    }
    if let Some(changeset) = changeset {
        let c_str = changeset.to_string();
        elem.push_attribute(("changeset", c_str.as_str()));
    }
    if let Some(uid) = uid {
        let u_str = uid.to_string();
        elem.push_attribute(("uid", u_str.as_str()));
    }
    if let Some(u) = user
        && !u.is_empty()
    {
        push_attribute_escaped(elem, "user", u).map_err(in_element(kind, id))?;
    }
    if visible == Some(false) {
        elem.push_attribute(("visible", "false"));
    }
    Ok(())
}

/// The decimicrodegree coordinates a derived node is printed at, checked
/// against what the OSC reader accepts. The exact accessors refuse a value
/// a wrapping conversion would put in some other place (the node scanner's
/// refusal, shared with extract); the error gains the node's id. The
/// geographic check is `coord_pack`'s on
/// the very `i32` pair printed: `format_coord(from_decimicro(v))` reads back
/// as `v` across the whole domain, and the first step past +-90 / +-180
/// prints past it, so the reader refuses exactly what this refuses.
fn osc_node_coords(
    lat: crate::Result<i32>,
    lon: crate::Result<i32>,
    id: i64,
) -> crate::Result<(i32, i32)> {
    let (lat, lon) = lat
        .and_then(|lat| Ok((lat, lon?)))
        .map_err(|e| e.context(format!("node {id}")))?;
    if !crate::coord_pack::in_domain(lat, lon) {
        return Err(crate::Error::invalid_data(format!(
            "node {id} lies outside the geographic domain (lat {}, lon {}); OSC readers, \
             pbfhogg's included, refuse such a coordinate",
            crate::coord_fmt::e7_to_deg(f64::from(lat)),
            crate::coord_fmt::e7_to_deg(f64::from(lon)),
        )));
    }
    Ok((lat, lon))
}

/// Write a borrowed `Element` as XML. Avoids the owned conversion path
/// (no tag/ref/member String cloning). Used by derive_changes for the
/// create/modify paths, on every path (block-pair, element stream and
/// parallel shards), so a derived element prints the same whichever path
/// found it.
///
/// Every string the element prints is resolved through the checked
/// lookups: a tag key or value, member role or user name whose string index
/// is past the block's string table is an error. An element that is
/// printed is therefore never silently shortened; an element that is only
/// compared, or only deleted (which prints id and version), is not
/// validated here. See `crate::content` for the comparison side.
pub(crate) fn write_element_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    elem: &crate::Element<'_>,
    coord_buf: &mut String,
) -> crate::Result<()> {
    match elem {
        crate::Element::DenseNode(dn) => write_dense_node_xml(writer, dn, coord_buf),
        crate::Element::Node(n) => write_borrowed_node_xml(writer, n, coord_buf),
        crate::Element::Way(w) => write_borrowed_way_xml(writer, w),
        crate::Element::Relation(r) => write_borrowed_relation_xml(writer, r),
    }
}

fn write_dense_node_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    node: &crate::dense::DenseNode<'_>,
    coord_buf: &mut String,
) -> crate::Result<()> {
    let mut elem = BytesStart::new(ElementKind::Node.as_str());
    let id_str = node.id().to_string();
    let (lat, lon) = osc_node_coords(node.decimicro_lat(), node.decimicro_lon(), node.id())?;
    format_coord(coord_buf, from_decimicro(lat));
    let lat_str = coord_buf.clone();
    format_coord(coord_buf, from_decimicro(lon));
    elem.push_attribute(("id", id_str.as_str()));
    elem.push_attribute(("lat", lat_str.as_str()));
    elem.push_attribute(("lon", coord_buf.as_str()));
    // A DenseInfo block decodes every field for every element, so presence
    // is read from the values: `claims` says which fields this slot claims
    // (all of them for a positive version, none for a negative one, and
    // only values no padding holds for version 0, which is how writers
    // store a node without metadata in a block whose other nodes carry it).
    if let Some(info) = node.info()
        && let Some(claims) = info.claims()
    {
        push_info_attrs(
            &mut elem,
            ElementKind::Node,
            node.id(),
            claims.version,
            claims.milli_timestamp,
            claims.changeset,
            claims.uid,
            claims.user.transpose()?,
            claims.visible,
        )?;
    }

    let mut tags = node.tags().peekable();
    if tags.peek().is_none() {
        writer.write_event(Event::Empty(elem))?;
    } else {
        writer.write_event(Event::Start(elem))?;
        write_borrowed_tags_xml(writer, tags).map_err(in_element(ElementKind::Node, node.id()))?;
        writer.write_event(Event::End(BytesEnd::new(ElementKind::Node.as_str())))?;
    }
    Ok(())
}

fn write_borrowed_node_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    node: &crate::elements::Node<'_>,
    coord_buf: &mut String,
) -> crate::Result<()> {
    let mut elem = BytesStart::new(ElementKind::Node.as_str());
    let id_str = node.id().to_string();
    let (lat, lon) = osc_node_coords(node.decimicro_lat(), node.decimicro_lon(), node.id())?;
    format_coord(coord_buf, from_decimicro(lat));
    let lat_str = coord_buf.clone();
    format_coord(coord_buf, from_decimicro(lon));
    elem.push_attribute(("id", id_str.as_str()));
    elem.push_attribute(("lat", lat_str.as_str()));
    elem.push_attribute(("lon", coord_buf.as_str()));
    let info = node.info();
    // Called unconditionally: every field is independently optional in the
    // wire format, so gating the whole call on `version` being present would
    // discard a timestamp, changeset, uid or user carried by an Info block
    // that happens to omit `version`. `push_info_attrs` emits per field on
    // presence, so an entirely empty Info contributes no attributes. The
    // version alone is filtered to a claim: 0 is how PBF writers spell "no
    // version" inside an Info that carries other fields.
    push_info_attrs(
        &mut elem,
        ElementKind::Node,
        node.id(),
        info.claimed_version(),
        info.claimed_milli_timestamp(),
        info.changeset(),
        info.uid(),
        info.user().transpose()?,
        info.visible_opt(),
    )?;

    let mut tags = node.tags().peekable();
    if tags.peek().is_none() {
        writer.write_event(Event::Empty(elem))?;
    } else {
        writer.write_event(Event::Start(elem))?;
        write_borrowed_tags_xml(writer, tags).map_err(in_element(ElementKind::Node, node.id()))?;
        writer.write_event(Event::End(BytesEnd::new(ElementKind::Node.as_str())))?;
    }
    Ok(())
}

fn write_borrowed_way_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    way: &crate::elements::Way<'_>,
) -> crate::Result<()> {
    let mut elem = BytesStart::new(ElementKind::Way.as_str());
    let id_str = way.id().to_string();
    elem.push_attribute(("id", id_str.as_str()));
    let info = way.info();
    // Unconditional for the same reason as the node path above: presence is
    // per field, not gated on `version`.
    push_info_attrs(
        &mut elem,
        ElementKind::Way,
        way.id(),
        info.claimed_version(),
        info.claimed_milli_timestamp(),
        info.changeset(),
        info.uid(),
        info.user().transpose()?,
        info.visible_opt(),
    )?;

    let mut refs = way.refs().peekable();
    let mut tags = way.tags().peekable();
    if refs.peek().is_none() && tags.peek().is_none() {
        writer.write_event(Event::Empty(elem))?;
    } else {
        writer.write_event(Event::Start(elem))?;
        for r in refs {
            let mut nd = BytesStart::new("nd");
            let r_str = r.to_string();
            nd.push_attribute(("ref", r_str.as_str()));
            writer.write_event(Event::Empty(nd))?;
        }
        write_borrowed_tags_xml(writer, tags).map_err(in_element(ElementKind::Way, way.id()))?;
        writer.write_event(Event::End(BytesEnd::new(ElementKind::Way.as_str())))?;
    }
    Ok(())
}

fn write_borrowed_relation_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    rel: &crate::elements::Relation<'_>,
) -> crate::Result<()> {
    let mut elem = BytesStart::new(ElementKind::Relation.as_str());
    let id_str = rel.id().to_string();
    elem.push_attribute(("id", id_str.as_str()));
    let info = rel.info();
    // Unconditional for the same reason as the node path above: presence is
    // per field, not gated on `version`.
    push_info_attrs(
        &mut elem,
        ElementKind::Relation,
        rel.id(),
        info.claimed_version(),
        info.claimed_milli_timestamp(),
        info.changeset(),
        info.uid(),
        info.user().transpose()?,
        info.visible_opt(),
    )?;

    let mut members = rel.members().peekable();
    let mut tags = rel.tags().peekable();
    if members.peek().is_none() && tags.peek().is_none() {
        writer.write_event(Event::Empty(elem))?;
    } else {
        writer.write_event(Event::Start(elem))?;
        for m in members {
            let mut member = BytesStart::new("member");
            let type_str = member_type_str(m.id, rel.id())?;
            let member_id = m.id.id().to_string();
            member.push_attribute(("type", type_str));
            member.push_attribute(("ref", member_id.as_str()));
            let ctx = || in_element(ElementKind::Relation, rel.id());
            push_attribute_escaped(&mut member, "role", m.role()?).map_err(ctx())?;
            writer.write_event(Event::Empty(member))?;
        }
        write_borrowed_tags_xml(writer, tags)
            .map_err(in_element(ElementKind::Relation, rel.id()))?;
        writer.write_event(Event::End(BytesEnd::new(ElementKind::Relation.as_str())))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::osc::parse::parse_osc_file;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::fs::File;
    use std::io::Write as IoWrite;
    use std::path::{Path, PathBuf};

    fn write_osc_gz(dir: &Path, filename: &str, xml: &[u8]) -> crate::Result<PathBuf> {
        let path = dir.join(filename);
        let file = File::create(&path)?;
        let mut enc = GzEncoder::new(file, Compression::fast());
        enc.write_all(xml)?;
        enc.finish()?;
        Ok(path)
    }

    /// Control characters in tag values must survive an OSC write -> parse
    /// roundtrip. XML attribute-value normalization turns RAW tab/newline/CR
    /// into spaces at parse time, so the writer must emit them as character
    /// references. Regression test for the 2026-07-10 finding where applying
    /// a derived OSC turned multi-line `inscription` values into
    /// space-separated ones.
    #[test]
    fn control_chars_in_tag_values_roundtrip() -> crate::Result<()> {
        let value = "MINDESTEN\n1864 -1920\ttabbed\rcr &<>\"' end";
        let node = OwnedNode {
            id: 100,
            location: Some((551_989_605, 92_041_876)),
            tags: vec![("inscription".to_string(), value.to_string())],
            metadata: None,
        };

        let mut writer = Writer::new(Vec::new());
        writer.write_event(Event::Start(BytesStart::new("osmChange")))?;
        writer.write_event(Event::Start(BytesStart::new("modify")))?;
        write_node_xml(&mut writer, &node)?;
        writer.write_event(Event::End(BytesEnd::new("modify")))?;
        writer.write_event(Event::End(BytesEnd::new("osmChange")))?;
        let xml = writer.into_inner();

        // Writer-side mechanism: the emitted document must carry character
        // references and no raw control bytes (the Writer emits no
        // indentation, so any raw control byte would be inside an attribute).
        let xml_str = std::str::from_utf8(&xml).map_err(crate::Error::invalid_data_from)?;
        assert!(xml_str.contains("&#10;"), "newline not escaped: {xml_str}");
        assert!(xml_str.contains("&#9;"), "tab not escaped: {xml_str}");
        assert!(xml_str.contains("&#13;"), "CR not escaped: {xml_str}");
        assert!(!xml_str.contains('\n'), "raw newline in output: {xml_str}");
        assert!(!xml_str.contains('\t'), "raw tab in output: {xml_str}");
        assert!(!xml_str.contains('\r'), "raw CR in output: {xml_str}");

        // Parse-side roundtrip: the value comes back byte-identical.
        let scratch = crate::path_guard::tests::scratch();
        let path = write_osc_gz(scratch.path(), "roundtrip.osc.gz", &xml)?;
        let overlay = parse_osc_file(&path)?;
        let parsed = overlay
            .get_node(100)
            .ok_or_else(|| crate::Error::internal("node 100 missing"))?;
        let tags: Vec<(&str, &str)> = parsed.tags().collect();
        assert_eq!(tags, vec![("inscription", value)]);
        Ok(())
    }

    /// Same guarantee for relation member roles (the other user-data
    /// attribute), via the owned-relation writer.
    #[test]
    fn control_chars_in_member_roles_roundtrip() -> crate::Result<()> {
        let role = "outer\nline2";
        let rel = OwnedRelation {
            id: 200,
            tags: Vec::new(),
            members: vec![OwnedMember {
                id: MemberId::Way(42),
                role: role.to_string(),
            }],
            metadata: None,
        };

        let mut writer = Writer::new(Vec::new());
        writer.write_event(Event::Start(BytesStart::new("osmChange")))?;
        writer.write_event(Event::Start(BytesStart::new("create")))?;
        write_relation_xml(&mut writer, &rel)?;
        writer.write_event(Event::End(BytesEnd::new("create")))?;
        writer.write_event(Event::End(BytesEnd::new("osmChange")))?;
        let xml = writer.into_inner();

        let xml_str = std::str::from_utf8(&xml).map_err(crate::Error::invalid_data_from)?;
        assert!(xml_str.contains("&#10;"), "newline not escaped: {xml_str}");
        assert!(!xml_str.contains('\n'), "raw newline in output: {xml_str}");

        let scratch = crate::path_guard::tests::scratch();
        let path = write_osc_gz(scratch.path(), "roundtrip.osc.gz", &xml)?;
        let overlay = parse_osc_file(&path)?;
        let parsed = overlay
            .get_relation(200)
            .ok_or_else(|| crate::Error::internal("relation 200 missing"))?;
        let members: Vec<_> = parsed.members().collect();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].2, role);
        Ok(())
    }

    /// A relation member of an unknown type must not be written as a node
    /// member. `type="node"` is well-formed OSC pointing at a different
    /// object, and nothing downstream can tell.
    #[test]
    fn unknown_member_type_is_refused_not_written_as_node() {
        let rel = OwnedRelation {
            id: 200,
            tags: Vec::new(),
            members: vec![OwnedMember {
                id: MemberId::Unknown(42, 9),
                role: "outer".to_string(),
            }],
            metadata: None,
        };

        let mut writer = Writer::new(Vec::new());
        let err = write_relation_xml(&mut writer, &rel)
            .expect_err("an unknown member type must not be written");
        let text = err.to_string();
        assert!(
            text.contains("relation 200") && text.contains("unknown type value 9"),
            "error should name the relation and the type value; got: {text}"
        );

        // Independent of the error: nothing that looks like a node member may
        // have reached the byte stream. Asserting on the emitted bytes rather
        // than on our own parser is the point - the previous behaviour
        // produced output our parser accepts without complaint.
        let xml = String::from_utf8_lossy(&writer.into_inner()).into_owned();
        assert!(
            !xml.contains("type=\"node\""),
            "no node member may be emitted for an unknown type; got: {xml}"
        );
    }

    /// Build a one-element PBF block through `BlockBuilder` and hand the
    /// borrowed element to `write_element_xml`, returning the emitted XML.
    /// Goes through the borrowed writers, which is where `push_info_attrs`
    /// lives; the owned writers use `OwnedMetadata::push_attrs` instead.
    fn borrowed_way_xml(meta: &crate::write::block_builder::Metadata<'_>) -> crate::Result<String> {
        let mut bb = crate::write::block_builder::BlockBuilder::new();
        bb.add_way(7, [("highway", "residential")], &[1, 2], Some(meta));
        let bytes = bb
            .take()?
            .ok_or_else(|| crate::Error::internal("block builder produced no block"))?
            .to_vec();
        let block = crate::PrimitiveBlock::from_vec(bytes)?;

        let mut writer = Writer::new(Vec::new());
        let mut coord_buf = String::new();
        for element in block.elements() {
            write_element_xml(&mut writer, &element, &mut coord_buf)?;
        }
        String::from_utf8(writer.into_inner()).map_err(crate::Error::invalid_data_from)
    }

    /// Zero-valued metadata is legitimate - `uid="0"` is the anonymous mapper
    /// that old OSM data is full of - and must survive into the OSC. The
    /// writer used to gate each attribute on `> 0`, so a zero changeset or uid
    /// vanished, and an element whose metadata was all zeros shipped with no
    /// metadata attributes at all. The parse side then recorded no metadata
    /// present and apply-changes wrote the element without an info block,
    /// breaking the documented lossless `--format osc` roundtrip.
    #[test]
    fn zero_valued_metadata_is_emitted_not_dropped() -> crate::Result<()> {
        let xml = borrowed_way_xml(&crate::write::block_builder::Metadata {
            version: 1,
            timestamp: 0,
            changeset: 0,
            uid: 0,
            user: "",
            visible: true,
        })?;
        // Assert on the emitted bytes directly rather than round-tripping
        // through our own parser.
        assert!(
            xml.contains("changeset=\"0\""),
            "changeset=0 must be emitted; got: {xml}"
        );
        assert!(
            xml.contains("uid=\"0\""),
            "uid=0 must be emitted; got: {xml}"
        );
        assert!(
            xml.contains("timestamp=\"1970-01-01T00:00:00Z\""),
            "timestamp=0 must be emitted; got: {xml}"
        );
        // An empty user name stays absent: it is not distinguishable from no
        // user at all, so emitting it would assert something the source never
        // claimed.
        assert!(
            !xml.contains("user="),
            "an empty user must not be emitted; got: {xml}"
        );
        Ok(())
    }

    /// Non-zero metadata still comes through unchanged, so the test above is
    /// not satisfied by a writer that emits every attribute unconditionally
    /// with a wrong value.
    #[test]
    fn nonzero_metadata_still_roundtrips_through_the_writer() -> crate::Result<()> {
        let xml = borrowed_way_xml(&crate::write::block_builder::Metadata {
            version: 7,
            // `Metadata::timestamp` is epoch SECONDS; the reader scales by the
            // block's date granularity to get the milliseconds the writer formats.
            timestamp: 86_400,
            changeset: 123_456,
            uid: 42,
            user: "mapper one",
            visible: true,
        })?;
        assert!(xml.contains("version=\"7\""), "got: {xml}");
        assert!(xml.contains("changeset=\"123456\""), "got: {xml}");
        assert!(xml.contains("uid=\"42\""), "got: {xml}");
        assert!(xml.contains("user=\"mapper one\""), "got: {xml}");
        assert!(
            xml.contains("timestamp=\"1970-01-02T00:00:00Z\""),
            "got: {xml}"
        );
        Ok(())
    }

    // -----------------------------------------------------------------
    // Independent protobuf emitter.
    //
    // `BlockBuilder::Metadata` carries a non-optional `version`, so our own
    // writer physically cannot produce an `Info` that omits it - yet the
    // wire format makes every `Info` field independently optional, and real
    // producers do omit fields. Constructing the adversarial block by hand
    // is the only way to reach that shape, and hand-encoding it here keeps
    // the fixture independent of the encoder under test.
    // -----------------------------------------------------------------

    /// Append a protobuf varint.
    fn pb_varint(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let byte = u8::try_from(v & 0x7f).expect("masked to 7 bits");
            v >>= 7;
            if v == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    /// Append a varint-typed field (wire type 0).
    fn pb_field_varint(out: &mut Vec<u8>, field: u64, v: u64) {
        pb_varint(out, field << 3);
        pb_varint(out, v);
    }

    /// Append a length-delimited field (wire type 2).
    fn pb_field_bytes(out: &mut Vec<u8>, field: u64, body: &[u8]) {
        pb_varint(out, (field << 3) | 2);
        pb_varint(out, body.len() as u64);
        out.extend_from_slice(body);
    }

    /// Hand-encode a `PrimitiveBlock` holding one bare `Way` whose `Info`
    /// carries exactly the fields named, each field emitted only when
    /// `Some`. `version` deliberately reachable as `None`.
    fn handmade_way_block(
        version: Option<i32>,
        timestamp: Option<i64>,
        changeset: Option<i64>,
        uid: Option<i32>,
    ) -> Vec<u8> {
        // Info: 1=version, 2=timestamp, 3=changeset, 4=uid.
        let mut info = Vec::new();
        if let Some(v) = version {
            pb_field_varint(&mut info, 1, u64::try_from(v).expect("non-negative"));
        }
        if let Some(t) = timestamp {
            pb_field_varint(&mut info, 2, u64::try_from(t).expect("non-negative"));
        }
        if let Some(c) = changeset {
            pb_field_varint(&mut info, 3, u64::try_from(c).expect("non-negative"));
        }
        if let Some(u) = uid {
            pb_field_varint(&mut info, 4, u64::try_from(u).expect("non-negative"));
        }

        // Way: 1=id, 4=info. No keys/vals/refs - a bare way is legal.
        let mut way = Vec::new();
        pb_field_varint(&mut way, 1, 7);
        pb_field_bytes(&mut way, 4, &info);

        // PrimitiveGroup: 3=ways.
        let mut group = Vec::new();
        pb_field_bytes(&mut group, 3, &way);

        // StringTable: 1=repeated bytes s. Index 0 must be the empty string.
        let mut stringtable = Vec::new();
        pb_field_bytes(&mut stringtable, 1, b"");

        // PrimitiveBlock: 1=stringtable, 2=primitivegroup. granularity (17)
        // and date_granularity (18) are left at their protobuf defaults of
        // 100 and 1000, which is what a real producer emits.
        let mut block = Vec::new();
        pb_field_bytes(&mut block, 1, &stringtable);
        pb_field_bytes(&mut block, 2, &group);
        block
    }

    /// Run the handmade block through the borrowed writers and return the
    /// emitted XML bytes as a string.
    fn handmade_way_xml(
        version: Option<i32>,
        timestamp: Option<i64>,
        changeset: Option<i64>,
        uid: Option<i32>,
    ) -> crate::Result<String> {
        let block = crate::PrimitiveBlock::from_vec(handmade_way_block(
            version, timestamp, changeset, uid,
        ))?;
        let mut writer = Writer::new(Vec::new());
        let mut coord_buf = String::new();
        for element in block.elements() {
            write_element_xml(&mut writer, &element, &mut coord_buf)?;
        }
        String::from_utf8(writer.into_inner()).map_err(crate::Error::invalid_data_from)
    }

    /// The emitter itself must be able to produce the ordinary shape, or the
    /// absence test below would be satisfied by a fixture that decodes to
    /// nothing at all.
    #[test]
    fn handmade_emitter_produces_a_readable_way_with_full_metadata() -> crate::Result<()> {
        let xml = handmade_way_xml(Some(3), Some(86), Some(555), Some(9))?;
        assert!(xml.contains("id=\"7\""), "got: {xml}");
        assert!(xml.contains("version=\"3\""), "got: {xml}");
        assert!(xml.contains("changeset=\"555\""), "got: {xml}");
        assert!(xml.contains("uid=\"9\""), "got: {xml}");
        assert!(
            xml.contains("timestamp=\"1970-01-01T00:01:26Z\""),
            "got: {xml}"
        );
        Ok(())
    }

    /// An `Info` with no `version` must still contribute every other field
    /// it carries. The borrowed writers used to call `push_info_attrs` only
    /// when `info.version().is_some()`, so a valid protobuf `Info` holding a
    /// timestamp, changeset and uid but no version lost ALL of its metadata
    /// in derived OSC output - the same shape of defect as the `> 0` gating
    /// `push_info_attrs` was rewritten to fix, one layer up at the call
    /// sites.
    #[test]
    fn metadata_without_version_is_not_discarded() -> crate::Result<()> {
        let xml = handmade_way_xml(None, Some(86), Some(555), Some(9))?;
        // Assert on the emitted bytes rather than round-tripping through our
        // own parser: the previous output was well-formed OSC that our
        // parser accepts without complaint, just missing the metadata.
        assert!(
            xml.contains("changeset=\"555\""),
            "changeset must survive an Info with no version; got: {xml}"
        );
        assert!(
            xml.contains("uid=\"9\""),
            "uid must survive an Info with no version; got: {xml}"
        );
        assert!(
            xml.contains("timestamp=\"1970-01-01T00:01:26Z\""),
            "timestamp must survive an Info with no version; got: {xml}"
        );
        // The absent field stays absent - the fix must not invent a version.
        assert!(
            !xml.contains("version="),
            "an absent version must not be fabricated; got: {xml}"
        );
        Ok(())
    }

    /// The complement: an `Info` present but entirely empty contributes no
    /// attributes at all. Without this, the test above is satisfied by a
    /// writer that emits defaults for everything.
    #[test]
    fn empty_info_emits_no_metadata_attributes() -> crate::Result<()> {
        let xml = handmade_way_xml(None, None, None, None)?;
        for attr in ["version=", "timestamp=", "changeset=", "uid=", "user="] {
            assert!(
                !xml.contains(attr),
                "an empty Info must contribute no {attr} attribute; got: {xml}"
            );
        }
        assert!(xml.contains("id=\"7\""), "the way itself must be emitted");
        Ok(())
    }

    const META_ATTRS: [&str; 6] = [
        "version=",
        "timestamp=",
        "changeset=",
        "uid=",
        "user=",
        "visible=",
    ];

    /// A hidden created or modified element keeps `visible="false"`, which
    /// apply reads as the object's deleted state; dropping it published a
    /// hidden record as a visible upsert. A visible one prints no
    /// `visible` at all, plain and dense alike.
    #[test]
    fn a_hidden_element_prints_visible_false_and_a_visible_one_prints_none() -> crate::Result<()> {
        for visible in [false, true] {
            let m = crate::write::block_builder::Metadata {
                visible,
                ..meta(3, 9)
            };
            let way = borrowed_way_xml(&m)?;
            let node = borrowed_dense_xml(&[(1, Some(m))])?;
            for xml in [way.as_str(), node[0].as_str()] {
                if visible {
                    assert!(!xml.contains("visible="), "got: {xml}");
                } else {
                    assert!(xml.contains("visible=\"false\""), "got: {xml}");
                }
            }
        }
        Ok(())
    }

    fn meta(version: i32, uid: i32) -> crate::write::block_builder::Metadata<'static> {
        crate::write::block_builder::Metadata {
            version,
            timestamp: 86_400,
            changeset: 555,
            uid,
            user: "mapper",
            visible: true,
        }
    }

    /// Build one dense block through `BlockBuilder` from `(id, metadata)`
    /// pairs and return the borrowed writers' XML for each node, in order.
    fn borrowed_dense_xml(
        nodes: &[(i64, Option<crate::write::block_builder::Metadata<'_>>)],
    ) -> crate::Result<Vec<String>> {
        let mut bb = crate::write::block_builder::BlockBuilder::new();
        for (id, meta) in nodes {
            bb.add_node(*id, 0, 0, Vec::<(&str, &str)>::new(), meta.as_ref());
        }
        let bytes = bb
            .take()?
            .ok_or_else(|| crate::Error::internal("block builder produced no block"))?
            .to_vec();
        let block = crate::PrimitiveBlock::from_vec(bytes)?;
        let mut coord_buf = String::new();
        let mut out = Vec::new();
        for element in block.elements() {
            assert!(matches!(element, crate::Element::DenseNode(_)));
            let mut writer = Writer::new(Vec::new());
            write_element_xml(&mut writer, &element, &mut coord_buf)?;
            out.push(
                String::from_utf8(writer.into_inner()).map_err(crate::Error::invalid_data_from)?,
            );
        }
        Ok(out)
    }

    /// A node added without metadata to a dense block whose other nodes
    /// carry it, whether it comes first or follows, prints with no metadata
    /// at all, not as
    /// `version="0" timestamp="1970-01-01T00:00:00Z" changeset="0" uid="0"`.
    /// (`BlockBuilder` now gives it a dense group without DenseInfo; files
    /// from other writers can still pad it with a version-0 slot, which
    /// claims nothing either.)
    #[test]
    fn a_metadata_less_dense_node_prints_no_metadata() -> crate::Result<()> {
        for nodes in [
            [(1, None), (2, Some(meta(3, 9))), (3, None)],
            [(1, Some(meta(3, 9))), (2, None), (3, Some(meta(4, 9)))],
        ] {
            let xml = borrowed_dense_xml(&nodes)?;
            for (i, (_, m)) in nodes.iter().enumerate() {
                if m.is_some() {
                    assert!(xml[i].contains("version=\""), "got: {}", xml[i]);
                    assert!(xml[i].contains("uid=\"9\""), "got: {}", xml[i]);
                } else {
                    for attr in META_ATTRS {
                        assert!(
                            !xml[i].contains(attr),
                            "no {attr} expected; got: {}",
                            xml[i]
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// A versionless node that does carry other metadata - what apply-changes
    /// writes for an OSC element with a timestamp but no version - keeps
    /// those fields and gains no version. A zero field in such a slot is
    /// indistinguishable from padding and is dropped (uid 0 here).
    #[test]
    fn a_versionless_dense_node_keeps_its_nonzero_fields() -> crate::Result<()> {
        let xml = borrowed_dense_xml(&[
            (1, Some(meta(2, 9))),
            (2, Some(meta(0, 9))),
            (3, Some(meta(0, 0))),
        ])?;
        for x in &xml[1..] {
            assert!(!x.contains("version="), "got: {x}");
            assert!(x.contains("changeset=\"555\""), "got: {x}");
            assert!(x.contains("user=\"mapper\""), "got: {x}");
            assert!(x.contains("timestamp=\"1970-01-02T00:00:00Z\""), "got: {x}");
        }
        assert!(xml[1].contains("uid=\"9\""), "got: {}", xml[1]);
        assert!(!xml[2].contains("uid="), "got: {}", xml[2]);
        Ok(())
    }

    /// An `Info` with version 0 prints no version, but its other fields keep
    /// their own presence, zeros included - unlike a dense slot, an `Info`
    /// can say which fields it carries.
    #[test]
    fn a_version_zero_info_prints_every_field_but_the_version() -> crate::Result<()> {
        let xml = borrowed_way_xml(&meta(0, 0))?;
        assert!(!xml.contains("version="), "got: {xml}");
        assert!(xml.contains("uid=\"0\""), "got: {xml}");
        assert!(xml.contains("changeset=\"555\""), "got: {xml}");
        assert!(xml.contains("user=\"mapper\""), "got: {xml}");
        Ok(())
    }

    /// The dense writer's XML for one node at `(lat, lon)` decimicrodegrees.
    fn dense_node_at(lat: i32, lon: i32) -> crate::Result<String> {
        let mut bb = crate::write::block_builder::BlockBuilder::new();
        bb.add_node(1, lat, lon, Vec::<(&str, &str)>::new(), None);
        let bytes = bb
            .take()?
            .ok_or_else(|| crate::Error::internal("block builder produced no block"))?
            .to_vec();
        let block = crate::PrimitiveBlock::from_vec(bytes)?;
        let mut writer = Writer::new(Vec::new());
        let mut coord_buf = String::new();
        for element in block.elements() {
            write_element_xml(&mut writer, &element, &mut coord_buf)?;
        }
        String::from_utf8(writer.into_inner()).map_err(crate::Error::invalid_data_from)
    }

    /// The dense writer's XML for one node carrying the tag `(k, v)`.
    fn dense_node_tagged(k: &str, v: &str) -> crate::Result<String> {
        let mut bb = crate::write::block_builder::BlockBuilder::new();
        bb.add_node(3, 0, 0, [(k, v)], None);
        let bytes = bb
            .take()?
            .ok_or_else(|| crate::Error::internal("block builder produced no block"))?
            .to_vec();
        let block = crate::PrimitiveBlock::from_vec(bytes)?;
        let mut writer = Writer::new(Vec::new());
        let mut coord_buf = String::new();
        for element in block.elements() {
            write_element_xml(&mut writer, &element, &mut coord_buf)?;
        }
        String::from_utf8(writer.into_inner()).map_err(crate::Error::invalid_data_from)
    }

    /// A PBF string is valid UTF-8 but may hold a character XML 1.0 cannot
    /// carry even as a reference; derive refuses it, naming the element,
    /// rather than publish a file no XML reader accepts. The scan does not
    /// stop at a tab that needs a reference.
    #[test]
    fn pbf_strings_xml_cannot_carry_are_refused() -> crate::Result<()> {
        for (k, v) in [
            ("a\0", "b"),
            ("a", "\u{1}"),
            ("a", "x\ty\u{1F}"),
            ("a", "\u{FFFE}"),
            ("a", "\u{FFFF}"),
        ] {
            let err = dense_node_tagged(k, v).expect_err("forbidden character");
            let msg = err.to_string();
            assert!(
                msg.contains("XML 1.0 cannot carry") && msg.contains("node 3"),
                "{k:?}={v:?}: {msg}"
            );
        }
        let xml = dense_node_tagged("a", "\u{FFFD}\u{FEFF}\u{FF01}\u{85}\u{10000}\t")?;
        assert!(
            xml.contains("v=\"\u{FFFD}\u{FEFF}\u{FF01}\u{85}\u{10000}&#9;\""),
            "got: {xml}"
        );
        Ok(())
    }

    /// Derive only writes coordinates the OSC reader takes back: the domain
    /// edges print, one step past any of them is refused, never written as
    /// `lat="90.0000001"` or `lat="91"` for apply to refuse later.
    #[test]
    fn node_coordinates_outside_the_geographic_domain_are_refused() -> crate::Result<()> {
        let xml = dense_node_at(900_000_000, -1_800_000_000)?;
        assert!(
            xml.contains("lat=\"90\"") && xml.contains("lon=\"-180\""),
            "got: {xml}"
        );
        let xml = dense_node_at(-900_000_000, 1_800_000_000)?;
        assert!(
            xml.contains("lat=\"-90\"") && xml.contains("lon=\"180\""),
            "got: {xml}"
        );
        for (lat, lon) in [
            (900_000_001, 0),
            (-900_000_001, 0),
            (910_000_000, 0),
            (0, 1_800_000_001),
            (0, -1_800_000_001),
        ] {
            let err = dense_node_at(lat, lon).expect_err("outside the domain");
            assert!(
                err.to_string().contains("geographic domain"),
                "({lat}, {lon}): {err}"
            );
        }
        Ok(())
    }

    /// The written instant is one `parse_rfc3339_utc` reads: years 0000 and
    /// 9999 print, a second beyond either end is refused rather than written
    /// with a five-digit or signed year.
    #[test]
    fn element_timestamps_outside_years_0000_to_9999_are_refused() -> crate::Result<()> {
        let way_at = |timestamp| {
            borrowed_way_xml(&crate::write::block_builder::Metadata {
                timestamp,
                ..meta(1, 1)
            })
        };
        for (secs, text) in [
            (253_402_300_799, "9999-12-31T23:59:59Z"),
            (-62_167_219_200, "0000-01-01T00:00:00Z"),
        ] {
            let xml = way_at(secs)?;
            assert!(xml.contains(&format!("timestamp=\"{text}\"")), "got: {xml}");
            assert_eq!(crate::commands::parse_rfc3339_utc(text), Ok(secs));
        }
        for secs in [253_402_300_800, -62_167_219_201] {
            let err = way_at(secs).expect_err("outside years 0000 to 9999");
            assert!(err.to_string().contains("way 7"), "{secs}: {err}");
        }
        Ok(())
    }

    /// A timestamp whose scaling by date_granularity overflows i64 is
    /// refused, not wrapped: `i64::MAX / 1000 + 1` seconds at the default
    /// granularity would otherwise print as some unrelated, readable date.
    #[test]
    fn a_timestamp_whose_scaling_overflows_is_refused() {
        let err = handmade_way_xml(Some(1), Some(i64::MAX / 1000 + 1), None, None)
            .expect_err("scaling overflows");
        let text = err.to_string();
        assert!(
            text.contains("date_granularity") && text.contains("way 7"),
            "{text}"
        );
    }
}
