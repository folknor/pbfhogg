//! Stage 2d: parallel way rewrite - fused ref resolve + wire-format splice.
//!
//! Single pass over way blobs: resolves refs inline via
//! `node_id_set.rank()` and splices new IDs into the wire format.
//! No intermediate flat file, no sidecar, no mmap.

use super::super::id_set_dense::IdSetDense;
use super::super::Result;
use super::schedule::BlobTask;
use super::wire_rewrite::reframe_ways_with_new_ids;
use super::StageCounters;
use crate::block_builder::OwnedBlock;

/// Parallel stage 2d: fused way resolve + wire-format rewrite.
///
/// Single pass over way blobs: resolves refs inline via
/// `node_id_set.rank()` and splices new IDs into the wire format.
/// No intermediate flat file, no sidecar, no mmap.
///
/// Workers (STAGE2D_WORKERS) claim blobs via `AtomicUsize::fetch_add`.
/// Each worker owns an `IdSetDense` shard for `way_id_set` and scratch
/// buffers. Workers pread → decompress → `reframe_ways_with_new_ids`
/// (splice new way IDs + resolved refs) → send `Vec<OwnedBlock>` via
/// bounded channel. Main thread reorders by seq and writes output.
#[hotpath::measure]
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation
)]
pub(super) fn stage2d_parallel_way_assembly(
    shared_file: &std::sync::Arc<std::fs::File>,
    writer: &mut crate::writer::PbfWriter<crate::file_writer::FileWriter>,
    way_id_sets: &mut [IdSetDense],
    way_schedule: &[BlobTask],
    node_id_set: &IdSetDense,
    start_node_id: i64,
    start_way_id: i64,
    ways_written: &std::sync::atomic::AtomicU64,
    orphan_refs: &std::sync::atomic::AtomicU64,
) -> Result<()> {
    if way_schedule.is_empty() {
        return Ok(());
    }

    let total_ways: u64 = way_schedule.iter().map(|t| t.element_count).sum();
    i64::try_from(total_ways).map_err(|_| "planet way count > i64")?;
    let mut base_way_ids: Vec<i64> = Vec::with_capacity(way_schedule.len());
    let mut cursor = start_way_id;
    for task in way_schedule {
        base_way_ids.push(cursor);
        cursor = cursor
            .checked_add(
                i64::try_from(task.element_count)
                    .map_err(|_| "stage 2d way count > i64 in prefix sum")?,
            )
            .ok_or("stage 2d base way_id overflow")?;
    }

    type DecodedItem = (usize, std::result::Result<Vec<OwnedBlock>, String>);
    let (decoded_tx, decoded_rx) = std::sync::mpsc::sync_channel::<DecodedItem>(32);
    let base_ids_ref: &[i64] = &base_way_ids;
    let stage2d_counters = StageCounters::new();
    let stage2d_cref = &stage2d_counters;
    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let next_ref = &next_idx;

    std::thread::scope(|scope| -> Result<()> {
        {
            let mut remaining_sets: &mut [IdSetDense] = way_id_sets;
            for _ in 0..remaining_sets.len() {
                let (is, it) = remaining_sets.split_at_mut(1);
                remaining_sets = it;
                let id_set = &mut is[0];
                let file = std::sync::Arc::clone(shared_file);
                let tx = decoded_tx.clone();
                scope.spawn(move || {
                    stage2d_worker(
                        way_schedule,
                        next_ref,
                        base_ids_ref,
                        &file,
                        node_id_set,
                        start_node_id,
                        id_set,
                        ways_written,
                        orphan_refs,
                        stage2d_cref,
                        &tx,
                    );
                });
            }
        }

        drop(decoded_tx);

        let mut reorder: crate::reorder_buffer::ReorderBuffer<
            std::result::Result<Vec<OwnedBlock>, String>,
        > = crate::reorder_buffer::ReorderBuffer::with_capacity(64);

        loop {
            let t_recv = std::time::Instant::now();
            let msg = decoded_rx.recv();
            #[allow(clippy::cast_possible_truncation)]
            stage2d_cref.consumer_recv_ms.fetch_add(
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
                    stage2d_cref.consumer_write_ms.fetch_add(
                        t0.elapsed().as_millis() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
            }
        }
        Ok(())
    })?;

    stage2d_counters.emit("stage2d");
    Ok(())
}

/// Stage 2d per-worker loop. Claims blobs via `AtomicUsize::fetch_add`
/// and emits one owned-block batch per blob through the channel.
/// Resolves way refs inline via `node_id_set.rank()`.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn stage2d_worker(
    schedule: &[BlobTask],
    next_idx: &std::sync::atomic::AtomicUsize,
    base_way_ids: &[i64],
    shared_file: &std::sync::Arc<std::fs::File>,
    node_id_set: &IdSetDense,
    start_node_id: i64,
    way_id_set: &mut IdSetDense,
    ways_written: &std::sync::atomic::AtomicU64,
    orphan_refs: &std::sync::atomic::AtomicU64,
    counters: &StageCounters,
    tx: &std::sync::mpsc::SyncSender<(usize, std::result::Result<Vec<OwnedBlock>, String>)>,
) {
    use std::os::unix::fs::FileExt as _;
    use std::sync::atomic::Ordering::Relaxed;

    let mut read_buf: Vec<u8> = Vec::new();
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut reframe_buf: Vec<u8> = Vec::new();
    let mut refs_scratch: Vec<u8> = Vec::new();
    let mut group_scratch: Vec<u8> = Vec::new();
    let mut reframed_way_scratch: Vec<u8> = Vec::new();
    let mut output_blocks: Vec<OwnedBlock> = Vec::new();
    let mut way_group_ranges: Vec<(usize, usize)> = Vec::new();
    let mut way_scalar_fields: Vec<u8> = Vec::new();

    loop {
        let idx = next_idx.fetch_add(1, Relaxed);
        if idx >= schedule.len() {
            break;
        }
        let task = &schedule[idx];

        let base_way_id = base_way_ids[task.seq];

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
            let (blob_way_count, blob_orphans) = reframe_ways_with_new_ids(
                &decompress_buf,
                base_way_id,
                node_id_set,
                start_node_id,
                way_id_set,
                &mut reframe_buf,
                &mut refs_scratch,
                &mut group_scratch,
                &mut reframed_way_scratch,
                task.min_id < 0,
                &mut way_group_ranges,
                &mut way_scalar_fields,
            )?;
            #[allow(clippy::cast_possible_truncation)]
            counters.reframe_ms.fetch_add(t2.elapsed().as_millis() as u64, Relaxed);

            // Build the OwnedBlock from the reframed bytes.
            let index = crate::blob_meta::BlobIndex {
                kind: crate::blob_meta::ElemKind::Way,
                min_id: base_way_id,
                #[allow(clippy::cast_possible_wrap)]
                max_id: base_way_id + blob_way_count as i64 - 1,
                count: blob_way_count,
                bbox: None,
            };
            output_blocks.clear();
            let taken = std::mem::take(&mut reframe_buf);
            // Pre-reserve for next blob based on this blob's output size.
            reframe_buf.reserve(taken.len());
            output_blocks.push((taken, index, None));

            if blob_way_count != task.element_count {
                return Err(format!(
                    "stage 2d blob {} decoded {} ways, indexdata said {}",
                    task.seq, blob_way_count, task.element_count
                ));
            }

            ways_written.fetch_add(blob_way_count, Relaxed);
            orphan_refs.fetch_add(blob_orphans, Relaxed);
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
