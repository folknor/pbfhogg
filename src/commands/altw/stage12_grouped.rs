//! Prototype grouped stage-1B / stage-2 path.
//!
//! Pass B writes rank-bucket shard files as grouped runs:
//! `(local_rank: u32, run_len: u32, slot_pos[u64; run_len])`.
//! Stage 2 reparses those grouped runs directly, avoiding the current
//! per-record count+scatter step over raw `(local_rank, slot_pos)` tuples.
#![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap, clippy::too_many_lines)]

use std::io::{BufWriter, Write as _};
use std::path::Path;
use std::sync::Arc;

use super::super::external_radix::{ScratchDir, NUM_BUCKETS};
#[cfg(feature = "linux-direct-io")]
use super::super::external_radix::advise_dontneed_file;
use super::super::id_set_dense::IdSetDense;
use super::super::node_scanner::{extract_node_tuples, NodeTuple};
use super::super::Result;
use super::{
    slot_bucket_bounds, stage1, stage2, stage4, COORD_SLOT_SIZE, NodeBlobInfo,
    RESOLVED_ENTRY_SIZE, ResolvedEntry,
};

const GROUP_HEADER_SIZE: usize = 8;
const GROUPED_SEGMENT_TARGET_BYTES: usize = 64 * 1024 * 1024;

pub(super) fn parse_grouped_env() -> bool {
    matches!(
        std::env::var("PBFHOGG_ALTW_GROUPED_RANK_SEGMENTS").as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    )
}

struct LoaderScratch {
    data_buf: Vec<u8>,
    counts: Vec<u64>,
    write_pos: Vec<u64>,
}

impl LoaderScratch {
    fn new() -> Self {
        Self { data_buf: Vec::new(), counts: Vec::new(), write_pos: Vec::new() }
    }
}

struct PreparedBucket {
    grouped_slot_pos: Vec<u64>,
    group_offsets: Vec<u64>,
    bucket_rank_start: u64,
    local_range: usize,
    prepare_count_ms: u64,
    prepare_prefix_ms: u64,
    prepare_scatter_ms: u64,
    open_calls: u64,
    stat_calls: u64,
    fadvise_calls: u64,
    fadvise_bytes: u64,
}

#[allow(clippy::cast_possible_truncation)]
fn prepare_bucket_grouped(
    bucket_idx: usize,
    scratch: &ScratchDir,
    num_shard_workers: usize,
    unique_nodes: u64,
    rank_range_size: u64,
    loader: &mut LoaderScratch,
) -> std::result::Result<(PreparedBucket, u64), String> {
    let bucket_rank_start = bucket_idx as u64 * rank_range_size;
    let bucket_rank_end = if bucket_idx == NUM_BUCKETS - 1 {
        unique_nodes
    } else {
        ((bucket_idx as u64 + 1) * rank_range_size).min(unique_nodes)
    };
    let local_range = (bucket_rank_end - bucket_rank_start) as usize;

    loader.counts.clear();
    loader.counts.resize(local_range, 0);
    loader.data_buf.clear();
    let mut open_calls: u64 = 0;
    let mut stat_calls: u64 = 0;
    let mut fadvise_calls: u64 = 0;
    let mut fadvise_bytes: u64 = 0;
    for worker_id in 0..num_shard_workers {
        let path = scratch.path.join(format!("rank-W{worker_id}-{bucket_idx:03}"));
        let file = match std::fs::File::open(&path) {
            Ok(f) => {
                open_calls += 1;
                f
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("open grouped rank shard: {e}")),
        };
        stat_calls += 1;
        let len = file.metadata()
            .map_err(|e| format!("stat grouped rank shard: {e}"))?
            .len() as usize;
        if len == 0 {
            continue;
        }
        let start = loader.data_buf.len();
        loader.data_buf.resize(start + len, 0);
        std::io::Read::read_exact(&mut &file, &mut loader.data_buf[start..])
            .map_err(|e| format!("read grouped rank shard: {e}"))?;
        #[cfg(feature = "linux-direct-io")]
        {
            fadvise_calls += 1;
            fadvise_bytes += len as u64;
            advise_dontneed_file(&file);
        }
    }

    let t_count = std::time::Instant::now();
    let mut off = 0usize;
    let mut group_headers: u64 = 0;
    while off < loader.data_buf.len() {
        if off + GROUP_HEADER_SIZE > loader.data_buf.len() {
            return Err("grouped rank shard truncated in header".to_string());
        }
        let local_rank = u32::from_le_bytes([
            loader.data_buf[off],
            loader.data_buf[off + 1],
            loader.data_buf[off + 2],
            loader.data_buf[off + 3],
        ]) as usize;
        let run_len = u32::from_le_bytes([
            loader.data_buf[off + 4],
            loader.data_buf[off + 5],
            loader.data_buf[off + 6],
            loader.data_buf[off + 7],
        ]) as usize;
        if run_len == 0 {
            return Err("grouped rank shard encoded zero-length run".to_string());
        }
        if local_rank >= local_range {
            return Err(format!(
                "grouped rank shard local_rank {local_rank} out of range {local_range}"
            ));
        }
        loader.counts[local_rank] += run_len as u64;
        off = off
            .checked_add(GROUP_HEADER_SIZE + run_len * 8)
            .ok_or_else(|| "grouped rank shard overflow".to_string())?;
        if off > loader.data_buf.len() {
            return Err("grouped rank shard truncated in slot_pos payload".to_string());
        }
        group_headers += 1;
    }
    let prepare_count_ms = t_count.elapsed().as_millis() as u64;

    let t_prefix = std::time::Instant::now();
    let mut group_offsets = vec![0u64; local_range + 1];
    for (i, count) in loader.counts.iter().enumerate() {
        group_offsets[i + 1] = group_offsets[i] + count;
    }
    let prepare_prefix_ms = t_prefix.elapsed().as_millis() as u64;

    let t_scatter = std::time::Instant::now();
    let total = group_offsets[local_range] as usize;
    let mut grouped_slot_pos = vec![0u64; total];
    loader.write_pos.clear();
    loader.write_pos.extend_from_slice(&group_offsets[..local_range]);

    off = 0;
    while off < loader.data_buf.len() {
        let local_rank = u32::from_le_bytes([
            loader.data_buf[off],
            loader.data_buf[off + 1],
            loader.data_buf[off + 2],
            loader.data_buf[off + 3],
        ]) as usize;
        let run_len = u32::from_le_bytes([
            loader.data_buf[off + 4],
            loader.data_buf[off + 5],
            loader.data_buf[off + 6],
            loader.data_buf[off + 7],
        ]) as usize;
        let payload_off = off + GROUP_HEADER_SIZE;
        let payload_end = payload_off + run_len * 8;
        let write_pos = loader.write_pos[local_rank] as usize;
        for (dst, chunk) in grouped_slot_pos[write_pos..write_pos + run_len]
            .iter_mut()
            .zip(loader.data_buf[payload_off..payload_end].chunks_exact(8))
        {
            *dst = u64::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3],
                chunk[4], chunk[5], chunk[6], chunk[7],
            ]);
        }
        loader.write_pos[local_rank] += run_len as u64;
        off = payload_end;
    }
    let prepare_scatter_ms = t_scatter.elapsed().as_millis() as u64;

    Ok((
        PreparedBucket {
            grouped_slot_pos,
            group_offsets,
            bucket_rank_start,
            local_range,
            prepare_count_ms,
            prepare_prefix_ms,
            prepare_scatter_ms,
            open_calls,
            stat_calls,
            fadvise_calls,
            fadvise_bytes,
        },
        group_headers,
    ))
}

#[allow(clippy::cast_possible_truncation)]
fn flush_grouped_segment(
    shard_writers: &mut [Option<BufWriter<std::fs::File>>],
    bucket_records: &mut [Vec<(u32, u64)>],
    entry_counts: &mut [u64],
    bytes_written: &std::sync::atomic::AtomicU64,
    write_calls: &std::sync::atomic::AtomicU64,
    grouped_headers: &std::sync::atomic::AtomicU64,
    encode_write_ms: &std::sync::atomic::AtomicU64,
) -> std::result::Result<(), String> {
    use std::sync::atomic::Ordering::Relaxed;

    let t0 = std::time::Instant::now();
    let mut header_buf = [0u8; GROUP_HEADER_SIZE];
    let mut slot_buf = [0u8; 8];
    let mut local_bytes: u64 = 0;
    let mut local_writes: u64 = 0;
    let mut local_headers: u64 = 0;

    for (bucket_idx, records) in bucket_records.iter_mut().enumerate() {
        if records.is_empty() {
            continue;
        }
        records.sort_unstable_by_key(|&(local_rank, _)| local_rank);
        let Some(writer) = shard_writers[bucket_idx].as_mut() else {
            return Err(format!("missing grouped rank shard writer for bucket {bucket_idx}"));
        };
        let mut start = 0usize;
        while start < records.len() {
            let local_rank = records[start].0;
            let mut end = start + 1;
            while end < records.len() && records[end].0 == local_rank {
                end += 1;
            }
            let run_len = end - start;
            header_buf[..4].copy_from_slice(&local_rank.to_le_bytes());
            header_buf[4..8].copy_from_slice(&(run_len as u32).to_le_bytes());
            writer.write_all(&header_buf)
                .map_err(|e| format!("write grouped rank shard header: {e}"))?;
            local_bytes += GROUP_HEADER_SIZE as u64;
            local_writes += 1;
            local_headers += 1;
            for &(_, slot_pos) in &records[start..end] {
                slot_buf.copy_from_slice(&slot_pos.to_le_bytes());
                writer.write_all(&slot_buf)
                    .map_err(|e| format!("write grouped rank shard slot_pos: {e}"))?;
                local_bytes += 8;
                local_writes += 1;
            }
            start = end;
        }
        entry_counts[bucket_idx] += records.len() as u64;
        records.clear();
    }

    encode_write_ms.fetch_add(t0.elapsed().as_millis() as u64, Relaxed);
    bytes_written.fetch_add(local_bytes, Relaxed);
    write_calls.fetch_add(local_writes, Relaxed);
    grouped_headers.fetch_add(local_headers, Relaxed);
    Ok(())
}

#[hotpath::measure]
#[allow(clippy::too_many_lines)]
pub(super) fn stage1_way_pass_grouped(
    input: &Path,
    _direct_io: bool,
    scratch: &ScratchDir,
    ref_count_sidecar: &Path,
    per_way_refcount_sidecar: &Path,
) -> Result<stage1::Stage1Output> {
    use std::os::unix::fs::FileExt as _;

    let schedule = stage1::build_way_schedule(input)?;
    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4);

    let (total_refs, node_id_set) = stage1::stage1_pass_a(
        input, &schedule, num_workers, ref_count_sidecar, per_way_refcount_sidecar,
    )?;
    let unique_nodes_u64 = node_id_set.total_count();

    let shared_file = Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?,
    );
    let slot_starts = stage4::load_ref_count_sidecar(ref_count_sidecar, total_refs)?;

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
    let s1b_grouped_headers = std::sync::atomic::AtomicU64::new(0);
    let s1b_grouped_segment_flushes = std::sync::atomic::AtomicU64::new(0);
    let s1b_grouped_max_staged_bytes_est = std::sync::atomic::AtomicU64::new(0);

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
        let s1b_grouped_headers_ref = &s1b_grouped_headers;
        let s1b_grouped_segment_flushes_ref = &s1b_grouped_segment_flushes;
        let s1b_grouped_max_staged_bytes_est_ref = &s1b_grouped_max_staged_bytes_est;
        let err_ref = &pass_b_error;

        std::thread::scope(|scope| -> Result<()> {
            for worker_id in 0..num_workers {
                let file = Arc::clone(&shared_file);
                scope.spawn(move || {
                    use std::sync::atomic::Ordering::Relaxed;

                    let mut shard_writers: Vec<Option<BufWriter<std::fs::File>>> =
                        Vec::with_capacity(NUM_BUCKETS);
                    let mut entry_counts = vec![0u64; NUM_BUCKETS];
                    for bucket_idx in 0..NUM_BUCKETS {
                        let path = scratch.path.join(format!("rank-W{worker_id}-{bucket_idx:03}"));
                        let f = match std::fs::File::create(&path) {
                            Ok(f) => f,
                            Err(e) => {
                                *err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    Some(format!("create grouped rank shard {}: {e}", path.display()));
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
                    let mut bucket_records: Vec<Vec<(u32, u64)>> =
                        (0..NUM_BUCKETS).map(|_| Vec::new()).collect();
                    let mut staged_bytes_est: usize = 0;

                    loop {
                        if err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some() {
                            break;
                        }
                        let idx = next_ref.fetch_add(1, Relaxed);
                        if idx >= schedule_ref.len() {
                            break;
                        }
                        let task = &schedule_ref[idx];

                        let blob_result: std::result::Result<(), String> = (|| {
                            let t0 = std::time::Instant::now();
                            read_buf.resize(task.data_size, 0);
                            file.read_exact_at(&mut read_buf, task.data_offset)
                                .map_err(|e| format!("grouped pass B pread: {e}"))?;
                            s1b_pread_ref.fetch_add(t0.elapsed().as_millis() as u64, Relaxed);
                            s1b_bytes_read_ref.fetch_add(task.data_size as u64, Relaxed);
                            s1b_pread_calls_ref.fetch_add(1, Relaxed);

                            let t1 = std::time::Instant::now();
                            crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)
                                .map_err(|e| format!("grouped pass B decompress: {e}"))?;
                            s1b_decompress_ref.fetch_add(t1.elapsed().as_millis() as u64, Relaxed);

                            let t2 = std::time::Instant::now();
                            let slot_start = slot_starts_ref[task.seq as usize];
                            let mut blob_node_ids: Vec<i64> = Vec::new();
                            super::super::way_scanner::scan_way_refs(
                                &decompress_buf, &mut refs_buf, &mut group_starts,
                                |_way_id, refs| {
                                    blob_node_ids.extend_from_slice(refs);
                                },
                            ).map_err(|e| e.to_string())?;
                            s1b_scan_ref.fetch_add(t2.elapsed().as_millis() as u64, Relaxed);
                            let blob_ref_count = blob_node_ids.len() as u64;
                            s1b_refs_total_ref.fetch_add(blob_ref_count, Relaxed);

                            let t3 = std::time::Instant::now();
                            let rank_range = unique_nodes_u64.div_ceil(NUM_BUCKETS as u64);
                            for (i, &node_id) in blob_node_ids.iter().enumerate() {
                                let global_rank = node_id_set_ref.rank(node_id);
                                let bucket = if rank_range == 0 {
                                    0
                                } else {
                                    (global_rank / rank_range) as usize
                                }.min(NUM_BUCKETS - 1);
                                let bucket_rank_start = bucket as u64 * rank_range;
                                let local_rank = (global_rank - bucket_rank_start) as u32;
                                let slot_pos = slot_start + i as u64;
                                bucket_records[bucket].push((local_rank, slot_pos));
                                staged_bytes_est += std::mem::size_of::<(u32, u64)>();
                            }
                            s1b_rank_ref.fetch_add(t3.elapsed().as_millis() as u64, Relaxed);

                            let staged = staged_bytes_est as u64;
                            let mut current = s1b_grouped_max_staged_bytes_est_ref.load(Relaxed);
                            while staged > current {
                                match s1b_grouped_max_staged_bytes_est_ref.compare_exchange_weak(
                                    current, staged, Relaxed, Relaxed,
                                ) {
                                    Ok(_) => break,
                                    Err(actual) => current = actual,
                                }
                            }

                            if staged_bytes_est >= GROUPED_SEGMENT_TARGET_BYTES {
                                flush_grouped_segment(
                                    &mut shard_writers,
                                    &mut bucket_records,
                                    &mut entry_counts,
                                    s1b_bytes_written_ref,
                                    s1b_shard_write_calls_ref,
                                    s1b_grouped_headers_ref,
                                    s1b_encode_write_ref,
                                )?;
                                staged_bytes_est = 0;
                                s1b_grouped_segment_flushes_ref.fetch_add(1, Relaxed);
                            }

                            Ok(())
                        })();

                        if let Err(e) = blob_result {
                            *err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e);
                            break;
                        }
                    }

                    if staged_bytes_est > 0 {
                        if let Err(e) = flush_grouped_segment(
                            &mut shard_writers,
                            &mut bucket_records,
                            &mut entry_counts,
                            s1b_bytes_written_ref,
                            s1b_shard_write_calls_ref,
                            s1b_grouped_headers_ref,
                            s1b_encode_write_ref,
                        ) {
                            *err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e);
                        } else {
                            s1b_grouped_segment_flushes_ref.fetch_add(1, Relaxed);
                        }
                    }

                    let t_flush = std::time::Instant::now();
                    for w in &mut shard_writers {
                        if let Some(writer) = w.as_mut() {
                            if let Err(e) = writer.flush() {
                                *err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    Some(format!("flush grouped rank shard: {e}"));
                            }
                        }
                        *w = None;
                    }
                    s1b_flush_ref.fetch_add(t_flush.elapsed().as_millis() as u64, Relaxed);

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
        crate::debug::emit_counter("s1b_grouped_headers", s1b_grouped_headers.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1b_grouped_segment_flushes", s1b_grouped_segment_flushes.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s1b_grouped_max_staged_bytes_est", s1b_grouped_max_staged_bytes_est.load(std::sync::atomic::Ordering::Relaxed) as i64);
    }
    crate::debug::emit_marker("EXTJOIN_S1_PASS_B_END");

    let node_blob_mapping = stage1::build_node_blob_mapping(input, &node_id_set)?;

    Ok(stage1::Stage1Output {
        total_slots: total_refs,
        unique_nodes: unique_nodes_u64,
        rank_bucket_counts: merged_counts,
        num_shard_workers: num_actual_workers,
        node_id_set,
        node_blob_mapping,
    })
}

#[hotpath::measure]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) fn stage2_node_join_grouped(
    scratch: &ScratchDir,
    rank_bucket_counts: &[u64],
    num_shard_workers: usize,
    slot_buckets: &stage2::SharedSlotBuckets,
    slot_bucket_count: usize,
    total_slots: u64,
    unique_nodes: u64,
    input_pbf: Arc<std::fs::File>,
    node_id_set: &IdSetDense,
    node_blob_mapping: &[NodeBlobInfo],
) -> Result<u64> {
    let rank_range_size = unique_nodes.div_ceil(NUM_BUCKETS as u64);

    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4)
        .min(6);

    use std::os::unix::fs::FileExt as _;

    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let resolved_total = std::sync::atomic::AtomicU64::new(0);
    let s2_coord_fill_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_coord_zero_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_coord_zero_ns = std::sync::atomic::AtomicU64::new(0);
    let s2_node_pread_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_node_decompress_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_node_extract_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_node_rank_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_node_pread_ns = std::sync::atomic::AtomicU64::new(0);
    let s2_node_decompress_ns = std::sync::atomic::AtomicU64::new(0);
    let s2_node_extract_ns = std::sync::atomic::AtomicU64::new(0);
    let s2_node_rank_ns = std::sync::atomic::AtomicU64::new(0);
    let s2_node_bytes_read = std::sync::atomic::AtomicU64::new(0);
    let s2_node_blobs_read = std::sync::atomic::AtomicU64::new(0);
    let s2_node_straddler_blobs = std::sync::atomic::AtomicU64::new(0);
    let s2_resolve_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_bucket_load_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_bucket_loads = std::sync::atomic::AtomicU64::new(0);
    let s2_prepare_count_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_prepare_prefix_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_prepare_scatter_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_slot_flush_lock_wait_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_slot_flush_write_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_slot_bytes_written = std::sync::atomic::AtomicU64::new(0);
    let s2_slot_flush_calls = std::sync::atomic::AtomicU64::new(0);
    let s2_nonempty_slot_buckets_total = std::sync::atomic::AtomicU64::new(0);
    let s2_max_slot_buffer_bytes = std::sync::atomic::AtomicU64::new(0);
    let s2_max_worker_buf_bytes = std::sync::atomic::AtomicU64::new(0);
    let s2_pread_calls = std::sync::atomic::AtomicU64::new(0);
    let s2_open_calls = std::sync::atomic::AtomicU64::new(0);
    let s2_stat_calls = std::sync::atomic::AtomicU64::new(0);
    let s2_fadvise_calls = std::sync::atomic::AtomicU64::new(0);
    let s2_fadvise_bytes = std::sync::atomic::AtomicU64::new(0);
    let s2_grouped_headers = std::sync::atomic::AtomicU64::new(0);
    let s2_error: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

    let next_ref = &next_idx;
    let resolved_ref = &resolved_total;
    let coord_fill_ref = &s2_coord_fill_ms;
    let coord_zero_ms_ref = &s2_coord_zero_ms;
    let coord_zero_ns_ref = &s2_coord_zero_ns;
    let node_pread_ref = &s2_node_pread_ms;
    let node_decompress_ref = &s2_node_decompress_ms;
    let node_extract_ref = &s2_node_extract_ms;
    let node_rank_ref = &s2_node_rank_ms;
    let node_pread_ns_ref = &s2_node_pread_ns;
    let node_decompress_ns_ref = &s2_node_decompress_ns;
    let node_extract_ns_ref = &s2_node_extract_ns;
    let node_rank_ns_ref = &s2_node_rank_ns;
    let node_bytes_ref = &s2_node_bytes_read;
    let node_blobs_ref = &s2_node_blobs_read;
    let node_straddler_ref = &s2_node_straddler_blobs;
    let mapping_ref = node_blob_mapping;
    let id_set_ref = node_id_set;
    let resolve_ref = &s2_resolve_ms;
    let load_ref = &s2_bucket_load_ms;
    let loads_ref = &s2_bucket_loads;
    let prepare_count_ref = &s2_prepare_count_ms;
    let prepare_prefix_ref = &s2_prepare_prefix_ms;
    let prepare_scatter_ref = &s2_prepare_scatter_ms;
    let flush_lock_ref = &s2_slot_flush_lock_wait_ms;
    let flush_write_ref = &s2_slot_flush_write_ms;
    let slot_bytes_ref = &s2_slot_bytes_written;
    let flush_calls_ref = &s2_slot_flush_calls;
    let nonempty_ref = &s2_nonempty_slot_buckets_total;
    let max_buf_ref = &s2_max_slot_buffer_bytes;
    let max_worker_buf_ref = &s2_max_worker_buf_bytes;
    let pread_calls_ref = &s2_pread_calls;
    let open_calls_ref = &s2_open_calls;
    let stat_calls_ref = &s2_stat_calls;
    let fadvise_calls_ref = &s2_fadvise_calls;
    let fadvise_bytes_ref = &s2_fadvise_bytes;
    let grouped_headers_ref = &s2_grouped_headers;
    let err_ref = &s2_error;

    std::thread::scope(|scope| {
        for _ in 0..num_workers {
            let pbf_file = Arc::clone(&input_pbf);
            scope.spawn(move || {
                use std::sync::atomic::Ordering::Relaxed;

                let mut loader = LoaderScratch::new();
                let max_slice_bytes = (rank_range_size as usize) * COORD_SLOT_SIZE;
                let mut coord_slice: Vec<u8> = vec![0u8; max_slice_bytes];
                let mut node_read_buf: Vec<u8> = Vec::new();
                let mut node_decompress_buf: Vec<u8> = Vec::new();
                let mut node_tuples: Vec<NodeTuple> = Vec::new();
                let mut node_group_starts: Vec<(usize, usize)> = Vec::new();
                let mut entry_buf = [0u8; RESOLVED_ENTRY_SIZE];
                let mut local_resolved: u64 = 0;

                const FLUSH_THRESHOLD: usize = 256 * 1024;
                let mut slot_bufs: Vec<Vec<u8>> = (0..slot_bucket_count).map(|_| Vec::new()).collect();
                let mut slot_counts: Vec<u64> = vec![0; slot_bucket_count];

                loop {
                    if err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some() {
                        break;
                    }
                    let bucket_idx = next_ref.fetch_add(1, Relaxed);
                    if bucket_idx >= NUM_BUCKETS {
                        break;
                    }
                    if rank_bucket_counts[bucket_idx] == 0 {
                        continue;
                    }

                    let result: std::result::Result<(), String> = (|| {
                        let t_load = std::time::Instant::now();
                        let (bkt, grouped_headers) = prepare_bucket_grouped(
                            bucket_idx, scratch, num_shard_workers,
                            unique_nodes, rank_range_size, &mut loader,
                        )?;
                        load_ref.fetch_add(t_load.elapsed().as_millis() as u64, Relaxed);
                        loads_ref.fetch_add(1, Relaxed);
                        prepare_count_ref.fetch_add(bkt.prepare_count_ms, Relaxed);
                        prepare_prefix_ref.fetch_add(bkt.prepare_prefix_ms, Relaxed);
                        prepare_scatter_ref.fetch_add(bkt.prepare_scatter_ms, Relaxed);
                        open_calls_ref.fetch_add(bkt.open_calls, Relaxed);
                        stat_calls_ref.fetch_add(bkt.stat_calls, Relaxed);
                        fadvise_calls_ref.fetch_add(bkt.fadvise_calls, Relaxed);
                        fadvise_bytes_ref.fetch_add(bkt.fadvise_bytes, Relaxed);
                        grouped_headers_ref.fetch_add(grouped_headers, Relaxed);

                        let t_coord = std::time::Instant::now();
                        let slice_bytes = bkt.local_range * COORD_SLOT_SIZE;
                        let t_zero = std::time::Instant::now();
                        coord_slice[..slice_bytes].fill(0);
                        coord_zero_ms_ref.fetch_add(t_zero.elapsed().as_millis() as u64, Relaxed);
                        coord_zero_ns_ref.fetch_add(t_zero.elapsed().as_nanos() as u64, Relaxed);
                        let bucket_rank_start = bkt.bucket_rank_start;
                        let bucket_rank_end = bucket_rank_start + bkt.local_range as u64;

                        let lo = mapping_ref.partition_point(|b| b.ref_rank_end <= bucket_rank_start);
                        let hi = mapping_ref.partition_point(|b| b.ref_rank_start < bucket_rank_end);
                        let mut bucket_blobs_read: u64 = 0;
                        let mut bucket_straddlers: u64 = 0;
                        for blob in &mapping_ref[lo..hi] {
                            if blob.ref_count() == 0 {
                                continue;
                            }
                            if blob.ref_rank_start < bucket_rank_start || blob.ref_rank_end > bucket_rank_end {
                                bucket_straddlers += 1;
                            }

                            let t_pr = std::time::Instant::now();
                            node_read_buf.resize(blob.data_size, 0);
                            pbf_file.read_exact_at(&mut node_read_buf, blob.data_offset)
                                .map_err(|e| format!("stage2 grouped node pread: {e}"))?;
                            node_pread_ref.fetch_add(t_pr.elapsed().as_millis() as u64, Relaxed);
                            node_pread_ns_ref.fetch_add(t_pr.elapsed().as_nanos() as u64, Relaxed);
                            node_bytes_ref.fetch_add(blob.data_size as u64, Relaxed);

                            let t_dc = std::time::Instant::now();
                            crate::blob::decompress_blob_raw(&node_read_buf, &mut node_decompress_buf)
                                .map_err(|e| format!("stage2 grouped node decompress: {e}"))?;
                            node_decompress_ref.fetch_add(t_dc.elapsed().as_millis() as u64, Relaxed);
                            node_decompress_ns_ref.fetch_add(t_dc.elapsed().as_nanos() as u64, Relaxed);

                            let t_ex = std::time::Instant::now();
                            node_tuples.clear();
                            extract_node_tuples(
                                &node_decompress_buf, &mut node_tuples, &mut node_group_starts,
                            ).map_err(|e| format!("stage2 grouped node extract: {e}"))?;
                            node_extract_ref.fetch_add(t_ex.elapsed().as_millis() as u64, Relaxed);
                            node_extract_ns_ref.fetch_add(t_ex.elapsed().as_nanos() as u64, Relaxed);

                            let t_rk = std::time::Instant::now();
                            for &NodeTuple { id, lat, lon } in &node_tuples {
                                let Some(rank) = id_set_ref.rank_if_set(id) else {
                                    continue;
                                };
                                if rank < bucket_rank_start || rank >= bucket_rank_end {
                                    continue;
                                }
                                let local_rank = (rank - bucket_rank_start) as usize;
                                let off = local_rank * COORD_SLOT_SIZE;
                                coord_slice[off..off + 4].copy_from_slice(&lat.to_le_bytes());
                                coord_slice[off + 4..off + 8].copy_from_slice(&lon.to_le_bytes());
                            }
                            node_rank_ref.fetch_add(t_rk.elapsed().as_millis() as u64, Relaxed);
                            node_rank_ns_ref.fetch_add(t_rk.elapsed().as_nanos() as u64, Relaxed);
                            bucket_blobs_read += 1;
                        }
                        node_blobs_ref.fetch_add(bucket_blobs_read, Relaxed);
                        node_straddler_ref.fetch_add(bucket_straddlers, Relaxed);
                        coord_fill_ref.fetch_add(t_coord.elapsed().as_millis() as u64, Relaxed);
                        pread_calls_ref.fetch_add(bucket_blobs_read, Relaxed);

                        let t_resolve = std::time::Instant::now();
                        for local_rank in 0..bkt.local_range {
                            let start = bkt.group_offsets[local_rank] as usize;
                            let end = bkt.group_offsets[local_rank + 1] as usize;
                            if start == end {
                                continue;
                            }

                            let co = local_rank * COORD_SLOT_SIZE;
                            let lat = i32::from_le_bytes([
                                coord_slice[co], coord_slice[co + 1], coord_slice[co + 2], coord_slice[co + 3],
                            ]);
                            let lon = i32::from_le_bytes([
                                coord_slice[co + 4], coord_slice[co + 5], coord_slice[co + 6], coord_slice[co + 7],
                            ]);
                            let is_resolved = lat != 0 || lon != 0;

                            for &slot_pos in &bkt.grouped_slot_pos[start..end] {
                                let entry = ResolvedEntry { slot_pos, lat, lon };
                                let bucket = entry.slot_bucket(total_slots, slot_bucket_count);
                                let (bucket_start, bucket_end) =
                                    slot_bucket_bounds(total_slots, slot_bucket_count, bucket);
                                debug_assert!(bucket_end - bucket_start <= u32::MAX as u64);
                                entry.write_to(bucket_start, &mut entry_buf);
                                slot_bufs[bucket].extend_from_slice(&entry_buf);
                                slot_counts[bucket] += 1;
                                if is_resolved {
                                    local_resolved += 1;
                                }
                                if slot_bufs[bucket].len() >= FLUSH_THRESHOLD {
                                    let t_lock = std::time::Instant::now();
                                    let mut w = slot_buckets.writers[bucket]
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                                    flush_lock_ref.fetch_add(t_lock.elapsed().as_millis() as u64, Relaxed);
                                    let t_wr = std::time::Instant::now();
                                    let flush_bytes = slot_bufs[bucket].len() as u64;
                                    w.write_all(&slot_bufs[bucket])
                                        .map_err(|e| format!("write slot bucket: {e}"))?;
                                    drop(w);
                                    flush_write_ref.fetch_add(t_wr.elapsed().as_millis() as u64, Relaxed);
                                    slot_bytes_ref.fetch_add(flush_bytes, Relaxed);
                                    flush_calls_ref.fetch_add(1, Relaxed);
                                    let buf_sz = flush_bytes;
                                    let mut current = max_buf_ref.load(Relaxed);
                                    while buf_sz > current {
                                        match max_buf_ref.compare_exchange_weak(
                                            current, buf_sz, Relaxed, Relaxed,
                                        ) {
                                            Ok(_) => break,
                                            Err(actual) => current = actual,
                                        }
                                    }
                                    slot_buckets.entry_counts[bucket]
                                        .fetch_add(slot_counts[bucket], Relaxed);
                                    slot_bufs[bucket].clear();
                                    slot_counts[bucket] = 0;
                                }
                            }
                        }
                        resolve_ref.fetch_add(t_resolve.elapsed().as_millis() as u64, Relaxed);

                        let worker_bytes = loader.data_buf.capacity() as u64
                            + coord_slice.capacity() as u64
                            + node_read_buf.capacity() as u64
                            + node_decompress_buf.capacity() as u64
                            + (node_tuples.capacity() * std::mem::size_of::<NodeTuple>()) as u64
                            + (node_group_starts.capacity() * std::mem::size_of::<(usize, usize)>()) as u64
                            + slot_bufs.iter().map(|b| b.capacity() as u64).sum::<u64>();
                        let mut current = max_worker_buf_ref.load(Relaxed);
                        while worker_bytes > current {
                            match max_worker_buf_ref.compare_exchange_weak(
                                current, worker_bytes, Relaxed, Relaxed,
                            ) {
                                Ok(_) => break,
                                Err(actual) => current = actual,
                            }
                        }

                        let mut nonempty_count: u64 = 0;
                        for sb in 0..slot_bucket_count {
                            if slot_bufs[sb].is_empty() {
                                continue;
                            }
                            nonempty_count += 1;
                            let t_lock = std::time::Instant::now();
                            let mut w = slot_buckets.writers[sb]
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            flush_lock_ref.fetch_add(t_lock.elapsed().as_millis() as u64, Relaxed);
                            let t_wr = std::time::Instant::now();
                            let flush_bytes = slot_bufs[sb].len() as u64;
                            w.write_all(&slot_bufs[sb])
                                .map_err(|e| format!("write slot bucket: {e}"))?;
                            drop(w);
                            flush_write_ref.fetch_add(t_wr.elapsed().as_millis() as u64, Relaxed);
                            slot_bytes_ref.fetch_add(flush_bytes, Relaxed);
                            flush_calls_ref.fetch_add(1, Relaxed);
                            slot_buckets.entry_counts[sb].fetch_add(slot_counts[sb], Relaxed);
                            slot_bufs[sb].clear();
                            slot_counts[sb] = 0;
                        }
                        nonempty_ref.fetch_add(nonempty_count, Relaxed);

                        Ok(())
                    })();

                    if let Err(e) = result {
                        *err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e);
                        break;
                    }
                }

                resolved_ref.fetch_add(local_resolved, Relaxed);
            });
        }
    });

    if let Some(e) = s2_error.into_inner().unwrap_or(None) {
        return Err(e.into());
    }

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("s2_coord_fill_ms", s2_coord_fill_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_coord_zero_ms", s2_coord_zero_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_coord_zero_ns", s2_coord_zero_ns.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_pread_ms", s2_node_pread_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_decompress_ms", s2_node_decompress_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_extract_ms", s2_node_extract_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_rank_ms", s2_node_rank_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_pread_ns", s2_node_pread_ns.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_decompress_ns", s2_node_decompress_ns.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_extract_ns", s2_node_extract_ns.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_rank_ns", s2_node_rank_ns.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_bytes_read", s2_node_bytes_read.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_blobs_read", s2_node_blobs_read.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_straddler_blobs", s2_node_straddler_blobs.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_resolve_ms", s2_resolve_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_bucket_load_ms", s2_bucket_load_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_bucket_loads", s2_bucket_loads.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_prepare_count_ms", s2_prepare_count_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_prepare_prefix_ms", s2_prepare_prefix_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_prepare_scatter_ms", s2_prepare_scatter_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_slot_flush_lock_wait_ms", s2_slot_flush_lock_wait_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_slot_flush_write_ms", s2_slot_flush_write_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_slot_bytes_written", s2_slot_bytes_written.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_slot_flush_calls", s2_slot_flush_calls.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_nonempty_slot_buckets_total", s2_nonempty_slot_buckets_total.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_max_slot_buffer_bytes", s2_max_slot_buffer_bytes.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_max_worker_buf_bytes", s2_max_worker_buf_bytes.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_pread_calls", s2_pread_calls.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_open_calls", s2_open_calls.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_stat_calls", s2_stat_calls.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_fadvise_calls", s2_fadvise_calls.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_fadvise_bytes", s2_fadvise_bytes.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_num_workers", num_workers as i64);
        crate::debug::emit_counter("s2_grouped_headers", s2_grouped_headers.load(std::sync::atomic::Ordering::Relaxed) as i64);
    }

    let resolved_count = resolved_total.load(std::sync::atomic::Ordering::Relaxed);
    Ok(resolved_count)
}
