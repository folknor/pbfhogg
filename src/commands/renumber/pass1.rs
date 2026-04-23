//! Pass 1: parallel node scan - worker pool with work-stealing dispatch.
//!
//! Workers (PASS1_WORKERS) claim blobs via `AtomicUsize::fetch_add` over
//! the schedule. All workers share a single pre-allocated `IdSet`
//! via `set_atomic()`. Per-blob base new_id is pre-computed in a
//! prefix-sum array so workers process blobs in any order.

use crate::idset::IdSet;
use super::super::Result;
use super::schedule::BlobTask;
use super::wire_rewrite::reframe_dense_with_new_ids;
use super::{StageCounters, PASS1_WORKERS};
use crate::block_builder::OwnedBlock;

/// Parallel pass 1: wire-format node rewriter with work-stealing dispatch.
///
/// Workers (PASS1_WORKERS) claim blobs via `AtomicUsize::fetch_add`
/// over the schedule. All workers share a single pre-allocated `IdSet`
/// via `set_atomic()`. Per-blob base new_id is pre-computed in a
/// prefix-sum array so workers process blobs in any order.
///
/// Workers pread → decompress → `reframe_dense_with_new_ids` (splice
/// new ID deltas, copy lat/lon/tags/metadata verbatim) → send
/// `Vec<OwnedBlock>` via bounded channel. Main thread reorders by
/// seq and writes output.
#[hotpath::measure]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn pass1_parallel_scan(
    schedule: &[BlobTask],
    start_node_id: i64,
    shared_file: &std::sync::Arc<std::fs::File>,
    node_id_set: &IdSet,
    nodes_written: &std::sync::atomic::AtomicU64,
    writer: &mut crate::writer::PbfWriter<crate::file_writer::FileWriter>,
) -> Result<()> {
    if schedule.is_empty() {
        return Ok(());
    }

    // Pre-compute per-blob base new_id = start + sum(element_count[..seq]).
    // Workers look up `base_new_ids[task.seq]` instead of maintaining a
    // sequential counter - they may process tasks in any order.
    let mut base_new_ids: Vec<i64> = Vec::with_capacity(schedule.len());
    let mut cursor = start_node_id;
    for task in schedule {
        base_new_ids.push(cursor);
        cursor = cursor
            .checked_add(
                i64::try_from(task.element_count)
                    .map_err(|_| "planet node count > i64 in pass1 prefix sum")?,
            )
            .ok_or("pass1 base new_id overflow")?;
    }

    type DecodedItem = (usize, std::result::Result<Vec<OwnedBlock>, String>);
    let (decoded_tx, decoded_rx) = std::sync::mpsc::sync_channel::<DecodedItem>(32);
    let base_ids_ref: &[i64] = &base_new_ids;
    let pass1_counters = StageCounters::new();
    let pass1_cref = &pass1_counters;
    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let next_ref = &next_idx;

    std::thread::scope(|scope| -> Result<()> {
        for _ in 0..PASS1_WORKERS {
            let file = std::sync::Arc::clone(shared_file);
            let tx = decoded_tx.clone();
            scope.spawn(move || {
                pass1_worker(
                    schedule,
                    next_ref,
                    base_ids_ref,
                    &file,
                    node_id_set,
                    nodes_written,
                    pass1_cref,
                    &tx,
                );
            });
        }

        drop(decoded_tx);

        let mut reorder: crate::reorder_buffer::ReorderBuffer<
            std::result::Result<Vec<OwnedBlock>, String>,
        > = crate::reorder_buffer::ReorderBuffer::with_capacity(64);

        loop {
            let t_recv = std::time::Instant::now();
            let msg = decoded_rx.recv();
            #[allow(clippy::cast_possible_truncation)]
            pass1_cref.consumer_recv_ms.fetch_add(
                t_recv.elapsed().as_millis() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
            let (seq_num, item) = match msg {
                Ok(v) => v,
                Err(_) => break,
            };
            reorder.push(seq_num, item);
            while let Some(result) = reorder.pop_ready() {
                let blocks = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                for (block_bytes, index, tagdata) in blocks {
                    let t0 = std::time::Instant::now();
                    writer.write_primitive_block_owned(
                        block_bytes,
                        index,
                        tagdata.as_deref(),
                    )?;
                    #[allow(clippy::cast_possible_truncation)]
                    pass1_cref.consumer_write_ms.fetch_add(
                        t0.elapsed().as_millis() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
            }
        }

        Ok(())
    })?;

    pass1_counters.emit("pass1");
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn pass1_worker(
    schedule: &[BlobTask],
    next_idx: &std::sync::atomic::AtomicUsize,
    base_new_ids: &[i64],
    shared_file: &std::sync::Arc<std::fs::File>,
    id_set: &IdSet,
    nodes_written: &std::sync::atomic::AtomicU64,
    counters: &StageCounters,
    tx: &std::sync::mpsc::SyncSender<(usize, std::result::Result<Vec<OwnedBlock>, String>)>,
) {
    use std::os::unix::fs::FileExt as _;
    use std::sync::atomic::Ordering::Relaxed;

    let mut read_buf: Vec<u8> = Vec::new();
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut reframe_buf: Vec<u8> = Vec::new();
    let mut output_blocks: Vec<OwnedBlock> = Vec::new();
    let mut group_ranges_scratch: Vec<(usize, usize)> = Vec::new();
    let mut scalar_fields_scratch: Vec<u8> = Vec::new();
    let mut other_fields_scratch: Vec<u8> = Vec::new();
    let mut new_id_packed_scratch: Vec<u8> = Vec::new();
    let mut dense_out_scratch: Vec<u8> = Vec::new();
    let mut group_out_scratch: Vec<u8> = Vec::new();

    loop {
        let idx = next_idx.fetch_add(1, Relaxed);
        if idx >= schedule.len() {
            break;
        }
        let task = &schedule[idx];

        let base_new_id = base_new_ids[task.seq];
        let result: std::result::Result<Vec<OwnedBlock>, String> = (|| {
            let t0 = std::time::Instant::now();
            read_buf.resize(task.data_size, 0);
            shared_file
                .read_exact_at(&mut read_buf, task.data_offset)
                .map_err(|e| format!("pread failed at offset {}: {e}", task.data_offset))?;
            #[allow(clippy::cast_possible_truncation)]
            counters.pread_ms.fetch_add(t0.elapsed().as_millis() as u64, Relaxed);

            let t1 = std::time::Instant::now();
            crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)
                .map_err(|e| e.to_string())?;
            #[allow(clippy::cast_possible_truncation)]
            counters.decompress_ms.fetch_add(t1.elapsed().as_millis() as u64, Relaxed);

            let t2 = std::time::Instant::now();
            reframe_buf.clear();
            let blob_node_count = reframe_dense_with_new_ids(
                &decompress_buf,
                base_new_id,
                id_set,
                &mut reframe_buf,
                &mut group_ranges_scratch,
                &mut scalar_fields_scratch,
                &mut other_fields_scratch,
                &mut new_id_packed_scratch,
                &mut dense_out_scratch,
                &mut group_out_scratch,
            )?;
            #[allow(clippy::cast_possible_truncation)]
            counters.reframe_ms.fetch_add(t2.elapsed().as_millis() as u64, Relaxed);

            let index = crate::blob_meta::BlobIndex {
                kind: crate::blob_meta::ElemKind::Node,
                min_id: base_new_id,
                #[allow(clippy::cast_possible_wrap)]
                max_id: base_new_id + blob_node_count as i64 - 1,
                count: blob_node_count,
                bbox: task.bbox,
            };
            output_blocks.clear();
            let taken = std::mem::take(&mut reframe_buf);
            reframe_buf.reserve(taken.len());
            output_blocks.push((taken, index, None));

            if blob_node_count != task.element_count {
                return Err(format!(
                    "pass1 blob {} decoded {} nodes, indexdata said {}",
                    task.seq, blob_node_count, task.element_count
                ));
            }

            nodes_written.fetch_add(blob_node_count, Relaxed);
            counters.blobs.fetch_add(1, Relaxed);

            Ok(std::mem::take(&mut output_blocks))
        })();

        let t4 = std::time::Instant::now();
        if tx.send((task.seq, result)).is_err() {
            break;
        }
        #[allow(clippy::cast_possible_truncation)]
        counters.send_ms.fetch_add(t4.elapsed().as_millis() as u64, Relaxed);
    }
}
