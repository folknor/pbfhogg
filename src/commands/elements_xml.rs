//! XML-oriented owned element types for derive_changes, diff, merge_changes, and tags_filter_osc.
//!
//! Metadata fields are String-typed for direct XML attribute output.
//! See `elements_pbf` for the PBF-oriented variant with native types.

use quick_xml::events::{BytesEnd, BytesStart, Event};
use quick_xml::Writer;

use crate::MemberId;

// ---------------------------------------------------------------------------
// Owned element types — Vec fields are not converted to Box<[T]> because these
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
            elem.push_attribute(("user", self.user.as_str()));
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
) -> super::Result<()> {
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
) -> super::Result<()> {
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
) -> super::Result<()> {
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
            member.push_attribute(("role", m.role.as_str()));
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
) -> super::Result<()> {
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
) -> super::Result<()> {
    for (k, v) in tags {
        let mut tag = BytesStart::new("tag");
        tag.push_attribute(("k", k.as_str()));
        tag.push_attribute(("v", v.as_str()));
        writer.write_event(Event::Empty(tag))?;
    }
    Ok(())
}
