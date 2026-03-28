//! Renumber OSM elements with sequential IDs. Equivalent to `osmium renumber`.
//!
//! Single-pass sequential scan: assigns new IDs starting from configurable values
//! and remaps all cross-references (way→node, relation→node/way/relation).

use std::collections::HashMap;
use std::path::Path;

use super::{
    dense_node_metadata, element_metadata, ensure_node_capacity, ensure_relation_capacity,
    ensure_way_capacity, flush_block, require_sorted, writer_from_header, HeaderOverrides, Result,
};
use crate::block_builder::{BlockBuilder, MemberData};
use crate::writer::Compression;
use crate::{Element, ElementReader, MemberId};

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
    let reader = ElementReader::open(input, direct_io)?;
    require_sorted(reader.header(), input, "Input PBF")?;
    super::warn_locations_on_ways_loss(reader.header());

    let mut writer = writer_from_header(output, compression, reader.header(), true, overrides, |hb| {
        hb.sorted()
    }, direct_io, false)?;
    let mut bb = BlockBuilder::new();

    let mut node_map: HashMap<i64, i64> = HashMap::new();
    let mut way_map: HashMap<i64, i64> = HashMap::new();
    let mut relation_map: HashMap<i64, i64> = HashMap::new();

    let mut next_node_id = opts.start_node_id;
    let mut next_way_id = opts.start_way_id;
    let mut next_relation_id = opts.start_relation_id;

    let mut stats = RenumberStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
    };

    let mut refs_buf: Vec<i64> = Vec::new();

    let blocks = reader.into_blocks_pipelined();
    for block in blocks {
        let block = block?;
        let mut tags_buf: Vec<(&str, &str)> = Vec::new();
        let mut members_buf: Vec<MemberData<'_>> = Vec::new();
        for element in block.elements() {
            match &element {
                Element::DenseNode(dn) => {
                    ensure_node_capacity(&mut bb, &mut writer)?;
                    let new_id = next_node_id;
                    next_node_id += 1;
                    node_map.insert(dn.id(), new_id);
                    tags_buf.clear();
                    tags_buf.extend(dn.tags());
                    let meta = dense_node_metadata(dn);
                    bb.add_node(new_id, dn.decimicro_lat(), dn.decimicro_lon(), &tags_buf, meta.as_ref());
                    stats.nodes_written += 1;
                }
                Element::Node(n) => {
                    ensure_node_capacity(&mut bb, &mut writer)?;
                    let new_id = next_node_id;
                    next_node_id += 1;
                    node_map.insert(n.id(), new_id);
                    tags_buf.clear();
                    tags_buf.extend(n.tags());
                    let meta = element_metadata(&n.info());
                    bb.add_node(new_id, n.decimicro_lat(), n.decimicro_lon(), &tags_buf, meta.as_ref());
                    stats.nodes_written += 1;
                }
                Element::Way(w) => {
                    ensure_way_capacity(&mut bb, &mut writer)?;
                    let new_id = next_way_id;
                    next_way_id += 1;
                    way_map.insert(w.id(), new_id);
                    tags_buf.clear();
                    tags_buf.extend(w.tags());
                    refs_buf.clear();
                    refs_buf.extend(w.refs().map(|r| node_map.get(&r).copied().unwrap_or(r)));
                    let meta = element_metadata(&w.info());
                    bb.add_way(new_id, &tags_buf, &refs_buf, meta.as_ref());
                    stats.ways_written += 1;
                }
                Element::Relation(r) => {
                    ensure_relation_capacity(&mut bb, &mut writer)?;
                    let new_id = next_relation_id;
                    next_relation_id += 1;
                    relation_map.insert(r.id(), new_id);
                    tags_buf.clear();
                    tags_buf.extend(r.tags());
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
                    bb.add_relation(new_id, &tags_buf, &members_buf, meta.as_ref());
                    stats.relations_written += 1;
                }
            }
        }
    }

    flush_block(&mut bb, &mut writer)?;
    writer.flush()?;

    Ok(stats)
}
