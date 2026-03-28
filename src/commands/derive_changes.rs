//! Generate an OSC diff from two PBF snapshots. Equivalent to `osmium derive-changes`.
//!
//! Streams through both files in constant memory using [`StreamingBlocks`] cursors.
//! Requires both inputs to declare `Sort.Type_then_ID`.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use flate2::write::GzEncoder;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

use super::elements_xml::{
    from_decimicro, format_coord, OwnedMetadata, OwnedNode, OwnedRelation, OwnedWay,
};
use super::stream_merge::{merge_join_phase, MergeJoinAction, StreamingBlocks};
use super::{require_sorted, Result};
use crate::{ElementReader, MemberType};

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Statistics from a derive-changes operation.
#[derive(Debug)]
pub struct DeriveChangesStats {
    pub creates: u64,
    pub modifies: u64,
    pub deletes: u64,
}

impl DeriveChangesStats {
    pub fn print_summary(&self) {
        let total = self.creates + self.modifies + self.deletes;
        eprintln!(
            "{total} changes: {} creates, {} modifies, {} deletes",
            self.creates, self.modifies, self.deletes,
        );
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Generate an OSC diff from two sorted PBF snapshots.
///
/// Streams through both files using pipelined block iterators and performs
/// a merge-join by (type, id). Changes are buffered by action type and
/// written as gzipped OsmChange XML. Memory is bounded by the number of
/// changed elements, not total input size.
///
/// Requires both inputs to declare `Sort.Type_then_ID` — returns an
/// actionable error if either is unsorted.
#[hotpath::measure]
pub fn derive_changes(
    old_path: &Path,
    new_path: &Path,
    output: &Path,
    direct_io: bool,
    increment_version: bool,
    update_timestamp: bool,
) -> Result<DeriveChangesStats> {
    // Check sorted headers before opening sequential readers.
    {
        let old_reader = ElementReader::open(old_path, direct_io)?;
        let new_reader = ElementReader::open(new_path, direct_io)?;
        require_sorted(old_reader.header(), old_path, "Old PBF")?;
        require_sorted(new_reader.header(), new_path, "New PBF")?;
    }

    // Sequential readers to avoid 2× PrimitiveBlock cross-thread retention.
    crate::debug::emit_marker("DERIVECHANGES_SCAN_START");
    let mut old_src = StreamingBlocks::new_sequential(old_path, direct_io)?;
    let mut new_src = StreamingBlocks::new_sequential(new_path, direct_io)?;

    // Collect changes by action type.
    let mut creates = Changes::new();
    let mut modifies = Changes::new();
    let mut deletes = Changes::new();

    // Phase 1: Nodes
    {
        let (mut ob, mut nb) = (Vec::new(), Vec::new());
        collect_changes_phase(
            &mut old_src, &mut ob, &mut new_src, &mut nb,
            &mut creates.nodes, &mut modifies.nodes, &mut deletes.nodes,
        )?;
    }

    // Phase 2: Ways
    {
        let (mut ob, mut nb) = (Vec::new(), Vec::new());
        collect_changes_phase(
            &mut old_src, &mut ob, &mut new_src, &mut nb,
            &mut creates.ways, &mut modifies.ways, &mut deletes.ways,
        )?;
    }

    // Phase 3: Relations
    {
        let (mut ob, mut nb) = (Vec::new(), Vec::new());
        collect_changes_phase(
            &mut old_src, &mut ob, &mut new_src, &mut nb,
            &mut creates.relations, &mut modifies.relations, &mut deletes.relations,
        )?;
    }

    crate::debug::emit_marker("DERIVECHANGES_SCAN_END");

    let stats = DeriveChangesStats {
        creates: (creates.nodes.len() + creates.ways.len() + creates.relations.len()) as u64,
        modifies: (modifies.nodes.len() + modifies.ways.len() + modifies.relations.len()) as u64,
        deletes: (deletes.nodes.len() + deletes.ways.len() + deletes.relations.len()) as u64,
    };

    crate::debug::emit_marker("DERIVECHANGES_WRITE_START");
    write_osc(output, &creates, &modifies, &deletes, increment_version, update_timestamp)?;
    crate::debug::emit_marker("DERIVECHANGES_WRITE_END");

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("derivechanges_creates", stats.creates as i64);
        crate::debug::emit_counter("derivechanges_modifies", stats.modifies as i64);
        crate::debug::emit_counter("derivechanges_deletes", stats.deletes as i64);
    }

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Change collection
// ---------------------------------------------------------------------------

struct Changes {
    nodes: Vec<OwnedNode>,
    ways: Vec<OwnedWay>,
    relations: Vec<OwnedRelation>,
}

impl Changes {
    fn new() -> Self {
        Self {
            nodes: Vec::new(),
            ways: Vec::new(),
            relations: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.ways.is_empty() && self.relations.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Change collection via shared merge-join
// ---------------------------------------------------------------------------

/// Run one type phase, collecting creates/modifies/deletes into Vecs.
fn collect_changes_phase<T: super::stream_merge::MergeJoinElement + Clone>(
    old_src: &mut StreamingBlocks,
    old_buf: &mut Vec<T>,
    new_src: &mut StreamingBlocks,
    new_buf: &mut Vec<T>,
    creates: &mut Vec<T>,
    modifies: &mut Vec<T>,
    deletes: &mut Vec<T>,
) -> Result<()> {
    merge_join_phase(old_src, old_buf, new_src, new_buf, |action| {
        match action {
            MergeJoinAction::OldOnly(o) => deletes.push(o.clone()),
            MergeJoinAction::NewOnly(n) => creates.push(n.clone()),
            MergeJoinAction::Modified(_o, n) => modifies.push(n.clone()),
            MergeJoinAction::Equal(_) => {}
        }
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// OSC XML writer
// ---------------------------------------------------------------------------

fn write_osc(
    output: &Path,
    creates: &Changes,
    modifies: &Changes,
    deletes: &Changes,
    increment_version: bool,
    update_timestamp: bool,
) -> Result<()> {
    let file = File::create(output)?;
    let gz = GzEncoder::new(io::BufWriter::new(file), flate2::Compression::fast());
    let mut writer = Writer::new_with_indent(gz, b' ', 2);

    // XML declaration
    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
    writer.write_event(Event::Text(BytesText::new("\n")))?;

    // <osmChange version="0.6">
    let mut root = BytesStart::new("osmChange");
    root.push_attribute(("version", "0.6"));
    writer.write_event(Event::Start(root))?;

    // <create>
    if !creates.is_empty() {
        writer.write_event(Event::Start(BytesStart::new("create")))?;
        for node in &creates.nodes {
            write_node(&mut writer, node)?;
        }
        for way in &creates.ways {
            write_way(&mut writer, way)?;
        }
        for rel in &creates.relations {
            write_relation(&mut writer, rel)?;
        }
        writer.write_event(Event::End(BytesEnd::new("create")))?;
    }

    // <modify>
    if !modifies.is_empty() {
        writer.write_event(Event::Start(BytesStart::new("modify")))?;
        for node in &modifies.nodes {
            write_node(&mut writer, node)?;
        }
        for way in &modifies.ways {
            write_way(&mut writer, way)?;
        }
        for rel in &modifies.relations {
            write_relation(&mut writer, rel)?;
        }
        writer.write_event(Event::End(BytesEnd::new("modify")))?;
    }

    // <delete>
    if !deletes.is_empty() {
        writer.write_event(Event::Start(BytesStart::new("delete")))?;
        for node in &deletes.nodes {
            write_delete_element(&mut writer, "node", node.id, node.metadata.as_ref(), increment_version, update_timestamp)?;
        }
        for way in &deletes.ways {
            write_delete_element(&mut writer, "way", way.id, way.metadata.as_ref(), increment_version, update_timestamp)?;
        }
        for rel in &deletes.relations {
            write_delete_element(&mut writer, "relation", rel.id, rel.metadata.as_ref(), increment_version, update_timestamp)?;
        }
        writer.write_event(Event::End(BytesEnd::new("delete")))?;
    }

    // </osmChange>
    writer.write_event(Event::End(BytesEnd::new("osmChange")))?;

    let gz = writer.into_inner();
    gz.finish()?;
    Ok(())
}

fn write_node<W: Write>(writer: &mut Writer<W>, node: &OwnedNode) -> Result<()> {
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
        write_tags(writer, &node.tags)?;
        writer.write_event(Event::End(BytesEnd::new("node")))?;
    }
    Ok(())
}

fn write_way<W: Write>(writer: &mut Writer<W>, way: &OwnedWay) -> Result<()> {
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
        write_tags(writer, &way.tags)?;
        writer.write_event(Event::End(BytesEnd::new("way")))?;
    }
    Ok(())
}

fn write_relation<W: Write>(writer: &mut Writer<W>, rel: &OwnedRelation) -> Result<()> {
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
            let type_str = match m.id.member_type() {
                MemberType::Node => "node",
                MemberType::Way => "way",
                MemberType::Relation => "relation",
                // Unknown member types from newer PBF producers — write as "node"
                // since OSC XML has no "unknown" type value. The protobuf enum
                // only defines NODE/WAY/RELATION and has never been extended.
                MemberType::Unknown(_) => "node",
            };
            let id_str = m.id.id().to_string();
            member.push_attribute(("type", type_str));
            member.push_attribute(("ref", id_str.as_str()));
            member.push_attribute(("role", m.role.as_str()));
            writer.write_event(Event::Empty(member))?;
        }
        write_tags(writer, &rel.tags)?;
        writer.write_event(Event::End(BytesEnd::new("relation")))?;
    }
    Ok(())
}

fn write_delete_element<W: Write>(
    writer: &mut Writer<W>,
    tag_name: &str,
    id: i64,
    metadata: Option<&OwnedMetadata>,
    increment_version: bool,
    update_timestamp: bool,
) -> Result<()> {
    let mut elem = BytesStart::new(tag_name);
    let id_str = id.to_string();
    elem.push_attribute(("id", id_str.as_str()));
    if let Some(meta) = metadata {
        let version = if increment_version {
            meta.version.saturating_add(1)
        } else {
            meta.version
        };
        let v_str = version.to_string();
        elem.push_attribute(("version", v_str.as_str()));
    }
    if update_timestamp {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let ts = super::format_epoch_secs(now.as_secs());
        elem.push_attribute(("timestamp", ts.as_str()));
    }
    writer.write_event(Event::Empty(elem))?;
    Ok(())
}

fn write_tags<W: Write>(writer: &mut Writer<W>, tags: &[(String, String)]) -> Result<()> {
    for (k, v) in tags {
        let mut tag = BytesStart::new("tag");
        tag.push_attribute(("k", k.as_str()));
        tag.push_attribute(("v", v.as_str()));
        writer.write_event(Event::Empty(tag))?;
    }
    Ok(())
}
