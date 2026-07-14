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
use std::path::Path;

use super::super::Result;
use super::blob_meta::BlobMeta;
use super::radix::{NUM_BUCKETS, ScratchDir};
use super::{BucketLayout, ID_RECORD_SIZE, IdRecord, NodeBlobInfo};

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
    write_ns: &'a std::sync::atomic::AtomicU64,
    write_calls: &'a std::sync::atomic::AtomicU64,
}

impl std::io::Write for TimedShardWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let t = std::time::Instant::now();
        let n = self.file.write(buf)?;
        #[allow(clippy::cast_possible_truncation)]
        self.write_ns.fetch_add(
            t.elapsed().as_nanos() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.write_calls
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
}

/// Stage 1 output handed to stage 2. Owns the `IdSet` (kept alive
/// because stage 2 needs `rank_if_set` for inline node-blob coord
/// resolution) and the per-blob rank mapping.
pub(super) struct Stage1Output {
    pub total_slots: u64,
    /// Per-(id-bucket) IdRecord counts produced by pass A. Stage 2
    /// reads it to skip empty buckets without a probe.
    pub id_bucket_counts: Vec<u64>,
    pub num_shard_workers: usize,
    pub node_blob_mapping: Vec<NodeBlobInfo>,
}

/// Build the way-blob schedule from the shared blob metadata scan.
pub(super) fn build_way_schedule(blob_meta: &[BlobMeta]) -> Result<Vec<WayBlobTask>> {
    crate::debug::emit_marker("EXTJOIN_S1_WAY_SCHEDULE_START");
    let t0 = std::time::Instant::now();
    let mut schedule: Vec<WayBlobTask> = Vec::new();
    let mut seq: u32 = 0;
    for meta in blob_meta {
        if !matches!(meta.kind, crate::blob_meta::ElemKind::Way) {
            continue;
        }
        schedule.push(WayBlobTask {
            seq,
            data_offset: meta.data_offset,
            data_size: meta.data_size,
        });
        seq += 1;
    }
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    {
        crate::debug::emit_counter("s1_way_schedule_blobs", schedule.len() as i64);
        crate::debug::emit_counter("s1_way_schedule_build_ms", t0.elapsed().as_millis() as i64);
    }
    crate::debug::emit_marker("EXTJOIN_S1_WAY_SCHEDULE_END");
    Ok(schedule)
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
    input: &Path,
    schedule: &[WayBlobTask],
    num_workers: usize,
    scratch: &ScratchDir,
    layout: &BucketLayout,
    ref_count_sidecar: &Path,
    per_way_refcount_sidecar: &Path,
    inject_prepass: bool,
) -> Result<(u64, Vec<u64>)> {
    use std::os::unix::fs::FileExt as _;

    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?,
    );

    crate::debug::emit_marker("EXTJOIN_S1_PASS_A_START");

    let s1a_pread_ms = std::sync::atomic::AtomicU64::new(0);
    let s1a_decompress_ms = std::sync::atomic::AtomicU64::new(0);
    let s1a_scan_way_refs_ms = std::sync::atomic::AtomicU64::new(0);
    let s1a_bytes_read = std::sync::atomic::AtomicU64::new(0);
    let s1a_pread_calls = std::sync::atomic::AtomicU64::new(0);

    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let mut total_refs: u64 = 0;

    let s1a_per_way_sidecar_bytes = std::sync::atomic::AtomicU64::new(0);
    // A1 step 2 counters: per-ref IdRecord emission to id-bucket
    // shards. Records that can't be encoded (negative ref or
    // node_id > max_node_id) are skipped; those slots fall through to
    // stage 4 as zero-coord missing locations, matching the existing
    // missing-ref behaviour pinned by `missing_node_refs_get_zero_coordinates`.
    let s1a_id_emit_ms = std::sync::atomic::AtomicU64::new(0);
    let s1a_id_records_emitted = std::sync::atomic::AtomicU64::new(0);
    let s1a_id_records_skipped = std::sync::atomic::AtomicU64::new(0);
    let s1a_id_shard_bytes_written = std::sync::atomic::AtomicU64::new(0);
    let s1a_id_shard_write_ns = std::sync::atomic::AtomicU64::new(0);
    let s1a_id_shard_write_calls = std::sync::atomic::AtomicU64::new(0);
    let s1a_id_shard_flush_err: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    // Per-(id-bucket) record counts. Each worker tallies into a local
    // `Vec<u64>` (no atomic in the hot loop - the per-record cost
    // would dominate; europe sees ~4.7B records and a contended
    // AtomicU64::fetch_add adds ~25 ns each, which alone explains a
    // 30s+ wall regression vs the pre-A1 pass B baseline that used
    // local counters). Workers push their tallies into this shared
    // mutex on exit; the orchestrator merges into the
    // `Stage1Output.id_bucket_counts` vector after the scope joins.
    let s1a_id_bucket_counts_workers: std::sync::Mutex<Vec<Vec<u64>>> =
        std::sync::Mutex::new(Vec::new());
    {
        type PassAItem = (u32, std::result::Result<(u64, Vec<u32>), String>);
        let (tx, rx) = std::sync::mpsc::sync_channel::<PassAItem>(32);
        let schedule_ref = schedule;
        let next_ref = &next_idx;
        let s1a_pread_ref = &s1a_pread_ms;
        let s1a_decompress_ref = &s1a_decompress_ms;
        let s1a_scan_ref = &s1a_scan_way_refs_ms;
        let s1a_bytes_ref = &s1a_bytes_read;
        let s1a_pread_calls_ref = &s1a_pread_calls;
        let s1a_per_way_bytes_ref = &s1a_per_way_sidecar_bytes;
        let s1a_id_emit_ref = &s1a_id_emit_ms;
        let s1a_id_emitted_ref = &s1a_id_records_emitted;
        let s1a_id_skipped_ref = &s1a_id_records_skipped;
        let s1a_id_bytes_ref = &s1a_id_shard_bytes_written;
        let s1a_id_shard_write_ns_ref = &s1a_id_shard_write_ns;
        let s1a_id_shard_write_calls_ref = &s1a_id_shard_write_calls;
        let s1a_id_flush_err_ref = &s1a_id_shard_flush_err;
        let s1a_id_bucket_counts_workers_ref = &s1a_id_bucket_counts_workers;
        let layout_ref = layout;
        let scratch_ref = scratch;

        std::thread::scope(|scope| -> Result<()> {
            for worker_id in 0..num_workers {
                let file = std::sync::Arc::clone(&shared_file);
                let tx = tx.clone();
                scope.spawn(move || {
                    use std::sync::atomic::Ordering::Relaxed;
                    let mut read_buf: Vec<u8> = Vec::new();
                    let mut decompress_buf: Vec<u8> = Vec::new();
                    let mut refs_buf: Vec<i64> = Vec::new();
                    let mut group_starts: Vec<(usize, usize)> = Vec::new();
                    // Lazy-initialised on first blob so creation errors
                    // flow into the per-blob IIFE result and propagate
                    // through the existing tx/rx channel.
                    let mut id_shard_writers: Vec<Option<BufWriter<TimedShardWriter<'_>>>> =
                        Vec::new();
                    let mut id_rec_buf = [0u8; ID_RECORD_SIZE];
                    let mut id_bucket_local_counts: Vec<u64> = vec![0; NUM_BUCKETS];

                    loop {
                        let idx = next_ref.fetch_add(1, Relaxed);
                        if idx >= schedule_ref.len() {
                            break;
                        }
                        let task = &schedule_ref[idx];

                        let result: std::result::Result<(u64, Vec<u32>), String> = (|| {
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

                            let t2 = std::time::Instant::now();
                            let mut blob_node_ids: Vec<i64> = Vec::new();
                            let mut per_way_rcs: Vec<u32> = Vec::new();
                            let mut closure_slots: Vec<bool> = Vec::new();
                            crate::scan::way::scan_way_refs(
                                &decompress_buf,
                                &mut refs_buf,
                                &mut group_starts,
                                |_way_id, refs| {
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
                                        closure_slots.extend(
                                            (0..refs.len()).map(|i| closed && i + 1 == refs.len()),
                                        );
                                    }
                                    #[allow(clippy::cast_possible_truncation)]
                                    per_way_rcs.push(refs.len() as u32);
                                },
                            )
                            .map_err(|e| e.to_string())?;
                            #[allow(clippy::cast_possible_truncation)]
                            s1a_scan_ref.fetch_add(t2.elapsed().as_millis() as u64, Relaxed);

                            // A1 step 2: emit IdRecords per ref into
                            // id-bucket shards. Records for negative refs
                            // or out-of-range node ids (> max_node_id)
                            // are skipped; those slots stay unresolved
                            // and stage 4 fills zero coordinates.
                            let t_id = std::time::Instant::now();
                            if id_shard_writers.is_empty() {
                                id_shard_writers.reserve(NUM_BUCKETS);
                                for bucket_idx in 0..NUM_BUCKETS {
                                    let path = scratch_ref
                                        .path
                                        .join(format!("id-W{worker_id}-{bucket_idx:03}"));
                                    let f = std::fs::File::create(&path).map_err(|e| {
                                        format!("create id shard {}: {e}", path.display(),)
                                    })?;
                                    id_shard_writers.push(Some(BufWriter::with_capacity(
                                        super::radix::BUCKET_BUF_SIZE,
                                        TimedShardWriter {
                                            file: f,
                                            write_ns: s1a_id_shard_write_ns_ref,
                                            write_calls: s1a_id_shard_write_calls_ref,
                                        },
                                    )));
                                }
                            }
                            let mut blob_emitted: u64 = 0;
                            let mut blob_skipped: u64 = 0;
                            let mut blob_bytes: u64 = 0;
                            for (i, &node_id) in blob_node_ids.iter().enumerate() {
                                let blob_local_slot = u32::try_from(i).map_err(|_| {
                                    format!("blob {} has > u32::MAX refs (i={i})", task.seq,)
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
                                    rec.write_to(&mut id_rec_buf);
                                    let writer = id_shard_writers[bucket_idx]
                                        .as_mut()
                                        .expect("shard writer initialised");
                                    writer
                                        .write_all(&id_rec_buf)
                                        .map_err(|e| format!("write id shard W{worker_id}: {e}"))?;
                                    id_bucket_local_counts[bucket_idx] += 1;
                                    blob_emitted += 1;
                                    blob_bytes += ID_RECORD_SIZE as u64;
                                } else {
                                    blob_skipped += 1;
                                }
                            }
                            #[allow(clippy::cast_possible_truncation)]
                            s1a_id_emit_ref.fetch_add(t_id.elapsed().as_millis() as u64, Relaxed);
                            s1a_id_emitted_ref.fetch_add(blob_emitted, Relaxed);
                            s1a_id_skipped_ref.fetch_add(blob_skipped, Relaxed);
                            s1a_id_bytes_ref.fetch_add(blob_bytes, Relaxed);

                            Ok((blob_node_ids.len() as u64, per_way_rcs))
                        })(
                        );

                        if tx.send((task.seq, result)).is_err() {
                            break;
                        }
                    }

                    // Flush id-shard writers before the worker thread
                    // exits. Drop would silently swallow flush errors,
                    // so do it explicitly and surface any failure via
                    // the shared mutex; the orchestrator checks after
                    // the scope joins.
                    for w in id_shard_writers.iter_mut().flatten() {
                        if let Err(e) = w.flush() {
                            let mut slot = s1a_id_flush_err_ref
                                .lock()
                                .expect("id shard flush error mutex");
                            if slot.is_none() {
                                *slot = Some(format!("flush id shard W{worker_id}: {e}",));
                            }
                            break;
                        }
                    }

                    // Hand the local per-bucket tally to the
                    // orchestrator for post-scope merge.
                    s1a_id_bucket_counts_workers_ref
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(id_bucket_local_counts);
                });
            }
            drop(tx);

            let mut sidecar_writer = BufWriter::with_capacity(
                64 * 1024,
                std::fs::File::create(ref_count_sidecar)
                    .map_err(|e| format!("create sidecar: {e}"))?,
            );
            let mut per_way_writer = BufWriter::with_capacity(
                256 * 1024,
                std::fs::File::create(per_way_refcount_sidecar)
                    .map_err(|e| format!("create per-way sidecar: {e}"))?,
            );
            let mut varint_buf: Vec<u8> = Vec::with_capacity(1024);
            let mut reorder: crate::reorder_buffer::ReorderBuffer<
                std::result::Result<(u64, Vec<u32>), String>,
            > = crate::reorder_buffer::ReorderBuffer::with_capacity(64);

            for (seq_num, item) in rx {
                reorder.push(seq_num as usize, item);
                while let Some(result) = reorder.pop_ready() {
                    let (ref_count, per_way_rcs) =
                        result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                    sidecar_writer.write_all(&ref_count.to_le_bytes())?;
                    total_refs += ref_count;

                    varint_buf.clear();
                    protohoggr::encode_varint(&mut varint_buf, per_way_rcs.len() as u64);
                    for rc in &per_way_rcs {
                        protohoggr::encode_varint(&mut varint_buf, u64::from(*rc));
                    }
                    per_way_writer.write_all(&varint_buf)?;
                    s1a_per_way_bytes_ref.fetch_add(
                        varint_buf.len() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
            }
            sidecar_writer.write_all(&total_refs.to_le_bytes())?;
            sidecar_writer.flush()?;
            per_way_writer.flush()?;
            Ok(())
        })?;
    }

    // Surface any worker-side flush errors from the id-shard writers.
    let mut id_flush_slot = s1a_id_shard_flush_err
        .lock()
        .map_err(|e| format!("id shard flush mutex poisoned: {e}"))?;
    if let Some(err) = id_flush_slot.take() {
        return Err(err.into());
    }
    drop(id_flush_slot);

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter(
            "s1a_pread_ms",
            s1a_pread_ms.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s1a_decompress_ms",
            s1a_decompress_ms.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s1a_scan_way_refs_ms",
            s1a_scan_way_refs_ms.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s1a_bytes_read",
            s1a_bytes_read.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s1a_pread_calls",
            s1a_pread_calls.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s1a_per_way_sidecar_bytes",
            s1a_per_way_sidecar_bytes.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s1a_id_emit_ms",
            s1a_id_emit_ms.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s1a_id_records_emitted",
            s1a_id_records_emitted.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s1a_id_records_skipped",
            s1a_id_records_skipped.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s1a_id_shard_bytes_written",
            s1a_id_shard_bytes_written.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s1a_id_shard_write_ms",
            (s1a_id_shard_write_ns.load(std::sync::atomic::Ordering::Relaxed) / 1_000_000) as i64,
        );
        crate::debug::emit_counter(
            "s1a_id_shard_write_calls",
            s1a_id_shard_write_calls.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
    }
    crate::debug::emit_marker("EXTJOIN_S1_PASS_A_END");

    let mut id_bucket_counts: Vec<u64> = vec![0; NUM_BUCKETS];
    for worker_counts in s1a_id_bucket_counts_workers
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        for (i, &c) in worker_counts.iter().enumerate() {
            id_bucket_counts[i] += c;
        }
    }

    Ok((total_refs, id_bucket_counts))
}

/// Header-only walk of node blobs that builds the `NodeBlobInfo` mapping
/// stage 2 uses to find which blobs cover each rank bucket.
///
/// Replaces the historical 82 GB `coords_by_rank` file. No decompression
/// happens here - for each node blob we read its indexdata `(min_id, max_id)`
/// Stage 2 partitions this slice by `[min_id, max_id]` to find blobs
/// that intersect each id bucket; the ranges are non-overlapping and
/// monotonic in id because the input PBF is sorted by node id.
#[hotpath::measure]
pub(super) fn build_node_blob_mapping(blob_meta: &[BlobMeta]) -> Result<Vec<NodeBlobInfo>> {
    crate::debug::emit_marker("EXTJOIN_S1_NODE_MAP_START");
    let t0 = std::time::Instant::now();

    let mut mapping: Vec<NodeBlobInfo> = Vec::new();

    for meta in blob_meta {
        if !matches!(meta.kind, crate::blob_meta::ElemKind::Node) {
            continue;
        }
        // Reject malformed indexdata at the trust boundary - a blob
        // whose metadata advertises max_id < min_id is a producer bug
        // and stage 2's id-range partition would otherwise consume
        // some other blob's records as if they belonged here.
        if meta.max_id < meta.min_id {
            return Err(format!(
                "altw stage 1: blob at data_offset={} has reversed \
                 indexdata range [min_id={}, max_id={}]",
                meta.data_offset, meta.min_id, meta.max_id,
            )
            .into());
        }
        mapping.push(NodeBlobInfo {
            data_offset: meta.data_offset,
            data_size: meta.data_size,
            min_id: meta.min_id,
            max_id: meta.max_id,
        });
    }

    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    {
        crate::debug::emit_counter("s1_node_map_blobs", mapping.len() as i64);
        crate::debug::emit_counter("s1_node_map_build_ms", t0.elapsed().as_millis() as i64);
    }
    crate::debug::emit_marker("EXTJOIN_S1_NODE_MAP_END");
    Ok(mapping)
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
    input: &Path,
    _direct_io: bool,
    scratch: &ScratchDir,
    layout: &BucketLayout,
    ref_count_sidecar: &Path,
    per_way_refcount_sidecar: &Path,
    inject_prepass: bool,
) -> Result<Stage1Output> {
    let schedule = build_way_schedule(blob_meta)?;

    // Cap num_workers against the file descriptor budget. Pass B below
    // holds `num_workers * NUM_BUCKETS` rank-shard files open
    // concurrently (see `stage1.rs` Pass B loop at lines 385-405). On a
    // host with default soft ulimit (1024) this blows past RLIMIT_NOFILE
    // at ~4 workers, so we self-raise to the hard cap first and then
    // floor the worker count to what actually fits. Narrate the outcome
    // to stderr so the user sees what adjustments we made on their
    // behalf.
    let cpu_cap = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4);
    let fd_budget = super::raise_nofile_to_hard_cap();
    eprintln!("[altw external] {fd_budget}");
    let effective_soft = fd_budget.effective_soft;
    // Headroom for input PBF, scratch dir, sidecars, stdin/stdout/stderr,
    // mpsc channel fds, rayon worker fds, page-cache probe fds, etc.
    const HEADROOM_FDS: u64 = 64;
    let buckets_per_worker = NUM_BUCKETS as u64;
    if effective_soft < HEADROOM_FDS + buckets_per_worker {
        let min_needed = HEADROOM_FDS + buckets_per_worker;
        return Err(format!(
            "altw external: file descriptor limit too low \
             (RLIMIT_NOFILE = {effective_soft}, need >= {min_needed} \
             for even a single shard worker). Raise with \
             `ulimit -n {min_needed}` (or higher) and retry."
        )
        .into());
    }
    #[allow(clippy::cast_possible_truncation)]
    let fd_cap = ((effective_soft - HEADROOM_FDS) / buckets_per_worker) as usize;
    let num_workers = cpu_cap.min(fd_cap).max(1);
    if num_workers < cpu_cap {
        eprintln!(
            "[altw external] num_workers = {num_workers} \
             (capped from cpu={cpu_cap} by fd budget; each worker holds {NUM_BUCKETS} rank-shards)"
        );
    } else {
        eprintln!(
            "[altw external] num_workers = {num_workers} \
             (cpu-bound; fd budget allows up to {fd_cap} workers)"
        );
    }
    crate::debug::emit_counter(
        "extjoin_nofile_soft_cap",
        #[allow(clippy::cast_possible_wrap)]
        {
            effective_soft as i64
        },
    );
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("extjoin_cpu_cap_workers", cpu_cap as i64);
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("extjoin_fd_cap_workers", fd_cap as i64);

    let (total_refs, id_bucket_counts) = stage1_pass_a(
        input,
        &schedule,
        num_workers,
        scratch,
        layout,
        ref_count_sidecar,
        per_way_refcount_sidecar,
        inject_prepass,
    )?;

    // Build the per-blob mapping (header-only walk - no decompression).
    let node_blob_mapping = build_node_blob_mapping(blob_meta)?;

    Ok(Stage1Output {
        total_slots: total_refs,
        id_bucket_counts,
        num_shard_workers: num_workers,
        node_blob_mapping,
    })
}
