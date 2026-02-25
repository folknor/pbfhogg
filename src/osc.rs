// OSC (.osc.gz) parser for OpenStreetMap change files.
//
// Parses Geofabrik-style replication diffs into a `DiffOverlay` that tracks
// created, modified, and deleted nodes/ways/relations.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use flate2::read::GzDecoder;
use quick_xml::events::Event;
use quick_xml::Reader;

// Import MemberType from the crate's read::elements module for type-safe
// representation of relation member types (Node, Way, Relation) instead of
// raw strings.
use crate::read::elements::MemberType;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

pub struct OscNode {
    pub id: i64,
    pub lat: f64,
    pub lon: f64,
    pub tags: Vec<(String, String)>,
}

pub struct OscWay {
    pub id: i64,
    pub node_refs: Vec<i64>,
    pub tags: Vec<(String, String)>,
}

pub struct OscRelMember {
    /// The element type of this relation member, using the crate's `MemberType`
    /// enum for type safety instead of a raw string.
    pub member_type: MemberType,
    pub ref_id: i64,
    pub role: String,
}

pub struct OscRelation {
    pub id: i64,
    pub members: Vec<OscRelMember>,
    pub tags: Vec<(String, String)>,
}

pub struct DiffOverlay {
    pub nodes: HashMap<i64, OscNode>,
    pub ways: HashMap<i64, OscWay>,
    pub relations: HashMap<i64, OscRelation>,
    pub deleted_nodes: HashSet<i64>,
    pub deleted_ways: HashSet<i64>,
    pub deleted_relations: HashSet<i64>,
}

// ---------------------------------------------------------------------------
// DiffOverlay
// ---------------------------------------------------------------------------

impl DiffOverlay {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            ways: HashMap::new(),
            relations: HashMap::new(),
            deleted_nodes: HashSet::new(),
            deleted_ways: HashSet::new(),
            deleted_relations: HashSet::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
            && self.ways.is_empty()
            && self.relations.is_empty()
            && self.deleted_nodes.is_empty()
            && self.deleted_ways.is_empty()
            && self.deleted_relations.is_empty()
    }

    /// Merge another overlay into this one. Later overlay wins for conflicts.
    pub fn merge(&mut self, other: DiffOverlay) {
        // Created/modified in other → remove from our deleted sets
        for id in other.nodes.keys() {
            self.deleted_nodes.remove(id);
        }
        for id in other.ways.keys() {
            self.deleted_ways.remove(id);
        }
        for id in other.relations.keys() {
            self.deleted_relations.remove(id);
        }

        // Deleted in other → remove from our data maps
        for &id in &other.deleted_nodes {
            self.nodes.remove(&id);
        }
        for &id in &other.deleted_ways {
            self.ways.remove(&id);
        }
        for &id in &other.deleted_relations {
            self.relations.remove(&id);
        }

        // Extend all (later wins for same key)
        self.nodes.extend(other.nodes);
        self.ways.extend(other.ways);
        self.relations.extend(other.relations);
        self.deleted_nodes.extend(other.deleted_nodes);
        self.deleted_ways.extend(other.deleted_ways);
        self.deleted_relations.extend(other.deleted_relations);
    }
}

impl Default for DiffOverlay {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Section tracking
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    None,
    Create,
    Modify,
    Delete,
}

// ---------------------------------------------------------------------------
// Attribute parsing helpers
// ---------------------------------------------------------------------------

// Box<dyn Error> is intentional — OSC parsing is CLI-internal, callers only
// display errors. String errors include the missing attribute name for context.
type ParseResult<T> = Result<T, Box<dyn std::error::Error>>;

fn parse_i64_attr(e: &quick_xml::events::BytesStart, name: &[u8]) -> ParseResult<i64> {
    for attr_result in e.attributes() {
        let attr = attr_result?;
        if attr.key.as_ref() == name {
            let val = std::str::from_utf8(&attr.value)?;
            let parsed = val.parse::<i64>()?;
            return Ok(parsed);
        }
    }
    Err(format!("missing attribute '{}'", String::from_utf8_lossy(name)).into())
}

fn parse_f64_attr(e: &quick_xml::events::BytesStart, name: &[u8]) -> ParseResult<f64> {
    for attr_result in e.attributes() {
        let attr = attr_result?;
        if attr.key.as_ref() == name {
            let val = std::str::from_utf8(&attr.value)?;
            let parsed = val.parse::<f64>()?;
            return Ok(parsed);
        }
    }
    Err(format!("missing attribute '{}'", String::from_utf8_lossy(name)).into())
}

fn parse_str_attr(e: &quick_xml::events::BytesStart, name: &[u8]) -> ParseResult<String> {
    for attr_result in e.attributes() {
        let attr = attr_result?;
        if attr.key.as_ref() == name {
            let val = attr.unescape_value()?;
            return Ok(val.into_owned());
        }
    }
    Err(format!("missing attribute '{}'", String::from_utf8_lossy(name)).into())
}

// ---------------------------------------------------------------------------
// Tag/nd/member handlers
// ---------------------------------------------------------------------------

fn handle_tag(
    e: &quick_xml::events::BytesStart,
    current_node: &mut Option<OscNode>,
    current_way: &mut Option<OscWay>,
    current_relation: &mut Option<OscRelation>,
) -> ParseResult<()> {
    let k = parse_str_attr(e, b"k")?;
    let v = parse_str_attr(e, b"v")?;
    if let Some(node) = current_node.as_mut() {
        node.tags.push((k, v));
    } else if let Some(way) = current_way.as_mut() {
        way.tags.push((k, v));
    } else if let Some(rel) = current_relation.as_mut() {
        rel.tags.push((k, v));
    }
    Ok(())
}

fn handle_nd(
    e: &quick_xml::events::BytesStart,
    current_way: &mut Option<OscWay>,
) -> ParseResult<()> {
    let ref_id = parse_i64_attr(e, b"ref")?;
    if let Some(way) = current_way.as_mut() {
        way.node_refs.push(ref_id);
    }
    Ok(())
}

/// Convert an OSC XML member type string ("node", "way", "relation") to
/// the crate's `MemberType` enum. Unknown values produce an error rather
/// than panicking, so callers can decide how to handle malformed input.
fn parse_member_type(s: &str) -> ParseResult<MemberType> {
    match s {
        "node" => Ok(MemberType::Node),
        "way" => Ok(MemberType::Way),
        "relation" => Ok(MemberType::Relation),
        other => Err(format!("unknown relation member type: '{other}'").into()),
    }
}

fn handle_member(
    e: &quick_xml::events::BytesStart,
    current_relation: &mut Option<OscRelation>,
) -> ParseResult<()> {
    // Parse the "type" attribute as a string, then convert to MemberType.
    // Unknown type values will propagate as an error to the caller.
    let member_type_str = parse_str_attr(e, b"type")?;
    let member_type = parse_member_type(&member_type_str)?;
    let ref_id = parse_i64_attr(e, b"ref")?;
    let role = parse_str_attr(e, b"role").unwrap_or_default();
    if let Some(rel) = current_relation.as_mut() {
        rel.members.push(OscRelMember {
            member_type,
            ref_id,
            role,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Element start/empty handlers
// ---------------------------------------------------------------------------

fn handle_node_start(
    e: &quick_xml::events::BytesStart,
    section: Section,
    is_empty: bool,
    overlay: &mut DiffOverlay,
    current_node: &mut Option<OscNode>,
) -> ParseResult<()> {
    let id = parse_i64_attr(e, b"id")?;
    if section == Section::Delete {
        overlay.deleted_nodes.insert(id);
        return Ok(());
    }
    let lat = parse_f64_attr(e, b"lat").unwrap_or(0.0);
    let lon = parse_f64_attr(e, b"lon").unwrap_or(0.0);
    let node = OscNode {
        id,
        lat,
        lon,
        tags: Vec::new(),
    };
    if is_empty {
        overlay.nodes.insert(id, node);
    } else {
        *current_node = Some(node);
    }
    Ok(())
}

fn handle_way_start(
    e: &quick_xml::events::BytesStart,
    section: Section,
    is_empty: bool,
    overlay: &mut DiffOverlay,
    current_way: &mut Option<OscWay>,
) -> ParseResult<()> {
    let id = parse_i64_attr(e, b"id")?;
    if section == Section::Delete {
        overlay.deleted_ways.insert(id);
        return Ok(());
    }
    let way = OscWay {
        id,
        node_refs: Vec::new(),
        tags: Vec::new(),
    };
    if is_empty {
        overlay.ways.insert(id, way);
    } else {
        *current_way = Some(way);
    }
    Ok(())
}

fn handle_relation_start(
    e: &quick_xml::events::BytesStart,
    section: Section,
    is_empty: bool,
    overlay: &mut DiffOverlay,
    current_relation: &mut Option<OscRelation>,
) -> ParseResult<()> {
    let id = parse_i64_attr(e, b"id")?;
    if section == Section::Delete {
        overlay.deleted_relations.insert(id);
        return Ok(());
    }
    let rel = OscRelation {
        id,
        members: Vec::new(),
        tags: Vec::new(),
    };
    if is_empty {
        overlay.relations.insert(id, rel);
    } else {
        *current_relation = Some(rel);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Main parser
// ---------------------------------------------------------------------------

/// Parse a single .osc.gz file into a `DiffOverlay`.
pub fn parse_osc_file(path: &Path) -> ParseResult<DiffOverlay> {
    let file = File::open(path)?;
    let decoder = GzDecoder::new(file);
    let buf_reader = BufReader::new(decoder);
    let mut reader = Reader::from_reader(buf_reader);
    reader.config_mut().trim_text(true);

    let mut overlay = DiffOverlay::new();
    let mut section = Section::None;
    let mut current_node: Option<OscNode> = None;
    let mut current_way: Option<OscWay> = None;
    let mut current_relation: Option<OscRelation> = None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                handle_start_event(
                    e,
                    &mut section,
                    &mut overlay,
                    &mut current_node,
                    &mut current_way,
                    &mut current_relation,
                )?;
            }
            Ok(Event::Empty(ref e)) => {
                handle_empty_event(
                    e,
                    section,
                    &mut overlay,
                    &mut current_node,
                    &mut current_way,
                    &mut current_relation,
                )?;
            }
            Ok(Event::End(ref e)) => {
                handle_end_event(
                    e,
                    &mut section,
                    &mut overlay,
                    &mut current_node,
                    &mut current_way,
                    &mut current_relation,
                );
            }
            Ok(Event::Eof) => break,
            Ok(_) => {} // text, comments, decl, etc.
            Err(e) => return Err(Box::new(e)),
        }
        buf.clear();
    }

    Ok(overlay)
}

fn handle_start_event(
    e: &quick_xml::events::BytesStart,
    section: &mut Section,
    overlay: &mut DiffOverlay,
    current_node: &mut Option<OscNode>,
    current_way: &mut Option<OscWay>,
    current_relation: &mut Option<OscRelation>,
) -> ParseResult<()> {
    match e.name().as_ref() {
        b"create" => *section = Section::Create,
        b"modify" => *section = Section::Modify,
        b"delete" => *section = Section::Delete,
        b"node" => handle_node_start(e, *section, false, overlay, current_node)?,
        b"way" => handle_way_start(e, *section, false, overlay, current_way)?,
        b"relation" => handle_relation_start(e, *section, false, overlay, current_relation)?,
        b"tag" => handle_tag(e, current_node, current_way, current_relation)?,
        b"nd" => handle_nd(e, current_way)?,
        b"member" => handle_member(e, current_relation)?,
        _ => {}
    }
    Ok(())
}

fn handle_empty_event(
    e: &quick_xml::events::BytesStart,
    section: Section,
    overlay: &mut DiffOverlay,
    current_node: &mut Option<OscNode>,
    current_way: &mut Option<OscWay>,
    current_relation: &mut Option<OscRelation>,
) -> ParseResult<()> {
    match e.name().as_ref() {
        b"node" => handle_node_start(e, section, true, overlay, current_node)?,
        b"way" => handle_way_start(e, section, true, overlay, current_way)?,
        b"relation" => handle_relation_start(e, section, true, overlay, current_relation)?,
        b"tag" => handle_tag(e, current_node, current_way, current_relation)?,
        b"nd" => handle_nd(e, current_way)?,
        b"member" => handle_member(e, current_relation)?,
        _ => {}
    }
    Ok(())
}

fn handle_end_event(
    e: &quick_xml::events::BytesEnd,
    section: &mut Section,
    overlay: &mut DiffOverlay,
    current_node: &mut Option<OscNode>,
    current_way: &mut Option<OscWay>,
    current_relation: &mut Option<OscRelation>,
) {
    match e.name().as_ref() {
        b"create" | b"modify" | b"delete" => *section = Section::None,
        b"node" => {
            if let Some(node) = current_node.take() {
                overlay.nodes.insert(node.id, node);
            }
        }
        b"way" => {
            if let Some(way) = current_way.take() {
                overlay.ways.insert(way.id, way);
            }
        }
        b"relation" => {
            if let Some(rel) = current_relation.take() {
                overlay.relations.insert(rel.id, rel);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Load all diffs from a directory
// ---------------------------------------------------------------------------

/// Parse the numeric sequence number from an OSC filename.
/// E.g. "4705.osc.gz" → stem "4705.osc" → strip ".osc" → 4705.
fn parse_sequence_number(filename: &str) -> Option<u64> {
    let stem = filename.strip_suffix(".gz")?;
    let num_str = stem.strip_suffix(".osc")?;
    num_str.parse::<u64>().ok()
}

/// Load all .osc.gz diffs from a directory, sorted by sequence number, and
/// merge them into a single `DiffOverlay`.
pub fn load_all_diffs(diffs_dir: &Path) -> ParseResult<DiffOverlay> {
    let mut entries: Vec<(u64, std::path::PathBuf)> = Vec::new();

    for entry in std::fs::read_dir(diffs_dir)? {
        let entry = entry?;
        let path = entry.path();
        let filename = match path.file_name().and_then(|f| f.to_str()) {
            Some(f) => f.to_string(),
            None => continue,
        };
        if !filename.ends_with(".gz") {
            continue;
        }
        if let Some(seq) = parse_sequence_number(&filename) {
            entries.push((seq, path));
        }
    }

    entries.sort_by_key(|(seq, _)| *seq);

    let mut overlay = DiffOverlay::new();
    let total = entries.len();

    for (i, (seq, path)) in entries.iter().enumerate() {
        eprintln!(
            "[{}/{}] Parsing diff {} (sequence {seq})...",
            i + 1,
            total,
            path.display()
        );
        let diff = parse_osc_file(path)?;
        overlay.merge(diff);
    }

    eprintln!(
        "Loaded {total} diffs: {} nodes, {} ways, {} relations \
         ({} deleted nodes, {} deleted ways, {} deleted relations)",
        overlay.nodes.len(),
        overlay.ways.len(),
        overlay.relations.len(),
        overlay.deleted_nodes.len(),
        overlay.deleted_ways.len(),
        overlay.deleted_relations.len(),
    );

    Ok(overlay)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    /// Create a unique temp directory for test isolation.
    fn make_test_dir(suffix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pbfhogg_osc_test_{suffix}"));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    /// Write a .osc.gz file from raw XML string.
    fn write_osc_gz(dir: &Path, filename: &str, xml: &str) {
        let path = dir.join(filename);
        let file = File::create(&path).expect("create osc.gz");
        let mut enc = GzEncoder::new(file, Compression::fast());
        enc.write_all(xml.as_bytes()).expect("write xml");
        enc.finish().expect("finish gz");
    }

    // All test functions return Result so that fallible operations can use `?`
    // instead of `.unwrap()`. This avoids the need for
    // `#[allow(clippy::unwrap_used)]` on the entire test module and gives
    // clearer error messages on failure (the error is printed rather than a
    // bare panic with no context).

    #[test]
    fn test_parse_osc_create_modify_delete() -> ParseResult<()> {
        let dir = make_test_dir("create_modify_delete");

        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="100" lat="55.6761" lon="12.5683" version="1">
      <tag k="name" v="Copenhagen"/>
    </node>
  </create>
  <modify>
    <node id="100" lat="55.6800" lon="12.5700" version="2">
      <tag k="name" v="CPH"/>
    </node>
  </modify>
  <delete>
    <way id="200" version="3"/>
  </delete>
</osmChange>"#;

        write_osc_gz(&dir, "test.osc.gz", xml);
        // Use `?` instead of `.unwrap()` to propagate parse errors with context.
        let overlay = parse_osc_file(&dir.join("test.osc.gz"))?;

        // Modified node should overwrite created node
        let node = overlay
            .nodes
            .get(&100)
            .ok_or("node 100 should exist in overlay")?;
        assert!((node.lat - 55.68).abs() < 0.0001);
        assert!((node.lon - 12.57).abs() < 0.0001);
        assert_eq!(node.tags.len(), 1);
        assert_eq!(node.tags[0].0, "name");
        assert_eq!(node.tags[0].1, "CPH");

        // Deleted way
        assert!(overlay.deleted_ways.contains(&200));
        assert!(!overlay.ways.contains_key(&200));

        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn test_merge_later_wins() -> ParseResult<()> {
        let mut a = DiffOverlay::new();
        a.nodes.insert(
            100,
            OscNode {
                id: 100,
                lat: 1.0,
                lon: 2.0,
                tags: Vec::new(),
            },
        );

        let mut b = DiffOverlay::new();
        b.nodes.insert(
            100,
            OscNode {
                id: 100,
                lat: 3.0,
                lon: 4.0,
                tags: Vec::new(),
            },
        );

        a.merge(b);
        let node = a.nodes.get(&100).ok_or("node 100 should exist after merge")?;
        assert!((node.lat - 3.0).abs() < f64::EPSILON);
        assert!((node.lon - 4.0).abs() < f64::EPSILON);
        Ok(())
    }

    #[test]
    fn test_merge_delete_removes_create() {
        let mut a = DiffOverlay::new();
        a.nodes.insert(
            100,
            OscNode {
                id: 100,
                lat: 1.0,
                lon: 2.0,
                tags: Vec::new(),
            },
        );

        let mut b = DiffOverlay::new();
        b.deleted_nodes.insert(100);

        a.merge(b);
        assert!(!a.nodes.contains_key(&100));
        assert!(a.deleted_nodes.contains(&100));
    }

    #[test]
    fn test_merge_create_removes_delete() {
        let mut a = DiffOverlay::new();
        a.deleted_nodes.insert(100);

        let mut b = DiffOverlay::new();
        b.nodes.insert(
            100,
            OscNode {
                id: 100,
                lat: 1.0,
                lon: 2.0,
                tags: Vec::new(),
            },
        );

        a.merge(b);
        assert!(a.nodes.contains_key(&100));
        assert!(!a.deleted_nodes.contains(&100));
    }

    #[test]
    fn test_numeric_sort() -> ParseResult<()> {
        let dir = make_test_dir("numeric_sort");

        // Create files in non-numeric-alphabetical order
        let xml_999 = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create><node id="1" lat="1.0" lon="1.0" version="1"/></create>
</osmChange>"#;

        let xml_4705 = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create><node id="2" lat="2.0" lon="2.0" version="1"/></create>
</osmChange>"#;

        let xml_10000 = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify><node id="1" lat="10.0" lon="10.0" version="2"/></modify>
</osmChange>"#;

        write_osc_gz(&dir, "10000.osc.gz", xml_10000);
        write_osc_gz(&dir, "4705.osc.gz", xml_4705);
        write_osc_gz(&dir, "999.osc.gz", xml_999);

        // Use `?` instead of `.unwrap()` to propagate errors with context.
        let overlay = load_all_diffs(&dir)?;

        // Node 1 should have been created by 999, then modified by 10000
        let node1 = overlay
            .nodes
            .get(&1)
            .ok_or("node 1 should exist after loading diffs")?;
        assert!((node1.lat - 10.0).abs() < f64::EPSILON);
        assert!((node1.lon - 10.0).abs() < f64::EPSILON);

        // Node 2 from 4705 should exist
        assert!(overlay.nodes.contains_key(&2));

        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn test_empty_overlay() {
        let overlay = DiffOverlay::new();
        assert!(overlay.is_empty());
    }

    #[test]
    fn test_self_closing_delete() -> ParseResult<()> {
        let dir = make_test_dir("self_closing_delete");

        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <delete>
    <node id="123"/>
  </delete>
</osmChange>"#;

        write_osc_gz(&dir, "test.osc.gz", xml);
        // Use `?` instead of `.unwrap()` to propagate parse errors with context.
        let overlay = parse_osc_file(&dir.join("test.osc.gz"))?;

        assert!(overlay.deleted_nodes.contains(&123));
        assert!(!overlay.nodes.contains_key(&123));

        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
