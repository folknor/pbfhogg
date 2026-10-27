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

use super::super::Result;
use super::radix::advise_dontneed_file;
use super::{
    BucketLayout, CLOSURE_FLAG, ID_RECORD_SIZE, IdRecord, LOCAL_ID_MASK, NodeBlobInfo,
    RESOLVED_ENTRY_SIZE, ResolvedEntry, slot_bucket_bounds,
};
use crate::counters::record_max;
use crate::error::Error;
use crate::scan::node::NodeTuple;
use crate::scratch::RunScratch;
use crate::tunables::{
    ALTW_STAGE2_WORKERS_CAP, BUCKET_BUF_SIZE, DECODE_WORKERS_FALLBACK, DECODE_WORKERS_MIN,
    DECODE_WORKERS_RESERVED, NUM_BUCKETS, STAGE2_FLUSH_THRESHOLD,
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
        Self {
            data_buf: Vec::new(),
            records: Vec::new(),
        }
    }
}

crate::counters::counter_set! {
    /// Stage 2 counters, shared by every node-join worker. Bumped per id
    /// bucket, per node blob read, or per slot-buffer flush; never per
    /// record. The `_ms` timers keep their historical per-sample
    /// truncation; the `_ns` twins exist because that is lossy for
    /// sub-millisecond blob phases (see `crate::counters::Stopwatch`).
    struct Stage2Counters {
        node_pread_ms => "s2_node_pread_ms",
        node_decompress_ms => "s2_node_decompress_ms",
        node_extract_ms => "s2_node_extract_ms",
        node_walk_ms => "s2_node_walk_ms",
        node_pread_ns => "s2_node_pread_ns",
        node_decompress_ns => "s2_node_decompress_ns",
        node_extract_ns => "s2_node_extract_ns",
        node_walk_ns => "s2_node_walk_ns",
        node_bytes_read => "s2_node_bytes_read",
        node_blobs_read => "s2_node_blobs_read",
        /// Node blobs read whose id range crosses the bucket boundary
        /// (straddlers). Each straddler is re-decompressed by adjacent
        /// workers; high values mean wasted decode work. With id-bucketing,
        /// a node blob's id range can be wider than the bucket width, so
        /// straddler density is expected to be high for sparse ID ranges.
        /// Watch this metric when diagnosing repeated node decompression.
        node_straddler_blobs => "s2_node_straddler_blobs",
        resolve_ms => "s2_resolve_ms",
        bucket_load_ms => "s2_bucket_load_ms",
        bucket_loads => "s2_bucket_loads",
        prepare_parse_ms => "s2_prepare_parse_ms",
        prepare_sort_ms => "s2_prepare_sort_ms",
        orphan_records => "s2_orphan_records",
        slot_flush_lock_wait_ms => "s2_slot_flush_lock_wait_ms",
        /// Time workers spent blocked acquiring a shared slot-bucket writer's
        /// mutex, queued behind a peer flushing into the same bucket (the
        /// holder keeps it across its `write_all`, which can reach the
        /// file). The stage's one wait on another thread, as a `--stalls`
        /// counter: gated (an uncontended lock adds nothing) and in
        /// nanoseconds. `s2_slot_flush_lock_wait_ms` keeps timing every
        /// acquisition with per-sample millisecond truncation, so it reads
        /// zero for any wait under a millisecond.
        slot_flush_lock_wait_ns => "s2_slot_flush_lock_wait_ns",
        slot_flush_write_ms => "s2_slot_flush_write_ms",
        slot_bytes_written => "s2_slot_bytes_written",
        slot_flush_calls => "s2_slot_flush_calls",
        nonempty_slot_buckets_total => "s2_nonempty_slot_buckets_total",
        max_slot_buffer_bytes => "s2_max_slot_buffer_bytes",
        max_worker_buf_bytes => "s2_max_worker_buf_bytes",
        pread_calls => "s2_pread_calls",
        open_calls => "s2_open_calls",
        stat_calls => "s2_stat_calls",
        fadvise_calls => "s2_fadvise_calls",
        fadvise_bytes => "s2_fadvise_bytes",
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

#[hotpath::measure]
#[allow(clippy::cast_possible_truncation)]
fn prepare_bucket(
    bucket_idx: usize,
    scratch: &RunScratch,
    num_shard_workers: usize,
    loader: &mut LoaderScratch,
    invocation: &crate::host::Invocation,
) -> crate::Result<PreparedBucket> {
    loader.data_buf.clear();
    loader.records.clear();
    let mut open_calls: u64 = 0;
    let mut stat_calls: u64 = 0;
    // Zero off Linux, where there is no advice to give.
    let mut fadvise_calls: u64 = 0;
    let mut fadvise_bytes: u64 = 0;

    for worker_id in 0..num_shard_workers {
        let path = super::radix::id_shard_path(scratch, worker_id, bucket_idx)?;
        let named = |what: &str, e: std::io::Error| {
            Error::from(e).context(format!("failed to {what} id shard {}", path.display()))
        };
        let file = match crate::scratch::open(&path) {
            Ok(f) => {
                open_calls += 1;
                f
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(named("open", e)),
        };
        stat_calls += 1;
        let len = file.metadata().map_err(|e| named("stat", e))?.len() as usize;
        if len == 0 {
            continue;
        }
        let start = loader.data_buf.len();
        loader.data_buf.resize(start + len, 0);
        std::io::Read::read_exact(&mut &file, &mut loader.data_buf[start..])
            .map_err(|e| named("read", e))?;
        if advise_dontneed_file(&file, invocation) {
            fadvise_calls += 1;
            fadvise_bytes += len as u64;
        }
    }

    // Shard integrity: each id-shard must be a sequence of
    // ID_RECORD_SIZE-byte records. A non-multiple length means a
    // truncated write (kernel page cache loss, fs corruption, partial
    // worker write that didn't flush). chunks_exact would silently
    // drop the partial tail; error explicitly instead.
    if !loader.data_buf.len().is_multiple_of(ID_RECORD_SIZE) {
        return Err(Error::internal(format!(
            "stage2 bucket {bucket_idx}: id shard total length {} is not \
             a multiple of {ID_RECORD_SIZE}",
            loader.data_buf.len(),
        )));
    }

    // Parse 12-byte chunks into IdRecords.
    let t_parse = crate::counters::Stopwatch::start();
    let count = loader.data_buf.len() / ID_RECORD_SIZE;
    loader.records.reserve(count);
    let (chunks, _rem) = loader.data_buf.as_chunks::<ID_RECORD_SIZE>();
    for buf in chunks {
        loader.records.push(IdRecord::read_from(buf));
    }
    let parse_ms = t_parse.elapsed_ms();

    // Sort by local_node_id so the merge walk against ID-sorted node
    // tuples advances both pointers monotonically.
    let t_sort = crate::counters::Stopwatch::start();
    loader
        .records
        .sort_unstable_by_key(|r| r.local_node_id & LOCAL_ID_MASK);
    let sort_ms = t_sort.elapsed_ms();

    Ok(PreparedBucket {
        records: std::mem::take(&mut loader.records),
        open_calls,
        stat_calls,
        fadvise_calls,
        fadvise_bytes,
        parse_ms,
        sort_ms,
    })
}

/// Shared slot bucket writers protected by per-bucket mutexes.
/// `slot_bucket_count` files total regardless of worker count.
pub(super) struct SharedSlotBuckets {
    writers: Vec<std::sync::Mutex<std::io::BufWriter<std::fs::File>>>,
    entry_counts: Vec<std::sync::atomic::AtomicU64>,
    /// Each bucket's file, for naming it in a failed write.
    paths: Vec<crate::scratch::ScratchPath>,
}

impl SharedSlotBuckets {
    pub(super) fn create(scratch: &RunScratch, slot_bucket_count: usize) -> Result<Self> {
        let mut writers = Vec::with_capacity(slot_bucket_count);
        let mut entry_counts = Vec::with_capacity(slot_bucket_count);
        let mut paths = Vec::with_capacity(slot_bucket_count);

        for i in 0..slot_bucket_count {
            let path = super::radix::slot_bucket_path(scratch, i)?;
            let file = crate::scratch::create(&path).map_err(|e| {
                Error::from(e).context(format!("failed to create slot bucket {}", path.display()))
            })?;
            writers.push(std::sync::Mutex::new(std::io::BufWriter::with_capacity(
                BUCKET_BUF_SIZE,
                file,
            )));
            entry_counts.push(std::sync::atomic::AtomicU64::new(0));
            paths.push(path);
        }

        Ok(Self {
            writers,
            entry_counts,
            paths,
        })
    }

    /// A failed write to bucket `bucket`, naming its file.
    #[cold]
    fn write_error(&self, bucket: usize, err: std::io::Error) -> Error {
        let path = self
            .paths
            .get(bucket)
            .map_or_else(|| format!("{bucket}"), |p| p.display().to_string());
        Error::from(err).context(format!("failed to write slot bucket {path}"))
    }

    pub(super) fn finish(&self, invocation: &crate::host::Invocation) -> Result<()> {
        for (bucket, writer_mutex) in self.writers.iter().enumerate() {
            let mut w = writer_mutex
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            w.flush().map_err(|e| self.write_error(bucket, e))?;
            // Stage 3 reads each bucket back once, so its pages are dropped
            // now rather than left to crowd out the stage-3 working set.
            // DONTNEED leaves dirty pages alone, so the bucket is written back
            // first. Linux only, where the advice exists; in every Linux
            // build, since this is library scratch (ADR-0016), not the
            // `linux-direct-io` feature it used to be compiled under.
            #[cfg(target_os = "linux")]
            {
                // A failed writeback means the bucket on disk is not what was
                // written, and once its pages leave the cache (by the advice
                // below or by ordinary reclaim) stage 3 would read that back.
                // The run fails here instead. This covers the slot buckets
                // only; it is not a scratch-wide integrity check.
                #[expect(
                    clippy::disallowed_methods,
                    reason = "scratch pages are written back so the fadvise below can drop them; \
                              this is not an output sync"
                )]
                w.get_ref()
                    .sync_data()
                    .map_err(|e| self.sync_error(bucket, e))?;
                advise_dontneed_file(w.get_ref(), invocation);
            }
        }
        Ok(())
    }

    /// A failed writeback of bucket `bucket`, naming its file.
    #[cfg(target_os = "linux")]
    #[cold]
    fn sync_error(&self, bucket: usize, err: std::io::Error) -> Error {
        let path = self
            .paths
            .get(bucket)
            .map_or_else(|| format!("{bucket}"), |p| p.display().to_string());
        Error::from(err).context(format!("failed to sync slot bucket {path}"))
    }
}

/// The refusal for a node blob whose extracted tuples the fast check in
/// [`stage2_node_join`] did not find exact and ascending: the shared claim
/// walk over the same tuples, which names the false claim or the disorder
/// by the walk's precedence. Cold: only a lying blob reaches it.
#[cold]
fn stage2_claim_refusal(blob: &NodeBlobInfo, tuples: &[NodeTuple]) -> Error {
    let claim = blob.claim();
    let mut walk = crate::blob_meta::ClaimWalk::new(Some(&claim), super::blob_meta::EXTERNAL_ORDER);
    for tuple in tuples {
        if let Err(e) = walk.observe(tuple.id) {
            return e;
        }
    }
    match walk.finish() {
        Err(e) => e,
        Ok(_) => Error::internal(
            "altw external stage 2: a node blob failed the fast claim check but passed the walk",
        ),
    }
}

/// Parallel stage 2: N workers each claim id buckets via atomic
/// dispatch, load + sort the bucket's IdRecords, find the contiguous
/// run of node blobs whose `[min_id, max_id]` intersects the bucket's
/// id range, decode those blobs, walk records and node tuples in
/// lockstep, and emit `ResolvedEntry` to per-bucket slot writers
/// (256 files total, per-bucket mutex). Returns the count of records
/// that found their node in the id-sorted tuple stream - presence, not
/// non-zero-ness; see the counting site below.
#[hotpath::measure]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) fn stage2_node_join(
    scratch: &RunScratch,
    id_bucket_counts: &[u64],
    num_shard_workers: usize,
    bucket_layout: &BucketLayout,
    way_slot_starts: &[u64],
    slot_buckets: &SharedSlotBuckets,
    slot_bucket_count: usize,
    total_slots: u64,
    input_pbf: &crate::read::input::InputFile,
    node_blob_mapping: &[NodeBlobInfo],
    inject_prepass: bool,
    host: &crate::host::Host,
) -> Result<u64> {
    let num_workers = crate::host::sized_workers(
        &*host.resources,
        DECODE_WORKERS_RESERVED,
        DECODE_WORKERS_MIN,
        DECODE_WORKERS_FALLBACK,
    )
    .min(ALTW_STAGE2_WORKERS_CAP);

    use std::os::unix::fs::FileExt as _;

    // A hand-built claim-counter pool, deliberately not on
    // `crate::ordered_pool`: it has no consumer and no order to keep.
    // Workers claim id buckets with one `fetch_add`, each holding its own
    // bucket scratch and buffered slot writes, under the stage's worker cap.
    // The first recorded error (not the lowest bucket) stops the others at
    // their next claim, and work in flight finishes; cancellation is checked
    // per non-empty bucket and per node blob inside it. Its one wait on
    // another thread is a shared slot-bucket writer's mutex, recorded as
    // `s2_slot_flush_lock_wait_ns`; the claim is a `fetch_add` and the
    // first-error slot is locked once per bucket, held for a load.
    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let resolved_total = std::sync::atomic::AtomicU64::new(0);
    let counters = Stage2Counters::new();
    let s2_error: std::sync::Mutex<Option<Error>> = std::sync::Mutex::new(None);

    let next_ref = &next_idx;
    let resolved_ref = &resolved_total;
    let c = &counters;
    let mapping_ref = node_blob_mapping;
    let err_ref = &s2_error;
    let layout_ref = bucket_layout;
    let slot_starts_ref = way_slot_starts;

    std::thread::scope(|scope| {
        for _ in 0..num_workers {
            let pbf_file = Arc::clone(input_pbf.file());
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
                // exceeds STAGE2_FLUSH_THRESHOLD, and at the end of each id
                // bucket.
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

                    let result: crate::Result<()> = (|| {
                        // Cancellation is checked per bucket and per node
                        // blob inside it, and leaves through this worker's
                        // error slot like any bucket failure.
                        host.invocation.check()?;
                        // Load + sort id-bucket records.
                        let t_load = crate::counters::Stopwatch::start();
                        let bkt = prepare_bucket(
                            bucket_idx,
                            scratch,
                            num_shard_workers,
                            &mut loader,
                            &host.invocation,
                        )?;
                        t_load.record_ms(&c.bucket_load_ms);
                        c.bucket_loads.fetch_add(1, Relaxed);
                        c.prepare_parse_ms.fetch_add(bkt.parse_ms, Relaxed);
                        c.prepare_sort_ms.fetch_add(bkt.sort_ms, Relaxed);
                        c.open_calls.fetch_add(bkt.open_calls, Relaxed);
                        c.stat_calls.fetch_add(bkt.stat_calls, Relaxed);
                        c.fadvise_calls.fetch_add(bkt.fadvise_calls, Relaxed);
                        c.fadvise_bytes.fetch_add(bkt.fadvise_bytes, Relaxed);

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
                        // The metadata scan guarantees blob ranges in
                        // non-decreasing id order (equal boundaries
                        // allowed), so both predicates are monotone and
                        // intersecting blobs form a contiguous slice.
                        #[allow(clippy::cast_sign_loss)]
                        let lo_blob = mapping_ref.partition_point(|b| {
                            (b.max_id as u64) < bucket_lo
                        });
                        #[allow(clippy::cast_sign_loss)]
                        let hi_blob = mapping_ref.partition_point(|b| {
                            (b.min_id as u64) < bucket_hi
                        });

                        let t_resolve = crate::counters::Stopwatch::start();
                        let mut record_ptr = 0usize;
                        let resolved_before_bucket = local_resolved;
                        let mut bucket_blobs_read: u64 = 0;
                        let mut bucket_straddlers: u64 = 0;

                        for blob in &mapping_ref[lo_blob..hi_blob] {
                            host.invocation.check()?;
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
                            let t_pr = crate::counters::Stopwatch::start();
                            node_read_buf.resize(blob.data_size, 0);
                            // Input failures are located with the input and
                            // the blob's payload offset (cold).
                            let in_input =
                                |e: Error| input_pbf.locate_at(blob.data_offset, e);
                            pbf_file.read_exact_at(&mut node_read_buf, blob.data_offset)
                                .map_err(|e| in_input(Error::from(e).context("stage2 node pread")))?;
                            t_pr.record_ms(&c.node_pread_ms);
                            t_pr.record_ns(&c.node_pread_ns);
                            c.node_bytes_read.fetch_add(blob.data_size as u64, Relaxed);

                            let t_dc = crate::counters::Stopwatch::start();
                            crate::blob::decompress_blob_raw(
                                &node_read_buf, &mut node_decompress_buf,
                            ).map_err(|e| in_input(e.context("stage2 node decompress")))?;
                            t_dc.record_ms(&c.node_decompress_ms);
                            t_dc.record_ns(&c.node_decompress_ns);

                            let t_ex = crate::counters::Stopwatch::start();
                            node_tuples.clear();
                            // Every tuple goes through the blob's claim walk
                            // in the sink: the partition above placed the
                            // blob by its claimed range, and the merge walk
                            // below needs its nodes ascending, so a claim
                            // that is not exact, or a payload out of order,
                            // is refused before any record is resolved
                            // against it. A foreign kind is one this
                            // scanner never reads.
                            //
                            // One raw compare per tuple: the claimed range is
                            // non-negative (the metadata scan refused any
                            // other), so a tuple sequence that ascends as
                            // i64 and starts at the claimed minimum is in
                            // canonical order and holds no negative ID, and
                            // then its count, minimum and maximum are its
                            // length, first and last. Anything else takes the
                            // shared walk, cold, for the exact refusal.
                            let mut descended = false;
                            let mut previous = i64::MIN;
                            let kinds = crate::scan::node::extract_node_coords(
                                &node_decompress_buf,
                                &mut node_group_starts,
                                |id, lat, lon| {
                                    descended |= id < previous;
                                    previous = id;
                                    node_tuples.push(NodeTuple { id, lat, lon });
                                },
                            ).map_err(|e| in_input(e.context("stage2 node extract")))?;
                            kinds
                                .refuse_under(Some(crate::ElementKind::Node))
                                .map_err(in_input)?;
                            let exact = !descended
                                && node_tuples.len() as u64 == blob.count
                                && node_tuples.first().is_some_and(|t| t.id == blob.min_id)
                                && previous == blob.max_id;
                            if !exact {
                                return Err(in_input(stage2_claim_refusal(blob, &node_tuples)));
                            }
                            t_ex.record_ms(&c.node_extract_ms);
                            t_ex.record_ns(&c.node_extract_ns);

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
                            //
                            // Order inside the blob was checked on
                            // extraction above (a blob out of order would
                            // make this walk pass present nodes, which would
                            // go out as missing locations), so the
                            // debug_assert below only restates it.
                            let t_walk = crate::counters::Stopwatch::start();
                            let blob_actual_upper: Option<u64> = node_tuples
                                .last()
                                .and_then(|t| u64::try_from(t.id).ok());
                            let mut tuple_ptr = 0usize;
                            // Pin decision cached for the current run of equal
                            // masked ids. Records with the same id are
                            // contiguous after the stage-2 sort and a run never
                            // spans blobs, so the run is computed once at its
                            // start and every record in it inherits the result.
                            let mut run_pin_cache: Option<(u32, bool)> = None;
                            #[cfg(debug_assertions)]
                            let mut prev_tuple_id: Option<i64> = None;
                            while record_ptr < bkt.records.len() {
                                let record = &bkt.records[record_ptr];
                                let global_id = bucket_lo + u64::from(record.local_node_id & LOCAL_ID_MASK);
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
                                    // Same geographic-domain refusal as the
                                    // sparse backend's slot encoding
                                    // (`coord_pack::pack`): a referenced node
                                    // outside +/-90 / +/-180 degrees is not a
                                    // coordinate, and writing it through
                                    // would make `--index-type` change what
                                    // the command accepts. The backends must
                                    // agree on refusal, not just on counting.
                                    if !crate::coord_pack::in_domain(tuple.lat, tuple.lon) {
                                        return Err(super::super::node_coordinate_domain_error(
                                            tuple.id, tuple.lat, tuple.lon,
                                        ));
                                    }
                                    let blob_start = slot_starts_ref
                                        .get(record.blob_idx as usize)
                                        .copied()
                                        .ok_or_else(|| {
                                            Error::internal(format!(
                                                "stage2: record blob_idx {} out of range \
                                                 (way_slot_starts.len()={})",
                                                record.blob_idx,
                                                slot_starts_ref.len(),
                                            ))
                                        })?;
                                    let linear_slot_pos =
                                        blob_start + u64::from(record.blob_local_slot);
                                    // Pin = the node is shared: it occurs in at
                                    // least two non-closure ref positions. All
                                    // occurrences of an id form one run, so the
                                    // whole run is counted once (from its start)
                                    // and every record inherits the decision -
                                    // including the trailing closure ref, which
                                    // mirrors bit 0 by construction.
                                    let pin = if inject_prepass {
                                        let masked = record.local_node_id & LOCAL_ID_MASK;
                                        if run_pin_cache.map(|(m, _)| m) != Some(masked) {
                                            let run_len = bkt.records[record_ptr..]
                                                .iter()
                                                .take_while(|c| {
                                                    c.local_node_id & LOCAL_ID_MASK == masked
                                                })
                                                .count();
                                            let unflagged = bkt.records
                                                [record_ptr..record_ptr + run_len]
                                                .iter()
                                                .filter(|c| c.local_node_id & CLOSURE_FLAG == 0)
                                                .count();
                                            run_pin_cache = Some((masked, unflagged >= 2));
                                        }
                                        run_pin_cache.is_some_and(|(_, p)| p)
                                    } else {
                                        false
                                    };
                                    // Private scratch codec documented on ResolvedEntry:
                                    // domain validation above makes the shift lossless;
                                    // the run-wide flag also governs payload decoding.
                                    let entry = ResolvedEntry {
                                        slot_pos: linear_slot_pos,
                                        lat: if inject_prepass {
                                            (tuple.lat << 1) | i32::from(pin)
                                        } else {
                                            tuple.lat
                                        },
                                        lon: tuple.lon,
                                    };
                                    let bucket = entry.slot_bucket(total_slots, slot_bucket_count);
                                    let (bucket_start, _bucket_end) =
                                        slot_bucket_bounds(total_slots, slot_bucket_count, bucket);
                                    debug_assert!(_bucket_end - bucket_start <= u64::from(u32::MAX));
                                    entry.write_to(bucket_start, &mut entry_buf);
                                    slot_bufs[bucket].extend_from_slice(&entry_buf);
                                    slot_counts[bucket] += 1;
                                    // Count on presence, not on truthiness.
                                    // `resolved` already means the id was
                                    // found in the node stream; gating the
                                    // increment on a non-zero coordinate
                                    // additionally reported every genuine
                                    // node at Null Island as missing, so the
                                    // external backend disagreed with the
                                    // sparse one about the same input. Absent
                                    // is not zero.
                                    local_resolved += 1;
                                    if slot_bufs[bucket].len() >= STAGE2_FLUSH_THRESHOLD {
                                        let t_lock = crate::counters::Stopwatch::start();
                                        let mut w = crate::counters::lock_recording_wait(
                                            &slot_buckets.writers[bucket],
                                            &c.slot_flush_lock_wait_ns,
                                        );
                                        t_lock.record_ms(&c.slot_flush_lock_wait_ms);
                                        let t_wr = crate::counters::Stopwatch::start();
                                        let flush_bytes = slot_bufs[bucket].len() as u64;
                                        w.write_all(&slot_bufs[bucket])
                                            .map_err(|e| slot_buckets.write_error(bucket, e))?;
                                        drop(w);
                                        t_wr.record_ms(&c.slot_flush_write_ms);
                                        c.slot_bytes_written.fetch_add(flush_bytes, Relaxed);
                                        c.slot_flush_calls.fetch_add(1, Relaxed);
                                        // Track max buffer size before flush.
                                        record_max(&c.max_slot_buffer_bytes, flush_bytes);
                                        slot_buckets.entry_counts[bucket]
                                            .fetch_add(slot_counts[bucket], Relaxed);
                                        slot_bufs[bucket].clear();
                                        slot_counts[bucket] = 0;
                                    }
                                }
                                record_ptr += 1;
                            }
                            t_walk.record_ms(&c.node_walk_ms);
                            t_walk.record_ns(&c.node_walk_ns);
                            bucket_blobs_read += 1;
                        }

                        // Count every absent record, including IDs in gaps
                        // within a blob range and past the last blob.
                        let bucket_resolved = local_resolved - resolved_before_bucket;
                        let orphans = (bkt.records.len() as u64).saturating_sub(bucket_resolved);
                        c.orphan_records.fetch_add(orphans, Relaxed);

                        c.node_blobs_read.fetch_add(bucket_blobs_read, Relaxed);
                        c.node_straddler_blobs.fetch_add(bucket_straddlers, Relaxed);
                        t_resolve.record_ms(&c.resolve_ms);
                        c.pread_calls.fetch_add(bucket_blobs_read, Relaxed);

                        // Track max live buffer bytes for this worker.
                        {
                            let worker_bytes = loader.data_buf.capacity() as u64
                                + (loader.records.capacity() * std::mem::size_of::<IdRecord>()) as u64
                                + node_read_buf.capacity() as u64
                                + node_decompress_buf.capacity() as u64
                                + (node_tuples.capacity() * std::mem::size_of::<NodeTuple>()) as u64
                                + (node_group_starts.capacity() * std::mem::size_of::<(usize, usize)>()) as u64
                                + slot_bufs.iter().map(|b| b.capacity() as u64).sum::<u64>();
                            record_max(&c.max_worker_buf_bytes, worker_bytes);
                        }

                        // Flush non-empty local buffers to shared writers.
                        let mut nonempty_count: u64 = 0;
                        for sb in 0..slot_bucket_count {
                            if slot_bufs[sb].is_empty() { continue; }
                            nonempty_count += 1;
                            let t_lock = crate::counters::Stopwatch::start();
                            let mut w = crate::counters::lock_recording_wait(
                                &slot_buckets.writers[sb],
                                &c.slot_flush_lock_wait_ns,
                            );
                            t_lock.record_ms(&c.slot_flush_lock_wait_ms);
                            let t_wr = crate::counters::Stopwatch::start();
                            let flush_bytes = slot_bufs[sb].len() as u64;
                            w.write_all(&slot_bufs[sb])
                                .map_err(|e| slot_buckets.write_error(sb, e))?;
                            drop(w);
                            t_wr.record_ms(&c.slot_flush_write_ms);
                            c.slot_bytes_written.fetch_add(flush_bytes, Relaxed);
                            c.slot_flush_calls.fetch_add(1, Relaxed);
                            slot_buckets.entry_counts[sb]
                                .fetch_add(slot_counts[sb], Relaxed);
                            slot_bufs[sb].clear();
                            slot_counts[sb] = 0;
                        }
                        c.nonempty_slot_buckets_total.fetch_add(nonempty_count, Relaxed);

                        // Recycle the records buffer for the next bucket.
                        loader.records = bkt.records;
                        loader.records.clear();

                        Ok(())
                    })();

                    if let Err(e) = result {
                        // The first error wins: a later one (another worker
                        // stopping on the same cancellation, say) must not
                        // replace the cause.
                        err_ref
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .get_or_insert(e);
                        break;
                    }
                }

                resolved_ref.fetch_add(local_resolved, Relaxed);
            });
        }
    });

    if let Some(e) = s2_error.into_inner().unwrap_or(None) {
        return Err(e);
    }

    counters.emit_all(&host.invocation);
    host.invocation.counter("s2_num_workers", num_workers);

    let resolved_count = resolved_total.load(std::sync::atomic::Ordering::Relaxed);
    Ok(resolved_count)
}

pub(super) type SlotBuckets = SharedSlotBuckets;
