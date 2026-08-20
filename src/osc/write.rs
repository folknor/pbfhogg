//! XML-oriented owned element types for derive_changes, diff, merge_changes, and tags_filter_osc.
//!
//! Metadata fields are String-typed for direct XML attribute output.
//! See `crate::owned` for the PBF-oriented variant with native types.

use quick_xml::Writer;
use quick_xml::events::{BytesEnd, BytesStart, Event};

use crate::MemberId;
use crate::coord_fmt::{format_coord, from_decimicro};

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
            key: quick_xml::name::QName(key.as_bytes()),
            value: std::borrow::Cow::Owned(out.into_bytes()),
        });
    } else {
        elem.push_attribute((key, value));
    }
}

// ---------------------------------------------------------------------------
// Owned element types - Vec fields are not converted to Box<[T]> because these
// are low-volume types (derive_changes/diff output), not hot-path allocations.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(crate) struct OwnedMetadata {
    pub(crate) version: i32,
    pub(crate) timestamp: String,
    pub(crate) changeset: String,
    pub(crate) uid: String,
    pub(crate) user: String,
    pub(crate) visible: String,
}

impl OwnedMetadata {
    pub(crate) fn version_only(version: i32) -> Self {
        Self {
            version,
            timestamp: String::new(),
            changeset: String::new(),
            uid: String::new(),
            user: String::new(),
            visible: String::new(),
        }
    }

    pub(crate) fn push_attrs(&self, elem: &mut quick_xml::events::BytesStart<'_>) {
        let v = self.version.to_string();
        elem.push_attribute(("version", v.as_str()));
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
// Element comparison
// ---------------------------------------------------------------------------

pub(crate) fn nodes_equal(a: &OwnedNode, b: &OwnedNode) -> bool {
    a.decimicro_lat == b.decimicro_lat && a.decimicro_lon == b.decimicro_lon && a.tags == b.tags
}

pub(crate) fn ways_equal(a: &OwnedWay, b: &OwnedWay) -> bool {
    a.refs == b.refs && a.tags == b.tags
}

pub(crate) fn members_equal(a: &[OwnedMember], b: &[OwnedMember]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(ma, mb)| ma.id == mb.id && ma.role == mb.role)
}

pub(crate) fn relations_equal(a: &OwnedRelation, b: &OwnedRelation) -> bool {
    a.tags == b.tags && members_equal(&a.members, &b.members)
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
fn member_type_str(id: MemberId, relation_id: i64) -> crate::BoxResult<&'static str> {
    match id {
        MemberId::Node(_) => Ok("node"),
        MemberId::Way(_) => Ok("way"),
        MemberId::Relation(_) => Ok("relation"),
        MemberId::Unknown(member_id, type_value) => Err(format!(
            "relation {relation_id} has a member (ref {member_id}) with unknown type value \
             {type_value}; OSC has no spelling for it, and writing it as a node member would \
             silently repoint the member at an unrelated object"
        )
        .into()),
    }
}

pub(crate) fn write_node_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    node: &OwnedNode,
) -> crate::BoxResult<()> {
    let mut elem = BytesStart::new("node");
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
        writer.write_event(Event::End(BytesEnd::new("node")))?;
    }
    Ok(())
}

pub(crate) fn write_way_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    way: &OwnedWay,
) -> crate::BoxResult<()> {
    let mut elem = BytesStart::new("way");
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
        writer.write_event(Event::End(BytesEnd::new("way")))?;
    }
    Ok(())
}

pub(crate) fn write_relation_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    rel: &OwnedRelation,
) -> crate::BoxResult<()> {
    let mut elem = BytesStart::new("relation");
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
        writer.write_event(Event::End(BytesEnd::new("relation")))?;
    }
    Ok(())
}

/// Write an element as a delete entry (id + metadata only, no content).
pub(crate) fn write_delete_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    tag_name: &str,
    id: i64,
    metadata: Option<&OwnedMetadata>,
) -> crate::BoxResult<()> {
    let mut elem = BytesStart::new(tag_name);
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
) -> crate::BoxResult<()> {
    for (k, v) in tags {
        let mut tag = BytesStart::new("tag");
        push_attribute_escaped(&mut tag, "k", k.as_str());
        push_attribute_escaped(&mut tag, "v", v.as_str());
        writer.write_event(Event::Empty(tag))?;
    }
    Ok(())
}

fn write_borrowed_tags_xml<'a, W: std::io::Write>(
    writer: &mut Writer<W>,
    tags: impl Iterator<Item = (&'a str, &'a str)>,
) -> crate::BoxResult<()> {
    for (k, v) in tags {
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
        let ts = crate::commands::format_epoch_secs((milli_timestamp / 1000).cast_unsigned());
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
/// create/modify paths.
pub(crate) fn write_element_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    elem: &crate::Element<'_>,
    coord_buf: &mut String,
) -> crate::BoxResult<()> {
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
) -> crate::BoxResult<()> {
    let mut elem = BytesStart::new("node");
    let id_str = node.id().to_string();
    format_coord(coord_buf, from_decimicro(node.decimicro_lat()));
    let lat_str = coord_buf.clone();
    format_coord(coord_buf, from_decimicro(node.decimicro_lon()));
    elem.push_attribute(("id", id_str.as_str()));
    elem.push_attribute(("lat", lat_str.as_str()));
    elem.push_attribute(("lon", coord_buf.as_str()));
    if let Some(info) = node.info()
        && info.version() != -1
    {
        // A DenseInfo block delta-decodes every field for every element, so
        // once past the `version() != -1` per-element absence sentinel above,
        // all of these are genuinely present - unlike the non-dense path,
        // where each field is independently optional.
        push_info_attrs(
            &mut elem,
            Some(info.version()),
            Some(info.milli_timestamp()),
            Some(info.changeset()),
            Some(info.uid()),
            info.user().ok(),
        );
    }

    let mut tags = node.tags().peekable();
    if tags.peek().is_none() {
        writer.write_event(Event::Empty(elem))?;
    } else {
        writer.write_event(Event::Start(elem))?;
        write_borrowed_tags_xml(writer, tags)?;
        writer.write_event(Event::End(BytesEnd::new("node")))?;
    }
    Ok(())
}

fn write_borrowed_node_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    node: &crate::elements::Node<'_>,
    coord_buf: &mut String,
) -> crate::BoxResult<()> {
    let mut elem = BytesStart::new("node");
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
    // presence, so an entirely empty Info contributes no attributes.
    push_info_attrs(
        &mut elem,
        info.version(),
        info.milli_timestamp(),
        info.changeset(),
        info.uid(),
        info.user().and_then(Result::ok),
    );

    let mut tags = node.tags().peekable();
    if tags.peek().is_none() {
        writer.write_event(Event::Empty(elem))?;
    } else {
        writer.write_event(Event::Start(elem))?;
        write_borrowed_tags_xml(writer, tags)?;
        writer.write_event(Event::End(BytesEnd::new("node")))?;
    }
    Ok(())
}

fn write_borrowed_way_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    way: &crate::elements::Way<'_>,
) -> crate::BoxResult<()> {
    let mut elem = BytesStart::new("way");
    let id_str = way.id().to_string();
    elem.push_attribute(("id", id_str.as_str()));
    let info = way.info();
    // Unconditional for the same reason as the node path above: presence is
    // per field, not gated on `version`.
    push_info_attrs(
        &mut elem,
        info.version(),
        info.milli_timestamp(),
        info.changeset(),
        info.uid(),
        info.user().and_then(Result::ok),
    );

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
        write_borrowed_tags_xml(writer, tags)?;
        writer.write_event(Event::End(BytesEnd::new("way")))?;
    }
    Ok(())
}

fn write_borrowed_relation_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    rel: &crate::elements::Relation<'_>,
) -> crate::BoxResult<()> {
    let mut elem = BytesStart::new("relation");
    let id_str = rel.id().to_string();
    elem.push_attribute(("id", id_str.as_str()));
    let info = rel.info();
    // Unconditional for the same reason as the node path above: presence is
    // per field, not gated on `version`.
    push_info_attrs(
        &mut elem,
        info.version(),
        info.milli_timestamp(),
        info.changeset(),
        info.uid(),
        info.user().and_then(Result::ok),
    );

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
            push_attribute_escaped(&mut member, "role", m.role().unwrap_or(""));
            writer.write_event(Event::Empty(member))?;
        }
        write_borrowed_tags_xml(writer, tags)?;
        writer.write_event(Event::End(BytesEnd::new("relation")))?;
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

    fn make_test_dir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pbfhogg_osc_write_test_{suffix}"));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn write_osc_gz(dir: &Path, filename: &str, xml: &[u8]) -> crate::BoxResult<PathBuf> {
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
    fn control_chars_in_tag_values_roundtrip() -> crate::BoxResult<()> {
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
        let xml_str = std::str::from_utf8(&xml)?;
        assert!(xml_str.contains("&#10;"), "newline not escaped: {xml_str}");
        assert!(xml_str.contains("&#9;"), "tab not escaped: {xml_str}");
        assert!(xml_str.contains("&#13;"), "CR not escaped: {xml_str}");
        assert!(!xml_str.contains('\n'), "raw newline in output: {xml_str}");
        assert!(!xml_str.contains('\t'), "raw tab in output: {xml_str}");
        assert!(!xml_str.contains('\r'), "raw CR in output: {xml_str}");

        // Parse-side roundtrip: the value comes back byte-identical.
        let dir = make_test_dir("tag_values");
        let path = write_osc_gz(&dir, "roundtrip.osc.gz", &xml)?;
        let overlay = parse_osc_file(&path)?;
        let parsed = overlay.get_node(100).ok_or("node 100 missing")?;
        let tags: Vec<(&str, &str)> = parsed.tags().collect();
        assert_eq!(tags, vec![("inscription", value)]);

        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// Same guarantee for relation member roles (the other user-data
    /// attribute), via the owned-relation writer.
    #[test]
    fn control_chars_in_member_roles_roundtrip() -> crate::BoxResult<()> {
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

        let xml_str = std::str::from_utf8(&xml)?;
        assert!(xml_str.contains("&#10;"), "newline not escaped: {xml_str}");
        assert!(!xml_str.contains('\n'), "raw newline in output: {xml_str}");

        let dir = make_test_dir("member_roles");
        let path = write_osc_gz(&dir, "roundtrip.osc.gz", &xml)?;
        let overlay = parse_osc_file(&path)?;
        let parsed = overlay.get_relation(200).ok_or("relation 200 missing")?;
        let members: Vec<_> = parsed.members().collect();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].2, role);

        std::fs::remove_dir_all(&dir)?;
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
    fn borrowed_way_xml(
        meta: &crate::write::block_builder::Metadata<'_>,
    ) -> crate::BoxResult<String> {
        let mut bb = crate::write::block_builder::BlockBuilder::new();
        bb.add_way(7, [("highway", "residential")], &[1, 2], Some(meta));
        let bytes = bb
            .take()?
            .ok_or("block builder produced no block")?
            .to_vec();
        let block = crate::PrimitiveBlock::from_vec(bytes)?;

        let mut writer = Writer::new(Vec::new());
        let mut coord_buf = String::new();
        for element in block.elements() {
            write_element_xml(&mut writer, &element, &mut coord_buf)?;
        }
        Ok(String::from_utf8(writer.into_inner())?)
    }

    /// Zero-valued metadata is legitimate - `uid="0"` is the anonymous mapper
    /// that old OSM data is full of - and must survive into the OSC. The
    /// writer used to gate each attribute on `> 0`, so a zero changeset or uid
    /// vanished, and an element whose metadata was all zeros shipped with no
    /// metadata attributes at all. The parse side then recorded no metadata
    /// present and apply-changes wrote the element without an info block,
    /// breaking the documented lossless `--format osc` roundtrip.
    #[test]
    fn zero_valued_metadata_is_emitted_not_dropped() -> crate::BoxResult<()> {
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
    fn nonzero_metadata_still_roundtrips_through_the_writer() -> crate::BoxResult<()> {
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
        // and date_granularity (19) are left at their protobuf defaults of
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
    ) -> crate::BoxResult<String> {
        let block = crate::PrimitiveBlock::from_vec(handmade_way_block(
            version, timestamp, changeset, uid,
        ))?;
        let mut writer = Writer::new(Vec::new());
        let mut coord_buf = String::new();
        for element in block.elements() {
            write_element_xml(&mut writer, &element, &mut coord_buf)?;
        }
        Ok(String::from_utf8(writer.into_inner())?)
    }

    /// The emitter itself must be able to produce the ordinary shape, or the
    /// absence test below would be satisfied by a fixture that decodes to
    /// nothing at all.
    #[test]
    fn handmade_emitter_produces_a_readable_way_with_full_metadata() -> crate::BoxResult<()> {
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
    fn metadata_without_version_is_not_discarded() -> crate::BoxResult<()> {
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
    fn empty_info_emits_no_metadata_attributes() -> crate::BoxResult<()> {
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
}
