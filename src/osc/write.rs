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

/// Push an attribute whose value is OSM user data (tag key/value, member
/// role, user name) and may therefore contain control characters.
///
/// XML 1.0 attribute-value normalization (spec section 3.3.3) replaces raw
/// tab, newline, and carriage-return characters in attribute values with
/// spaces at parse time, so a literal newline inside e.g. a multi-line
/// `inscription` tag value does not survive a write -> parse roundtrip -
/// the applied result silently has spaces where the source had newlines.
/// Character references (`&#10;` etc.) are the only spec-conforming way to
/// preserve them; osmium's OSC writer does the same (`&#xA;`).
/// quick-xml's tuple `push_attribute` escapes only `< > & " '`, so the
/// control characters must be pre-escaped here and pushed as an
/// already-escaped `Attribute`.
fn push_attribute_escaped<'a>(elem: &mut BytesStart<'a>, key: &'a str, value: &str) {
    if value.bytes().any(|b| matches!(b, b'\n' | b'\r' | b'\t')) {
        let escaped = quick_xml::escape::escape(value);
        let mut out = String::with_capacity(escaped.len() + 8);
        for ch in escaped.chars() {
            match ch {
                '\n' => out.push_str("&#10;"),
                '\r' => out.push_str("&#13;"),
                '\t' => out.push_str("&#9;"),
                _ => out.push(ch),
            }
        }
        elem.push_attribute(quick_xml::events::attributes::Attribute {
            key: quick_xml::name::QName(key),
            value: std::borrow::Cow::Owned(out),
        });
    } else {
        elem.push_attribute((key, value));
    }
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
}

impl OwnedMetadata {
    pub(crate) fn version_only(version: i32) -> Self {
        Self {
            version: Some(version),
            timestamp: String::new(),
            changeset: String::new(),
            uid: String::new(),
            user: String::new(),
            visible: String::new(),
        }
    }

    pub(crate) fn push_attrs(&self, elem: &mut quick_xml::events::BytesStart<'_>) {
        if let Some(version) = self.version {
            let v = version.to_string();
            elem.push_attribute(("version", v.as_str()));
        }
        if !self.timestamp.is_empty() {
            elem.push_attribute(("timestamp", self.timestamp.as_str()));
        }
        if !self.changeset.is_empty() {
            elem.push_attribute(("changeset", self.changeset.as_str()));
        }
        if !self.uid.is_empty() {
            elem.push_attribute(("uid", self.uid.as_str()));
        }
        if !self.user.is_empty() {
            push_attribute_escaped(elem, "user", self.user.as_str());
        }
        if !self.visible.is_empty() {
            elem.push_attribute(("visible", self.visible.as_str()));
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct OwnedNode {
    pub(crate) id: i64,
    pub(crate) decimicro_lat: i32,
    pub(crate) decimicro_lon: i32,
    pub(crate) tags: Vec<(String, String)>,
    pub(crate) metadata: Option<OwnedMetadata>,
}

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

pub(crate) fn write_node_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    node: &OwnedNode,
) -> crate::Result<()> {
    let mut elem = BytesStart::new(ElementKind::Node.as_str());
    let id_str = node.id.to_string();
    let mut coord_buf = String::new();
    format_coord(&mut coord_buf, from_decimicro(node.decimicro_lat));
    let lat_str = coord_buf.clone();
    format_coord(&mut coord_buf, from_decimicro(node.decimicro_lon));
    elem.push_attribute(("id", id_str.as_str()));
    elem.push_attribute(("lat", lat_str.as_str()));
    elem.push_attribute(("lon", coord_buf.as_str()));
    if let Some(meta) = &node.metadata {
        meta.push_attrs(&mut elem);
    }

    if node.tags.is_empty() {
        writer.write_event(Event::Empty(elem))?;
    } else {
        writer.write_event(Event::Start(elem))?;
        write_tags_xml(writer, &node.tags)?;
        writer.write_event(Event::End(BytesEnd::new(ElementKind::Node.as_str())))?;
    }
    Ok(())
}

pub(crate) fn write_way_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    way: &OwnedWay,
) -> crate::Result<()> {
    let mut elem = BytesStart::new(ElementKind::Way.as_str());
    let id_str = way.id.to_string();
    elem.push_attribute(("id", id_str.as_str()));
    if let Some(meta) = &way.metadata {
        meta.push_attrs(&mut elem);
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
        write_tags_xml(writer, &way.tags)?;
        writer.write_event(Event::End(BytesEnd::new(ElementKind::Way.as_str())))?;
    }
    Ok(())
}

pub(crate) fn write_relation_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    rel: &OwnedRelation,
) -> crate::Result<()> {
    let mut elem = BytesStart::new(ElementKind::Relation.as_str());
    let id_str = rel.id.to_string();
    elem.push_attribute(("id", id_str.as_str()));
    if let Some(meta) = &rel.metadata {
        meta.push_attrs(&mut elem);
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
            push_attribute_escaped(&mut member, "role", m.role.as_str());
            writer.write_event(Event::Empty(member))?;
        }
        write_tags_xml(writer, &rel.tags)?;
        writer.write_event(Event::End(BytesEnd::new(ElementKind::Relation.as_str())))?;
    }
    Ok(())
}

/// Write an element as a delete entry (id + metadata only, no content).
pub(crate) fn write_delete_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    kind: ElementKind,
    id: i64,
    metadata: Option<&OwnedMetadata>,
) -> crate::Result<()> {
    let mut elem = BytesStart::new(kind.as_str());
    let id_str = id.to_string();
    elem.push_attribute(("id", id_str.as_str()));
    if let Some(meta) = metadata {
        meta.push_attrs(&mut elem);
    }
    writer.write_event(Event::Empty(elem))?;
    Ok(())
}

pub(crate) fn write_tags_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    tags: &[(String, String)],
) -> crate::Result<()> {
    write_borrowed_tags_xml(writer, tags.iter().map(|(k, v)| (k.as_str(), v.as_str())))
}

fn write_borrowed_tags_xml<'a, W: std::io::Write>(
    writer: &mut Writer<W>,
    tags: impl Iterator<Item = (&'a str, &'a str)>,
) -> crate::Result<()> {
    write_checked_tags_xml(writer, tags.map(Ok))
}

/// The tag event loop over checked tags: a tag whose key or value index is
/// past the block's string table is an error, never a shortened tag list.
fn write_checked_tags_xml<'a, W: std::io::Write>(
    writer: &mut Writer<W>,
    tags: impl Iterator<Item = crate::Result<(&'a str, &'a str)>>,
) -> crate::Result<()> {
    for tag in tags {
        let (k, v) = tag?;
        let mut tag = BytesStart::new("tag");
        push_attribute_escaped(&mut tag, "k", k);
        push_attribute_escaped(&mut tag, "v", v);
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
fn push_info_attrs(
    elem: &mut BytesStart<'_>,
    version: Option<i32>,
    milli_timestamp: Option<i64>,
    changeset: Option<i64>,
    uid: Option<i32>,
    user: Option<&str>,
) {
    if let Some(version) = version {
        let v_str = version.to_string();
        elem.push_attribute(("version", v_str.as_str()));
    }
    if let Some(milli_timestamp) = milli_timestamp {
        // Floor, not truncate, so a pre-1970 millisecond timestamp lands in
        // the right second; export formats the same way.
        let ts = crate::commands::format_epoch_secs(milli_timestamp.div_euclid(1000));
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
        push_attribute_escaped(elem, "user", u);
    }
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
    format_coord(coord_buf, from_decimicro(node.decimicro_lat()));
    let lat_str = coord_buf.clone();
    format_coord(coord_buf, from_decimicro(node.decimicro_lon()));
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
            claims.version,
            claims.milli_timestamp,
            claims.changeset,
            claims.uid,
            if claims.user {
                Some(info.user()?)
            } else {
                None
            },
        );
    }

    let mut tags = node.tags_checked().peekable();
    if tags.peek().is_none() {
        writer.write_event(Event::Empty(elem))?;
    } else {
        writer.write_event(Event::Start(elem))?;
        write_checked_tags_xml(writer, tags)?;
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
    format_coord(coord_buf, from_decimicro(node.decimicro_lat()));
    let lat_str = coord_buf.clone();
    format_coord(coord_buf, from_decimicro(node.decimicro_lon()));
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
        info.claimed_version(),
        info.milli_timestamp(),
        info.changeset(),
        info.uid(),
        info.user().transpose()?,
    );

    let mut tags = node.tags_checked().peekable();
    if tags.peek().is_none() {
        writer.write_event(Event::Empty(elem))?;
    } else {
        writer.write_event(Event::Start(elem))?;
        write_checked_tags_xml(writer, tags)?;
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
        info.claimed_version(),
        info.milli_timestamp(),
        info.changeset(),
        info.uid(),
        info.user().transpose()?,
    );

    let mut refs = way.refs().peekable();
    let mut tags = way.tags_checked().peekable();
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
        write_checked_tags_xml(writer, tags)?;
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
        info.claimed_version(),
        info.milli_timestamp(),
        info.changeset(),
        info.uid(),
        info.user().transpose()?,
    );

    let mut members = rel.members().peekable();
    let mut tags = rel.tags_checked().peekable();
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
            push_attribute_escaped(&mut member, "role", m.role()?);
            writer.write_event(Event::Empty(member))?;
        }
        write_checked_tags_xml(writer, tags)?;
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
            decimicro_lat: 551_989_605,
            decimicro_lon: 92_041_876,
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

    const META_ATTRS: [&str; 5] = ["version=", "timestamp=", "changeset=", "uid=", "user="];

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
    /// carry it gets a padding slot - version 0 and zeros - both when it
    /// comes first (backfilled) and when it follows (pushed). That slot
    /// claims nothing, so the node prints with no metadata at all, not as
    /// `version="0" timestamp="1970-01-01T00:00:00Z" changeset="0" uid="0"`.
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
}
