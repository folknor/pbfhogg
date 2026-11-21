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
use super::keys::{Family, NodeKeyLayout, NodeSpans};
use super::{
    NodeBlobInfo, RESOLVED_ENTRY_SIZE, ResolvedEntry, ShardCodec, ShardFormat, ShardRecord,
    slot_bucket_bounds,
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

struct LoaderScratch<R> {
    data_buf: Vec<u8>,
    records: Vec<R>,
}

impl<R> LoaderScratch<R> {
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
        /// Id shards advised out of the page cache once read back; zero off
        /// Linux.
        fadvise_calls => "s2_fadvise_calls",
        /// Bytes of those shards; zero off Linux.
        fadvise_bytes => "s2_fadvise_bytes",
        /// Slot buckets advised out of the page cache once written back
        /// ([`SharedSlotBuckets::finish`]), empty ones included; zero off
        /// Linux.
        slot_fadvise_calls => "s2_slot_fadvise_calls",
        /// Bytes written to those buckets; zero off Linux.
        slot_fadvise_bytes => "s2_slot_fadvise_bytes",
    }
}

struct PreparedBucket<R> {
    /// Records for this bucket, sorted by local key.
    records: Vec<R>,
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
fn prepare_bucket<C: ShardCodec>(
    bucket_idx: usize,
    scratch: &RunScratch,
    num_shard_workers: usize,
    codec: C,
    expected_records: u64,
    loader: &mut LoaderScratch<C::Record>,
    invocation: &crate::host::Recorder,
) -> crate::Result<PreparedBucket<C::Record>> {
    loader.data_buf.clear();
    loader.records.clear();
    // The bucket's exact logical record count, from pass A: wire bytes are
    // not a record count once escaped records take two words.
    let expected = usize::try_from(expected_records).map_err(|_| {
        Error::internal(format!(
            "stage2 bucket {bucket_idx}: {expected_records} records do not fit in memory"
        ))
    })?;
    // `s2_prepare_parse_ms` has always covered the reserve as well as the
    // decode, so the reserve is timed into it.
    let t_reserve = crate::counters::Stopwatch::start();
    loader.records.reserve(expected);
    let mut parse_ns: u64 = t_reserve.elapsed_ns();
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
        // One shard at a time: each file is decoded and checked on its own
        // (whole words, no dangling escape), so a truncated tail in one
        // shard cannot borrow the next shard's first word.
        loader.data_buf.clear();
        loader.data_buf.resize(len, 0);
        std::io::Read::read_exact(&mut &file, &mut loader.data_buf)
            .map_err(|e| named("read", e))?;
        if crate::fadvise::evict_scratch(&file, invocation) {
            fadvise_calls += 1;
            fadvise_bytes += len as u64;
        }
        let t_parse = crate::counters::Stopwatch::start();
        codec
            .decode_into(&loader.data_buf, &mut loader.records)
            .map_err(|e| e.context(format!("id shard {}", path.display())))?;
        parse_ns += t_parse.elapsed_ns();
    }
    let parse_ms = parse_ns / 1_000_000;

    // A partial worker write that did not flush, or a lost file, leaves
    // whole words; only the count pass A recorded catches it.
    if loader.records.len() != expected {
        return Err(Error::internal(format!(
            "stage2 bucket {bucket_idx}: id shards hold {} records, pass A wrote {expected}",
            loader.records.len(),
        )));
    }

    // Sort by local key so the merge walk against key-sorted node
    // tuples advances both pointers monotonically.
    let t_sort = crate::counters::Stopwatch::start();
    loader.records.sort_unstable_by_key(ShardRecord::local);
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
    writers: Vec<std::sync::Mutex<std::io::BufWriter<crate::scratch::ScratchFile>>>,
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

    /// Close the writers and hand stage 3 the manifest. Only after
    /// [`Self::finish`] succeeded, so every count is on disk.
    ///
    /// The manifest's counts are the ones [`flush_slot_buffer`] advanced
    /// after each successful `write_all`, so they are what stage 2 committed
    /// to each file. Stage 3 trusts them, never a filesystem observation: a
    /// bucket's file length used to be read back by `stat`, and a failed
    /// `stat` became "empty", which published every vertex of that bucket at
    /// (0, 0) with no missing location reported.
    fn into_manifest(self) -> SlotBucketManifest {
        let Self {
            writers,
            entry_counts,
            paths,
        } = self;
        drop(writers);
        SlotBucketManifest {
            buckets: paths
                .into_iter()
                .zip(
                    entry_counts
                        .into_iter()
                        .map(std::sync::atomic::AtomicU64::into_inner),
                )
                .collect(),
        }
    }

    /// Flush every bucket and, on Linux, write it back and advise it out of
    /// the page cache, counting each advice into `counters`. Runs once, at
    /// the end of [`stage2_node_join`], after its workers have joined.
    fn finish(&self, counters: &Stage2Counters, invocation: &crate::host::Recorder) -> Result<()> {
        #[cfg(not(target_os = "linux"))]
        let _ = (counters, invocation);
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
                if crate::fadvise::evict_scratch(w.get_ref(), invocation) {
                    use std::sync::atomic::Ordering::Relaxed;
                    counters.slot_fadvise_calls.fetch_add(1, Relaxed);
                    let entries = self.entry_counts.get(bucket).map_or(0, |n| n.load(Relaxed));
                    counters
                        .slot_fadvise_bytes
                        .fetch_add(entries * RESOLVED_ENTRY_SIZE as u64, Relaxed);
                }
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

/// Stage 2's slot bucket files as committed: each bucket's path and the
/// number of `ResolvedEntry` records stage 2 wrote to it. The only source
/// of stage 3's per-bucket counts.
pub(super) struct SlotBucketManifest {
    pub(super) buckets: Vec<(crate::scratch::ScratchPath, u64)>,
}

/// Parallel stage 2: N workers each claim id buckets via atomic
/// dispatch, load + sort the bucket's records, find the contiguous run
/// of node blobs whose key span in the bucket's family intersects the
/// bucket's key range, decode those blobs, walk records and the family's
/// node tuples in lockstep, and emit `ResolvedEntry` to per-bucket slot
/// writers (256 files total, per-bucket mutex). Returns the count of
/// records that found their node in the key-sorted tuple stream -
/// presence, not non-zero-ness; see the counting site in [`walk_blob`] -
/// and the manifest of what was written to each slot bucket, whose counts
/// sum to it.
#[allow(clippy::too_many_arguments)]
pub(super) fn stage2_node_join(
    scratch: &RunScratch,
    id_bucket_counts: &[u64],
    num_shard_workers: usize,
    format: ShardFormat,
    layout: &NodeKeyLayout,
    way_slot_starts: &[u64],
    slot_buckets: SharedSlotBuckets,
    slot_bucket_count: usize,
    total_slots: u64,
    input_pbf: &crate::read::input::InputFile,
    node_blob_mapping: &[NodeBlobInfo],
    node_spans: &NodeSpans,
    inject_prepass: bool,
    host: &crate::host::Entered<'_>,
) -> Result<(u64, SlotBucketManifest)> {
    let job = Stage2Job {
        scratch,
        id_bucket_counts,
        num_shard_workers,
        layout,
        way_slot_starts,
        slot_buckets: &slot_buckets,
        slot_bucket_count,
        total_slots,
        input_pbf,
        node_blob_mapping,
        node_spans,
        inject_prepass,
        host,
    };
    // One dispatch per stage: each record form gets its own join.
    let resolved_count = match format {
        ShardFormat::Packed(codec) => node_join(codec, &job),
        ShardFormat::Wide(codec) => node_join(codec, &job),
    }?;
    let manifest = slot_buckets.into_manifest();
    // Every resolved record appends exactly one entry, and a successful join
    // drained every worker buffer, so the two tallies agree. A disagreement
    // is a lost or doubled flush, and stage 3 would build on it.
    let written = manifest
        .buckets
        .iter()
        .try_fold(0u64, |acc, (_, n)| acc.checked_add(*n));
    if written != Some(resolved_count) || resolved_count > total_slots {
        return Err(Error::internal(format!(
            "stage 2 wrote {written:?} slot bucket entries for {resolved_count} resolved \
             references of {total_slots} slots"
        )));
    }
    Ok((resolved_count, manifest))
}

/// Everything stage 2 reads, shared by every worker.
struct Stage2Job<'j, 'h> {
    scratch: &'j RunScratch,
    id_bucket_counts: &'j [u64],
    num_shard_workers: usize,
    layout: &'j NodeKeyLayout,
    way_slot_starts: &'j [u64],
    slot_buckets: &'j SharedSlotBuckets,
    slot_bucket_count: usize,
    total_slots: u64,
    input_pbf: &'j crate::read::input::InputFile,
    node_blob_mapping: &'j [NodeBlobInfo],
    node_spans: &'j NodeSpans,
    inject_prepass: bool,
    host: &'j crate::host::Entered<'h>,
}

/// One worker's buffers, kept across its buckets.
struct WorkerBufs<R> {
    loader: LoaderScratch<R>,
    node_read_buf: crate::read::read_buf::ReadBuf,
    node_decompress_buf: Vec<u8>,
    node_tuples: Vec<NodeTuple>,
    node_group_starts: Vec<(usize, usize)>,
    entry_buf: [u8; RESOLVED_ENTRY_SIZE],
    local_resolved: u64,
    /// Per-slot-bucket local buffers. Flushed when any buffer exceeds
    /// STAGE2_FLUSH_THRESHOLD, and at the end of each id bucket.
    slot_bufs: Vec<Vec<u8>>,
    slot_counts: Vec<u64>,
}

#[hotpath::measure]
fn node_join<C: ShardCodec>(codec: C, job: &Stage2Job<'_, '_>) -> Result<u64> {
    let host = job.host;
    let num_workers = crate::host::sized_workers(
        &*host.resources,
        DECODE_WORKERS_RESERVED,
        DECODE_WORKERS_MIN,
        DECODE_WORKERS_FALLBACK,
    )
    .min(ALTW_STAGE2_WORKERS_CAP);

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
    let err_ref = &s2_error;

    crate::threads::scope(&host.invocation, |scope| {
        for worker in 0..num_workers {
            let pbf_file = Arc::clone(job.input_pbf.file());
            let spawned = scope.spawn(move || {
                use std::sync::atomic::Ordering::Relaxed;

                let mut w = WorkerBufs {
                    loader: LoaderScratch::new(),
                    node_read_buf: crate::read::read_buf::ReadBuf::default(),
                    node_decompress_buf: Vec::new(),
                    node_tuples: Vec::new(),
                    node_group_starts: Vec::new(),
                    entry_buf: [0u8; RESOLVED_ENTRY_SIZE],
                    local_resolved: 0,
                    slot_bufs: (0..job.slot_bucket_count).map(|_| Vec::new()).collect(),
                    slot_counts: vec![0; job.slot_bucket_count],
                };

                loop {
                    // The first-error mutex is locked once per bucket claim.
                    if err_ref
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_some()
                    {
                        break;
                    }
                    let bucket_idx = next_ref.fetch_add(1, Relaxed);
                    if bucket_idx >= NUM_BUCKETS {
                        break;
                    }
                    if job.id_bucket_counts[bucket_idx] == 0 {
                        continue;
                    }
                    if let Err(e) = join_bucket(codec, job, c, &pbf_file, bucket_idx, &mut w) {
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

                resolved_ref.fetch_add(w.local_resolved, Relaxed);
            });
            // A refusal is recorded in the first-error slot (an earlier
            // failure keeps its place), so the workers already born stop at
            // their next claim instead of joining the rest of the buckets.
            if let Err(e) = spawned {
                err_ref
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get_or_insert(
                        Error::from(e).context(format!("spawn stage 2 worker {worker}")),
                    );
                break;
            }
        }
    });

    if let Some(e) = s2_error.into_inner().unwrap_or(None) {
        return Err(e);
    }

    // Stage 2's tail: every worker has joined, so the slot buckets are
    // complete and are written back and evicted here, counted with the rest
    // of the stage.
    job.slot_buckets.finish(&counters, &host.invocation)?;

    counters.emit_all(&host.invocation);
    host.invocation.counter("s2_num_workers", num_workers);

    let resolved_count = resolved_total.load(std::sync::atomic::Ordering::Relaxed);
    Ok(resolved_count)
}

/// Join one non-empty id bucket.
#[allow(clippy::too_many_lines)]
fn join_bucket<C: ShardCodec>(
    codec: C,
    job: &Stage2Job<'_, '_>,
    c: &Stage2Counters,
    pbf_file: &std::fs::File,
    bucket_idx: usize,
    w: &mut WorkerBufs<C::Record>,
) -> crate::Result<()> {
    use std::sync::atomic::Ordering::Relaxed;
    let host = job.host;
    // Cancellation is checked per bucket and per node blob inside it, and
    // leaves through this worker's error slot like any bucket failure.
    host.invocation.check()?;
    // Load + sort id-bucket records.
    let t_load = crate::counters::Stopwatch::start();
    let bkt = prepare_bucket(
        bucket_idx,
        job.scratch,
        job.num_shard_workers,
        codec,
        job.id_bucket_counts[bucket_idx],
        &mut w.loader,
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
        w.loader.records = bkt.records;
        return Ok(());
    }

    // The bucket's family and key range [bucket_lo, bucket_hi), the same
    // bounds pass A routed by.
    let Some((family, bucket_lo, bucket_hi)) = job.layout.bucket(bucket_idx) else {
        return Err(Error::internal(format!(
            "stage2: id bucket {bucket_idx} holds records but no key routes to it"
        )));
    };

    // Find the contiguous run of the family's node-blob spans that
    // intersect [bucket_lo, bucket_hi). The metadata scan guarantees the
    // blobs in canonical order (equal boundaries allowed), so each family's
    // spans ascend, both predicates are monotone and intersecting spans form
    // a contiguous slice.
    let spans = job.node_spans.of(family);
    let lo_span = spans.partition_point(|s| s.hi < bucket_lo);
    let hi_span = spans.partition_point(|s| s.lo < bucket_hi);

    let t_resolve = crate::counters::Stopwatch::start();
    let mut record_ptr = 0usize;
    // The last resolved node, while it may still continue into the next
    // blob of the span list; see `walk_blob`.
    let mut carry: Option<Resolved> = None;
    let resolved_before_bucket = w.local_resolved;
    let mut bucket_blobs_read: u64 = 0;
    let mut bucket_straddlers: u64 = 0;
    let mut node_decompress_peak = w.node_decompress_buf.capacity();
    let mut node_read_peak = w.node_read_buf.capacity();
    let threshold = job.layout.threshold();

    for span in &spans[lo_span..hi_span] {
        host.invocation.check()?;
        let blob = job
            .node_blob_mapping
            .get(span.blob as usize)
            .ok_or_else(|| Error::internal("stage2: a key span names no node blob"))?;
        // Straddler: blob extends past either bucket boundary, so it's also
        // touched by an adjacent worker.
        if span.lo < bucket_lo || span.hi >= bucket_hi {
            bucket_straddlers += 1;
        }

        // pread + decompress + extract.
        let t_pr = crate::counters::Stopwatch::start();
        // Input failures are located with the input and the blob's payload
        // offset (cold).
        let in_input = |e: Error| job.input_pbf.locate_at(blob.data_offset, e);
        let raw = w
            .node_read_buf
            .read_exact_at(pbf_file, blob.data_offset, blob.data_size)
            .map_err(|e| in_input(Error::from(e).context("stage2 node pread")))?;
        t_pr.record_ms(&c.node_pread_ms);
        t_pr.record_ns(&c.node_pread_ns);
        c.node_bytes_read.fetch_add(blob.data_size as u64, Relaxed);

        let t_dc = crate::counters::Stopwatch::start();
        crate::blob::decompress_blob_raw(raw, &mut w.node_decompress_buf)
            .map_err(|e| in_input(e.context("stage2 node decompress")))?;
        t_dc.record_ms(&c.node_decompress_ms);
        t_dc.record_ns(&c.node_decompress_ns);

        let t_ex = crate::counters::Stopwatch::start();
        w.node_tuples.clear();
        // Every tuple goes through the blob's claim check: the partition
        // above placed the blob by its claimed range, and the merge walk
        // below needs its nodes in canonical order, so a claim that is not
        // exact, or a payload out of order, is refused before any record is
        // resolved against it, and before the tuples are split by family.
        // A foreign kind is one this scanner never reads.
        //
        // On a non-negative claim, one raw compare per tuple decides it
        // (`AscendingTally`); a claim reaching below zero is walked in
        // canonical order. A refusal replays the tuples, cold.
        let mut tally = crate::blob_meta::AscendingTally::default();
        let node_tuples = &mut w.node_tuples;
        let kinds = crate::scan::node::extract_node_coords(
            &w.node_decompress_buf,
            &mut w.node_group_starts,
            |id, lat, lon| {
                tally.add(id);
                node_tuples.push(NodeTuple { id, lat, lon });
            },
        )
        .map_err(|e| in_input(e.context("stage2 node extract")))?;
        kinds
            .refuse_under(Some(crate::ElementKind::Node))
            .map_err(in_input)?;
        let claim = blob.claim();
        crate::blob_meta::hold_claim(&tally, &claim, super::blob_meta::EXTERNAL_ORDER, || {
            w.node_tuples.iter().map(|t| t.id)
        })
        .map_err(in_input)?;
        t_ex.record_ms(&c.node_extract_ms);
        t_ex.record_ns(&c.node_extract_ns);

        // The family's tuples: in a canonically ordered blob (checked just
        // above) the IDs below the threshold come first, so each family is
        // one contiguous slice, ascending in its key.
        let split = if blob.min_id >= threshold {
            0
        } else if blob.max_id < threshold {
            w.node_tuples.len()
        } else {
            w.node_tuples.partition_point(|t| t.id < threshold)
        };
        let t_walk = crate::counters::Stopwatch::start();
        let mut out = WalkOut {
            entry_buf: &mut w.entry_buf,
            slot_bufs: &mut w.slot_bufs,
            slot_counts: &mut w.slot_counts,
            local_resolved: &mut w.local_resolved,
        };
        match family {
            Family::Pos => walk_blob(
                job,
                c,
                &bkt.records,
                &mut record_ptr,
                &mut carry,
                &w.node_tuples[split..],
                bucket_lo,
                family,
                |id| Family::Pos.key(id),
                &mut out,
            )?,
            Family::Neg => walk_blob(
                job,
                c,
                &bkt.records,
                &mut record_ptr,
                &mut carry,
                &w.node_tuples[..split],
                bucket_lo,
                family,
                |id| Family::Neg.key(id),
                &mut out,
            )?,
        }
        t_walk.record_ms(&c.node_walk_ms);
        t_walk.record_ns(&c.node_walk_ns);
        bucket_blobs_read += 1;
        // The node read and decode buffers follow their retention limits
        // after each blob; the bucket's peak sample below reads what each
        // held at most.
        node_read_peak = node_read_peak.max(w.node_read_buf.capacity());
        w.node_read_buf.release_outlier(&host.invocation);
        node_decompress_peak = node_decompress_peak.max(w.node_decompress_buf.capacity());
        crate::blob::release_decode_outlier(&mut w.node_decompress_buf, &host.invocation);
    }

    // Count every absent record, including IDs in gaps within a blob range
    // and past the last blob.
    let bucket_resolved = w.local_resolved - resolved_before_bucket;
    let orphans = (bkt.records.len() as u64).saturating_sub(bucket_resolved);
    c.orphan_records.fetch_add(orphans, Relaxed);

    c.node_blobs_read.fetch_add(bucket_blobs_read, Relaxed);
    c.node_straddler_blobs.fetch_add(bucket_straddlers, Relaxed);
    t_resolve.record_ms(&c.resolve_ms);
    c.pread_calls.fetch_add(bucket_blobs_read, Relaxed);

    // Track max live buffer bytes for this worker.
    {
        let worker_bytes = w.loader.data_buf.capacity() as u64
            // The bucket's records live in `bkt` until they are recycled
            // below; the loader's vector is empty here.
            + (bkt.records.capacity() * std::mem::size_of::<C::Record>()) as u64
            + node_read_peak as u64
            + node_decompress_peak as u64
            + (w.node_tuples.capacity() * std::mem::size_of::<NodeTuple>()) as u64
            + (w.node_group_starts.capacity() * std::mem::size_of::<(usize, usize)>()) as u64
            + w.slot_bufs.iter().map(|b| b.capacity() as u64).sum::<u64>();
        record_max(&c.max_worker_buf_bytes, worker_bytes);
    }

    // Flush non-empty local buffers to shared writers.
    let mut nonempty_count: u64 = 0;
    for sb in 0..job.slot_bucket_count {
        if w.slot_bufs[sb].is_empty() {
            continue;
        }
        nonempty_count += 1;
        flush_slot_buffer(job, c, sb, &mut w.slot_bufs[sb], &mut w.slot_counts[sb])?;
    }
    c.nonempty_slot_buckets_total
        .fetch_add(nonempty_count, Relaxed);

    // Recycle the records buffer for the next bucket.
    w.loader.records = bkt.records;
    w.loader.records.clear();
    Ok(())
}

/// Write one local slot buffer to its shared slot bucket.
fn flush_slot_buffer(
    job: &Stage2Job<'_, '_>,
    c: &Stage2Counters,
    bucket: usize,
    buf: &mut Vec<u8>,
    count: &mut u64,
) -> crate::Result<()> {
    use std::sync::atomic::Ordering::Relaxed;
    let t_lock = crate::counters::Stopwatch::start();
    let mut writer = crate::counters::lock_recording_wait(
        &job.slot_buckets.writers[bucket],
        &c.slot_flush_lock_wait_ns,
    );
    t_lock.record_ms(&c.slot_flush_lock_wait_ms);
    let t_wr = crate::counters::Stopwatch::start();
    let flush_bytes = buf.len() as u64;
    writer
        .write_all(buf)
        .map_err(|e| job.slot_buckets.write_error(bucket, e))?;
    drop(writer);
    t_wr.record_ms(&c.slot_flush_write_ms);
    c.slot_bytes_written.fetch_add(flush_bytes, Relaxed);
    c.slot_flush_calls.fetch_add(1, Relaxed);
    job.slot_buckets.entry_counts[bucket].fetch_add(*count, Relaxed);
    buf.clear();
    *count = 0;
    Ok(())
}

/// A worker's outputs a blob walk writes into.
struct WalkOut<'o> {
    entry_buf: &'o mut [u8; RESOLVED_ENTRY_SIZE],
    slot_bufs: &'o mut [Vec<u8>],
    slot_counts: &'o mut [u64],
    local_resolved: &'o mut u64,
}

/// A resolved node: its family key and the coordinate every reference to it
/// was given.
type Resolved = (u64, i32, i32);

/// Hold every further record of a resolved node to its first one: in the
/// geographic domain, and at the same coordinate. A conflicting duplicate is
/// refused rather than resolved to whichever record the walk met first, so
/// this backend refuses exactly the inputs the sparse store refuses.
#[inline]
fn check_duplicate(family: Family, resolved: Resolved, tuple: &NodeTuple) -> crate::Result<()> {
    let (key, lat, lon) = resolved;
    if !crate::coord_pack::in_domain(tuple.lat, tuple.lon) {
        return Err(super::super::node_coordinate_domain_error(
            family.id_of(key),
            tuple.lat,
            tuple.lon,
        ));
    }
    if (tuple.lat, tuple.lon) != (lat, lon) {
        return Err(super::super::conflicting_node_coordinates_error(
            family.id_of(key),
            (lat, lon),
            (tuple.lat, tuple.lon),
        ));
    }
    Ok(())
}

/// Merge-walk one blob's family tuples against the bucket's records from
/// `record_ptr` on.
///
/// Records are sorted by local key; `tuples` ascend in `key` (the blob's
/// order was checked before the split). Advance both pointers
/// monotonically. The walk's upper bound is the last key this blob's slice
/// actually contains, NOT the claimed range: indexdata is a producer claim
/// and can overstate the real range; using the actual last key avoids
/// consuming a record as "orphan in this blob" when it should be resolved
/// by a later blob whose claimed span overlaps. The old rank path's
/// `next_rank == ref_rank_end` canary failed loud on this class of bug; the
/// walk uses the decoded stream as the source of truth instead.
///
/// Records that find no matching tuple (true orphans - id absent from PBF)
/// emit nothing; stage 4 fills zero coords for any slot_pos not covered by a
/// ResolvedEntry, matching the existing missing-ref behaviour.
///
/// Duplicate node IDs: every record of a resolved node is compared with the
/// one that resolved it (`check_duplicate`). In canonical order a node's
/// records are contiguous, within the blob or across consecutive blobs of
/// the bucket's span list (every blob holding the key has a span containing
/// it, so all of them are in this bucket's list, in order). `carry` holds the
/// last resolved node while it ends the blob's slice, and the next slice's
/// equal-key prefix is checked against it before the record loop: the
/// records for that key were consumed by the earlier blob, so the loop
/// itself would never look. Duplicates of unreferenced nodes are not
/// examined, as in the sparse store.
#[allow(clippy::too_many_arguments)]
#[inline]
fn walk_blob<R: ShardRecord>(
    job: &Stage2Job<'_, '_>,
    c: &Stage2Counters,
    records: &[R],
    record_ptr: &mut usize,
    carry: &mut Option<Resolved>,
    tuples: &[NodeTuple],
    bucket_lo: u64,
    family: Family,
    key: impl Fn(i64) -> u64,
    out: &mut WalkOut<'_>,
) -> crate::Result<()> {
    let Some(upper) = tuples.last().map(|t| key(t.id)) else {
        return Ok(());
    };
    let mut tuple_ptr = 0usize;
    let mut last: Option<Resolved> = None;
    if let Some(prev) = carry.take() {
        while tuple_ptr < tuples.len() && key(tuples[tuple_ptr].id) == prev.0 {
            check_duplicate(family, prev, &tuples[tuple_ptr])?;
            tuple_ptr += 1;
        }
        last = Some(prev);
    }
    // Pin decision cached for the current run of equal local keys. Records
    // with the same key are contiguous after the stage-2 sort and a run
    // never spans blobs, so the run is computed once at its start and every
    // record in it inherits the result.
    let mut run_pin_cache: Option<(u64, bool)> = None;
    while *record_ptr < records.len() {
        let record = &records[*record_ptr];
        let global_key = bucket_lo + record.local();
        if global_key > upper {
            break;
        }
        while tuple_ptr < tuples.len() && key(tuples[tuple_ptr].id) < global_key {
            tuple_ptr += 1;
        }
        let resolved = tuple_ptr < tuples.len() && key(tuples[tuple_ptr].id) == global_key;
        if resolved {
            let tuple = &tuples[tuple_ptr];
            // Same geographic-domain refusal as the sparse backend's slot
            // encoding (`coord_pack::pack`): a referenced node outside
            // +/-90 / +/-180 degrees is not a coordinate, and writing it
            // through would make `--index-type` change what the command
            // accepts. The backends must agree on refusal, not just on
            // counting. Judged before the duplicate scan compares against it.
            if !crate::coord_pack::in_domain(tuple.lat, tuple.lon) {
                return Err(super::super::node_coordinate_domain_error(
                    family.id_of(global_key),
                    tuple.lat,
                    tuple.lon,
                ));
            }
            if last.map(|r| r.0) != Some(global_key) {
                // First record of this node: check its other records once.
                let this = (global_key, tuple.lat, tuple.lon);
                for other in tuples[tuple_ptr + 1..]
                    .iter()
                    .take_while(|t| key(t.id) == global_key)
                {
                    check_duplicate(family, this, other)?;
                }
                last = Some(this);
            }
            let blob_start = job
                .way_slot_starts
                .get(record.blob_idx() as usize)
                .copied()
                .ok_or_else(|| {
                    Error::internal(format!(
                        "stage2: record blob_idx {} out of range (way_slot_starts.len()={})",
                        record.blob_idx(),
                        job.way_slot_starts.len(),
                    ))
                })?;
            let linear_slot_pos = blob_start + u64::from(record.blob_local_slot());
            // Pin = the node is shared: it occurs in at least two
            // non-closure ref positions. All occurrences of an id form one
            // run, so the whole run is counted once (from its start) and
            // every record inherits the decision - including the trailing
            // closure ref, which mirrors bit 0 by construction.
            let pin = if job.inject_prepass {
                let local = record.local();
                if run_pin_cache.map(|(m, _)| m) != Some(local) {
                    let run = records[*record_ptr..]
                        .iter()
                        .take_while(|r| r.local() == local);
                    let unflagged = run.filter(|r| !r.closure()).count();
                    run_pin_cache = Some((local, unflagged >= 2));
                }
                run_pin_cache.is_some_and(|(_, p)| p)
            } else {
                false
            };
            // Private scratch codec documented on ResolvedEntry: domain
            // validation above makes the shift lossless; the run-wide flag
            // also governs payload decoding.
            let entry = ResolvedEntry {
                slot_pos: linear_slot_pos,
                lat: if job.inject_prepass {
                    (tuple.lat << 1) | i32::from(pin)
                } else {
                    tuple.lat
                },
                lon: tuple.lon,
            };
            let bucket = entry.slot_bucket(job.total_slots, job.slot_bucket_count);
            // The bucket-local slot fits u32: the layout's widest bucket
            // was checked once, before stage 2.
            let (bucket_start, _) =
                slot_bucket_bounds(job.total_slots, job.slot_bucket_count, bucket);
            entry.write_to(bucket_start, out.entry_buf);
            out.slot_bufs[bucket].extend_from_slice(&out.entry_buf[..]);
            out.slot_counts[bucket] += 1;
            // Count on presence, not on truthiness. `resolved` already
            // means the id was found in the node stream; gating the
            // increment on a non-zero coordinate additionally reported every
            // genuine node at Null Island as missing, so the external
            // backend disagreed with the sparse one about the same input.
            // Absent is not zero.
            *out.local_resolved += 1;
            if out.slot_bufs[bucket].len() >= STAGE2_FLUSH_THRESHOLD {
                // Track max buffer size before flush.
                record_max(&c.max_slot_buffer_bytes, out.slot_bufs[bucket].len() as u64);
                flush_slot_buffer(
                    job,
                    c,
                    bucket,
                    &mut out.slot_bufs[bucket],
                    &mut out.slot_counts[bucket],
                )?;
            }
        }
        *record_ptr += 1;
    }
    *carry = last.filter(|r| r.0 == upper);
    Ok(())
}
pub(super) type SlotBuckets = SharedSlotBuckets;
