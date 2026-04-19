//! Stage 2: Node join - parallel counting-sort per rank bucket, inline
//! per-bucket coordinate resolution from node blobs.
//!
//! Replaces the historical 82 GB `coords_by_rank` file pread with a direct
//! per-bucket node-blob decode: each worker uses the stage 1 `NodeBlobInfo`
//! mapping to find which blobs cover its bucket's rank range, preads and
//! decompresses them, runs `extract_node_tuples`, and writes resolved
//! coordinates into a per-bucket `coord_slice` keyed by `(rank - R_lo)`.
//! The slice is then consumed by the existing rank-record join unchanged.

use std::io::Write as _;
use std::sync::Arc;

use super::radix::{ScratchDir, NUM_BUCKETS};
#[cfg(feature = "linux-direct-io")]
use super::radix::advise_dontneed_file;
use crate::idset::IdSet;
use crate::scan::node::{extract_node_tuples, NodeTuple};
use super::super::Result;
use super::{
    slot_bucket_bounds, NodeBlobInfo, RANK_RECORD_SIZE, RESOLVED_ENTRY_SIZE, COORD_SLOT_SIZE,
    ResolvedEntry,
};

// ---------------------------------------------------------------------------
// Stage 2: Parallel node join - coord slice lookup
// ---------------------------------------------------------------------------

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
    // fadvise_* only mutated under feature = "linux-direct-io".
    #[allow(unused_mut)]
    let mut fadvise_calls: u64 = 0;
    #[allow(unused_mut)]
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
}

const BUCKET_BUF_SIZE: usize = 256 * 1024;

impl SharedSlotBuckets {
    pub(super) fn create(
        scratch: &ScratchDir,
        slot_bucket_count: usize,
    ) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let mut writers = Vec::with_capacity(slot_bucket_count);
        let mut entry_counts = Vec::with_capacity(slot_bucket_count);

        for i in 0..slot_bucket_count {
            let path = scratch.bucket_path("slot", i);
            let file = std::fs::File::create(&path)
                .map_err(|e| format!("failed to create slot bucket {}: {e}", path.display()))?;
            writers.push(std::sync::Mutex::new(
                std::io::BufWriter::with_capacity(BUCKET_BUF_SIZE, file),
            ));
            entry_counts.push(std::sync::atomic::AtomicU64::new(0));
        }

        Ok(Self { writers, entry_counts })
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

}

/// Parallel stage 2: N workers each claim rank buckets via atomic dispatch,
/// load rank records, counting-sort, fill the bucket's coord slice by
/// decoding the node blobs covering its rank range (using the stage 1
/// `NodeBlobInfo` mapping), resolve to `(lat, lon)`, write to shared slot
/// bucket files (256 files total, per-bucket mutex).
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
    input_pbf: &Arc<std::fs::File>,
    node_id_set: &IdSet,
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
    // Wall time spent populating coord_slice (node-blob pread + decompress
    // + extract + rank fill). Replaces the old s2_coord_read_ms metric.
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
    // Buckets for which we touched at least one straddler blob (a blob
    // whose rank range crosses our bucket boundary). Each straddler is
    // re-decompressed by the adjacent bucket's worker, so high values
    // mean wasted decode work.
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
    let err_ref = &s2_error;

    std::thread::scope(|scope| {
        for _ in 0..num_workers {
            let pbf_file = Arc::clone(input_pbf);
            scope.spawn(move || {
                use std::sync::atomic::Ordering::Relaxed;

                let mut loader = LoaderScratch::new();
                // Pre-size once to the per-bucket worst case (the nominal
                // rank_range_size; the last bucket may be smaller but
                // never larger). The slice is fully zeroed at the start
                // of each bucket and then populated only at slots where
                // we decode a referenced node - the resolve loop below
                // depends on (lat==0 && lon==0) as the missing-coord
                // sentinel, so leftover bytes from a previous bucket
                // would be silently misresolved as real coordinates.
                #[allow(clippy::cast_possible_truncation)]
                let max_slice_bytes = (rank_range_size as usize) * COORD_SLOT_SIZE;
                let mut coord_slice: Vec<u8> = vec![0u8; max_slice_bytes];
                let mut node_read_buf: Vec<u8> = Vec::new();
                let mut node_decompress_buf: Vec<u8> = Vec::new();
                let mut node_tuples: Vec<NodeTuple> = Vec::new();
                let mut node_group_starts: Vec<(usize, usize)> = Vec::new();
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

                        // Build this bucket's coord slice by decoding the
                        // node blobs whose referenced-rank ranges intersect
                        // [bucket_rank_start, bucket_rank_end).
                        //
                        // Correctness: the slice is reused across buckets
                        // by this worker, so it MUST be zeroed before the
                        // fill loop - only positions where a referenced
                        // node lands get overwritten, and the resolve loop
                        // depends on (lat==0 && lon==0) as the missing
                        // sentinel. Without the zero, leftover bytes from
                        // a previous bucket would silently look like real
                        // resolved coordinates.
                        let t_coord = std::time::Instant::now();
                        let slice_bytes = bkt.local_range * COORD_SLOT_SIZE;
                        let t_zero = std::time::Instant::now();
                        coord_slice[..slice_bytes].fill(0);
                        #[allow(clippy::cast_possible_truncation)]
                        coord_zero_ms_ref.fetch_add(t_zero.elapsed().as_millis() as u64, Relaxed);
                        #[allow(clippy::cast_possible_truncation)]
                        coord_zero_ns_ref.fetch_add(t_zero.elapsed().as_nanos() as u64, Relaxed);
                        let bucket_rank_start = bkt.bucket_rank_start;
                        let bucket_rank_end = bucket_rank_start + bkt.local_range as u64;

                        // Binary search the mapping for the contiguous run
                        // of blobs intersecting this bucket. Because the
                        // input PBF is ID-sorted and rank is monotonic in
                        // ID, blob rank ranges are non-overlapping and
                        // monotonic; intersecting blobs form a contiguous
                        // [lo, hi) slice of `mapping_ref`.
                        let lo = mapping_ref.partition_point(
                            |b| b.ref_rank_end <= bucket_rank_start,
                        );
                        let hi = mapping_ref.partition_point(
                            |b| b.ref_rank_start < bucket_rank_end,
                        );
                        let mut bucket_blobs_read: u64 = 0;
                        let mut bucket_straddlers: u64 = 0;
                        for blob in &mapping_ref[lo..hi] {
                            if blob.ref_count() == 0 { continue; }
                            // A blob is a straddler if it extends past
                            // either bucket boundary - i.e. it will also
                            // be touched by an adjacent bucket worker.
                            if blob.ref_rank_start < bucket_rank_start
                                || blob.ref_rank_end > bucket_rank_end
                            {
                                bucket_straddlers += 1;
                            }

                            let t_pr = std::time::Instant::now();
                            node_read_buf.resize(blob.data_size, 0);
                            pbf_file.read_exact_at(&mut node_read_buf, blob.data_offset)
                                .map_err(|e| format!("stage2 node pread: {e}"))?;
                            #[allow(clippy::cast_possible_truncation)]
                            node_pread_ref.fetch_add(t_pr.elapsed().as_millis() as u64, Relaxed);
                            #[allow(clippy::cast_possible_truncation)]
                            node_pread_ns_ref.fetch_add(t_pr.elapsed().as_nanos() as u64, Relaxed);
                            node_bytes_ref.fetch_add(blob.data_size as u64, Relaxed);

                            let t_dc = std::time::Instant::now();
                            crate::blob::decompress_blob_raw(&node_read_buf, &mut node_decompress_buf)
                                .map_err(|e| format!("stage2 node decompress: {e}"))?;
                            #[allow(clippy::cast_possible_truncation)]
                            node_decompress_ref.fetch_add(t_dc.elapsed().as_millis() as u64, Relaxed);
                            #[allow(clippy::cast_possible_truncation)]
                            node_decompress_ns_ref.fetch_add(t_dc.elapsed().as_nanos() as u64, Relaxed);

                            let t_ex = std::time::Instant::now();
                            node_tuples.clear();
                            extract_node_tuples(
                                &node_decompress_buf, &mut node_tuples, &mut node_group_starts,
                            ).map_err(|e| format!("stage2 node extract: {e}"))?;
                            #[allow(clippy::cast_possible_truncation)]
                            node_extract_ref.fetch_add(t_ex.elapsed().as_millis() as u64, Relaxed);
                            #[allow(clippy::cast_possible_truncation)]
                            node_extract_ns_ref.fetch_add(t_ex.elapsed().as_nanos() as u64, Relaxed);

                            let t_rk = std::time::Instant::now();
                            // Blob-local rank assignment: node blobs are
                            // ID-sorted and `extract_node_tuples` emits in ID
                            // order, so the referenced nodes in this blob
                            // occupy exactly [ref_rank_start, ref_rank_end).
                            // We assign ranks by incrementing `next_rank`
                            // instead of calling `rank_if_set` per tuple -
                            // membership becomes an O(1) `get()` bit test.
                            let mut next_rank = blob.ref_rank_start;
                            #[cfg(debug_assertions)]
                            let mut prev_id: Option<i64> = None;
                            for &NodeTuple { id, lat, lon } in &node_tuples {
                                #[cfg(debug_assertions)]
                                {
                                    if let Some(p) = prev_id {
                                        debug_assert!(
                                            id >= p,
                                            "extract_node_tuples non-monotonic: {p} then {id}",
                                        );
                                    }
                                    prev_id = Some(id);
                                }
                                if !id_set_ref.get(id) {
                                    continue;
                                }
                                let rank = next_rank;
                                next_rank += 1;
                                if rank < bucket_rank_start || rank >= bucket_rank_end {
                                    // Belongs to an adjacent bucket - skip.
                                    continue;
                                }
                                #[allow(clippy::cast_possible_truncation)]
                                let local_rank = (rank - bucket_rank_start) as usize;
                                let off = local_rank * COORD_SLOT_SIZE;
                                coord_slice[off..off + 4].copy_from_slice(&lat.to_le_bytes());
                                coord_slice[off + 4..off + 8].copy_from_slice(&lon.to_le_bytes());
                            }
                            debug_assert_eq!(
                                next_rank, blob.ref_rank_end,
                                "blob-local rank drift: expected {} hits, got {}",
                                blob.ref_count(),
                                next_rank - blob.ref_rank_start,
                            );
                            #[allow(clippy::cast_possible_truncation)]
                            node_rank_ref.fetch_add(t_rk.elapsed().as_millis() as u64, Relaxed);
                            #[allow(clippy::cast_possible_truncation)]
                            node_rank_ns_ref.fetch_add(t_rk.elapsed().as_nanos() as u64, Relaxed);
                            bucket_blobs_read += 1;
                        }
                        node_blobs_ref.fetch_add(bucket_blobs_read, Relaxed);
                        node_straddler_ref.fetch_add(bucket_straddlers, Relaxed);
                        #[allow(clippy::cast_possible_truncation)]
                        coord_fill_ref.fetch_add(t_coord.elapsed().as_millis() as u64, Relaxed);
                        pread_calls_ref.fetch_add(bucket_blobs_read, Relaxed);

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
                            // KNOWN LIMITATION: (0, 0) doubles as the
                            // unresolved-coord sentinel (coord slots are
                            // zero-filled before the node scan fills them)
                            // AND as a legitimate OSM node at Null Island.
                            // A real node at 0°, 0° will be counted here as
                            // unresolved (under-counting `resolved_rank`) and
                            // still get written as (0, 0) to ways that
                            // reference it. A proper fix is a presence bitmap
                            // alongside the coord array. Same pattern exists
                            // in the geocode builder (`src/geocode_index/builder.rs`
                            // `coords` filter_map); fix both together if the
                            // contract ever changes.
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
    }

    let resolved_count = resolved_total.load(std::sync::atomic::Ordering::Relaxed);
    Ok(resolved_count)
}

pub(super) type SlotBuckets = SharedSlotBuckets;
