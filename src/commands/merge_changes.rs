//! Merge multiple OSC files into a single OSC stream.
//!
//! Default mode preserves the full change stream in input order.
//! `--simplify` keeps only the last change per object (type + id).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, Seek, Write};
use std::path::Path;

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};
use quick_xml::name::QName;
use quick_xml::{Reader, Writer};

use super::elements_xml::{
    format_coord, from_decimicro, OwnedMember, OwnedMetadata, OwnedNode, OwnedRelation, OwnedWay,
};
use super::Result;
use crate::MemberId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Create,
    Modify,
    Delete,
}

#[derive(Clone, Debug)]
enum ChangeElement {
    Node(OwnedNode),
    Way(OwnedWay),
    Relation(OwnedRelation),
}

impl ChangeElement {
    fn key(&self) -> (u8, i64) {
        match self {
            Self::Node(n) => (0, n.id),
            Self::Way(w) => (1, w.id),
            Self::Relation(r) => (2, r.id),
        }
    }
}

#[derive(Clone, Debug)]
struct Change {
    action: Action,
    element: ChangeElement,
}

#[derive(Default)]
struct ChangeStream {
    changes: Vec<Change>,
}

impl ChangeStream {
    fn push(&mut self, action: Action, element: ChangeElement) {
        self.changes.push(Change { action, element });
    }
}

#[derive(Debug, Default)]
pub struct MergeChangesStats {
    pub files: usize,
    pub changes_in: u64,
    pub changes_out: u64,
    pub simplified: bool,
}

impl MergeChangesStats {
    pub fn print_summary(&self) {
        if self.simplified {
            eprintln!(
                "Merged {} files: {} input changes -> {} output changes (simplified)",
                self.files, self.changes_in, self.changes_out
            );
        } else {
            eprintln!("Merged {} files: {} changes", self.files, self.changes_out);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    None,
    Create,
    Modify,
    Delete,
}

impl Section {
    fn as_action(self) -> Option<Action> {
        match self {
            Self::Create => Some(Action::Create),
            Self::Modify => Some(Action::Modify),
            Self::Delete => Some(Action::Delete),
            Self::None => None,
        }
    }
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
    metadata: Option<OwnedMetadata>,
    tags: Vec<(String, String)>,
    refs: Vec<i64>,
    members: Vec<OwnedMember>,
}

impl CurrentElem {
    fn new(kind: ElemKind, id: i64, metadata: Option<OwnedMetadata>) -> Self {
        Self {
            kind,
            id,
            decimicro_lat: 0,
            decimicro_lon: 0,
            metadata,
            tags: Vec::new(),
            refs: Vec::new(),
            members: Vec::new(),
        }
    }

    fn into_change_element(self) -> ChangeElement {
        match self.kind {
            ElemKind::Node => ChangeElement::Node(OwnedNode {
                id: self.id,
                decimicro_lat: self.decimicro_lat,
                decimicro_lon: self.decimicro_lon,
                tags: self.tags,
                metadata: self.metadata,
            }),
            ElemKind::Way => ChangeElement::Way(OwnedWay {
                id: self.id,
                refs: self.refs,
                tags: self.tags,
                metadata: self.metadata,
            }),
            ElemKind::Relation => ChangeElement::Relation(OwnedRelation {
                id: self.id,
                members: self.members,
                tags: self.tags,
                metadata: self.metadata,
            }),
        }
    }
}

#[hotpath::measure]
pub fn merge_changes(inputs: &[&Path], output: &Path, simplify: bool) -> Result<MergeChangesStats> {
    if inputs.is_empty() {
        return Err("at least one input OSC file is required".into());
    }

    if simplify {
        let mut stream = ChangeStream::default();
        for path in inputs {
            parse_osc_into(path, &mut stream)?;
        }
        let changes_in = stream.changes.len() as u64;
        let changes_out = write_simplified(output, &stream)? as u64;
        Ok(MergeChangesStats {
            files: inputs.len(),
            changes_in,
            changes_out,
            simplified: true,
        })
    } else {
        let changes_out = write_streaming(inputs, output)?;
        Ok(MergeChangesStats {
            files: inputs.len(),
            changes_in: changes_out,
            changes_out,
            simplified: false,
        })
    }
}

fn parse_osc_into(path: &Path, stream: &mut ChangeStream) -> Result<()> {
    let mut file = File::open(path)?;
    let mut magic = [0u8; 2];
    io::Read::read_exact(&mut file, &mut magic)?;
    file.seek(io::SeekFrom::Start(0))?;

    let reader: Reader<BufReader<Box<dyn io::Read>>> = if magic == [0x1f, 0x8b] {
        Reader::from_reader(BufReader::new(Box::new(GzDecoder::new(file))))
    } else {
        Reader::from_reader(BufReader::new(Box::new(file)))
    };
    let mut reader = reader;
    reader.config_mut().trim_text(true);

    let mut section = Section::None;
    let mut current: Option<CurrentElem> = None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                handle_start_like(e, false, &mut section, &mut current, stream)?;
            }
            Ok(Event::Empty(ref e)) => {
                handle_start_like(e, true, &mut section, &mut current, stream)?;
            }
            Ok(Event::End(ref e)) => match e.name().as_ref() {
                b"create" | b"modify" | b"delete" => section = Section::None,
                b"node" | b"way" | b"relation" => {
                    finalize_current(section, &mut current, stream);
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(Box::new(e)),
        }
        buf.clear();
    }

    Ok(())
}

fn handle_start_like(
    e: &BytesStart<'_>,
    is_empty: bool,
    section: &mut Section,
    current: &mut Option<CurrentElem>,
    stream: &mut ChangeStream,
) -> Result<()> {
    match e.name().as_ref() {
        b"create" => *section = Section::Create,
        b"modify" => *section = Section::Modify,
        b"delete" => *section = Section::Delete,
        b"node" | b"way" | b"relation" => {
            let kind = match e.name().as_ref() {
                b"node" => ElemKind::Node,
                b"way" => ElemKind::Way,
                _ => ElemKind::Relation,
            };
            let id = parse_i64_attr(e, b"id")?;
            let metadata = parse_metadata(e);
            let mut elem = CurrentElem::new(kind, id, metadata);

            if kind == ElemKind::Node {
                let lat = parse_f64_attr_optional(e, b"lat").unwrap_or(0.0);
                let lon = parse_f64_attr_optional(e, b"lon").unwrap_or(0.0);
                #[allow(clippy::cast_possible_truncation)]
                {
                    elem.decimicro_lat = (lat * 1e7).round() as i32;
                    elem.decimicro_lon = (lon * 1e7).round() as i32;
                }
            }

            if is_empty || *section == Section::Delete {
                let action = section
                    .as_action()
                    .ok_or_else(|| "element outside create/modify/delete section".to_string())?;
                stream.push(action, elem.into_change_element());
            } else {
                *current = Some(elem);
            }
        }
        b"tag" => {
            if let Some(cur) = current {
                let k = parse_str_attr(e, b"k")?;
                let v = parse_str_attr(e, b"v")?;
                cur.tags.push((k, v));
            }
        }
        b"nd" => {
            if let Some(cur) = current
                && cur.kind == ElemKind::Way
            {
                let rf = parse_i64_attr(e, b"ref")?;
                cur.refs.push(rf);
            }
        }
        b"member" => {
            if let Some(cur) = current
                && cur.kind == ElemKind::Relation
            {
                let ref_id = parse_i64_attr(e, b"ref")?;
                let role = parse_str_attr_optional(e, b"role").unwrap_or_default();
                let member_id = match parse_str_attr(e, b"type")?.as_str() {
                    "node" => MemberId::Node(ref_id),
                    "way" => MemberId::Way(ref_id),
                    "relation" => MemberId::Relation(ref_id),
                    other => {
                        return Err(format!("unknown relation member type '{other}'").into());
                    }
                };
                cur.members.push(OwnedMember {
                    id: member_id,
                    role,
                });
            }
        }
        _ => {}
    }

    if is_empty {
        match e.name().as_ref() {
            b"create" | b"modify" | b"delete" => *section = Section::None,
            _ => {}
        }
    }

    Ok(())
}

fn finalize_current(
    section: Section,
    current: &mut Option<CurrentElem>,
    stream: &mut ChangeStream,
) {
    let Some(elem) = current.take() else {
        return;
    };
    if let Some(action) = section.as_action() {
        stream.push(action, elem.into_change_element());
    }
}

fn parse_i64_attr(e: &BytesStart<'_>, name: &[u8]) -> Result<i64> {
    for attr in e.attributes() {
        let attr = attr?;
        if attr.key == QName(name) {
            return Ok(std::str::from_utf8(&attr.value)?.parse::<i64>()?);
        }
    }
    Err(format!("missing attribute '{}'", String::from_utf8_lossy(name)).into())
}

fn parse_metadata(e: &BytesStart<'_>) -> Option<OwnedMetadata> {
    let version = parse_i32_attr_optional(e, b"version")?;
    Some(OwnedMetadata {
        version,
        timestamp: parse_str_attr_optional(e, b"timestamp").unwrap_or_default(),
        changeset: parse_str_attr_optional(e, b"changeset").unwrap_or_default(),
        uid: parse_str_attr_optional(e, b"uid").unwrap_or_default(),
        user: parse_str_attr_optional(e, b"user").unwrap_or_default(),
        visible: parse_str_attr_optional(e, b"visible").unwrap_or_default(),
    })
}

fn parse_i32_attr_optional(e: &BytesStart<'_>, name: &[u8]) -> Option<i32> {
    for attr in e.attributes().flatten() {
        if attr.key == QName(name) {
            let text = std::str::from_utf8(&attr.value).ok()?;
            let parsed = text.parse::<i32>().ok()?;
            return Some(parsed);
        }
    }
    None
}

fn parse_f64_attr_optional(e: &BytesStart<'_>, name: &[u8]) -> Option<f64> {
    for attr in e.attributes().flatten() {
        if attr.key == QName(name) {
            let text = std::str::from_utf8(&attr.value).ok()?;
            let parsed = text.parse::<f64>().ok()?;
            return Some(parsed);
        }
    }
    None
}

fn parse_str_attr(e: &BytesStart<'_>, name: &[u8]) -> Result<String> {
    for attr in e.attributes() {
        let attr = attr?;
        if attr.key == QName(name) {
            return Ok(attr.unescape_value()?.into_owned());
        }
    }
    Err(format!("missing attribute '{}'", String::from_utf8_lossy(name)).into())
}

fn parse_str_attr_optional(e: &BytesStart<'_>, name: &[u8]) -> Option<String> {
    for attr in e.attributes().flatten() {
        if attr.key == QName(name) {
            return attr.unescape_value().ok().map(std::borrow::Cow::into_owned);
        }
    }
    None
}

enum OscWriter {
    Gz(Box<Writer<GzEncoder<io::BufWriter<File>>>>),
    Plain(Writer<io::BufWriter<File>>),
}

impl OscWriter {
    fn new(output: &Path) -> Result<Self> {
        let file = File::create(output)?;
        let buf = io::BufWriter::new(file);
        let is_gz = output.to_str().is_some_and(|s| s.ends_with(".gz"));
        if is_gz {
            Ok(Self::Gz(Box::new(Writer::new_with_indent(
                GzEncoder::new(buf, flate2::Compression::fast()),
                b' ',
                2,
            ))))
        } else {
            Ok(Self::Plain(Writer::new_with_indent(buf, b' ', 2)))
        }
    }

    fn write_event(&mut self, event: Event<'_>) -> Result<()> {
        match self {
            Self::Gz(w) => w.write_event(event)?,
            Self::Plain(w) => w.write_event(event)?,
        }
        Ok(())
    }

    fn finish(self) -> Result<()> {
        match self {
            Self::Gz(w) => {
                let gz = w.into_inner();
                gz.finish()?;
            }
            Self::Plain(w) => {
                let mut buf = w.into_inner();
                io::Write::flush(&mut buf)?;
            }
        }
        Ok(())
    }
}

fn write_streaming(inputs: &[&Path], output: &Path) -> Result<u64> {
    let mut writer = OscWriter::new(output)?;

    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
    let mut root = BytesStart::new("osmChange");
    root.push_attribute(("version", "0.6"));
    writer.write_event(Event::Start(root))?;

    let mut open_action: Option<Action> = None;
    let mut count = 0u64;
    for path in inputs {
        parse_osc_streaming(path, &mut writer, &mut open_action, &mut count)?;
    }

    if let Some(prev) = open_action {
        writer.write_event(Event::End(BytesEnd::new(action_tag(prev))))?;
    }

    writer.write_event(Event::End(BytesEnd::new("osmChange")))?;
    writer.finish()?;

    Ok(count)
}

fn parse_osc_streaming(
    path: &Path,
    writer: &mut OscWriter,
    open_action: &mut Option<Action>,
    count: &mut u64,
) -> Result<()> {
    let mut file = File::open(path)?;
    let mut magic = [0u8; 2];
    io::Read::read_exact(&mut file, &mut magic)?;
    file.seek(io::SeekFrom::Start(0))?;

    let reader: Reader<BufReader<Box<dyn io::Read>>> = if magic == [0x1f, 0x8b] {
        Reader::from_reader(BufReader::new(Box::new(GzDecoder::new(file))))
    } else {
        Reader::from_reader(BufReader::new(Box::new(file)))
    };
    let mut reader = reader;
    reader.config_mut().trim_text(true);

    let mut section = Section::None;
    let mut current: Option<CurrentElem> = None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                handle_start_like_streaming(
                    e, false, &mut section, &mut current, writer, open_action, count,
                )?;
            }
            Ok(Event::Empty(ref e)) => {
                handle_start_like_streaming(
                    e, true, &mut section, &mut current, writer, open_action, count,
                )?;
            }
            Ok(Event::End(ref e)) => match e.name().as_ref() {
                b"create" | b"modify" | b"delete" => section = Section::None,
                b"node" | b"way" | b"relation" => {
                    if let Some(elem) = current.take()
                        && let Some(action) = section.as_action()
                    {
                        let change = Change {
                            action,
                            element: elem.into_change_element(),
                        };
                        emit_change(writer, open_action, &change)?;
                        *count += 1;
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(Box::new(e)),
        }
        buf.clear();
    }

    Ok(())
}

fn handle_start_like_streaming(
    e: &BytesStart<'_>,
    is_empty: bool,
    section: &mut Section,
    current: &mut Option<CurrentElem>,
    writer: &mut OscWriter,
    open_action: &mut Option<Action>,
    count: &mut u64,
) -> Result<()> {
    match e.name().as_ref() {
        b"create" => *section = Section::Create,
        b"modify" => *section = Section::Modify,
        b"delete" => *section = Section::Delete,
        b"node" | b"way" | b"relation" => {
            let kind = match e.name().as_ref() {
                b"node" => ElemKind::Node,
                b"way" => ElemKind::Way,
                _ => ElemKind::Relation,
            };
            let id = parse_i64_attr(e, b"id")?;
            let metadata = parse_metadata(e);
            let mut elem = CurrentElem::new(kind, id, metadata);

            if kind == ElemKind::Node {
                let lat = parse_f64_attr_optional(e, b"lat").unwrap_or(0.0);
                let lon = parse_f64_attr_optional(e, b"lon").unwrap_or(0.0);
                #[allow(clippy::cast_possible_truncation)]
                {
                    elem.decimicro_lat = (lat * 1e7).round() as i32;
                    elem.decimicro_lon = (lon * 1e7).round() as i32;
                }
            }

            if is_empty || *section == Section::Delete {
                let action = section
                    .as_action()
                    .ok_or_else(|| "element outside create/modify/delete section".to_string())?;
                let change = Change {
                    action,
                    element: elem.into_change_element(),
                };
                emit_change(writer, open_action, &change)?;
                *count += 1;
            } else {
                *current = Some(elem);
            }
        }
        b"tag" => {
            if let Some(cur) = current {
                let k = parse_str_attr(e, b"k")?;
                let v = parse_str_attr(e, b"v")?;
                cur.tags.push((k, v));
            }
        }
        b"nd" => {
            if let Some(cur) = current
                && cur.kind == ElemKind::Way
            {
                let rf = parse_i64_attr(e, b"ref")?;
                cur.refs.push(rf);
            }
        }
        b"member" => {
            if let Some(cur) = current
                && cur.kind == ElemKind::Relation
            {
                let ref_id = parse_i64_attr(e, b"ref")?;
                let role = parse_str_attr_optional(e, b"role").unwrap_or_default();
                let member_id = match parse_str_attr(e, b"type")?.as_str() {
                    "node" => MemberId::Node(ref_id),
                    "way" => MemberId::Way(ref_id),
                    "relation" => MemberId::Relation(ref_id),
                    other => {
                        return Err(format!("unknown relation member type '{other}'").into());
                    }
                };
                cur.members.push(OwnedMember {
                    id: member_id,
                    role,
                });
            }
        }
        _ => {}
    }

    if is_empty {
        match e.name().as_ref() {
            b"create" | b"modify" | b"delete" => *section = Section::None,
            _ => {}
        }
    }

    Ok(())
}

fn emit_change(
    writer: &mut OscWriter,
    open_action: &mut Option<Action>,
    change: &Change,
) -> Result<()> {
    if *open_action != Some(change.action) {
        if let Some(prev) = open_action.take() {
            writer.write_event(Event::End(BytesEnd::new(action_tag(prev))))?;
        }
        writer.write_event(Event::Start(BytesStart::new(action_tag(change.action))))?;
        *open_action = Some(change.action);
    }
    write_change_to(writer, change)
}

fn write_simplified(output: &Path, stream: &ChangeStream) -> Result<usize> {
    let mut last_by_object: BTreeMap<(u8, i64), Change> = BTreeMap::new();
    for change in &stream.changes {
        last_by_object.insert(change.element.key(), change.clone());
    }

    let mut creates = Vec::new();
    let mut modifies = Vec::new();
    let mut deletes = Vec::new();
    for (_, change) in last_by_object {
        match change.action {
            Action::Create => creates.push(change),
            Action::Modify => modifies.push(change),
            Action::Delete => deletes.push(change),
        }
    }

    let mut writer = OscWriter::new(output)?;

    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
    let mut root = BytesStart::new("osmChange");
    root.push_attribute(("version", "0.6"));
    writer.write_event(Event::Start(root))?;

    if !creates.is_empty() {
        writer.write_event(Event::Start(BytesStart::new("create")))?;
        for change in &creates {
            write_change_to(&mut writer, change)?;
        }
        writer.write_event(Event::End(BytesEnd::new("create")))?;
    }

    if !modifies.is_empty() {
        writer.write_event(Event::Start(BytesStart::new("modify")))?;
        for change in &modifies {
            write_change_to(&mut writer, change)?;
        }
        writer.write_event(Event::End(BytesEnd::new("modify")))?;
    }

    if !deletes.is_empty() {
        writer.write_event(Event::Start(BytesStart::new("delete")))?;
        for change in &deletes {
            write_change_to(&mut writer, change)?;
        }
        writer.write_event(Event::End(BytesEnd::new("delete")))?;
    }

    writer.write_event(Event::End(BytesEnd::new("osmChange")))?;
    writer.finish()?;

    Ok(creates.len() + modifies.len() + deletes.len())
}

fn action_tag(action: Action) -> &'static str {
    match action {
        Action::Create => "create",
        Action::Modify => "modify",
        Action::Delete => "delete",
    }
}

fn write_change_to(writer: &mut OscWriter, change: &Change) -> Result<()> {
    let delete = change.action == Action::Delete;
    match (&change.element, writer) {
        (ChangeElement::Node(node), OscWriter::Gz(w)) => write_node(w, node, delete),
        (ChangeElement::Node(node), OscWriter::Plain(w)) => write_node(w, node, delete),
        (ChangeElement::Way(way), OscWriter::Gz(w)) => write_way(w, way, delete),
        (ChangeElement::Way(way), OscWriter::Plain(w)) => write_way(w, way, delete),
        (ChangeElement::Relation(rel), OscWriter::Gz(w)) => write_relation(w, rel, delete),
        (ChangeElement::Relation(rel), OscWriter::Plain(w)) => write_relation(w, rel, delete),
    }
}

fn write_node<W: Write>(writer: &mut Writer<W>, node: &OwnedNode, delete_only: bool) -> Result<()> {
    let mut elem = BytesStart::new("node");
    let id = node.id.to_string();
    elem.push_attribute(("id", id.as_str()));
    if let Some(meta) = &node.metadata {
        meta.push_attrs(&mut elem);
    }

    if delete_only {
        writer.write_event(Event::Empty(elem))?;
        return Ok(());
    }

    let mut coord_buf = String::new();
    format_coord(&mut coord_buf, from_decimicro(node.decimicro_lat));
    let lat = coord_buf.clone();
    format_coord(&mut coord_buf, from_decimicro(node.decimicro_lon));
    elem.push_attribute(("lat", lat.as_str()));
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

fn write_way<W: Write>(writer: &mut Writer<W>, way: &OwnedWay, delete_only: bool) -> Result<()> {
    let mut elem = BytesStart::new("way");
    let id = way.id.to_string();
    elem.push_attribute(("id", id.as_str()));
    if let Some(meta) = &way.metadata {
        meta.push_attrs(&mut elem);
    }

    if delete_only || (way.refs.is_empty() && way.tags.is_empty()) {
        writer.write_event(Event::Empty(elem))?;
        return Ok(());
    }

    writer.write_event(Event::Start(elem))?;
    for rf in &way.refs {
        let mut nd = BytesStart::new("nd");
        let rf = rf.to_string();
        nd.push_attribute(("ref", rf.as_str()));
        writer.write_event(Event::Empty(nd))?;
    }
    for (k, v) in &way.tags {
        let mut tag = BytesStart::new("tag");
        tag.push_attribute(("k", k.as_str()));
        tag.push_attribute(("v", v.as_str()));
        writer.write_event(Event::Empty(tag))?;
    }
    writer.write_event(Event::End(BytesEnd::new("way")))?;
    Ok(())
}

fn write_relation<W: Write>(
    writer: &mut Writer<W>,
    relation: &OwnedRelation,
    delete_only: bool,
) -> Result<()> {
    let mut elem = BytesStart::new("relation");
    let id = relation.id.to_string();
    elem.push_attribute(("id", id.as_str()));
    if let Some(meta) = &relation.metadata {
        meta.push_attrs(&mut elem);
    }

    if delete_only || (relation.members.is_empty() && relation.tags.is_empty()) {
        writer.write_event(Event::Empty(elem))?;
        return Ok(());
    }

    writer.write_event(Event::Start(elem))?;
    for member in &relation.members {
        let mut m = BytesStart::new("member");
        let type_str = match member.id {
            MemberId::Node(_) => "node",
            MemberId::Way(_) => "way",
            MemberId::Relation(_) => "relation",
            MemberId::Unknown(_, _) => "node",
        };
        let member_id = member.id.id().to_string();
        m.push_attribute(("type", type_str));
        m.push_attribute(("ref", member_id.as_str()));
        m.push_attribute(("role", member.role.as_str()));
        writer.write_event(Event::Empty(m))?;
    }
    for (k, v) in &relation.tags {
        let mut tag = BytesStart::new("tag");
        tag.push_attribute(("k", k.as_str()));
        tag.push_attribute(("v", v.as_str()));
        writer.write_event(Event::Empty(tag))?;
    }
    writer.write_event(Event::End(BytesEnd::new("relation")))?;
    Ok(())
}
