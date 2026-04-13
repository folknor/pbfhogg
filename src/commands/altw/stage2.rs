//! Stage 2: Node join — counting-sort per rank bucket, single-pass node merge.
//! Stage 3: Slot reorder — build final coord_slots file.

use std::io::Write as _;
use std::path::Path;

use super::super::external_radix::{BucketWriters, ScratchDir, NUM_BUCKETS};
#[cfg(feature = "linux-direct-io")]
use super::super::external_radix::advise_dontneed_file;
use super::super::id_set_dense::IdSetDense;
use super::super::Result;
use super::{RANK_RECORD_SIZE, RESOLVED_ENTRY_SIZE, COORD_SLOT_SIZE, ResolvedEntry};

// ---------------------------------------------------------------------------
// Stage 2: Node join
// ---------------------------------------------------------------------------

/// For each rank bucket: load records, counting-sort by rank, single-pass
/// node merge using rank order (= node-ID order by construction).
///
/// Uses a pipelined loader thread: one thread loads and counting-sorts
/// the next bucket while the consumer merges the current bucket against
/// the node stream. Queue depth 2 to hide load latency.
#[hotpath::measure]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) fn stage2_node_join(
    input: &Path,
    _direct_io: bool,
    scratch: &ScratchDir,
    rank_bucket_counts: &[u64],
    num_shard_workers: usize,
    slot_buckets: &mut BucketWriters,
    total_slots: u64,
    unique_nodes: u64,
    node_id_set: &IdSetDense,
) -> Result<u64> {
    let mut resolved_count: u64 = 0;
    let rank_range_size = u64::from(unique_nodes).div_ceil(NUM_BUCKETS as u64);

    // A prepared rank bucket: loaded from disk, counting-sorted, ready
    // for the consumer to merge against the node stream.
    struct PreparedBucket {
        bucket_idx: usize,
        grouped_slot_pos: Vec<u64>,
        group_offsets: Vec<u64>,
        max_rank: u64,
        bucket_rank_start: u64,
    }

    /// Reusable scratch buffers for the bucket loader thread.
    struct LoaderScratch {
        data_buf: Vec<u8>,
        counts: Vec<u64>,
        write_pos: Vec<u64>,
    }

    /// Load shard files for one rank bucket, counting-sort by local rank
    /// directly from raw bytes (no intermediate RankRecord Vec), return
    /// a ready-to-consume PreparedBucket.
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

        // Pass 1: load shard bytes and count occurrences per local rank
        // directly from raw bytes — no RankRecord materialization.
        loader.counts.clear();
        loader.counts.resize(local_range, 0);

        // Collect all shard bytes into data_buf (concatenated).
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

        // Count directly from raw bytes (12-byte records: u32 local_rank + u64 slot_pos).
        for chunk in loader.data_buf.chunks_exact(RANK_RECORD_SIZE) {
            let local_rank = u32::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3],
            ]) as usize;
            loader.counts[local_rank] += 1;
        }

        // Prefix sum → offsets.
        let mut group_offsets = vec![0u64; local_range + 1];
        for (i, count) in loader.counts.iter().enumerate() {
            group_offsets[i + 1] = group_offsets[i] + count;
        }

        // Pass 2: scatter slot_pos values directly from raw bytes.
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

        Ok(PreparedBucket {
            bucket_idx,
            grouped_slot_pos,
            group_offsets,
            max_rank: bucket_rank_end,
            bucket_rank_start,
        })
    }

    // Pipelined bucket loader: one thread prepares buckets ahead of the
    // consumer. Queue depth 2 to hide load latency behind merge work.
    let (bucket_tx, bucket_rx) = std::sync::mpsc::sync_channel::<
        std::result::Result<PreparedBucket, String>
    >(2);

    // Parallel node scan (same P2b-v2 pattern as before).
    use super::super::node_scanner::{NodeTuple, extract_node_tuples};
    use std::os::unix::fs::FileExt;

    let mut blob_reader = crate::blob::BlobReader::seekable_from_path(input)?;
    blob_reader.set_parse_indexdata(true);
    blob_reader.next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?
    );

    let mut entry_buf = [0u8; RESOLVED_ENTRY_SIZE];

    type Descriptor = (usize, u64, usize);
    type DecodedItem = (usize, crate::error::Result<Vec<NodeTuple>>);

    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<Descriptor>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (decoded_tx, decoded_rx) = std::sync::mpsc::sync_channel::<DecodedItem>(32);

    let decode_threads = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4);

    let io_error: std::sync::Mutex<Option<crate::error::Error>> = std::sync::Mutex::new(None);

    let s2_pread_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_decompress_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_extract_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_blobs = std::sync::atomic::AtomicU64::new(0);
    let s2_pread_ref = &s2_pread_ms;
    let s2_decompress_ref = &s2_decompress_ms;
    let s2_extract_ref = &s2_extract_ms;
    let s2_blobs_ref = &s2_blobs;

    let mut s2_recv_ms: u64 = 0;
    let mut s2_merge_ms: u64 = 0;
    let mut s2_bucket_load_ms: u64 = 0;
    let mut s2_bucket_loads: u64 = 0;

    let s2_stop = std::sync::atomic::AtomicBool::new(false);
    let s2_stop_ref = &s2_stop;

    std::thread::scope(|scope| -> Result<()> {
        // Bucket loader thread: prepares buckets ahead of the consumer.
        {
            let tx = bucket_tx;
            scope.spawn(move || {
                let mut loader = LoaderScratch {
                    data_buf: Vec::new(),
                    counts: Vec::new(),
                    write_pos: Vec::new(),
                };
                for bucket_idx in 0..NUM_BUCKETS {
                    if s2_stop_ref.load(std::sync::atomic::Ordering::Relaxed) { break; }
                    if rank_bucket_counts[bucket_idx] == 0 { continue; }
                    let result = prepare_bucket(
                        bucket_idx, scratch, num_shard_workers,
                        unique_nodes, rank_range_size,
                        &mut loader,
                    );
                    let is_err = result.is_err();
                    if tx.send(result).is_err() { break; }
                    if is_err { break; }
                }
            });
        }

        let io_error_ref = &io_error;
        scope.spawn(move || {
            let mut seq: usize = 0;
            while let Some(result) = blob_reader.next_header_with_data_offset() {
                match result {
                    Ok((header, _, data_offset, data_size)) => {
                        if !matches!(header.blob_type(), crate::blob::BlobType::OsmData) { continue; }
                        if let Some(idx) = header.index() {
                            if !matches!(idx.kind, crate::blob_index::ElemKind::Node) { continue; }
                        }
                        if desc_tx.send((seq, data_offset, data_size)).is_err() { break; }
                        seq += 1;
                    }
                    Err(e) => {
                        if let Ok(mut guard) = io_error_ref.lock() { *guard = Some(e); }
                        break;
                    }
                }
            }
        });

        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let tx = decoded_tx.clone();
            let file = std::sync::Arc::clone(&shared_file);
            scope.spawn(move || {
                use std::sync::atomic::Ordering::Relaxed;
                let mut read_buf: Vec<u8> = Vec::new();
                let mut decompress_buf: Vec<u8> = Vec::new();
                let mut tuples: Vec<NodeTuple> = Vec::new();
                let mut group_starts: Vec<(usize, usize)> = Vec::new();

                loop {
                    let (seq, data_offset, data_size) = {
                        let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match guard.recv() {
                            Ok(item) => item,
                            Err(_) => break,
                        }
                    };
                    let result = (|| -> crate::error::Result<Vec<NodeTuple>> {
                        let t0 = std::time::Instant::now();
                        read_buf.resize(data_size, 0);
                        file.read_exact_at(&mut read_buf, data_offset)
                            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                        #[allow(clippy::cast_possible_truncation)]
                        s2_pread_ref.fetch_add(t0.elapsed().as_millis() as u64, Relaxed);

                        #[cfg(target_os = "linux")]
                        {
                            use std::os::unix::io::AsRawFd;
                            #[allow(clippy::cast_possible_wrap)]
                            unsafe {
                                libc::posix_fadvise(file.as_raw_fd(), data_offset as i64, data_size as i64, libc::POSIX_FADV_DONTNEED);
                            }
                        }

                        let t1 = std::time::Instant::now();
                        crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)?;
                        #[allow(clippy::cast_possible_truncation)]
                        s2_decompress_ref.fetch_add(t1.elapsed().as_millis() as u64, Relaxed);

                        let t2 = std::time::Instant::now();
                        tuples.clear();
                        extract_node_tuples(&decompress_buf, &mut tuples, &mut group_starts)
                            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(std::io::Error::other(e.to_string()))))?;
                        #[allow(clippy::cast_possible_truncation)]
                        s2_extract_ref.fetch_add(t2.elapsed().as_millis() as u64, Relaxed);

                        s2_blobs_ref.fetch_add(1, Relaxed);
                        Ok(std::mem::take(&mut tuples))
                    })();
                    if tx.send((seq, result)).is_err() { break; }
                }
            });
        }
        drop(desc_rx);
        drop(decoded_tx);

        // Consumer: reorder node tuples + merge against pipelined prepared buckets.
        // The bucket loader thread feeds PreparedBuckets via bucket_rx,
        // overlapping load+sort with merge work.
        let mut reorder: crate::reorder_buffer::ReorderBuffer<crate::error::Result<Vec<NodeTuple>>> =
            crate::reorder_buffer::ReorderBuffer::with_capacity(32);

        // Receive the first prepared bucket.
        let mut current_bucket: Option<PreparedBucket> = match bucket_rx.recv() {
            Ok(Ok(b)) => Some(b),
            Ok(Err(e)) => {
                s2_stop_ref.store(true, std::sync::atomic::Ordering::Relaxed);
                return Err(e.into());
            }
            Err(_) => None,
        };

        loop {
            let t_recv = std::time::Instant::now();
            let msg = decoded_rx.recv();
            #[allow(clippy::cast_possible_truncation)]
            { s2_recv_ms += t_recv.elapsed().as_millis() as u64; }
            let (seq, item) = match msg {
                Ok(v) => v,
                Err(_) => break,
            };

            reorder.push(seq, item);

            while let Some(result) = reorder.pop_ready() {
                let tuples = result?;

                let t_merge = std::time::Instant::now();
                for &NodeTuple { id, lat, lon } in &tuples {
                    let rank = match node_id_set.rank_if_set(id) {
                        Some(r) => r,
                        None => continue,
                    };

                    // Advance to the rank bucket covering this rank.
                    while current_bucket.as_ref().is_none_or(|b| rank >= b.max_rank) {
                        let t_load = std::time::Instant::now();
                        current_bucket = match bucket_rx.recv() {
                            Ok(Ok(b)) => Some(b),
                            Ok(Err(e)) => {
                                s2_stop_ref.store(true, std::sync::atomic::Ordering::Relaxed);
                                return Err(e.into());
                            }
                            Err(_) => None,
                        };
                        #[allow(clippy::cast_possible_truncation)]
                        { s2_bucket_load_ms += t_load.elapsed().as_millis() as u64; }
                        s2_bucket_loads += 1;
                        if current_bucket.is_none() {
                            return Ok(());
                        }
                    }

                    let bkt = current_bucket.as_ref()
                        .ok_or("no bucket available for rank merge")?;

                    #[allow(clippy::cast_possible_truncation)]
                    let local_rank = (rank - bkt.bucket_rank_start) as usize;
                    #[allow(clippy::cast_possible_truncation)]
                    let start = bkt.group_offsets[local_rank] as usize;
                    #[allow(clippy::cast_possible_truncation)]
                    let end = bkt.group_offsets[local_rank + 1] as usize;

                    for &slot_pos in &bkt.grouped_slot_pos[start..end] {
                        let entry = ResolvedEntry { slot_pos, lat, lon };
                        let bucket = entry.slot_bucket(total_slots);
                        entry.write_to(&mut entry_buf);
                        if let Some(writer) = slot_buckets.writers[bucket].as_mut() {
                            writer.write_all(&entry_buf)?;
                        }
                        slot_buckets.entry_counts[bucket] += 1;
                        resolved_count += 1;
                    }
                }
                #[allow(clippy::cast_possible_truncation)]
                { s2_merge_ms += t_merge.elapsed().as_millis() as u64; }

                drop(tuples);
            }
        }

        Ok(())
    })?;

    if let Some(e) = io_error.into_inner().unwrap_or(None) {
        return Err(Box::new(e));
    }

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("s2_pread_ms", s2_pread_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_decompress_ms", s2_decompress_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_extract_ms", s2_extract_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_blobs", s2_blobs.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_consumer_recv_ms", s2_recv_ms as i64);
        crate::debug::emit_counter("s2_consumer_merge_ms", s2_merge_ms as i64);
        crate::debug::emit_counter("s2_bucket_load_ms", s2_bucket_load_ms as i64);
        crate::debug::emit_counter("s2_bucket_loads", s2_bucket_loads as i64);
    }

    Ok(resolved_count)
}

// ---------------------------------------------------------------------------
// Stage 3: Slot reorder — build final coord_slots file
// ---------------------------------------------------------------------------

/// Parallel slot reorder: workers claim buckets via AtomicUsize, load
/// slot bucket file, scatter entries into a local buffer, pwrite to
/// the pre-sized coord_slots file at each bucket's disjoint byte range.
///
/// Lightweight reference to slot bucket paths + counts for stage 3.
/// Used when resuming from `--start-stage 3` without a live `BucketWriters`.
pub(super) struct SlotBucketRef {
    pub(super) paths: Vec<std::path::PathBuf>,
    pub(super) entry_counts: Vec<u64>,
}

/// Stage 3 from a `BucketWriters` (normal path).
#[hotpath::measure]
#[allow(clippy::cast_possible_truncation)]
pub(super) fn stage3_slot_reorder(
    slot_buckets: &BucketWriters,
    coord_slots_path: &Path,
    total_slots: u64,
) -> Result<()> {
    let r = SlotBucketRef {
        paths: slot_buckets.paths.clone(),
        entry_counts: slot_buckets.entry_counts.clone(),
    };
    stage3_slot_reorder_from_ref(&r, coord_slots_path, total_slots)
}

/// Pre-allocates the output file to `total_slots * 8` bytes (zero-filled
/// by the OS). Empty buckets need no explicit zero-write — the file is
/// already zeroed.
#[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
pub(super) fn stage3_slot_reorder_from_ref(
    slot_buckets: &SlotBucketRef,
    coord_slots_path: &Path,
    total_slots: u64,
) -> Result<()> {
    use std::os::unix::fs::FileExt as _;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(coord_slots_path)
        .map_err(|e| format!("create coord_slots: {e}"))?;
    let total_bytes = total_slots * COORD_SLOT_SIZE as u64;
    file.set_len(total_bytes)
        .map_err(|e| format!("ftruncate coord_slots to {total_bytes}: {e}"))?;
    let shared_file = std::sync::Arc::new(file);

    let range_size = total_slots.div_ceil(NUM_BUCKETS as u64);

    // Cap workers for I/O-heavy stage — too many workers thrash the disk.
    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4)
        .min(6);

    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let s3_load_ms = std::sync::atomic::AtomicU64::new(0);
    let s3_scatter_ms = std::sync::atomic::AtomicU64::new(0);
    let s3_write_ms = std::sync::atomic::AtomicU64::new(0);
    let s3_buckets_loaded = std::sync::atomic::AtomicU64::new(0);
    let s3_error: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

    let next_ref = &next_idx;
    let s3_load_ref = &s3_load_ms;
    let s3_scatter_ref = &s3_scatter_ms;
    let s3_write_ref = &s3_write_ms;
    let s3_loaded_ref = &s3_buckets_loaded;
    let err_ref = &s3_error;
    let entry_counts = &slot_buckets.entry_counts;
    let paths = &slot_buckets.paths;

    std::thread::scope(|scope| {
        for _ in 0..num_workers {
            let file = std::sync::Arc::clone(&shared_file);
            scope.spawn(move || {
                use std::sync::atomic::Ordering::Relaxed;
                let mut data_buf: Vec<u8> = Vec::new();
                let mut scatter_buf: Vec<u8> = Vec::new();
                let mut buf = [0u8; RESOLVED_ENTRY_SIZE];

                loop {
                    if err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some() {
                        break;
                    }
                    let bucket_idx = next_ref.fetch_add(1, Relaxed);
                    if bucket_idx >= NUM_BUCKETS { break; }

                    let bucket_start = bucket_idx as u64 * range_size;
                    let bucket_end = if bucket_idx == NUM_BUCKETS - 1 {
                        total_slots
                    } else {
                        ((bucket_idx as u64 + 1) * range_size).min(total_slots)
                    };
                    let bucket_slots = bucket_end - bucket_start;

                    if entry_counts[bucket_idx] == 0 {
                        // Pre-sized file is already zeroed — nothing to write.
                        continue;
                    }

                    let result: std::result::Result<(), String> = (|| {
                        let bucket_bytes = bucket_slots as usize * COORD_SLOT_SIZE;
                        scatter_buf.clear();
                        scatter_buf.resize(bucket_bytes, 0);

                        let t_load = std::time::Instant::now();
                        data_buf.clear();
                        let bucket_file = std::fs::File::open(&paths[bucket_idx])
                            .map_err(|e| format!("open slot bucket: {e}"))?;
                        std::io::Read::read_to_end(&mut &bucket_file, &mut data_buf)
                            .map_err(|e| format!("read slot bucket: {e}"))?;
                        #[cfg(feature = "linux-direct-io")]
                        advise_dontneed_file(&bucket_file);
                        s3_load_ref.fetch_add(t_load.elapsed().as_millis() as u64, Relaxed);

                        let t_scatter = std::time::Instant::now();
                        for chunk in data_buf.chunks_exact(RESOLVED_ENTRY_SIZE) {
                            buf.copy_from_slice(chunk);
                            let entry = ResolvedEntry::read_from(&buf);
                            let local_pos = (entry.slot_pos - bucket_start) as usize;
                            let offset = local_pos * COORD_SLOT_SIZE;
                            scatter_buf[offset..offset + 4].copy_from_slice(&entry.lat.to_le_bytes());
                            scatter_buf[offset + 4..offset + 8].copy_from_slice(&entry.lon.to_le_bytes());
                        }
                        s3_scatter_ref.fetch_add(t_scatter.elapsed().as_millis() as u64, Relaxed);

                        let t_write = std::time::Instant::now();
                        let file_offset = bucket_start * COORD_SLOT_SIZE as u64;
                        file.write_all_at(&scatter_buf, file_offset)
                            .map_err(|e| format!("pwrite coord_slots: {e}"))?;
                        s3_write_ref.fetch_add(t_write.elapsed().as_millis() as u64, Relaxed);

                        s3_loaded_ref.fetch_add(1, Relaxed);
                        Ok(())
                    })();

                    if let Err(e) = result {
                        *err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e);
                        break;
                    }
                }
            });
        }
    });

    if let Some(e) = s3_error.into_inner().unwrap_or(None) {
        return Err(e.into());
    }

    // Sync to ensure all pwrite data is flushed before mmap in stage 4.
    shared_file.sync_data()
        .map_err(|e| format!("sync coord_slots: {e}"))?;

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("s3_load_ms", s3_load_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s3_scatter_ms", s3_scatter_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s3_write_ms", s3_write_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s3_buckets_loaded", s3_buckets_loaded.load(std::sync::atomic::Ordering::Relaxed) as i64);
    }

    Ok(())
}
