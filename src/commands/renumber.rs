//! Renumber OSM elements with sequential IDs. Equivalent to `osmium renumber`.
//!
//! Single-pass sequential scan: assigns new IDs starting from configurable values
//! and remaps all cross-references (way→node, relation→node/way/relation).

use std::path::Path;

use super::{
    dense_node_metadata, element_metadata, ensure_node_capacity, ensure_relation_capacity,
    ensure_way_capacity, flush_block, require_sorted, writer_from_header, HeaderOverrides, Result,
};
use crate::blob::DecompressPool;
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
pub struct RenumberStats {
    pub nodes_written: u64,
    pub ways_written: u64,
    pub relations_written: u64,
}

impl RenumberStats {
    pub fn print_summary(&self) {
        let total = self.nodes_written + self.ways_written + self.relations_written;
        eprintln!(
            "Renumbered {total} elements: {} nodes, {} ways, {} relations",
            self.nodes_written, self.ways_written, self.relations_written,
        );
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
    };

    let mut refs_buf: Vec<i64> = Vec::new();
    let decompress_pool = DecompressPool::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) { continue; }
        let decompressed = blob.decompress_pooled(&decompress_pool)?;
        let block = crate::block::PrimitiveBlock::new_with_scratch(decompressed, &mut st_scratch, &mut gr_scratch)?;
        let mut members_buf: Vec<MemberData<'_>> = Vec::new();
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
                    refs_buf.extend(w.refs().map(|r| node_map.get(&r).copied().unwrap_or(r)));
                    let meta = element_metadata(&w.info());
                    bb.add_way(new_id, w.tags(), &refs_buf, meta.as_ref());
                    stats.ways_written += 1;
                }
                Element::Relation(r) => {
                    ensure_relation_capacity(&mut bb, &mut writer)?;
                    let new_id = next_relation_id;
                    next_relation_id += 1;
                    relation_map.insert(r.id(), new_id);
                    members_buf.clear();
                    members_buf.extend(r.members().map(|m| {
                        let remapped_id = match m.id {
                            MemberId::Node(id) => MemberId::Node(node_map.get(&id).copied().unwrap_or(id)),
                            MemberId::Way(id) => MemberId::Way(way_map.get(&id).copied().unwrap_or(id)),
                            MemberId::Relation(id) => MemberId::Relation(relation_map.get(&id).copied().unwrap_or(id)),
                            MemberId::Unknown(t, id) => MemberId::Unknown(t, id),
                        };
                        MemberData { id: remapped_id, role: m.role().unwrap_or("") }
                    }));
                    let meta = element_metadata(&r.info());
                    bb.add_relation(new_id, r.tags(), &members_buf, meta.as_ref());
                    stats.relations_written += 1;
                }
            }
        }
    }

    flush_block(&mut bb, &mut writer)?;
    writer.flush()?;

    Ok(stats)
}
