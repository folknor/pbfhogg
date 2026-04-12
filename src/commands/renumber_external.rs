//! External-join implementation of renumber for planet-scale input.
//!
//! The in-memory `renumber` module allocates three `FxHashMap<i64, i64>`
//! tables whose combined size on planet is ~278 GB (node_map ~250 GB,
//! way_map ~28 GB, relation_map ~340 MB), which OOM-kills any host
//! that isn't already oversized. This module replaces `node_map` and
//! `way_map` with IdSetDense bitsets + rank-based O(1) lookup, keeping
//! only the small `relation_map` in RAM.
//!
//! ## Architecture
//!
//! - **Pass 1**: parallel wire-format node rewriter (4 work-stealing
//!   workers). Per-worker IdSetDense bitsets merged after pass 1 for
//!   O(1) rank-based node ID lookup.
//! - **Fused way scan**: resolve each way ref via `node_id_set.rank()`,
//!   write resolved refs to a flat file sequentially.
//! - **Stage 2d**: parallel wire-format way splice rewriter (6
//!   work-stealing workers). Per-worker IdSetDense for way_id_set.
//! - **Fused relation scan (R1+R2A+R2B)**: sequential relation scan
//!   that assigns new relation IDs and resolves node/way member refs
//!   via rank() inline, writing flat files directly.
//! - **R2d**: sequential wire-format splice rewriter for relations.
//!
//! Planet: 442 s (7m22s). Denmark cross-validated against in-memory mode.

use std::path::Path;

use super::renumber::{RenumberOptions, RenumberStats};
use super::{require_sorted, writer_from_header, HeaderOverrides, Result};
use crate::block_builder::OwnedBlock;
use crate::writer::Compression;
use crate::Element;

/// Alias for the deterministic hash map used by the in-memory relation map.
type FxHashMap<K, V> = rustc_hash::FxHashMap<K, V>;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Reject negative ids at the entry of the external path.
///
/// Production OSM planet extracts don't contain negative ids (they're
/// JOSM-local editor staging identifiers resolved before upload);
/// `renumber --mode inmem` handles them via the in-memory FxHashMap path.
/// The external path rejects them with a clear error.
fn reject_negative_id(id: i64, kind: &str) -> Result<()> {
    if id < 0 {
        return Err(format!(
            "renumber --mode external requires non-negative input ids. \
             Input contains {kind} id {id}. \
             Use --mode inmem for files with negative (editor-local) ids."
        )
        .into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the planet-safe external renumber.
///
/// Architecture: pass 1 rewrites nodes (parallel wire-format rewriter),
/// fused way scan resolves refs via IdSetDense rank, stage 2d rewrites
/// ways (parallel wire-format rewriter), fused relation scan resolves
/// member refs via rank, R2d assembles relations via full decode.
#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub fn renumber_external(
    input: &Path,
    output: &Path,
    opts: &RenumberOptions,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<RenumberStats> {
    // Limit glibc malloc arenas to prevent cross-thread free
    // fragmentation. Without this, OwnedBlock Vec<u8>s allocated on
    // pass1/stage2d worker threads and freed on rayon compression
    // threads cause glibc arena accumulation growing to ~26 GB anon
    // RSS on planet. With 2 arenas the peak stays under 1 GB.
    // Scoped to this command — other pbfhogg commands are unaffected.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }

    // ---- Header validation + output writer setup ----
    {
        let mut header_reader = crate::blob::BlobReader::open(input, direct_io)?;
        let header_blob = header_reader
            .next()
            .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
        let header = header_blob.to_headerblock()?;
        require_sorted(&header, input, "Input PBF")?;
        super::warn_locations_on_ways_loss(&header);
    }
    // Re-parse header for writer construction (the earlier reader is dropped).
    let header = {
        let mut header_reader = crate::blob::BlobReader::open(input, direct_io)?;
        let header_blob = header_reader
            .next()
            .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
        header_blob.to_headerblock()?
    };
    // Default to zlib:1 for external renumber — the compression pipeline
    // is on the critical path for pass 1 and stage 2d, and zlib:6 adds
    // ~22 s of backpressure at planet scale for ~15% smaller output.
    // Respect explicit caller overrides (e.g. --compression zlib:6).
    let effective_compression = if compression == Compression::default() {
        Compression::Zlib(1)
    } else {
        compression
    };
    let mut writer = writer_from_header(output, effective_compression, &header, true, overrides, |hb| {
        hb.sorted()
    }, direct_io, false)?;

    let mut next_relation_id = opts.start_relation_id;
    let mut relation_map: FxHashMap<i64, i64> = FxHashMap::default();
    let mut stats = RenumberStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
    };

    crate::debug::emit_marker("RENUMBER_EXT_START");
    crate::debug::emit_marker("RENUMBER_EXT_PASS1_START");

    // ---- Pass 1: parallel node scan ----
    //
    // Architecture ports external_join.rs stage 4 (assembly) pattern:
    //
    // 1. Pre-scan blob headers to build a schedule filtered to node
    //    blobs, with each blob's element count. Compute prefix sums so
    //    each blob's base new_id is known before any decode work runs.
    // 2. Range-split the schedule in half by blob index. Worker 0 gets
    //    the first half, worker 1 gets the second. Range-based (not
    //    work-stealing) dispatch preserves per-shard bucket-file sort:
    //    each shard's bucket N contains old_ids in strictly ascending
    //    order, and the two shards are disjoint (shard 0's old_ids are
    //    all less than shard 1's). Stage 2b reads them as a concatenated
    //    sorted run.
    // 3. Each worker owns: its IdSetDense bitset,
    //    its BlockBuilder, its read_buf + decompress_buf + scratch Vecs,
    //    its output_blocks Vec<OwnedBlock>. All allocations stay worker-
    //    local — no cross-thread malloc/free churn.
    // 4. Workers send (seq, Result<Vec<OwnedBlock>>) via a bounded
    //    channel. The OwnedBlock's Vec<u8> IS cross-thread-transferred to
    //    the consumer, but bounded at ~32 items × ~1.4 MB = ~45 MB
    //    in flight. Matches the external_join stage 4 pattern which
    //    runs planet-scale without OOM.
    // 5. Main thread consumer drains the channel, uses ReorderBuffer to
    //    deliver (seq, blocks) in file order, pushes each OwnedBlock via
    //    writer.write_primitive_block_owned.
    //
    // The for_each_block_pipelined path was attempted first and OOMed at
    // 26 GB anon RSS on planet — cross-thread PrimitiveBlock retention
    // via glibc arena accumulation, exactly as notes/parallel-classify-
    // regression.md predicted. This pattern avoids that by extracting
    // per-blob OwnedBlock output on the worker thread (so PrimitiveBlocks
    // drop on the worker) and only crossing the Vec<u8> of already-encoded
    // output bytes.
    // Scan all blob headers once — builds node/way/relation schedules
    // in a single pass instead of scanning 3-4 times.
    let (pass1_schedule, way_schedule, relation_schedule) =
        build_all_blob_schedules(input)?;
    let pass1_total_nodes: u64 = pass1_schedule.iter().map(|t| t.element_count).sum();

    // Per-worker IdSetDense bitsets. Merged after pass 1 to produce a
    // single bitset with rank index for O(1) new_id lookup in the fused
    // way scan. Replaces the old node_map shard bucket files entirely.
    const PASS1_WORKERS: usize = 4;
    let mut worker_id_sets: Vec<super::id_set_dense::IdSetDense> = (0..PASS1_WORKERS)
        .map(|_| super::id_set_dense::IdSetDense::new())
        .collect();

    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input).map_err(|e| format!("failed to open {}: {e}", input.display()))?,
    );

    let nodes_written_atomic = std::sync::atomic::AtomicU64::new(0);

    pass1_parallel_scan(
        &pass1_schedule,
        opts.start_node_id,
        &shared_file,
        &mut worker_id_sets,
        &nodes_written_atomic,
        &mut writer,
    )?;

    stats.nodes_written += nodes_written_atomic.load(std::sync::atomic::Ordering::Relaxed);
    // Sanity check: the two-worker prefix sum must match the actual
    // atomic count. If not, either the schedule's indexdata count
    // diverged from the decoded node count or a worker dropped work.
    if stats.nodes_written != pass1_total_nodes {
        return Err(format!(
            "pass1 node count mismatch: schedule reported {pass1_total_nodes}, \
             workers wrote {}",
            stats.nodes_written,
        )
        .into());
    }

    crate::debug::emit_marker("RENUMBER_EXT_PASS1_END");

    // ---- Merge per-worker IdSetDense bitsets and build rank index ----
    // Workers each built an independent bitset. Merge via bitwise OR,
    // then build the rank prefix sums for O(1) rank() lookup.
    let mut node_id_set = worker_id_sets.remove(0);
    for other in worker_id_sets {
        node_id_set.merge(other);
    }
    node_id_set.build_rank_index();
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter(
            "renumber_ext_node_map_entries",
            node_id_set.total_count() as i64,
        );
    }

    // ---- Stage 2d: fused way resolve + rewrite (single pass) ----
    // Resolves way refs inline via node_id_set.rank() during
    // wire-format splice. No intermediate flat file or sidecar.
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2D_START");
    const STAGE2D_WORKERS: usize = 6;
    let mut way_id_sets: Vec<super::id_set_dense::IdSetDense> = (0..STAGE2D_WORKERS)
        .map(|_| super::id_set_dense::IdSetDense::new())
        .collect();
    let stage2d_ways_atomic = std::sync::atomic::AtomicU64::new(0);
    stage2d_parallel_way_assembly(
        input,
        &mut writer,
        &mut way_id_sets,
        &way_schedule,
        &node_id_set,
        opts.start_node_id,
        opts.start_way_id,
        &stage2d_ways_atomic,
    )?;
    stats.ways_written += stage2d_ways_atomic.load(std::sync::atomic::Ordering::Relaxed);
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2D_END");

    // ---- Relation passes R1 + R2a (fused): assign ids + emit member refs ----
    // Single scan over relation blobs. R1 assigns new_relation_ids and
    // builds the in-memory relation_map. R2a emits (old_id, slot_pos)
    // COO pairs for node and way members into their respective bucket
    // sets. Both halves operate on each relation in isolation — R2a
    // does not consult relation_map (relation members are resolved in
    // R2d directly), so the two passes can share a single decoded
    // block.
    // ---- R1: assign relation IDs + build relation_map ----
    crate::debug::emit_marker("RENUMBER_EXT_R1_R2A_START");

    // Merge per-worker way_id_sets built during stage 2d.
    let mut way_id_set = way_id_sets.remove(0);
    for other in way_id_sets {
        way_id_set.merge(other);
    }
    way_id_set.build_rank_index();

    relation_r1_assign_ids(
        input,
        &relation_schedule,
        &mut relation_map,
        &mut next_relation_id,
    )?;
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("renumber_ext_relation_map_entries", relation_map.len() as i64);
    }
    crate::debug::emit_marker("RENUMBER_EXT_R1_R2A_END");

    // ---- R2d: parallel wire-format rewrite of relations ----
    // Resolves node/way member refs inline via resolve().
    // No flat files, no mmaps, no sidecar.
    crate::debug::emit_marker("RENUMBER_EXT_R2D_START");
    relation_r2d_assembly(
        input,
        &relation_schedule,
        &mut writer,
        &node_id_set,
        opts.start_node_id,
        &way_id_set,
        opts.start_way_id,
        &relation_map,
        &mut stats,
    )?;
    crate::debug::emit_marker("RENUMBER_EXT_R2D_END");

    writer.flush()?;

    crate::debug::emit_marker("RENUMBER_EXT_END");

    Ok(stats)
}


// ---------------------------------------------------------------------------
// Pass 2 stage D: way assembly — re-scan ways, rewrite refs, write output
// ---------------------------------------------------------------------------

/// Parallel stage 2d: fused way resolve + wire-format rewrite.
///
/// Single pass over way blobs: resolves refs inline via
/// `node_id_set.rank()` and splices new IDs into the wire format.
/// No intermediate flat file, no sidecar, no mmap.
///
/// Mirrors the pass-1 worker-pool pattern: two range-partitioned
/// workers, each owning a way_map shard, a `BlockBuilder`, its own
/// scratch buffers, and an `output_blocks: Vec<OwnedBlock>` staging
/// vec. Workers pread way blobs, decompress, resolve refs inline via
/// `node_id_set.rank()`, wire-format splice new IDs, and send
/// `(seq, Vec<OwnedBlock>)` via a bounded channel. The main thread
/// reorders by seq and writes via `write_primitive_block_owned`.
#[hotpath::measure]
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation
)]
fn stage2d_parallel_way_assembly(
    input: &Path,
    writer: &mut crate::writer::PbfWriter<crate::file_writer::FileWriter>,
    way_id_sets: &mut [super::id_set_dense::IdSetDense],
    way_schedule: &[BlobTask],
    node_id_set: &super::id_set_dense::IdSetDense,
    start_node_id: i64,
    start_way_id: i64,
    ways_written: &std::sync::atomic::AtomicU64,
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

    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?,
    );

    type DecodedItem = (usize, std::result::Result<Vec<OwnedBlock>, String>);
    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<&BlobTask>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (decoded_tx, decoded_rx) = std::sync::mpsc::sync_channel::<DecodedItem>(32);
    let schedule_ref = way_schedule;
    let base_ids_ref: &[i64] = &base_way_ids;
    let stage2d_counters = StageCounters::new();
    let stage2d_cref = &stage2d_counters;

    std::thread::scope(|scope| -> Result<()> {
        // Dispatcher thread
        scope.spawn(move || {
            for task in schedule_ref {
                if desc_tx.send(task).is_err() {
                    break;
                }
            }
        });

        {
            let mut remaining_sets: &mut [super::id_set_dense::IdSetDense] = way_id_sets;
            for _ in 0..remaining_sets.len() {
                let (is, it) = remaining_sets.split_at_mut(1);
                remaining_sets = it;
                let id_set = &mut is[0];
                let rx = std::sync::Arc::clone(&desc_rx);
                let file = std::sync::Arc::clone(&shared_file);
                let tx = decoded_tx.clone();
                scope.spawn(move || {
                    stage2d_worker(
                        &rx,
                        base_ids_ref,
                        &file,
                        node_id_set,
                        start_node_id,
                        id_set,
                        ways_written,
                        stage2d_cref,
                        &tx,
                    );
                });
            }
        }

        drop(decoded_tx);
        drop(desc_rx);

        let mut reorder: crate::reorder_buffer::ReorderBuffer<
            std::result::Result<Vec<OwnedBlock>, String>,
        > = crate::reorder_buffer::ReorderBuffer::with_capacity(64);

        for (seq_num, item) in decoded_rx {
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

/// Stage 2d per-worker loop. Claims blobs from a shared FIFO queue
/// and emits one owned-block batch per blob through the channel.
/// Resolves way refs inline via `node_id_set.rank()` — no flat file
/// or mmap needed.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn stage2d_worker(
    rx: &std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<&BlobTask>>>,
    base_way_ids: &[i64],
    shared_file: &std::sync::Arc<std::fs::File>,
    node_id_set: &super::id_set_dense::IdSetDense,
    start_node_id: i64,
    way_id_set: &mut super::id_set_dense::IdSetDense,
    ways_written: &std::sync::atomic::AtomicU64,
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
        let task = {
            let guard = rx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match guard.recv() {
                Ok(t) => t,
                Err(_) => break,
            }
        };

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
            let blob_way_count = reframe_ways_with_new_ids(
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
            let index = crate::blob_index::BlobIndex {
                kind: crate::blob_index::ElemKind::Way,
                min_id: base_way_id,
                #[allow(clippy::cast_possible_wrap)]
                max_id: base_way_id + blob_way_count as i64 - 1,
                count: blob_way_count,
                bbox: None,
            };
            output_blocks.clear();
            output_blocks.push((std::mem::take(&mut reframe_buf), index, None));

            if blob_way_count != task.element_count {
                return Err(format!(
                    "stage 2d blob {} decoded {} ways, indexdata said {}",
                    task.seq, blob_way_count, task.element_count
                ));
            }

            ways_written.fetch_add(blob_way_count, Relaxed);
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

// ---------------------------------------------------------------------------
// Pass 1: parallel node scan — worker pool with work-stealing dispatch
// ---------------------------------------------------------------------------

/// Per-blob task for the parallel pass pool. `seq` is the filtered-index
/// position (monotonic within the per-kind blob list, used for writer
/// reorder). `data_offset` / `data_size` address the compressed blob body
/// for pread. `element_count` comes from the indexdata `BlobIndex.count`
/// and lets the caller precompute base new_ids without racing decode.
struct BlobTask {
    seq: usize,
    data_offset: u64,
    data_size: usize,
    element_count: u64,
    /// Source blob's min element ID from indexdata. Used for per-block
    /// negative-id skip: if min_id >= 0, all IDs are non-negative.
    min_id: i64,
    /// Source blob's spatial bbox (node blobs only).
    bbox: Option<crate::blob_index::BlobBbox>,
}

/// Header-only scan building a per-kind schedule with element counts.
/// Requires indexed PBFs (all brokkr datasets are indexed): the per-blob
/// element count is read from `BlobIndex.count`, which is required to
/// precompute each blob's `base_new_id` without a full decode pass. If a
/// matching blob is missing indexdata, we error out with a pointer to
/// `brokkr cat` / indexed datasets.
/// Scan all blob headers once and build per-kind schedules.
/// Returns `(node_schedule, way_schedule, relation_schedule)`.
fn build_all_blob_schedules(
    input: &Path,
) -> Result<(Vec<BlobTask>, Vec<BlobTask>, Vec<BlobTask>)> {
    let mut scanner = crate::blob::BlobReader::seekable_from_path(input)?;
    scanner.set_parse_indexdata(true);
    scanner
        .next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let mut nodes: Vec<BlobTask> = Vec::new();
    let mut ways: Vec<BlobTask> = Vec::new();
    let mut relations: Vec<BlobTask> = Vec::new();
    let mut node_seq: usize = 0;
    let mut way_seq: usize = 0;
    let mut rel_seq: usize = 0;
    while let Some(result) = scanner.next_header_with_data_offset() {
        let (hdr, _frame_offset, data_offset, data_size) = result?;
        if !matches!(hdr.blob_type(), crate::blob::BlobType::OsmData) {
            continue;
        }
        let Some(idx) = hdr.index() else {
            return Err(
                "renumber --mode external requires an indexed PBF — run `brokkr cat` to add \
                 indexdata or use the indexed variant"
                    .into(),
            );
        };
        let (sched, seq) = match idx.kind {
            crate::blob_index::ElemKind::Node => (&mut nodes, &mut node_seq),
            crate::blob_index::ElemKind::Way => (&mut ways, &mut way_seq),
            crate::blob_index::ElemKind::Relation => (&mut relations, &mut rel_seq),
        };
        sched.push(BlobTask {
            seq: *seq,
            data_offset,
            data_size,
            element_count: idx.count,
            min_id: idx.min_id,
            bbox: idx.bbox,
        });
        *seq += 1;
    }
    Ok((nodes, ways, relations))
}

/// Two-worker parallel pass 1 via **work-stealing** dispatch.
///
/// Both workers pull blob tasks from a shared `Arc<Mutex<Receiver>>`
/// queue fed in monotonic file order by a dispatcher thread. This is a
/// deliberate departure from the original range-based split: range
/// splitting produced disjoint seq ranges `[0..split)` and
/// `[split..n)` which could *never* be interleaved in a single
/// `ReorderBuffer`, so the buffer accumulated worker B's entire
/// backlog (up to ~200k `Vec<OwnedBlock>`s at ~400 KB each = ~80 GB)
/// while worker A's range drained. Measured on planet as linear
/// ~118 MB/s anon-RSS growth, OOM-kill at 26 GB by t=295 s — see
/// commits `9695ad5` / `e7219f0` and `notes/renumber-planet-scale.md`
/// "Pass 1 memory blowup" for the full forensic.
///
/// Work-stealing keeps the reorder-buffer gap bounded by
/// `num_workers × channel_capacity` ≈ O(64) slots instead of
/// O(schedule_len / 2). Each worker owns its own IdSetDense bitset;
/// bitsets are merged after the scan via bitwise OR.
///
/// Each worker owns: its node_map bucket shard, a local
/// `BlockBuilder`, read_buf + decompress_buf scratch Vecs, and an
/// `output_blocks: Vec<OwnedBlock>` staging buffer. All allocations
/// stay worker-local — PrimitiveBlocks drop on the worker thread,
/// only `Vec<OwnedBlock>` crosses the channel bounded at ~32 items.
/// The per-blob starting `new_id` is pre-computed in a prefix-sum
/// array so workers can process any blob out of FIFO order and still
/// know which `new_id` slice to assign.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn pass1_parallel_scan(
    schedule: &[BlobTask],
    start_node_id: i64,
    shared_file: &std::sync::Arc<std::fs::File>,
    id_sets: &mut [super::id_set_dense::IdSetDense],
    nodes_written: &std::sync::atomic::AtomicU64,
    writer: &mut crate::writer::PbfWriter<crate::file_writer::FileWriter>,
) -> Result<()> {
    if schedule.is_empty() {
        return Ok(());
    }

    // Pre-compute per-blob base new_id = start + sum(element_count[..seq]).
    // Workers look up `base_new_ids[task.seq]` instead of maintaining a
    // sequential counter — they may process tasks in any order.
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
    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<&BlobTask>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (decoded_tx, decoded_rx) = std::sync::mpsc::sync_channel::<DecodedItem>(32);
    let base_ids_ref: &[i64] = &base_new_ids;
    let pass1_counters = StageCounters::new();
    let pass1_cref = &pass1_counters;

    std::thread::scope(|scope| -> Result<()> {
        // Dispatcher: feed schedule into the descriptor queue in file
        // order. Workers compete for items, so each shard receives a
        // FIFO-monotonic *subset* of the schedule.
        scope.spawn(move || {
            for task in schedule {
                if desc_tx.send(task).is_err() {
                    break;
                }
            }
        });

        {
            let mut remaining: &mut [super::id_set_dense::IdSetDense] = id_sets;
            for _ in 0..remaining.len() {
                let (head, tail) = remaining.split_at_mut(1);
                remaining = tail;
                let shard = &mut head[0];
                let rx = std::sync::Arc::clone(&desc_rx);
                let file = std::sync::Arc::clone(shared_file);
                let tx = decoded_tx.clone();
                scope.spawn(move || {
                    pass1_worker(
                        &rx,
                        base_ids_ref,
                        &file,
                        shard,
                        nodes_written,
                        pass1_cref,
                        &tx,
                    );
                });
            }
        }

        drop(decoded_tx);
        drop(desc_rx);

        let mut reorder: crate::reorder_buffer::ReorderBuffer<
            std::result::Result<Vec<OwnedBlock>, String>,
        > = crate::reorder_buffer::ReorderBuffer::with_capacity(64);

        for (seq_num, item) in decoded_rx {
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

/// Shared instrumentation counters for parallel worker stages.
/// All fields are AtomicU64 so workers can fetch_add concurrently.
/// Emit all counters via `emit()` after the scope joins workers.
struct StageCounters {
    pread_ms: std::sync::atomic::AtomicU64,
    decompress_ms: std::sync::atomic::AtomicU64,
    reframe_ms: std::sync::atomic::AtomicU64,
    bucket_emit_ms: std::sync::atomic::AtomicU64,
    send_ms: std::sync::atomic::AtomicU64,
    consumer_write_ms: std::sync::atomic::AtomicU64,
    blobs: std::sync::atomic::AtomicU64,
}

impl StageCounters {
    fn new() -> Self {
        Self {
            pread_ms: std::sync::atomic::AtomicU64::new(0),
            decompress_ms: std::sync::atomic::AtomicU64::new(0),
            reframe_ms: std::sync::atomic::AtomicU64::new(0),
            bucket_emit_ms: std::sync::atomic::AtomicU64::new(0),
            send_ms: std::sync::atomic::AtomicU64::new(0),
            consumer_write_ms: std::sync::atomic::AtomicU64::new(0),
            blobs: std::sync::atomic::AtomicU64::new(0),
        }
    }

    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    fn emit(&self, prefix: &str) {
        use std::sync::atomic::Ordering::Relaxed;
        crate::debug::emit_counter(&format!("{prefix}_pread_ms"), self.pread_ms.load(Relaxed) as i64);
        crate::debug::emit_counter(&format!("{prefix}_decompress_ms"), self.decompress_ms.load(Relaxed) as i64);
        crate::debug::emit_counter(&format!("{prefix}_reframe_ms"), self.reframe_ms.load(Relaxed) as i64);
        crate::debug::emit_counter(&format!("{prefix}_bucket_emit_ms"), self.bucket_emit_ms.load(Relaxed) as i64);
        crate::debug::emit_counter(&format!("{prefix}_send_ms"), self.send_ms.load(Relaxed) as i64);
        crate::debug::emit_counter(&format!("{prefix}_consumer_write_ms"), self.consumer_write_ms.load(Relaxed) as i64);
        crate::debug::emit_counter(&format!("{prefix}_blobs"), self.blobs.load(Relaxed) as i64);
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn pass1_worker(
    rx: &std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<&BlobTask>>>,
    base_new_ids: &[i64],
    shared_file: &std::sync::Arc<std::fs::File>,
    id_set: &mut super::id_set_dense::IdSetDense,
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
    // Reusable scratch for reframe_dense_with_new_ids.
    let mut group_ranges_scratch: Vec<(usize, usize)> = Vec::new();
    let mut scalar_fields_scratch: Vec<u8> = Vec::new();
    let mut other_fields_scratch: Vec<u8> = Vec::new();
    let mut new_id_packed_scratch: Vec<u8> = Vec::new();
    let mut dense_out_scratch: Vec<u8> = Vec::new();
    let mut group_out_scratch: Vec<u8> = Vec::new();

    loop {
        let task = {
            let guard = rx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match guard.recv() {
                Ok(t) => t,
                Err(_) => break,
            }
        };

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
                task.min_id < 0,
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

            let index = crate::blob_index::BlobIndex {
                kind: crate::blob_index::ElemKind::Node,
                min_id: base_new_id,
                #[allow(clippy::cast_possible_wrap)]
                max_id: base_new_id + blob_node_count as i64 - 1,
                count: blob_node_count,
                bbox: task.bbox,
            };
            output_blocks.clear();
            output_blocks.push((std::mem::take(&mut reframe_buf), index, None));

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

// ---------------------------------------------------------------------------
// DenseNodes wire-format rewriter for pass 1
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
/// Reframe a decompressed PrimitiveBlock by replacing only the DenseNodes
/// ID deltas while copying everything else (string table, lat/lon, tags,
/// metadata) verbatim at the byte level.
///
/// This is the renumber-specific fast path: renumber only changes IDs,
/// so we avoid the full decode→BlockBuilder→re-encode cycle. Per-node
/// cost drops from ~113 ns (HashMap lookups, delta arrays, metadata) to
/// ~10-15 ns (varint decode of old ID + varint encode of new delta).
///
/// Returns the number of nodes in the block. Sets old node IDs
/// directly in `id_set` as they are decoded.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn reframe_dense_with_new_ids(
    decompressed: &[u8],
    base_new_id: i64,
    id_set: &mut super::id_set_dense::IdSetDense,
    check_negative_ids: bool,
    output: &mut Vec<u8>,
    // Reusable scratch buffers — hoisted to worker level.
    group_ranges_scratch: &mut Vec<(usize, usize)>,
    scalar_fields_scratch: &mut Vec<u8>,
    other_fields_scratch: &mut Vec<u8>,
    new_id_packed_scratch: &mut Vec<u8>,
    dense_out_scratch: &mut Vec<u8>,
    group_out_scratch: &mut Vec<u8>,
) -> std::result::Result<u64, String> {
    use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

    group_ranges_scratch.clear();
    scalar_fields_scratch.clear();
    let mut stringtable_range: Option<(usize, usize)> = None;

    let mut cursor = Cursor::new(decompressed);
    while let Some((field, wire_type)) = cursor.read_tag().map_err(|e| e.to_string())? {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(|e| e.to_string())?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                stringtable_range = Some((offset, data.len()));
            }
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(|e| e.to_string())?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                group_ranges_scratch.push((offset, data.len()));
            }
            (17..=20, WIRE_VARINT) => {
                let raw = cursor.read_raw_field(wire_type).map_err(|e| e.to_string())?;
                protohoggr::encode_tag(scalar_fields_scratch, field, wire_type);
                scalar_fields_scratch.extend_from_slice(raw);
            }
            _ => cursor.skip_field(wire_type).map_err(|e| e.to_string())?,
        }
    }

    let (st_offset, st_len) = stringtable_range
        .ok_or("reframe: no StringTable in PrimitiveBlock")?;
    if group_ranges_scratch.is_empty() {
        return Err("reframe: no PrimitiveGroup in PrimitiveBlock".into());
    }
    let stringtable_bytes = &decompressed[st_offset..st_offset + st_len];

    // Phase 2-5: process each PrimitiveGroup, reframing its DenseNodes.
    output.clear();

    // PrimitiveBlock field 1: StringTable (copy verbatim)
    protohoggr::encode_bytes_field(output, 1, stringtable_bytes);

    let mut total_nodes: u64 = 0;
    let mut current_new_id = base_new_id;

    for &(gr_offset, gr_len) in group_ranges_scratch.iter() {
        let group_bytes = &decompressed[gr_offset..gr_offset + gr_len];

        let mut dense_data: Option<&[u8]> = None;
        let mut gr_cursor = Cursor::new(group_bytes);
        while let Some((field, wire_type)) = gr_cursor.read_tag().map_err(|e| e.to_string())? {
            if field == 2 && wire_type == WIRE_LEN {
                dense_data = Some(gr_cursor.read_len_delimited().map_err(|e| e.to_string())?);
            } else {
                gr_cursor.skip_field(wire_type).map_err(|e| e.to_string())?;
            }
        }

        let dense_bytes = dense_data.ok_or("reframe: no DenseNodes in PrimitiveGroup")?;

        let mut id_field: Option<&[u8]> = None;
        other_fields_scratch.clear();

        let mut dn_cursor = Cursor::new(dense_bytes);
        while let Some((field, wire_type)) = dn_cursor.read_tag().map_err(|e| e.to_string())? {
            if field == 1 && wire_type == WIRE_LEN {
                id_field = Some(dn_cursor.read_len_delimited().map_err(|e| e.to_string())?);
            } else {
                let raw = dn_cursor.read_raw_field(wire_type).map_err(|e| e.to_string())?;
                protohoggr::encode_tag(other_fields_scratch, field, wire_type);
                other_fields_scratch.extend_from_slice(raw);
            }
        }

        let id_bytes = id_field.ok_or("reframe: no packed ID field in DenseNodes")?;

        // Decode old ID deltas → absolute old IDs. Set bits in
        // id_set inline — no intermediate Vec.
        let mut old_id: i64 = 0;
        let mut id_cursor = Cursor::new(id_bytes);
        let mut group_node_count: u64 = 0;
        while id_cursor.remaining() > 0 {
            let delta = id_cursor.read_sint64().map_err(|e| e.to_string())?;
            old_id += delta;
            if check_negative_ids && old_id < 0 {
                return Err(format!(
                    "renumber --mode external requires non-negative input ids. \
                     Input contains node id {old_id}. \
                     Use --mode inmem for files with negative (editor-local) ids."
                ));
            }
            id_set.set(old_id);
            group_node_count += 1;
        }
        total_nodes += group_node_count;

        // Build new packed ID field for this group.
        let gnc = usize::try_from(group_node_count)
            .map_err(|_| "group node count > usize")?;
        new_id_packed_scratch.clear();
        protohoggr::encode_varint(
            new_id_packed_scratch,
            protohoggr::zigzag_encode_64(current_new_id),
        );
        new_id_packed_scratch.extend(std::iter::repeat_n(0x02u8, gnc.saturating_sub(1)));
        #[allow(clippy::cast_possible_wrap)]
        {
            current_new_id += group_node_count as i64;
        }

        dense_out_scratch.clear();
        protohoggr::encode_bytes_field(dense_out_scratch, 1, new_id_packed_scratch);
        dense_out_scratch.extend_from_slice(other_fields_scratch);

        group_out_scratch.clear();
        protohoggr::encode_bytes_field(group_out_scratch, 2, dense_out_scratch);
        protohoggr::encode_bytes_field(output, 2, group_out_scratch);
    }

    output.extend_from_slice(scalar_fields_scratch);

    Ok(total_nodes)
}

/// Reframe a decompressed way-blob PrimitiveBlock by replacing way IDs
/// and node refs while copying everything else verbatim.
///
/// For each way: decode old way id (for bucket emission), assign new
/// sequential way id, look up each ref's new node id from the new_refs
/// mmap at the appropriate slot position, delta-encode the new refs,
/// and copy keys/vals/info/lat/lon raw bytes verbatim.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn reframe_ways_with_new_ids(
    decompressed: &[u8],
    base_new_way_id: i64,
    node_id_set: &super::id_set_dense::IdSetDense,
    start_node_id: i64,
    way_id_set: &mut super::id_set_dense::IdSetDense,
    output: &mut Vec<u8>,
    refs_scratch: &mut Vec<u8>,
    group_scratch: &mut Vec<u8>,
    reframed_way_scratch: &mut Vec<u8>,
    check_negative_ids: bool,
    group_ranges_scratch: &mut Vec<(usize, usize)>,
    scalar_fields_scratch: &mut Vec<u8>,
) -> std::result::Result<u64, String> {
    use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

    group_ranges_scratch.clear();
    scalar_fields_scratch.clear();
    let mut stringtable_range: Option<(usize, usize)> = None;

    let mut cursor = Cursor::new(decompressed);
    while let Some((field, wire_type)) = cursor.read_tag().map_err(|e| e.to_string())? {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(|e| e.to_string())?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                stringtable_range = Some((offset, data.len()));
            }
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(|e| e.to_string())?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                group_ranges_scratch.push((offset, data.len()));
            }
            _ => {
                let raw = cursor.read_raw_field(wire_type).map_err(|e| e.to_string())?;
                protohoggr::encode_tag(scalar_fields_scratch, field, wire_type);
                scalar_fields_scratch.extend_from_slice(raw);
            }
        }
    }

    let (st_offset, st_len) = stringtable_range
        .ok_or("reframe_ways: no StringTable in PrimitiveBlock")?;
    let stringtable_bytes = &decompressed[st_offset..st_offset + st_len];

    output.clear();
    protohoggr::encode_bytes_field(output, 1, stringtable_bytes);

    let mut total_ways: u64 = 0;
    let mut current_new_id = base_new_way_id;

    for &(gr_offset, gr_len) in group_ranges_scratch.iter() {
        let group_bytes = &decompressed[gr_offset..gr_offset + gr_len];
        group_scratch.clear();

        let mut gr_cursor = Cursor::new(group_bytes);
        while let Some((field, wire_type)) = gr_cursor.read_tag().map_err(|e| e.to_string())? {
            if field == 3 && wire_type == WIRE_LEN {
                // Way submessage — splice-reframe it.
                // Find byte positions of field 1 (id) and field 8 (refs)
                // in way_bytes. Everything else is copied as contiguous
                // verbatim byte ranges — no per-field parse+re-encode.
                let way_bytes = gr_cursor.read_len_delimited().map_err(|e| e.to_string())?;

                // (tag_start, value_end) for fields we're replacing.
                let mut id_range: Option<(usize, usize)> = None;
                let mut refs_range: Option<(usize, usize)> = None;
                let mut old_way_id: i64 = 0;
                let mut old_refs_data: &[u8] = &[];

                let mut way_cursor = Cursor::new(way_bytes);
                while let Some((wf, wt)) = way_cursor.read_tag().map_err(|e| e.to_string())? {
                    // tag_start = position before read_raw_field consumed the value
                    let val_start = way_bytes.len() - way_cursor.remaining();
                    if wf == 1 && wt == WIRE_VARINT {
                        let tag_start = val_start - 1; // field 1 varint tag = 1 byte
                        old_way_id = way_cursor.read_varint_i64().map_err(|e| e.to_string())?;
                        let val_end = way_bytes.len() - way_cursor.remaining();
                        id_range = Some((tag_start, val_end));
                    } else if wf == 8 && wt == WIRE_LEN {
                        let tag_start = val_start - 1; // field 8 varint tag = 1 byte
                        old_refs_data = way_cursor.read_len_delimited().map_err(|e| e.to_string())?;
                        let val_end = way_bytes.len() - way_cursor.remaining();
                        refs_range = Some((tag_start, val_end));
                    } else {
                        way_cursor.read_raw_field(wt).map_err(|e| e.to_string())?;
                    }
                }

                if check_negative_ids && old_way_id < 0 {
                    return Err(format!(
                        "renumber --mode external requires non-negative input ids. \
                         Input contains way id {old_way_id}. \
                         Use --mode inmem for files with negative (editor-local) ids."
                    ));
                }
                way_id_set.set(old_way_id);

                // Decode old ref deltas, resolve via rank(), delta-encode new refs.
                refs_scratch.clear();
                let mut prev_old_ref: i64 = 0;
                let mut prev_new_ref: i64 = 0;
                let mut refs_cursor = protohoggr::Cursor::new(old_refs_data);
                while !refs_cursor.is_empty() {
                    let raw = refs_cursor.read_varint().map_err(|e| e.to_string())?;
                    let delta = protohoggr::zigzag_decode_64(raw);
                    prev_old_ref += delta;
                    let old_node_id = prev_old_ref;
                    if old_node_id < 0 {
                        return Err(format!(
                            "renumber --mode external requires non-negative \
                             input ids. Way references negative node id \
                             {old_node_id}. Use --mode inmem for files with \
                             negative (editor-local) ids."
                        ));
                    }
                    let new_ref = node_id_set.resolve(old_node_id, start_node_id);
                    protohoggr::encode_varint(
                        refs_scratch,
                        protohoggr::zigzag_encode_64(new_ref - prev_new_ref),
                    );
                    prev_new_ref = new_ref;
                }

                // Splice: emit way_bytes with id and refs replaced.
                // Sort the two replacement ranges by start position to
                // handle any field order in the wire format.
                let id_r = id_range.ok_or("reframe_ways: no id field")?;
                let refs_r = refs_range.ok_or("reframe_ways: no refs field")?;
                let (first, second) = if id_r.0 < refs_r.0 {
                    (id_r, refs_r)
                } else {
                    (refs_r, id_r)
                };

                reframed_way_scratch.clear();
                // Bytes before first replaced field.
                reframed_way_scratch.extend_from_slice(&way_bytes[..first.0]);
                // First replacement.
                if first.0 == id_r.0 {
                    protohoggr::encode_int64_field(reframed_way_scratch, 1, current_new_id);
                } else {
                    protohoggr::encode_bytes_field(reframed_way_scratch, 8, refs_scratch);
                }
                // Bytes between first and second replaced fields.
                reframed_way_scratch.extend_from_slice(&way_bytes[first.1..second.0]);
                // Second replacement.
                if second.0 == refs_r.0 {
                    protohoggr::encode_bytes_field(reframed_way_scratch, 8, refs_scratch);
                } else {
                    protohoggr::encode_int64_field(reframed_way_scratch, 1, current_new_id);
                }
                // Bytes after second replaced field.
                reframed_way_scratch.extend_from_slice(&way_bytes[second.1..]);

                protohoggr::encode_bytes_field(group_scratch, 3, reframed_way_scratch);

                current_new_id += 1;
                total_ways += 1;
            } else {
                // Non-way field in the group — copy verbatim.
                let raw = gr_cursor.read_raw_field(wire_type).map_err(|e| e.to_string())?;
                protohoggr::encode_tag(group_scratch, field, wire_type);
                group_scratch.extend_from_slice(raw);
            }
        }

        protohoggr::encode_bytes_field(output, 2, group_scratch);
    }

    output.extend_from_slice(scalar_fields_scratch);

    Ok(total_ways)
}

// ---------------------------------------------------------------------------
// Wire-format relation rewriter
// ---------------------------------------------------------------------------

/// Wire-format splice rewriter for relations. Patches field 1 (id) and
/// field 9 (memids) in each Relation submessage; copies all other fields
/// (keys, vals, info, roles_sid, types) verbatim as raw bytes.
///
/// The memids field (packed sint64, delta-encoded) interleaves node, way,
/// and relation member IDs in one stream. Field 10 (types, packed int32)
/// tells us which lookup to use for each member:
///   0 = node (read from node_mmap, advance node cursor)
///   1 = way  (read from way_mmap, advance way cursor)
///   2 = relation (look up in relation_map)
///   other = unknown (preserve old absolute ID unchanged)
///
/// One `prev_new_id` accumulator tracks across ALL member types — the
/// delta encoding is over the interleaved stream, not per-type.
///
/// Returns `(relation_count, min_new_id, max_new_id)`.
#[allow(clippy::too_many_arguments, clippy::too_many_lines, clippy::cast_possible_truncation)]
fn reframe_relations_with_new_ids(
    decompressed: &[u8],
    relation_map: &FxHashMap<i64, i64>,
    node_id_set: &super::id_set_dense::IdSetDense,
    start_node_id: i64,
    way_id_set: &super::id_set_dense::IdSetDense,
    start_way_id: i64,
    output: &mut Vec<u8>,
    memids_scratch: &mut Vec<u8>,
    group_scratch: &mut Vec<u8>,
    reframed_rel_scratch: &mut Vec<u8>,
    group_ranges_scratch: &mut Vec<(usize, usize)>,
    scalar_fields_scratch: &mut Vec<u8>,
) -> std::result::Result<(u64, i64, i64), String> {
    use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

    // ---- Level 1: PrimitiveBlock ----
    group_ranges_scratch.clear();
    scalar_fields_scratch.clear();
    let mut stringtable_range: Option<(usize, usize)> = None;

    let mut cursor = Cursor::new(decompressed);
    while let Some((field, wire_type)) = cursor.read_tag().map_err(|e| e.to_string())? {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(|e| e.to_string())?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                stringtable_range = Some((offset, data.len()));
            }
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(|e| e.to_string())?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                group_ranges_scratch.push((offset, data.len()));
            }
            _ => {
                let raw = cursor.read_raw_field(wire_type).map_err(|e| e.to_string())?;
                protohoggr::encode_tag(scalar_fields_scratch, field, wire_type);
                scalar_fields_scratch.extend_from_slice(raw);
            }
        }
    }

    let (st_offset, st_len) = stringtable_range
        .ok_or("reframe_relations: no StringTable in PrimitiveBlock")?;
    let stringtable_bytes = &decompressed[st_offset..st_offset + st_len];

    output.clear();
    protohoggr::encode_bytes_field(output, 1, stringtable_bytes);

    let mut total_relations: u64 = 0;
    let mut min_new_id: i64 = i64::MAX;
    let mut max_new_id: i64 = i64::MIN;

    // ---- Level 2: PrimitiveGroup ----
    for &(gr_offset, gr_len) in group_ranges_scratch.iter() {
        let group_bytes = &decompressed[gr_offset..gr_offset + gr_len];
        group_scratch.clear();

        let mut gr_cursor = Cursor::new(group_bytes);
        while let Some((field, wire_type)) = gr_cursor.read_tag().map_err(|e| e.to_string())? {
            if field == 4 && wire_type == WIRE_LEN {
                // Relation submessage — splice-reframe it.
                let rel_bytes = gr_cursor.read_len_delimited().map_err(|e| e.to_string())?;

                // Scan relation fields to find byte ranges for id and memids.
                let mut id_range: Option<(usize, usize)> = None;
                let mut memids_range: Option<(usize, usize)> = None;
                let mut old_rel_id: i64 = 0;
                let mut old_memids_data: &[u8] = &[];
                let mut types_data: &[u8] = &[];

                let mut rel_cursor = Cursor::new(rel_bytes);
                while let Some((rf, rt)) = rel_cursor.read_tag().map_err(|e| e.to_string())? {
                    let val_start = rel_bytes.len() - rel_cursor.remaining();
                    match (rf, rt) {
                        (1, WIRE_VARINT) => {
                            let tag_start = val_start - 1; // field 1 tag = 0x08, 1 byte
                            old_rel_id = rel_cursor.read_varint_i64().map_err(|e| e.to_string())?;
                            let val_end = rel_bytes.len() - rel_cursor.remaining();
                            id_range = Some((tag_start, val_end));
                        }
                        (9, WIRE_LEN) => {
                            let tag_start = val_start - 1; // field 9 tag = 0x4A, 1 byte
                            old_memids_data = rel_cursor.read_len_delimited().map_err(|e| e.to_string())?;
                            let val_end = rel_bytes.len() - rel_cursor.remaining();
                            memids_range = Some((tag_start, val_end));
                        }
                        (10, WIRE_LEN) => {
                            types_data = rel_cursor.read_len_delimited().map_err(|e| e.to_string())?;
                            // Not patched — just captured for dispatch.
                        }
                        _ => {
                            rel_cursor.read_raw_field(rt).map_err(|e| e.to_string())?;
                        }
                    }
                }

                // Look up new relation id.
                let new_rel_id = relation_map.get(&old_rel_id).copied().ok_or_else(|| {
                    format!("reframe_relations: relation id {old_rel_id} missing from relation_map")
                })?;

                if new_rel_id < min_new_id {
                    min_new_id = new_rel_id;
                }
                if new_rel_id > max_new_id {
                    max_new_id = new_rel_id;
                }

                // ---- Patch memids: decode old deltas + types, look up new ids, re-encode ----
                memids_scratch.clear();

                if !old_memids_data.is_empty() || !types_data.is_empty() {
                    // Validate: both must have the same varint count.
                    let memids_count = old_memids_data.iter().filter(|&&b| b & 0x80 == 0).count();
                    let types_count = types_data.iter().filter(|&&b| b & 0x80 == 0).count();
                    if memids_count != types_count {
                        return Err(format!(
                            "reframe_relations: relation {old_rel_id} has {memids_count} memids \
                             but {types_count} types"
                        ));
                    }

                    let mut memids_cursor = Cursor::new(old_memids_data);
                    let mut types_cursor = Cursor::new(types_data);
                    let mut prev_old_id: i64 = 0;
                    let mut prev_new_id: i64 = 0;

                    for _ in 0..memids_count {
                        // Decode member type.
                        let member_type = types_cursor
                            .read_varint()
                            .map_err(|e| format!("types varint: {e}"))?;

                        // Decode old memid delta → absolute old id.
                        let raw_delta = memids_cursor
                            .read_varint()
                            .map_err(|e| format!("memids varint: {e}"))?;
                        let delta = protohoggr::zigzag_decode_64(raw_delta);
                        prev_old_id += delta;
                        let old_abs_id = prev_old_id;

                        // Look up new absolute id by member type.
                        let new_abs_id = match member_type {
                            0 => node_id_set.resolve(old_abs_id, start_node_id),
                            1 => way_id_set.resolve(old_abs_id, start_way_id),
                            2 => relation_map.get(&old_abs_id).copied().unwrap_or(old_abs_id),
                            _ => old_abs_id, // unknown type — preserve
                        };

                        // Delta-encode the new id.
                        protohoggr::encode_varint(
                            memids_scratch,
                            protohoggr::zigzag_encode_64(new_abs_id - prev_new_id),
                        );
                        prev_new_id = new_abs_id;
                    }
                }

                // ---- Splice: emit rel_bytes with id and memids replaced ----
                let id_r = id_range.ok_or_else(|| {
                    format!("reframe_relations: no id field in relation {old_rel_id}")
                })?;

                reframed_rel_scratch.clear();

                if let Some(memids_r) = memids_range {
                    // Two replacement fields — sort by position, splice.
                    let (first, second) = if id_r.0 < memids_r.0 {
                        (id_r, memids_r)
                    } else {
                        (memids_r, id_r)
                    };

                    // Bytes before first replaced field.
                    reframed_rel_scratch.extend_from_slice(&rel_bytes[..first.0]);
                    // First replacement.
                    if first.0 == id_r.0 {
                        protohoggr::encode_int64_field(reframed_rel_scratch, 1, new_rel_id);
                    } else {
                        protohoggr::encode_bytes_field(reframed_rel_scratch, 9, memids_scratch);
                    }
                    // Bytes between first and second replaced fields.
                    reframed_rel_scratch.extend_from_slice(&rel_bytes[first.1..second.0]);
                    // Second replacement.
                    if second.0 == memids_r.0 {
                        protohoggr::encode_bytes_field(reframed_rel_scratch, 9, memids_scratch);
                    } else {
                        protohoggr::encode_int64_field(reframed_rel_scratch, 1, new_rel_id);
                    }
                    // Bytes after second replaced field.
                    reframed_rel_scratch.extend_from_slice(&rel_bytes[second.1..]);
                } else {
                    // No memids field (zero-member relation) — only patch id.
                    reframed_rel_scratch.extend_from_slice(&rel_bytes[..id_r.0]);
                    protohoggr::encode_int64_field(reframed_rel_scratch, 1, new_rel_id);
                    reframed_rel_scratch.extend_from_slice(&rel_bytes[id_r.1..]);
                }

                protohoggr::encode_bytes_field(group_scratch, 4, reframed_rel_scratch);
                total_relations += 1;
            } else {
                // Non-relation field in the group — drop it to match
                // current R2d behavior (only relations are emitted).
                gr_cursor.read_raw_field(wire_type).map_err(|e| e.to_string())?;
            }
        }

        if !group_scratch.is_empty() {
            protohoggr::encode_bytes_field(output, 2, group_scratch);
        }
    }

    output.extend_from_slice(scalar_fields_scratch);

    Ok((total_relations, min_new_id, max_new_id))
}

// ---------------------------------------------------------------------------
// Fused relation resolve: R1 + R2A + R2B in one pass
// ---------------------------------------------------------------------------

/// R1 pass: assign sequential relation IDs and build the in-memory
/// `relation_map`. No member ref resolution — that's done inline by
/// R2d's wire-format rewriter via `resolve()`.
fn relation_r1_assign_ids(
    input: &Path,
    relation_schedule: &[BlobTask],
    relation_map: &mut FxHashMap<i64, i64>,
    next_relation_id: &mut i64,
) -> Result<()> {
    let shared_file = std::fs::File::open(input)
        .map_err(|e| format!("failed to open {}: {e}", input.display()))?;

    let pool = crate::blob::DecompressPool::new();
    let mut raw_buf: Vec<u8> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    use std::os::unix::fs::FileExt;
    for task in relation_schedule {
        raw_buf.resize(task.data_size, 0);
        shared_file
            .read_exact_at(&mut raw_buf, task.data_offset)
            .map_err(|e| format!("failed to pread relation blob at {}: {e}", task.data_offset))?;
        let mut decompress_buf = pool.get();
        crate::blob::decompress_blob_raw(&raw_buf, &mut decompress_buf)?;
        let block = crate::block::PrimitiveBlock::from_vec_pooled_with_scratch(
            decompress_buf,
            &pool,
            &mut st_scratch,
            &mut gr_scratch,
        )?;
        for element in block.elements() {
            if let Element::Relation(r) = &element {
                reject_negative_id(r.id(), "relation")?;
                let new_id = *next_relation_id;
                *next_relation_id += 1;
                relation_map.insert(r.id(), new_id);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Relation pass R2d: parallel wire-format rewrite + write output
// ---------------------------------------------------------------------------

/// Parallel R2d: wire-format splice rewriter for relation blobs.
/// Work-stealing dispatch with ReorderBuffer, same pattern as pass 1
/// and stage 2d. Each worker resolves node/way member refs inline via
/// `resolve()` — no flat files, no mmaps, no sidecar.
#[hotpath::measure]
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation
)]
fn relation_r2d_assembly(
    input: &Path,
    relation_schedule: &[BlobTask],
    writer: &mut crate::writer::PbfWriter<crate::write::file_writer::FileWriter>,
    node_id_set: &super::id_set_dense::IdSetDense,
    start_node_id: i64,
    way_id_set: &super::id_set_dense::IdSetDense,
    start_way_id: i64,
    relation_map: &FxHashMap<i64, i64>,
    stats: &mut RenumberStats,
) -> Result<()> {

    if relation_schedule.is_empty() {
        return Ok(());
    }

    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?,
    );

    let decode_threads = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4)
        .min(6);

    // Each blob produces one OwnedBlock tuple.
    type R2dItem = (usize, std::result::Result<(Vec<u8>, crate::blob_index::BlobIndex, u64), String>);
    let (desc_tx, desc_rx) =
        std::sync::mpsc::sync_channel::<(usize, u64, usize)>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (decoded_tx, decoded_rx) = std::sync::mpsc::sync_channel::<R2dItem>(32);

    let rels_written = std::sync::atomic::AtomicU64::new(0);

    std::thread::scope(|scope| -> Result<()> {
        // Dispatcher thread — sends (seq, data_offset, data_size, node_start, way_start).
        let sched = relation_schedule;
        scope.spawn(move || {
            for (i, task) in sched.iter().enumerate() {
                if desc_tx.send((i, task.data_offset, task.data_size)).is_err() {
                    break;
                }
            }
        });

        // Worker threads.
        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let file = std::sync::Arc::clone(&shared_file);
            let tx = decoded_tx.clone();
            let rmap = relation_map;
            let rw = &rels_written;
            scope.spawn(move || {
                use std::os::unix::fs::FileExt as _;
                let mut read_buf: Vec<u8> = Vec::new();
                let mut decompress_buf: Vec<u8> = Vec::new();
                let mut reframe_buf: Vec<u8> = Vec::new();
                let mut memids_scratch: Vec<u8> = Vec::new();
                let mut group_scratch: Vec<u8> = Vec::new();
                let mut reframed_rel_scratch: Vec<u8> = Vec::new();
                let mut group_ranges: Vec<(usize, usize)> = Vec::new();
                let mut scalar_fields: Vec<u8> = Vec::new();

                loop {
                    let (seq, data_offset, data_size) = {
                        let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match guard.recv() {
                            Ok(msg) => msg,
                            Err(_) => break,
                        }
                    };

                    let result: std::result::Result<(Vec<u8>, crate::blob_index::BlobIndex, u64), String> = (|| {
                        read_buf.resize(data_size, 0);
                        file.read_exact_at(&mut read_buf, data_offset)
                            .map_err(|e| format!("pread at {data_offset}: {e}"))?;
                        crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)
                            .map_err(|e| e.to_string())?;

                        let (blob_count, min_id, max_id) = reframe_relations_with_new_ids(
                            &decompress_buf,
                            rmap,
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

                        rw.fetch_add(blob_count, std::sync::atomic::Ordering::Relaxed);

                        let index = crate::blob_index::BlobIndex {
                            kind: crate::blob_index::ElemKind::Relation,
                            min_id,
                            max_id,
                            count: blob_count,
                            bbox: None,
                        };
                        Ok((std::mem::take(&mut reframe_buf), index, blob_count))
                    })();

                    if tx.send((seq, result)).is_err() {
                        break;
                    }
                }
            });
        }

        drop(decoded_tx);
        drop(desc_rx);

        // Consumer: reorder by seq, write to output in file order.
        let mut reorder: crate::reorder_buffer::ReorderBuffer<
            std::result::Result<(Vec<u8>, crate::blob_index::BlobIndex, u64), String>,
        > = crate::reorder_buffer::ReorderBuffer::with_capacity(64);

        for (seq_num, item) in decoded_rx {
            reorder.push(seq_num, item);
            while let Some(result) = reorder.pop_ready() {
                let (block_bytes, index, _count) =
                    result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                if index.count > 0 {
                    writer.write_primitive_block_owned(block_bytes, index, None)?;
                }
            }
        }
        Ok(())
    })?;

    stats.relations_written += rels_written.load(std::sync::atomic::Ordering::Relaxed);

    Ok(())
}

