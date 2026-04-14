//! Stage 2: Node join — parallel counting-sort per rank bucket, coord slice lookup.

use std::io::Write as _;
use std::path::Path;

use super::super::external_radix::{ScratchDir, NUM_BUCKETS};
#[cfg(feature = "linux-direct-io")]
use super::super::external_radix::advise_dontneed_file;
use super::super::Result;
use super::{RANK_RECORD_SIZE, RESOLVED_ENTRY_SIZE, COORD_SLOT_SIZE, ResolvedEntry};

// ---------------------------------------------------------------------------
// Stage 2: Parallel node join — coord slice lookup
// ---------------------------------------------------------------------------

struct LoaderScratch {
    data_buf: Vec<u8>,
    counts: Vec<u64>,
    write_pos: Vec<u64>,
}

struct PreparedBucket {
    grouped_slot_pos: Vec<u64>,
    group_offsets: Vec<u64>,
    bucket_rank_start: u64,
    local_range: usize,
    /// Sub-phase timings (ms): counting pass, prefix sum, scatter pass.
    prepare_count_ms: u64,
    prepare_prefix_ms: u64,
    prepare_scatter_ms: u64,
    /// I/O accounting from shard loading.
    open_calls: u64,
    stat_calls: u64,
    fadvise_calls: u64,
    fadvise_bytes: u64,
}

#[allow(clippy::cast_possible_truncation)]
fn prepare_bucket(
    bucket_idx: usize,
    scratch: &ScratchDir,
    num_shard_workers: usize,
    unique_nodes: u64,
    rank_range_size: u64,
    loader: &mut LoaderScratch,
) -> std::result::Result<PreparedBucket, String> {
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
            Ok(f) => { open_calls += 1; f }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("open rank shard: {e}")),
        };
        stat_calls += 1;
        let len = file.metadata()
            .map_err(|e| format!("stat rank shard: {e}"))?
            .len() as usize;
        if len == 0 { continue; }
        let start = loader.data_buf.len();
        loader.data_buf.resize(start + len, 0);
        std::io::Read::read_exact(&mut &file, &mut loader.data_buf[start..])
            .map_err(|e| format!("read rank shard: {e}"))?;
        #[cfg(feature = "linux-direct-io")]
        {
            fadvise_calls += 1;
            fadvise_bytes += len as u64;
            advise_dontneed_file(&file);
        }
    }

    // Count pass: histogram of local_rank frequencies.
    let t_count = std::time::Instant::now();
    for chunk in loader.data_buf.chunks_exact(RANK_RECORD_SIZE) {
        let local_rank = u32::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3],
        ]) as usize;
        loader.counts[local_rank] += 1;
    }
    let prepare_count_ms = t_count.elapsed().as_millis() as u64;

    // Prefix sum: compute group offsets.
    let t_prefix = std::time::Instant::now();
    let mut group_offsets = vec![0u64; local_range + 1];
    for (i, count) in loader.counts.iter().enumerate() {
        group_offsets[i + 1] = group_offsets[i] + count;
    }
    let prepare_prefix_ms = t_prefix.elapsed().as_millis() as u64;

    // Scatter pass: place slot_pos values into rank-grouped order.
    let t_scatter = std::time::Instant::now();
    let total = group_offsets[local_range] as usize;
    let mut grouped_slot_pos = vec![0u64; total];
    loader.write_pos.clear();
    loader.write_pos.extend_from_slice(&group_offsets[..local_range]);
    for chunk in loader.data_buf.chunks_exact(RANK_RECORD_SIZE) {
        let local_rank = u32::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3],
        ]) as usize;
        let slot_pos = u64::from_le_bytes([
            chunk[4], chunk[5], chunk[6], chunk[7],
            chunk[8], chunk[9], chunk[10], chunk[11],
        ]);
        let pos = loader.write_pos[local_rank] as usize;
        grouped_slot_pos[pos] = slot_pos;
        loader.write_pos[local_rank] += 1;
    }
    let prepare_scatter_ms = t_scatter.elapsed().as_millis() as u64;

    Ok(PreparedBucket {
        grouped_slot_pos, group_offsets, bucket_rank_start, local_range,
        prepare_count_ms, prepare_prefix_ms, prepare_scatter_ms,
        open_calls, stat_calls, fadvise_calls, fadvise_bytes,
    })
}

/// Shared slot bucket writers protected by per-bucket mutexes.
/// `slot_bucket_count` files total regardless of worker count.
pub(super) struct SharedSlotBuckets {
    writers: Vec<std::sync::Mutex<std::io::BufWriter<std::fs::File>>>,
    entry_counts: Vec<std::sync::atomic::AtomicU64>,
    paths: Vec<std::path::PathBuf>,
}

const BUCKET_BUF_SIZE: usize = 256 * 1024;

impl SharedSlotBuckets {
    pub(super) fn create(
        scratch: &ScratchDir,
        slot_bucket_count: usize,
    ) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let mut writers = Vec::with_capacity(slot_bucket_count);
        let mut paths = Vec::with_capacity(slot_bucket_count);
        let mut entry_counts = Vec::with_capacity(slot_bucket_count);

        for i in 0..slot_bucket_count {
            let path = scratch.bucket_path("slot", i);
            let file = std::fs::File::create(&path)
                .map_err(|e| format!("failed to create slot bucket {}: {e}", path.display()))?;
            writers.push(std::sync::Mutex::new(
                std::io::BufWriter::with_capacity(BUCKET_BUF_SIZE, file),
            ));
            paths.push(path);
            entry_counts.push(std::sync::atomic::AtomicU64::new(0));
        }

        Ok(Self { writers, entry_counts, paths })
    }

    pub(super) fn finish(&self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        for writer_mutex in &self.writers {
            let mut w = writer_mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            w.flush()?;
            #[cfg(feature = "linux-direct-io")]
            {
                use std::os::unix::io::AsRawFd;
                drop(w.get_ref().sync_data());
                unsafe {
                    libc::posix_fadvise(
                        w.get_ref().as_raw_fd(),
                        0,
                        0,
                        libc::POSIX_FADV_DONTNEED,
                    )
                };
            }
        }
        Ok(())
    }

    fn entry_counts_snapshot(&self) -> Vec<u64> {
        self.entry_counts.iter()
            .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
            .collect()
    }
}

/// Parallel stage 2: N workers each claim rank buckets via atomic dispatch,
/// load rank records, counting-sort, pread coord slice, resolve to (lat, lon),
/// write to shared slot bucket files (256 files total, per-bucket mutex).
///
/// Returns resolved_count.
#[hotpath::measure]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) fn stage2_node_join(
    scratch: &ScratchDir,
    rank_bucket_counts: &[u64],
    num_shard_workers: usize,
    slot_buckets: &SharedSlotBuckets,
    slot_bucket_count: usize,
    total_slots: u64,
    unique_nodes: u64,
    coord_file_path: &Path,
) -> Result<u64> {
    let rank_range_size = unique_nodes.div_ceil(NUM_BUCKETS as u64);

    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4)
        .min(6);

    use std::os::unix::fs::FileExt as _;
    let coord_file = std::sync::Arc::new(
        std::fs::File::open(coord_file_path)
            .map_err(|e| format!("open coords_by_rank for stage 2: {e}"))?
    );

    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let resolved_total = std::sync::atomic::AtomicU64::new(0);
    let s2_coord_read_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_resolve_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_bucket_load_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_bucket_loads = std::sync::atomic::AtomicU64::new(0);
    let s2_prepare_count_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_prepare_prefix_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_prepare_scatter_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_slot_flush_lock_wait_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_slot_flush_write_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_coord_slice_bytes_read = std::sync::atomic::AtomicU64::new(0);
    let s2_slot_bytes_written = std::sync::atomic::AtomicU64::new(0);
    let s2_slot_flush_calls = std::sync::atomic::AtomicU64::new(0);
    let s2_nonempty_slot_buckets_total = std::sync::atomic::AtomicU64::new(0);
    let s2_max_slot_buffer_bytes = std::sync::atomic::AtomicU64::new(0);
    let s2_slot_buffer_append_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_max_worker_buf_bytes = std::sync::atomic::AtomicU64::new(0);
    let s2_pread_calls = std::sync::atomic::AtomicU64::new(0);
    let s2_open_calls = std::sync::atomic::AtomicU64::new(0);
    let s2_stat_calls = std::sync::atomic::AtomicU64::new(0);
    let s2_fadvise_calls = std::sync::atomic::AtomicU64::new(0);
    let s2_fadvise_bytes = std::sync::atomic::AtomicU64::new(0);
    let s2_error: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

    let next_ref = &next_idx;
    let resolved_ref = &resolved_total;
    let coord_read_ref = &s2_coord_read_ms;
    let resolve_ref = &s2_resolve_ms;
    let load_ref = &s2_bucket_load_ms;
    let loads_ref = &s2_bucket_loads;
    let prepare_count_ref = &s2_prepare_count_ms;
    let prepare_prefix_ref = &s2_prepare_prefix_ms;
    let prepare_scatter_ref = &s2_prepare_scatter_ms;
    let flush_lock_ref = &s2_slot_flush_lock_wait_ms;
    let flush_write_ref = &s2_slot_flush_write_ms;
    let coord_bytes_ref = &s2_coord_slice_bytes_read;
    let slot_bytes_ref = &s2_slot_bytes_written;
    let flush_calls_ref = &s2_slot_flush_calls;
    let nonempty_ref = &s2_nonempty_slot_buckets_total;
    let max_buf_ref = &s2_max_slot_buffer_bytes;
    let append_ref = &s2_slot_buffer_append_ms;
    let max_worker_buf_ref = &s2_max_worker_buf_bytes;
    let pread_calls_ref = &s2_pread_calls;
    let open_calls_ref = &s2_open_calls;
    let stat_calls_ref = &s2_stat_calls;
    let fadvise_calls_ref = &s2_fadvise_calls;
    let fadvise_bytes_ref = &s2_fadvise_bytes;
    let err_ref = &s2_error;

    std::thread::scope(|scope| {
        for _ in 0..num_workers {
            let cf = std::sync::Arc::clone(&coord_file);
            scope.spawn(move || {
                use std::sync::atomic::Ordering::Relaxed;

                let mut loader = LoaderScratch {
                    data_buf: Vec::new(), counts: Vec::new(), write_pos: Vec::new(),
                };
                let mut coord_slice: Vec<u8> = Vec::new();
                let mut entry_buf = [0u8; RESOLVED_ENTRY_SIZE];
                let mut local_resolved: u64 = 0;

                // Per-slot-bucket local buffers. Flushed when any buffer
                // exceeds FLUSH_THRESHOLD, and at the end of each rank bucket.
                // 256 KB per buffer × slot_bucket_count = 64 MB max per worker
                // at full 256-bucket scale, scales down for small inputs.
                const FLUSH_THRESHOLD: usize = 256 * 1024;
                let mut slot_bufs: Vec<Vec<u8>> = (0..slot_bucket_count).map(|_| Vec::new()).collect();
                let mut slot_counts: Vec<u64> = vec![0; slot_bucket_count];

                loop {
                    if err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some() {
                        break;
                    }
                    let bucket_idx = next_ref.fetch_add(1, Relaxed);
                    if bucket_idx >= NUM_BUCKETS { break; }
                    if rank_bucket_counts[bucket_idx] == 0 { continue; }

                    let result: std::result::Result<(), String> = (|| {
                        // Load + counting-sort rank records.
                        let t_load = std::time::Instant::now();
                        let bkt = prepare_bucket(
                            bucket_idx, scratch, num_shard_workers,
                            unique_nodes, rank_range_size, &mut loader,
                        )?;
                        #[allow(clippy::cast_possible_truncation)]
                        load_ref.fetch_add(t_load.elapsed().as_millis() as u64, Relaxed);
                        loads_ref.fetch_add(1, Relaxed);
                        prepare_count_ref.fetch_add(bkt.prepare_count_ms, Relaxed);
                        prepare_prefix_ref.fetch_add(bkt.prepare_prefix_ms, Relaxed);
                        prepare_scatter_ref.fetch_add(bkt.prepare_scatter_ms, Relaxed);
                        open_calls_ref.fetch_add(bkt.open_calls, Relaxed);
                        stat_calls_ref.fetch_add(bkt.stat_calls, Relaxed);
                        fadvise_calls_ref.fetch_add(bkt.fadvise_calls, Relaxed);
                        fadvise_bytes_ref.fetch_add(bkt.fadvise_bytes, Relaxed);

                        // Pread this bucket's contiguous coord slice.
                        let t_coord = std::time::Instant::now();
                        let slice_bytes = bkt.local_range * COORD_SLOT_SIZE;
                        coord_slice.resize(slice_bytes, 0);
                        cf.read_exact_at(
                            &mut coord_slice, bkt.bucket_rank_start * COORD_SLOT_SIZE as u64,
                        ).map_err(|e| format!("pread coord slice: {e}"))?;
                        #[allow(clippy::cast_possible_truncation)]
                        coord_read_ref.fetch_add(t_coord.elapsed().as_millis() as u64, Relaxed);
                        coord_bytes_ref.fetch_add(slice_bytes as u64, Relaxed);
                        pread_calls_ref.fetch_add(1, Relaxed);

                        // Resolve each rank group into worker-local slot buffers.
                        let t_resolve = std::time::Instant::now();
                        for local_rank in 0..bkt.local_range {
                            #[allow(clippy::cast_possible_truncation)]
                            let start = bkt.group_offsets[local_rank] as usize;
                            #[allow(clippy::cast_possible_truncation)]
                            let end = bkt.group_offsets[local_rank + 1] as usize;
                            if start == end { continue; }

                            let co = local_rank * COORD_SLOT_SIZE;
                            let lat = i32::from_le_bytes([
                                coord_slice[co], coord_slice[co+1], coord_slice[co+2], coord_slice[co+3],
                            ]);
                            let lon = i32::from_le_bytes([
                                coord_slice[co+4], coord_slice[co+5], coord_slice[co+6], coord_slice[co+7],
                            ]);
                            let is_resolved = lat != 0 || lon != 0;

                            for &slot_pos in &bkt.grouped_slot_pos[start..end] {
                                let entry = ResolvedEntry { slot_pos, lat, lon };
                                let bucket = entry.slot_bucket(total_slots, slot_bucket_count);
                                entry.write_to(&mut entry_buf);
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
                                    #[allow(clippy::cast_possible_truncation)]
                                    flush_lock_ref.fetch_add(t_lock.elapsed().as_millis() as u64, Relaxed);
                                    let t_wr = std::time::Instant::now();
                                    let flush_bytes = slot_bufs[bucket].len() as u64;
                                    w.write_all(&slot_bufs[bucket])
                                        .map_err(|e| format!("write slot bucket: {e}"))?;
                                    drop(w);
                                    #[allow(clippy::cast_possible_truncation)]
                                    flush_write_ref.fetch_add(t_wr.elapsed().as_millis() as u64, Relaxed);
                                    slot_bytes_ref.fetch_add(flush_bytes, Relaxed);
                                    flush_calls_ref.fetch_add(1, Relaxed);
                                    // Track max buffer size before flush.
                                    {
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
                                    }
                                    slot_buckets.entry_counts[bucket]
                                        .fetch_add(slot_counts[bucket], Relaxed);
                                    slot_bufs[bucket].clear();
                                    slot_counts[bucket] = 0;
                                }
                            }
                        }
                        #[allow(clippy::cast_possible_truncation)]
                        resolve_ref.fetch_add(t_resolve.elapsed().as_millis() as u64, Relaxed);

                        // Track max live buffer bytes for this worker.
                        {
                            let worker_bytes = loader.data_buf.capacity() as u64
                                + coord_slice.capacity() as u64
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
                        }

                        // Flush non-empty local buffers to shared writers.
                        let mut nonempty_count: u64 = 0;
                        for sb in 0..slot_bucket_count {
                            if slot_bufs[sb].is_empty() { continue; }
                            nonempty_count += 1;
                            let t_lock = std::time::Instant::now();
                            let mut w = slot_buckets.writers[sb]
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            #[allow(clippy::cast_possible_truncation)]
                            flush_lock_ref.fetch_add(t_lock.elapsed().as_millis() as u64, Relaxed);
                            let t_wr = std::time::Instant::now();
                            let flush_bytes = slot_bufs[sb].len() as u64;
                            w.write_all(&slot_bufs[sb])
                                .map_err(|e| format!("write slot bucket: {e}"))?;
                            drop(w);
                            #[allow(clippy::cast_possible_truncation)]
                            flush_write_ref.fetch_add(t_wr.elapsed().as_millis() as u64, Relaxed);
                            slot_bytes_ref.fetch_add(flush_bytes, Relaxed);
                            flush_calls_ref.fetch_add(1, Relaxed);
                            slot_buckets.entry_counts[sb]
                                .fetch_add(slot_counts[sb], Relaxed);
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
        crate::debug::emit_counter("s2_coord_read_ms", s2_coord_read_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_resolve_ms", s2_resolve_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_bucket_load_ms", s2_bucket_load_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_bucket_loads", s2_bucket_loads.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_prepare_count_ms", s2_prepare_count_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_prepare_prefix_ms", s2_prepare_prefix_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_prepare_scatter_ms", s2_prepare_scatter_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_slot_flush_lock_wait_ms", s2_slot_flush_lock_wait_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_slot_flush_write_ms", s2_slot_flush_write_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_coord_slice_bytes_read", s2_coord_slice_bytes_read.load(std::sync::atomic::Ordering::Relaxed) as i64);
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
    }

    let resolved_count = resolved_total.load(std::sync::atomic::Ordering::Relaxed);
    Ok(resolved_count)
}

pub(super) type SlotBuckets = SharedSlotBuckets;
