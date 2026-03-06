//! Reverse lookup: find ways/relations referencing given IDs. Equivalent to `osmium getparents`.
//!
//! Single-pass scan: for each way, check if any of its node refs are in the requested set;
//! for each relation, check if any member matches. Only one level of indirection.

use std::path::Path;

use rayon::prelude::*;

use super::getid::IdSet;
use super::{
    dense_node_metadata, drain_batch_results, element_metadata, flush_local,
    for_each_primitive_block_batch, writer_from_header, HeaderOverrides,
    ensure_node_capacity_local, ensure_way_capacity_local, ensure_relation_capacity_local,
};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::writer::Compression;
use crate::{Element, ElementReader, MemberId, PrimitiveBlock};

use super::{Result, BATCH_SIZE};

/// Options for the getparents command.
pub struct GetparentsOptions {
    /// Also include the queried objects themselves in the output.
    pub add_self: bool,
}

/// Statistics from a getparents operation.
pub struct GetparentsStats {
    pub nodes_written: u64,
    pub ways_written: u64,
    pub relations_written: u64,
}

impl GetparentsStats {
    pub fn print_summary(&self) {
        let total = self.nodes_written + self.ways_written + self.relations_written;
        eprintln!(
            "Wrote {total} elements: {} nodes, {} ways, {} relations",
            self.nodes_written, self.ways_written, self.relations_written,
        );
    }
}

/// Find parent ways/relations referencing the given IDs.
#[hotpath::measure]
pub fn getparents(
    input: &Path,
    output: &Path,
    ids: &IdSet,
    opts: &GetparentsOptions,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<GetparentsStats> {
    {
        let reader = crate::ElementReader::open(input, direct_io)?;
        super::warn_locations_on_ways_loss(reader.header());
    }

    let reader = ElementReader::open(input, direct_io)?;
    let mut writer = writer_from_header(output, compression, reader.header(), true, overrides, |hb| hb)?;
    let mut stats = GetparentsStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
    };

    for_each_primitive_block_batch(reader.into_blocks_pipelined(), BATCH_SIZE, |batch| {
        let (nodes, ways, relations) = process_batch(
            batch, &mut writer, ids, opts.add_self,
        )?;
        stats.nodes_written += nodes;
        stats.ways_written += ways;
        stats.relations_written += relations;
        Ok(())
    })?;

    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Parallel batch processing
// ---------------------------------------------------------------------------

fn process_block(
    block: &PrimitiveBlock,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    ids: &IdSet,
    add_self: bool,
) -> std::result::Result<(u64, u64, u64), String> {
    let mut nodes: u64 = 0;
    let mut ways: u64 = 0;
    let mut relations: u64 = 0;

    let mut tags_buf: Vec<(&str, &str)> = Vec::new();
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                // Nodes are never parents. Include only if --add-self and ID matches.
                if add_self && ids.node_ids.contains(&dn.id()) {
                    ensure_node_capacity_local(bb, output)?;
                    tags_buf.clear();
                    tags_buf.extend(dn.tags());
                    let meta = dense_node_metadata(dn);
                    bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), &tags_buf, meta.as_ref());
                    nodes += 1;
                }
            }
            Element::Node(n) => {
                if add_self && ids.node_ids.contains(&n.id()) {
                    ensure_node_capacity_local(bb, output)?;
                    tags_buf.clear();
                    tags_buf.extend(n.tags());
                    let meta = element_metadata(&n.info());
                    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), &tags_buf, meta.as_ref());
                    nodes += 1;
                }
            }
            Element::Way(w) => {
                // A way is a parent if it references any requested node ID.
                let is_parent = w.refs().any(|r| ids.node_ids.contains(&r));
                let is_self = add_self && ids.way_ids.contains(&w.id());
                if is_parent || is_self {
                    ensure_way_capacity_local(bb, output)?;
                    tags_buf.clear();
                    tags_buf.extend(w.tags());
                    refs_buf.clear();
                    refs_buf.extend(w.refs());
                    let meta = element_metadata(&w.info());
                    bb.add_way(w.id(), &tags_buf, &refs_buf, meta.as_ref());
                    ways += 1;
                }
            }
            Element::Relation(r) => {
                // A relation is a parent if any member matches a requested ID.
                let is_parent = r.members().any(|m| match m.id {
                    MemberId::Node(id) => ids.node_ids.contains(&id),
                    MemberId::Way(id) => ids.way_ids.contains(&id),
                    MemberId::Relation(id) => ids.relation_ids.contains(&id),
                    MemberId::Unknown(..) => false,
                });
                let is_self = add_self && ids.relation_ids.contains(&r.id());
                if is_parent || is_self {
                    ensure_relation_capacity_local(bb, output)?;
                    tags_buf.clear();
                    tags_buf.extend(r.tags());
                    members_buf.clear();
                    members_buf.extend(r.members().map(|m| MemberData {
                        id: m.id,
                        role: m.role().unwrap_or(""),
                    }));
                    let meta = element_metadata(&r.info());
                    bb.add_relation(r.id(), &tags_buf, &members_buf, meta.as_ref());
                    relations += 1;
                }
            }
        }
    }

    Ok((nodes, ways, relations))
}

fn process_batch(
    batch: &[PrimitiveBlock],
    writer: &mut crate::writer::PbfWriter<crate::file_writer::FileWriter>,
    ids: &IdSet,
    add_self: bool,
) -> Result<(u64, u64, u64)> {
    type BatchResult = std::result::Result<(Vec<OwnedBlock>, (u64, u64, u64)), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            BlockBuilder::new,
            |bb, block| {
                let mut output: Vec<OwnedBlock> = Vec::new();
                let counts = process_block(block, bb, &mut output, ids, add_self)?;
                flush_local(bb, &mut output)?;
                Ok((output, counts))
            },
        )
        .collect();

    let mut total_nodes: u64 = 0;
    let mut total_ways: u64 = 0;
    let mut total_relations: u64 = 0;
    drain_batch_results(results, writer, |(n, w, r)| {
        total_nodes += n;
        total_ways += w;
        total_relations += r;
    })?;

    Ok((total_nodes, total_ways, total_relations))
}
