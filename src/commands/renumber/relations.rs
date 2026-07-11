//! Relation rewrite pipeline.
//!
//! - **R1**: sequential relation scan to collect all relation IDs into
//!   an `IdSet` bitset + rank index.
//! - **R2d**: parallel wire-format splice rewriter for relations.
//!   Resolves node/way/relation member refs inline via `resolve()`.

use super::super::Result;
use super::super::renumber::RenumberStats;
use super::StageCounters;
use super::schedule::BlobTask;
use super::wire_rewrite::reframe_relations_with_new_ids;
use crate::idset::IdSet;

/// Reject negative ids at the entry of the external path.
///
/// Production OSM planet extracts don't contain negative ids (they're
/// JOSM-local editor staging identifiers resolved before upload).
/// Renumber rejects them with a clear error.
fn reject_negative_id(id: i64, kind: &str) -> Result<()> {
    if id < 0 {
        return Err(format!(
            "renumber requires non-negative input ids. \
             Input contains {kind} id {id}. \
             Negative ids are JOSM editor-local staging identifiers \
             that should be resolved before processing."
        )
        .into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// R1: collect relation IDs
// ---------------------------------------------------------------------------

/// R1 pass: collect relation IDs into an `IdSet` bitset.
/// New IDs are derived via `start_relation_id + rank(old_id)` -
/// no explicit mapping needed.
#[hotpath::measure]
pub(super) fn relation_r1_collect_ids(
    shared_file: &std::fs::File,
    relation_schedule: &[BlobTask],
    relation_id_set: &mut IdSet,
) -> Result<()> {
    use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

    let mut raw_buf: Vec<u8> = Vec::new();
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut group_ranges: Vec<(usize, usize)> = Vec::new();

    let mut pread_ms: u64 = 0;
    let mut decompress_ms: u64 = 0;
    let mut scan_ms: u64 = 0;

    use std::os::unix::fs::FileExt;
    for task in relation_schedule {
        let t0 = std::time::Instant::now();
        raw_buf.resize(task.data_size, 0);
        shared_file
            .read_exact_at(&mut raw_buf, task.data_offset)
            .map_err(|e| format!("failed to pread relation blob at {}: {e}", task.data_offset))?;
        #[allow(clippy::cast_possible_truncation)]
        {
            pread_ms += t0.elapsed().as_millis() as u64;
        }

        let t1 = std::time::Instant::now();
        crate::blob::decompress_blob_raw(&raw_buf, &mut decompress_buf)
            .map_err(|e| format!("R1 decompress: {e}"))?;
        #[allow(clippy::cast_possible_truncation)]
        {
            decompress_ms += t1.elapsed().as_millis() as u64;
        }

        // Wire-format scan: extract relation IDs without full
        // PrimitiveBlock decode. Skip string table, skip all fields
        // except PrimitiveGroup (field 2) → Relation (field 4) → id (field 1).
        let t2 = std::time::Instant::now();
        group_ranges.clear();
        let mut cursor = Cursor::new(&decompress_buf);
        while let Some((field, wire_type)) =
            cursor.read_tag().map_err(|e| format!("R1 block: {e}"))?
        {
            if field == 2 && wire_type == WIRE_LEN {
                let data = cursor
                    .read_len_delimited()
                    .map_err(|e| format!("R1 group: {e}"))?;
                let offset = data.as_ptr() as usize - decompress_buf.as_ptr() as usize;
                group_ranges.push((offset, data.len()));
            } else {
                cursor
                    .skip_field(wire_type)
                    .map_err(|e| format!("R1 skip: {e}"))?;
            }
        }

        for &(off, len) in &group_ranges {
            let group_bytes = &decompress_buf[off..off + len];
            let mut gcursor = Cursor::new(group_bytes);
            while let Some((field, wire_type)) =
                gcursor.read_tag().map_err(|e| format!("R1 gfield: {e}"))?
            {
                if field == 4 && wire_type == WIRE_LEN {
                    // Relation submessage - extract field 1 (id).
                    let rel_bytes = gcursor
                        .read_len_delimited()
                        .map_err(|e| format!("R1 rel: {e}"))?;
                    let mut rcursor = Cursor::new(rel_bytes);
                    while let Some((rf, rt)) =
                        rcursor.read_tag().map_err(|e| format!("R1 rfield: {e}"))?
                    {
                        if rf == 1 && rt == WIRE_VARINT {
                            let rel_id = rcursor
                                .read_varint_i64()
                                .map_err(|e| format!("R1 id: {e}"))?;
                            reject_negative_id(rel_id, "relation")?;
                            relation_id_set.set(rel_id);
                            // id is always field 1, first in the message - skip the rest.
                            break;
                        }
                        rcursor
                            .skip_field(rt)
                            .map_err(|e| format!("R1 rskip: {e}"))?;
                    }
                } else {
                    gcursor
                        .skip_field(wire_type)
                        .map_err(|e| format!("R1 gskip: {e}"))?;
                }
            }
        }
        #[allow(clippy::cast_possible_truncation)]
        {
            scan_ms += t2.elapsed().as_millis() as u64;
        }
    }

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("r1_pread_ms", pread_ms as i64);
        crate::debug::emit_counter("r1_decompress_ms", decompress_ms as i64);
        crate::debug::emit_counter("r1_scan_ms", scan_ms as i64);
        crate::debug::emit_counter("r1_blobs", relation_schedule.len() as i64);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Relation pass R2d: parallel wire-format rewrite + write output
// ---------------------------------------------------------------------------

/// Parallel R2d: wire-format splice rewriter for relation blobs.
/// Work-stealing dispatch with ReorderBuffer, same pattern as pass 1
/// and stage 2d. Each worker resolves node/way member refs inline via
/// `resolve()` - no flat files, no mmaps, no sidecar.
#[hotpath::measure]
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation
)]
pub(super) fn relation_r2d_assembly(
    shared_file: &std::sync::Arc<std::fs::File>,
    relation_schedule: &[BlobTask],
    writer: &mut crate::writer::PbfWriter<crate::write::file_writer::FileWriter>,
    node_id_set: &IdSet,
    start_node_id: i64,
    way_id_set: &IdSet,
    start_way_id: i64,
    relation_id_set: &IdSet,
    start_relation_id: i64,
    stats: &mut RenumberStats,
) -> Result<()> {
    if relation_schedule.is_empty() {
        return Ok(());
    }

    let decode_threads = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4)
        .min(6);

    // Each blob produces one OwnedBlock tuple + orphan count.
    type R2dItem = (
        usize,
        std::result::Result<(Vec<u8>, crate::blob_meta::BlobIndex, u64, u64), String>,
    );
    let (decoded_tx, decoded_rx) = std::sync::mpsc::sync_channel::<R2dItem>(32);

    let rels_written = std::sync::atomic::AtomicU64::new(0);
    let r2d_orphans = std::sync::atomic::AtomicU64::new(0);
    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let next_ref = &next_idx;
    let r2d_counters = StageCounters::new();
    let r2d_cref = &r2d_counters;

    std::thread::scope(|scope| -> Result<()> {
        for _ in 0..decode_threads {
            let file = std::sync::Arc::clone(shared_file);
            let tx = decoded_tx.clone();
            let rid_set = relation_id_set;
            let start_rid = start_relation_id;
            scope.spawn(move || {
                use std::os::unix::fs::FileExt as _;
                use std::sync::atomic::Ordering::Relaxed;
                let mut read_buf: Vec<u8> = Vec::new();
                let mut decompress_buf: Vec<u8> = Vec::new();
                let mut reframe_buf: Vec<u8> = Vec::new();
                let mut memids_scratch: Vec<u8> = Vec::new();
                let mut group_scratch: Vec<u8> = Vec::new();
                let mut reframed_rel_scratch: Vec<u8> = Vec::new();
                let mut group_ranges: Vec<(usize, usize)> = Vec::new();
                let mut scalar_fields: Vec<u8> = Vec::new();

                loop {
                    let idx = next_ref.fetch_add(1, Relaxed);
                    if idx >= relation_schedule.len() {
                        break;
                    }
                    let task = &relation_schedule[idx];

                    let result: std::result::Result<
                        (Vec<u8>, crate::blob_meta::BlobIndex, u64, u64),
                        String,
                    > = (|| {
                        let t0 = std::time::Instant::now();
                        read_buf.resize(task.data_size, 0);
                        file.read_exact_at(&mut read_buf, task.data_offset)
                            .map_err(|e| format!("pread at {}: {e}", task.data_offset))?;
                        #[allow(clippy::cast_possible_truncation)]
                        r2d_cref
                            .pread_ms
                            .fetch_add(t0.elapsed().as_millis() as u64, Relaxed);

                        let t1 = std::time::Instant::now();
                        crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)
                            .map_err(|e| e.to_string())?;
                        #[allow(clippy::cast_possible_truncation)]
                        r2d_cref
                            .decompress_ms
                            .fetch_add(t1.elapsed().as_millis() as u64, Relaxed);

                        let t2 = std::time::Instant::now();
                        let (blob_count, min_id, max_id, blob_orphans) =
                            reframe_relations_with_new_ids(
                                &decompress_buf,
                                rid_set,
                                start_rid,
                                node_id_set,
                                start_node_id,
                                way_id_set,
                                start_way_id,
                                &mut reframe_buf,
                                &mut memids_scratch,
                                &mut group_scratch,
                                &mut reframed_rel_scratch,
                                &mut group_ranges,
                                &mut scalar_fields,
                            )?;
                        #[allow(clippy::cast_possible_truncation)]
                        r2d_cref
                            .reframe_ms
                            .fetch_add(t2.elapsed().as_millis() as u64, Relaxed);

                        r2d_cref.blobs.fetch_add(1, Relaxed);

                        let index = crate::blob_meta::BlobIndex {
                            kind: crate::blob_meta::ElemKind::Relation,
                            min_id,
                            max_id,
                            count: blob_count,
                            bbox: None,
                        };
                        let taken = std::mem::take(&mut reframe_buf);
                        reframe_buf.reserve(taken.len());
                        Ok((taken, index, blob_count, blob_orphans))
                    })();

                    let t4 = std::time::Instant::now();
                    if tx.send((idx, result)).is_err() {
                        break;
                    }
                    #[allow(clippy::cast_possible_truncation)]
                    r2d_cref
                        .send_ms
                        .fetch_add(t4.elapsed().as_millis() as u64, Relaxed);
                }
            });
        }

        drop(decoded_tx);

        // Consumer: reorder by seq, write to output in file order.
        #[allow(clippy::type_complexity)]
        let mut reorder: crate::reorder_buffer::ReorderBuffer<
            std::result::Result<(Vec<u8>, crate::blob_meta::BlobIndex, u64, u64), String>,
        > = crate::reorder_buffer::ReorderBuffer::with_capacity(64);

        loop {
            let t_recv = std::time::Instant::now();
            let msg = decoded_rx.recv();
            #[allow(clippy::cast_possible_truncation)]
            r2d_cref.consumer_recv_ms.fetch_add(
                t_recv.elapsed().as_millis() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
            let (seq_num, item) = match msg {
                Ok(v) => v,
                Err(_) => break,
            };
            reorder.push(seq_num, item);
            while let Some(result) = reorder.pop_ready() {
                let (block_bytes, index, blob_count, blob_orphans) =
                    result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                if index.count > 0 {
                    let t0 = std::time::Instant::now();
                    writer.write_primitive_block_owned(block_bytes, index, None, None)?;
                    #[allow(clippy::cast_possible_truncation)]
                    r2d_cref.consumer_write_ms.fetch_add(
                        t0.elapsed().as_millis() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
                // Bump summary counters after the successful write so that
                // on a mid-stream error both relations_written and
                // orphan_refs reflect only output actually emitted. Empty
                // blobs (blob_count == 0 && index.count == 0) contribute
                // zero on either side and so don't need a guard.
                rels_written.fetch_add(blob_count, std::sync::atomic::Ordering::Relaxed);
                r2d_orphans.fetch_add(blob_orphans, std::sync::atomic::Ordering::Relaxed);
            }
        }
        Ok(())
    })?;

    r2d_counters.emit("r2d");
    stats.relations_written += rels_written.load(std::sync::atomic::Ordering::Relaxed);
    stats.orphan_refs += r2d_orphans.load(std::sync::atomic::Ordering::Relaxed);

    Ok(())
}
