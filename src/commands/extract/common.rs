//! Shared types and helpers across extract strategies.

use std::path::Path;

use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::cat::CleanAttrs;
use crate::writer::PbfWriter;
use crate::{BlobFilter, Element, MemberId, PrimitiveBlock};

use super::super::{
    Result, ensure_node_capacity_local, ensure_relation_capacity_local, ensure_way_capacity_local,
    flush_local,
};
pub(super) use crate::commands::spatial::BboxInt;
use crate::idset::IdSet;

use super::ExtractStats;

// ---------------------------------------------------------------------------
// Integer bbox + spatial filter
// ---------------------------------------------------------------------------

/// Build a [`BlobFilter`] that accepts all element types but spatially filters
/// node blobs: only node blobs whose coordinate bbox intersects the extraction
/// bbox are decompressed. Way and relation blobs always pass through.
///
/// Requires v2 indexdata with spatial bounds. Blobs without spatial indexdata
/// are conservatively passed through.
pub(super) fn spatial_blob_filter(bbox_int: &BboxInt) -> BlobFilter {
    BlobFilter::new(true, true, true).with_node_bbox(crate::BlobBbox::new(
        bbox_int.min_lat,
        bbox_int.max_lat,
        bbox_int.min_lon,
        bbox_int.max_lon,
    ))
}

// ---------------------------------------------------------------------------
// Blob schedule
// ---------------------------------------------------------------------------

/// Blob descriptor for pread schedule.
#[derive(Clone, Copy)]
pub(super) struct BlobDesc {
    /// Byte offset of the 4-byte frame length prefix (start of the entire blob frame).
    pub(super) frame_offset: u64,
    /// Total size of the blob frame (4-byte len + header + blob body).
    pub(super) frame_size: usize,
    pub(super) offset: u64,
    pub(super) size: usize,
    pub(super) kind: Option<crate::blob_meta::ElemKind>,
    /// Spatial bbox from indexdata (node blobs only, v2 format).
    pub(super) bbox: Option<crate::BlobBbox>,
    /// Element count from indexdata (for stats on raw passthrough blobs).
    pub(super) count: u64,
    /// True if this blob can be passed through raw (no decode/re-encode).
    /// Set by the schedule builder based on blob bbox containment.
    pub(super) raw_passthrough: bool,
}

/// Build a blob schedule from a header-only scan.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn build_blob_schedule(input: &Path) -> Result<Vec<BlobDesc>> {
    build_blob_schedule_with_passthrough(input, None)
}

/// Build a blob schedule, optionally tagging node blobs eligible for raw passthrough.
/// A node blob is eligible if its bbox is fully contained in the extract bbox.
///
/// Walks via the pread-only `HeaderWalker` so blob bodies stay out of the
/// page cache during the scan - extract's later `pread_execute` pass opens
/// a fresh fd and reads only the blobs it actually needs.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn build_blob_schedule_with_passthrough(
    input: &Path,
    extract_bbox: Option<&crate::BlobBbox>,
) -> Result<Vec<BlobDesc>> {
    crate::debug::emit_marker("EXTRACT_SCHEDULE_SCAN_START");
    let mut walker = crate::read::header_walker::HeaderWalker::open(input)?;
    let _ = walker
        .next_header()?
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))?;

    let mut schedule = Vec::new();
    let mut passthrough_node_blobs: u64 = 0;
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        let idx = meta.index.as_ref();
        let kind = idx.map(|i| i.kind);

        // Tag node blobs for raw passthrough if fully contained in extract bbox.
        let raw_passthrough = extract_bbox.is_some_and(|ebbox| {
            idx.is_some_and(|i| {
                matches!(i.kind, crate::blob_meta::ElemKind::Node)
                    && i.bbox.as_ref().is_some_and(|bb| ebbox.contains(bb))
            })
        });
        if raw_passthrough {
            passthrough_node_blobs += 1;
        }

        let bbox = idx.and_then(|i| i.bbox);
        let count = idx.map_or(0, |i| i.count);

        schedule.push(BlobDesc {
            frame_offset: meta.frame_start,
            frame_size: meta.frame_size,
            offset: meta.data_offset,
            size: meta.data_size,
            kind,
            bbox,
            count,
            raw_passthrough,
        });
    }
    crate::debug::emit_marker("EXTRACT_SCHEDULE_SCAN_END");

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("extract_schedule_blobs", schedule.len() as i64);
        crate::debug::emit_counter(
            "extract_schedule_passthrough_node_blobs",
            passthrough_node_blobs as i64,
        );
    }
    Ok(schedule)
}

// ---------------------------------------------------------------------------
// pread execute + write passes
// ---------------------------------------------------------------------------

pub(super) fn merge_extract_stats(target: &mut ExtractStats, source: &ExtractStats) {
    target.nodes_in_bbox += source.nodes_in_bbox;
    target.nodes_from_ways += source.nodes_from_ways;
    target.nodes_from_relations += source.nodes_from_relations;
    target.ways_written += source.ways_written;
    target.ways_from_relations += source.ways_from_relations;
    target.relations_written += source.relations_written;
}

/// Execute a pread-from-workers write pass on a pre-built schedule.
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn pread_execute<F>(
    input: &Path,
    schedule: &[BlobDesc],
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut ExtractStats,
    block_fn: F,
) -> Result<()>
where
    F: Fn(
            &PrimitiveBlock,
            &mut BlockBuilder,
            &mut Vec<OwnedBlock>,
        ) -> std::result::Result<ExtractStats, String>
        + Send
        + Sync,
{
    use std::os::unix::fs::FileExt as _;

    if schedule.is_empty() {
        return Ok(());
    }

    // Shared file for pread. Uses buffered (non-O_DIRECT) I/O because O_DIRECT
    // requires aligned buffers for pread, which we don't have - our read buffers
    // are plain Vec<u8> without alignment guarantees.
    //
    // wontfix(fd-per-call): simple extract calls `pread_execute` three times
    // (nodes, ways, relations) and each call opens a fresh fd. The cost is
    // 1-5 us per open, 3-15 us total per run - invisible against the ~tens of
    // seconds of pread+decode work. Hoisting to the caller is possible but
    // buys nothing measurable; the local-scope Arc keeps the API simple.
    // See TODO.md history 2026-04-19 for the investigation.
    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?,
    );

    let decode_threads = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4);

    type WorkerResult = (usize, crate::error::Result<(Vec<OwnedBlock>, ExtractStats)>);

    // Split schedule: decode blobs go to workers, passthrough blobs handled by consumer.
    // Both are re-sequenced for the reorder buffer.
    let mut decode_items: Vec<(usize, u64, usize)> = Vec::new(); // (global_seq, data_offset, data_size)
    let mut passthrough_items: Vec<(usize, u64, usize, u64)> = Vec::new(); // (global_seq, frame_offset, frame_size, count)
    for (i, d) in schedule.iter().enumerate() {
        if d.raw_passthrough {
            passthrough_items.push((i, d.frame_offset, d.frame_size, d.count));
        } else {
            decode_items.push((i, d.offset, d.size));
        }
    }

    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<(usize, u64, usize)>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<WorkerResult>(32);

    std::thread::scope(|scope| -> Result<()> {
        // Dispatcher: feed decode-only blobs to workers.
        // Passthrough blobs are handled directly by the consumer.
        scope.spawn(move || {
            for item in decode_items {
                if desc_tx.send(item).is_err() {
                    break;
                }
            }
        });

        // Workers: pread → decompress → PrimitiveBlock → extract → OwnedBlocks.
        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let tx = result_tx.clone();
            let file = std::sync::Arc::clone(&shared_file);
            let block_fn_ref = &block_fn;
            scope.spawn(move || {
                let mut read_buf: Vec<u8> = Vec::new();
                let mut bb = BlockBuilder::new();
                let mut output_blocks: Vec<OwnedBlock> = Vec::new();
                let worker_pool = crate::blob::DecompressPool::new();
                let mut st_scratch: Vec<(u32, u32)> = Vec::new();
                let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

                loop {
                    let (s, data_offset, data_size) = {
                        let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match guard.recv() {
                            Ok(d) => d,
                            Err(_) => break,
                        }
                    };

                    let r: crate::error::Result<(Vec<OwnedBlock>, ExtractStats)> = (|| {
                        read_buf.resize(data_size, 0);
                        file.read_exact_at(&mut read_buf, data_offset)
                            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;

                        // Decode path: full PrimitiveBlock → extract → OwnedBlocks.
                        let mut buf = crate::blob::pool_get_pub(&worker_pool, data_size * 4);
                        crate::blob::decompress_blob_raw(&read_buf, &mut buf)?;
                        let block = PrimitiveBlock::from_vec_pooled_with_scratch(
                            buf,
                            &worker_pool,
                            &mut st_scratch,
                            &mut gr_scratch,
                        )?;
                        output_blocks.clear();
                        let block_stats = block_fn_ref(&block, &mut bb, &mut output_blocks)
                            .map_err(|e| {
                                crate::error::new_error(crate::error::ErrorKind::Io(
                                    std::io::Error::other(e),
                                ))
                            })?;
                        flush_local(&mut bb, &mut output_blocks).map_err(|e| {
                            crate::error::new_error(crate::error::ErrorKind::Io(
                                std::io::Error::other(e),
                            ))
                        })?;
                        Ok((std::mem::take(&mut output_blocks), block_stats))
                    })(
                    );
                    if tx.send((s, r)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(desc_rx);
        drop(result_tx);

        // Consumer: merge two streams - worker OwnedBlocks + passthrough raw frames.
        // Both use the reorder buffer keyed by global sequence number.
        // Passthrough blobs: consumer reads raw frame directly, writes via write_raw_owned.
        // Worker blobs: consumer receives OwnedBlocks, writes via write_primitive_block_owned.

        enum ConsumerItem {
            Decoded(crate::error::Result<(Vec<OwnedBlock>, ExtractStats)>),
            Passthrough(u64, usize, u64), // (frame_offset, frame_size, element_count)
        }

        let _total_blobs = schedule.len();
        let mut reorder: crate::reorder_buffer::ReorderBuffer<ConsumerItem> =
            crate::reorder_buffer::ReorderBuffer::with_capacity(32);

        // Pre-insert passthrough items into the reorder buffer.
        for &(seq, frame_offset, frame_size, count) in &passthrough_items {
            reorder.push(
                seq,
                ConsumerItem::Passthrough(frame_offset, frame_size, count),
            );
        }

        // Drain worker results into the reorder buffer.
        let mut frame_read_buf: Vec<u8> = Vec::new();
        for (s, item) in result_rx {
            reorder.push(s, ConsumerItem::Decoded(item));

            while let Some(ci) = reorder.pop_ready() {
                match ci {
                    ConsumerItem::Decoded(r) => {
                        let (blocks, block_stats) = r?;
                        merge_extract_stats(stats, &block_stats);
                        for OwnedBlock {
                            bytes: block_bytes,
                            index,
                            tagdata,
                            way_members,
                        } in blocks
                        {
                            writer.write_primitive_block_owned(
                                block_bytes,
                                index,
                                tagdata.as_deref(),
                                way_members.as_deref(),
                            )?;
                        }
                    }
                    ConsumerItem::Passthrough(frame_offset, frame_size, count) => {
                        // Read raw frame directly and write without decode/re-encode.
                        frame_read_buf.resize(frame_size, 0);
                        shared_file
                            .read_exact_at(&mut frame_read_buf, frame_offset)
                            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                        writer.write_raw_owned(std::mem::take(&mut frame_read_buf))?;
                        stats.nodes_in_bbox += count;
                    }
                }
            }
        }

        // Drain any remaining passthrough items after workers are done.
        while let Some(ci) = reorder.pop_ready() {
            match ci {
                ConsumerItem::Decoded(r) => {
                    let (blocks, block_stats) = r?;
                    merge_extract_stats(stats, &block_stats);
                    for OwnedBlock {
                        bytes: block_bytes,
                        index,
                        tagdata,
                        way_members,
                    } in blocks
                    {
                        writer.write_primitive_block_owned(
                            block_bytes,
                            index,
                            tagdata.as_deref(),
                            way_members.as_deref(),
                        )?;
                    }
                }
                ConsumerItem::Passthrough(frame_offset, frame_size, count) => {
                    frame_read_buf.resize(frame_size, 0);
                    shared_file
                        .read_exact_at(&mut frame_read_buf, frame_offset)
                        .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                    writer.write_raw_owned(std::mem::take(&mut frame_read_buf))?;
                    stats.nodes_in_bbox += count;
                }
            }
        }
        Ok(())
    })?;

    Ok(())
}

/// Convenience: build schedule + execute + flush. Used by complete/smart write passes.
/// Flushes the writer after execution (assumes single-use - don't call on a writer
/// that will be reused for subsequent phases).
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn pread_write_pass<F>(
    input: &Path,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut ExtractStats,
    block_fn: F,
) -> Result<()>
where
    F: Fn(
            &PrimitiveBlock,
            &mut BlockBuilder,
            &mut Vec<OwnedBlock>,
        ) -> std::result::Result<ExtractStats, String>
        + Send
        + Sync,
{
    crate::debug::emit_mallinfo2("MI_PRE_BLOB_SCHEDULE");
    crate::debug::emit_marker("PREAD_WRITE_BLOB_SCHEDULE_START");
    let schedule = build_blob_schedule(input)?;
    crate::debug::emit_marker("PREAD_WRITE_BLOB_SCHEDULE_END");
    crate::debug::emit_mallinfo2("MI_POST_BLOB_SCHEDULE");
    pread_write_pass_with_schedule(input, &schedule, writer, stats, block_fn)
}

/// Variant of [`pread_write_pass`] that takes a pre-built blob schedule
/// instead of calling [`build_blob_schedule`]. Used by smart/complete extract
/// to reuse the schedule built during PASS1's manual header scan, avoiding
/// a third post-PASS1 file scan and its associated cold-arena-page residency
/// cascade (commits `d4ea760`, `0b085b1`, 2026-04-10/11).
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn pread_write_pass_with_schedule<F>(
    input: &Path,
    schedule: &[BlobDesc],
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut ExtractStats,
    block_fn: F,
) -> Result<()>
where
    F: Fn(
            &PrimitiveBlock,
            &mut BlockBuilder,
            &mut Vec<OwnedBlock>,
        ) -> std::result::Result<ExtractStats, String>
        + Send
        + Sync,
{
    crate::debug::emit_marker("PREAD_WRITE_EXECUTE_START");
    pread_execute(input, schedule, writer, stats, block_fn)?;
    crate::debug::emit_marker("PREAD_WRITE_EXECUTE_END");
    crate::debug::emit_mallinfo2("MI_POST_EXECUTE");
    crate::debug::emit_marker("PREAD_WRITE_FLUSH_START");
    writer.flush()?;
    crate::debug::emit_marker("PREAD_WRITE_FLUSH_END");
    Ok(())
}

// ---------------------------------------------------------------------------
// Pass 2 block processing (shared by simple + complete)
// ---------------------------------------------------------------------------

/// Read-only ID sets for Pass 2 of complete-ways strategy, shared across rayon threads.
pub(super) struct ExtractPass2IdSets<'a> {
    pub(super) bbox_node_ids: &'a IdSet,
    pub(super) all_way_node_ids: &'a IdSet,
    pub(super) matched_way_ids: &'a IdSet,
    pub(super) matched_relation_ids: &'a IdSet,
}

use super::super::clean_metadata;
use crate::owned::{dense_node_metadata, element_metadata};

/// Process a single block for Pass 2 of complete-ways: write elements whose IDs
/// were collected in Pass 1. Uses thread-local BlockBuilder and output buffer.
///
/// `phase_kind` scopes which element kinds this call may emit. `None` emits
/// every matching element in the block (single-pass write: complete, smart,
/// simple-unsorted-fallback). `Some(kind)` emits only that kind, needed by
/// simple's 3-phase sorted single-pass write where the same non-indexed blob
/// is fed to every phase: without the filter, the monotonically-growing id
/// sets would cause nodes to be emitted 3x, ways 2x, relations 1x from one
/// non-indexed blob. For indexed PBFs the filter is a no-op (each blob is
/// homogeneous by type, pre-routed to its phase).
#[hotpath::measure]
pub(super) fn extract_block_pass2(
    block: &PrimitiveBlock,
    ids: &ExtractPass2IdSets<'_>,
    clean: &CleanAttrs,
    phase_kind: Option<crate::blob_meta::ElemKind>,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> std::result::Result<ExtractStats, String> {
    let mut stats = ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_written: 0,
        ways_from_relations: 0,
        relations_written: 0,
        strategy: "",
    };
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    let emit_nodes = phase_kind.is_none_or(|k| matches!(k, crate::blob_meta::ElemKind::Node));
    let emit_ways = phase_kind.is_none_or(|k| matches!(k, crate::blob_meta::ElemKind::Way));
    let emit_rels = phase_kind.is_none_or(|k| matches!(k, crate::blob_meta::ElemKind::Relation));

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) if emit_nodes => {
                let in_bbox = ids.bbox_node_ids.get(dn.id());
                let from_way = ids.all_way_node_ids.get(dn.id());
                if in_bbox || from_way {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = clean_metadata(dense_node_metadata(dn), clean);
                    bb.add_node(
                        dn.id(),
                        dn.decimicro_lat(),
                        dn.decimicro_lon(),
                        dn.tags(),
                        meta.as_ref(),
                    );
                    if in_bbox {
                        stats.nodes_in_bbox += 1;
                    } else {
                        stats.nodes_from_ways += 1;
                    }
                }
            }
            Element::Node(n) if emit_nodes => {
                let in_bbox = ids.bbox_node_ids.get(n.id());
                let from_way = ids.all_way_node_ids.get(n.id());
                if in_bbox || from_way {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = clean_metadata(element_metadata(&n.info()), clean);
                    bb.add_node(
                        n.id(),
                        n.decimicro_lat(),
                        n.decimicro_lon(),
                        n.tags(),
                        meta.as_ref(),
                    );
                    if in_bbox {
                        stats.nodes_in_bbox += 1;
                    } else {
                        stats.nodes_from_ways += 1;
                    }
                }
            }
            Element::Way(w) if emit_ways && ids.matched_way_ids.get(w.id()) => {
                ensure_way_capacity_local(bb, output)?;
                refs_buf.clear();
                refs_buf.extend(w.refs());
                let meta = clean_metadata(element_metadata(&w.info()), clean);
                bb.add_way(w.id(), w.tags(), &refs_buf, meta.as_ref());
                stats.ways_written += 1;
            }
            Element::Relation(r) if emit_rels && ids.matched_relation_ids.get(r.id()) => {
                ensure_relation_capacity_local(bb, output)?;
                members_buf.clear();
                members_buf.extend(r.members().map(|m| MemberData {
                    id: m.id,
                    role: m.role().unwrap_or(""),
                }));
                let meta = clean_metadata(element_metadata(&r.info()), clean);
                bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
                stats.relations_written += 1;
            }
            _ => {}
        }
    }
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Relation member matching
// ---------------------------------------------------------------------------

/// Check if a relation has any member whose ID is in the matched node or way sets.
pub(super) fn relation_has_matched_member(
    r: &crate::Relation,
    node_ids: &IdSet,
    way_ids: &IdSet,
) -> bool {
    r.members().any(|m| match m.id {
        MemberId::Node(id) => node_ids.get(id),
        MemberId::Way(id) => way_ids.get(id),
        MemberId::Relation(_) | MemberId::Unknown(_, _) => false,
    })
}
