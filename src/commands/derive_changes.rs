//! Generate an OSC diff from two PBF snapshots. Equivalent to `osmium derive-changes`.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use flate2::write::GzEncoder;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

use super::owned_elements::{
    from_decimicro, format_coord, nodes_equal, read_elements, relations_equal, take_node,
    take_relation, take_way, ways_equal, OwnedNode, OwnedRelation, OwnedWay,
};
use crate::MemberType;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Statistics from a derive-changes operation.
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
/// Reads both files into memory, performs a merge-join by (type, id),
/// and writes differences as gzipped OsmChange XML.
#[hotpath::measure]
pub fn derive_changes(
    old_path: &Path,
    new_path: &Path,
    output: &Path,
    direct_io: bool,
) -> Result<DeriveChangesStats> {
    let mut old = read_elements(old_path, direct_io)?;
    let mut new = read_elements(new_path, direct_io)?;

    // Ensure sorted by ID
    old.nodes.sort_by_key(|n| n.id);
    old.ways.sort_by_key(|w| w.id);
    old.relations.sort_by_key(|r| r.id);
    new.nodes.sort_by_key(|n| n.id);
    new.ways.sort_by_key(|w| w.id);
    new.relations.sort_by_key(|r| r.id);

    // Collect changes
    let mut creates = Changes::new();
    let mut modifies = Changes::new();
    let mut deletes = Changes::new();

    merge_join_nodes(&old.nodes, &new.nodes, &mut creates, &mut modifies, &mut deletes);
    merge_join_ways(&old.ways, &new.ways, &mut creates, &mut modifies, &mut deletes);
    merge_join_relations(
        &old.relations,
        &new.relations,
        &mut creates,
        &mut modifies,
        &mut deletes,
    );

    let stats = DeriveChangesStats {
        creates: (creates.nodes.len() + creates.ways.len() + creates.relations.len()) as u64,
        modifies: (modifies.nodes.len() + modifies.ways.len() + modifies.relations.len()) as u64,
        deletes: (deletes.nodes.len() + deletes.ways.len() + deletes.relations.len()) as u64,
    };

    write_osc(output, &creates, &modifies, &deletes)?;

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
// Merge-join
// ---------------------------------------------------------------------------

fn merge_join_nodes(
    old: &[OwnedNode],
    new: &[OwnedNode],
    creates: &mut Changes,
    modifies: &mut Changes,
    deletes: &mut Changes,
) {
    let mut oi = 0;
    let mut ni = 0;

    while oi < old.len() && ni < new.len() {
        match old[oi].id.cmp(&new[ni].id) {
            std::cmp::Ordering::Less => {
                // In old only → delete
                deletes.nodes.push(take_node(&old[oi]));
                oi += 1;
            }
            std::cmp::Ordering::Greater => {
                // In new only → create
                creates.nodes.push(take_node(&new[ni]));
                ni += 1;
            }
            std::cmp::Ordering::Equal => {
                // In both → check if modified
                if !nodes_equal(&old[oi], &new[ni]) {
                    modifies.nodes.push(take_node(&new[ni]));
                }
                oi += 1;
                ni += 1;
            }
        }
    }

    // Remaining old → deletes
    for o in &old[oi..] {
        deletes.nodes.push(take_node(o));
    }
    // Remaining new → creates
    for n in &new[ni..] {
        creates.nodes.push(take_node(n));
    }
}

fn merge_join_ways(
    old: &[OwnedWay],
    new: &[OwnedWay],
    creates: &mut Changes,
    modifies: &mut Changes,
    deletes: &mut Changes,
) {
    let mut oi = 0;
    let mut ni = 0;

    while oi < old.len() && ni < new.len() {
        match old[oi].id.cmp(&new[ni].id) {
            std::cmp::Ordering::Less => {
                deletes.ways.push(take_way(&old[oi]));
                oi += 1;
            }
            std::cmp::Ordering::Greater => {
                creates.ways.push(take_way(&new[ni]));
                ni += 1;
            }
            std::cmp::Ordering::Equal => {
                if !ways_equal(&old[oi], &new[ni]) {
                    modifies.ways.push(take_way(&new[ni]));
                }
                oi += 1;
                ni += 1;
            }
        }
    }

    for o in &old[oi..] {
        deletes.ways.push(take_way(o));
    }
    for n in &new[ni..] {
        creates.ways.push(take_way(n));
    }
}

fn merge_join_relations(
    old: &[OwnedRelation],
    new: &[OwnedRelation],
    creates: &mut Changes,
    modifies: &mut Changes,
    deletes: &mut Changes,
) {
    let mut oi = 0;
    let mut ni = 0;

    while oi < old.len() && ni < new.len() {
        match old[oi].id.cmp(&new[ni].id) {
            std::cmp::Ordering::Less => {
                deletes.relations.push(take_relation(&old[oi]));
                oi += 1;
            }
            std::cmp::Ordering::Greater => {
                creates.relations.push(take_relation(&new[ni]));
                ni += 1;
            }
            std::cmp::Ordering::Equal => {
                if !relations_equal(&old[oi], &new[ni]) {
                    modifies.relations.push(take_relation(&new[ni]));
                }
                oi += 1;
                ni += 1;
            }
        }
    }

    for o in &old[oi..] {
        deletes.relations.push(take_relation(o));
    }
    for n in &new[ni..] {
        creates.relations.push(take_relation(n));
    }
}

// ---------------------------------------------------------------------------
// OSC XML writer
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn write_osc(
    output: &Path,
    creates: &Changes,
    modifies: &Changes,
    deletes: &Changes,
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
            write_delete_node(&mut writer, node)?;
        }
        for way in &deletes.ways {
            write_delete_element(&mut writer, "way", way.id, way.version)?;
        }
        for rel in &deletes.relations {
            write_delete_element(&mut writer, "relation", rel.id, rel.version)?;
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
    let lat_str = format_coord(from_decimicro(node.decimicro_lat));
    let lon_str = format_coord(from_decimicro(node.decimicro_lon));
    elem.push_attribute(("id", id_str.as_str()));
    elem.push_attribute(("lat", lat_str.as_str()));
    elem.push_attribute(("lon", lon_str.as_str()));
    if let Some(v) = node.version {
        let v_str = v.to_string();
        elem.push_attribute(("version", v_str.as_str()));
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
    if let Some(v) = way.version {
        let v_str = v.to_string();
        elem.push_attribute(("version", v_str.as_str()));
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
    if let Some(v) = rel.version {
        let v_str = v.to_string();
        elem.push_attribute(("version", v_str.as_str()));
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
                MemberType::Unknown(_) => "node", // fallback for unrecognized types
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

fn write_delete_node<W: Write>(writer: &mut Writer<W>, node: &OwnedNode) -> Result<()> {
    let mut elem = BytesStart::new("node");
    let id_str = node.id.to_string();
    elem.push_attribute(("id", id_str.as_str()));
    if let Some(v) = node.version {
        let v_str = v.to_string();
        elem.push_attribute(("version", v_str.as_str()));
    }
    writer.write_event(Event::Empty(elem))?;
    Ok(())
}

fn write_delete_element<W: Write>(
    writer: &mut Writer<W>,
    tag_name: &str,
    id: i64,
    version: Option<i32>,
) -> Result<()> {
    let mut elem = BytesStart::new(tag_name);
    let id_str = id.to_string();
    elem.push_attribute(("id", id_str.as_str()));
    if let Some(v) = version {
        let v_str = v.to_string();
        elem.push_attribute(("version", v_str.as_str()));
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
