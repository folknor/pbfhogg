// OSC (.osc.gz) parser for OpenStreetMap change files.
//
// Parses Geofabrik-style replication diffs into a `CompactDiffOverlay` that tracks
// created, modified, and deleted nodes/ways/relations using arena-packed binary
// layouts with interned tag keys and relation member roles.

use std::collections::HashSet;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use flate2::read::MultiGzDecoder;
use quick_xml::Reader;
use quick_xml::events::Event;
use rustc_hash::FxHashMap;

use super::ParseResult;
use super::compact::{ArenaMeta, arena_append_node, arena_append_relation, arena_append_way};
use super::interner::StringInterner;
use super::xml_parse::{
    ParserState, handle_empty_event_compact, handle_end_event_compact, handle_start_event_compact,
};

pub use super::compact::{
    CompactMemberIter, CompactNodeRef, CompactRefIter, CompactRelationRef, CompactTagIter,
    CompactWayRef,
};

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

    // -----------------------------------------------------------------------
    // Mutators (crate-internal)
    //
    // Each add/delete pair maintains a load-bearing invariant: an arena write
    // must be paired with the corresponding index insert, and a delete-set
    // insert must be paired with an index remove. Exposing these as methods
    // rather than `pub(super)` fields keeps callers (the XML state machine
    // in `xml_parse.rs`) from having to remember the pairing.
    // -----------------------------------------------------------------------

    /// Append a node to the arena and register it in the node index.
    #[inline]
    pub(super) fn push_node(
        &mut self,
        id: i64,
        lat: i32,
        lon: i32,
        tags: &[(u32, &str)],
        meta: &ArenaMeta,
    ) {
        let offset = arena_append_node(&mut self.node_arena, id, lat, lon, tags, meta);
        self.node_index.insert(id, offset);
    }

    /// Append a way to the arena and register it in the way index.
    #[inline]
    pub(super) fn push_way(
        &mut self,
        id: i64,
        refs: &[i64],
        tags: &[(u32, &str)],
        meta: &ArenaMeta,
    ) {
        let offset = arena_append_way(&mut self.way_arena, id, refs, tags, meta);
        self.way_index.insert(id, offset);
    }

    /// Append a relation to the arena and register it in the relation index.
    #[inline]
    pub(super) fn push_relation(
        &mut self,
        id: i64,
        members: &[(i64, u8, u32)],
        tags: &[(u32, &str)],
        meta: &ArenaMeta,
    ) {
        let offset = arena_append_relation(&mut self.relation_arena, id, members, tags, meta);
        self.relation_index.insert(id, offset);
    }

    /// Mark a node as deleted and drop any live entry from the node index.
    #[inline]
    pub(super) fn delete_node(&mut self, id: i64) {
        self.deleted_nodes.insert(id);
        self.node_index.remove(&id);
    }

    /// Mark a way as deleted and drop any live entry from the way index.
    #[inline]
    pub(super) fn delete_way(&mut self, id: i64) {
        self.deleted_ways.insert(id);
        self.way_index.remove(&id);
    }

    /// Mark a relation as deleted and drop any live entry from the relation index.
    #[inline]
    pub(super) fn delete_relation(&mut self, id: i64) {
        self.deleted_relations.insert(id);
        self.relation_index.remove(&id);
    }

    /// Intern a tag key or relation member role, returning its interned ID.
    #[inline]
    pub(super) fn intern(&mut self, s: &str) -> u32 {
        self.interner.intern(s)
    }
}

impl Default for CompactDiffOverlay {
    fn default() -> Self {
        Self::new()
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
    let decoder = MultiGzDecoder::new(file);
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
    use super::super::compact::{
        arena_append_node, arena_append_relation, arena_append_way, member_type_to_byte,
    };
    use super::*;
    use crate::read::elements::MemberType;
    use flate2::Compression;
    use flate2::write::GzEncoder;
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

    /// Element metadata attributes survive the parse into the overlay and
    /// come back through the ref accessors; elements without any metadata
    /// attribute report `None`.
    #[test]
    fn test_parse_osc_element_metadata() -> ParseResult<()> {
        let dir = make_test_dir("element_metadata");

        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="100" lat="55.6761" lon="12.5683" version="7" timestamp="1970-01-02T00:00:00Z" changeset="123456" uid="42" user="mapper one"/>
    <way id="200">
      <nd ref="100"/>
    </way>
    <relation id="300" version="2" timestamp="2026-02-20T21:39:49Z">
      <member type="way" ref="200" role="outer"/>
    </relation>
  </modify>
</osmChange>"#;

        write_osc_gz(&dir, "meta.osc.gz", xml);
        let overlay = parse_osc_file(&dir.join("meta.osc.gz"))?;

        let node = overlay.get_node(100).ok_or("node 100 missing")?;
        let meta = node.metadata().ok_or("node 100 metadata missing")?;
        assert_eq!(meta.version, 7);
        assert_eq!(meta.timestamp, 86_400);
        assert_eq!(meta.changeset, 123_456);
        assert_eq!(meta.uid, 42);
        assert_eq!(meta.user, "mapper one");
        assert!(meta.visible);

        // No metadata attributes at all -> None.
        let way = overlay.get_way(200).ok_or("way 200 missing")?;
        assert!(way.metadata().is_none());
        // Metadata does not disturb the variable-length sections.
        assert_eq!(way.refs().collect::<Vec<_>>(), vec![100]);

        // Partial metadata: version + timestamp only; user absent -> "".
        let rel = overlay.get_relation(300).ok_or("relation 300 missing")?;
        let rmeta = rel.metadata().ok_or("relation 300 metadata missing")?;
        assert_eq!(rmeta.version, 2);
        assert_eq!(
            rmeta.timestamp,
            crate::commands::parse_rfc3339_utc("2026-02-20T21:39:49Z")
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?
        );
        assert_eq!(rmeta.changeset, 0);
        assert_eq!(rmeta.user, "");
        let members: Vec<_> = rel.members().collect();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].2, "outer");

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

        let node = overlay
            .get_node(100)
            .ok_or("node 100 should exist after merge")?;
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
        let offset = arena_append_node(
            &mut arena,
            42,
            556_800_000,
            125_700_000,
            &tags,
            &ArenaMeta::default(),
        );

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
        let offset = arena_append_way(&mut arena, 99, &refs, &tags, &ArenaMeta::default());

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
            (
                30,
                member_type_to_byte(MemberType::Node),
                interner.intern(""),
            ),
        ];
        let tags: Vec<(u32, &str)> = vec![(key_type, "multipolygon")];
        let offset = arena_append_relation(&mut arena, 500, &members, &tags, &ArenaMeta::default());

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
        assert_eq!(
            tags,
            vec![("highway", "residential"), ("name", "Main Street")]
        );

        // Way 300: 2 refs, 1 tag (modify)
        let way300 = overlay.get_way(300).ok_or("way 300 should exist")?;
        let refs: Vec<i64> = way300.refs().collect();
        assert_eq!(refs, vec![10, 11]);
        let tags: Vec<(&str, &str)> = way300.tags().collect();
        assert_eq!(tags, vec![("highway", "primary")]);

        // Relation 200: 3 members, 1 tag
        let rel = overlay
            .get_relation(200)
            .ok_or("relation 200 should exist")?;
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
