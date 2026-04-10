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
    OwnedMetadata, OwnedNode, OwnedRelation, OwnedWay,
    write_node_xml, write_way_xml, write_relation_xml,
};
use super::stream_merge::{
    block_pair_merge_phase, merge_join_phase, BlockMergeAction, BlockPairMergeState,
    MergeJoinAction, StreamingBlocks,
};
use super::Result;
use crate::blob_index::ElemKind;

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
    // Single-pass: check sorted headers + indexdata from one file open each.
    let (old_sorted, old_indexed) = super::diff::check_sorted_and_indexed(old_path, direct_io)?;
    let (new_sorted, new_indexed) = super::diff::check_sorted_and_indexed(new_path, direct_io)?;
    if !old_sorted { super::require_sorted_err(old_path, "Old PBF")?; }
    if !new_sorted { super::require_sorted_err(new_path, "New PBF")?; }
    let both_indexed = old_indexed && new_indexed;

    crate::debug::emit_marker("DERIVECHANGES_SCAN_START");

    let (creates, modifies, deletes) = if both_indexed {
        derive_changes_block_pair(old_path, new_path, direct_io)?
    } else {
        derive_changes_element_stream(old_path, new_path, direct_io)?
    };

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
// Optimized block-pair path (borrowed elements, zero-alloc for Equal)
// ---------------------------------------------------------------------------

/// Collect changes using block-pair merge with borrowed elements.
/// Only changed elements (~1.2% of typical daily diff) are materialized as owned.
fn derive_changes_block_pair(
    old_path: &Path,
    new_path: &Path,
    direct_io: bool,
) -> Result<(Changes, Changes, Changes)> {
    let mut old_reader = crate::blob::BlobReader::open(old_path, direct_io)?;
    old_reader.set_parse_indexdata(true);
    let mut new_reader = crate::blob::BlobReader::open(new_path, direct_io)?;
    new_reader.set_parse_indexdata(true);

    let mut merge = BlockPairMergeState::new(old_reader, new_reader);

    let mut creates = Changes::new();
    let mut modifies = Changes::new();
    let mut deletes = Changes::new();

    collect_phase_block_pair(&mut merge, ElemKind::Node, &mut creates, &mut modifies, &mut deletes)?;
    collect_phase_block_pair(&mut merge, ElemKind::Way, &mut creates, &mut modifies, &mut deletes)?;
    collect_phase_block_pair(&mut merge, ElemKind::Relation, &mut creates, &mut modifies, &mut deletes)?;

    Ok((creates, modifies, deletes))
}

/// Run one type phase of block-pair merge, collecting changed elements as owned.
fn collect_phase_block_pair(
    merge: &mut BlockPairMergeState,
    kind: ElemKind,
    creates: &mut Changes,
    modifies: &mut Changes,
    deletes: &mut Changes,
) -> Result<()> {
    block_pair_merge_phase(merge, kind, true, &mut |action| {
        match action {
            BlockMergeAction::BlobEqual(_) | BlockMergeAction::ElementEqual { .. } => {}
            BlockMergeAction::BlobOldOnly { block, skip, .. } => {
                for elem in block.elements().skip(skip) {
                    push_converted(&elem, kind, deletes);
                }
            }
            BlockMergeAction::BlobNewOnly { block, skip, .. } => {
                for elem in block.elements().skip(skip) {
                    push_converted(&elem, kind, creates);
                }
            }
            BlockMergeAction::ElementModified { new, .. } => {
                push_converted(new, kind, modifies);
            }
            BlockMergeAction::ElementOldOnly(o) => {
                push_converted(o, kind, deletes);
            }
            BlockMergeAction::ElementNewOnly(n) => {
                push_converted(n, kind, creates);
            }
        }
        Ok(())
    })
}

/// Convert a borrowed Element to the appropriate owned type and push to Changes.
fn push_converted(elem: &crate::Element<'_>, kind: ElemKind, target: &mut Changes) {
    use super::stream_merge::{convert_node, convert_relation, convert_way};

    match kind {
        ElemKind::Node => {
            if let Some(owned) = convert_node(elem) {
                target.nodes.push(owned);
            }
        }
        ElemKind::Way => {
            if let Some(owned) = convert_way(elem) {
                target.ways.push(owned);
            }
        }
        ElemKind::Relation => {
            if let Some(owned) = convert_relation(elem) {
                target.relations.push(owned);
            }
        }
    }
}

/// Fallback path using element-level merge-join with owned elements.
fn derive_changes_element_stream(
    old_path: &Path,
    new_path: &Path,
    direct_io: bool,
) -> Result<(Changes, Changes, Changes)> {
    let mut old_src = StreamingBlocks::new_sequential(old_path, direct_io)?;
    let mut new_src = StreamingBlocks::new_sequential(new_path, direct_io)?;

    let mut creates = Changes::new();
    let mut modifies = Changes::new();
    let mut deletes = Changes::new();

    // Phase 1: Nodes
    {
        let (mut ob, mut nb) = (Vec::new(), Vec::new());
        collect_changes_phase(
            &mut old_src,
            &mut ob,
            &mut new_src,
            &mut nb,
            &mut creates.nodes,
            &mut modifies.nodes,
            &mut deletes.nodes,
        )?;
    }
    // Phase 2: Ways
    {
        let (mut ob, mut nb) = (Vec::new(), Vec::new());
        collect_changes_phase(
            &mut old_src,
            &mut ob,
            &mut new_src,
            &mut nb,
            &mut creates.ways,
            &mut modifies.ways,
            &mut deletes.ways,
        )?;
    }
    // Phase 3: Relations
    {
        let (mut ob, mut nb) = (Vec::new(), Vec::new());
        collect_changes_phase(
            &mut old_src,
            &mut ob,
            &mut new_src,
            &mut nb,
            &mut creates.relations,
            &mut modifies.relations,
            &mut deletes.relations,
        )?;
    }

    Ok((creates, modifies, deletes))
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
            write_node_xml(&mut writer, node)?;
        }
        for way in &creates.ways {
            write_way_xml(&mut writer, way)?;
        }
        for rel in &creates.relations {
            write_relation_xml(&mut writer, rel)?;
        }
        writer.write_event(Event::End(BytesEnd::new("create")))?;
    }

    // <modify>
    if !modifies.is_empty() {
        writer.write_event(Event::Start(BytesStart::new("modify")))?;
        for node in &modifies.nodes {
            write_node_xml(&mut writer, node)?;
        }
        for way in &modifies.ways {
            write_way_xml(&mut writer, way)?;
        }
        for rel in &modifies.relations {
            write_relation_xml(&mut writer, rel)?;
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

