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
    for worker_id in 0..num_shard_workers {
        let path = scratch.path.join(format!("rank-W{worker_id}-{bucket_idx:03}"));
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("open rank shard: {e}")),
        };
        let len = file.metadata()
            .map_err(|e| format!("stat rank shard: {e}"))?
            .len() as usize;
        if len == 0 { continue; }
        let start = loader.data_buf.len();
        loader.data_buf.resize(start + len, 0);
        std::io::Read::read_exact(&mut &file, &mut loader.data_buf[start..])
            .map_err(|e| format!("read rank shard: {e}"))?;
        #[cfg(feature = "linux-direct-io")]
        advise_dontneed_file(&file);
    }

    for chunk in loader.data_buf.chunks_exact(RANK_RECORD_SIZE) {
        let local_rank = u32::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3],
        ]) as usize;
        loader.counts[local_rank] += 1;
    }

    let mut group_offsets = vec![0u64; local_range + 1];
    for (i, count) in loader.counts.iter().enumerate() {
        group_offsets[i + 1] = group_offsets[i] + count;
    }

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

    Ok(PreparedBucket { grouped_slot_pos, group_offsets, bucket_rank_start, local_range })
}

/// Shared slot bucket writers protected by per-bucket mutexes.
/// 256 files total regardless of worker count.
pub(super) struct SharedSlotBuckets {
    writers: Vec<std::sync::Mutex<std::io::BufWriter<std::fs::File>>>,
    entry_counts: Vec<std::sync::atomic::AtomicU64>,
    paths: Vec<std::path::PathBuf>,
}

const BUCKET_BUF_SIZE: usize = 256 * 1024;

impl SharedSlotBuckets {
    pub(super) fn create(scratch: &ScratchDir) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let mut writers = Vec::with_capacity(NUM_BUCKETS);
        let mut paths = Vec::with_capacity(NUM_BUCKETS);
        let mut entry_counts = Vec::with_capacity(NUM_BUCKETS);

        for i in 0..NUM_BUCKETS {
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
    let s2_error: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

    let next_ref = &next_idx;
    let resolved_ref = &resolved_total;
    let coord_read_ref = &s2_coord_read_ms;
    let resolve_ref = &s2_resolve_ms;
    let load_ref = &s2_bucket_load_ms;
    let loads_ref = &s2_bucket_loads;
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

                        // Pread this bucket's contiguous coord slice.
                        let t_coord = std::time::Instant::now();
                        let slice_bytes = bkt.local_range * COORD_SLOT_SIZE;
                        coord_slice.resize(slice_bytes, 0);
                        cf.read_exact_at(
                            &mut coord_slice, bkt.bucket_rank_start * COORD_SLOT_SIZE as u64,
                        ).map_err(|e| format!("pread coord slice: {e}"))?;
                        #[allow(clippy::cast_possible_truncation)]
                        coord_read_ref.fetch_add(t_coord.elapsed().as_millis() as u64, Relaxed);

                        // Resolve each rank group against the local coord slice.
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
                                let bucket = entry.slot_bucket(total_slots);
                                entry.write_to(&mut entry_buf);
                                {
                                    let mut w = slot_buckets.writers[bucket]
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                                    w.write_all(&entry_buf)
                                        .map_err(|e| format!("write slot bucket: {e}"))?;
                                }
                                slot_buckets.entry_counts[bucket]
                                    .fetch_add(1, Relaxed);
                                if is_resolved {
                                    local_resolved += 1;
                                }
                            }
                        }
                        #[allow(clippy::cast_possible_truncation)]
                        resolve_ref.fetch_add(t_resolve.elapsed().as_millis() as u64, Relaxed);

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
        crate::debug::emit_counter("s2_num_workers", num_workers as i64);
    }

    let resolved_count = resolved_total.load(std::sync::atomic::Ordering::Relaxed);
    Ok(resolved_count)
}

pub(super) type SlotBuckets = SharedSlotBuckets;
