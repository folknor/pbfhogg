// OSC (.osc.gz) parser for OpenStreetMap change files.
//
// Parses Geofabrik-style replication diffs into a `CompactDiffOverlay` that tracks
// created, modified, and deleted nodes/ways/relations using arena-packed binary
// layouts with interned tag keys and relation member roles.

use std::collections::HashSet;
use std::path::Path;

use rustc_hash::FxHashMap;

use crate::ElementKind;

use super::ParseResult;
use super::compact::{
    ArenaMeta, NODE_META_OFF, RELATION_META_OFF, WAY_META_OFF, arena_append_node,
    arena_append_relation, arena_append_way, version_at,
};
use super::interner::StringInterner;
use super::version_floor::VersionFloor;
use super::xml_parse::{CompactSink, parse_osc_path};

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
/// element data is packed into flat `Vec<u8>` arenas with a `FxHashMap<i64, usize>`
/// index mapping element IDs to byte offsets. Tag keys and relation member roles
/// are interned via `StringInterner` to eliminate duplicate string storage.
///
/// This avoids per-element heap allocations and reuses repeated tag keys and
/// relation member roles.
///
/// Storage is bounded by parsed payload, not surviving object count: replacing
/// or deleting an object leaves its old arena record and interned strings
/// (including user names) allocated. Append-only storage avoids moving records
/// and rebuilding offsets during parsing. Compaction would need to rewrite all
/// live offsets and intern IDs, with a second live arena at peak or an in-place
/// relocation algorithm. Keep that cost off the measured daily-diff path until
/// repeated-update catch-up inputs justify it; splitting a catch-up into smaller
/// apply operations bounds each overlay's history without changing this layout.
pub struct CompactDiffOverlay {
    node_arena: Vec<u8>,
    way_arena: Vec<u8>,
    relation_arena: Vec<u8>,
    // Arena offsets are `usize`, not `u32`. A per-kind arena crossing 4 GiB
    // is reachable on a catch-up diff or a `load_all_diffs` accumulation
    // (node records are 49 B plus tags, so ~85 M node changes), and a
    // truncating `u32` offset points the index at an arbitrary earlier
    // record - `get_node` then hands back another element's bytes and
    // apply-changes writes them under this ID. `(i64, usize)` and
    // `(i64, u32)` are both 16 bytes after padding, so the width costs
    // nothing.
    node_index: FxHashMap<i64, usize>,
    way_index: FxHashMap<i64, usize>,
    relation_index: FxHashMap<i64, usize>,
    deleted_nodes: HashSet<i64>,
    deleted_ways: HashSet<i64>,
    deleted_relations: HashSet<i64>,
    // Version floors: history the live record no longer holds. The gate
    // reads the stored version from the live record, and two transitions
    // throw that away. A delete drops the live index entry: without a
    // floor, delete node 1 version 9 then modify node 1 version 8
    // resurrects version 8. An unversioned create or modify overwrites
    // the live record with one that claims nothing: without a floor,
    // modify version 9, an unversioned modify, then modify version 8 is
    // accepted. Each of those transitions folds the version it is about
    // to lose (and a delete's own claim) into these maps, so the gate
    // compares against the highest version ever claimed for the id. Only
    // ids that went through such a transition are stored, so the cost is
    // proportional to the delete and unversioned-change count, not the
    // change count. Entries are never removed: a recreated object keeps
    // its floor.
    node_version_floors: FxHashMap<i64, i32>,
    way_version_floors: FxHashMap<i64, i32>,
    relation_version_floors: FxHashMap<i64, i32>,
    interner: StringInterner,
    /// When false (the default), an incoming change whose `version` is
    /// lower than the highest version already claimed for the id is a
    /// hard error. See [`CompactDiffOverlay::allow_version_regression`].
    allow_version_regression: bool,
}

/// A read-only view of one element kind's storage: the arena, the live
/// index, the version floors, and where the metadata block sits in a
/// record of that kind. Bundled because the version-regression gate
/// needs all four together and each of the three kinds supplies its own.
#[derive(Clone, Copy)]
struct KindStore<'a> {
    arena: &'a [u8],
    index: &'a FxHashMap<i64, usize>,
    floors: &'a FxHashMap<i64, i32>,
    meta_off: usize,
}

impl KindStore<'_> {
    /// The highest version this overlay already holds for `id`, from the
    /// live record or the floor map, or `None` when nothing stored for it
    /// ever claimed one.
    fn stored_version(&self, id: i64) -> Option<i32> {
        let live = self
            .index
            .get(&id)
            .and_then(|&offset| version_at(self.arena, offset, self.meta_off));
        let dead = self.floors.get(&id).copied();
        match (live, dead) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }
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
            node_version_floors: FxHashMap::default(),
            way_version_floors: FxHashMap::default(),
            relation_version_floors: FxHashMap::default(),
            interner: StringInterner::new(),
            allow_version_regression: false,
        }
    }

    /// Opt out of the version-regression gate.
    ///
    /// The overlay's conflict rule is purely positional: the last change
    /// parsed into it wins, whatever version it claims. That is fine when
    /// the changes arrive in order and is silent data corruption when
    /// they do not - an older OSC after a newer one downgrades objects to
    /// earlier versions and reports success. The overlay already stores
    /// the version, so by default it refuses: an incoming change whose
    /// version is lower than the highest one held for the object is an
    /// error naming both. An equal version is admitted and wins by
    /// position: regional extract diffs delete an object leaving the
    /// region at the version it already had, and overlapping diffs repeat
    /// boundary changes, so equal versions are normal input and carry no
    /// ordering of their own.
    ///
    /// Set this to restore the old positional behaviour, for the case
    /// where a caller genuinely intends to replay history backwards.
    ///
    /// The flag may be toggled between parses, but history is only fully
    /// kept while the gate is on. With the gate off, a create or modify
    /// is neither checked nor recorded beyond its own live record, so a
    /// versioned overwrite replaces the live version outright and an
    /// unversioned one does not carry the old version into the floor;
    /// deletes still record their floor either way. Turning the gate back
    /// on therefore enforces against the highest version still held -
    /// the live record and the floors - which can be lower than a version
    /// admitted while the gate was off.
    pub fn allow_version_regression(&mut self, allow: bool) {
        self.allow_version_regression = allow;
    }

    /// Refuse an incoming change that does not move the object forward.
    ///
    /// Both sides must claim a version for the comparison to mean
    /// anything: a change with no `version` attribute makes no claim, and
    /// neither does a stored record that had none. Only an actual
    /// backwards claim is refused; an equal one is admitted.
    ///
    /// "Stored" spans both the live index and the version floor map, so
    /// a delete is as much a version claim as a create is, and an
    /// unversioned change in between does not erase either.
    ///
    /// Takes the incoming claim already unwrapped: the callers branch on
    /// whether the change claims a version anyway (an unversioned create
    /// or modify carries the live version into the floor instead), so a
    /// versioned change pays one claim test, one flag test and one
    /// `stored_version` call (live index plus floor map), as it did when
    /// this function did the unwrapping.
    fn check_version_advance(
        &self,
        kind: ElementKind,
        id: i64,
        store: KindStore<'_>,
        incoming_version: i32,
    ) -> ParseResult<()> {
        if self.allow_version_regression {
            return Ok(());
        }
        let Err(stored) = VersionFloor::new(store.stored_version(id)).admit(Some(incoming_version))
        else {
            return Ok(());
        };
        Err(crate::error::new_version_regression(
            "OSC change overlay",
            kind,
            id,
            incoming_version,
            stored,
        ))
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
            data: &self.node_arena[offset..],
            interner: &self.interner,
        })
    }

    /// Look up a way by ID, returning a zero-copy accessor.
    pub fn get_way(&self, id: i64) -> Option<CompactWayRef<'_>> {
        let &offset = self.way_index.get(&id)?;
        Some(CompactWayRef {
            data: &self.way_arena[offset..],
            interner: &self.interner,
        })
    }

    /// Look up a relation by ID, returning a zero-copy accessor.
    pub fn get_relation(&self, id: i64) -> Option<CompactRelationRef<'_>> {
        let &offset = self.relation_index.get(&id)?;
        Some(CompactRelationRef {
            data: &self.relation_arena[offset..],
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
    /// and interner memory. Includes superseded records, unused capacity and
    /// strings no surviving object references: these still consume heap, so
    /// subtracting them would underreport retained memory. This is an allocated
    /// storage estimate, not a live-payload estimate or allocator/RSS accounting.
    /// Does not include the stack size of the struct itself.
    pub fn heap_size_estimate(&self) -> usize {
        let mut total: usize = 0;

        // Arenas
        total += self.node_arena.capacity();
        total += self.way_arena.capacity();
        total += self.relation_arena.capacity();

        // Indexes: FxHashMap<i64, usize>, each bucket is (i64, usize) + 1 control byte.
        let index_entry_size = std::mem::size_of::<(i64, usize)>() + 1;
        total += self.node_index.capacity() * index_entry_size;
        total += self.way_index.capacity() * index_entry_size;
        total += self.relation_index.capacity() * index_entry_size;

        // Deleted sets: HashSet<i64>, each bucket is i64 + 1 control byte.
        let delete_entry_size = std::mem::size_of::<i64>() + 1;
        total += self.deleted_nodes.capacity() * delete_entry_size;
        total += self.deleted_ways.capacity() * delete_entry_size;
        total += self.deleted_relations.capacity() * delete_entry_size;

        // Version floors: FxHashMap<i64, i32>, (i64, i32) + control byte.
        let floor_entry_size = std::mem::size_of::<(i64, i32)>() + 1;
        total += self.node_version_floors.capacity() * floor_entry_size;
        total += self.way_version_floors.capacity() * floor_entry_size;
        total += self.relation_version_floors.capacity() * floor_entry_size;

        // Interner
        total += self.interner.heap_size_estimate();

        total
    }

    /// Deleted IDs by OSM object kind, without mutable access to overlay state.
    pub fn deleted_ids(&self, kind: ElementKind) -> &HashSet<i64> {
        match kind {
            ElementKind::Node => &self.deleted_nodes,
            ElementKind::Way => &self.deleted_ways,
            ElementKind::Relation => &self.deleted_relations,
        }
    }

    /// Whether the final change for an object deletes it.
    pub fn is_deleted(&self, kind: ElementKind, id: i64) -> bool {
        self.deleted_ids(kind).contains(&id)
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

    /// The node kind's storage as one view for the version gate.
    #[inline]
    fn node_store(&self) -> KindStore<'_> {
        KindStore {
            arena: &self.node_arena,
            index: &self.node_index,
            floors: &self.node_version_floors,
            meta_off: NODE_META_OFF,
        }
    }

    /// The way kind's storage as one view for the version gate.
    #[inline]
    fn way_store(&self) -> KindStore<'_> {
        KindStore {
            arena: &self.way_arena,
            index: &self.way_index,
            floors: &self.way_version_floors,
            meta_off: WAY_META_OFF,
        }
    }

    /// The relation kind's storage as one view for the version gate.
    #[inline]
    fn relation_store(&self) -> KindStore<'_> {
        KindStore {
            arena: &self.relation_arena,
            index: &self.relation_index,
            floors: &self.relation_version_floors,
            meta_off: RELATION_META_OFF,
        }
    }

    /// Append a node to the arena and register it in the node index.
    #[inline]
    pub(super) fn push_node(
        &mut self,
        id: i64,
        lat: i32,
        lon: i32,
        tags: &[(u32, &str)],
        meta: &ArenaMeta,
    ) -> ParseResult<()> {
        if let Some(claim) = meta.version_claim() {
            self.check_version_advance(ElementKind::Node, id, self.node_store(), claim)?;
        } else if !self.allow_version_regression {
            let carried = self.node_store().stored_version(id);
            Self::raise_floor(&mut self.node_version_floors, id, carried, meta);
        }
        let offset = arena_append_node(&mut self.node_arena, id, lat, lon, tags, meta)?;
        self.node_index.insert(id, offset);
        self.deleted_nodes.remove(&id);
        Ok(())
    }

    /// Append a way to the arena and register it in the way index.
    #[inline]
    pub(super) fn push_way(
        &mut self,
        id: i64,
        refs: &[i64],
        tags: &[(u32, &str)],
        meta: &ArenaMeta,
    ) -> ParseResult<()> {
        if let Some(claim) = meta.version_claim() {
            self.check_version_advance(ElementKind::Way, id, self.way_store(), claim)?;
        } else if !self.allow_version_regression {
            let carried = self.way_store().stored_version(id);
            Self::raise_floor(&mut self.way_version_floors, id, carried, meta);
        }
        let offset = arena_append_way(&mut self.way_arena, id, refs, tags, meta)?;
        self.way_index.insert(id, offset);
        self.deleted_ways.remove(&id);
        Ok(())
    }

    /// Append a relation to the arena and register it in the relation index.
    #[inline]
    pub(super) fn push_relation(
        &mut self,
        id: i64,
        members: &[(i64, u8, u32)],
        tags: &[(u32, &str)],
        meta: &ArenaMeta,
    ) -> ParseResult<()> {
        if let Some(claim) = meta.version_claim() {
            self.check_version_advance(ElementKind::Relation, id, self.relation_store(), claim)?;
        } else if !self.allow_version_regression {
            let carried = self.relation_store().stored_version(id);
            Self::raise_floor(&mut self.relation_version_floors, id, carried, meta);
        }
        let offset = arena_append_relation(&mut self.relation_arena, id, members, tags, meta)?;
        self.relation_index.insert(id, offset);
        self.deleted_relations.remove(&id);
        Ok(())
    }

    /// Mark a node as deleted and drop any live entry from the node index.
    ///
    /// The delete is version-gated exactly like a create or modify, and
    /// records a version floor so a later stale change still has a
    /// version to be measured against.
    #[inline]
    pub(super) fn delete_node(&mut self, id: i64, meta: &ArenaMeta) -> ParseResult<()> {
        if let Some(claim) = meta.version_claim() {
            self.check_version_advance(ElementKind::Node, id, self.node_store(), claim)?;
        }
        let carried = self.node_store().stored_version(id);
        Self::raise_floor(&mut self.node_version_floors, id, carried, meta);
        self.deleted_nodes.insert(id);
        self.node_index.remove(&id);
        Ok(())
    }

    /// Mark a way as deleted and drop any live entry from the way index.
    #[inline]
    pub(super) fn delete_way(&mut self, id: i64, meta: &ArenaMeta) -> ParseResult<()> {
        if let Some(claim) = meta.version_claim() {
            self.check_version_advance(ElementKind::Way, id, self.way_store(), claim)?;
        }
        let carried = self.way_store().stored_version(id);
        Self::raise_floor(&mut self.way_version_floors, id, carried, meta);
        self.deleted_ways.insert(id);
        self.way_index.remove(&id);
        Ok(())
    }

    /// Mark a relation as deleted and drop any live entry from the relation index.
    #[inline]
    pub(super) fn delete_relation(&mut self, id: i64, meta: &ArenaMeta) -> ParseResult<()> {
        if let Some(claim) = meta.version_claim() {
            self.check_version_advance(ElementKind::Relation, id, self.relation_store(), claim)?;
        }
        let carried = self.relation_store().stored_version(id);
        Self::raise_floor(&mut self.relation_version_floors, id, carried, meta);
        self.deleted_relations.insert(id);
        self.relation_index.remove(&id);
        Ok(())
    }

    /// Fold the version a transition is about to lose into the id's
    /// floor: the version the live record held (live or already floored)
    /// before a delete or unversioned overwrite, and the incoming
    /// change's own claim, if any.
    #[inline]
    fn raise_floor(
        floors: &mut FxHashMap<i64, i32>,
        id: i64,
        carried: Option<i32>,
        meta: &ArenaMeta,
    ) {
        let highest = match (carried, meta.version_claim()) {
            (Some(a), Some(b)) => a.max(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => return,
        };
        floors
            .entry(id)
            .and_modify(|v| *v = (*v).max(highest))
            .or_insert(highest);
    }

    /// Intern a tag key or relation member role, returning its interned ID.
    #[inline]
    pub(super) fn intern(&mut self, s: &str) -> ParseResult<u32> {
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

/// Parse a single OSC file into an existing `CompactDiffOverlay`.
///
/// Accepts both gzip-compressed (`.osc.gz`) and plain (`.osc`) input -
/// the format is sniffed from the file's leading bytes, not its name.
///
/// This is the streaming entry point: call it multiple times with the same
/// overlay to accumulate multiple diff files. Later diffs win for
/// conflicts, but only when they do not move the object backwards: see
/// [`CompactDiffOverlay::allow_version_regression`].
pub fn parse_osc_file_into(path: &Path, overlay: &mut CompactDiffOverlay) -> ParseResult<()> {
    parse_osc_file_into_until(path, overlay, &crate::host::Invocation::new().call())
}

/// [`parse_osc_file_into`] that stops at the next read of the input once
/// `cancel` is tripped (`ErrorKind::Cancelled`).
pub(crate) fn parse_osc_file_into_until(
    path: &Path,
    overlay: &mut CompactDiffOverlay,
    cancel: &crate::host::Recorder,
) -> ParseResult<()> {
    let mut sink = CompactSink::new(overlay);
    // Deliberately use the shared historical per-file decompression counter.
    // Command-specific names or accumulation would break brokkr comparisons.
    parse_osc_path(path, &mut sink, cancel)
}

/// Parse a single gzip-compressed (`.osc.gz`) or plain (`.osc`) file into a
/// new `CompactDiffOverlay`.
pub fn parse_osc_file(path: &Path) -> ParseResult<CompactDiffOverlay> {
    let mut overlay = CompactDiffOverlay::new();
    parse_osc_file_into(path, &mut overlay)?;
    Ok(overlay)
}

// ---------------------------------------------------------------------------
// Load all diffs from a directory
// ---------------------------------------------------------------------------

/// Parse the numeric sequence number from an OSC filename.
///
/// E.g. "4705.osc.gz" and "4705.osc" both produce sequence 4705.
fn parse_sequence_number(filename: &str) -> Option<u64> {
    let stem = filename.strip_suffix(".gz").unwrap_or(filename);
    let num_str = stem.strip_suffix(".osc")?;
    num_str.parse::<u64>().ok() // ok-deliberate: not a sequence-named diff
}

/// Load numeric-sequence `.osc` and `.osc.gz` diffs from a directory, sorted
/// by sequence number, and parse them into a single `CompactDiffOverlay`.
/// Subdirectories are skipped; only files directly inside `diffs_dir` are
/// considered.
/// Later diffs win for conflicts because `parse_osc_file_into` overwrites
/// existing entries.
///
/// This remains supported library API for callers that want to load a whole
/// directory of diffs; the crate has no in-repository production caller.
pub fn load_all_diffs(
    diffs_dir: &Path,
    diagnostics: &crate::host::Diagnostics,
) -> ParseResult<CompactDiffOverlay> {
    let mut entries: Vec<(u64, std::path::PathBuf)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for entry in std::fs::read_dir(diffs_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let filename = match path.file_name().and_then(|f| f.to_str()) {
            Some(f) => f.to_string(),
            None => continue,
        };
        match parse_sequence_number(&filename) {
            Some(seq) => entries.push((seq, path)),
            None => skipped.push(filename),
        }
    }

    // An empty directory is a legitimate no-op. A directory with files but
    // no numeric-sequence `.osc` or `.osc.gz` names is an operator error;
    // otherwise it would load zero diffs and the caller could report success
    // without applying any changes.
    if entries.is_empty() && !skipped.is_empty() {
        let mut shown: Vec<&str> = skipped.iter().map(String::as_str).take(5).collect();
        if skipped.len() > shown.len() {
            shown.push("...");
        }
        return Err(crate::Error::invalid_argument(format!(
            "no OSC diffs matched in {}: {} file(s) are present but none is named \
             `<sequence>.osc` or `<sequence>.osc.gz`, which are the only names this loader recognises (saw: {}). \
             Loading them as zero diffs would apply no changes and report success.",
            diffs_dir.display(),
            skipped.len(),
            shown.join(", ")
        )));
    }

    entries.sort_by_key(|(seq, _)| *seq);

    let mut overlay = CompactDiffOverlay::new();
    let total = entries.len();

    for (i, (seq, path)) in entries.iter().enumerate() {
        crate::host::diag!(
            diagnostics,
            Progress,
            "osc.load.parsing",
            "[{}/{}] Parsing diff {} (sequence {seq})...",
            i + 1,
            total,
            path.display()
        );
        parse_osc_file_into(path, &mut overlay)?;
    }

    crate::host::diag!(
        diagnostics,
        Progress,
        "osc.load.loaded",
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
        META_LEN, arena_append_node, arena_append_relation, arena_append_way, member_type_to_byte,
    };
    use super::*;
    use crate::read::elements::MemberType;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::fs::File;
    use std::io::Write;

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
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path().to_path_buf();

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
            .ok_or_else(|| crate::Error::internal("node 100 should exist in overlay"))?;
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

        Ok(())
    }

    /// Element metadata attributes survive the parse into the overlay and
    /// come back through the ref accessors; elements without any metadata
    /// attribute report `None`.
    #[test]
    fn test_parse_osc_element_metadata() -> ParseResult<()> {
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path().to_path_buf();

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

        let node = overlay
            .get_node(100)
            .ok_or_else(|| crate::Error::internal("node 100 missing"))?;
        let meta = node
            .metadata()
            .ok_or_else(|| crate::Error::internal("node 100 metadata missing"))?;
        assert_eq!(meta.version, 7);
        assert_eq!(meta.timestamp, 86_400);
        assert_eq!(meta.changeset, 123_456);
        assert_eq!(meta.uid, 42);
        assert_eq!(meta.user, "mapper one");
        assert!(meta.visible);

        // No metadata attributes at all -> None.
        let way = overlay
            .get_way(200)
            .ok_or_else(|| crate::Error::internal("way 200 missing"))?;
        assert!(way.metadata().is_none());
        // Metadata does not disturb the variable-length sections.
        assert_eq!(way.refs().collect::<Vec<_>>(), vec![100]);

        // Partial metadata: version + timestamp only; user absent -> "".
        let rel = overlay
            .get_relation(300)
            .ok_or_else(|| crate::Error::internal("relation 300 missing"))?;
        let rmeta = rel
            .metadata()
            .ok_or_else(|| crate::Error::internal("relation 300 metadata missing"))?;
        assert_eq!(rmeta.version, 2);
        assert_eq!(
            rmeta.timestamp,
            crate::commands::parse_rfc3339_utc("2026-02-20T21:39:49Z")
                .map_err(crate::Error::invalid_data_from)?
        );
        assert_eq!(rmeta.changeset, 0);
        assert_eq!(rmeta.user, "");
        let members: Vec<_> = rel.members().collect();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].2, "outer");

        Ok(())
    }

    #[test]
    fn test_merge_later_wins() -> ParseResult<()> {
        // Parse create first, then modify into the same overlay.
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path().to_path_buf();

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
            .ok_or_else(|| crate::Error::internal("node 100 should exist after merge"))?;
        // lat=3.0 -> decimicro = 30000000
        assert_eq!(node.decimicro_lat(), 30_000_000);
        assert_eq!(node.decimicro_lon(), 40_000_000);

        Ok(())
    }

    #[test]
    fn test_merge_delete_removes_create() -> ParseResult<()> {
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path().to_path_buf();

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

        Ok(())
    }

    #[test]
    fn test_merge_create_removes_delete() -> ParseResult<()> {
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path().to_path_buf();

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

        Ok(())
    }

    #[test]
    fn test_numeric_sort() -> ParseResult<()> {
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path().to_path_buf();

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

        let overlay = load_all_diffs(&dir, &crate::host::Diagnostics::discard())?;

        // Node 1 should have been created by 999, then modified by 10000
        let node1 = overlay
            .get_node(1)
            .ok_or_else(|| crate::Error::internal("node 1 should exist after loading diffs"))?;
        assert_eq!(node1.decimicro_lat(), 100_000_000);
        assert_eq!(node1.decimicro_lon(), 100_000_000);

        // Node 2 from 4705 should exist
        assert!(overlay.has_node(2));

        Ok(())
    }

    /// Build a one-node OSC in `section` with the given attribute string and
    /// try to parse it. Returns the parse result so callers can assert on
    /// either the value or the error text.
    fn parse_single_node(section: &str, attrs: &str) -> ParseResult<CompactDiffOverlay> {
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path();
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <osmChange version=\"0.6\">\n  <{section}>\n    <node {attrs}/>\n  \
             </{section}>\n</osmChange>"
        );
        write_osc_gz(dir, "coords.osc.gz", &xml);
        parse_osc_file(&dir.join("coords.osc.gz"))
    }

    /// A create/modify `<node>` without usable coordinates must be a hard
    /// parse error. It used to be accepted and stored at (0, 0) - which is a
    /// real coordinate in the Gulf of Guinea, so the corrupted node was
    /// indistinguishable from a genuine one, and under `--locations-on-ways`
    /// the (0, 0) propagated into every way referencing it.
    #[test]
    fn test_node_without_usable_coords_is_a_parse_error() {
        // (attribute string, the substring the error must name)
        let cases = [
            (r#"id="1" lon="12.0" version="1""#, "lat"),
            (r#"id="1" lat="55.0" version="1""#, "lon"),
            (r#"id="1" lat="" lon="12.0" version="1""#, "lat"),
            (r#"id="1" lat="abc" lon="12.0" version="1""#, "lat"),
            // `as i32` used to saturate this to i32::MAX.
            (r#"id="1" lat="1e30" lon="12.0" version="1""#, "lat"),
            // `as i32` used to map NaN to 0 - Null Island again.
            (r#"id="1" lat="NaN" lon="12.0" version="1""#, "lat"),
        ];
        for (attrs, expect_attr) in cases {
            let err = parse_single_node("create", attrs)
                .err()
                .unwrap_or_else(|| panic!("must not accept `{attrs}`"));
            let text = err.to_string();
            assert!(
                text.contains("OSC node 1") && text.contains(expect_attr),
                "error should name the node and the attribute; got: {text}"
            );
        }
    }

    /// Coordinates outside the valid geographic range must be a hard parse
    /// error, not merely coordinates that overflow `i32`.
    ///
    /// `lat="100"` scales to 1_000_000_000 decimicrodegrees, which fits an
    /// `i32` comfortably, so the representation check alone waves it through
    /// and the node enters the overlay looking like any other. Nothing
    /// downstream re-validates it: under `--locations-on-ways` it propagates
    /// into every way that references it, and the resulting PBF carries a
    /// latitude no projection can place. This is the same finding as the
    /// test above, one layer up - the earlier fix closed the fits-in-i32
    /// case and stopped there.
    #[test]
    fn test_out_of_range_coords_are_a_parse_error() {
        let cases = [
            (r#"id="1" lat="100.0" lon="12.0" version="1""#, "lat"),
            (r#"id="1" lat="-90.0000001" lon="12.0" version="1""#, "lat"),
            (r#"id="1" lat="55.0" lon="200.0" version="1""#, "lon"),
            (r#"id="1" lat="55.0" lon="-180.0000001" version="1""#, "lon"),
            // Well inside i32 after scaling, so only a geographic check
            // catches it. 91 degrees is 910_000_000, an ordinary i32.
            (r#"id="1" lat="91.0" lon="12.0" version="1""#, "lat"),
        ];
        for (attrs, expect_attr) in cases {
            let err = parse_single_node("create", attrs)
                .err()
                .unwrap_or_else(|| panic!("must not accept `{attrs}`"));
            let text = err.to_string();
            assert!(
                text.contains("OSC node 1") && text.contains(expect_attr),
                "error should name the node and the attribute; got: {text}"
            );
        }
    }

    /// The counterpart the above test needs in order to mean anything: a node
    /// genuinely AT 0/0 still parses, and so does one at the representable
    /// extremes. Rejecting everything would satisfy the test above.
    #[test]
    fn test_genuine_null_island_node_still_parses() -> ParseResult<()> {
        let overlay = parse_single_node("create", r#"id="1" lat="0.0" lon="0.0" version="1""#)?;
        let node = overlay
            .get_node(1)
            .ok_or_else(|| crate::Error::internal("node 1 missing"))?;
        assert_eq!((node.decimicro_lat(), node.decimicro_lon()), (0, 0));

        let overlay = parse_single_node("create", r#"id="1" lat="-90.0" lon="180.0" version="1""#)?;
        let node = overlay
            .get_node(1)
            .ok_or_else(|| crate::Error::internal("node 1 missing"))?;
        assert_eq!(
            (node.decimicro_lat(), node.decimicro_lon()),
            (-900_000_000, 1_800_000_000)
        );
        Ok(())
    }

    /// A `<delete>` node carries no coordinates by design and must keep
    /// parsing - the coordinate requirement applies to create/modify only.
    #[test]
    fn test_delete_node_needs_no_coords() -> ParseResult<()> {
        let overlay = parse_single_node("delete", r#"id="7" version="3""#)?;
        assert!(overlay.deleted_nodes.contains(&7));
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Input framing: truncation and compression
    // -----------------------------------------------------------------------

    /// A `.osc.gz` whose gzip member is complete but whose XML stops
    /// short of `</osmChange>` must be a hard error.
    ///
    /// This is the shape that made it dangerous: the gzip layer is
    /// intact, so `MultiGzDecoder` returns a clean EOF, quick-xml
    /// reports `Event::Eof`, and every caller treated the partial
    /// document as the whole change stream. `apply-changes` then wrote a
    /// partially-applied PBF and reported success. Only the closing tag
    /// proves the document is whole.
    #[test]
    fn test_truncated_osc_is_a_parse_error() {
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path().to_path_buf();
        // Deliberately no `</osmChange>` - and note the gzip member IS
        // properly finished, so nothing below the XML layer can notice.
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                   <osmChange version=\"0.6\">\n  <create>\n    \
                   <node id=\"1\" lat=\"1.0\" lon=\"2.0\" version=\"1\"/>\n  </create>\n";
        write_osc_gz(&dir, "cut.osc.gz", xml);
        let err = parse_osc_file(&dir.join("cut.osc.gz"))
            .err()
            .expect("a truncated OSC must not parse as a complete one");
        let text = err.to_string();
        assert!(
            text.contains("closing </osmChange>") && text.contains("truncated"),
            "error must name the missing closing tag; got: {text}"
        );
    }

    /// The counterpart that keeps the test above from being satisfied by
    /// rejecting everything: a whole document still parses, including
    /// the self-closing empty form `<osmChange version="0.6"/>`, which
    /// never produces a closing tag at all.
    #[test]
    fn test_complete_osc_forms_still_parse() -> ParseResult<()> {
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path().to_path_buf();
        write_osc_gz(
            &dir,
            "whole.osc.gz",
            "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <create>\n    \
             <node id=\"1\" lat=\"1.0\" lon=\"2.0\" version=\"1\"/>\n  \
             </create>\n</osmChange>",
        );
        assert!(parse_osc_file(&dir.join("whole.osc.gz"))?.has_node(1));

        write_osc_gz(
            &dir,
            "empty.osc.gz",
            "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\"/>",
        );
        assert!(parse_osc_file(&dir.join("empty.osc.gz"))?.is_empty());
        Ok(())
    }

    /// An uncompressed `.osc` parses. `parse_osc_file_into` used to wrap
    /// every input in `MultiGzDecoder` unconditionally and fail with a
    /// raw gzip error, while `merge-changes` sniffed the magic and
    /// handled both - the same file, two answers.
    #[test]
    fn test_uncompressed_osc_is_accepted() -> ParseResult<()> {
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path().to_path_buf();
        let path = dir.join("plain.osc");
        std::fs::write(
            &path,
            "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <create>\n    \
             <node id=\"7\" lat=\"1.0\" lon=\"2.0\" version=\"1\"/>\n  \
             </create>\n</osmChange>",
        )?;
        let overlay = parse_osc_file(&path)?;
        assert!(overlay.has_node(7));
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Framing: elements outside a change section
    // -----------------------------------------------------------------------

    /// An element outside `<create>` / `<modify>` / `<delete>` is
    /// malformed and refused. The compact parser used to treat it as a
    /// create and put it in the overlay; `merge-changes` errored on the
    /// self-closing form and silently dropped the other. Three code
    /// paths, three answers.
    #[test]
    fn test_element_outside_section_is_refused() {
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path().to_path_buf();
        for xml in [
            "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  \
             <node id=\"5\" lat=\"1.0\" lon=\"2.0\" version=\"1\"/>\n</osmChange>",
            "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  \
             <node id=\"5\" lat=\"1.0\" lon=\"2.0\" version=\"1\">\n    \
             <tag k=\"a\" v=\"b\"/>\n  </node>\n</osmChange>",
        ] {
            write_osc_gz(&dir, "loose.osc.gz", xml);
            let err = parse_osc_file(&dir.join("loose.osc.gz"))
                .err()
                .expect("element outside a section must not parse");
            let text = err.to_string();
            assert!(
                text.contains("outside any <create>") && text.contains("node 5"),
                "error must name the element and the missing section; got: {text}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Metadata leniency: presence, not version-gating
    // -----------------------------------------------------------------------

    /// An element carrying `timestamp` but no `version` has metadata,
    /// and every attribute it did carry survives. `version` itself
    /// stays absent rather than becoming 0.
    #[test]
    fn test_metadata_without_version_is_kept() -> ParseResult<()> {
        let overlay = parse_single_node(
            "create",
            r#"id="1" lat="1.0" lon="2.0" timestamp="1970-01-02T00:00:00Z" user="m""#,
        )?;
        let node = overlay
            .get_node(1)
            .ok_or_else(|| crate::Error::internal("node 1 missing"))?;
        let meta = node
            .metadata()
            .ok_or_else(|| crate::Error::internal("metadata must survive an absent version"))?;
        assert_eq!(meta.timestamp, 86_400);
        assert_eq!(meta.user, "m");
        Ok(())
    }

    /// A metadata attribute that is present but unparsable is an error,
    /// not a silent zero. The old parser folded `version="abc"` into
    /// `version = 0` - the same absent-versus-zero collapse the
    /// coordinate and `push_info_attrs` fixes had to undo, and now
    /// load-bearing because the version-regression gate compares these
    /// numbers.
    #[test]
    fn test_unparsable_metadata_is_refused() {
        for attrs in [
            r#"id="1" lat="1.0" lon="2.0" version="abc""#,
            r#"id="1" lat="1.0" lon="2.0" version="""#,
            r#"id="1" lat="1.0" lon="2.0" version="1" changeset="not-a-number""#,
            r#"id="1" lat="1.0" lon="2.0" version="1" uid="x""#,
            r#"id="1" lat="1.0" lon="2.0" version="1" timestamp="yesterday""#,
        ] {
            let err = parse_single_node("create", attrs)
                .err()
                .unwrap_or_else(|| panic!("must not accept `{attrs}`"));
            let text = err.to_string();
            assert!(
                text.contains("unparsable value"),
                "error must name the unparsable metadata value; got: {text}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Version precedence
    // -----------------------------------------------------------------------

    /// Build a two-file overlay from `(section, attrs)` pairs.
    fn accumulate(files: &[(&str, &str)]) -> ParseResult<CompactDiffOverlay> {
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path();
        let mut overlay = CompactDiffOverlay::new();
        for (i, (section, attrs)) in files.iter().enumerate() {
            let name = format!("{i:03}.osc.gz");
            write_osc_gz(
                dir,
                &name,
                &format!(
                    "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <{section}>\n    \
                     <node {attrs}/>\n  </{section}>\n</osmChange>"
                ),
            );
            parse_osc_file_into(&dir.join(&name), &mut overlay)?;
        }
        Ok(overlay)
    }

    /// An older diff after a newer one used to silently downgrade the
    /// object and report success. The overlay already stored the
    /// version; now it compares it.
    #[test]
    fn test_version_regression_is_refused() {
        for (suffix, files) in [
            // Out-of-sequence: an older diff after a newer one.
            (
                "backwards",
                [
                    ("modify", r#"id="1" lat="1.0" lon="2.0" version="9""#),
                    ("modify", r#"id="1" lat="3.0" lon="4.0" version="8""#),
                ],
            ),
            // Within a single accumulation, create-then-lower-modify.
            (
                "create_then_older",
                [
                    ("create", r#"id="1" lat="1.0" lon="2.0" version="4""#),
                    ("modify", r#"id="1" lat="3.0" lon="4.0" version="2""#),
                ],
            ),
        ] {
            let err = accumulate(&files)
                .err()
                .unwrap_or_else(|| panic!("{suffix}: version regression must be refused"));
            let text = err.to_string();
            assert!(
                text.contains("is lower than version"),
                "{suffix}: error must name the regression; got: {text}"
            );
        }
    }

    /// Equal versions are admitted and the later change wins by position.
    /// Overlapping replication diffs repeat boundary changes, and regional
    /// extract diffs delete an object leaving the region at the version
    /// it already had (germany seq 4705 modifies node 135379864 at
    /// version 12, seq 4706 deletes it at version 12). Both used to be
    /// refused, failing every range that spanned them.
    #[test]
    fn test_equal_version_is_admitted_and_later_wins() -> ParseResult<()> {
        // An identical repeat.
        let overlay = accumulate(&[
            ("modify", r#"id="1" lat="1.0" lon="2.0" version="7""#),
            ("modify", r#"id="1" lat="1.0" lon="2.0" version="7""#),
        ])?;
        assert_eq!(
            overlay
                .get_node(1)
                .ok_or_else(|| crate::Error::internal("node 1 missing"))?
                .decimicro_lat(),
            10_000_000
        );

        // An equal version with different content: position decides.
        let overlay = accumulate(&[
            ("modify", r#"id="1" lat="1.0" lon="2.0" version="7""#),
            ("modify", r#"id="1" lat="3.0" lon="4.0" version="7""#),
        ])?;
        assert_eq!(
            overlay
                .get_node(1)
                .ok_or_else(|| crate::Error::internal("node 1 missing"))?
                .decimicro_lat(),
            30_000_000
        );

        // The regional-extract shape: a delete at the version just modified.
        let overlay = accumulate(&[
            ("modify", r#"id="1" lat="1.0" lon="2.0" version="12""#),
            ("delete", r#"id="1" version="12""#),
        ])?;
        assert!(!overlay.has_node(1));
        assert!(overlay.is_deleted(ElementKind::Node, 1));

        // A repeated delete, and re-entry at the deleted version.
        let overlay = accumulate(&[
            ("delete", r#"id="1" version="12""#),
            ("delete", r#"id="1" version="12""#),
            ("create", r#"id="1" lat="1.0" lon="2.0" version="12""#),
        ])?;
        assert!(overlay.has_node(1));
        assert!(!overlay.is_deleted(ElementKind::Node, 1));

        // Admitting an equal claim never lowers the floor: a lower claim
        // after it is still refused against the same version.
        let err = accumulate(&[
            ("modify", r#"id="1" lat="1.0" lon="2.0" version="12""#),
            ("delete", r#"id="1" version="12""#),
            ("create", r#"id="1" lat="1.0" lon="2.0" version="12""#),
            ("modify", r#"id="1" lat="1.0" lon="2.0" version="11""#),
        ])
        .err()
        .ok_or_else(|| crate::Error::internal("a lower claim after equal ones must be refused"))?;
        assert!(
            err.to_string().contains("is lower than version 12"),
            "got: {err}"
        );
        Ok(())
    }

    /// The counterparts that stop the gate from being satisfied by
    /// refusing every repeat: a genuine forward step is accepted, and so
    /// is a repeat where neither side claims a version (no claim, no
    /// comparison). The opt-out restores the old positional behaviour.
    #[test]
    fn test_version_advance_and_unversioned_still_accepted() -> ParseResult<()> {
        let overlay = accumulate(&[
            ("create", r#"id="1" lat="1.0" lon="2.0" version="1""#),
            ("modify", r#"id="1" lat="3.0" lon="4.0" version="2""#),
        ])?;
        let node = overlay
            .get_node(1)
            .ok_or_else(|| crate::Error::internal("node 1 missing"))?;
        assert_eq!(node.decimicro_lat(), 30_000_000);

        // Neither element claims a version, so there is nothing to
        // compare and the positional rule stands.
        let overlay = accumulate(&[
            ("create", r#"id="1" lat="1.0" lon="2.0""#),
            ("modify", r#"id="1" lat="3.0" lon="4.0""#),
        ])?;
        let node = overlay
            .get_node(1)
            .ok_or_else(|| crate::Error::internal("node 1 missing"))?;
        assert_eq!(node.decimicro_lat(), 30_000_000);

        // And the escape hatch really is an escape hatch.
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path().to_path_buf();
        let mut overlay = CompactDiffOverlay::new();
        overlay.allow_version_regression(true);
        for (i, v) in [(0, "9"), (1, "8")] {
            let name = format!("{i:03}.osc.gz");
            write_osc_gz(
                &dir,
                &name,
                &format!(
                    "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <modify>\n    \
                     <node id=\"1\" lat=\"5.0\" lon=\"6.0\" version=\"{v}\"/>\n  \
                     </modify>\n</osmChange>"
                ),
            );
            parse_osc_file_into(&dir.join(&name), &mut overlay)?;
        }
        assert!(overlay.has_node(1));
        Ok(())
    }

    /// Parse one XML document into a fresh overlay. Unlike `accumulate`,
    /// the caller supplies the whole document, so several actions on the
    /// same object inside a single OSC are expressible.
    fn parse_one_xml(body: &str) -> ParseResult<CompactDiffOverlay> {
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path();
        let xml =
            format!("<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n{body}\n</osmChange>");
        write_osc_gz(dir, "000.osc.gz", &xml);
        parse_osc_file(&dir.join("000.osc.gz"))
    }

    /// A delete used to erase the object's version history: it dropped
    /// the live index entry, which is the only place the gate read a
    /// stored version from, so the very next stale change found nothing
    /// to be compared against and resurrected an older version. Delete
    /// node 1 version 9 then modify node 1 version 8, and version 8
    /// won - past a gate whose whole purpose is to stop exactly that.
    ///
    /// Both shapes the hole reached: several actions inside one OSC,
    /// and one action per file across an accumulated overlay.
    #[test]
    fn test_delete_leaves_a_versioned_tombstone() {
        // Within one OSC: delete v9, then a stale modify v8.
        let err = parse_one_xml(
            "  <delete>\n    <node id=\"1\" version=\"9\"/>\n  </delete>\n  \
             <modify>\n    <node id=\"1\" lat=\"1.0\" lon=\"2.0\" version=\"8\"/>\n  </modify>",
        )
        .err()
        .expect("a stale modify after a delete must be refused");
        assert!(
            err.to_string().contains("is lower than version 9"),
            "error must name the deleted version as the floor; got: {err}"
        );

        // A stale create is the same hole wearing a different action.
        let err = parse_one_xml(
            "  <delete>\n    <node id=\"1\" version=\"9\"/>\n  </delete>\n  \
             <create>\n    <node id=\"1\" lat=\"1.0\" lon=\"2.0\" version=\"8\"/>\n  </create>",
        )
        .err()
        .expect("a stale create after a delete must be refused");
        assert!(err.to_string().contains("is lower than version 9"));

        // And a second delete may not walk the version backwards either.
        let err = parse_one_xml(
            "  <delete>\n    <node id=\"1\" version=\"9\"/>\n    \
             <node id=\"1\" version=\"8\"/>\n  </delete>",
        )
        .err()
        .expect("a stale delete after a delete must be refused");
        assert!(err.to_string().contains("is lower than version 9"));

        // Across files: the multi-file overlay accumulation.
        let err = accumulate(&[
            ("delete", r#"id="1" version="9""#),
            ("modify", r#"id="1" lat="1.0" lon="2.0" version="8""#),
        ])
        .err()
        .expect("a stale modify in a later file must be refused");
        assert!(err.to_string().contains("is lower than version 9"));

        // A delete carrying no version of its own still cannot erase the
        // version the live record held: the tombstone carries it forward.
        let err = parse_one_xml(
            "  <create>\n    <node id=\"1\" lat=\"1.0\" lon=\"2.0\" version=\"9\"/>\n  \
             </create>\n  <delete>\n    <node id=\"1\"/>\n  </delete>\n  \
             <modify>\n    <node id=\"1\" lat=\"3.0\" lon=\"4.0\" version=\"8\"/>\n  </modify>",
        )
        .err()
        .expect("an unversioned delete must not discard the live version");
        assert!(err.to_string().contains("is lower than version 9"));
    }

    /// The counterparts, so the tombstone is not satisfied by refusing
    /// every post-delete change: a genuine undelete that advances the
    /// version is accepted, an unversioned pair is still positional, and
    /// the escape hatch still escapes.
    #[test]
    fn test_delete_then_forward_change_is_accepted() -> ParseResult<()> {
        let overlay = parse_one_xml(
            "  <delete>\n    <node id=\"1\" version=\"9\"/>\n  </delete>\n  \
             <create>\n    <node id=\"1\" lat=\"1.0\" lon=\"2.0\" version=\"10\"/>\n  </create>",
        )?;
        assert!(overlay.has_node(1), "the undelete must be live again");
        assert!(!overlay.deleted_nodes.contains(&1));

        let overlay = parse_one_xml(
            "  <delete>\n    <node id=\"1\"/>\n  </delete>\n  \
             <modify>\n    <node id=\"1\" lat=\"3.0\" lon=\"4.0\"/>\n  </modify>",
        )?;
        assert!(overlay.has_node(1), "no claim on either side, no gate");

        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path();
        let mut overlay = CompactDiffOverlay::new();
        overlay.allow_version_regression(true);
        write_osc_gz(
            dir,
            "000.osc.gz",
            "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <delete>\n    \
             <node id=\"1\" version=\"9\"/>\n  </delete>\n  <modify>\n    \
             <node id=\"1\" lat=\"3.0\" lon=\"4.0\" version=\"8\"/>\n  \
             </modify>\n</osmChange>",
        );
        parse_osc_file_into(&dir.join("000.osc.gz"), &mut overlay)?;
        assert!(overlay.has_node(1));
        Ok(())
    }

    /// Ways and relations get the same tombstone, not just nodes: the
    /// hole was one line per kind, and a node-only test would have let
    /// two thirds of it survive.
    #[test]
    fn test_delete_tombstone_covers_ways_and_relations() {
        for (suffix, body) in [
            (
                "tombstone_way",
                "  <delete>\n    <way id=\"5\" version=\"4\"/>\n  </delete>\n  \
                 <modify>\n    <way id=\"5\" version=\"3\"><nd ref=\"1\"/></way>\n  </modify>",
            ),
            (
                "tombstone_relation",
                "  <delete>\n    <relation id=\"6\" version=\"4\"/>\n  </delete>\n  \
                 <modify>\n    <relation id=\"6\" version=\"3\">\
                 <member type=\"node\" ref=\"1\" role=\"\"/></relation>\n  </modify>",
            ),
        ] {
            let err = parse_one_xml(body)
                .err()
                .unwrap_or_else(|| panic!("{suffix}: stale change after delete must be refused"));
            assert!(
                err.to_string().contains("is lower than version 4"),
                "{suffix}: error must name the deleted version; got: {err}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // load_all_diffs
    // -----------------------------------------------------------------------

    /// A directory whose files are all named something this loader does
    /// not recognise is an operator error, not zero diffs. It used to
    /// return an empty overlay and print "Loaded 0 diffs", after which
    /// the caller applied nothing and reported success.
    #[test]
    fn test_load_all_diffs_errors_when_nothing_matched() {
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path().to_path_buf();
        let xml = "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\"/>";
        write_osc_gz(&dir, "changes-4705.osc.gz", xml);
        write_osc_gz(&dir, "planet-latest.osc.gz", xml);
        let err = load_all_diffs(&dir, &crate::host::Diagnostics::discard())
            .err()
            .expect("a directory of unmatched files must not load as zero diffs");
        let text = err.to_string();
        assert!(
            text.contains("none is named") && text.contains("changes-4705.osc.gz"),
            "error must name what was skipped; got: {text}"
        );
    }

    /// The counterparts: a genuinely empty directory is a legitimate
    /// no-op, and a plain `.osc` now counts as a match because the
    /// parser sniffs compression from the bytes rather than the name.
    #[test]
    fn test_load_all_diffs_empty_dir_and_plain_osc() -> ParseResult<()> {
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path().to_path_buf();
        assert!(load_all_diffs(&dir, &crate::host::Diagnostics::discard())?.is_empty());

        std::fs::write(
            dir.join("4705.osc"),
            "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <create>\n    \
             <node id=\"3\" lat=\"1.0\" lon=\"2.0\" version=\"1\"/>\n  \
             </create>\n</osmChange>",
        )?;
        assert!(load_all_diffs(&dir, &crate::host::Diagnostics::discard())?.has_node(3));
        Ok(())
    }

    #[test]
    fn test_empty_overlay() {
        let overlay = CompactDiffOverlay::new();
        assert!(overlay.is_empty());
    }

    #[test]
    fn test_self_closing_delete() -> ParseResult<()> {
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path().to_path_buf();

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

        Ok(())
    }

    // -----------------------------------------------------------------------
    // New tests: StringInterner
    // -----------------------------------------------------------------------

    #[test]
    fn test_interner_roundtrip() -> ParseResult<()> {
        let mut interner = StringInterner::new();
        let id_hello = interner.intern("hello")?;
        let id_world = interner.intern("world")?;
        let id_empty = interner.intern("")?;

        assert_eq!(interner.resolve(id_hello), "hello");
        assert_eq!(interner.resolve(id_world), "world");
        assert_eq!(interner.resolve(id_empty), "");
        assert_eq!(id_empty, 0); // empty string is always intern_id 0
        Ok(())
    }

    /// An id the interner never handed out is a broken arena invariant,
    /// not an empty string.
    #[test]
    #[should_panic(expected = "intern id was never handed out by this interner")]
    fn test_interner_resolve_unknown_id_panics() {
        let mut interner = StringInterner::new();
        let known = interner.intern("highway").expect("intern");
        assert_eq!(interner.resolve(known), "highway");
        let _ = interner.resolve(known + 1);
    }

    /// A tag value whose arena bytes do not decode is a broken arena
    /// invariant, not an empty value.
    #[test]
    #[should_panic(expected = "tag value bytes are not the UTF-8 append_tags wrote")]
    fn test_corrupt_tag_value_panics() {
        let mut interner = StringInterner::new();
        let key = interner.intern("name").expect("intern");
        let mut arena = Vec::new();
        let offset = arena_append_node(&mut arena, 1, 0, 0, &[(key, "x")], &ArenaMeta::default())
            .expect("append");
        // Header (49 bytes), then the tag's key id and value length.
        arena[offset + NODE_META_OFF + META_LEN + 8] = 0xFF;
        let node = CompactNodeRef {
            data: &arena[offset..],
            interner: &interner,
        };
        let _ = node.tags().count();
    }

    /// A member type byte other than the three the writer emits is a
    /// broken arena invariant, not `MemberType::Unknown`.
    #[test]
    #[should_panic(expected = "member type byte is not one member_type_to_byte writes")]
    fn test_corrupt_member_type_byte_panics() {
        let mut interner = StringInterner::new();
        let role = interner.intern("outer").expect("intern");
        let mut arena = Vec::new();
        let offset = arena_append_relation(
            &mut arena,
            1,
            &[(10, member_type_to_byte(MemberType::Way).expect("way"), role)],
            &[],
            &ArenaMeta::default(),
        )
        .expect("append");
        // Header (45 bytes), then the member's ref id.
        arena[offset + RELATION_META_OFF + META_LEN + 8] = 7;
        let rel = CompactRelationRef {
            data: &arena[offset..],
            interner: &interner,
        };
        let _ = rel.members().count();
    }

    #[test]
    fn test_interner_dedup() -> ParseResult<()> {
        let mut interner = StringInterner::new();
        let id1 = interner.intern("highway")?;
        let id2 = interner.intern("highway")?;
        let id3 = interner.intern("name")?;

        assert_eq!(id1, id2); // same string -> same id
        assert_ne!(id1, id3); // different string -> different id
        Ok(())
    }

    // -----------------------------------------------------------------------
    // New tests: arena roundtrips
    // -----------------------------------------------------------------------

    #[test]
    fn test_node_roundtrip() -> ParseResult<()> {
        let mut interner = StringInterner::new();
        let key_name = interner.intern("name")?;
        let key_place = interner.intern("place")?;

        let mut arena = Vec::new();
        let tags: Vec<(u32, &str)> = vec![(key_name, "Test City"), (key_place, "city")];
        let offset = arena_append_node(
            &mut arena,
            42,
            556_800_000,
            125_700_000,
            &tags,
            &ArenaMeta::default(),
        )?;

        let node = CompactNodeRef {
            data: &arena[offset..],
            interner: &interner,
        };

        assert_eq!(node.id(), 42);
        assert_eq!(node.decimicro_lat(), 556_800_000);
        assert_eq!(node.decimicro_lon(), 125_700_000);
        assert_eq!(node.tag_count(), 2);

        let tag_vec: Vec<(&str, &str)> = node.tags().collect();
        assert_eq!(tag_vec[0], ("name", "Test City"));
        assert_eq!(tag_vec[1], ("place", "city"));
        Ok(())
    }

    #[test]
    fn test_way_roundtrip() -> ParseResult<()> {
        let mut interner = StringInterner::new();
        let key_highway = interner.intern("highway")?;

        let mut arena = Vec::new();
        let refs = vec![1, 2, 3, 4, 5];
        let tags: Vec<(u32, &str)> = vec![(key_highway, "residential")];
        let offset = arena_append_way(&mut arena, 99, &refs, &tags, &ArenaMeta::default())?;

        let way = CompactWayRef {
            data: &arena[offset..],
            interner: &interner,
        };

        assert_eq!(way.id(), 99);
        assert_eq!(way.ref_count(), 5);
        assert_eq!(way.tag_count(), 1);

        let ref_vec: Vec<i64> = way.refs().collect();
        assert_eq!(ref_vec, vec![1, 2, 3, 4, 5]);

        let tag_vec: Vec<(&str, &str)> = way.tags().collect();
        assert_eq!(tag_vec[0], ("highway", "residential"));
        Ok(())
    }

    #[test]
    fn test_relation_roundtrip() -> ParseResult<()> {
        let mut interner = StringInterner::new();
        let key_type = interner.intern("type")?;
        let role_outer = interner.intern("outer")?;
        let role_inner = interner.intern("inner")?;
        let role_empty = interner.intern("")?;

        let mut arena = Vec::new();
        let members = vec![
            (10, member_type_to_byte(MemberType::Way)?, role_outer),
            (20, member_type_to_byte(MemberType::Way)?, role_inner),
            (30, member_type_to_byte(MemberType::Node)?, role_empty),
        ];
        let tags: Vec<(u32, &str)> = vec![(key_type, "multipolygon")];
        let offset =
            arena_append_relation(&mut arena, 500, &members, &tags, &ArenaMeta::default())?;

        let rel = CompactRelationRef {
            data: &arena[offset..],
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
        Ok(())
    }

    /// `MemberType::Unknown` has no faithful single-byte encoding in the
    /// arena - 0/1/2 are node/way/relation - so it is refused rather than
    /// stored as some other type.
    #[test]
    fn test_member_type_unknown_is_refused() {
        for value in [3, 1, 257, -1] {
            let err = member_type_to_byte(MemberType::Unknown(value))
                .expect_err("Unknown member type must not encode");
            let text = err.to_string();
            assert!(
                text.contains("unknown type value") && text.contains(&value.to_string()),
                "error should name the rejected type value: {text}"
            );
        }
        assert_eq!(member_type_to_byte(MemberType::Node).ok(), Some(0));
        assert_eq!(member_type_to_byte(MemberType::Way).ok(), Some(1));
        assert_eq!(member_type_to_byte(MemberType::Relation).ok(), Some(2));
    }

    /// F57: End-to-end test parsing an OSC file with way `<nd>` children
    /// and relation `<member>` children through `parse_osc_file`.
    #[test]
    fn test_parse_osc_way_and_relation_children() -> ParseResult<()> {
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path().to_path_buf();

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
        let way100 = overlay
            .get_way(100)
            .ok_or_else(|| crate::Error::internal("way 100 should exist"))?;
        let refs: Vec<i64> = way100.refs().collect();
        assert_eq!(refs, vec![1, 2, 3]);
        let tags: Vec<(&str, &str)> = way100.tags().collect();
        assert_eq!(
            tags,
            vec![("highway", "residential"), ("name", "Main Street")]
        );

        // Way 300: 2 refs, 1 tag (modify)
        let way300 = overlay
            .get_way(300)
            .ok_or_else(|| crate::Error::internal("way 300 should exist"))?;
        let refs: Vec<i64> = way300.refs().collect();
        assert_eq!(refs, vec![10, 11]);
        let tags: Vec<(&str, &str)> = way300.tags().collect();
        assert_eq!(tags, vec![("highway", "primary")]);

        // Relation 200: 3 members, 1 tag
        let rel = overlay
            .get_relation(200)
            .ok_or_else(|| crate::Error::internal("relation 200 should exist"))?;
        let members: Vec<(MemberType, i64, &str)> = rel.members().collect();
        assert_eq!(members.len(), 3);
        assert_eq!(members[0], (MemberType::Way, 100, "outer"));
        assert_eq!(members[1], (MemberType::Way, 101, "inner"));
        assert_eq!(members[2], (MemberType::Node, 1, "label"));
        let tags: Vec<(&str, &str)> = rel.tags().collect();
        assert_eq!(tags, vec![("type", "multipolygon")]);
        Ok(())
    }
}
