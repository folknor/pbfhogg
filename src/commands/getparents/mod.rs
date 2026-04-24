//! Reverse lookup: find ways/relations referencing given IDs. Equivalent to `osmium getparents`.
//!
//! HeaderWalker-driven schedule: pread only the blob kinds whose bodies can
//! contribute matches. Workers decode and scan; a reorder buffer delivers
//! owned blocks to the writer in file order.
//!
//! Node blobs are skipped unless `--add-self` matches a node ID, saving the
//! ~75 % of planet bytes that nodes occupy. Way blobs are skipped when no
//! node IDs are in the query (nothing to ref), and relation blobs when the
//! query is empty of node/way/relation IDs.

use std::path::Path;
use std::sync::Arc;

use super::getid::ElementIds;
use super::{
    flush_local, writer_from_header_bytes, HeaderOverrides,
    ensure_node_capacity_local, ensure_way_capacity_local, ensure_relation_capacity_local,
};
use crate::blob::{decode_blob_to_headerblock, BlobKind};
use crate::blob_meta::ElemKind;
use crate::owned::{dense_node_metadata, element_metadata};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::read::header_walker::HeaderWalker;
use crate::reorder_buffer::ReorderBuffer;
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
#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub fn getparents(
    input: &Path,
    output: &Path,
    ids: &ElementIds,
    opts: &GetparentsOptions,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<GetparentsStats> {
    // Which blob kinds can contribute matches?
    // - node blobs: only for --add-self on node IDs
    // - way blobs: any way may reference a query node ID; --add-self on way IDs
    // - relation blobs: any relation may reference a query node/way/relation ID;
    //   --add-self on relation IDs
    let need_node_blobs = opts.add_self && ids.node_ids.has_any();
    let need_way_blobs = ids.node_ids.has_any()
        || (opts.add_self && ids.way_ids.has_any());
    let need_relation_blobs = ids.node_ids.has_any()
        || ids.way_ids.has_any()
        || ids.relation_ids.has_any();

    crate::debug::emit_marker("GETPARENTS_SCHEDULE_START");
    let mut walker = HeaderWalker::open(input)?;
    let file_size = walker.file_size();
    let mut header_buf: Vec<u8> = Vec::new();
    let mut header_block: Option<crate::HeaderBlock> = None;
    let mut schedule: Vec<(usize, u64, usize)> = Vec::new();
    let mut blobs_skipped: u64 = 0;

    while let Some(meta) = walker.next_header()? {
        match meta.blob_type {
            BlobKind::OsmHeader if header_block.is_none() => {
                walker.pread_data(meta.data_offset, meta.data_size, &mut header_buf)?;
                header_block = Some(decode_blob_to_headerblock(&header_buf)?);
            }
            BlobKind::OsmData => {
                let keep = match meta.index.as_ref().map(|i| i.kind) {
                    Some(ElemKind::Node) => need_node_blobs,
                    Some(ElemKind::Way) => need_way_blobs,
                    Some(ElemKind::Relation) => need_relation_blobs,
                    // Unindexed blob: include conservatively since we don't
                    // know its kind without decoding.
                    None => true,
                };
                if keep {
                    if meta.data_offset + meta.data_size as u64 > file_size {
                        return Err(format!(
                            "blob at offset {} claims data_size {} but file is only {} bytes",
                            meta.data_offset, meta.data_size, file_size,
                        ).into());
                    }
                    schedule.push((schedule.len(), meta.data_offset, meta.data_size));
                } else {
                    blobs_skipped += 1;
                }
            }
            _ => {}
        }
    }

    let shared_file = Arc::clone(walker.shared_file());
    drop(walker);

    let header = header_block.ok_or_else(|| {
        crate::error::new_error(crate::error::ErrorKind::MissingHeader)
    })?;

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("getparents_schedule_blobs", schedule.len() as i64);
        crate::debug::emit_counter("getparents_blobs_skipped", blobs_skipped as i64);
    }
    crate::debug::emit_marker("GETPARENTS_SCHEDULE_END");

    super::warn_locations_on_ways_loss(&header);
    let header_bytes = super::build_output_header(&header, true, overrides, |hb| hb)?;
    let mut writer = writer_from_header_bytes(
        output, compression, &header_bytes, direct_io, false,
    )?;

    let mut stats = GetparentsStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
    };

    crate::debug::emit_marker("GETPARENTS_DECODE_START");
    type ClassifyResult =
        std::result::Result<(Vec<OwnedBlock>, (u64, u64, u64)), String>;
    let mut reorder: ReorderBuffer<ClassifyResult> = ReorderBuffer::with_capacity(32);
    // Captured write error: `parallel_classify_phase`'s `merge` is `FnMut(usize, R)`
    // and cannot return a Result. Capture the first error here and bail out
    // at the end of the phase.
    let mut write_err: Option<Box<dyn std::error::Error + Send + Sync>> = None;

    crate::scan::classify::parallel_classify_phase(
        &shared_file,
        &schedule,
        None,
        // `BlockBuilder` contains `Rc<str>` (string table) and is not
        // Send, so it can't live as worker state across
        // `parallel_classify_phase`'s `S: Send` bound. Instead, each
        // classify call materialises its own builder, flushing into a
        // per-blob `Vec<OwnedBlock>` before returning.
        || (),
        |block, _state| -> ClassifyResult {
            let mut bb = BlockBuilder::new();
            let mut output: Vec<OwnedBlock> = Vec::new();
            let counts = process_block(block, &mut bb, &mut output, ids, opts.add_self)?;
            flush_local(&mut bb, &mut output)?;
            Ok((output, counts))
        },
        |seq, result| {
            if write_err.is_some() { return; }
            reorder.push(seq, result);
            while let Some(item) = reorder.pop_ready() {
                match item {
                    Ok((blocks, (n, w, r))) => {
                        for (block_bytes, index, tagdata) in blocks {
                            if let Err(e) = writer.write_primitive_block_owned(
                                block_bytes, index, tagdata.as_deref(),
                            ) {
                                write_err = Some(Box::new(e));
                                return;
                            }
                        }
                        stats.nodes_written += n;
                        stats.ways_written += w;
                        stats.relations_written += r;
                    }
                    Err(e) => {
                        write_err = Some(e.into());
                        return;
                    }
                }
            }
        },
    )?;

    if let Some(e) = write_err {
        return Err(e);
    }

    writer.flush()?;
    crate::debug::emit_marker("GETPARENTS_DECODE_END");
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Parallel batch processing
// ---------------------------------------------------------------------------

fn process_block(
    block: &PrimitiveBlock,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    ids: &ElementIds,
    add_self: bool,
) -> std::result::Result<(u64, u64, u64), String> {
    let mut nodes: u64 = 0;
    let mut ways: u64 = 0;
    let mut relations: u64 = 0;

    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                // Nodes are never parents. Include only if --add-self and ID matches.
                if add_self && ids.node_ids.get(dn.id()) {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = dense_node_metadata(dn);
                    bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), dn.tags(), meta.as_ref());
                    nodes += 1;
                }
            }
            Element::Node(n) => {
                if add_self && ids.node_ids.get(n.id()) {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = element_metadata(&n.info());
                    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), n.tags(), meta.as_ref());
                    nodes += 1;
                }
            }
            Element::Way(w) => {
                // A way is a parent if it references any requested node ID.
                let is_parent = w.refs().any(|r| ids.node_ids.get(r));
                let is_self = add_self && ids.way_ids.get(w.id());
                if is_parent || is_self {
                    ensure_way_capacity_local(bb, output)?;
                    refs_buf.clear();
                    refs_buf.extend(w.refs());
                    let meta = element_metadata(&w.info());
                    bb.add_way(w.id(), w.tags(), &refs_buf, meta.as_ref());
                    ways += 1;
                }
            }
            Element::Relation(r) => {
                // A relation is a parent if any member matches a requested ID.
                let is_parent = r.members().any(|m| match m.id {
                    MemberId::Node(id) => ids.node_ids.get(id),
                    MemberId::Way(id) => ids.way_ids.get(id),
                    MemberId::Relation(id) => ids.relation_ids.get(id),
                    MemberId::Unknown(..) => false,
                });
                let is_self = add_self && ids.relation_ids.get(r.id());
                if is_parent || is_self {
                    ensure_relation_capacity_local(bb, output)?;
                    members_buf.clear();
                    members_buf.extend(r.members().map(|m| MemberData {
                        id: m.id,
                        role: m.role().unwrap_or(""),
                    }));
                    let meta = element_metadata(&r.info());
                    bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
                    relations += 1;
                }
            }
        }
    }

    Ok((nodes, ways, relations))
}

