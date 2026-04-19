// OSC (.osc.gz) parser for OpenStreetMap change files.
//
// Parses Geofabrik-style replication diffs into a `CompactDiffOverlay` that tracks
// created, modified, and deleted nodes/ways/relations using arena-packed binary
// layouts with interned tag keys and relation member roles.

use std::collections::HashSet;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use flate2::read::GzDecoder;
use quick_xml::events::Event;
use quick_xml::Reader;
use rustc_hash::FxHashMap;

use crate::read::elements::MemberType;

use super::compact::{
    arena_append_node, arena_append_relation, arena_append_way, member_type_to_byte,
};
use super::interner::StringInterner;

pub use super::compact::{
    CompactMemberIter, CompactNodeRef, CompactRefIter, CompactRelationRef,
    CompactTagIter, CompactWayRef,
};

// Box<dyn Error> is intentional - OSC parsing is CLI-internal, callers only
// display errors. String errors include the missing attribute name for context.
type ParseResult<T> = Result<T, Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// CompactDiffOverlay
// ---------------------------------------------------------------------------

/// Arena-packed diff overlay for OSC change files.
///
/// Instead of per-element heap allocations (`HashMap<i64, OscNode>` etc.), all
/// element data is packed into flat `Vec<u8>` arenas with a `FxHashMap<i64, u32>`
/// index mapping element IDs to byte offsets. Tag keys and relation member roles
/// are interned via `StringInterner` to eliminate duplicate string storage.
///
/// This typically uses 40-60% less memory than the old `DiffOverlay` for
/// real-world planet-scale diffs (millions of elements with repeated tag keys).
pub struct CompactDiffOverlay {
    node_arena: Vec<u8>,
    way_arena: Vec<u8>,
    relation_arena: Vec<u8>,
    node_index: FxHashMap<i64, u32>,
    way_index: FxHashMap<i64, u32>,
    relation_index: FxHashMap<i64, u32>,
    pub deleted_nodes: HashSet<i64>,
    pub deleted_ways: HashSet<i64>,
    pub deleted_relations: HashSet<i64>,
    interner: StringInterner,
}

impl CompactDiffOverlay {
    /// Create a new empty overlay.
    pub fn new() -> Self {
        Self {
            node_arena: Vec::new(),
            way_arena: Vec::new(),
            relation_arena: Vec::new(),
            node_index: FxHashMap::default(),
            way_index: FxHashMap::default(),
            relation_index: FxHashMap::default(),
            deleted_nodes: HashSet::new(),
            deleted_ways: HashSet::new(),
            deleted_relations: HashSet::new(),
            interner: StringInterner::new(),
        }
    }

    /// Returns true if the overlay contains no data at all.
    pub fn is_empty(&self) -> bool {
        self.node_index.is_empty()
            && self.way_index.is_empty()
            && self.relation_index.is_empty()
            && self.deleted_nodes.is_empty()
            && self.deleted_ways.is_empty()
            && self.deleted_relations.is_empty()
    }

    /// Look up a node by ID, returning a zero-copy accessor.
    pub fn get_node(&self, id: i64) -> Option<CompactNodeRef<'_>> {
        let &offset = self.node_index.get(&id)?;
        Some(CompactNodeRef {
            data: &self.node_arena[offset as usize..],
            interner: &self.interner,
        })
    }

    /// Look up a way by ID, returning a zero-copy accessor.
    pub fn get_way(&self, id: i64) -> Option<CompactWayRef<'_>> {
        let &offset = self.way_index.get(&id)?;
        Some(CompactWayRef {
            data: &self.way_arena[offset as usize..],
            interner: &self.interner,
        })
    }

    /// Look up a relation by ID, returning a zero-copy accessor.
    pub fn get_relation(&self, id: i64) -> Option<CompactRelationRef<'_>> {
        let &offset = self.relation_index.get(&id)?;
        Some(CompactRelationRef {
            data: &self.relation_arena[offset as usize..],
            interner: &self.interner,
        })
    }

    /// Returns true if a node with this ID exists (not deleted).
    pub fn has_node(&self, id: i64) -> bool {
        self.node_index.contains_key(&id)
    }

    /// Returns true if a way with this ID exists (not deleted).
    pub fn has_way(&self, id: i64) -> bool {
        self.way_index.contains_key(&id)
    }

    /// Returns true if a relation with this ID exists (not deleted).
    pub fn has_relation(&self, id: i64) -> bool {
        self.relation_index.contains_key(&id)
    }

    /// Returns an iterator over all node IDs in the overlay.
    pub fn node_ids(&self) -> impl Iterator<Item = &i64> {
        self.node_index.keys()
    }

    /// Returns an iterator over all way IDs in the overlay.
    pub fn way_ids(&self) -> impl Iterator<Item = &i64> {
        self.way_index.keys()
    }

    /// Returns an iterator over all relation IDs in the overlay.
    pub fn relation_ids(&self) -> impl Iterator<Item = &i64> {
        self.relation_index.keys()
    }

    /// Returns the number of nodes in the overlay (not counting deleted).
    pub fn node_count(&self) -> usize {
        self.node_index.len()
    }

    /// Returns the number of ways in the overlay (not counting deleted).
    pub fn way_count(&self) -> usize {
        self.way_index.len()
    }

    /// Returns the number of relations in the overlay (not counting deleted).
    pub fn relation_count(&self) -> usize {
        self.relation_index.len()
    }

    /// Estimate the heap memory used by this overlay in bytes.
    ///
    /// Counts arena backing store, index HashMap overhead, deleted set overhead,
    /// and interner memory. Does not include the stack size of the struct itself.
    pub fn heap_size_estimate(&self) -> usize {
        let mut total: usize = 0;

        // Arenas
        total += self.node_arena.capacity();
        total += self.way_arena.capacity();
        total += self.relation_arena.capacity();

        // Indexes: FxHashMap<i64, u32>, each bucket is (i64, u32) + 1 control byte.
        let index_entry_size = std::mem::size_of::<(i64, u32)>() + 1;
        total += self.node_index.capacity() * index_entry_size;
        total += self.way_index.capacity() * index_entry_size;
        total += self.relation_index.capacity() * index_entry_size;

        // Deleted sets: HashSet<i64>, each bucket is i64 + 1 control byte.
        let delete_entry_size = std::mem::size_of::<i64>() + 1;
        total += self.deleted_nodes.capacity() * delete_entry_size;
        total += self.deleted_ways.capacity() * delete_entry_size;
        total += self.deleted_relations.capacity() * delete_entry_size;

        // Interner
        total += self.interner.heap_size_estimate();

        total
    }
}

impl Default for CompactDiffOverlay {
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
// Parser staging
// ---------------------------------------------------------------------------

/// Which element type is currently being parsed (between start and end tags).
#[derive(Clone, Copy, PartialEq, Eq)]
enum CurrentElem {
    None,
    Node,
    Way,
    Relation,
}

// ---------------------------------------------------------------------------
// Parser handler functions
// ---------------------------------------------------------------------------

/// Convert an OSC XML member type string ("node", "way", "relation") to
/// the crate's `MemberType` enum.
fn parse_member_type(s: &str) -> ParseResult<MemberType> {
    match s {
        "node" => Ok(MemberType::Node),
        "way" => Ok(MemberType::Way),
        "relation" => Ok(MemberType::Relation),
        other => Err(format!("unknown relation member type: '{other}'").into()),
    }
}

/// State carried through the parser loop. Extracted into a struct to keep
/// handler function signatures from exceeding the too_many_arguments lint.
struct ParserState {
    section: Section,
    current_elem: CurrentElem,
    current_id: i64,
    current_lat: i32,
    current_lon: i32,
    tag_keys: Vec<u32>,
    tag_values: Vec<String>,
    refs: Vec<i64>,
    members: Vec<(i64, u8, u32)>,
}

impl ParserState {
    fn new() -> Self {
        Self {
            section: Section::None,
            current_elem: CurrentElem::None,
            current_id: 0,
            current_lat: 0,
            current_lon: 0,
            tag_keys: Vec::new(),
            tag_values: Vec::new(),
            refs: Vec::new(),
            members: Vec::new(),
        }
    }

    fn clear_staging(&mut self) {
        self.tag_keys.clear();
        self.tag_values.clear();
        self.refs.clear();
        self.members.clear();
        self.current_elem = CurrentElem::None;
    }
}

/// Finalize the current element: build the tag slice, append to the appropriate
/// arena, insert into the index, and clear staging.
fn finalize_element(state: &mut ParserState, overlay: &mut CompactDiffOverlay) {
    let tags: Vec<(u32, &str)> = state
        .tag_keys
        .iter()
        .zip(state.tag_values.iter())
        .map(|(&k, v)| (k, v.as_str()))
        .collect();

    match state.current_elem {
        CurrentElem::Node => {
            let offset = arena_append_node(
                &mut overlay.node_arena,
                state.current_id,
                state.current_lat,
                state.current_lon,
                &tags,
            );
            overlay.node_index.insert(state.current_id, offset);
        }
        CurrentElem::Way => {
            let offset = arena_append_way(
                &mut overlay.way_arena,
                state.current_id,
                &state.refs,
                &tags,
            );
            overlay.way_index.insert(state.current_id, offset);
        }
        CurrentElem::Relation => {
            let offset = arena_append_relation(
                &mut overlay.relation_arena,
                state.current_id,
                &state.members,
                &tags,
            );
            overlay.relation_index.insert(state.current_id, offset);
        }
        CurrentElem::None => {}
    }

    state.clear_staging();
}

/// Handle the opening tag (or self-closing tag) for a node/way/relation element.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn handle_elem_start(
    e: &quick_xml::events::BytesStart,
    elem_kind: CurrentElem,
    is_empty: bool,
    state: &mut ParserState,
    overlay: &mut CompactDiffOverlay,
) -> ParseResult<()> {
    let id = parse_i64_attr(e, b"id")?;

    if state.section == Section::Delete {
        match elem_kind {
            CurrentElem::Node => {
                overlay.deleted_nodes.insert(id);
                overlay.node_index.remove(&id);
            }
            CurrentElem::Way => {
                overlay.deleted_ways.insert(id);
                overlay.way_index.remove(&id);
            }
            CurrentElem::Relation => {
                overlay.deleted_relations.insert(id);
                overlay.relation_index.remove(&id);
            }
            CurrentElem::None => {}
        }
        // For deletes, do not set current_elem (no child elements expected).
        return Ok(());
    }

    // Create/modify: remove from deleted sets if re-created.
    match elem_kind {
        CurrentElem::Node => {
            overlay.deleted_nodes.remove(&id);
        }
        CurrentElem::Way => {
            overlay.deleted_ways.remove(&id);
        }
        CurrentElem::Relation => {
            overlay.deleted_relations.remove(&id);
        }
        CurrentElem::None => {}
    }

    state.current_id = id;

    if elem_kind == CurrentElem::Node {
        let lat = parse_f64_attr(e, b"lat").unwrap_or(0.0);
        let lon = parse_f64_attr(e, b"lon").unwrap_or(0.0);
        state.current_lat = (lat * 1e7).round() as i32;
        state.current_lon = (lon * 1e7).round() as i32;
    }

    if is_empty {
        // Self-closing element: immediately finalize with empty tags/refs/members.
        state.current_elem = elem_kind;
        finalize_element(state, overlay);
    } else {
        state.current_elem = elem_kind;
    }

    Ok(())
}

/// Handle a `<tag k="..." v="..."/>` element.
fn handle_tag_compact(
    e: &quick_xml::events::BytesStart,
    state: &mut ParserState,
    overlay: &mut CompactDiffOverlay,
) -> ParseResult<()> {
    if state.current_elem == CurrentElem::None {
        return Ok(());
    }
    let k = parse_str_attr(e, b"k")?;
    let v = parse_str_attr(e, b"v")?;
    let key_id = overlay.interner.intern(&k);
    state.tag_keys.push(key_id);
    state.tag_values.push(v);
    Ok(())
}

/// Handle a `<nd ref="..."/>` element.
fn handle_nd_compact(
    e: &quick_xml::events::BytesStart,
    state: &mut ParserState,
) -> ParseResult<()> {
    if state.current_elem != CurrentElem::Way {
        return Ok(());
    }
    let ref_id = parse_i64_attr(e, b"ref")?;
    state.refs.push(ref_id);
    Ok(())
}

/// Handle a `<member type="..." ref="..." role="..."/>` element.
fn handle_member_compact(
    e: &quick_xml::events::BytesStart,
    state: &mut ParserState,
    overlay: &mut CompactDiffOverlay,
) -> ParseResult<()> {
    if state.current_elem != CurrentElem::Relation {
        return Ok(());
    }
    let member_type_str = parse_str_attr(e, b"type")?;
    let member_type = parse_member_type(&member_type_str)?;
    let ref_id = parse_i64_attr(e, b"ref")?;
    let role = parse_str_attr(e, b"role").unwrap_or_default();
    let type_byte = member_type_to_byte(member_type);
    let role_id = overlay.interner.intern(&role);
    state.members.push((ref_id, type_byte, role_id));
    Ok(())
}

/// Dispatch a Start event to the appropriate handler.
fn handle_start_event_compact(
    e: &quick_xml::events::BytesStart,
    state: &mut ParserState,
    overlay: &mut CompactDiffOverlay,
) -> ParseResult<()> {
    match e.name().as_ref() {
        b"create" => state.section = Section::Create,
        b"modify" => state.section = Section::Modify,
        b"delete" => state.section = Section::Delete,
        b"node" => handle_elem_start(e, CurrentElem::Node, false, state, overlay)?,
        b"way" => handle_elem_start(e, CurrentElem::Way, false, state, overlay)?,
        b"relation" => handle_elem_start(e, CurrentElem::Relation, false, state, overlay)?,
        b"tag" => handle_tag_compact(e, state, overlay)?,
        b"nd" => handle_nd_compact(e, state)?,
        b"member" => handle_member_compact(e, state, overlay)?,
        _ => {}
    }
    Ok(())
}

/// Dispatch an Empty (self-closing) event to the appropriate handler.
fn handle_empty_event_compact(
    e: &quick_xml::events::BytesStart,
    state: &mut ParserState,
    overlay: &mut CompactDiffOverlay,
) -> ParseResult<()> {
    match e.name().as_ref() {
        b"node" => handle_elem_start(e, CurrentElem::Node, true, state, overlay)?,
        b"way" => handle_elem_start(e, CurrentElem::Way, true, state, overlay)?,
        b"relation" => handle_elem_start(e, CurrentElem::Relation, true, state, overlay)?,
        b"tag" => handle_tag_compact(e, state, overlay)?,
        b"nd" => handle_nd_compact(e, state)?,
        b"member" => handle_member_compact(e, state, overlay)?,
        _ => {}
    }
    Ok(())
}

/// Dispatch an End event to the appropriate handler.
fn handle_end_event_compact(
    e: &quick_xml::events::BytesEnd,
    state: &mut ParserState,
    overlay: &mut CompactDiffOverlay,
) {
    match e.name().as_ref() {
        b"create" | b"modify" | b"delete" => state.section = Section::None,
        b"node" | b"way" | b"relation"
            if state.current_elem != CurrentElem::None =>
        {
            finalize_element(state, overlay);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Main parser
// ---------------------------------------------------------------------------

/// Parse a single .osc.gz file into an existing `CompactDiffOverlay`.
///
/// This is the streaming entry point: call it multiple times with the same
/// overlay to accumulate multiple diff files (later diffs win for conflicts).
pub fn parse_osc_file_into(path: &Path, overlay: &mut CompactDiffOverlay) -> ParseResult<()> {
    let file = File::open(path)?;
    let decoder = GzDecoder::new(file);
    let buf_reader = BufReader::new(decoder);
    let mut reader = Reader::from_reader(buf_reader);
    reader.config_mut().trim_text(true);

    let mut state = ParserState::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                handle_start_event_compact(e, &mut state, overlay)?;
            }
            Ok(Event::Empty(ref e)) => {
                handle_empty_event_compact(e, &mut state, overlay)?;
            }
            Ok(Event::End(ref e)) => {
                handle_end_event_compact(e, &mut state, overlay);
            }
            Ok(Event::Eof) => break,
            Ok(_) => {} // text, comments, decl, etc.
            Err(e) => return Err(Box::new(e)),
        }
        buf.clear();
    }

    Ok(())
}

/// Parse a single .osc.gz file into a new `CompactDiffOverlay`.
pub fn parse_osc_file(path: &Path) -> ParseResult<CompactDiffOverlay> {
    let mut overlay = CompactDiffOverlay::new();
    parse_osc_file_into(path, &mut overlay)?;
    Ok(overlay)
}

// ---------------------------------------------------------------------------
// Load all diffs from a directory
// ---------------------------------------------------------------------------

/// Parse the numeric sequence number from an OSC filename.
/// E.g. "4705.osc.gz" -> stem "4705.osc" -> strip ".osc" -> 4705.
fn parse_sequence_number(filename: &str) -> Option<u64> {
    let stem = filename.strip_suffix(".gz")?;
    let num_str = stem.strip_suffix(".osc")?;
    num_str.parse::<u64>().ok()
}

/// Load all .osc.gz diffs from a directory, sorted by sequence number, and
/// parse them into a single `CompactDiffOverlay`. Later diffs win for conflicts
/// because `parse_osc_file_into` overwrites existing entries.
pub fn load_all_diffs(diffs_dir: &Path) -> ParseResult<CompactDiffOverlay> {
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

    let mut overlay = CompactDiffOverlay::new();
    let total = entries.len();

    for (i, (seq, path)) in entries.iter().enumerate() {
        eprintln!(
            "[{}/{}] Parsing diff {} (sequence {seq})...",
            i + 1,
            total,
            path.display()
        );
        parse_osc_file_into(path, &mut overlay)?;
    }

    eprintln!(
        "Loaded {total} diffs: {} nodes, {} ways, {} relations \
         ({} deleted nodes, {} deleted ways, {} deleted relations)",
        overlay.node_count(),
        overlay.way_count(),
        overlay.relation_count(),
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
        let overlay = parse_osc_file(&dir.join("test.osc.gz"))?;

        // Modified node should overwrite created node
        let node = overlay
            .get_node(100)
            .ok_or("node 100 should exist in overlay")?;
        // lat=55.68 -> decimicro = 556800000, but f64 rounding may be +-1
        assert!((node.decimicro_lat() - 556_800_000).abs() <= 1);
        assert!((node.decimicro_lon() - 125_700_000).abs() <= 1);
        let tags: Vec<(&str, &str)> = node.tags().collect();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].0, "name");
        assert_eq!(tags[0].1, "CPH");

        // Deleted way
        assert!(overlay.deleted_ways.contains(&200));
        assert!(!overlay.has_way(200));

        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn test_merge_later_wins() -> ParseResult<()> {
        // Parse create first, then modify into the same overlay.
        let dir = make_test_dir("merge_later_wins");

        let xml_create = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="100" lat="1.0" lon="2.0" version="1"/>
  </create>
</osmChange>"#;

        let xml_modify = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="100" lat="3.0" lon="4.0" version="2"/>
  </modify>
</osmChange>"#;

        write_osc_gz(&dir, "001.osc.gz", xml_create);
        write_osc_gz(&dir, "002.osc.gz", xml_modify);

        let mut overlay = CompactDiffOverlay::new();
        parse_osc_file_into(&dir.join("001.osc.gz"), &mut overlay)?;
        parse_osc_file_into(&dir.join("002.osc.gz"), &mut overlay)?;

        let node = overlay.get_node(100).ok_or("node 100 should exist after merge")?;
        // lat=3.0 -> decimicro = 30000000
        assert_eq!(node.decimicro_lat(), 30_000_000);
        assert_eq!(node.decimicro_lon(), 40_000_000);

        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn test_merge_delete_removes_create() -> ParseResult<()> {
        let dir = make_test_dir("delete_removes_create");

        let xml_create = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="100" lat="1.0" lon="2.0" version="1"/>
  </create>
</osmChange>"#;

        let xml_delete = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <delete>
    <node id="100" version="2"/>
  </delete>
</osmChange>"#;

        write_osc_gz(&dir, "001.osc.gz", xml_create);
        write_osc_gz(&dir, "002.osc.gz", xml_delete);

        let mut overlay = CompactDiffOverlay::new();
        parse_osc_file_into(&dir.join("001.osc.gz"), &mut overlay)?;
        parse_osc_file_into(&dir.join("002.osc.gz"), &mut overlay)?;

        assert!(!overlay.has_node(100));
        assert!(overlay.deleted_nodes.contains(&100));

        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn test_merge_create_removes_delete() -> ParseResult<()> {
        let dir = make_test_dir("create_removes_delete");

        let xml_delete = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <delete>
    <node id="100" version="1"/>
  </delete>
</osmChange>"#;

        let xml_create = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="100" lat="1.0" lon="2.0" version="2"/>
  </create>
</osmChange>"#;

        write_osc_gz(&dir, "001.osc.gz", xml_delete);
        write_osc_gz(&dir, "002.osc.gz", xml_create);

        let mut overlay = CompactDiffOverlay::new();
        parse_osc_file_into(&dir.join("001.osc.gz"), &mut overlay)?;
        parse_osc_file_into(&dir.join("002.osc.gz"), &mut overlay)?;

        assert!(overlay.has_node(100));
        assert!(!overlay.deleted_nodes.contains(&100));

        std::fs::remove_dir_all(&dir)?;
        Ok(())
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

        let overlay = load_all_diffs(&dir)?;

        // Node 1 should have been created by 999, then modified by 10000
        let node1 = overlay
            .get_node(1)
            .ok_or("node 1 should exist after loading diffs")?;
        assert_eq!(node1.decimicro_lat(), 100_000_000);
        assert_eq!(node1.decimicro_lon(), 100_000_000);

        // Node 2 from 4705 should exist
        assert!(overlay.has_node(2));

        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn test_empty_overlay() {
        let overlay = CompactDiffOverlay::new();
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
        let overlay = parse_osc_file(&dir.join("test.osc.gz"))?;

        assert!(overlay.deleted_nodes.contains(&123));
        assert!(!overlay.has_node(123));

        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // New tests: StringInterner
    // -----------------------------------------------------------------------

    #[test]
    fn test_interner_roundtrip() {
        let mut interner = StringInterner::new();
        let id_hello = interner.intern("hello");
        let id_world = interner.intern("world");
        let id_empty = interner.intern("");

        assert_eq!(interner.resolve(id_hello), "hello");
        assert_eq!(interner.resolve(id_world), "world");
        assert_eq!(interner.resolve(id_empty), "");
        assert_eq!(id_empty, 0); // empty string is always intern_id 0
    }

    #[test]
    fn test_interner_dedup() {
        let mut interner = StringInterner::new();
        let id1 = interner.intern("highway");
        let id2 = interner.intern("highway");
        let id3 = interner.intern("name");

        assert_eq!(id1, id2); // same string -> same id
        assert_ne!(id1, id3); // different string -> different id
    }

    // -----------------------------------------------------------------------
    // New tests: arena roundtrips
    // -----------------------------------------------------------------------

    #[test]
    fn test_node_roundtrip() {
        let mut interner = StringInterner::new();
        let key_name = interner.intern("name");
        let key_place = interner.intern("place");

        let mut arena = Vec::new();
        let tags: Vec<(u32, &str)> = vec![(key_name, "Test City"), (key_place, "city")];
        let offset = arena_append_node(&mut arena, 42, 556_800_000, 125_700_000, &tags);

        let node = CompactNodeRef {
            data: &arena[offset as usize..],
            interner: &interner,
        };

        assert_eq!(node.id(), 42);
        assert_eq!(node.decimicro_lat(), 556_800_000);
        assert_eq!(node.decimicro_lon(), 125_700_000);
        assert_eq!(node.tag_count(), 2);

        let tag_vec: Vec<(&str, &str)> = node.tags().collect();
        assert_eq!(tag_vec[0], ("name", "Test City"));
        assert_eq!(tag_vec[1], ("place", "city"));
    }

    #[test]
    fn test_way_roundtrip() {
        let mut interner = StringInterner::new();
        let key_highway = interner.intern("highway");

        let mut arena = Vec::new();
        let refs = vec![1, 2, 3, 4, 5];
        let tags: Vec<(u32, &str)> = vec![(key_highway, "residential")];
        let offset = arena_append_way(&mut arena, 99, &refs, &tags);

        let way = CompactWayRef {
            data: &arena[offset as usize..],
            interner: &interner,
        };

        assert_eq!(way.id(), 99);
        assert_eq!(way.ref_count(), 5);
        assert_eq!(way.tag_count(), 1);

        let ref_vec: Vec<i64> = way.refs().collect();
        assert_eq!(ref_vec, vec![1, 2, 3, 4, 5]);

        let tag_vec: Vec<(&str, &str)> = way.tags().collect();
        assert_eq!(tag_vec[0], ("highway", "residential"));
    }

    #[test]
    fn test_relation_roundtrip() {
        let mut interner = StringInterner::new();
        let key_type = interner.intern("type");
        let role_outer = interner.intern("outer");
        let role_inner = interner.intern("inner");

        let mut arena = Vec::new();
        let members = vec![
            (10, member_type_to_byte(MemberType::Way), role_outer),
            (20, member_type_to_byte(MemberType::Way), role_inner),
            (30, member_type_to_byte(MemberType::Node), interner.intern("")),
        ];
        let tags: Vec<(u32, &str)> = vec![(key_type, "multipolygon")];
        let offset = arena_append_relation(&mut arena, 500, &members, &tags);

        let rel = CompactRelationRef {
            data: &arena[offset as usize..],
            interner: &interner,
        };

        assert_eq!(rel.id(), 500);
        assert_eq!(rel.member_count(), 3);
        assert_eq!(rel.tag_count(), 1);

        let member_vec: Vec<(MemberType, i64, &str)> = rel.members().collect();
        assert_eq!(member_vec[0], (MemberType::Way, 10, "outer"));
        assert_eq!(member_vec[1], (MemberType::Way, 20, "inner"));
        assert_eq!(member_vec[2], (MemberType::Node, 30, ""));

        let tag_vec: Vec<(&str, &str)> = rel.tags().collect();
        assert_eq!(tag_vec[0], ("type", "multipolygon"));
    }

    /// F57: End-to-end test parsing an OSC file with way `<nd>` children
    /// and relation `<member>` children through `parse_osc_file`.
    #[test]
    fn test_parse_osc_way_and_relation_children() -> ParseResult<()> {
        let dir = make_test_dir("way_relation_children");

        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <way id="100" version="1">
      <nd ref="1"/>
      <nd ref="2"/>
      <nd ref="3"/>
      <tag k="highway" v="residential"/>
      <tag k="name" v="Main Street"/>
    </way>
    <relation id="200" version="1">
      <member type="way" ref="100" role="outer"/>
      <member type="way" ref="101" role="inner"/>
      <member type="node" ref="1" role="label"/>
      <tag k="type" v="multipolygon"/>
    </relation>
  </create>
  <modify>
    <way id="300" version="5">
      <nd ref="10"/>
      <nd ref="11"/>
      <tag k="highway" v="primary"/>
    </way>
  </modify>
</osmChange>"#;

        write_osc_gz(&dir, "children.osc.gz", xml);
        let overlay = parse_osc_file(&dir.join("children.osc.gz"))?;

        // Way 100: 3 refs, 2 tags
        let way100 = overlay.get_way(100).ok_or("way 100 should exist")?;
        let refs: Vec<i64> = way100.refs().collect();
        assert_eq!(refs, vec![1, 2, 3]);
        let tags: Vec<(&str, &str)> = way100.tags().collect();
        assert_eq!(tags, vec![("highway", "residential"), ("name", "Main Street")]);

        // Way 300: 2 refs, 1 tag (modify)
        let way300 = overlay.get_way(300).ok_or("way 300 should exist")?;
        let refs: Vec<i64> = way300.refs().collect();
        assert_eq!(refs, vec![10, 11]);
        let tags: Vec<(&str, &str)> = way300.tags().collect();
        assert_eq!(tags, vec![("highway", "primary")]);

        // Relation 200: 3 members, 1 tag
        let rel = overlay.get_relation(200).ok_or("relation 200 should exist")?;
        let members: Vec<(MemberType, i64, &str)> = rel.members().collect();
        assert_eq!(members.len(), 3);
        assert_eq!(members[0], (MemberType::Way, 100, "outer"));
        assert_eq!(members[1], (MemberType::Way, 101, "inner"));
        assert_eq!(members[2], (MemberType::Node, 1, "label"));
        let tags: Vec<(&str, &str)> = rel.tags().collect();
        assert_eq!(tags, vec![("type", "multipolygon")]);

        drop(std::fs::remove_dir_all(&dir));
        Ok(())
    }
}
