//! Shared types and helpers across extract strategies.

use std::path::Path;

use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::cat::CleanAttrs;
use crate::read::wire::{Cursor, WIRE_LEN};
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

/// Whether a node blob may be copied to the output as a raw frame.
///
/// Raw passthrough skips the decode/re-encode of a blob whose every node is
/// inside the region. The schedule builder can only nominate a blob, from
/// its indexdata bbox - an unverified claim (`check --index` exists to check
/// it). Nothing may be copied or counted on that claim alone: a lying bbox
/// used to put out-of-region nodes in the output, and a lying `count` used
/// to land in the "N in bbox" headline as if it had been observed.
///
/// So the classify phase, which decodes every node blob anyway, upgrades a
/// [`Candidate`](Passthrough::Candidate) to [`Verified`](Passthrough::Verified)
/// only after [`dense_only_payload`] has proved the payload holds nothing
/// but DenseNodes and every one of them landed in the region. The count
/// carried by `Verified` is the number of nodes that classify decoded, so
/// the stats stay observation. Only `Verified` is passed through; a
/// `Candidate` that was never verified is decoded like any other blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Passthrough {
    /// Decode and filter.
    No,
    /// The indexdata bbox claims containment; not yet checked.
    Candidate,
    /// Checked against the decoded payload: copy raw, count `nodes`.
    Verified { nodes: u64 },
}

/// Blob descriptor for pread schedule.
#[derive(Clone, Copy)]
pub(super) struct BlobDesc {
    /// Byte offset of the 4-byte frame length prefix (start of the entire blob frame).
    pub(super) frame_offset: u64,
    /// Total size of the blob frame (4-byte len + header + blob body).
    pub(super) frame_size: usize,
    pub(super) offset: u64,
    pub(super) size: usize,
    pub(super) kind: Option<crate::ElementKind>,
    /// Spatial bbox from indexdata (node blobs only, v2 format).
    pub(super) bbox: Option<crate::BlobBbox>,
    /// Raw-passthrough state. See [`Passthrough`].
    pub(super) passthrough: Passthrough,
}

/// Decide whether a passthrough candidate's decompressed payload is
/// structurally eligible to be copied raw.
///
/// Returns `Ok(true)` only when every `PrimitiveGroup` holds nothing but at
/// most one DenseNodes message, and that message at most one DenseInfo -
/// the shape `BlockBuilder` writes. Under that shape the dense column decoder
/// (`decode_dense_columns`) sees every element the raw copy would carry, so
/// comparing the in-region count to the column length proves the whole blob
/// is in the region. Anything else makes the blob ineligible (`Ok(false)`),
/// not an error: plain `Node` messages (classified beside the dense columns,
/// so the column length that verification compares against cannot vouch for
/// them), ways or relations in a blob whose indexdata says nodes,
/// changesets and unknown group fields (which no element iterator reports),
/// and a repeated DenseNodes or DenseInfo
/// (the decoder refuses a repeated DenseNodes and keeps only the last
/// DenseInfo, while the raw bytes would carry both).
///
/// This is a structural walk only. Callers must also construct the payload
/// as a [`PrimitiveBlock`] - what the decode path would do with it - and
/// propagate a construction error, so a blob the decoder rejects cannot slip
/// into the output through a raw copy. That is constructor validation only;
/// decode-time arithmetic on metadata (timestamp deltas and the like) is not
/// exercised by either.
///
/// A framing error in the walk itself is returned as an error, distinct
/// from ineligibility.
pub(super) fn dense_only_payload(decompressed: &[u8]) -> crate::error::Result<bool> {
    let mut cursor = Cursor::new(decompressed);
    while let Some((field, wire_type)) = cursor.read_tag()? {
        if field == 2 && wire_type == WIRE_LEN {
            if !dense_only_group(cursor.read_len_delimited()?)? {
                return Ok(false);
            }
        } else {
            // Block-level fields: stringtable, granularity, offsets.
            cursor.skip_field(wire_type)?;
        }
    }
    Ok(true)
}

/// One `PrimitiveGroup` of [`dense_only_payload`]: zero or one DenseNodes
/// (field 2) and nothing else.
fn dense_only_group(group: &[u8]) -> crate::error::Result<bool> {
    let mut cursor = Cursor::new(group);
    let mut dense_seen = false;
    while let Some((field, wire_type)) = cursor.read_tag()? {
        if field != 2 || wire_type != WIRE_LEN || dense_seen {
            return Ok(false);
        }
        dense_seen = true;
        if !single_dense_info(cursor.read_len_delimited()?)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// A DenseNodes message carries at most one DenseInfo (field 5).
/// `WireDenseNodes::parse` keeps only the last occurrence, while a raw copy
/// carries all of them and a merging protobuf reader would combine them.
fn single_dense_info(dense: &[u8]) -> crate::error::Result<bool> {
    let mut cursor = Cursor::new(dense);
    let mut info_seen = false;
    while let Some((field, wire_type)) = cursor.read_tag()? {
        if field == 5 {
            if info_seen {
                return Ok(false);
            }
            info_seen = true;
        }
        cursor.skip_field(wire_type)?;
    }
    Ok(true)
}

/// Report the outcome of passthrough verification: counters always, and a
/// warning when a candidate failed: either its indexdata bbox claimed a
/// containment its own payload contradicts, or the payload has a shape no
/// raw copy can be proved safe for (see [`dense_only_payload`]).
pub(super) fn warn_passthrough_demotions(verified: u64, demoted: u64, host: &crate::host::Host) {
    crate::debug::emit_counter("extract_passthrough_verified_blobs", verified);
    crate::debug::emit_counter("extract_passthrough_demoted_blobs", demoted);
    if demoted > 0 {
        crate::host::diag!(
            host.diagnostics,
            Warning,
            "extract.passthrough_demoted",
            "{demoted} node blob(s) whose indexdata bbox lies inside the region \
             hold out-of-region nodes or content other than DenseNodes, so they were \
             filtered instead of copied raw. The input's indexdata may be wrong."
        );
    }
}

/// Build a blob schedule from a header-only scan.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn build_blob_schedule(input: &Path, host: &crate::host::Host) -> Result<Vec<BlobDesc>> {
    build_blob_schedule_with_passthrough(input, None, host)
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
    host: &crate::host::Host,
) -> Result<Vec<BlobDesc>> {
    crate::debug::emit_marker("EXTRACT_SCHEDULE_SCAN_START");
    let mut walker =
        crate::read::header_walker::HeaderWalker::open(input, host)?.refusing_partial_index();
    let _ = walker
        .next_header()?
        .ok_or_else(|| crate::error::new_missing_header(input))?;

    let mut schedule = Vec::new();
    let mut passthrough_candidates: u64 = 0;
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        walker.observe_filter_index(meta.index.as_ref());
        let idx = meta.index.as_ref();
        let kind = idx.map(|i| i.kind);

        // Nominate node blobs whose indexdata bbox claims containment in the
        // extract bbox. Only a nomination: see `Passthrough`.
        let candidate = extract_bbox.is_some_and(|ebbox| {
            idx.is_some_and(|i| {
                matches!(i.kind, crate::ElementKind::Node)
                    && i.bbox.as_ref().is_some_and(|bb| ebbox.contains(bb))
            })
        });
        if candidate {
            passthrough_candidates += 1;
        }

        schedule.push(BlobDesc {
            frame_offset: meta.frame_start,
            frame_size: meta.frame_size,
            offset: meta.data_offset,
            size: meta.data_size,
            kind,
            bbox: idx.and_then(|i| i.bbox),
            passthrough: if candidate {
                Passthrough::Candidate
            } else {
                Passthrough::No
            },
        });
    }
    crate::debug::emit_marker("EXTRACT_SCHEDULE_SCAN_END");

    crate::debug::emit_counter("extract_schedule_blobs", schedule.len());
    crate::debug::emit_counter(
        "extract_passthrough_candidate_blobs",
        passthrough_candidates,
    );
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
    host: &crate::host::Host,
) -> Result<()>
where
    F: Fn(&PrimitiveBlock, &mut BlockBuilder, &mut Vec<OwnedBlock>) -> crate::Result<ExtractStats>
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
    let shared_file = std::sync::Arc::new(crate::read::input::open(input)?);
    // The input's identity, captured once; each worker borrows it to locate a
    // failure with the frame offset the schedule handed it.
    let source: std::sync::Arc<Path> = std::sync::Arc::from(input);
    let source = &source;

    let decode_threads = crate::host::sized_workers(
        &*host.resources,
        crate::tunables::DECODE_WORKERS_RESERVED,
        crate::tunables::DECODE_WORKERS_MIN,
        crate::tunables::DECODE_WORKERS_FALLBACK,
    );

    type WorkerResult = (usize, crate::error::Result<(Vec<OwnedBlock>, ExtractStats)>);

    // Split schedule: decode blobs go to workers, passthrough blobs handled by consumer.
    // Both are re-sequenced for the reorder buffer.
    // (global_seq, data_offset, data_size, frame_offset)
    let mut decode_items: Vec<(usize, u64, usize, u64)> = Vec::new();
    // (global_seq, frame_offset, frame_size, observed node count)
    let mut passthrough_items: Vec<(usize, u64, usize, u64)> = Vec::new();
    for (i, d) in schedule.iter().enumerate() {
        // Only a verified blob is copied raw. An unverified `Candidate` is
        // decoded: its indexdata claim alone is not grounds to skip the
        // filter.
        if let Passthrough::Verified { nodes } = d.passthrough {
            passthrough_items.push((i, d.frame_offset, d.frame_size, nodes));
        } else {
            decode_items.push((i, d.offset, d.size, d.frame_offset));
        }
    }

    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<(usize, u64, usize, u64)>(
        crate::tunables::POOL_DESCRIPTOR_CHANNEL_DEPTH,
    );
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (result_tx, result_rx) =
        std::sync::mpsc::sync_channel::<WorkerResult>(crate::tunables::POOL_RESULT_CHANNEL_DEPTH);

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
                    let (s, data_offset, data_size, frame_offset) = {
                        let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match guard.recv() {
                            Ok(d) => d,
                            Err(_) => break,
                        }
                    };

                    let r: crate::error::Result<(Vec<OwnedBlock>, ExtractStats)> =
                        (|| -> crate::error::Result<(Vec<OwnedBlock>, ExtractStats)> {
                            read_buf.resize(data_size, 0);
                            file.read_exact_at(&mut read_buf, data_offset)?;

                            // Decode path: full PrimitiveBlock, extract, OwnedBlocks.
                            let mut buf = crate::blob::pool_get(
                                Some(&worker_pool),
                                data_size * crate::tunables::DECOMPRESS_EXPANSION_GUESS,
                            );
                            crate::blob::decompress_blob_raw(&read_buf, &mut buf)?;
                            let block = PrimitiveBlock::from_vec_pooled_with_scratch(
                                buf,
                                &worker_pool,
                                &mut st_scratch,
                                &mut gr_scratch,
                            )?;
                            output_blocks.clear();
                            let block_stats = block_fn_ref(&block, &mut bb, &mut output_blocks)?;
                            flush_local(&mut bb, &mut output_blocks)?;
                            Ok((std::mem::take(&mut output_blocks), block_stats))
                        })()
                        // Cold: only a failure builds the location.
                        .map_err(|e| {
                            e.locate(crate::error::Location::new(
                                Some(source),
                                None,
                                Some(crate::ByteOffset(frame_offset)),
                            ))
                        });
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
            // (frame_offset, frame_size, node count observed by classify)
            Passthrough(u64, usize, u64),
        }

        let mut reorder: crate::reorder_buffer::ReorderBuffer<ConsumerItem> =
            crate::reorder_buffer::ReorderBuffer::with_capacity(
                crate::tunables::POOL_REORDER_CAPACITY,
            );

        // Pre-insert passthrough items into the reorder buffer.
        for &(seq, frame_offset, frame_size, count) in &passthrough_items {
            reorder.push(
                seq,
                ConsumerItem::Passthrough(frame_offset, frame_size, count),
            );
        }

        // Drain worker results into the reorder buffer.
        let mut frame_read_buf: Vec<u8> = Vec::new();
        // Cancellation is checked per result on this consumer; an early
        // return drops the receiver (moved into this loop), which fails the
        // workers' sends and then the dispatcher's.
        for (s, item) in result_rx {
            host.cancel.check()?;
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
                        shared_file.read_exact_at(&mut frame_read_buf, frame_offset)?;
                        writer.write_raw_owned(std::mem::take(&mut frame_read_buf))?;
                        stats.nodes_in_bbox += count;
                    }
                }
            }
        }

        // Drain any remaining passthrough items after workers are done.
        host.cancel.check()?;
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
                    shared_file.read_exact_at(&mut frame_read_buf, frame_offset)?;
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
    host: &crate::host::Host,
) -> Result<()>
where
    F: Fn(&PrimitiveBlock, &mut BlockBuilder, &mut Vec<OwnedBlock>) -> crate::Result<ExtractStats>
        + Send
        + Sync,
{
    // Brokkr stores named counters generically and reports changes in sidecar
    // comparisons; these samples are inert unless the CLI initialized the FIFO.
    crate::debug::emit_mallinfo2("MI_PRE_BLOB_SCHEDULE");
    crate::debug::emit_marker("PREAD_WRITE_BLOB_SCHEDULE_START");
    let schedule = build_blob_schedule(input, host)?;
    crate::debug::emit_marker("PREAD_WRITE_BLOB_SCHEDULE_END");
    // Brokkr stores named counters generically and reports changes in sidecar
    // comparisons; this sample is inert unless the CLI initialized the FIFO.
    crate::debug::emit_mallinfo2("MI_POST_BLOB_SCHEDULE");
    pread_write_pass_with_schedule(input, &schedule, writer, stats, block_fn, host)
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
    host: &crate::host::Host,
) -> Result<()>
where
    F: Fn(&PrimitiveBlock, &mut BlockBuilder, &mut Vec<OwnedBlock>) -> crate::Result<ExtractStats>
        + Send
        + Sync,
{
    crate::debug::emit_marker("PREAD_WRITE_EXECUTE_START");
    pread_execute(input, schedule, writer, stats, block_fn, host)?;
    crate::debug::emit_marker("PREAD_WRITE_EXECUTE_END");
    // Brokkr stores named counters generically and reports changes in sidecar
    // comparisons; this sample is inert unless the CLI initialized the FIFO.
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
    phase_kind: Option<crate::ElementKind>,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> crate::Result<ExtractStats> {
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

    let emit_nodes = phase_kind.is_none_or(|k| matches!(k, crate::ElementKind::Node));
    let emit_ways = phase_kind.is_none_or(|k| matches!(k, crate::ElementKind::Way));
    let emit_rels = phase_kind.is_none_or(|k| matches!(k, crate::ElementKind::Relation));

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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn node_block() -> Vec<u8> {
        let mut bb = BlockBuilder::new();
        for id in 1..=3 {
            bb.add_node(id, 556_500_000, 125_000_000, std::iter::empty(), None);
        }
        bb.take().unwrap().unwrap().to_vec()
    }

    fn way_block() -> Vec<u8> {
        let mut bb = BlockBuilder::new();
        bb.add_way(10, std::iter::empty(), &[1, 2], None);
        bb.take().unwrap().unwrap().to_vec()
    }

    /// The first field-2 group's content, and the first DenseNodes message
    /// inside it (empty if the group has none).
    fn first_group_and_dense(block: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut cursor = Cursor::new(block);
        while let Some((field, wire_type)) = cursor.read_tag().unwrap() {
            if field == 2 && wire_type == WIRE_LEN {
                let group = cursor.read_len_delimited().unwrap();
                let mut g = Cursor::new(group);
                let mut dense = Vec::new();
                while let Some((gf, gw)) = g.read_tag().unwrap() {
                    if gf == 2 && gw == WIRE_LEN {
                        dense = g.read_len_delimited().unwrap().to_vec();
                        break;
                    }
                    g.skip_field(gw).unwrap();
                }
                return (group.to_vec(), dense);
            }
            cursor.skip_field(wire_type).unwrap();
        }
        panic!("block has no group");
    }

    /// Append one more `PrimitiveGroup` with the given content.
    fn with_extra_group(block: &[u8], group: &[u8]) -> Vec<u8> {
        let mut out = block.to_vec();
        protohoggr::encode_bytes_field_always(&mut out, 2, group);
        out
    }

    #[test]
    fn a_builder_node_block_is_eligible() {
        let block = node_block();
        assert!(dense_only_payload(&block).unwrap());
        PrimitiveBlock::from_vec(block).unwrap();
    }

    /// Ways in a blob whose indexdata says nodes are invisible to the dense
    /// scanners, so the in-region count cannot cover them.
    #[test]
    fn a_way_group_makes_the_blob_ineligible() {
        let (way_group, _) = first_group_and_dense(&way_block());
        let mixed = with_extra_group(&node_block(), &way_group);
        assert!(!dense_only_payload(&mixed).unwrap());
    }

    /// The decoder refuses a repeated DenseNodes in one group; a raw copy
    /// would carry the second one unchecked.
    #[test]
    fn a_second_dense_message_in_one_group_makes_the_blob_ineligible() {
        let (_, dense) = first_group_and_dense(&node_block());
        let mut group = Vec::new();
        protohoggr::encode_bytes_field_always(&mut group, 2, &dense);
        protohoggr::encode_bytes_field_always(&mut group, 2, &dense);
        assert!(!dense_only_payload(&with_extra_group(&node_block(), &group)).unwrap());

        // One per group, in several groups, is the normal shape.
        let mut single = Vec::new();
        protohoggr::encode_bytes_field_always(&mut single, 2, &dense);
        assert!(dense_only_payload(&with_extra_group(&node_block(), &single)).unwrap());
    }

    #[test]
    fn a_repeated_dense_info_makes_the_blob_ineligible() {
        let (_, dense) = first_group_and_dense(&node_block());
        let mut once = dense.clone();
        protohoggr::encode_bytes_field_always(&mut once, 5, &[]);
        let mut twice = once.clone();
        protohoggr::encode_bytes_field_always(&mut twice, 5, &[]);
        for (dense, eligible) in [(once, true), (twice, false)] {
            let mut group = Vec::new();
            protohoggr::encode_bytes_field_always(&mut group, 2, &dense);
            assert_eq!(
                dense_only_payload(&with_extra_group(&node_block(), &group)).unwrap(),
                eligible
            );
        }
    }

    /// Plain `Node` messages and changeset groups are not dense nodes.
    #[test]
    fn non_dense_group_fields_make_the_blob_ineligible() {
        for field in [1_u32, 5, 9] {
            let mut group = Vec::new();
            protohoggr::encode_bytes_field_always(&mut group, field, &[]);
            assert!(
                !dense_only_payload(&with_extra_group(&node_block(), &group)).unwrap(),
                "group field {field}"
            );
        }
        // An empty group carries nothing and stays eligible.
        assert!(dense_only_payload(&with_extra_group(&node_block(), &[])).unwrap());
    }

    #[test]
    fn broken_framing_is_an_error_not_ineligibility() {
        let mut block = node_block();
        block.extend_from_slice(&[0x12, 0x7f]); // a group claiming 127 bytes
        assert!(dense_only_payload(&block).is_err());
    }
}
