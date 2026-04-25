//! Stage 1: Two-pass way scan + node-blob rank mapping.
//!
//!   Pass A: build IdSet of referenced node IDs (parallel).
//!   Pass B: emit rank-bucketed (local_rank, slot_pos) records (parallel).
//!   Node blob mapping: header-only walk of node blobs that records each
//!     blob's referenced-rank range using IdSet::rank queries on the
//!     blob's indexdata `(min_id, max_id)`. Replaces the historical 82 GB
//!     `coords_by_rank` file - stage 2 reads node blobs directly.

use std::io::{BufWriter, Write as _};
use std::path::Path;

use super::radix::{ScratchDir, NUM_BUCKETS};
use crate::idset::IdSet;
use super::super::Result;
use super::blob_meta::BlobMeta;
use super::{
    BucketLayout, IdRecord, NodeBlobInfo, RankRecord, ID_RECORD_SIZE, MAX_NODE_ID,
    RANK_RECORD_SIZE,
};

/// Way-blob schedule entry for the parallel way scans.
pub(super) struct WayBlobTask {
    pub(super) seq: u32,
    pub(super) data_offset: u64,
    pub(super) data_size: usize,
}

/// Stage 1 output handed to stage 2. Owns the `IdSet` (kept alive
/// because stage 2 needs `rank_if_set` for inline node-blob coord
/// resolution) and the per-blob rank mapping.
pub(super) struct Stage1Output {
    pub total_slots: u64,
    pub unique_nodes: u64,
    pub rank_bucket_counts: Vec<u64>,
    /// Per-(id-bucket) IdRecord counts produced by pass A. Used by
    /// stage 2 (A1 step 3+) to skip empty buckets without a probe.
    /// Same length as `rank_bucket_counts`. Populated alongside the
    /// existing rank-bucket emission for the duration of the dual-path
    /// window (A1 steps 2-3); step 4 deletes `rank_bucket_counts`.
    #[allow(dead_code)] // wired up in step 3 (stage 2 ID-bucket consumer).
    pub id_bucket_counts: Vec<u64>,
    pub num_shard_workers: usize,
    pub node_id_set: IdSet,
    pub node_blob_mapping: Vec<NodeBlobInfo>,
}

/// Build the way-blob schedule from the shared blob metadata scan.
pub(super) fn build_way_schedule(blob_meta: &[BlobMeta]) -> Result<Vec<WayBlobTask>> {
    crate::debug::emit_marker("EXTJOIN_S1_WAY_SCHEDULE_START");
    let t0 = std::time::Instant::now();
    let mut schedule: Vec<WayBlobTask> = Vec::new();
    let mut seq: u32 = 0;
    for meta in blob_meta {
        if !matches!(meta.kind, crate::blob_meta::ElemKind::Way) {
            continue;
        }
        schedule.push(WayBlobTask {
            seq,
            data_offset: meta.data_offset,
            data_size: meta.data_size,
        });
        seq += 1;
    }
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    {
        crate::debug::emit_counter("s1_way_schedule_blobs", schedule.len() as i64);
        crate::debug::emit_counter("s1_way_schedule_build_ms", t0.elapsed().as_millis() as i64);
    }
    crate::debug::emit_marker("EXTJOIN_S1_WAY_SCHEDULE_END");
    Ok(schedule)
}

/// Pass A standalone: parallel way scan to build `IdSet` of all
/// referenced node IDs and write the two ref-count sidecars in blob order.
///
/// Returns `(total_refs, IdSet, id_bucket_counts)` with
/// `build_rank_index()` already called. Used by `stage1_way_pass` as
/// the entry into stage 1. `id_bucket_counts[k]` is the number of
/// IdRecords pass A wrote to ID-bucket `k` across all workers; stage
/// 2 reads it to skip empty buckets.
#[hotpath::measure]
#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
pub(super) fn stage1_pass_a(
    input: &Path,
    schedule: &[WayBlobTask],
    num_workers: usize,
    scratch: &ScratchDir,
    layout: &BucketLayout,
    ref_count_sidecar: &Path,
    per_way_refcount_sidecar: &Path,
) -> Result<(u64, IdSet, Vec<u64>)> {
    use std::os::unix::fs::FileExt as _;

    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?,
    );

    crate::debug::emit_marker("EXTJOIN_S1_PASS_A_START");

    let mut node_id_set = IdSet::new();
    // Pre-allocate for planet-scale node IDs (~13B max).
    #[allow(clippy::cast_possible_wrap)]
    node_id_set.pre_allocate(MAX_NODE_ID as i64);

    let s1a_pread_ms = std::sync::atomic::AtomicU64::new(0);
    let s1a_decompress_ms = std::sync::atomic::AtomicU64::new(0);
    let s1a_scan_way_refs_ms = std::sync::atomic::AtomicU64::new(0);
    let s1a_idset_set_ms = std::sync::atomic::AtomicU64::new(0);
    let s1a_bytes_read = std::sync::atomic::AtomicU64::new(0);
    let s1a_pread_calls = std::sync::atomic::AtomicU64::new(0);

    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let mut total_refs: u64 = 0;

    let s1a_per_way_sidecar_bytes = std::sync::atomic::AtomicU64::new(0);
    // A1 step 2 counters: per-ref IdRecord emission to id-bucket
    // shards. Records that can't be encoded (negative ref or
    // node_id > max_node_id) are skipped; those slots fall through to
    // stage 4 as zero-coord missing locations, matching the existing
    // missing-ref behaviour pinned by `missing_node_refs_get_zero_coordinates`.
    let s1a_id_emit_ms = std::sync::atomic::AtomicU64::new(0);
    let s1a_id_records_emitted = std::sync::atomic::AtomicU64::new(0);
    let s1a_id_records_skipped = std::sync::atomic::AtomicU64::new(0);
    let s1a_id_shard_bytes_written = std::sync::atomic::AtomicU64::new(0);
    let s1a_id_shard_flush_err: std::sync::Mutex<Option<String>> =
        std::sync::Mutex::new(None);
    // Per-(id-bucket) record counts. Stage 2 reads these via the
    // `Stage1Output` field to skip empty buckets without a probe.
    let s1a_id_bucket_counts: Vec<std::sync::atomic::AtomicU64> =
        (0..NUM_BUCKETS).map(|_| std::sync::atomic::AtomicU64::new(0)).collect();
    {
        type PassAItem = (u32, std::result::Result<(u64, Vec<u32>), String>);
        let (tx, rx) = std::sync::mpsc::sync_channel::<PassAItem>(32);
        let schedule_ref = schedule;
        let next_ref = &next_idx;
        let s1a_pread_ref = &s1a_pread_ms;
        let s1a_decompress_ref = &s1a_decompress_ms;
        let s1a_scan_ref = &s1a_scan_way_refs_ms;
        let s1a_idset_ref = &s1a_idset_set_ms;
        let s1a_bytes_ref = &s1a_bytes_read;
        let s1a_pread_calls_ref = &s1a_pread_calls;
        let s1a_per_way_bytes_ref = &s1a_per_way_sidecar_bytes;
        let s1a_id_emit_ref = &s1a_id_emit_ms;
        let s1a_id_emitted_ref = &s1a_id_records_emitted;
        let s1a_id_skipped_ref = &s1a_id_records_skipped;
        let s1a_id_bytes_ref = &s1a_id_shard_bytes_written;
        let s1a_id_flush_err_ref = &s1a_id_shard_flush_err;
        let s1a_id_bucket_counts_ref: &[std::sync::atomic::AtomicU64] =
            &s1a_id_bucket_counts;
        let layout_ref = layout;
        let scratch_ref = scratch;

        std::thread::scope(|scope| -> Result<()> {
            for worker_id in 0..num_workers {
                let file = std::sync::Arc::clone(&shared_file);
                let tx = tx.clone();
                let node_id_set_ref = &node_id_set;
                scope.spawn(move || {
                    use std::sync::atomic::Ordering::Relaxed;
                    let mut read_buf: Vec<u8> = Vec::new();
                    let mut decompress_buf: Vec<u8> = Vec::new();
                    let mut refs_buf: Vec<i64> = Vec::new();
                    let mut group_starts: Vec<(usize, usize)> = Vec::new();
                    // Lazy-initialised on first blob so creation errors
                    // flow into the per-blob IIFE result and propagate
                    // through the existing tx/rx channel.
                    let mut id_shard_writers: Vec<Option<BufWriter<std::fs::File>>> =
                        Vec::new();
                    let mut id_rec_buf = [0u8; ID_RECORD_SIZE];

                    loop {
                        let idx = next_ref.fetch_add(1, Relaxed);
                        if idx >= schedule_ref.len() { break; }
                        let task = &schedule_ref[idx];

                        let result: std::result::Result<(u64, Vec<u32>), String> = (|| {
                            let t0 = std::time::Instant::now();
                            read_buf.resize(task.data_size, 0);
                            file.read_exact_at(&mut read_buf, task.data_offset)
                                .map_err(|e| format!("pass A pread: {e}"))?;
                            #[allow(clippy::cast_possible_truncation)]
                            s1a_pread_ref.fetch_add(t0.elapsed().as_millis() as u64, Relaxed);
                            s1a_bytes_ref.fetch_add(task.data_size as u64, Relaxed);
                            s1a_pread_calls_ref.fetch_add(1, Relaxed);

                            let t1 = std::time::Instant::now();
                            crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)
                                .map_err(|e| format!("pass A decompress: {e}"))?;
                            #[allow(clippy::cast_possible_truncation)]
                            s1a_decompress_ref.fetch_add(t1.elapsed().as_millis() as u64, Relaxed);

                            let t2 = std::time::Instant::now();
                            let mut blob_node_ids: Vec<i64> = Vec::new();
                            let mut per_way_rcs: Vec<u32> = Vec::new();
                            crate::scan::way::scan_way_refs(
                                &decompress_buf, &mut refs_buf, &mut group_starts,
                                |_way_id, refs| {
                                    blob_node_ids.extend_from_slice(refs);
                                    #[allow(clippy::cast_possible_truncation)]
                                    per_way_rcs.push(refs.len() as u32);
                                },
                            ).map_err(|e| e.to_string())?;
                            #[allow(clippy::cast_possible_truncation)]
                            s1a_scan_ref.fetch_add(t2.elapsed().as_millis() as u64, Relaxed);

                            // A1 step 2: emit IdRecords per ref into
                            // id-bucket shards. Records for negative refs
                            // or out-of-range node ids (> max_node_id)
                            // are skipped; those slots stay unresolved
                            // and stage 4 fills zero coordinates.
                            let t_id = std::time::Instant::now();
                            if id_shard_writers.is_empty() {
                                id_shard_writers.reserve(NUM_BUCKETS);
                                for bucket_idx in 0..NUM_BUCKETS {
                                    let path = scratch_ref.path.join(
                                        format!("id-W{worker_id}-{bucket_idx:03}"),
                                    );
                                    let f = std::fs::File::create(&path).map_err(|e| {
                                        format!(
                                            "create id shard {}: {e}",
                                            path.display(),
                                        )
                                    })?;
                                    id_shard_writers.push(Some(BufWriter::with_capacity(
                                        super::radix::BUCKET_BUF_SIZE,
                                        f,
                                    )));
                                }
                            }
                            let mut blob_emitted: u64 = 0;
                            let mut blob_skipped: u64 = 0;
                            let mut blob_bytes: u64 = 0;
                            for (i, &node_id) in blob_node_ids.iter().enumerate() {
                                let blob_local_slot = u32::try_from(i).map_err(|_| {
                                    format!(
                                        "blob {} has > u32::MAX refs (i={i})",
                                        task.seq,
                                    )
                                })?;
                                let location = if node_id < 0 {
                                    None
                                } else {
                                    #[allow(clippy::cast_sign_loss)]
                                    layout_ref.locate(node_id as u64)
                                };
                                if let Some((bucket_idx, local_node_id)) = location {
                                    let rec = IdRecord {
                                        local_node_id,
                                        blob_idx: task.seq,
                                        blob_local_slot,
                                    };
                                    rec.write_to(&mut id_rec_buf);
                                    let writer = id_shard_writers[bucket_idx]
                                        .as_mut()
                                        .expect("shard writer initialised");
                                    writer.write_all(&id_rec_buf).map_err(|e| {
                                        format!("write id shard W{worker_id}: {e}")
                                    })?;
                                    s1a_id_bucket_counts_ref[bucket_idx]
                                        .fetch_add(1, Relaxed);
                                    blob_emitted += 1;
                                    blob_bytes += ID_RECORD_SIZE as u64;
                                } else {
                                    blob_skipped += 1;
                                }
                            }
                            #[allow(clippy::cast_possible_truncation)]
                            s1a_id_emit_ref.fetch_add(
                                t_id.elapsed().as_millis() as u64,
                                Relaxed,
                            );
                            s1a_id_emitted_ref.fetch_add(blob_emitted, Relaxed);
                            s1a_id_skipped_ref.fetch_add(blob_skipped, Relaxed);
                            s1a_id_bytes_ref.fetch_add(blob_bytes, Relaxed);

                            // Populate IdSet for the legacy rank path
                            // (still consumed by pass B / stage 2 until
                            // step 4). Apply the same locate-based skip
                            // filter as IdRecord emission so negative
                            // refs (which would land near u64::MAX after
                            // an `i64 as u64` cast and panic the
                            // pre_allocate'd IdSet) and out-of-range
                            // refs are handled consistently across both
                            // paths. The end result for skipped refs is
                            // identical: stage 4 fills zero coords. Cost
                            // of the double `locate` is bounded - it's a
                            // pair of integer ops on u64, dwarfed by the
                            // pread/decompress phases.
                            let t3 = std::time::Instant::now();
                            for &node_id in &blob_node_ids {
                                if node_id < 0 {
                                    continue;
                                }
                                #[allow(clippy::cast_sign_loss)]
                                if layout_ref.locate(node_id as u64).is_none() {
                                    continue;
                                }
                                node_id_set_ref.set_atomic(node_id);
                            }
                            #[allow(clippy::cast_possible_truncation)]
                            s1a_idset_ref.fetch_add(t3.elapsed().as_millis() as u64, Relaxed);

                            Ok((blob_node_ids.len() as u64, per_way_rcs))
                        })();

                        if tx.send((task.seq, result)).is_err() { break; }
                    }

                    // Flush id-shard writers before the worker thread
                    // exits. Drop would silently swallow flush errors,
                    // so do it explicitly and surface any failure via
                    // the shared mutex; the orchestrator checks after
                    // the scope joins.
                    for w in id_shard_writers.iter_mut().flatten() {
                        if let Err(e) = w.flush() {
                            let mut slot = s1a_id_flush_err_ref
                                .lock()
                                .expect("id shard flush error mutex");
                            if slot.is_none() {
                                *slot = Some(format!(
                                    "flush id shard W{worker_id}: {e}",
                                ));
                            }
                            break;
                        }
                    }
                });
            }
            drop(tx);

            let mut sidecar_writer = BufWriter::with_capacity(
                64 * 1024,
                std::fs::File::create(ref_count_sidecar)
                    .map_err(|e| format!("create sidecar: {e}"))?,
            );
            let mut per_way_writer = BufWriter::with_capacity(
                256 * 1024,
                std::fs::File::create(per_way_refcount_sidecar)
                    .map_err(|e| format!("create per-way sidecar: {e}"))?,
            );
            let mut varint_buf: Vec<u8> = Vec::with_capacity(1024);
            let mut reorder: crate::reorder_buffer::ReorderBuffer<
                std::result::Result<(u64, Vec<u32>), String>,
            > = crate::reorder_buffer::ReorderBuffer::with_capacity(64);

            for (seq_num, item) in rx {
                reorder.push(seq_num as usize, item);
                while let Some(result) = reorder.pop_ready() {
                    let (ref_count, per_way_rcs) =
                        result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                    sidecar_writer.write_all(&ref_count.to_le_bytes())?;
                    total_refs += ref_count;

                    varint_buf.clear();
                    protohoggr::encode_varint(&mut varint_buf, per_way_rcs.len() as u64);
                    for rc in &per_way_rcs {
                        protohoggr::encode_varint(&mut varint_buf, u64::from(*rc));
                    }
                    per_way_writer.write_all(&varint_buf)?;
                    s1a_per_way_bytes_ref.fetch_add(
                        varint_buf.len() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
            }
            sidecar_writer.write_all(&total_refs.to_le_bytes())?;
            sidecar_writer.flush()?;
            per_way_writer.flush()?;
            Ok(())
        })?;
    }

    // Surface any worker-side flush errors from the id-shard writers.
    let mut id_flush_slot = s1a_id_shard_flush_err
        .lock()
        .map_err(|e| format!("id shard flush mutex poisoned: {e}"))?;
    if let Some(err) = id_flush_slot.take() {
        return Err(err.into());
    }
    drop(id_flush_slot);

    node_id_set.build_rank_index();
    let unique_nodes = node_id_set.total_count();

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("s1a_pread_ms", s1a_pread_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1a_decompress_ms", s1a_decompress_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1a_scan_way_refs_ms", s1a_scan_way_refs_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1a_idset_set_ms", s1a_idset_set_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1a_bytes_read", s1a_bytes_read.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1a_pread_calls", s1a_pread_calls.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1a_unique_nodes", unique_nodes as i64);
        crate::debug::emit_counter("s1a_per_way_sidecar_bytes", s1a_per_way_sidecar_bytes.load(std::sync::atomic::Ordering::Relaxed) as i64);
        // A1 step 2 IdRecord emission counters.
        crate::debug::emit_counter("s1a_id_emit_ms", s1a_id_emit_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1a_id_records_emitted", s1a_id_records_emitted.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1a_id_records_skipped", s1a_id_records_skipped.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1a_id_shard_bytes_written", s1a_id_shard_bytes_written.load(std::sync::atomic::Ordering::Relaxed) as i64);
    }
    crate::debug::emit_marker("EXTJOIN_S1_PASS_A_END");

    let id_bucket_counts: Vec<u64> = s1a_id_bucket_counts
        .iter()
        .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
        .collect();

    Ok((total_refs, node_id_set, id_bucket_counts))
}

/// Header-only walk of node blobs that builds the `NodeBlobInfo` mapping
/// stage 2 uses to find which blobs cover each rank bucket.
///
/// Replaces the historical 82 GB `coords_by_rank` file. No decompression
/// happens here - for each node blob we read its indexdata `(min_id, max_id)`
/// and call `IdSet::rank` to compute the half-open referenced-rank range
/// `[ref_rank_start, ref_rank_end)`. Adjacent blobs' ranges are non-overlapping
/// and monotonic in rank because the input PBF is sorted by node ID.
#[hotpath::measure]
pub(super) fn build_node_blob_mapping(
    blob_meta: &[BlobMeta],
    node_id_set: &IdSet,
) -> Result<Vec<NodeBlobInfo>> {
    crate::debug::emit_marker("EXTJOIN_S1_NODE_MAP_START");
    let t0 = std::time::Instant::now();

    let mut mapping: Vec<NodeBlobInfo> = Vec::new();
    let mut blobs_with_zero_refs: u64 = 0;

    for meta in blob_meta {
        if !matches!(meta.kind, crate::blob_meta::ElemKind::Node) {
            continue;
        }
        // Sanity-check the indexdata range itself. A blob whose metadata
        // advertises max_id < min_id is malformed (adversarial, bitrot,
        // or a producer bug). count_below() with such bounds would
        // produce a reversed rank range and silently feed stage 2 a
        // negative-length slice via `ref_rank_end - ref_rank_start`;
        // the tail drift check at stage2.rs:488 would fire eventually,
        // but with a less specific diagnostic. Error here at the
        // boundary instead.
        if meta.max_id < meta.min_id {
            return Err(format!(
                "altw stage 1: blob at data_offset={} has reversed \
                 indexdata range [min_id={}, max_id={}]",
                meta.data_offset, meta.min_id, meta.max_id,
            )
            .into());
        }
        // count_below() is the safe variant of rank() that handles IDs past
        // the highest allocated chunk (which can happen when a node blob's
        // max_id sits in a chunk that contains no referenced nodes - rank()
        // would panic on the chunks[] index). Returns count of set IDs
        // strictly less than the argument, so this yields the half-open
        // referenced-rank range over [min_id, max_id].
        let ref_rank_start = node_id_set.count_below(meta.min_id);
        let ref_rank_end = match meta.max_id.checked_add(1) {
            Some(v) => node_id_set.count_below(v),
            None => node_id_set.total_count(),
        };
        if ref_rank_end == ref_rank_start {
            blobs_with_zero_refs += 1;
        }
        mapping.push(NodeBlobInfo {
            data_offset: meta.data_offset,
            data_size: meta.data_size,
            ref_rank_start,
            ref_rank_end,
            min_id: meta.min_id,
            max_id: meta.max_id,
        });
    }

    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    {
        crate::debug::emit_counter("s1_node_map_blobs", mapping.len() as i64);
        crate::debug::emit_counter("s1_node_map_zero_ref_blobs", blobs_with_zero_refs as i64);
        crate::debug::emit_counter("s1_node_map_build_ms", t0.elapsed().as_millis() as i64);
    }
    crate::debug::emit_marker("EXTJOIN_S1_NODE_MAP_END");
    Ok(mapping)
}

/// Two-pass stage 1 + node-blob mapping construction.
///
/// **Pass A**: parallel way scan to build `IdSet` of all referenced
/// node IDs + write sidecar ref counts.
///
/// **Pass B**: rescan ways with rank index available. Emit `(local_rank, slot_pos)`
/// records into rank-bucketed per-worker shard files.
///
/// **Mapping**: header-only walk of node blobs to compute the
/// `NodeBlobInfo` table stage 2 uses to find blobs covering each rank
/// bucket. Replaces the historical 82 GB `coords_by_rank` file.
///
/// `IdSet` (~2 GB RSS at planet) is **kept alive** in `Stage1Output`
/// because stage 2 calls `rank_if_set` while resolving coordinates inline.
#[hotpath::measure]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) fn stage1_way_pass(
    blob_meta: &[BlobMeta],
    input: &Path,
    _direct_io: bool,
    scratch: &ScratchDir,
    layout: &BucketLayout,
    ref_count_sidecar: &Path,
    per_way_refcount_sidecar: &Path,
) -> Result<Stage1Output> {
    use std::os::unix::fs::FileExt as _;

    let schedule = build_way_schedule(blob_meta)?;

    // Cap num_workers against the file descriptor budget. Pass B below
    // holds `num_workers * NUM_BUCKETS` rank-shard files open
    // concurrently (see `stage1.rs` Pass B loop at lines 385-405). On a
    // host with default soft ulimit (1024) this blows past RLIMIT_NOFILE
    // at ~4 workers, so we self-raise to the hard cap first and then
    // floor the worker count to what actually fits. Narrate the outcome
    // to stderr so the user sees what adjustments we made on their
    // behalf.
    let cpu_cap = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4);
    let fd_budget = super::raise_nofile_to_hard_cap();
    eprintln!("[altw external] {fd_budget}");
    let effective_soft = fd_budget.effective_soft;
    // Headroom for input PBF, scratch dir, sidecars, stdin/stdout/stderr,
    // mpsc channel fds, rayon worker fds, page-cache probe fds, etc.
    const HEADROOM_FDS: u64 = 64;
    let buckets_per_worker = NUM_BUCKETS as u64;
    if effective_soft < HEADROOM_FDS + buckets_per_worker {
        let min_needed = HEADROOM_FDS + buckets_per_worker;
        return Err(format!(
            "altw external: file descriptor limit too low \
             (RLIMIT_NOFILE = {effective_soft}, need >= {min_needed} \
             for even a single shard worker). Raise with \
             `ulimit -n {min_needed}` (or higher) and retry."
        )
        .into());
    }
    #[allow(clippy::cast_possible_truncation)]
    let fd_cap = ((effective_soft - HEADROOM_FDS) / buckets_per_worker) as usize;
    let num_workers = cpu_cap.min(fd_cap).max(1);
    if num_workers < cpu_cap {
        eprintln!(
            "[altw external] num_workers = {num_workers} \
             (capped from cpu={cpu_cap} by fd budget; each worker holds {NUM_BUCKETS} rank-shards)"
        );
    } else {
        eprintln!(
            "[altw external] num_workers = {num_workers} \
             (cpu-bound; fd budget allows up to {fd_cap} workers)"
        );
    }
    crate::debug::emit_counter(
        "extjoin_nofile_soft_cap",
        #[allow(clippy::cast_possible_wrap)]
        {
            effective_soft as i64
        },
    );
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("extjoin_cpu_cap_workers", cpu_cap as i64);
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("extjoin_fd_cap_workers", fd_cap as i64);

    let (total_refs, node_id_set, id_bucket_counts) = stage1_pass_a(
        input,
        &schedule,
        num_workers,
        scratch,
        layout,
        ref_count_sidecar,
        per_way_refcount_sidecar,
    )?;
    let unique_nodes_u64 = node_id_set.total_count();

    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?,
    );

    // Load sidecar prefix sums for slot_pos computation in pass B.
    let slot_starts = super::stage4::load_ref_count_sidecar(ref_count_sidecar, total_refs)?;

    // ---- Pass B: emit rank-bucketed (local_rank, slot_pos) records ----
    crate::debug::emit_marker("EXTJOIN_S1_PASS_B_START");

    let s1b_pread_ms = std::sync::atomic::AtomicU64::new(0);
    let s1b_decompress_ms = std::sync::atomic::AtomicU64::new(0);
    let s1b_scan_ms = std::sync::atomic::AtomicU64::new(0);
    let s1b_rank_ms = std::sync::atomic::AtomicU64::new(0);
    let s1b_encode_write_ms = std::sync::atomic::AtomicU64::new(0);
    let s1b_flush_ms = std::sync::atomic::AtomicU64::new(0);
    let s1b_refs_total = std::sync::atomic::AtomicU64::new(0);
    let s1b_bytes_written = std::sync::atomic::AtomicU64::new(0);
    let s1b_bytes_read = std::sync::atomic::AtomicU64::new(0);
    let s1b_shard_write_calls = std::sync::atomic::AtomicU64::new(0);
    let s1b_pread_calls = std::sync::atomic::AtomicU64::new(0);

    let next_idx = std::sync::atomic::AtomicUsize::new(0);

    let worker_counts: std::sync::Mutex<Vec<Vec<u64>>> = std::sync::Mutex::new(Vec::new());
    let actual_num_workers: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    let pass_b_error: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

    {
        let schedule_ref = &schedule;
        let next_ref = &next_idx;
        let node_id_set_ref = &node_id_set;
        let slot_starts_ref = &slot_starts;
        let worker_counts_ref = &worker_counts;
        let actual_ref = &actual_num_workers;
        let s1b_pread_ref = &s1b_pread_ms;
        let s1b_decompress_ref = &s1b_decompress_ms;
        let s1b_scan_ref = &s1b_scan_ms;
        let s1b_rank_ref = &s1b_rank_ms;
        let s1b_encode_write_ref = &s1b_encode_write_ms;
        let s1b_flush_ref = &s1b_flush_ms;
        let s1b_refs_total_ref = &s1b_refs_total;
        let s1b_bytes_written_ref = &s1b_bytes_written;
        let s1b_bytes_read_ref = &s1b_bytes_read;
        let s1b_shard_write_calls_ref = &s1b_shard_write_calls;
        let s1b_pread_calls_ref = &s1b_pread_calls;
        let err_ref = &pass_b_error;

        std::thread::scope(|scope| -> Result<()> {
            for worker_id in 0..num_workers {
                let file = std::sync::Arc::clone(&shared_file);
                scope.spawn(move || {
                    use std::sync::atomic::Ordering::Relaxed;

                    // Per-worker rank-bucket shard files.
                    let mut shard_writers: Vec<Option<BufWriter<std::fs::File>>> =
                        Vec::with_capacity(NUM_BUCKETS);
                    let mut entry_counts = vec![0u64; NUM_BUCKETS];
                    for bucket_idx in 0..NUM_BUCKETS {
                        let path = scratch.path.join(format!("rank-W{worker_id}-{bucket_idx:03}"));
                        let f = match std::fs::File::create(&path) {
                            Ok(f) => f,
                            Err(e) => {
                                *err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    Some(format!("create rank shard {}: {e}", path.display()));
                                return;
                            }
                        };
                        shard_writers.push(Some(BufWriter::with_capacity(
                            super::radix::BUCKET_BUF_SIZE, f,
                        )));
                    }

                    let mut read_buf: Vec<u8> = Vec::new();
                    let mut decompress_buf: Vec<u8> = Vec::new();
                    let mut refs_buf: Vec<i64> = Vec::new();
                    let mut group_starts: Vec<(usize, usize)> = Vec::new();
                    let mut rec_buf = [0u8; RANK_RECORD_SIZE];

                    loop {
                        if err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some() {
                            break;
                        }
                        let idx = next_ref.fetch_add(1, Relaxed);
                        if idx >= schedule_ref.len() { break; }
                        let task = &schedule_ref[idx];

                        let blob_result: std::result::Result<(), String> = (|| {
                            let t0 = std::time::Instant::now();
                            read_buf.resize(task.data_size, 0);
                            file.read_exact_at(&mut read_buf, task.data_offset)
                                .map_err(|e| format!("pass B pread: {e}"))?;
                            #[allow(clippy::cast_possible_truncation)]
                            s1b_pread_ref.fetch_add(t0.elapsed().as_millis() as u64, Relaxed);
                            s1b_bytes_read_ref.fetch_add(task.data_size as u64, Relaxed);
                            s1b_pread_calls_ref.fetch_add(1, Relaxed);

                            let t1 = std::time::Instant::now();
                            crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)
                                .map_err(|e| format!("pass B decompress: {e}"))?;
                            #[allow(clippy::cast_possible_truncation)]
                            s1b_decompress_ref.fetch_add(t1.elapsed().as_millis() as u64, Relaxed);

                            let t2 = std::time::Instant::now();
                            let slot_start = slot_starts_ref[task.seq as usize];
                            let mut blob_node_ids: Vec<i64> = Vec::new();
                            crate::scan::way::scan_way_refs(
                                &decompress_buf, &mut refs_buf, &mut group_starts,
                                |_way_id, refs| {
                                    blob_node_ids.extend_from_slice(refs);
                                },
                            ).map_err(|e| e.to_string())?;
                            #[allow(clippy::cast_possible_truncation)]
                            s1b_scan_ref.fetch_add(t2.elapsed().as_millis() as u64, Relaxed);
                            let blob_ref_count = blob_node_ids.len() as u64;
                            s1b_refs_total_ref.fetch_add(blob_ref_count, Relaxed);

                            let t3 = std::time::Instant::now();
                            let rank_range = unique_nodes_u64.div_ceil(NUM_BUCKETS as u64);
                            let mut ranked: Vec<(u32, usize, u64)> = Vec::with_capacity(blob_node_ids.len());
                            for (i, &node_id) in blob_node_ids.iter().enumerate() {
                                let global_rank = node_id_set_ref.rank(node_id);
                                #[allow(clippy::cast_possible_truncation)]
                                let bucket = (global_rank
                                    .checked_div(rank_range)
                                    .unwrap_or(0) as usize)
                                    .min(NUM_BUCKETS - 1);
                                let bucket_rank_start = bucket as u64 * rank_range;
                                #[allow(clippy::cast_possible_truncation)]
                                let local_rank = (global_rank - bucket_rank_start) as u32;
                                let slot_pos = slot_start + i as u64;
                                ranked.push((local_rank, bucket, slot_pos));
                            }
                            #[allow(clippy::cast_possible_truncation)]
                            s1b_rank_ref.fetch_add(t3.elapsed().as_millis() as u64, Relaxed);

                            let t4 = std::time::Instant::now();
                            let mut blob_bytes: u64 = 0;
                            let mut blob_writes: u64 = 0;
                            for &(local_rank, bucket, slot_pos) in &ranked {
                                let rec = RankRecord { local_rank, slot_pos };
                                rec.write_to(&mut rec_buf);
                                if let Some(w) = shard_writers[bucket].as_mut() {
                                    w.write_all(&rec_buf)
                                        .map_err(|e| format!("write rank shard: {e}"))?;
                                    blob_bytes += RANK_RECORD_SIZE as u64;
                                    blob_writes += 1;
                                }
                                entry_counts[bucket] += 1;
                            }
                            #[allow(clippy::cast_possible_truncation)]
                            s1b_encode_write_ref.fetch_add(t4.elapsed().as_millis() as u64, Relaxed);
                            s1b_bytes_written_ref.fetch_add(blob_bytes, Relaxed);
                            s1b_shard_write_calls_ref.fetch_add(blob_writes, Relaxed);
                            Ok(())
                        })();

                        if let Err(e) = blob_result {
                            *err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e);
                            break;
                        }
                    }

                    let t_flush = std::time::Instant::now();
                    for w in &mut shard_writers {
                        if let Some(writer) = w.as_mut() {
                            if let Err(e) = writer.flush() {
                                *err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    Some(format!("flush rank shard: {e}"));
                            }
                        }
                        *w = None;
                    }
                    #[allow(clippy::cast_possible_truncation)]
                    s1b_flush_ref.fetch_add(t_flush.elapsed().as_millis() as u64, std::sync::atomic::Ordering::Relaxed);

                    actual_ref.fetch_add(1, Relaxed);
                    worker_counts_ref.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(entry_counts);
                });
            }

            Ok(())
        })?;
    }

    if let Some(e) = pass_b_error.into_inner().unwrap_or(None) {
        return Err(e.into());
    }

    let num_actual_workers = actual_num_workers.load(std::sync::atomic::Ordering::Relaxed);

    let all_counts = worker_counts.into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut merged_counts = vec![0u64; NUM_BUCKETS];
    for counts in &all_counts {
        for (i, &c) in counts.iter().enumerate() {
            merged_counts[i] += c;
        }
    }

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("s1b_pread_ms", s1b_pread_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1b_decompress_ms", s1b_decompress_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1b_scan_ms", s1b_scan_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1b_rank_ms", s1b_rank_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1b_encode_write_ms", s1b_encode_write_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1b_flush_ms", s1b_flush_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1b_refs_total", s1b_refs_total.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1b_bytes_written", s1b_bytes_written.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1b_bytes_read", s1b_bytes_read.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1b_shard_write_calls", s1b_shard_write_calls.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1b_pread_calls", s1b_pread_calls.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1b_blobs", schedule.len() as i64);
        crate::debug::emit_counter("s1b_actual_workers", num_actual_workers as i64);
    }
    crate::debug::emit_marker("EXTJOIN_S1_PASS_B_END");

    // Build the per-blob rank mapping (header-only walk + rank queries -
    // no decompression).
    let node_blob_mapping = build_node_blob_mapping(blob_meta, &node_id_set)?;

    Ok(Stage1Output {
        total_slots: total_refs,
        unique_nodes: unique_nodes_u64,
        rank_bucket_counts: merged_counts,
        id_bucket_counts,
        num_shard_workers: num_actual_workers,
        node_id_set,
        node_blob_mapping,
    })
}
