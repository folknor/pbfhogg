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

use rayon::prelude::*;

use super::getid::ElementIds;
use super::{
    BATCH_SIZE, HeaderOverrides, drain_batch_results, ensure_node_capacity_local,
    ensure_relation_capacity_local, ensure_way_capacity_local, flush_local,
    for_each_primitive_block_batch, writer_from_header_bytes,
};
use crate::blob::{BlobKind, decode_blob_to_headerblock};
use crate::blob_meta::ElemKind;
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::owned::{dense_node_metadata, element_metadata};
use crate::read::header_walker::{HeaderWalker, PIPELINED_ARM_MIN_BLOBS, ScanArm};
use crate::reorder_buffer::ReorderBuffer;
use crate::writer::Compression;
use crate::{BlobFilter, Element, ElementReader, MemberId, PrimitiveBlock};

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
    ids: &ElementIds,
    opts: &GetparentsOptions,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<GetparentsStats> {
    let (_, stats) = getparents_dispatched(
        input,
        output,
        ids,
        opts,
        compression,
        direct_io,
        overrides,
        PIPELINED_ARM_MIN_BLOBS,
    )?;
    Ok(stats)
}

/// Dispatch on the blob-count estimate, then run the selected arm. Returns
/// the arm alongside the stats so tests can inject `min_blobs` and assert
/// which arm auto-dispatch actually executed.
#[allow(clippy::too_many_arguments)]
fn getparents_dispatched(
    input: &Path,
    output: &Path,
    ids: &ElementIds,
    opts: &GetparentsOptions,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
    min_blobs: u64,
) -> Result<(ScanArm, GetparentsStats)> {
    let arm = super::dispatch_scan_arm(input, super::has_indexdata(input, direct_io)?, min_blobs)?;
    let stats = getparents_with_arm(
        input,
        output,
        ids,
        opts,
        compression,
        direct_io,
        overrides,
        arm,
    )?;
    Ok((arm, stats))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn getparents_with_arm(
    input: &Path,
    output: &Path,
    ids: &ElementIds,
    opts: &GetparentsOptions,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
    arm: ScanArm,
) -> Result<GetparentsStats> {
    match arm {
        ScanArm::Walker => {
            getparents_walker(input, output, ids, opts, compression, direct_io, overrides)
        }
        ScanArm::Pipelined => {
            getparents_pipelined(input, output, ids, opts, compression, direct_io, overrides)
        }
    }
}

/// Which blob kinds can contribute matches?
/// - node blobs: only for --add-self on node IDs
/// - way blobs: any way may reference a query node ID; --add-self on way IDs
/// - relation blobs: any relation may reference a query node/way/relation ID;
///   --add-self on relation IDs
fn needed_blob_kinds(ids: &ElementIds, opts: &GetparentsOptions) -> (bool, bool, bool) {
    (
        opts.add_self && ids.node_ids.has_any(),
        ids.node_ids.has_any() || (opts.add_self && ids.way_ids.has_any()),
        ids.node_ids.has_any() || ids.way_ids.has_any() || ids.relation_ids.has_any(),
    )
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn getparents_walker(
    input: &Path,
    output: &Path,
    ids: &ElementIds,
    opts: &GetparentsOptions,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<GetparentsStats> {
    let (need_node_blobs, need_way_blobs, need_relation_blobs) = needed_blob_kinds(ids, opts);

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
                        )
                        .into());
                    }
                    schedule.push((schedule.len(), meta.data_offset, meta.data_size));
                } else {
                    blobs_skipped += 1;
                }
            }
            _ => {}
        }
    }

    crate::debug::emit_counter(
        "walk_actual_osmdata_blobs",
        i64::try_from(schedule.len())
            .unwrap_or(i64::MAX)
            .saturating_add(i64::try_from(blobs_skipped).unwrap_or(i64::MAX)),
    );

    let shared_file = Arc::clone(walker.shared_file());
    drop(walker);

    let header = header_block
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))?;

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("getparents_schedule_blobs", schedule.len() as i64);
        crate::debug::emit_counter("getparents_blobs_skipped", blobs_skipped as i64);
    }
    crate::debug::emit_marker("GETPARENTS_SCHEDULE_END");

    super::warn_locations_on_ways_loss(&header);
    let header_bytes = super::build_output_header(&header, true, overrides, |hb| hb)?;
    let mut writer =
        writer_from_header_bytes(output, compression, &header_bytes, direct_io, false)?;

    let mut stats = GetparentsStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
    };

    crate::debug::emit_marker("GETPARENTS_DECODE_START");
    type ClassifyResult = std::result::Result<(Vec<OwnedBlock>, (u64, u64, u64)), String>;
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
            if write_err.is_some() {
                return;
            }
            reorder.push(seq, result);
            while let Some(item) = reorder.pop_ready() {
                match item {
                    Ok((blocks, (n, w, r))) => {
                        for (block_bytes, index, tagdata) in blocks {
                            if let Err(e) = writer.write_primitive_block_owned(
                                block_bytes,
                                index,
                                tagdata.as_deref(),
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

#[allow(clippy::too_many_arguments)]
fn getparents_pipelined(
    input: &Path,
    output: &Path,
    ids: &ElementIds,
    opts: &GetparentsOptions,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<GetparentsStats> {
    let (need_node_blobs, need_way_blobs, need_relation_blobs) = needed_blob_kinds(ids, opts);
    let reader = ElementReader::open(input, direct_io)?.with_blob_filter(BlobFilter::new(
        need_node_blobs,
        need_way_blobs,
        need_relation_blobs,
    ));
    super::warn_locations_on_ways_loss(reader.header());
    let header_bytes = super::build_output_header(reader.header(), true, overrides, |hb| hb)?;
    let mut writer =
        writer_from_header_bytes(output, compression, &header_bytes, direct_io, false)?;
    let mut stats = GetparentsStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
    };

    crate::debug::emit_marker("GETPARENTS_DECODE_START");
    for_each_primitive_block_batch(reader.into_blocks_pipelined(), BATCH_SIZE, |batch| {
        let (nodes, ways, relations) = process_batch(batch, &mut writer, ids, opts.add_self)?;
        stats.nodes_written += nodes;
        stats.ways_written += ways;
        stats.relations_written += relations;
        Ok(())
    })?;
    writer.flush()?;
    crate::debug::emit_marker("GETPARENTS_DECODE_END");
    Ok(stats)
}

/// Classify a batch of blocks in parallel via rayon. Each worker owns a
/// `BlockBuilder` through `map_init` and reuses it across the blocks it
/// takes, flushing serialized output blocks into a local `Vec`; the
/// serialized blocks are then written sequentially in batch order.
///
/// Classify cost is linear in decoded elements, so it must not run on the
/// single consumer thread: at planet scale that serializes over a billion
/// way-ref checks behind the decode pipeline and dominates wall time.
fn process_batch(
    batch: &[PrimitiveBlock],
    writer: &mut crate::writer::PbfWriter<crate::file_writer::FileWriter>,
    ids: &ElementIds,
    add_self: bool,
) -> Result<(u64, u64, u64)> {
    type BatchResult = std::result::Result<(Vec<OwnedBlock>, (u64, u64, u64)), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(BlockBuilder::new, |bb, block| {
            let mut output: Vec<OwnedBlock> = Vec::new();
            let counts = process_block(block, bb, &mut output, ids, add_self)?;
            flush_local(bb, &mut output)?;
            Ok((output, counts))
        })
        .collect();

    let mut total_nodes: u64 = 0;
    let mut total_ways: u64 = 0;
    let mut total_relations: u64 = 0;
    drain_batch_results(results, writer, |(nodes, ways, relations)| {
        total_nodes += nodes;
        total_ways += ways;
        total_relations += relations;
    })?;
    Ok((total_nodes, total_ways, total_relations))
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
                    bb.add_node(
                        dn.id(),
                        dn.decimicro_lat(),
                        dn.decimicro_lon(),
                        dn.tags(),
                        meta.as_ref(),
                    );
                    nodes += 1;
                }
            }
            Element::Node(n) => {
                if add_self && ids.node_ids.get(n.id()) {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = element_metadata(&n.info());
                    bb.add_node(
                        n.id(),
                        n.decimicro_lat(),
                        n.decimicro_lon(),
                        n.tags(),
                        meta.as_ref(),
                    );
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

#[cfg(test)]
mod tests {
    use super::{
        GetparentsOptions, PIPELINED_ARM_MIN_BLOBS, ScanArm, getparents_dispatched,
        getparents_with_arm,
    };
    use crate::block_builder::{BlockBuilder, HeaderBuilder};
    use crate::commands::getid::parse_ids;
    use crate::writer::{Compression, PbfWriter};
    use crate::{Element, ElementReader, HeaderOverrides};

    /// Write a node block, then optionally a way block, which can be written
    /// without indexdata to model a partially indexed input.
    fn write_blocks(
        path: &std::path::Path,
        node_ids: &[i64],
        way: Option<(i64, &[i64])>,
        index_way_blob: bool,
    ) {
        let file = std::fs::File::create(path).expect("create fixture");
        let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
        writer
            .write_header(&HeaderBuilder::new().sorted().build().expect("header"))
            .expect("write header");
        let mut block = BlockBuilder::new();
        for &id in node_ids {
            block.add_node(id, 0, 0, std::iter::empty::<(&str, &str)>(), None);
        }
        writer
            .write_primitive_block(block.take().expect("node block").expect("nodes"))
            .expect("write nodes");
        if let Some((way_id, refs)) = way {
            block.add_way(way_id, std::iter::empty::<(&str, &str)>(), refs, None);
            let bytes = block.take().expect("way block").expect("ways");
            if index_way_blob {
                writer.write_primitive_block(bytes).expect("write ways");
            } else {
                writer
                    .write_primitive_block_no_indexdata(bytes)
                    .expect("write ways");
            }
        }
        writer.flush().expect("flush fixture");
    }

    fn fixture(path: &std::path::Path) {
        write_blocks(path, &[1, 2], Some((10, &[1, 2])), true);
    }

    fn ids(path: &std::path::Path) -> Vec<i64> {
        let mut ids = Vec::new();
        ElementReader::from_path(path)
            .expect("read output")
            .for_each(|element| match element {
                Element::DenseNode(node) => ids.push(node.id()),
                Element::Node(node) => ids.push(node.id()),
                Element::Way(way) => ids.push(way.id()),
                Element::Relation(relation) => ids.push(relation.id()),
            })
            .expect("iterate output");
        ids.sort_unstable();
        ids
    }

    /// Run the same query under both arms, assert they emit identical element
    /// sets, and return that common (sorted) ID list.
    fn ids_from_both_arms(input: &std::path::Path, query: &[&str], add_self: bool) -> Vec<i64> {
        let dir = tempfile::tempdir().expect("tempdir");
        let query: Vec<String> = query.iter().map(|s| (*s).to_owned()).collect();
        let query = parse_ids(&query).expect("ids");
        let opts = GetparentsOptions { add_self };
        let walker = dir.path().join("walker.pbf");
        let pipelined = dir.path().join("pipelined.pbf");
        for (output, arm) in [(&walker, ScanArm::Walker), (&pipelined, ScanArm::Pipelined)] {
            getparents_with_arm(
                input,
                output,
                &query,
                &opts,
                Compression::default(),
                false,
                &HeaderOverrides::default(),
                arm,
            )
            .expect("getparents");
        }
        let walker_ids = ids(&walker);
        assert_eq!(walker_ids, ids(&pipelined));
        walker_ids
    }

    #[test]
    fn walker_and_pipelined_arms_emit_the_same_elements() {
        let dir = tempfile::tempdir().expect("tempdir");
        let input = dir.path().join("input.pbf");
        fixture(&input);
        // Plain parent query: the referencing way, not the queried node.
        assert_eq!(ids_from_both_arms(&input, &["n1"], false), vec![10]);
        // --add-self additionally emits the queried node.
        assert_eq!(ids_from_both_arms(&input, &["n1"], true), vec![1, 10]);
    }

    #[test]
    fn arms_agree_on_empty_query_result() {
        let dir = tempfile::tempdir().expect("tempdir");
        let input = dir.path().join("input.pbf");
        fixture(&input);
        assert!(ids_from_both_arms(&input, &["n999"], true).is_empty());
    }

    #[test]
    fn arms_agree_on_single_data_blob_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let input = dir.path().join("input.pbf");
        write_blocks(&input, &[1, 2], None, true);
        assert_eq!(ids_from_both_arms(&input, &["n1"], true), vec![1]);
    }

    #[test]
    fn arms_agree_when_a_blob_lacks_indexdata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let input = dir.path().join("input.pbf");
        write_blocks(&input, &[1, 2], Some((10, &[1, 2])), false);
        assert_eq!(ids_from_both_arms(&input, &["n1"], false), vec![10]);
    }

    #[test]
    fn auto_dispatch_crosses_arms_at_the_injected_threshold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let input = dir.path().join("input.pbf");
        fixture(&input);
        let query = parse_ids(&["n1".to_owned()]).expect("ids");
        let opts = GetparentsOptions { add_self: true };
        for (min_blobs, expected_arm) in [
            (1, ScanArm::Pipelined),
            (PIPELINED_ARM_MIN_BLOBS, ScanArm::Walker),
        ] {
            let output = dir.path().join(format!("out-{min_blobs}.pbf"));
            let (arm, _) = getparents_dispatched(
                &input,
                &output,
                &query,
                &opts,
                Compression::default(),
                false,
                &HeaderOverrides::default(),
                min_blobs,
            )
            .expect("getparents");
            assert_eq!(arm, expected_arm);
            assert_eq!(ids(&output), vec![1, 10]);
        }
    }
}
