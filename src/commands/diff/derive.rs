//! Generate an OSC diff from two PBF snapshots. Equivalent to `osmium derive-changes`.
//!
//! Streams through both files writing changes directly to temp files,
//! then assembles the final `.osc.gz` in a single pass. Memory is bounded
//! by one element at a time, not total change count.
//! Requires both inputs to declare `Sort.Type_then_ID`.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use flate2::write::GzEncoder;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

use crate::osc::write::{
    OwnedMetadata,
    write_element_xml,
    write_node_xml, write_way_xml, write_relation_xml,
};
use crate::osc::merge_join::{
    block_pair_merge_phase, merge_join_phase, BlockMergeAction, BlockPairMergeState,
    MergeJoinAction, StreamingBlocks,
};
use crate::BoxResult as Result;
use crate::blob_meta::ElemKind;

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
/// Requires both inputs to declare `Sort.Type_then_ID` - returns an
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
    let (old_sorted, old_indexed) = super::check_sorted_and_indexed(old_path, direct_io)?;
    let (new_sorted, new_indexed) = super::check_sorted_and_indexed(new_path, direct_io)?;
    if !old_sorted { crate::commands::require_sorted_err(old_path, "Old PBF")?; }
    if !new_sorted { crate::commands::require_sorted_err(new_path, "New PBF")?; }
    let both_indexed = old_indexed && new_indexed;

    // Scratch directory for temp files (same as brokkr scratch).
    let scratch_dir = output.parent().unwrap_or(Path::new("."));
    let mut sink = ChangeSink::new(scratch_dir, increment_version, update_timestamp)?;

    crate::debug::emit_marker("DERIVECHANGES_SCAN_START");

    let result = (|| -> Result<DeriveChangesStats> {
        if both_indexed {
            derive_changes_block_pair(old_path, new_path, direct_io, &mut sink)?;
        } else {
            derive_changes_element_stream(old_path, new_path, direct_io, &mut sink)?;
        }
        sink.flush()?;

        crate::debug::emit_marker("DERIVECHANGES_SCAN_END");

        let stats = sink.stats();

        crate::debug::emit_marker("DERIVECHANGES_WRITE_START");
        assemble_osc(output, &sink)?;
        crate::debug::emit_marker("DERIVECHANGES_WRITE_END");
        Ok(stats)
    })();
    // Always clean up temp files, even on error
    sink.cleanup();
    let stats = result?;

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("derivechanges_creates", stats.creates as i64);
        crate::debug::emit_counter("derivechanges_modifies", stats.modifies as i64);
        crate::debug::emit_counter("derivechanges_deletes", stats.deletes as i64);
    }

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Streaming change sink - writes element XML directly to temp files
// ---------------------------------------------------------------------------

struct ChangeSink {
    creates: Writer<io::BufWriter<File>>,
    modifies: Writer<io::BufWriter<File>>,
    deletes: Writer<io::BufWriter<File>>,
    creates_path: std::path::PathBuf,
    modifies_path: std::path::PathBuf,
    deletes_path: std::path::PathBuf,
    create_count: u64,
    modify_count: u64,
    delete_count: u64,
    increment_version: bool,
    update_timestamp: bool,
    coord_buf: String,
}

impl ChangeSink {
    fn new(
        scratch_dir: &Path,
        increment_version: bool,
        update_timestamp: bool,
    ) -> io::Result<Self> {
        let pid = std::process::id();
        let cp = scratch_dir.join(format!("derive-creates-{pid}.xml.tmp"));
        let mp = scratch_dir.join(format!("derive-modifies-{pid}.xml.tmp"));
        let dp = scratch_dir.join(format!("derive-deletes-{pid}.xml.tmp"));
        Ok(Self {
            creates: Writer::new(io::BufWriter::new(File::create(&cp)?)),
            modifies: Writer::new(io::BufWriter::new(File::create(&mp)?)),
            deletes: Writer::new(io::BufWriter::new(File::create(&dp)?)),
            creates_path: cp,
            modifies_path: mp,
            deletes_path: dp,
            create_count: 0,
            modify_count: 0,
            delete_count: 0,
            increment_version,
            update_timestamp,
            coord_buf: String::new(),
        })
    }

    fn write_create(&mut self, elem: &crate::Element<'_>, _kind: ElemKind) -> Result<()> {
        write_element_xml(&mut self.creates, elem, &mut self.coord_buf)?;
        self.create_count += 1;
        Ok(())
    }

    fn write_modify(&mut self, elem: &crate::Element<'_>, _kind: ElemKind) -> Result<()> {
        write_element_xml(&mut self.modifies, elem, &mut self.coord_buf)?;
        self.modify_count += 1;
        Ok(())
    }

    fn write_delete(&mut self, elem: &crate::Element<'_>, kind: ElemKind) -> Result<()> {
        if let Some((tag, id, meta)) = extract_delete_info(elem, kind) {
            write_delete_element(
                &mut self.deletes, tag, id, meta.as_ref(),
                self.increment_version, self.update_timestamp,
            )?;
            self.delete_count += 1;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.creates.get_mut().flush()?;
        self.modifies.get_mut().flush()?;
        self.deletes.get_mut().flush()?;
        Ok(())
    }

    fn cleanup(&self) {
        drop(std::fs::remove_file(&self.creates_path));
        drop(std::fs::remove_file(&self.modifies_path));
        drop(std::fs::remove_file(&self.deletes_path));
    }

    fn stats(&self) -> DeriveChangesStats {
        DeriveChangesStats {
            creates: self.create_count,
            modifies: self.modify_count,
            deletes: self.delete_count,
        }
    }
}

/// Extract just id + metadata from a borrowed element for delete output.
/// Avoids full owned conversion (no tag/ref cloning - deletes only need id + version).
fn extract_delete_info(elem: &crate::Element<'_>, kind: ElemKind) -> Option<(&'static str, i64, Option<OwnedMetadata>)> {
    match (kind, elem) {
        (ElemKind::Node, crate::Element::DenseNode(dn)) => Some(("node", dn.id(),
            dn.info().map(crate::dense::DenseNodeInfo::version).filter(|&v| v != -1).map(OwnedMetadata::version_only))),
        (ElemKind::Node, crate::Element::Node(n)) => Some(("node", n.id(),
            n.info().version().map(OwnedMetadata::version_only))),
        (ElemKind::Way, crate::Element::Way(w)) => Some(("way", w.id(),
            w.info().version().map(OwnedMetadata::version_only))),
        (ElemKind::Relation, crate::Element::Relation(r)) => Some(("relation", r.id(),
            r.info().version().map(OwnedMetadata::version_only))),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Change collection via shared merge-join
// ---------------------------------------------------------------------------


// ---------------------------------------------------------------------------
// Optimized block-pair path (borrowed elements, zero-alloc for Equal)
// ---------------------------------------------------------------------------

/// Stream changes using block-pair merge with borrowed elements.
/// Only changed elements (~1.2% of typical daily diff) are materialized as owned,
/// written to temp files immediately, then dropped.
fn derive_changes_block_pair(
    old_path: &Path,
    new_path: &Path,
    direct_io: bool,
    sink: &mut ChangeSink,
) -> Result<()> {
    let mut old_reader = crate::blob::BlobReader::open(old_path, direct_io)?;
    old_reader.set_parse_indexdata(true);
    let mut new_reader = crate::blob::BlobReader::open(new_path, direct_io)?;
    new_reader.set_parse_indexdata(true);

    let mut merge = BlockPairMergeState::new(old_reader, new_reader);

    collect_phase_block_pair(&mut merge, ElemKind::Node, sink)?;
    collect_phase_block_pair(&mut merge, ElemKind::Way, sink)?;
    collect_phase_block_pair(&mut merge, ElemKind::Relation, sink)?;

    Ok(())
}

/// Run one type phase of block-pair merge, streaming changed elements to temp files.
fn collect_phase_block_pair(
    merge: &mut BlockPairMergeState,
    kind: ElemKind,
    sink: &mut ChangeSink,
) -> Result<()> {
    block_pair_merge_phase(merge, kind, true, &mut |action| {
        match action {
            BlockMergeAction::BlobEqual(_) | BlockMergeAction::ElementEqual { .. } => {}
            BlockMergeAction::BlobOldOnly { block, skip, .. } => {
                for elem in block.elements().skip(skip) {
                    sink.write_delete(&elem, kind)?;
                }
            }
            BlockMergeAction::BlobNewOnly { block, skip, .. } => {
                for elem in block.elements().skip(skip) {
                    sink.write_create(&elem, kind)?;
                }
            }
            BlockMergeAction::ElementModified { new, .. } => {
                sink.write_modify(new, kind)?;
            }
            BlockMergeAction::ElementOldOnly(o) => {
                sink.write_delete(o, kind)?;
            }
            BlockMergeAction::ElementNewOnly(n) => {
                sink.write_create(n, kind)?;
            }
        }
        Ok(())
    })
}

/// Fallback path using element-level merge-join with owned elements.
/// Streams changes directly to temp files via `ChangeSink`.
fn derive_changes_element_stream(
    old_path: &Path,
    new_path: &Path,
    direct_io: bool,
    sink: &mut ChangeSink,
) -> Result<()> {
    use crate::osc::write::{OwnedNode, OwnedWay, OwnedRelation};

    let mut old_src = StreamingBlocks::new_sequential(old_path, direct_io)?;
    let mut new_src = StreamingBlocks::new_sequential(new_path, direct_io)?;

    let iv = sink.increment_version;
    let ut = sink.update_timestamp;

    // Phase 1: Nodes
    {
        let (mut ob, mut nb): (Vec<OwnedNode>, Vec<OwnedNode>) = (Vec::new(), Vec::new());
        merge_join_phase(&mut old_src, &mut ob, &mut new_src, &mut nb, |action| {
            match action {
                MergeJoinAction::OldOnly(n) => {
                    write_delete_element(&mut sink.deletes, "node", n.id, n.metadata.as_ref(), iv, ut)?;
                    sink.delete_count += 1;
                }
                MergeJoinAction::NewOnly(n) => { write_node_xml(&mut sink.creates, n)?; sink.create_count += 1; }
                MergeJoinAction::Modified(_, n) => { write_node_xml(&mut sink.modifies, n)?; sink.modify_count += 1; }
                MergeJoinAction::Equal(_) => {}
            }
            Ok(())
        })?;
    }
    // Phase 2: Ways
    {
        let (mut ob, mut nb): (Vec<OwnedWay>, Vec<OwnedWay>) = (Vec::new(), Vec::new());
        merge_join_phase(&mut old_src, &mut ob, &mut new_src, &mut nb, |action| {
            match action {
                MergeJoinAction::OldOnly(w) => {
                    write_delete_element(&mut sink.deletes, "way", w.id, w.metadata.as_ref(), iv, ut)?;
                    sink.delete_count += 1;
                }
                MergeJoinAction::NewOnly(w) => { write_way_xml(&mut sink.creates, w)?; sink.create_count += 1; }
                MergeJoinAction::Modified(_, w) => { write_way_xml(&mut sink.modifies, w)?; sink.modify_count += 1; }
                MergeJoinAction::Equal(_) => {}
            }
            Ok(())
        })?;
    }
    // Phase 3: Relations
    {
        let (mut ob, mut nb): (Vec<OwnedRelation>, Vec<OwnedRelation>) = (Vec::new(), Vec::new());
        merge_join_phase(&mut old_src, &mut ob, &mut new_src, &mut nb, |action| {
            match action {
                MergeJoinAction::OldOnly(r) => {
                    write_delete_element(&mut sink.deletes, "relation", r.id, r.metadata.as_ref(), iv, ut)?;
                    sink.delete_count += 1;
                }
                MergeJoinAction::NewOnly(r) => { write_relation_xml(&mut sink.creates, r)?; sink.create_count += 1; }
                MergeJoinAction::Modified(_, r) => { write_relation_xml(&mut sink.modifies, r)?; sink.modify_count += 1; }
                MergeJoinAction::Equal(_) => {}
            }
            Ok(())
        })?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// OSC XML writer
// ---------------------------------------------------------------------------

/// Assemble the final `.osc.gz` from temp file fragments.
/// Writes XML structure via quick_xml Writer, copies raw fragment bytes
/// directly to the underlying GzEncoder (not through the XML writer).
fn assemble_osc(output: &Path, sink: &ChangeSink) -> Result<()> {
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

    // Copy each non-empty temp file as a section
    copy_section(&mut writer, "create", &sink.creates_path, sink.create_count)?;
    copy_section(&mut writer, "modify", &sink.modifies_path, sink.modify_count)?;
    copy_section(&mut writer, "delete", &sink.deletes_path, sink.delete_count)?;

    // </osmChange>
    writer.write_event(Event::End(BytesEnd::new("osmChange")))?;

    let gz = writer.into_inner();
    gz.finish()?;
    Ok(())
}

/// Copy a temp file's raw XML bytes into an action section.
/// Flushes the XML writer before copying raw bytes to the underlying writer,
/// then resumes structured writing for the closing tag.
fn copy_section<W: Write>(
    writer: &mut Writer<W>,
    tag: &str,
    path: &Path,
    count: u64,
) -> Result<()> {
    if count == 0 {
        return Ok(());
    }
    writer.write_event(Event::Start(BytesStart::new(tag)))?;
    // Flush the XML writer's internal buffer before raw byte copy
    writer.get_mut().flush()?;
    // Copy raw XML fragment bytes directly to the underlying writer
    let mut tmp = io::BufReader::new(File::open(path)?);
    io::copy(&mut tmp, writer.get_mut())?;
    writer.write_event(Event::End(BytesEnd::new(tag)))?;
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
        let ts = crate::commands::format_epoch_secs(now.as_secs());
        elem.push_attribute(("timestamp", ts.as_str()));
    }
    writer.write_event(Event::Empty(elem))?;
    Ok(())
}

