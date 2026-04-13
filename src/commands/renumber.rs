//! Renumber OSM elements with sequential IDs. Equivalent to `osmium renumber`.
//!
//! Two-pass sequential scan:
//!
//! - **Pass 1**: stream nodes + ways, assign new IDs, remap way refs via
//!   `node_map`, write to output. For relations, assign new IDs into
//!   `relation_map` only — do not write.
//! - **Pass 2**: reopen the input, fast-skip non-relation blobs via the blob
//!   index, remap relation members using the now-complete maps, write to
//!   output.
//!
//! The two-pass relation handling is required for correctness: a single-pass
//! implementation falls through to `unwrap_or(old_id)` on forward relation→
//! relation references (target not yet assigned), silently writing the OLD
//! id into the new output. osmium-tool uses the same two-pass structure in
//! `command_renumber.cpp:380-403` for the same reason.

use std::path::Path;

use super::{
    dense_node_metadata, element_metadata, ensure_node_capacity, ensure_relation_capacity,
    ensure_way_capacity, flush_block, require_sorted, writer_from_header, HeaderOverrides, Result,
};
use crate::block_builder::{BlockBuilder, MemberData};
use crate::writer::Compression;
use crate::{Element, MemberId};

/// Configuration for the renumber command.
pub struct RenumberOptions {
    pub start_node_id: i64,
    pub start_way_id: i64,
    pub start_relation_id: i64,
}

/// Statistics from a renumber operation.
#[derive(Debug, Clone)]
pub struct RenumberStats {
    pub nodes_written: u64,
    pub ways_written: u64,
    pub relations_written: u64,
    /// Way refs and relation members whose old ID was not found in the
    /// corresponding ID set. These pass through with their old ID
    /// unchanged (orphan passthrough).
    pub orphan_refs: u64,
}

impl RenumberStats {
    pub fn print_summary(&self) {
        let total = self.nodes_written + self.ways_written + self.relations_written;
        eprintln!(
            "Renumbered {total} elements: {} nodes, {} ways, {} relations",
            self.nodes_written, self.ways_written, self.relations_written,
        );
        if self.orphan_refs > 0 {
            eprintln!(
                "Warning: {} orphan refs preserved with old IDs (referenced \
                 elements not present in input)",
                self.orphan_refs,
            );
        }
    }
}

/// Renumber all OSM elements with sequential IDs, remapping cross-references.
///
/// Input must be sorted (nodes before ways before relations). Processing is
/// sequential to ensure deterministic ID assignment.
#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub fn renumber(
    input: &Path,
    output: &Path,
    opts: &RenumberOptions,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<RenumberStats> {
    let mut blob_reader = crate::blob::BlobReader::open(input, direct_io)?;
    let header_blob = blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let header = header_blob.to_headerblock()?;
    require_sorted(&header, input, "Input PBF")?;
    super::warn_locations_on_ways_loss(&header);

    let mut writer = writer_from_header(output, compression, &header, true, overrides, |hb| {
        hb.sorted()
    }, direct_io, false)?;
    let mut bb = BlockBuilder::new();

    let mut node_map: rustc_hash::FxHashMap<i64, i64> = rustc_hash::FxHashMap::default();
    let mut way_map: rustc_hash::FxHashMap<i64, i64> = rustc_hash::FxHashMap::default();
    let mut relation_map: rustc_hash::FxHashMap<i64, i64> = rustc_hash::FxHashMap::default();

    let mut next_node_id = opts.start_node_id;
    let mut next_way_id = opts.start_way_id;
    let mut next_relation_id = opts.start_relation_id;

    let mut stats = RenumberStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
        orphan_refs: 0,
    };

    let mut refs_buf: Vec<i64> = Vec::new();
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    crate::debug::emit_marker("RENUMBER_START");

    // Pass 1: assign IDs for all three types, write nodes + ways to output,
    // defer relation writes to pass 2. Relation ID assignment happens here so
    // the full relation_map is ready by the time pass 2 begins remapping
    // forward relation→relation references.
    crate::debug::emit_marker("RENUMBER_PASS1_START");
    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) { continue; }
        blob.decompress_into(&mut decompress_buf)?;
        let block = crate::block::PrimitiveBlock::from_vec_with_scratch(
            std::mem::take(&mut decompress_buf), &mut st_scratch, &mut gr_scratch,
        )?;
        for element in block.elements() {
            match &element {
                Element::DenseNode(dn) => {
                    ensure_node_capacity(&mut bb, &mut writer)?;
                    let new_id = next_node_id;
                    next_node_id += 1;
                    node_map.insert(dn.id(), new_id);
                    let meta = dense_node_metadata(dn);
                    bb.add_node(new_id, dn.decimicro_lat(), dn.decimicro_lon(), dn.tags(), meta.as_ref());
                    stats.nodes_written += 1;
                }
                Element::Node(n) => {
                    ensure_node_capacity(&mut bb, &mut writer)?;
                    let new_id = next_node_id;
                    next_node_id += 1;
                    node_map.insert(n.id(), new_id);
                    let meta = element_metadata(&n.info());
                    bb.add_node(new_id, n.decimicro_lat(), n.decimicro_lon(), n.tags(), meta.as_ref());
                    stats.nodes_written += 1;
                }
                Element::Way(w) => {
                    ensure_way_capacity(&mut bb, &mut writer)?;
                    let new_id = next_way_id;
                    next_way_id += 1;
                    way_map.insert(w.id(), new_id);
                    refs_buf.clear();
                    for r in w.refs() {
                        if let Some(&new_ref) = node_map.get(&r) {
                            refs_buf.push(new_ref);
                        } else {
                            refs_buf.push(r);
                            stats.orphan_refs += 1;
                        }
                    }
                    let meta = element_metadata(&w.info());
                    bb.add_way(new_id, w.tags(), &refs_buf, meta.as_ref());
                    stats.ways_written += 1;
                }
                Element::Relation(r) => {
                    // Pass 1: assign a new id only. Member remap + write deferred
                    // to pass 2 so forward relation→relation refs see a fully-
                    // populated relation_map.
                    let new_id = next_relation_id;
                    next_relation_id += 1;
                    relation_map.insert(r.id(), new_id);
                }
            }
        }
    }
    crate::debug::emit_marker("RENUMBER_PASS1_END");
    drop(blob_reader);

    // Pass 2: rescan the input, fast-skip non-relation blobs, remap relation
    // members using the now-complete maps, and write to output.
    crate::debug::emit_marker("RENUMBER_PASS2_START");
    let mut blob_reader2 = crate::blob::BlobReader::open(input, direct_io)?;
    // Skip the header blob (already validated above).
    blob_reader2.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

    for blob_result in &mut blob_reader2 {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) { continue; }
        // Fast-skip non-relation blobs when the blob carries an indexdata
        // hint (the `indexed` variant datasets all do). Non-indexed PBFs
        // fall through to full decompress + element-level filter.
        if let Some(idx) = blob.index() {
            if !matches!(idx.kind, crate::blob_index::ElemKind::Relation) {
                continue;
            }
        }
        blob.decompress_into(&mut decompress_buf)?;
        let block = crate::block::PrimitiveBlock::from_vec_with_scratch(
            std::mem::take(&mut decompress_buf), &mut st_scratch, &mut gr_scratch,
        )?;
        let mut members_buf: Vec<MemberData<'_>> = Vec::new();
        for element in block.elements() {
            let Element::Relation(r) = &element else { continue };
            ensure_relation_capacity(&mut bb, &mut writer)?;
            // Every relation id was inserted in pass 1; missing entry is an
            // internal consistency violation, not a user-facing condition.
            let new_id = relation_map.get(&r.id()).copied().ok_or_else(|| {
                format!(
                    "internal error: relation id {} missing from relation_map in pass 2",
                    r.id()
                )
            })?;
            members_buf.clear();
            for m in r.members() {
                let (remapped_id, is_orphan) = match m.id {
                    MemberId::Node(id) => match node_map.get(&id) {
                        Some(&new_id) => (MemberId::Node(new_id), false),
                        None => (MemberId::Node(id), true),
                    },
                    MemberId::Way(id) => match way_map.get(&id) {
                        Some(&new_id) => (MemberId::Way(new_id), false),
                        None => (MemberId::Way(id), true),
                    },
                    MemberId::Relation(id) => match relation_map.get(&id) {
                        Some(&new_id) => (MemberId::Relation(new_id), false),
                        None => (MemberId::Relation(id), true),
                    },
                    MemberId::Unknown(t, id) => (MemberId::Unknown(t, id), false),
                };
                if is_orphan {
                    stats.orphan_refs += 1;
                }
                members_buf.push(MemberData { id: remapped_id, role: m.role().unwrap_or("") });
            }
            let meta = element_metadata(&r.info());
            bb.add_relation(new_id, r.tags(), &members_buf, meta.as_ref());
            stats.relations_written += 1;
        }
    }
    crate::debug::emit_marker("RENUMBER_PASS2_END");

    flush_block(&mut bb, &mut writer)?;
    writer.flush()?;

    crate::debug::emit_marker("RENUMBER_END");
    Ok(stats)
}
