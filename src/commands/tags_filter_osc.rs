//! Filter OSC files by tag expressions while preserving all delete actions.

use std::fs::File;
use std::io::{self, BufReader, Write};
use std::path::Path;

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};
use quick_xml::name::QName;
use quick_xml::{Reader, Writer};

use super::elements_xml::{format_coord, from_decimicro, OwnedMember, OwnedNode, OwnedRelation, OwnedWay};
use crate::tag_expr::{tag_matches, parse_expressions, Expression};
use super::Result;
use crate::{MemberId, MemberType};

fn matches_any(expressions: &[Expression], tags: &[(String, String)], element_type: char) -> bool {
    for (key, value) in tags {
        for expr in expressions {
            let type_ok = match element_type {
                'n' => expr.type_filter.nodes,
                'w' => expr.type_filter.ways,
                'r' => expr.type_filter.relations,
                _ => false,
            };
            if type_ok && tag_matches(&expr.matcher, key, value) {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// OSC model used by this command
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    None,
    Create,
    Modify,
    Delete,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ElemKind {
    Node,
    Way,
    Relation,
}

struct CurrentElem {
    kind: ElemKind,
    id: i64,
    decimicro_lat: i32,
    decimicro_lon: i32,
    tags: Vec<(String, String)>,
    refs: Vec<i64>,
    members: Vec<OwnedMember>,
}

impl CurrentElem {
    fn new(kind: ElemKind, id: i64) -> Self {
        Self {
            kind,
            id,
            decimicro_lat: 0,
            decimicro_lon: 0,
            tags: Vec::new(),
            refs: Vec::new(),
            members: Vec::new(),
        }
    }
}

struct FilteredOsc {
    create_nodes: Vec<OwnedNode>,
    create_ways: Vec<OwnedWay>,
    create_relations: Vec<OwnedRelation>,
    modify_nodes: Vec<OwnedNode>,
    modify_ways: Vec<OwnedWay>,
    modify_relations: Vec<OwnedRelation>,
    delete_node_ids: Vec<i64>,
    delete_way_ids: Vec<i64>,
    delete_relation_ids: Vec<i64>,
}

impl FilteredOsc {
    fn new() -> Self {
        Self {
            create_nodes: Vec::new(),
            create_ways: Vec::new(),
            create_relations: Vec::new(),
            modify_nodes: Vec::new(),
            modify_ways: Vec::new(),
            modify_relations: Vec::new(),
            delete_node_ids: Vec::new(),
            delete_way_ids: Vec::new(),
            delete_relation_ids: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct TagsFilterOscStats {
    pub creates_in: u64,
    pub creates_out: u64,
    pub modifies_in: u64,
    pub modifies_out: u64,
    pub deletes_in: u64,
    pub deletes_out: u64,
}

impl TagsFilterOscStats {
    pub fn print_summary(&self) {
        eprintln!(
            "OSC filtered: create {}/{} kept, modify {}/{} kept, delete {}/{} preserved",
            self.creates_out,
            self.creates_in,
            self.modifies_out,
            self.modifies_in,
            self.deletes_out,
            self.deletes_in,
        );
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Filter an OSC file by tag expressions.
///
/// - `create` / `modify`: only matching elements are kept.
/// - `delete`: all deletes are forwarded unconditionally.
#[hotpath::measure]
pub fn tags_filter_osc(
    input: &Path,
    output: &Path,
    expression_strs: &[String],
) -> Result<TagsFilterOscStats> {
    let expressions = parse_expressions(expression_strs)?;
    crate::debug::emit_marker("TAGSFILTEROSC_START");
    let (filtered, stats) = parse_and_filter_osc(input, &expressions)?;
    write_filtered_osc(output, &filtered)?;
    crate::debug::emit_marker("TAGSFILTEROSC_END");
    Ok(stats)
}

fn parse_and_filter_osc(
    input: &Path,
    expressions: &[Expression],
) -> Result<(FilteredOsc, TagsFilterOscStats)> {
    let file = File::open(input)?;
    let decoder = GzDecoder::new(file);
    let mut reader = Reader::from_reader(BufReader::new(decoder));
    reader.config_mut().trim_text(true);

    let mut filtered = FilteredOsc::new();
    let mut stats = TagsFilterOscStats::default();
    let mut section = Section::None;
    let mut current: Option<CurrentElem> = None;
    let mut buf: Vec<u8> = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                handle_event_start_like(
                    e,
                    false,
                    &mut section,
                    &mut current,
                    &mut filtered,
                    &mut stats,
                    expressions,
                )?;
            }
            Ok(Event::Empty(ref e)) => {
                handle_event_start_like(
                    e,
                    true,
                    &mut section,
                    &mut current,
                    &mut filtered,
                    &mut stats,
                    expressions,
                )?;
            }
            Ok(Event::End(ref e)) => match e.name().as_ref() {
                b"create" | b"modify" | b"delete" => section = Section::None,
                b"node" | b"way" | b"relation" => {
                    finalize_current(section, &mut current, &mut filtered, &mut stats, expressions);
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(Box::new(e)),
        }
        buf.clear();
    }

    Ok((filtered, stats))
}

fn handle_event_start_like(
    e: &BytesStart<'_>,
    is_empty: bool,
    section: &mut Section,
    current: &mut Option<CurrentElem>,
    filtered: &mut FilteredOsc,
    stats: &mut TagsFilterOscStats,
    expressions: &[Expression],
) -> Result<()> {
    match e.name().as_ref() {
        b"create" => *section = Section::Create,
        b"modify" => *section = Section::Modify,
        b"delete" => *section = Section::Delete,
        b"node" => {
            start_element(e, *section, ElemKind::Node, current, filtered, stats)?;
            if is_empty {
                finalize_current(*section, current, filtered, stats, expressions);
            }
        }
        b"way" => {
            start_element(e, *section, ElemKind::Way, current, filtered, stats)?;
            if is_empty {
                finalize_current(*section, current, filtered, stats, expressions);
            }
        }
        b"relation" => {
            start_element(e, *section, ElemKind::Relation, current, filtered, stats)?;
            if is_empty {
                finalize_current(*section, current, filtered, stats, expressions);
            }
        }
        b"tag" => append_tag(e, current)?,
        b"nd" => append_ref(e, current)?,
        b"member" => append_member(e, current)?,
        _ => {}
    }
    Ok(())
}

fn append_tag(e: &BytesStart<'_>, current: &mut Option<CurrentElem>) -> Result<()> {
    if let Some(cur) = current.as_mut() {
        let k = attr_string(e, b"k")?;
        let v = attr_string(e, b"v")?;
        cur.tags.push((k, v));
    }
    Ok(())
}

fn append_ref(e: &BytesStart<'_>, current: &mut Option<CurrentElem>) -> Result<()> {
    if let Some(cur) = current.as_mut()
        && cur.kind == ElemKind::Way
    {
        cur.refs.push(attr_i64(e, b"ref")?);
    }
    Ok(())
}

fn append_member(e: &BytesStart<'_>, current: &mut Option<CurrentElem>) -> Result<()> {
    if let Some(cur) = current.as_mut()
        && cur.kind == ElemKind::Relation
    {
        let member_type = match attr_string(e, b"type")?.as_str() {
            "node" => MemberType::Node,
            "way" => MemberType::Way,
            "relation" => MemberType::Relation,
            other => {
                return Err(format!("unknown relation member type: '{other}'").into());
            }
        };
        let id = attr_i64(e, b"ref")?;
        let role = attr_string(e, b"role").unwrap_or_default();
        let member_id = match member_type {
            MemberType::Node => MemberId::Node(id),
            MemberType::Way => MemberId::Way(id),
            MemberType::Relation => MemberId::Relation(id),
            MemberType::Unknown(v) => MemberId::Unknown(v, id),
        };
        cur.members.push(OwnedMember { id: member_id, role });
    }
    Ok(())
}

fn start_element(
    e: &BytesStart<'_>,
    section: Section,
    kind: ElemKind,
    current: &mut Option<CurrentElem>,
    filtered: &mut FilteredOsc,
    stats: &mut TagsFilterOscStats,
) -> Result<()> {
    let id = attr_i64(e, b"id")?;

    match section {
        Section::Delete => {
            stats.deletes_in += 1;
            stats.deletes_out += 1;
            match kind {
                ElemKind::Node => filtered.delete_node_ids.push(id),
                ElemKind::Way => filtered.delete_way_ids.push(id),
                ElemKind::Relation => filtered.delete_relation_ids.push(id),
            }
            *current = None;
            return Ok(());
        }
        Section::Create => {
            stats.creates_in += 1;
        }
        Section::Modify => {
            stats.modifies_in += 1;
        }
        Section::None => {}
    }

    let mut cur = CurrentElem::new(kind, id);
    if kind == ElemKind::Node {
        let lat = attr_f64(e, b"lat").unwrap_or(0.0);
        let lon = attr_f64(e, b"lon").unwrap_or(0.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        {
            cur.decimicro_lat = (lat * 1e7).round() as i32;
            cur.decimicro_lon = (lon * 1e7).round() as i32;
        }
    }
    *current = Some(cur);
    Ok(())
}

fn finalize_current(
    section: Section,
    current: &mut Option<CurrentElem>,
    filtered: &mut FilteredOsc,
    stats: &mut TagsFilterOscStats,
    expressions: &[Expression],
) {
    let Some(cur) = current.take() else {
        return;
    };
    let type_char = match cur.kind {
        ElemKind::Node => 'n',
        ElemKind::Way => 'w',
        ElemKind::Relation => 'r',
    };
    if !matches_any(expressions, &cur.tags, type_char) {
        return;
    }

    match (section, cur.kind) {
        (Section::Create, ElemKind::Node) => {
            stats.creates_out += 1;
            filtered.create_nodes.push(OwnedNode {
                id: cur.id,
                decimicro_lat: cur.decimicro_lat,
                decimicro_lon: cur.decimicro_lon,
                tags: cur.tags,
                metadata: None,
            });
        }
        (Section::Create, ElemKind::Way) => {
            stats.creates_out += 1;
            filtered.create_ways.push(OwnedWay {
                id: cur.id,
                tags: cur.tags,
                refs: cur.refs,
                metadata: None,
            });
        }
        (Section::Create, ElemKind::Relation) => {
            stats.creates_out += 1;
            filtered.create_relations.push(OwnedRelation {
                id: cur.id,
                tags: cur.tags,
                members: cur.members,
                metadata: None,
            });
        }
        (Section::Modify, ElemKind::Node) => {
            stats.modifies_out += 1;
            filtered.modify_nodes.push(OwnedNode {
                id: cur.id,
                decimicro_lat: cur.decimicro_lat,
                decimicro_lon: cur.decimicro_lon,
                tags: cur.tags,
                metadata: None,
            });
        }
        (Section::Modify, ElemKind::Way) => {
            stats.modifies_out += 1;
            filtered.modify_ways.push(OwnedWay {
                id: cur.id,
                tags: cur.tags,
                refs: cur.refs,
                metadata: None,
            });
        }
        (Section::Modify, ElemKind::Relation) => {
            stats.modifies_out += 1;
            filtered.modify_relations.push(OwnedRelation {
                id: cur.id,
                tags: cur.tags,
                members: cur.members,
                metadata: None,
            });
        }
        _ => {}
    }
}

fn attr_i64(e: &BytesStart<'_>, name: &[u8]) -> Result<i64> {
    for attr_result in e.attributes() {
        let attr = attr_result?;
        if attr.key == QName(name) {
            let val = std::str::from_utf8(&attr.value)?;
            return Ok(val.parse::<i64>()?);
        }
    }
    Err(format!("missing attribute '{}'", String::from_utf8_lossy(name)).into())
}

fn attr_f64(e: &BytesStart<'_>, name: &[u8]) -> Result<f64> {
    for attr_result in e.attributes() {
        let attr = attr_result?;
        if attr.key == QName(name) {
            let val = std::str::from_utf8(&attr.value)?;
            return Ok(val.parse::<f64>()?);
        }
    }
    Err(format!("missing attribute '{}'", String::from_utf8_lossy(name)).into())
}

fn attr_string(e: &BytesStart<'_>, name: &[u8]) -> Result<String> {
    for attr_result in e.attributes() {
        let attr = attr_result?;
        if attr.key == QName(name) {
            let val = attr.unescape_value()?;
            return Ok(val.into_owned());
        }
    }
    Err(format!("missing attribute '{}'", String::from_utf8_lossy(name)).into())
}

fn write_filtered_osc(output: &Path, filtered: &FilteredOsc) -> Result<()> {
    let file = File::create(output)?;
    let gz = GzEncoder::new(io::BufWriter::new(file), flate2::Compression::fast());
    let mut writer = Writer::new_with_indent(gz, b' ', 2);

    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
    let mut root = BytesStart::new("osmChange");
    root.push_attribute(("version", "0.6"));
    writer.write_event(Event::Start(root))?;

    if !filtered.create_nodes.is_empty() || !filtered.create_ways.is_empty() || !filtered.create_relations.is_empty() {
        writer.write_event(Event::Start(BytesStart::new("create")))?;
        for node in &filtered.create_nodes {
            write_node(&mut writer, node)?;
        }
        for way in &filtered.create_ways {
            write_way(&mut writer, way)?;
        }
        for rel in &filtered.create_relations {
            write_relation(&mut writer, rel)?;
        }
        writer.write_event(Event::End(BytesEnd::new("create")))?;
    }

    if !filtered.modify_nodes.is_empty() || !filtered.modify_ways.is_empty() || !filtered.modify_relations.is_empty() {
        writer.write_event(Event::Start(BytesStart::new("modify")))?;
        for node in &filtered.modify_nodes {
            write_node(&mut writer, node)?;
        }
        for way in &filtered.modify_ways {
            write_way(&mut writer, way)?;
        }
        for rel in &filtered.modify_relations {
            write_relation(&mut writer, rel)?;
        }
        writer.write_event(Event::End(BytesEnd::new("modify")))?;
    }

    if !filtered.delete_node_ids.is_empty() || !filtered.delete_way_ids.is_empty() || !filtered.delete_relation_ids.is_empty() {
        writer.write_event(Event::Start(BytesStart::new("delete")))?;
        for id in &filtered.delete_node_ids {
            write_delete_id(&mut writer, "node", *id)?;
        }
        for id in &filtered.delete_way_ids {
            write_delete_id(&mut writer, "way", *id)?;
        }
        for id in &filtered.delete_relation_ids {
            write_delete_id(&mut writer, "relation", *id)?;
        }
        writer.write_event(Event::End(BytesEnd::new("delete")))?;
    }

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

    if node.tags.is_empty() {
        writer.write_event(Event::Empty(elem))?;
    } else {
        writer.write_event(Event::Start(elem))?;
        for (k, v) in &node.tags {
            let mut tag = BytesStart::new("tag");
            tag.push_attribute(("k", k.as_str()));
            tag.push_attribute(("v", v.as_str()));
            writer.write_event(Event::Empty(tag))?;
        }
        writer.write_event(Event::End(BytesEnd::new("node")))?;
    }
    Ok(())
}

fn write_way<W: Write>(writer: &mut Writer<W>, way: &OwnedWay) -> Result<()> {
    let mut elem = BytesStart::new("way");
    let id_str = way.id.to_string();
    elem.push_attribute(("id", id_str.as_str()));

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
        for (k, v) in &way.tags {
            let mut tag = BytesStart::new("tag");
            tag.push_attribute(("k", k.as_str()));
            tag.push_attribute(("v", v.as_str()));
            writer.write_event(Event::Empty(tag))?;
        }
        writer.write_event(Event::End(BytesEnd::new("way")))?;
    }
    Ok(())
}

fn write_relation<W: Write>(writer: &mut Writer<W>, rel: &OwnedRelation) -> Result<()> {
    let mut elem = BytesStart::new("relation");
    let id_str = rel.id.to_string();
    elem.push_attribute(("id", id_str.as_str()));

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
                MemberType::Unknown(_) => "node",
            };
            let ref_str = m.id.id().to_string();
            member.push_attribute(("type", type_str));
            member.push_attribute(("ref", ref_str.as_str()));
            member.push_attribute(("role", m.role.as_str()));
            writer.write_event(Event::Empty(member))?;
        }
        for (k, v) in &rel.tags {
            let mut tag = BytesStart::new("tag");
            tag.push_attribute(("k", k.as_str()));
            tag.push_attribute(("v", v.as_str()));
            writer.write_event(Event::Empty(tag))?;
        }
        writer.write_event(Event::End(BytesEnd::new("relation")))?;
    }
    Ok(())
}

fn write_delete_id<W: Write>(writer: &mut Writer<W>, tag_name: &str, id: i64) -> Result<()> {
    let mut elem = BytesStart::new(tag_name);
    let id_str = id.to_string();
    elem.push_attribute(("id", id_str.as_str()));
    writer.write_event(Event::Empty(elem))?;
    Ok(())
}
