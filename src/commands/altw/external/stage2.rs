//! Stage 2: Node join - parallel id-bucket merge join.
//!
//! Each worker claims an id-bucket via atomic dispatch, loads the
//! IdRecords pass A wrote into that bucket's shard files, sorts them
//! by `local_node_id`, finds the contiguous run of node blobs whose
//! `[min_id, max_id]` intersects the bucket's id range, decodes those
//! blobs, and walks records and node tuples in lockstep (both
//! ID-sorted) to resolve coordinates. Each resolved record is written
//! to a shared slot bucket file as a `ResolvedEntry`.
//!
//! Replaces the rank-index path (counting-sort by `local_rank`,
//! `coord_slice` keyed by `(rank - bucket_rank_start)`,
//! `next_rank` per-blob counter, drift canary on
//! `next_rank == ref_rank_end`) with a streaming merge-walk that
//! computes `linear_slot_pos = blob_start_slot[record.blob_idx] +
//! record.blob_local_slot` per record. The `coord_slice` allocation
//! goes away entirely (would have ballooned to ~440 MB per worker
//! if sized to `bucket_width` rather than `rank_range_size`).
//!
//! Records whose global id has no matching node tuple in any blob
//! within the bucket's range are *orphans* - left unemitted. Stage 4
//! fills zero coordinates for any slot_pos not covered by a
//! `ResolvedEntry`, so orphan records produce the same end behaviour
//! as way refs to absent nodes.

use std::io::Write as _;
use std::sync::Arc;

use super::radix::{ScratchDir, NUM_BUCKETS};
#[cfg(feature = "linux-direct-io")]
use super::radix::advise_dontneed_file;
use crate::scan::node::{extract_node_tuples, NodeTuple};
use super::super::Result;
use super::{
    slot_bucket_bounds, BucketLayout, IdRecord, NodeBlobInfo, RESOLVED_ENTRY_SIZE, ID_RECORD_SIZE,
    ResolvedEntry,
};

// ---------------------------------------------------------------------------
// Stage 2: Parallel node join - id-bucket merge walk
// ---------------------------------------------------------------------------

struct LoaderScratch {
    data_buf: Vec<u8>,
    records: Vec<IdRecord>,
}

impl LoaderScratch {
    fn new() -> Self {
        Self { data_buf: Vec::new(), records: Vec::new() }
    }
}

struct PreparedBucket {
    /// Records for this bucket, sorted by `local_node_id`.
    records: Vec<IdRecord>,
    /// I/O accounting from shard loading.
    open_calls: u64,
    stat_calls: u64,
    fadvise_calls: u64,
    fadvise_bytes: u64,
    /// Sub-phase timings (ms).
    parse_ms: u64,
    sort_ms: u64,
}

#[allow(clippy::cast_possible_truncation)]
fn prepare_bucket(
    bucket_idx: usize,
    scratch: &ScratchDir,
    num_shard_workers: usize,
    loader: &mut LoaderScratch,
) -> std::result::Result<PreparedBucket, String> {
    loader.data_buf.clear();
    loader.records.clear();
    let mut open_calls: u64 = 0;
    let mut stat_calls: u64 = 0;
    // fadvise_* only mutated under feature = "linux-direct-io".
    #[allow(unused_mut)]
    let mut fadvise_calls: u64 = 0;
    #[allow(unused_mut)]
    let mut fadvise_bytes: u64 = 0;

    for worker_id in 0..num_shard_workers {
        let path = scratch.path.join(format!("id-W{worker_id}-{bucket_idx:03}"));
        let file = match std::fs::File::open(&path) {
            Ok(f) => { open_calls += 1; f }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("open id shard: {e}")),
        };
        stat_calls += 1;
        let len = file.metadata()
            .map_err(|e| format!("stat id shard: {e}"))?
            .len() as usize;
        if len == 0 { continue; }
        let start = loader.data_buf.len();
        loader.data_buf.resize(start + len, 0);
        std::io::Read::read_exact(&mut &file, &mut loader.data_buf[start..])
            .map_err(|e| format!("read id shard: {e}"))?;
        #[cfg(feature = "linux-direct-io")]
        {
            fadvise_calls += 1;
            fadvise_bytes += len as u64;
            advise_dontneed_file(&file);
        }
    }

    // Parse 12-byte chunks into IdRecords.
    let t_parse = std::time::Instant::now();
    let count = loader.data_buf.len() / ID_RECORD_SIZE;
    loader.records.reserve(count);
    for chunk in loader.data_buf.chunks_exact(ID_RECORD_SIZE) {
        let buf: &[u8; ID_RECORD_SIZE] = chunk.try_into()
            .map_err(|_| "chunks_exact returned non-12-byte chunk".to_string())?;
        loader.records.push(IdRecord::read_from(buf));
    }
    let parse_ms = t_parse.elapsed().as_millis() as u64;

    // Sort by local_node_id so the merge walk against ID-sorted node
    // tuples advances both pointers monotonically.
    let t_sort = std::time::Instant::now();
    loader.records.sort_unstable_by_key(|r| r.local_node_id);
    let sort_ms = t_sort.elapsed().as_millis() as u64;

    Ok(PreparedBucket {
        records: std::mem::take(&mut loader.records),
        open_calls, stat_calls, fadvise_calls, fadvise_bytes,
        parse_ms, sort_ms,
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

/// Parallel stage 2: N workers each claim id buckets via atomic
/// dispatch, load + sort the bucket's IdRecords, find the contiguous
/// run of node blobs whose `[min_id, max_id]` intersects the bucket's
/// id range, decode those blobs, walk records and node tuples in
/// lockstep, and emit `ResolvedEntry` to per-bucket slot writers
/// (256 files total, per-bucket mutex). Returns the count of records
/// resolved to non-zero coordinates.
#[hotpath::measure]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) fn stage2_node_join(
    scratch: &ScratchDir,
    id_bucket_counts: &[u64],
    num_shard_workers: usize,
    bucket_layout: &BucketLayout,
    way_slot_starts: &[u64],
    slot_buckets: &SharedSlotBuckets,
    slot_bucket_count: usize,
    total_slots: u64,
    input_pbf: &Arc<std::fs::File>,
    node_blob_mapping: &[NodeBlobInfo],
) -> Result<u64> {
    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4)
        .min(6);

    use std::os::unix::fs::FileExt as _;

    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let resolved_total = std::sync::atomic::AtomicU64::new(0);
    let s2_node_pread_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_node_decompress_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_node_extract_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_node_walk_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_node_pread_ns = std::sync::atomic::AtomicU64::new(0);
    let s2_node_decompress_ns = std::sync::atomic::AtomicU64::new(0);
    let s2_node_extract_ns = std::sync::atomic::AtomicU64::new(0);
    let s2_node_walk_ns = std::sync::atomic::AtomicU64::new(0);
    let s2_node_bytes_read = std::sync::atomic::AtomicU64::new(0);
    let s2_node_blobs_read = std::sync::atomic::AtomicU64::new(0);
    // Buckets for which we touched at least one straddler blob (a blob
    // whose id range crosses the bucket boundary). Each straddler is
    // re-decompressed by adjacent workers; high values mean wasted
    // decode work. With id-bucketing, a node blob's id range can be
    // wider than the bucket width, so straddler density is expected
    // to differ from the rank-bucketing baseline - watch this metric
    // when comparing pre/post-A1 traces.
    let s2_node_straddler_blobs = std::sync::atomic::AtomicU64::new(0);
    let s2_resolve_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_bucket_load_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_bucket_loads = std::sync::atomic::AtomicU64::new(0);
    let s2_prepare_parse_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_prepare_sort_ms = std::sync::atomic::AtomicU64::new(0);
    let s2_orphan_records = std::sync::atomic::AtomicU64::new(0);
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
    let node_pread_ref = &s2_node_pread_ms;
    let node_decompress_ref = &s2_node_decompress_ms;
    let node_extract_ref = &s2_node_extract_ms;
    let node_walk_ref = &s2_node_walk_ms;
    let node_pread_ns_ref = &s2_node_pread_ns;
    let node_decompress_ns_ref = &s2_node_decompress_ns;
    let node_extract_ns_ref = &s2_node_extract_ns;
    let node_walk_ns_ref = &s2_node_walk_ns;
    let node_bytes_ref = &s2_node_bytes_read;
    let node_blobs_ref = &s2_node_blobs_read;
    let node_straddler_ref = &s2_node_straddler_blobs;
    let mapping_ref = node_blob_mapping;
    let resolve_ref = &s2_resolve_ms;
    let load_ref = &s2_bucket_load_ms;
    let loads_ref = &s2_bucket_loads;
    let prepare_parse_ref = &s2_prepare_parse_ms;
    let prepare_sort_ref = &s2_prepare_sort_ms;
    let orphan_ref = &s2_orphan_records;
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
    let layout_ref = bucket_layout;
    let slot_starts_ref = way_slot_starts;

    std::thread::scope(|scope| {
        for _ in 0..num_workers {
            let pbf_file = Arc::clone(input_pbf);
            scope.spawn(move || {
                use std::sync::atomic::Ordering::Relaxed;

                let mut loader = LoaderScratch::new();
                let mut node_read_buf: Vec<u8> = Vec::new();
                let mut node_decompress_buf: Vec<u8> = Vec::new();
                let mut node_tuples: Vec<NodeTuple> = Vec::new();
                let mut node_group_starts: Vec<(usize, usize)> = Vec::new();
                let mut entry_buf = [0u8; RESOLVED_ENTRY_SIZE];
                let mut local_resolved: u64 = 0;

                // Per-slot-bucket local buffers. Flushed when any buffer
                // exceeds FLUSH_THRESHOLD, and at the end of each id bucket.
                const FLUSH_THRESHOLD: usize = 256 * 1024;
                let mut slot_bufs: Vec<Vec<u8>> =
                    (0..slot_bucket_count).map(|_| Vec::new()).collect();
                let mut slot_counts: Vec<u64> = vec![0; slot_bucket_count];

                loop {
                    if err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some() {
                        break;
                    }
                    let bucket_idx = next_ref.fetch_add(1, Relaxed);
                    if bucket_idx >= NUM_BUCKETS { break; }
                    if id_bucket_counts[bucket_idx] == 0 { continue; }

                    let result: std::result::Result<(), String> = (|| {
                        // Load + sort id-bucket records.
                        let t_load = std::time::Instant::now();
                        let bkt = prepare_bucket(
                            bucket_idx, scratch, num_shard_workers, &mut loader,
                        )?;
                        #[allow(clippy::cast_possible_truncation)]
                        load_ref.fetch_add(t_load.elapsed().as_millis() as u64, Relaxed);
                        loads_ref.fetch_add(1, Relaxed);
                        prepare_parse_ref.fetch_add(bkt.parse_ms, Relaxed);
                        prepare_sort_ref.fetch_add(bkt.sort_ms, Relaxed);
                        open_calls_ref.fetch_add(bkt.open_calls, Relaxed);
                        stat_calls_ref.fetch_add(bkt.stat_calls, Relaxed);
                        fadvise_calls_ref.fetch_add(bkt.fadvise_calls, Relaxed);
                        fadvise_bytes_ref.fetch_add(bkt.fadvise_bytes, Relaxed);

                        if bkt.records.is_empty() {
                            return Ok(());
                        }

                        // Bucket id range: [bucket_lo, bucket_hi). The
                        // last bucket extends to max_node_id (inclusive)
                        // so its hi is max_node_id + 1.
                        #[allow(clippy::cast_possible_truncation)]
                        let bucket_lo = (bucket_idx as u64) * layout_ref.bucket_width;
                        let bucket_hi = if bucket_idx == NUM_BUCKETS - 1 {
                            layout_ref.max_node_id + 1
                        } else {
                            (bucket_idx as u64 + 1) * layout_ref.bucket_width
                        };

                        // Find the contiguous run of node blobs whose
                        // [min_id, max_id] intersects [bucket_lo, bucket_hi).
                        // PBFs are id-sorted so blob ranges are
                        // monotonic and non-overlapping; intersecting
                        // blobs form a contiguous slice.
                        #[allow(clippy::cast_sign_loss)]
                        let lo_blob = mapping_ref.partition_point(|b| {
                            (b.max_id as u64) < bucket_lo
                        });
                        #[allow(clippy::cast_sign_loss)]
                        let hi_blob = mapping_ref.partition_point(|b| {
                            (b.min_id as u64) < bucket_hi
                        });

                        let t_resolve = std::time::Instant::now();
                        let mut record_ptr = 0usize;
                        let mut bucket_blobs_read: u64 = 0;
                        let mut bucket_straddlers: u64 = 0;

                        for blob in &mapping_ref[lo_blob..hi_blob] {
                            // Straddler: blob extends past either bucket
                            // boundary, so it's also touched by an
                            // adjacent worker.
                            #[allow(clippy::cast_sign_loss)]
                            let blob_min = blob.min_id as u64;
                            #[allow(clippy::cast_sign_loss)]
                            let blob_max = blob.max_id as u64;
                            if blob_min < bucket_lo || blob_max >= bucket_hi {
                                bucket_straddlers += 1;
                            }

                            // pread + decompress + extract.
                            let t_pr = std::time::Instant::now();
                            node_read_buf.resize(blob.data_size, 0);
                            pbf_file.read_exact_at(&mut node_read_buf, blob.data_offset)
                                .map_err(|e| format!("stage2 node pread: {e}"))?;
                            #[allow(clippy::cast_possible_truncation)]
                            node_pread_ref.fetch_add(
                                t_pr.elapsed().as_millis() as u64, Relaxed,
                            );
                            #[allow(clippy::cast_possible_truncation)]
                            node_pread_ns_ref.fetch_add(
                                t_pr.elapsed().as_nanos() as u64, Relaxed,
                            );
                            node_bytes_ref.fetch_add(blob.data_size as u64, Relaxed);

                            let t_dc = std::time::Instant::now();
                            crate::blob::decompress_blob_raw(
                                &node_read_buf, &mut node_decompress_buf,
                            ).map_err(|e| format!("stage2 node decompress: {e}"))?;
                            #[allow(clippy::cast_possible_truncation)]
                            node_decompress_ref.fetch_add(
                                t_dc.elapsed().as_millis() as u64, Relaxed,
                            );
                            #[allow(clippy::cast_possible_truncation)]
                            node_decompress_ns_ref.fetch_add(
                                t_dc.elapsed().as_nanos() as u64, Relaxed,
                            );

                            let t_ex = std::time::Instant::now();
                            node_tuples.clear();
                            extract_node_tuples(
                                &node_decompress_buf, &mut node_tuples, &mut node_group_starts,
                            ).map_err(|e| format!("stage2 node extract: {e}"))?;
                            #[allow(clippy::cast_possible_truncation)]
                            node_extract_ref.fetch_add(
                                t_ex.elapsed().as_millis() as u64, Relaxed,
                            );
                            #[allow(clippy::cast_possible_truncation)]
                            node_extract_ns_ref.fetch_add(
                                t_ex.elapsed().as_nanos() as u64, Relaxed,
                            );

                            // Merge-walk: records are sorted by
                            // local_node_id; node_tuples are id-sorted
                            // (PBF invariant + extract_node_tuples
                            // preserves order). Advance both pointers
                            // monotonically. The walk's upper bound is
                            // `node_tuples.last().id` (the last id this
                            // blob actually contains), NOT
                            // `blob.max_id` from indexdata. Indexdata
                            // is a producer claim and can overstate the
                            // real range; using the actual extracted
                            // last-id avoids consuming a record as
                            // "orphan in this blob" when it should be
                            // resolved by a later blob whose
                            // indexdata-claimed `min_id` overlaps the
                            // current blob's overstated `max_id`. The
                            // old rank path's `next_rank == ref_rank_end`
                            // canary failed loud on this class of bug;
                            // the new walk uses the decoded stream as
                            // the source of truth instead.
                            //
                            // Records that find no matching tuple
                            // (true orphans - id absent from PBF) emit
                            // nothing; stage 4 fills zero coords for
                            // any slot_pos not covered by a
                            // ResolvedEntry, matching the existing
                            // missing-ref behaviour.
                            let t_walk = std::time::Instant::now();
                            let blob_actual_upper: Option<u64> = node_tuples
                                .last()
                                .and_then(|t| u64::try_from(t.id).ok());
                            let mut tuple_ptr = 0usize;
                            #[cfg(debug_assertions)]
                            let mut prev_tuple_id: Option<i64> = None;
                            while record_ptr < bkt.records.len() {
                                let record = &bkt.records[record_ptr];
                                let global_id = bucket_lo + u64::from(record.local_node_id);
                                let upper = match blob_actual_upper {
                                    Some(u) => u,
                                    None => break,
                                };
                                if global_id > upper {
                                    break;
                                }
                                #[allow(clippy::cast_sign_loss)]
                                while tuple_ptr < node_tuples.len()
                                    && (node_tuples[tuple_ptr].id as u64) < global_id
                                {
                                    #[cfg(debug_assertions)]
                                    {
                                        let t_id = node_tuples[tuple_ptr].id;
                                        if let Some(p) = prev_tuple_id {
                                            debug_assert!(
                                                t_id >= p,
                                                "extract_node_tuples non-monotonic: {p} then {t_id}",
                                            );
                                        }
                                        prev_tuple_id = Some(t_id);
                                    }
                                    tuple_ptr += 1;
                                }
                                #[allow(clippy::cast_sign_loss)]
                                let resolved = tuple_ptr < node_tuples.len()
                                    && (node_tuples[tuple_ptr].id as u64) == global_id;
                                if resolved {
                                    let tuple = &node_tuples[tuple_ptr];
                                    let blob_start = slot_starts_ref
                                        .get(record.blob_idx as usize)
                                        .copied()
                                        .ok_or_else(|| {
                                            format!(
                                                "stage2: record blob_idx {} out of range \
                                                 (way_slot_starts.len()={})",
                                                record.blob_idx,
                                                slot_starts_ref.len(),
                                            )
                                        })?;
                                    let linear_slot_pos =
                                        blob_start + u64::from(record.blob_local_slot);
                                    let entry = ResolvedEntry {
                                        slot_pos: linear_slot_pos,
                                        lat: tuple.lat,
                                        lon: tuple.lon,
                                    };
                                    let bucket = entry.slot_bucket(total_slots, slot_bucket_count);
                                    let (bucket_start, _bucket_end) =
                                        slot_bucket_bounds(total_slots, slot_bucket_count, bucket);
                                    debug_assert!(_bucket_end - bucket_start <= u64::from(u32::MAX));
                                    entry.write_to(bucket_start, &mut entry_buf);
                                    slot_bufs[bucket].extend_from_slice(&entry_buf);
                                    slot_counts[bucket] += 1;
                                    if entry.lat != 0 || entry.lon != 0 {
                                        local_resolved += 1;
                                    }
                                    if slot_bufs[bucket].len() >= FLUSH_THRESHOLD {
                                        let t_lock = std::time::Instant::now();
                                        let mut w = slot_buckets.writers[bucket]
                                            .lock()
                                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                                        #[allow(clippy::cast_possible_truncation)]
                                        flush_lock_ref.fetch_add(
                                            t_lock.elapsed().as_millis() as u64, Relaxed,
                                        );
                                        let t_wr = std::time::Instant::now();
                                        let flush_bytes = slot_bufs[bucket].len() as u64;
                                        w.write_all(&slot_bufs[bucket])
                                            .map_err(|e| format!("write slot bucket: {e}"))?;
                                        drop(w);
                                        #[allow(clippy::cast_possible_truncation)]
                                        flush_write_ref.fetch_add(
                                            t_wr.elapsed().as_millis() as u64, Relaxed,
                                        );
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
                                record_ptr += 1;
                            }
                            #[allow(clippy::cast_possible_truncation)]
                            node_walk_ref.fetch_add(
                                t_walk.elapsed().as_millis() as u64, Relaxed,
                            );
                            #[allow(clippy::cast_possible_truncation)]
                            node_walk_ns_ref.fetch_add(
                                t_walk.elapsed().as_nanos() as u64, Relaxed,
                            );
                            bucket_blobs_read += 1;
                        }

                        // Records past the last blob's max_id are orphans
                        // (their global id has no matching node anywhere
                        // in the input - the canonical absent-node case).
                        let orphans = bkt.records.len().saturating_sub(record_ptr);
                        orphan_ref.fetch_add(orphans as u64, Relaxed);

                        node_blobs_ref.fetch_add(bucket_blobs_read, Relaxed);
                        node_straddler_ref.fetch_add(bucket_straddlers, Relaxed);
                        #[allow(clippy::cast_possible_truncation)]
                        resolve_ref.fetch_add(
                            t_resolve.elapsed().as_millis() as u64, Relaxed,
                        );
                        pread_calls_ref.fetch_add(bucket_blobs_read, Relaxed);

                        // Track max live buffer bytes for this worker.
                        {
                            let worker_bytes = loader.data_buf.capacity() as u64
                                + (loader.records.capacity() * std::mem::size_of::<IdRecord>()) as u64
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
                            flush_lock_ref.fetch_add(
                                t_lock.elapsed().as_millis() as u64, Relaxed,
                            );
                            let t_wr = std::time::Instant::now();
                            let flush_bytes = slot_bufs[sb].len() as u64;
                            w.write_all(&slot_bufs[sb])
                                .map_err(|e| format!("write slot bucket: {e}"))?;
                            drop(w);
                            #[allow(clippy::cast_possible_truncation)]
                            flush_write_ref.fetch_add(
                                t_wr.elapsed().as_millis() as u64, Relaxed,
                            );
                            slot_bytes_ref.fetch_add(flush_bytes, Relaxed);
                            flush_calls_ref.fetch_add(1, Relaxed);
                            slot_buckets.entry_counts[sb]
                                .fetch_add(slot_counts[sb], Relaxed);
                            slot_bufs[sb].clear();
                            slot_counts[sb] = 0;
                        }
                        nonempty_ref.fetch_add(nonempty_count, Relaxed);

                        // Recycle the records buffer for the next bucket.
                        loader.records = bkt.records;
                        loader.records.clear();

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
        crate::debug::emit_counter("s2_node_pread_ms", s2_node_pread_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_decompress_ms", s2_node_decompress_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_extract_ms", s2_node_extract_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_walk_ms", s2_node_walk_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_pread_ns", s2_node_pread_ns.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_decompress_ns", s2_node_decompress_ns.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_extract_ns", s2_node_extract_ns.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_walk_ns", s2_node_walk_ns.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_bytes_read", s2_node_bytes_read.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_blobs_read", s2_node_blobs_read.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_node_straddler_blobs", s2_node_straddler_blobs.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_resolve_ms", s2_resolve_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_bucket_load_ms", s2_bucket_load_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_bucket_loads", s2_bucket_loads.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_prepare_parse_ms", s2_prepare_parse_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_prepare_sort_ms", s2_prepare_sort_ms.load(std::sync::atomic::Ordering::Relaxed) as i64);
        crate::debug::emit_counter("s2_orphan_records", s2_orphan_records.load(std::sync::atomic::Ordering::Relaxed) as i64);
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
