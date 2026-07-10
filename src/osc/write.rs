//! XML-oriented owned element types for derive_changes, diff, merge_changes, and tags_filter_osc.
//!
//! Metadata fields are String-typed for direct XML attribute output.
//! See `crate::owned` for the PBF-oriented variant with native types.

use quick_xml::Writer;
use quick_xml::events::{BytesEnd, BytesStart, Event};

use crate::MemberId;

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
// Coordinate conversion
// ---------------------------------------------------------------------------

pub(crate) fn from_decimicro(d: i32) -> f64 {
    f64::from(d) / 1e7
}

// ---------------------------------------------------------------------------
// Coordinate formatting
// ---------------------------------------------------------------------------

/// Format a coordinate, stripping unnecessary trailing zeros.
/// Writes directly into a provided buffer to avoid intermediate allocations.
pub(crate) fn format_coord(buf: &mut String, deg: f64) {
    use std::fmt::Write;
    buf.clear();
    // Use 7 decimal places (matches decimicrodegree precision)
    // write! to String is infallible (String::write_str never fails)
    write!(buf, "{deg:.7}").ok();
    let trimmed = buf.trim_end_matches('0').trim_end_matches('.');
    buf.truncate(trimmed.len());
}

// ---------------------------------------------------------------------------
// OSC XML element writing (shared by derive_changes and merge_changes)
// ---------------------------------------------------------------------------

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
            let type_str = match m.id {
                MemberId::Node(_) => "node",
                MemberId::Way(_) => "way",
                MemberId::Relation(_) => "relation",
                MemberId::Unknown(_, _) => "node",
            };
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
/// Each attribute is emitted only when its value is meaningful (version and
/// changeset and uid positive, timestamp nonzero, user non-empty), matching
/// what osmium's OSC writer produces. Full emission (not just version)
/// keeps the derive -> apply roundtrip metadata-lossless now that
/// apply-changes carries OSC metadata into its output.
fn push_info_attrs(
    elem: &mut BytesStart<'_>,
    version: i32,
    milli_timestamp: i64,
    changeset: i64,
    uid: i32,
    user: Option<&str>,
) {
    if version > 0 {
        let v_str = version.to_string();
        elem.push_attribute(("version", v_str.as_str()));
    }
    if milli_timestamp > 0 {
        let ts = crate::commands::format_epoch_secs((milli_timestamp / 1000).cast_unsigned());
        elem.push_attribute(("timestamp", ts.as_str()));
    }
    if changeset > 0 {
        let c_str = changeset.to_string();
        elem.push_attribute(("changeset", c_str.as_str()));
    }
    if uid > 0 {
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
        push_info_attrs(
            &mut elem,
            info.version(),
            info.milli_timestamp(),
            info.changeset(),
            info.uid(),
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
    if let Some(v) = info.version() {
        push_info_attrs(
            &mut elem,
            v,
            info.milli_timestamp().unwrap_or(0),
            info.changeset().unwrap_or(0),
            info.uid().unwrap_or(0),
            info.user().and_then(Result::ok),
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

fn write_borrowed_way_xml<W: std::io::Write>(
    writer: &mut Writer<W>,
    way: &crate::elements::Way<'_>,
) -> crate::BoxResult<()> {
    let mut elem = BytesStart::new("way");
    let id_str = way.id().to_string();
    elem.push_attribute(("id", id_str.as_str()));
    let info = way.info();
    if let Some(v) = info.version() {
        push_info_attrs(
            &mut elem,
            v,
            info.milli_timestamp().unwrap_or(0),
            info.changeset().unwrap_or(0),
            info.uid().unwrap_or(0),
            info.user().and_then(Result::ok),
        );
    }

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
    if let Some(v) = info.version() {
        push_info_attrs(
            &mut elem,
            v,
            info.milli_timestamp().unwrap_or(0),
            info.changeset().unwrap_or(0),
            info.uid().unwrap_or(0),
            info.user().and_then(Result::ok),
        );
    }

    let mut members = rel.members().peekable();
    let mut tags = rel.tags().peekable();
    if members.peek().is_none() && tags.peek().is_none() {
        writer.write_event(Event::Empty(elem))?;
    } else {
        writer.write_event(Event::Start(elem))?;
        for m in members {
            let mut member = BytesStart::new("member");
            let type_str = match m.id {
                crate::MemberId::Node(_) => "node",
                crate::MemberId::Way(_) => "way",
                crate::MemberId::Relation(_) => "relation",
                crate::MemberId::Unknown(_, _) => "node",
            };
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
}
