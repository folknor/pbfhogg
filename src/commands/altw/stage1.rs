//! Stage 1: Two-pass way scan + coord pass.
//!
//!   Pass A: build IdSetDense of referenced node IDs (parallel)
//!   Pass B: emit rank-bucketed (rank, slot_pos) records (parallel)
//!   Coord pass: populate dense coords_by_rank temp file

use std::io::{BufWriter, Write as _};
use std::path::Path;

use super::super::external_radix::{BucketWriters, ScratchDir, NUM_BUCKETS};
#[cfg(feature = "linux-direct-io")]
use super::super::external_radix::advise_dontneed_file;
use super::super::id_set_dense::IdSetDense;
use super::super::Result;
use super::{MAX_NODE_ID, RANK_RECORD_SIZE, COORD_SLOT_SIZE, RankRecord};

/// Way-blob schedule entry for the parallel way scans.
pub(super) struct WayBlobTask {
    pub(super) seq: u32,
    pub(super) data_offset: u64,
    pub(super) data_size: usize,
}

/// Build the way-blob schedule via header-only scan.
pub(super) fn build_way_schedule(input: &Path) -> Result<Vec<WayBlobTask>> {
    let mut scanner = crate::blob::BlobReader::seekable_from_path(input)?;
    scanner.set_parse_indexdata(true);
    scanner.next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let mut schedule: Vec<WayBlobTask> = Vec::new();
    let mut seq: u32 = 0;
    while let Some(result) = scanner.next_header_with_data_offset() {
        let (hdr, _, data_offset, data_size) = result?;
        if !matches!(hdr.blob_type(), crate::blob::BlobType::OsmData) { continue; }
        if let Some(idx) = hdr.index() {
            if !matches!(idx.kind, crate::blob_index::ElemKind::Way) { continue; }
        }
        schedule.push(WayBlobTask { seq, data_offset, data_size });
        seq += 1;
    }
    Ok(schedule)
}

/// Two-pass stage 1.
///
/// **Pass A**: parallel way scan to build `IdSetDense` of all referenced
/// node IDs + write sidecar ref counts.
///
/// **Pass B**: rescan ways with rank index available. Emit `(rank, slot_pos)`
/// records into rank-bucketed per-worker shard files.
///
/// Returns `(total_refs, unique_nodes, rank_bucket_entry_counts, num_workers, node_id_set)`.
#[hotpath::measure]
#[allow(clippy::too_many_lines)]
pub(super) fn stage1_way_pass(
    input: &Path,
    _direct_io: bool,
    scratch: &ScratchDir,
    ref_count_sidecar: &Path,
    coord_file_path: Option<&Path>,
) -> Result<(u64, u64, Vec<u64>, usize, IdSetDense)> {
    use std::os::unix::fs::FileExt as _;

    let schedule = build_way_schedule(input)?;
    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?,
    );

    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4);

    // ---- Pass A: build IdSetDense + sidecar ----
    crate::debug::emit_marker("EXTJOIN_S1_PASS_A_START");

    let mut node_id_set = super::super::id_set_dense::IdSetDense::new();
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

    // Workers scan way refs and set bits in the shared IdSetDense.
    // Consumer writes sidecar ref counts in blob order.
    {
        type PassAItem = (u32, std::result::Result<u64, String>);
        let (tx, rx) = std::sync::mpsc::sync_channel::<PassAItem>(32);
        let schedule_ref = &schedule;
        let next_ref = &next_idx;
        let node_id_set_ref = &node_id_set;
        let s1a_pread_ref = &s1a_pread_ms;
        let s1a_decompress_ref = &s1a_decompress_ms;
        let s1a_scan_ref = &s1a_scan_way_refs_ms;
        let s1a_idset_ref = &s1a_idset_set_ms;
        let s1a_bytes_ref = &s1a_bytes_read;
        let s1a_pread_calls_ref = &s1a_pread_calls;

        std::thread::scope(|scope| -> Result<()> {
            for _ in 0..num_workers {
                let file = std::sync::Arc::clone(&shared_file);
                let tx = tx.clone();
                scope.spawn(move || {
                    use std::sync::atomic::Ordering::Relaxed;
                    let mut read_buf: Vec<u8> = Vec::new();
                    let mut decompress_buf: Vec<u8> = Vec::new();
                    let mut refs_buf: Vec<i64> = Vec::new();
                    let mut group_starts: Vec<(usize, usize)> = Vec::new();

                    loop {
                        let idx = next_ref.fetch_add(1, Relaxed);
                        if idx >= schedule_ref.len() { break; }
                        let task = &schedule_ref[idx];

                        let result: std::result::Result<u64, String> = (|| {
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

                            // Phase 1: scan way refs (proto parsing only).
                            let t2 = std::time::Instant::now();
                            let mut blob_node_ids: Vec<i64> = Vec::new();
                            super::super::way_scanner::scan_way_refs(
                                &decompress_buf, &mut refs_buf, &mut group_starts,
                                |_way_id, refs| {
                                    blob_node_ids.extend_from_slice(refs);
                                },
                            ).map_err(|e| e.to_string())?;
                            #[allow(clippy::cast_possible_truncation)]
                            s1a_scan_ref.fetch_add(t2.elapsed().as_millis() as u64, Relaxed);

                            // Phase 2: batch set_atomic into IdSetDense.
                            let t3 = std::time::Instant::now();
                            for &node_id in &blob_node_ids {
                                node_id_set_ref.set_atomic(node_id);
                            }
                            #[allow(clippy::cast_possible_truncation)]
                            s1a_idset_ref.fetch_add(t3.elapsed().as_millis() as u64, Relaxed);

                            Ok(blob_node_ids.len() as u64)
                        })();

                        if tx.send((task.seq, result)).is_err() { break; }
                    }
                });
            }
            drop(tx);

            // Consumer: write sidecar in blob order.
            let mut sidecar_writer = BufWriter::with_capacity(
                64 * 1024,
                std::fs::File::create(ref_count_sidecar)
                    .map_err(|e| format!("create sidecar: {e}"))?,
            );
            let mut reorder: crate::reorder_buffer::ReorderBuffer<
                std::result::Result<u64, String>,
            > = crate::reorder_buffer::ReorderBuffer::with_capacity(64);

            for (seq_num, item) in rx {
                reorder.push(seq_num as usize, item);
                while let Some(result) = reorder.pop_ready() {
                    let ref_count = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                    sidecar_writer.write_all(&ref_count.to_le_bytes())?;
                    total_refs += ref_count;
                }
            }
            sidecar_writer.write_all(&total_refs.to_le_bytes())?;
            sidecar_writer.flush()?;
            Ok(())
        })?;
    }

    // Build rank index.
    node_id_set.build_rank_index();
    let unique_nodes = node_id_set.total_count();
    let unique_nodes_u64 = unique_nodes;

    // Load sidecar prefix sums for slot_pos computation in pass B.
    let slot_starts = super::stage4::load_ref_count_sidecar(ref_count_sidecar, total_refs)?;

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("s1a_pread_ms", s1a_pread_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1a_decompress_ms", s1a_decompress_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1a_scan_way_refs_ms", s1a_scan_way_refs_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1a_idset_set_ms", s1a_idset_set_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1a_bytes_read", s1a_bytes_read.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1a_pread_calls", s1a_pread_calls.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1a_unique_nodes", unique_nodes as i64);
    }
    crate::debug::emit_marker("EXTJOIN_S1_PASS_A_END");

    // ---- Pass B: emit rank-bucketed (rank, slot_pos) records ----
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

    next_idx.store(0, std::sync::atomic::Ordering::Relaxed);

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
                    let mut shard_writers: Vec<Option<BufWriter<std::fs::File>>> = Vec::with_capacity(NUM_BUCKETS);
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

                    let mut read_buf: Vec<u8> = Vec::new();
                    let mut decompress_buf: Vec<u8> = Vec::new();
                    let mut refs_buf: Vec<i64> = Vec::new();
                    let mut group_starts: Vec<(usize, usize)> = Vec::new();
                    let mut rec_buf = [0u8; RANK_RECORD_SIZE];

                    loop {
                        // Stop if another worker hit an error.
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

                            // Phase 1: scan way refs (proto parsing only).
                            let t2 = std::time::Instant::now();
                            let slot_start = slot_starts_ref[task.seq as usize];
                            let mut blob_node_ids: Vec<i64> = Vec::new();
                            super::super::way_scanner::scan_way_refs(
                                &decompress_buf, &mut refs_buf, &mut group_starts,
                                |_way_id, refs| {
                                    blob_node_ids.extend_from_slice(refs);
                                },
                            ).map_err(|e| e.to_string())?;
                            #[allow(clippy::cast_possible_truncation)]
                            s1b_scan_ref.fetch_add(t2.elapsed().as_millis() as u64, Relaxed);
                            let blob_ref_count = blob_node_ids.len() as u64;
                            s1b_refs_total_ref.fetch_add(blob_ref_count, Relaxed);

                            // Phase 2: batch rank lookups + bucket selection.
                            let t3 = std::time::Instant::now();
                            let rank_range = unique_nodes_u64.div_ceil(NUM_BUCKETS as u64);
                            // (local_rank, bucket, slot_pos) per ref.
                            let mut ranked: Vec<(u32, usize, u64)> = Vec::with_capacity(blob_node_ids.len());
                            for (i, &node_id) in blob_node_ids.iter().enumerate() {
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

                            // Phase 3: batch encode + write.
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

                    // Flush shard writers.
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
            // Spawn coord pass concurrently with pass B workers.
            // Both read from the same PBF via pread (different blob types),
            // and share &node_id_set (read-only). No contention.
            if let Some(cfp) = coord_file_path {
                scope.spawn(move || {
                    crate::debug::emit_marker("EXTJOIN_COORD_PASS_START");
                    if let Err(e) = build_coords_by_rank_file(
                        input, node_id_set_ref, unique_nodes_u64, cfp,
                    ) {
                        *err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(format!("coord pass: {e}"));
                    }
                    crate::debug::emit_marker("EXTJOIN_COORD_PASS_END");
                });
            }

            Ok(())
        })?;
    }

    if let Some(e) = pass_b_error.into_inner().unwrap_or(None) {
        return Err(e.into());
    }

    let num_actual_workers = actual_num_workers.load(std::sync::atomic::Ordering::Relaxed);

    // Merge per-worker entry counts.
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
    }
    crate::debug::emit_marker("EXTJOIN_S1_PASS_B_END");

    let num_actual = num_actual_workers;
    Ok((total_refs, unique_nodes_u64, merged_counts, num_actual, node_id_set))
}

/// Parallel node scan that writes dense (lat, lon) to a temp file indexed
/// by IdSetDense rank. Workers decompress node blobs, compute rank_if_set
/// for each node, coalesce adjacent ranks into contiguous pwrite extents.
///
/// Pre-sizes the file to `unique_nodes * 8` bytes (zeroed = missing sentinel).
#[hotpath::measure]
pub(super) fn build_coords_by_rank_file(
    input: &Path,
    node_id_set: &super::super::id_set_dense::IdSetDense,
    unique_nodes: u64,
    coord_file_path: &Path,
) -> Result<()> {
    use super::super::node_scanner::{NodeTuple, extract_node_tuples};
    use std::os::unix::fs::FileExt as _;

    let coord_file = std::fs::OpenOptions::new()
        .read(true).write(true).create(true).truncate(true)
        .open(coord_file_path)
        .map_err(|e| format!("create coords_by_rank: {e}"))?;
    let total_bytes = unique_nodes * COORD_SLOT_SIZE as u64;
    coord_file.set_len(total_bytes)
        .map_err(|e| format!("ftruncate coords_by_rank to {total_bytes}: {e}"))?;
    let coord_file = std::sync::Arc::new(coord_file);

    let mut scanner = crate::blob::BlobReader::seekable_from_path(input)?;
    scanner.set_parse_indexdata(true);
    scanner.next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

    struct NodeBlobTask { data_offset: u64, data_size: usize }
    let mut schedule: Vec<NodeBlobTask> = Vec::new();
    while let Some(result) = scanner.next_header_with_data_offset() {
        let (hdr, _, data_offset, data_size) = result?;
        if !matches!(hdr.blob_type(), crate::blob::BlobType::OsmData) { continue; }
        if let Some(idx) = hdr.index() {
            if !matches!(idx.kind, crate::blob_index::ElemKind::Node) { continue; }
        }
        schedule.push(NodeBlobTask { data_offset, data_size });
    }

    let shared_input = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("open input for coord pass: {e}"))?
    );

    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4);

    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let s_pread_ms = std::sync::atomic::AtomicU64::new(0);
    let s_decompress_ms = std::sync::atomic::AtomicU64::new(0);
    let s_extract_ms = std::sync::atomic::AtomicU64::new(0);
    let s_rank_if_set_ms = std::sync::atomic::AtomicU64::new(0);
    let s_extent_build_ms = std::sync::atomic::AtomicU64::new(0);
    let s_pwrite_ms = std::sync::atomic::AtomicU64::new(0);
    let s_nodes_written = std::sync::atomic::AtomicU64::new(0);
    let s_extents = std::sync::atomic::AtomicU64::new(0);
    let s_bytes_written = std::sync::atomic::AtomicU64::new(0);
    let s_pwrite_calls = std::sync::atomic::AtomicU64::new(0);
    let s_bytes_read = std::sync::atomic::AtomicU64::new(0);
    let s_pread_calls = std::sync::atomic::AtomicU64::new(0);
    let s_zero_gap_bytes = std::sync::atomic::AtomicU64::new(0);
    let s_max_extents_per_blob = std::sync::atomic::AtomicU64::new(0);
    let coord_pass_error: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

    let schedule_ref = &schedule;
    let next_ref = &next_idx;
    let err_ref = &coord_pass_error;

    std::thread::scope(|scope| {
        for _ in 0..num_workers {
            let input_file = std::sync::Arc::clone(&shared_input);
            let out_file = std::sync::Arc::clone(&coord_file);
            let pread_ref = &s_pread_ms;
            let decompress_ref = &s_decompress_ms;
            let extract_ref = &s_extract_ms;
            let rank_if_set_ref = &s_rank_if_set_ms;
            let extent_build_ref = &s_extent_build_ms;
            let pwrite_ref = &s_pwrite_ms;
            let written_ref = &s_nodes_written;
            let extents_ref = &s_extents;
            let bytes_ref = &s_bytes_written;
            let pwrite_calls_ref = &s_pwrite_calls;
            let bytes_read_ref = &s_bytes_read;
            let pread_calls_ref = &s_pread_calls;
            let zero_gap_ref = &s_zero_gap_bytes;
            let max_extents_ref = &s_max_extents_per_blob;
            scope.spawn(move || {
                use std::sync::atomic::Ordering::Relaxed;
                let mut read_buf: Vec<u8> = Vec::new();
                let mut decompress_buf: Vec<u8> = Vec::new();
                let mut tuples: Vec<NodeTuple> = Vec::new();
                let mut group_starts: Vec<(usize, usize)> = Vec::new();
                let mut extent_buf: Vec<u8> = Vec::new();

                loop {
                    if err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some() { break; }
                    let idx = next_ref.fetch_add(1, Relaxed);
                    if idx >= schedule_ref.len() { break; }
                    let task = &schedule_ref[idx];

                    let blob_result: std::result::Result<(), String> = (|| {
                        let t0 = std::time::Instant::now();
                        read_buf.resize(task.data_size, 0);
                        input_file.read_exact_at(&mut read_buf, task.data_offset)
                            .map_err(|e| format!("coord pass pread: {e}"))?;
                        #[allow(clippy::cast_possible_truncation)]
                        pread_ref.fetch_add(t0.elapsed().as_millis() as u64, Relaxed);
                        bytes_read_ref.fetch_add(task.data_size as u64, Relaxed);
                        pread_calls_ref.fetch_add(1, Relaxed);

                        let t1 = std::time::Instant::now();
                        crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)
                            .map_err(|e| format!("coord pass decompress: {e}"))?;
                        #[allow(clippy::cast_possible_truncation)]
                        decompress_ref.fetch_add(t1.elapsed().as_millis() as u64, Relaxed);

                        // Phase 1: extract node tuples from proto.
                        let t_extract = std::time::Instant::now();
                        tuples.clear();
                        extract_node_tuples(&decompress_buf, &mut tuples, &mut group_starts)
                            .map_err(|e| format!("coord pass extract: {e}"))?;
                        #[allow(clippy::cast_possible_truncation)]
                        extract_ref.fetch_add(t_extract.elapsed().as_millis() as u64, Relaxed);

                        // Phase 2: batch rank_if_set to identify referenced nodes.
                        // Produces (rank, lat, lon) for referenced nodes only, in
                        // rank-ascending order (nodes are ID-sorted, rank is monotonic).
                        let t_rank = std::time::Instant::now();
                        let mut ranked_coords: Vec<(u64, i32, i32)> = Vec::new();
                        for &NodeTuple { id, lat, lon } in &tuples {
                            if let Some(rank) = node_id_set.rank_if_set(id) {
                                ranked_coords.push((rank, lat, lon));
                            }
                        }
                        #[allow(clippy::cast_possible_truncation)]
                        rank_if_set_ref.fetch_add(t_rank.elapsed().as_millis() as u64, Relaxed);
                        let blob_nodes = ranked_coords.len() as u64;

                        // Phase 3: coalesce adjacent ranks into contiguous pwrite
                        // extents and write to the coord file.
                        let t_extent = std::time::Instant::now();
                        let mut extent_start_rank: Option<u64> = None;
                        let mut prev_rank: u64 = 0;
                        let mut blob_extents: u64 = 0;
                        let mut blob_bytes: u64 = 0;
                        let mut blob_pwrite_calls: u64 = 0;
                        let mut blob_zero_gap_bytes: u64 = 0;
                        extent_buf.clear();

                        for &(rank, lat, lon) in &ranked_coords {
                            let continues = extent_start_rank.is_some() && rank == prev_rank + 1;
                            if !continues && !extent_buf.is_empty() {
                                // Flush current extent.
                                let start = extent_start_rank.unwrap_or(0);
                                let t_w = std::time::Instant::now();
                                out_file.write_all_at(&extent_buf, start * COORD_SLOT_SIZE as u64)
                                    .map_err(|e| format!("coord pass pwrite: {e}"))?;
                                #[allow(clippy::cast_possible_truncation)]
                                pwrite_ref.fetch_add(t_w.elapsed().as_millis() as u64, Relaxed);
                                blob_extents += 1;
                                blob_pwrite_calls += 1;
                                blob_bytes += extent_buf.len() as u64;
                                // Gap between end of this extent and start of next.
                                let extent_end_rank = prev_rank + 1;
                                blob_zero_gap_bytes += (rank - extent_end_rank) * COORD_SLOT_SIZE as u64;
                                extent_buf.clear();
                            }
                            if !continues {
                                extent_start_rank = Some(rank);
                            }
                            extent_buf.extend_from_slice(&lat.to_le_bytes());
                            extent_buf.extend_from_slice(&lon.to_le_bytes());
                            prev_rank = rank;
                        }

                        // Flush final extent.
                        if !extent_buf.is_empty() {
                            let start = extent_start_rank.unwrap_or(0);
                            let t_w = std::time::Instant::now();
                            out_file.write_all_at(&extent_buf, start * COORD_SLOT_SIZE as u64)
                                .map_err(|e| format!("coord pass pwrite final: {e}"))?;
                            #[allow(clippy::cast_possible_truncation)]
                            pwrite_ref.fetch_add(t_w.elapsed().as_millis() as u64, Relaxed);
                            blob_extents += 1;
                            blob_pwrite_calls += 1;
                            blob_bytes += extent_buf.len() as u64;
                            extent_buf.clear();
                        }
                        #[allow(clippy::cast_possible_truncation)]
                        extent_build_ref.fetch_add(t_extent.elapsed().as_millis() as u64, Relaxed);

                        written_ref.fetch_add(blob_nodes, Relaxed);
                        extents_ref.fetch_add(blob_extents, Relaxed);
                        bytes_ref.fetch_add(blob_bytes, Relaxed);
                        pwrite_calls_ref.fetch_add(blob_pwrite_calls, Relaxed);
                        zero_gap_ref.fetch_add(blob_zero_gap_bytes, Relaxed);
                        // Update max extents per blob (relaxed CAS loop).
                        {
                            let mut current = max_extents_ref.load(Relaxed);
                            while blob_extents > current {
                                match max_extents_ref.compare_exchange_weak(
                                    current, blob_extents, Relaxed, Relaxed,
                                ) {
                                    Ok(_) => break,
                                    Err(actual) => current = actual,
                                }
                            }
                        }
                        Ok(())
                    })();

                    if let Err(e) = blob_result {
                        *err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e);
                        break;
                    }
                }
            });
        }
    });

    if let Some(e) = coord_pass_error.into_inner().unwrap_or(None) {
        return Err(e.into());
    }

    #[allow(clippy::cast_possible_wrap)]
    {
        let total_extents = s_extents.load(std::sync::atomic::Ordering::Relaxed);
        let total_bytes = s_bytes_written.load(std::sync::atomic::Ordering::Relaxed);
        crate::debug::emit_counter("coord_pass_pread_ms", s_pread_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("coord_pass_decompress_ms", s_decompress_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("coord_pass_extract_ms", s_extract_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("coord_pass_rank_if_set_ms", s_rank_if_set_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("coord_pass_extent_build_ms", s_extent_build_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("coord_pass_pwrite_ms", s_pwrite_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("coord_pass_nodes_written", s_nodes_written.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("coord_pass_extents", total_extents as i64);
        crate::debug::emit_counter("coord_pass_bytes_written", total_bytes as i64);
        crate::debug::emit_counter("coord_pass_pwrite_calls", s_pwrite_calls.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("coord_pass_bytes_read", s_bytes_read.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("coord_pass_pread_calls", s_pread_calls.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("coord_pass_zero_gap_bytes", s_zero_gap_bytes.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("coord_pass_max_extents_per_blob", s_max_extents_per_blob.load(std::sync::atomic::Ordering::Relaxed) as i64);
        #[allow(clippy::cast_possible_truncation)]
        if total_extents > 0 {
            crate::debug::emit_counter("coord_pass_avg_extent_bytes", (total_bytes / total_extents) as i64);
        }
        crate::debug::emit_counter("coord_pass_blobs", schedule.len() as i64);
    }

    Ok(())
}
