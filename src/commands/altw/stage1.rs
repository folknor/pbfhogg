//! Stage 1: Two-pass way scan + node-blob rank mapping.
//!
//!   Pass A: build IdSetDense of referenced node IDs (parallel).
//!   Pass B: emit rank-bucketed (local_rank, slot_pos) records (parallel).
//!   Node blob mapping: header-only walk of node blobs that records each
//!     blob's referenced-rank range using IdSetDense::rank queries on the
//!     blob's indexdata `(min_id, max_id)`. Replaces the historical 82 GB
//!     `coords_by_rank` file — stage 2 reads node blobs directly.

use std::io::{BufWriter, Write as _};
use std::path::Path;
use std::sync::Arc;

use super::super::external_radix::{ScratchDir, NUM_BUCKETS};
use super::super::id_set_dense::IdSetDense;
use super::super::Result;
use super::blob_meta::BlobMeta;
use super::{MAX_NODE_ID, NodeBlobInfo, RANK_RECORD_SIZE, RankRecord};

/// Pass A → Pass B scratch handoff. Pass A workers write each blob's
/// `blob_node_ids` (flat `i64` LE) to per-worker scratch files during
/// their Pass A work; Pass B replaces the pread+decompress+scan chain
/// with a single `read_exact_at` of `ref_count * 8` bytes from the
/// owning worker's file.
pub(super) struct NodeIdsLocation {
    pub(super) worker_id: u16,
    pub(super) file_offset: u64,
    pub(super) ref_count: u32,
}

pub(super) struct PassAScratch {
    pub(super) files: Vec<Arc<std::fs::File>>,
    pub(super) locations: Vec<NodeIdsLocation>,
}

/// Way-blob schedule entry for the parallel way scans.
pub(super) struct WayBlobTask {
    pub(super) seq: u32,
    pub(super) data_offset: u64,
    pub(super) data_size: usize,
}

/// Stage 1 output handed to stage 2. Owns the `IdSetDense` (kept alive
/// because stage 2 needs `rank_if_set` for inline node-blob coord
/// resolution) and the per-blob rank mapping.
pub(super) struct Stage1Output {
    pub total_slots: u64,
    pub unique_nodes: u64,
    pub rank_bucket_counts: Vec<u64>,
    pub num_shard_workers: usize,
    pub node_id_set: IdSetDense,
    pub node_blob_mapping: Vec<NodeBlobInfo>,
}

/// Build the way-blob schedule from the shared blob metadata scan.
pub(super) fn build_way_schedule(blob_meta: &[BlobMeta]) -> Result<Vec<WayBlobTask>> {
    crate::debug::emit_marker("EXTJOIN_S1_WAY_SCHEDULE_START");
    let t0 = std::time::Instant::now();
    let mut schedule: Vec<WayBlobTask> = Vec::new();
    let mut seq: u32 = 0;
    for meta in blob_meta {
        if !matches!(meta.kind, crate::blob_index::ElemKind::Way) {
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

/// Pass A standalone: parallel way scan to build `IdSetDense` of all
/// referenced node IDs and write the two ref-count sidecars in blob order.
///
/// Returns `(total_refs, IdSetDense)` with `build_rank_index()` already
/// called. Used by `stage1_way_pass` as the entry into stage 1.
#[hotpath::measure]
#[allow(clippy::too_many_lines)]
pub(super) fn stage1_pass_a(
    input: &Path,
    schedule: &[WayBlobTask],
    num_workers: usize,
    ref_count_sidecar: &Path,
    per_way_refcount_sidecar: &Path,
    scratch: &ScratchDir,
) -> Result<(u64, IdSetDense, PassAScratch)> {
    use std::os::unix::fs::FileExt as _;

    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?,
    );

    crate::debug::emit_marker("EXTJOIN_S1_PASS_A_START");

    let mut node_id_set = IdSetDense::new();
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
    let s1a_node_ids_scratch_bytes = std::sync::atomic::AtomicU64::new(0);

    // Pass A → Pass B scratch files for the scratch-spool fusion (#3).
    // Each worker owns one file and appends flat `i64` LE bytes of each
    // blob's referenced node IDs. Opened here on the main thread so the
    // `Arc<File>` handles can flow to pass B without a second `open`.
    let mut pass_a_scratch_files: Vec<Arc<std::fs::File>> = Vec::with_capacity(num_workers);
    for worker_id in 0..num_workers {
        let path = scratch.path.join(format!("ways-nodeids-W{worker_id}"));
        let f = std::fs::OpenOptions::new()
            .read(true).write(true).create(true).truncate(true).open(&path)
            .map_err(|e| format!("create pass-A node-ids scratch {}: {e}", path.display()))?;
        pass_a_scratch_files.push(Arc::new(f));
    }
    // Populated via the existing Pass-A completion channel; indexed by
    // way-blob `seq` on the main thread.
    let mut pass_a_locations: Vec<NodeIdsLocation> = (0..schedule.len()).map(|_| NodeIdsLocation {
        worker_id: 0, file_offset: 0, ref_count: 0,
    }).collect();
    {
        type PassAItem = (
            u32,
            std::result::Result<(u64, Vec<u32>, NodeIdsLocation), String>,
        );
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

        let scratch_files_ref = &pass_a_scratch_files;
        let s1a_nodeids_bytes_ref = &s1a_node_ids_scratch_bytes;
        std::thread::scope(|scope| -> Result<()> {
            for worker_id in 0..num_workers {
                let file = std::sync::Arc::clone(&shared_file);
                let scratch_file = std::sync::Arc::clone(&scratch_files_ref[worker_id]);
                let tx = tx.clone();
                let node_id_set_ref = &node_id_set;
                scope.spawn(move || {
                    use std::sync::atomic::Ordering::Relaxed;
                    use std::os::unix::fs::FileExt as _;
                    let mut read_buf: Vec<u8> = Vec::new();
                    let mut decompress_buf: Vec<u8> = Vec::new();
                    let mut refs_buf: Vec<i64> = Vec::new();
                    let mut group_starts: Vec<(usize, usize)> = Vec::new();
                    // Append cursor for this worker's scratch file. Only
                    // this thread writes to the file, so the cursor is
                    // thread-local and need not be atomic.
                    let mut scratch_offset: u64 = 0;

                    loop {
                        let idx = next_ref.fetch_add(1, Relaxed);
                        if idx >= schedule_ref.len() { break; }
                        let task = &schedule_ref[idx];

                        let result: std::result::Result<(u64, Vec<u32>, NodeIdsLocation), String> = (|| {
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
                            super::super::way_scanner::scan_way_refs(
                                &decompress_buf, &mut refs_buf, &mut group_starts,
                                |_way_id, refs| {
                                    blob_node_ids.extend_from_slice(refs);
                                    #[allow(clippy::cast_possible_truncation)]
                                    per_way_rcs.push(refs.len() as u32);
                                },
                            ).map_err(|e| e.to_string())?;
                            #[allow(clippy::cast_possible_truncation)]
                            s1a_scan_ref.fetch_add(t2.elapsed().as_millis() as u64, Relaxed);

                            let t3 = std::time::Instant::now();
                            for &node_id in &blob_node_ids {
                                node_id_set_ref.set_atomic(node_id);
                            }
                            #[allow(clippy::cast_possible_truncation)]
                            s1a_idset_ref.fetch_add(t3.elapsed().as_millis() as u64, Relaxed);

                            // Write the blob's node IDs (flat i64 LE) into
                            // this worker's scratch file. Pass B will pread
                            // this region and skip pread+decompress+scan of
                            // the way blob entirely.
                            let ref_count = blob_node_ids.len();
                            let nodeid_bytes: Vec<u8> = {
                                let mut v = Vec::with_capacity(ref_count * 8);
                                for &id in &blob_node_ids {
                                    v.extend_from_slice(&id.to_le_bytes());
                                }
                                v
                            };
                            let blob_offset = scratch_offset;
                            if !nodeid_bytes.is_empty() {
                                scratch_file.write_all_at(&nodeid_bytes, blob_offset)
                                    .map_err(|e| format!("pass A scratch write: {e}"))?;
                                scratch_offset += nodeid_bytes.len() as u64;
                                s1a_nodeids_bytes_ref.fetch_add(nodeid_bytes.len() as u64, Relaxed);
                            }
                            #[allow(clippy::cast_possible_truncation)]
                            let loc = NodeIdsLocation {
                                worker_id: worker_id as u16,
                                file_offset: blob_offset,
                                ref_count: ref_count as u32,
                            };

                            Ok((ref_count as u64, per_way_rcs, loc))
                        })();

                        if tx.send((task.seq, result)).is_err() { break; }
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
                std::result::Result<(u64, Vec<u32>, NodeIdsLocation), String>,
            > = crate::reorder_buffer::ReorderBuffer::with_capacity(64);
            let mut next_seq: usize = 0;

            for (seq_num, item) in rx {
                reorder.push(seq_num as usize, item);
                while let Some(result) = reorder.pop_ready() {
                    let (ref_count, per_way_rcs, loc) =
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

                    pass_a_locations[next_seq] = loc;
                    next_seq += 1;
                }
            }
            sidecar_writer.write_all(&total_refs.to_le_bytes())?;
            sidecar_writer.flush()?;
            per_way_writer.flush()?;
            Ok(())
        })?;
    }

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
        crate::debug::emit_counter("s1a_nodeids_scratch_bytes", s1a_node_ids_scratch_bytes.load(std::sync::atomic::Ordering::Relaxed) as i64);
    }
    crate::debug::emit_marker("EXTJOIN_S1_PASS_A_END");

    let pass_a_scratch = PassAScratch {
        files: pass_a_scratch_files,
        locations: pass_a_locations,
    };
    Ok((total_refs, node_id_set, pass_a_scratch))
}

/// Header-only walk of node blobs that builds the `NodeBlobInfo` mapping
/// stage 2 uses to find which blobs cover each rank bucket.
///
/// Replaces the historical 82 GB `coords_by_rank` file. No decompression
/// happens here — for each node blob we read its indexdata `(min_id, max_id)`
/// and call `IdSetDense::rank` to compute the half-open referenced-rank range
/// `[ref_rank_start, ref_rank_end)`. Adjacent blobs' ranges are non-overlapping
/// and monotonic in rank because the input PBF is sorted by node ID.
#[hotpath::measure]
pub(super) fn build_node_blob_mapping(
    blob_meta: &[BlobMeta],
    node_id_set: &IdSetDense,
) -> Result<Vec<NodeBlobInfo>> {
    crate::debug::emit_marker("EXTJOIN_S1_NODE_MAP_START");
    let t0 = std::time::Instant::now();

    let mut mapping: Vec<NodeBlobInfo> = Vec::new();
    let mut blobs_with_zero_refs: u64 = 0;

    for meta in blob_meta {
        if !matches!(meta.kind, crate::blob_index::ElemKind::Node) {
            continue;
        }
        // count_below() is the safe variant of rank() that handles IDs past
        // the highest allocated chunk (which can happen when a node blob's
        // max_id sits in a chunk that contains no referenced nodes — rank()
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
/// **Pass A**: parallel way scan to build `IdSetDense` of all referenced
/// node IDs + write sidecar ref counts.
///
/// **Pass B**: rescan ways with rank index available. Emit `(local_rank, slot_pos)`
/// records into rank-bucketed per-worker shard files.
///
/// **Mapping**: header-only walk of node blobs to compute the
/// `NodeBlobInfo` table stage 2 uses to find blobs covering each rank
/// bucket. Replaces the historical 82 GB `coords_by_rank` file.
///
/// `IdSetDense` (~2 GB RSS at planet) is **kept alive** in `Stage1Output`
/// because stage 2 calls `rank_if_set` while resolving coordinates inline.
#[hotpath::measure]
#[allow(clippy::too_many_lines)]
pub(super) fn stage1_way_pass(
    blob_meta: &[BlobMeta],
    input: &Path,
    _direct_io: bool,
    scratch: &ScratchDir,
    ref_count_sidecar: &Path,
    per_way_refcount_sidecar: &Path,
) -> Result<Stage1Output> {
    use std::os::unix::fs::FileExt as _;

    let schedule = build_way_schedule(blob_meta)?;

    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4);

    let (total_refs, node_id_set, pass_a_scratch) = stage1_pass_a(
        input, &schedule, num_workers, ref_count_sidecar, per_way_refcount_sidecar,
        scratch,
    )?;
    let unique_nodes_u64 = node_id_set.total_count();

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

        let pass_a_files_ref = &pass_a_scratch.files;
        let pass_a_locations_ref = &pass_a_scratch.locations;
        std::thread::scope(|scope| -> Result<()> {
            for worker_id in 0..num_workers {
                scope.spawn(move || {
                    use std::sync::atomic::Ordering::Relaxed;
                    use std::os::unix::fs::FileExt as _;

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
                            super::super::external_radix::BUCKET_BUF_SIZE, f,
                        )));
                    }

                    let mut nodeid_buf: Vec<u8> = Vec::new();
                    let mut rec_buf = [0u8; RANK_RECORD_SIZE];

                    loop {
                        if err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some() {
                            break;
                        }
                        let idx = next_ref.fetch_add(1, Relaxed);
                        if idx >= schedule_ref.len() { break; }
                        let task = &schedule_ref[idx];

                        let blob_result: std::result::Result<(), String> = (|| {
                            // Pass B reads this blob's node IDs straight
                            // from the scratch file the owning Pass A
                            // worker wrote — no pread of the compressed
                            // way blob, no zlib decompression, no protobuf
                            // scan. This is the #3 scratch-spool fusion.
                            let loc = &pass_a_locations_ref[task.seq as usize];
                            let ref_count_usize = loc.ref_count as usize;
                            let payload_bytes = ref_count_usize * 8;
                            nodeid_buf.resize(payload_bytes, 0);
                            let t0 = std::time::Instant::now();
                            if payload_bytes > 0 {
                                let f = &pass_a_files_ref[loc.worker_id as usize];
                                f.read_exact_at(&mut nodeid_buf, loc.file_offset)
                                    .map_err(|e| format!("pass B scratch pread: {e}"))?;
                            }
                            #[allow(clippy::cast_possible_truncation)]
                            s1b_pread_ref.fetch_add(t0.elapsed().as_millis() as u64, Relaxed);
                            s1b_bytes_read_ref.fetch_add(payload_bytes as u64, Relaxed);
                            s1b_pread_calls_ref.fetch_add(1, Relaxed);

                            let slot_start = slot_starts_ref[task.seq as usize];
                            // Reinterpret the payload as `&[i64]` via
                            // explicit little-endian decode — safe for
                            // any alignment and matches the Pass A
                            // encoding above byte-for-byte.
                            let blob_ref_count = loc.ref_count as u64;
                            s1b_refs_total_ref.fetch_add(blob_ref_count, Relaxed);

                            let t3 = std::time::Instant::now();
                            let rank_range = unique_nodes_u64.div_ceil(NUM_BUCKETS as u64);
                            let mut ranked: Vec<(u32, usize, u64)> = Vec::with_capacity(ref_count_usize);
                            for i in 0..ref_count_usize {
                                let off = i * 8;
                                let node_id = i64::from_le_bytes(
                                    nodeid_buf[off..off + 8].try_into().unwrap_or([0u8; 8]),
                                );
                                let global_rank = node_id_set_ref.rank(node_id);
                                #[allow(clippy::cast_possible_truncation)]
                                let bucket = if rank_range == 0 { 0 } else {
                                    (global_rank / rank_range) as usize
                                }.min(NUM_BUCKETS - 1);
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

    // Build the per-blob rank mapping (header-only walk + rank queries —
    // no decompression).
    let node_blob_mapping = build_node_blob_mapping(blob_meta, &node_id_set)?;

    Ok(Stage1Output {
        total_slots: total_refs,
        unique_nodes: unique_nodes_u64,
        rank_bucket_counts: merged_counts,
        num_shard_workers: num_actual_workers,
        node_id_set,
        node_blob_mapping,
    })
}
