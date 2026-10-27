//! Stage 1: Single-pass way scan + node-blob mapping.
//!
//!   Pass A: parallel scan of way blobs. Per ref it emits one
//!     [`super::IdRecord`] into the appropriate id-bucket shard
//!     (1 of 256, keyed by `node_id / bucket_width`) and accumulates
//!     a per-way refcount sidecar plus the total ref count. Negative
//!     refs and refs above the indexdata-derived `max_node_id` are
//!     soft-skipped: the slot index still advances so stage 4 fills
//!     zero coords for those positions, matching the existing
//!     missing-ref behaviour.
//!   Node blob mapping: header-only walk of node blobs that records
//!     each blob's `(data_offset, data_size, min_id, max_id)`.
//!     Stage 2 partitions the slice by `[min_id, max_id]` to find
//!     blobs intersecting each id bucket.

use std::io::{BufWriter, Write as _};

use super::super::Result;
use super::blob_meta::BlobMeta;
use super::{BucketLayout, ID_RECORD_SIZE, IdRecord, NodeBlobInfo};
use crate::error::Error;
use crate::scratch::{RunScratch, ScratchPath};
use crate::tunables::{
    BUCKET_BUF_SIZE, DECODE_WORKERS_FALLBACK, DECODE_WORKERS_MIN, DECODE_WORKERS_RESERVED,
    NUM_BUCKETS, PER_WAY_REFCOUNT_SIDECAR_BUF_BYTES, REF_COUNT_SIDECAR_BUF_BYTES,
    STAGE1_HEADROOM_FDS, STAGE1_VARINT_BUF_CAPACITY,
};

crate::counters::counter_set! {
    /// Pass A counters, shared by every pass A worker; bumped once per way
    /// blob (or per shard drain), never per ref.
    struct PassACounters {
        pread_ms => "s1a_pread_ms",
        decompress_ms => "s1a_decompress_ms",
        scan_way_refs_ms => "s1a_scan_way_refs_ms",
        bytes_read => "s1a_bytes_read",
        pread_calls => "s1a_pread_calls",
        per_way_sidecar_bytes => "s1a_per_way_sidecar_bytes",
        /// Per-ref IdRecord emission to id-bucket shards. Records that can't
        /// be encoded (negative ref or node_id > max_node_id) are skipped;
        /// those slots fall through to stage 4 as zero-coord missing
        /// locations, matching the existing missing-ref behaviour pinned by
        /// `missing_node_refs_get_zero_coordinates`.
        id_emit_ms => "s1a_id_emit_ms",
        id_records_emitted => "s1a_id_records_emitted",
        id_records_skipped => "s1a_id_records_skipped",
        id_shard_bytes_written => "s1a_id_shard_bytes_written",
        id_shard_write_ns => "s1a_id_shard_write_ms" / 1_000_000,
        id_shard_write_calls => "s1a_id_shard_write_calls",
    }
}

/// `io::Write` shim between the id-shard `BufWriter`s and their files
/// that attributes real write calls (BufWriter buffer drains, 256 KB
/// granularity). `s1a_id_emit_ms` times the whole emission loop -
/// locate + closure staging + record encode + BufWriter memcpy + any
/// drains it triggers - so without this split, "CPU-limited emit" and
/// "disk-limited writeback" are indistinguishable in one run. The
/// atomics are shared across workers; at ~583 K drains per planet run
/// the fetch_add cost is noise.
struct TimedShardWriter<'a> {
    file: std::fs::File,
    counters: &'a PassACounters,
}

impl std::io::Write for TimedShardWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let t = crate::counters::Stopwatch::start();
        let n = self.file.write(buf)?;
        t.record_ns(&self.counters.id_shard_write_ns);
        self.counters
            .id_shard_write_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// Way-blob schedule entry for the parallel way scans.
pub(super) struct WayBlobTask {
    pub(super) seq: u32,
    pub(super) data_offset: u64,
    pub(super) data_size: usize,
    /// The blob's indexdata claim, which pass A holds to the ways it scans.
    pub(super) claim: crate::blob_meta::BlobIndex,
}

/// One pass A worker's shard, owned by the caller so the id-shard writers
/// can be flushed (and their errors surfaced) after the workers join.
struct Stage1Shard<'a> {
    worker_id: usize,
    /// Lazy-initialised on the worker's first blob so a creation error
    /// fails that blob, in order. All or nothing: the set is built aside and
    /// installed whole, because a partial set would be indexed past its end
    /// on the next blob.
    id_shard_writers: Vec<BufWriter<TimedShardWriter<'a>>>,
    id_rec_buf: [u8; ID_RECORD_SIZE],
    id_bucket_local_counts: Vec<u64>,
}

/// One pass A worker's thread-local decode buffers.
struct Stage1Local {
    read_buf: Vec<u8>,
    decompress_buf: Vec<u8>,
    refs_buf: Vec<i64>,
    group_starts: Vec<(usize, usize)>,
}

/// Pass A's consumer: writes the ref-count and per-way refcount sidecars in
/// blob order.
struct Stage1Consumer<'a> {
    sidecar_writer: BufWriter<std::fs::File>,
    per_way_writer: BufWriter<std::fs::File>,
    varint_buf: Vec<u8>,
    total_refs: u64,
    counters: &'a PassACounters,
    /// The way blobs in input order, held to the sorted header across blob
    /// boundaries by their decoded first ways. Each blob's own walk checks
    /// the order inside it, and its claim's exact range bounds its last key,
    /// but a mixed-sign range cannot name its first element (blobs `[-1]`
    /// then `[0, -2, 1]` pass the metadata scan's range watermark).
    schedule: &'a [WayBlobTask],
    order: crate::osm_id::OrderWatermark,
    input: &'a std::path::Path,
}

impl Stage1Consumer<'_> {
    /// Write the ref-count sidecar's trailer and flush both sidecars;
    /// returns the total ref count.
    fn finish(mut self) -> Result<u64> {
        self.sidecar_writer
            .write_all(&self.total_refs.to_le_bytes())?;
        self.sidecar_writer.flush()?;
        self.per_way_writer.flush()?;
        Ok(self.total_refs)
    }
}

impl crate::ordered_pool::OrderedConsumer<(u64, Vec<u32>, Option<i64>)> for Stage1Consumer<'_> {
    fn consume(
        &mut self,
        task: usize,
        (ref_count, per_way_rcs, first): (u64, Vec<u32>, Option<i64>),
    ) -> crate::Result<()> {
        // A way blob holds at least one way (its exact claim was checked),
        // so `first` is always present here.
        if let Some(first) = first {
            let claim = &self.schedule[task].claim;
            if let Err(v) = self.order.observe_keys(
                crate::ElementKind::Way.ordinal(),
                crate::osm_id::osm_id_key(first),
                crate::osm_id::blob_osm_last_key(claim.min_id, claim.max_id),
            ) {
                return Err(crate::error::new_unsorted(format!(
                    "altw external: refusing an input whose blob contradicts its \
                     Sort.Type_then_ID header - a way blob starting at way {} follows way \
                     blobs already reaching way {}",
                    v.first_id, v.previous_id
                ))
                .locate_blob(crate::error::Location::at_offset(
                    self.input,
                    self.schedule[task].data_offset,
                )));
            }
        }
        self.sidecar_writer.write_all(&ref_count.to_le_bytes())?;
        self.total_refs += ref_count;

        self.varint_buf.clear();
        protohoggr::encode_varint(&mut self.varint_buf, per_way_rcs.len() as u64);
        for rc in &per_way_rcs {
            protohoggr::encode_varint(&mut self.varint_buf, u64::from(*rc));
        }
        self.per_way_writer.write_all(&self.varint_buf)?;
        self.counters.per_way_sidecar_bytes.fetch_add(
            self.varint_buf.len() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        Ok(())
    }
}

/// Flush every shard's id-shard writers, one thread per shard; the first
/// failure (in shard order) names its shard file.
fn flush_shards(shards: &mut [Stage1Shard<'_>], scratch: &RunScratch) -> Result<()> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = shards
            .iter_mut()
            .map(|shard| {
                scope.spawn(move || -> Result<()> {
                    for (bucket_idx, w) in shard.id_shard_writers.iter_mut().enumerate() {
                        w.flush().map_err(|e| {
                            shard_error(e, "flush", scratch, shard.worker_id, bucket_idx)
                        })?;
                    }
                    Ok(())
                })
            })
            .collect();
        let mut first = Ok(());
        for handle in handles {
            let result = handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            if first.is_ok() {
                first = result;
            }
        }
        first
    })
}

/// Stage 1 output handed to stage 2: slot and id-bucket record counts,
/// the shard worker count, and the per-node-blob mapping.
pub(super) struct Stage1Output {
    pub total_slots: u64,
    /// Per-(id-bucket) IdRecord counts produced by pass A. Stage 2
    /// reads it to skip empty buckets without a probe.
    pub id_bucket_counts: Vec<u64>,
    pub num_shard_workers: usize,
    pub node_blob_mapping: Vec<NodeBlobInfo>,
}

/// A failed write or flush of one id shard, naming the shard file (its path
/// is recomputed here, on the error path only).
#[cold]
fn shard_error(
    err: std::io::Error,
    what: &str,
    scratch: &RunScratch,
    worker_id: usize,
    bucket_idx: usize,
) -> Error {
    let shard = super::radix::id_shard_path(scratch, worker_id, bucket_idx).map_or_else(
        |_| format!("worker {worker_id} bucket {bucket_idx}"),
        |path| path.display().to_string(),
    );
    Error::from(err).context(format!("failed to {what} id shard {shard}"))
}

/// Build the way-blob schedule from the shared blob metadata scan.
pub(super) fn build_way_schedule(
    blob_meta: &[BlobMeta],
    invocation: &crate::host::Invocation,
) -> Vec<WayBlobTask> {
    invocation.marker("EXTJOIN_S1_WAY_SCHEDULE_START");
    let t0 = crate::counters::Stopwatch::start();
    let mut schedule: Vec<WayBlobTask> = Vec::new();
    let mut seq: u32 = 0;
    for meta in blob_meta {
        if !matches!(meta.kind, crate::ElementKind::Way) {
            continue;
        }
        schedule.push(WayBlobTask {
            seq,
            data_offset: meta.data_offset,
            data_size: meta.data_size,
            claim: meta.claim(),
        });
        seq += 1;
    }
    invocation.counter("s1_way_schedule_blobs", schedule.len());
    invocation.counter("s1_way_schedule_build_ms", t0.elapsed_ms());
    invocation.marker("EXTJOIN_S1_WAY_SCHEDULE_END");
    schedule
}

/// Pass A: parallel way scan that emits one [`IdRecord`] per ref to
/// the appropriate id-bucket shard, writes the two ref-count sidecars
/// in blob order, and tracks per-bucket record counts.
///
/// Returns `(total_refs, id_bucket_counts)`.
/// `id_bucket_counts[k]` is the number of IdRecords pass A wrote to
/// id-bucket `k` across all workers; stage 2 reads it to skip empty
/// buckets.
#[hotpath::measure]
#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
pub(super) fn stage1_pass_a(
    input: &crate::read::input::PinnedInput,
    schedule: &[WayBlobTask],
    num_workers: usize,
    scratch: &RunScratch,
    layout: &BucketLayout,
    ref_count_sidecar: &ScratchPath,
    per_way_refcount_sidecar: &ScratchPath,
    inject_prepass: bool,
    host: &crate::host::Host,
) -> Result<(u64, Vec<u64>)> {
    use std::os::unix::fs::FileExt as _;

    let shared_file = std::sync::Arc::new(crate::read::input::InputSource::from(input).open()?);
    let input = input.path();

    host.invocation.marker("EXTJOIN_S1_PASS_A_START");

    let counters = PassACounters::new();
    let counters_ref = &counters;
    let layout_ref = layout;
    let scratch_ref = scratch;
    // One shard per worker, owned here: the id-shard writers and the
    // per-(id-bucket) record counts. Each worker tallies into its shard's
    // `Vec<u64>` (no atomic in the hot loop - the per-record cost would
    // dominate; europe sees ~4.7B records and a contended
    // AtomicU64::fetch_add adds ~25 ns each, which alone explains a 30s+
    // wall regression vs the pre-A1 pass B baseline that used local
    // counters); they are merged after the run.
    let mut shards: Vec<Stage1Shard<'_>> = (0..num_workers)
        .map(|worker_id| Stage1Shard {
            worker_id,
            id_shard_writers: Vec::new(),
            id_rec_buf: [0u8; ID_RECORD_SIZE],
            id_bucket_local_counts: vec![0; NUM_BUCKETS],
        })
        .collect();
    let mut consumer = Stage1Consumer {
        sidecar_writer: BufWriter::with_capacity(
            REF_COUNT_SIDECAR_BUF_BYTES,
            crate::scratch::create_file(ref_count_sidecar)?,
        ),
        per_way_writer: BufWriter::with_capacity(
            PER_WAY_REFCOUNT_SIDECAR_BUF_BYTES,
            crate::scratch::create_file(per_way_refcount_sidecar)?,
        ),
        varint_buf: Vec::with_capacity(STAGE1_VARINT_BUF_CAPACITY),
        total_refs: 0,
        counters: counters_ref,
        schedule,
        order: crate::osm_id::OrderWatermark::default(),
        input,
    };

    // The ordered pool writes the two sidecars in blob order; cancellation
    // is checked on its consumer.
    let pool_result = crate::ordered_pool::run_ordered(
        schedule.len(),
        &mut shards,
        crate::ordered_pool::PoolConfig {
            window: crate::ordered_pool::default_window(num_workers),
            cancel: &host.invocation,
            waits: crate::ordered_pool::Waits::default(),
        },
        |_| Stage1Local {
            read_buf: Vec::new(),
            decompress_buf: Vec::new(),
            refs_buf: Vec::new(),
            group_starts: Vec::new(),
        },
        |shard: &mut Stage1Shard<'_>, local: &mut Stage1Local, idx| {
            use std::sync::atomic::Ordering::Relaxed;
            let Stage1Local {
                read_buf,
                decompress_buf,
                refs_buf,
                group_starts,
            } = local;
            let Stage1Shard {
                worker_id,
                id_shard_writers,
                id_rec_buf,
                id_bucket_local_counts,
            } = shard;
            let worker_id = *worker_id;
            let file = &shared_file;
            let task = &schedule[idx];
            // A failure reading this blob is about the input:
            // located with its path and the blob's payload
            // offset (cold). Scratch failures name their shard.
            let in_input = |e: Error| {
                e.locate_blob(crate::error::Location::at_offset(input, task.data_offset))
            };

            let result: crate::Result<(u64, Vec<u32>, Option<i64>)> = (|| {
                let t0 = crate::counters::Stopwatch::start();
                read_buf.resize(task.data_size, 0);
                file.read_exact_at(read_buf, task.data_offset)
                    .map_err(|e| in_input(Error::from(e).context("pass A pread")))?;
                t0.record_ms(&counters_ref.pread_ms);
                counters_ref
                    .bytes_read
                    .fetch_add(task.data_size as u64, Relaxed);
                counters_ref.pread_calls.fetch_add(1, Relaxed);

                let t1 = crate::counters::Stopwatch::start();
                crate::blob::decompress_blob_raw(read_buf, decompress_buf)
                    .map_err(|e| in_input(e.context("pass A decompress")))?;
                t1.record_ms(&counters_ref.decompress_ms);

                let t2 = crate::counters::Stopwatch::start();
                let mut blob_node_ids: Vec<i64> = Vec::new();
                let mut per_way_rcs: Vec<u32> = Vec::new();
                let mut closure_slots: Vec<bool> = Vec::new();
                // Every way blob is scanned here, so each one's claim is held
                // to its ways: no foreign kind (the reframe would copy it
                // through unindexed), the exact range and count, and the
                // header's order.
                let mut walk = crate::blob_meta::ClaimWalk::new(
                    Some(&task.claim),
                    super::blob_meta::EXTERNAL_ORDER,
                );
                let mut walk_error: Option<Error> = None;
                let kinds = crate::scan::way::scan_way_refs(
                    decompress_buf,
                    refs_buf,
                    group_starts,
                    |way_id, refs| {
                        if walk_error.is_none()
                            && let Err(e) = walk.observe(way_id)
                        {
                            walk_error = Some(e);
                        }
                        blob_node_ids.extend_from_slice(refs);
                        // Closure flags exist only to feed the
                        // stage-2 pin logic, which is gated on
                        // the flag. Staging them unconditionally
                        // cost 12.4 B pushes per planet run on
                        // the plain path; `closure_slots` stays
                        // empty when the flag is off and the
                        // emission loop below must not index it.
                        if inject_prepass {
                            let closed = refs.len() >= 4 && refs.first() == refs.last();
                            closure_slots
                                .extend((0..refs.len()).map(|i| closed && i + 1 == refs.len()));
                        }
                        #[allow(clippy::cast_possible_truncation)]
                        per_way_rcs.push(refs.len() as u32);
                    },
                )
                .map_err(in_input)?;
                kinds
                    .refuse_under(Some(crate::ElementKind::Way))
                    .map_err(in_input)?;
                if let Some(e) = walk_error {
                    return Err(in_input(e));
                }
                let first_way = walk.finish().map_err(in_input)?;
                t2.record_ms(&counters_ref.scan_way_refs_ms);

                // Emit IdRecords per ref into
                // id-bucket shards. Records for negative refs
                // or out-of-range node ids (> max_node_id)
                // are skipped; those slots stay unresolved
                // and stage 4 fills zero coordinates.
                let t_id = crate::counters::Stopwatch::start();
                if id_shard_writers.is_empty() {
                    let mut fresh = Vec::with_capacity(NUM_BUCKETS);
                    for bucket_idx in 0..NUM_BUCKETS {
                        let path = super::radix::id_shard_path(scratch_ref, worker_id, bucket_idx)?;
                        let f = crate::scratch::create(&path).map_err(|e| {
                            Error::from(e).context(format!("create id shard {}", path.display()))
                        })?;
                        fresh.push(BufWriter::with_capacity(
                            BUCKET_BUF_SIZE,
                            TimedShardWriter {
                                file: f,
                                counters: counters_ref,
                            },
                        ));
                    }
                    *id_shard_writers = fresh;
                }
                let mut blob_emitted: u64 = 0;
                let mut blob_skipped: u64 = 0;
                let mut blob_bytes: u64 = 0;
                for (i, &node_id) in blob_node_ids.iter().enumerate() {
                    let blob_local_slot = u32::try_from(i).map_err(|_| {
                        Error::invalid_data(format!("way blob has > u32::MAX refs (i={i})"))
                    })?;
                    let location = if node_id < 0 {
                        None
                    } else {
                        #[allow(clippy::cast_sign_loss)]
                        layout_ref.locate(node_id as u64)
                    };
                    if let Some((bucket_idx, local_node_id)) = location {
                        let rec = IdRecord {
                            local_node_id: local_node_id
                                | if inject_prepass && closure_slots[i] {
                                    super::CLOSURE_FLAG
                                } else {
                                    0
                                },
                            blob_idx: task.seq,
                            blob_local_slot,
                        };
                        rec.write_to(id_rec_buf);
                        id_shard_writers[bucket_idx]
                            .write_all(id_rec_buf.as_slice())
                            .map_err(|e| {
                                shard_error(e, "write", scratch_ref, worker_id, bucket_idx)
                            })?;
                        id_bucket_local_counts[bucket_idx] += 1;
                        blob_emitted += 1;
                        blob_bytes += ID_RECORD_SIZE as u64;
                    } else {
                        blob_skipped += 1;
                    }
                }
                t_id.record_ms(&counters_ref.id_emit_ms);
                counters_ref
                    .id_records_emitted
                    .fetch_add(blob_emitted, Relaxed);
                counters_ref
                    .id_records_skipped
                    .fetch_add(blob_skipped, Relaxed);
                counters_ref
                    .id_shard_bytes_written
                    .fetch_add(blob_bytes, Relaxed);

                Ok((blob_node_ids.len() as u64, per_way_rcs, first_way))
            })();
            result
        },
        &mut consumer,
    );

    // Flush the id-shard writers now the workers have joined: explicitly,
    // since Drop would silently swallow a flush error, and in parallel, one
    // thread per shard, as each worker used to flush its own on exit. Also
    // after a pool error; that error wins over a flush failure.
    let flush_result = flush_shards(&mut shards, scratch_ref);
    pool_result?;
    flush_result?;
    let total_refs = consumer.finish()?;

    counters.emit_all(&host.invocation);
    host.invocation.marker("EXTJOIN_S1_PASS_A_END");

    let mut id_bucket_counts: Vec<u64> = vec![0; NUM_BUCKETS];
    for shard in &shards {
        for (i, &c) in shard.id_bucket_local_counts.iter().enumerate() {
            id_bucket_counts[i] += c;
        }
    }

    Ok((total_refs, id_bucket_counts))
}

/// Header-only walk of node blobs that builds the `NodeBlobInfo` mapping
/// stage 2 uses to find which blobs cover each id bucket.
///
/// Replaces the historical 82 GB `coords_by_rank` file. No decompression
/// happens here - for each node blob we read its indexdata `(min_id, max_id)`.
/// Stage 2 partitions this slice by `[min_id, max_id]` to find blobs
/// that intersect each id bucket. The ranges are well-formed, non-negative
/// and in non-decreasing id order (equal boundaries allowed) because
/// `blob_meta::scan_blob_metadata` refused the input otherwise; that scan
/// is the trust boundary, so nothing is re-checked here.
#[hotpath::measure]
pub(super) fn build_node_blob_mapping(
    blob_meta: &[BlobMeta],
    invocation: &crate::host::Invocation,
) -> Vec<NodeBlobInfo> {
    invocation.marker("EXTJOIN_S1_NODE_MAP_START");
    let t0 = crate::counters::Stopwatch::start();

    let mut mapping: Vec<NodeBlobInfo> = Vec::new();

    for meta in blob_meta {
        if !matches!(meta.kind, crate::ElementKind::Node) {
            continue;
        }
        mapping.push(NodeBlobInfo {
            data_offset: meta.data_offset,
            data_size: meta.data_size,
            min_id: meta.min_id,
            max_id: meta.max_id,
            count: meta.count,
        });
    }

    invocation.counter("s1_node_map_blobs", mapping.len());
    invocation.counter("s1_node_map_build_ms", t0.elapsed_ms());
    invocation.marker("EXTJOIN_S1_NODE_MAP_END");
    mapping
}

/// Stage 1 entry point: pass A + node-blob mapping construction.
///
/// **Pass A**: parallel way scan that emits one IdRecord per ref to
/// the right id-bucket shard, plus the per-way refcount sidecar and
/// total ref-count sidecar in blob order.
///
/// **Mapping**: header-only walk of node blobs to compute the
/// `NodeBlobInfo` table stage 2 partitions by `[min_id, max_id]`.
#[hotpath::measure]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) fn stage1_way_pass(
    blob_meta: &[BlobMeta],
    input: &crate::read::input::PinnedInput,
    _direct_io: bool,
    scratch: &RunScratch,
    layout: &BucketLayout,
    ref_count_sidecar: &ScratchPath,
    per_way_refcount_sidecar: &ScratchPath,
    inject_prepass: bool,
    host: &crate::host::Host,
) -> Result<Stage1Output> {
    let schedule = build_way_schedule(blob_meta, &host.invocation);

    // Cap num_workers against the file descriptor allowance. Pass A
    // below holds `num_workers * NUM_BUCKETS` id-shard files open
    // concurrently. A default soft ulimit of 1024 cannot support four
    // workers, so ask the caller's process actions for an allowance - the
    // descriptors this run may open on top of those already open - and
    // floor the worker count to what fits. The CLI raises the soft limit
    // to the hard cap; NoProcessActions only probes it. Report the outcome
    // through the caller's diagnostics.
    // Do not restore the process limit here: ADR-0016 leaves mutation and
    // its lifetime with the caller, and restoring a snapshot could undo a
    // concurrent host adjustment. Fixed host grants also make this arithmetic
    // exercisable without mutating the test process. Private per-worker shards
    // keep pass A writes independent; shared appenders would add synchronization
    // to the measured external pipeline, not merely remove a limit probe.
    let cpu_cap = crate::host::sized_workers(
        &*host.resources,
        DECODE_WORKERS_RESERVED,
        DECODE_WORKERS_MIN,
        DECODE_WORKERS_FALLBACK,
    );
    let fd_budget = host
        .process
        .fd_allowance(cpu_cap as u64 * NUM_BUCKETS as u64 + STAGE1_HEADROOM_FDS);
    crate::host::diag!(
        host.diagnostics,
        Developer,
        "altw.external.nofile",
        "[altw external] {fd_budget}"
    );
    let available = fd_budget.descriptors;
    let buckets_per_worker = NUM_BUCKETS as u64;
    let min_needed = STAGE1_HEADROOM_FDS + buckets_per_worker;
    if available < min_needed {
        return Err(Error::invalid_argument(fd_allowance_too_low(
            &fd_budget, min_needed,
        )));
    }
    #[allow(clippy::cast_possible_truncation)]
    let fd_cap = ((available - STAGE1_HEADROOM_FDS) / buckets_per_worker) as usize;
    let num_workers = cpu_cap.min(fd_cap).max(1);
    if num_workers < cpu_cap {
        crate::host::diag!(
            host.diagnostics,
            Developer,
            "altw.external.workers",
            "[altw external] num_workers = {num_workers} \
             (capped from cpu={cpu_cap} by fd budget; each worker holds {NUM_BUCKETS} ID shards)"
        );
    } else {
        crate::host::diag!(
            host.diagnostics,
            Developer,
            "altw.external.workers",
            "[altw external] num_workers = {num_workers} \
             (cpu-bound; fd budget allows up to {fd_cap} workers)"
        );
    }
    if let Some(probe) = fd_budget.probe {
        host.invocation
            .counter("extjoin_nofile_soft_cap", probe.soft_limit);
    }
    host.invocation.counter("extjoin_fd_available", available);
    host.invocation.counter("extjoin_cpu_cap_workers", cpu_cap);
    host.invocation.counter("extjoin_fd_cap_workers", fd_cap);

    let (total_refs, id_bucket_counts) = stage1_pass_a(
        input,
        &schedule,
        num_workers,
        scratch,
        layout,
        ref_count_sidecar,
        per_way_refcount_sidecar,
        inject_prepass,
        host,
    )?;

    // Build the per-blob mapping (header-only walk - no decompression).
    let node_blob_mapping = build_node_blob_mapping(blob_meta, &host.invocation);

    Ok(Stage1Output {
        total_slots: total_refs,
        id_bucket_counts,
        num_shard_workers: num_workers,
        node_blob_mapping,
    })
}

/// The up-front error when the allowance cannot hold even one shard
/// worker. It names what the allowance was derived from, and suggests a
/// `ulimit` only where raising the process limit is the remedy.
fn fd_allowance_too_low(allowance: &crate::process::FdAllowance, need: u64) -> String {
    use crate::process::{FdAdjustment, FdCount};
    let available = allowance.descriptors;
    let Some(probe) = allowance.probe else {
        return format!(
            "altw external: the host granted {available} file descriptors, \
             need >= {need} for even a single shard worker"
        );
    };
    let soft = probe.soft_limit;
    let assumed = matches!(probe.adjustment, Some(FdAdjustment::ProbeFailed));
    let limit = if assumed {
        format!("RLIMIT_NOFILE unreadable, assumed {soft}")
    } else {
        format!("RLIMIT_NOFILE = {soft}")
    };
    let estimate = if assumed {
        " (an estimate: the limit could not be read)"
    } else {
        ""
    };
    match probe.open {
        FdCount::Counted(open) => format!(
            "altw external: file descriptor limit too low ({limit}, {open} already \
             open, {available} available; need >= {need} free for even a single \
             shard worker). Raise with `ulimit -n {}`{estimate} (or higher) and retry.",
            open + need
        ),
        FdCount::Uncounted => format!(
            "altw external: file descriptor limit too low ({limit}, need >= {need} \
             for even a single shard worker). Raise with `ulimit -n {need}`{estimate} \
             (or higher) and retry."
        ),
        FdCount::Failed(errno) => format!(
            "altw external: counting the process's open file descriptors failed \
             ({}), so no descriptor budget can be granted ({limit})",
            std::io::Error::from_raw_os_error(errno)
        ),
        FdCount::Exhausted(errno) if errno == libc::ENFILE => format!(
            "altw external: no file descriptor free ({}): the system-wide file table \
             is full; raising {limit} will not help",
            std::io::Error::from_raw_os_error(errno)
        ),
        FdCount::Exhausted(errno) => format!(
            "altw external: no file descriptor free ({}): the process already holds \
             every descriptor {limit} allows; close descriptors or raise the limit",
            std::io::Error::from_raw_os_error(errno)
        ),
    }
}
