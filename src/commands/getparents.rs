//! Reverse lookup: find ways/relations referencing given IDs. Equivalent to `osmium getparents`.
//!
//! Single-pass scan: for each way, check if any of its node refs are in the requested set;
//! for each relation, check if any member matches. Only one level of indirection.
//!
//! Uses sequential BlobReader — per-block work is lightweight (ID lookups +
//! conditional writes), so pipelined decode + rayon par_iter overhead exceeds
//! the actual per-block CPU cost. Sequential decode eliminates both the
//! dedicated decode thread pool and the par_iter scheduling overhead.

use std::path::Path;

use super::getid::IdSet;
use super::{
    dense_node_metadata, element_metadata, flush_block,
    writer_from_header, HeaderOverrides,
    ensure_node_capacity, ensure_way_capacity, ensure_relation_capacity,
};
use crate::block_builder::{BlockBuilder, MemberData};
use crate::writer::Compression;
use crate::{Element, MemberId, PrimitiveBlock};

use super::Result;

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
    let need_nodes = opts.add_self && ids.node_ids.has_any();
    let blob_filter = crate::BlobFilter::new(need_nodes, true, true);

    // Open BlobReader and decode header for the writer.
    let mut blob_reader = crate::blob::BlobReader::open(input, direct_io)?;
    blob_reader.set_parse_indexdata(true);
    let header = match blob_reader.next() {
        Some(Ok(blob)) => match blob.decode()? {
            crate::blob::BlobDecode::OsmHeader(h) => *h,
            _ => return Err(crate::error::new_error(crate::error::ErrorKind::MissingHeader).into()),
        },
        Some(Err(e)) => return Err(e.into()),
        None => return Err(crate::error::new_error(crate::error::ErrorKind::MissingHeader).into()),
    };

    super::warn_locations_on_ways_loss(&header);
    let mut writer = writer_from_header(output, compression, &header, true, overrides, |hb| hb, direct_io, false)?;

    let mut stats = GetparentsStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
    };

    let mut bb = BlockBuilder::new();
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    crate::debug::emit_marker("GETPARENTS_START");

    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) { continue; }
        if let Some(idx) = blob.index() {
            if !blob_filter.wants_index(&idx) { continue; }
        }

        blob.decompress_into(&mut decompress_buf)?;
        let block = PrimitiveBlock::from_vec_with_scratch(
            std::mem::take(&mut decompress_buf), &mut st_scratch, &mut gr_scratch,
        )?;

        // Per-block scratch — refs_buf is i64 (no borrows), members_buf borrows
        // &str from block's string table so must not outlive block.
        let mut refs_buf: Vec<i64> = Vec::new();
        let mut members_buf: Vec<MemberData<'_>> = Vec::new();

        for element in block.elements() {
            match &element {
                Element::DenseNode(dn) => {
                    // Nodes are never parents. Include only if --add-self and ID matches.
                    if opts.add_self && ids.node_ids.get(dn.id()) {
                        ensure_node_capacity(&mut bb, &mut writer)?;
                        let meta = dense_node_metadata(dn);
                        bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), dn.tags(), meta.as_ref());
                        stats.nodes_written += 1;
                    }
                }
                Element::Node(n) => {
                    if opts.add_self && ids.node_ids.get(n.id()) {
                        ensure_node_capacity(&mut bb, &mut writer)?;
                        let meta = element_metadata(&n.info());
                        bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), n.tags(), meta.as_ref());
                        stats.nodes_written += 1;
                    }
                }
                Element::Way(w) => {
                    let is_parent = w.refs().any(|r| ids.node_ids.get(r));
                    let is_self = opts.add_self && ids.way_ids.get(w.id());
                    if is_parent || is_self {
                        ensure_way_capacity(&mut bb, &mut writer)?;
                        refs_buf.clear();
                        refs_buf.extend(w.refs());
                        let meta = element_metadata(&w.info());
                        bb.add_way(w.id(), w.tags(), &refs_buf, meta.as_ref());
                        stats.ways_written += 1;
                    }
                }
                Element::Relation(r) => {
                    let is_parent = r.members().any(|m| match m.id {
                        MemberId::Node(id) => ids.node_ids.get(id),
                        MemberId::Way(id) => ids.way_ids.get(id),
                        MemberId::Relation(id) => ids.relation_ids.get(id),
                        MemberId::Unknown(..) => false,
                    });
                    let is_self = opts.add_self && ids.relation_ids.get(r.id());
                    if is_parent || is_self {
                        ensure_relation_capacity(&mut bb, &mut writer)?;
                        members_buf.clear();
                        members_buf.extend(r.members().map(|m| MemberData {
                            id: m.id,
                            role: m.role().unwrap_or(""),
                        }));
                        let meta = element_metadata(&r.info());
                        bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
                        stats.relations_written += 1;
                    }
                }
            }
        }
    }

    flush_block(&mut bb, &mut writer)?;
    writer.flush()?;
    crate::debug::emit_marker("GETPARENTS_END");
    Ok(stats)
}
