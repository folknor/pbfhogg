//! Stage 3: Slot reorder - emit per-blob coord_payloads via the integrated
//! pipeline (the flat coord_slots intermediate was retired 2026-04).

use super::super::Result;
use super::radix::advise_dontneed_file;
use crate::error::Error;

use super::blob_bucket_index::{BlobBucketIntersection, classify_blobs_in_bucket};
use super::coord_payloads::{
    AbortOnDrop, ConcurrentBlobLocationRouter, PerWayRcs, StraddlerSide,
    encode_blob_payload_from_record,
};
use super::{COORD_SLOT_SIZE, RE_OFF_LAT, RE_OFF_LOCAL_SLOT_POS, RE_OFF_LON, RESOLVED_ENTRY_SIZE};

/// Lightweight reference to slot bucket paths + counts for stage 3.
pub(super) struct SlotBucketRef {
    pub(super) paths: Vec<std::path::PathBuf>,
    pub(super) entry_counts: Vec<u64>,
}

/// Inputs needed for the integrated dual-emit path. In the streaming
/// design (Commit B of #2), stage 3 publishes directly to the
/// `ConcurrentBlobLocationRouter` instead of returning manifests; the
/// `worker_tmp_paths` and `straddler_slots` members of the pre-streaming
/// shape are gone - worker tmp files live as `Arc<File>` inside the
/// router, and straddler halves are staged by `router.publish_straddler_half`.
pub(super) struct IntegratedInputs<'a> {
    pub way_slot_starts: &'a [u64],
    pub per_way_rcs: &'a PerWayRcs,
    pub router: &'a ConcurrentBlobLocationRouter,
    pub inject_prepass: bool,
}

crate::counters::counter_set! {
    /// Stage 3 counters, shared by every slot-reorder worker; bumped per
    /// slot bucket or per way blob it emits, never per slot.
    struct Stage3Counters {
        open_ms => "s3_open_ms",
        read_ms => "s3_read_ms",
        scatter_ms => "s3_scatter_ms",
        buckets_loaded => "s3_buckets_loaded",
        bytes_read => "s3_bytes_read",
        scatter_stores => "s3_scatter_stores",
        max_worker_buf_bytes => "s3_max_worker_buf_bytes",
        /// Slot buckets advised out of the page cache; zero off Linux.
        fadvise_calls => "s3_fadvise_calls",
        /// Bytes of those buckets; zero off Linux.
        fadvise_bytes => "s3_fadvise_bytes",
        encode_ms => "s3_encode_ms",
        straddler_copy_ms => "s3_straddler_copy_ms",
        worker_tmp_bytes => "s3_worker_tmp_bytes",
        tmp_pwrite_ns => "s3_tmp_pwrite_ms" / 1_000_000,
        tmp_pwrite_calls => "s3_tmp_pwrite_calls",
    }
}

#[hotpath::measure]
fn scatter_bucket_entries(
    data_buf: &[u8],
    bucket_idx: usize,
    scatter_buf: &mut [u8],
) -> crate::Result<u64> {
    if !data_buf.len().is_multiple_of(RESOLVED_ENTRY_SIZE) {
        return Err(Error::internal(format!(
            "slot bucket {bucket_idx} has {} trailing bytes (entry size {})",
            data_buf.len() % RESOLVED_ENTRY_SIZE,
            RESOLVED_ENTRY_SIZE,
        )));
    }

    let bucket_slots = scatter_buf.len() / COORD_SLOT_SIZE;
    let mut stores: u64 = 0;
    let (chunks, _rem) = data_buf.as_chunks::<RESOLVED_ENTRY_SIZE>();
    for chunk in chunks {
        let local_slot_pos = u32::from_le_bytes([
            chunk[RE_OFF_LOCAL_SLOT_POS],
            chunk[RE_OFF_LOCAL_SLOT_POS + 1],
            chunk[RE_OFF_LOCAL_SLOT_POS + 2],
            chunk[RE_OFF_LOCAL_SLOT_POS + 3],
        ]) as usize;
        if local_slot_pos >= bucket_slots {
            return Err(Error::internal(format!(
                "slot bucket {bucket_idx} local_pos {local_slot_pos} outside bucket slot span {bucket_slots}"
            )));
        }
        let offset = local_slot_pos * COORD_SLOT_SIZE;
        // Preserve the private ResolvedEntry latitude/pin encoding verbatim.
        // Zero-filled unwritten slots deliberately yield unpinned (0, 0)
        // orphan vertices. Presence/missing counts come from the stage-2
        // join, not a test of these bytes, so Null Island remains resolved.
        scatter_buf[offset..offset + 4].copy_from_slice(&chunk[RE_OFF_LAT..RE_OFF_LON]);
        scatter_buf[offset + 4..offset + 8]
            .copy_from_slice(&chunk[RE_OFF_LON..RESOLVED_ENTRY_SIZE]);
        stores += 1;
    }

    Ok(stores)
}

/// Pre-allocates the output file to `total_slots * 8` bytes (zero-filled
/// by the OS). Empty buckets need no explicit zero-write - the file is
/// already zeroed.
#[hotpath::measure]
#[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
pub(super) fn stage3_slot_reorder(
    slot_buckets: &SlotBucketRef,
    slot_bucket_count: usize,
    total_slots: u64,
    integrated: &IntegratedInputs<'_>,
    num_workers: usize,
    cancel: &crate::host::Recorder,
) -> Result<()> {
    // Floor division: every bucket is `range_size` slots wide except
    // the LAST, which extends to `total_slots` and may be wider. This
    // keeps the smallest-bucket-width = range_size, which the caller
    // sized so that range_size >= max_blob_slots - preserving the
    // 2-piece straddler invariant for small inputs. (See
    // `ResolvedEntry::slot_bucket` for the matching routing logic.)
    let range_size = total_slots / slot_bucket_count as u64;

    // `num_workers` is sized by the caller, which opens exactly that many
    // router worker files; `router.worker_file(worker_id)` relies on it.
    //
    // A hand-built claim-counter pool, deliberately not on
    // `crate::ordered_pool`: workers claim slot buckets with one
    // `fetch_add`, each bound for the run to its own router worker file, and
    // stage 4 consumes the router concurrently. Two error stores serve two
    // recipients: `s3_error` keeps this stage's cause for its caller, however
    // the handles join, and the router's abort error (raised here, and by
    // `AbortOnDrop` on a panic) wakes stage 4's readiness waits. A stage 4
    // failure aborts the router and stops these workers without becoming a
    // stage 3 error, and `external_join` prefers a stage 3 error when both
    // fail; so "first error" means first recorded, not lowest bucket, and the
    // two stores can name different concurrent causes. Work in flight
    // finishes after another worker fails.
    //
    // No `*_wait_ns` stall counter: nothing here blocks on another thread.
    // The claim is a `fetch_add`, the bucket reads and payload `pwrite`s go to
    // per-bucket and per-worker files, and the first-error slot is locked once
    // per bucket, held for a load. Publishing to the router takes its locks
    // (a per-blob slot, the straddler map, its stats and notify gate) only
    // around a store, a counter bump or a `notify_all`, never across I/O or
    // encoding. The blocking side of the router is stage 4's readiness wait,
    // recorded on stage 4's side (`s4_readiness_wait_ns` for genuine parks,
    // the older ungated `s4_readiness_wait_ms`).
    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let counters = Stage3Counters::new();
    let s3_error: std::sync::Mutex<Option<Error>> = std::sync::Mutex::new(None);

    let next_ref = &next_idx;
    let c = &counters;
    let err_ref = &s3_error;
    let entry_counts = &slot_buckets.entry_counts;
    let paths = &slot_buckets.paths;

    let ctx = integrated;
    let router = ctx.router;

    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(num_workers);
        for worker_id in 0..num_workers {
            let handle = scope.spawn(move || -> crate::Result<()> {
                use std::sync::atomic::Ordering::Relaxed;

                // Panic-safety: if this closure unwinds before `disarm`,
                // the Drop impl calls `router.abort(...)`, waking every
                // stage-4 wait_ready caller. Without this, a panicking
                // worker would leave stage 4 blocked indefinitely.
                let abort_guard = AbortOnDrop::new(router, "stage 3 worker");

                let mut data_buf: Vec<u8> = Vec::new();
                let mut scatter_buf: Vec<u8> = Vec::new();
                let mut encode_scratch: Vec<u8> = Vec::new();
                let mut tmp_byte_pos: u64 = 0;

                let tmp_file = router.worker_file(worker_id);

                loop {
                    if router.is_aborted()
                        || err_ref
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .is_some()
                    {
                        break;
                    }
                    let bucket_idx = next_ref.fetch_add(1, Relaxed);
                    if bucket_idx >= slot_bucket_count { break; }

                    // Cancellation, once per bucket, leaves through the same
                    // path as a bucket failure: record the cause first, then
                    // abort the router (waking stage 4's `wait_ready`).
                    // There is no check inside a bucket: after the token
                    // trips each worker finishes the bucket in hand, so the
                    // stop latency is that bucket's work, which depends on
                    // its population (buckets can be skewed, and there may
                    // be fewer than 256). A check inside would be per element
                    // in the hot loop; if a stop proves too slow, split the
                    // bucket and check per chunk, with a matched A/B.
                    if let Err(e) = cancel.check() {
                        err_ref
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .get_or_insert_with(|| e.clone());
                        router.abort(e.clone());
                        abort_guard.disarm();
                        return Err(e);
                    }

                    // Test-only: hold this bucket, before any of its work,
                    // until stage 4 aborts the router (bounded).
                    #[cfg(feature = "test-hooks")]
                    cancel
                        .faults()
                        .on_altw_stage3_bucket(bucket_idx, || router.is_aborted());

                    // Test-only: the fault plan's
                    // `altw_stage3_panic_at_bucket`. Simulates a mid-phase
                    // stage 3 worker crash. The surrounding `AbortOnDrop`
                    // guard calls `router.abort(...)` on unwind, waking
                    // stage 4 waiters; `ScratchDir::drop` cleans up the
                    // scratch tree. Release builds compile this out
                    // entirely.
                    #[cfg(feature = "test-hooks")]
                    if cancel.faults().altw_stage3_panic_at_bucket.load(Relaxed) == bucket_idx {
                        panic!(
                            "test-hooks: altw stage 3 worker {worker_id} panicking at bucket {bucket_idx}"
                        );
                    }

                    let bucket_start = bucket_idx as u64 * range_size;
                    let bucket_end = if bucket_idx == slot_bucket_count - 1 {
                        total_slots
                    } else {
                        ((bucket_idx as u64 + 1) * range_size).min(total_slots)
                    };
                    let bucket_slots = bucket_end - bucket_start;

                    if entry_counts[bucket_idx] == 0 {
                        // No resolved entries landed in this bucket, but blobs
                        // can still overlap its slot range (all-zero coords).
                        // Classify intersections and emit matching zero-coord
                        // bytes so the FullyContained and straddler paths stay
                        // consistent with what the per-blob coord slice looked
                        // like under the pre-integration coord_slots reader.
                        let result: crate::Result<()> = (|| {
                            let intersections = classify_blobs_in_bucket(
                                bucket_start, bucket_end,
                                ctx.way_slot_starts, total_slots,
                            ).map_err(|e| e.context(format!("classify bucket {bucket_idx}")))?;
                            let bucket_bytes = bucket_slots as usize * COORD_SLOT_SIZE;
                            scatter_buf.clear();
                            scatter_buf.resize(bucket_bytes, 0);
                            emit_integrated_intersections(
                                &intersections, &scatter_buf, bucket_start,
                                total_slots, ctx, &mut encode_scratch,
                                #[allow(clippy::cast_possible_truncation)]
                                { worker_id as u32 },
                                &mut tmp_byte_pos, tmp_file, c,
                            )?;
                            Ok(())
                        })();
                        if let Err(e) = result {
                            err_ref
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .get_or_insert_with(|| e.clone());
                            router.abort(e.clone().context(format!("stage 3 worker {worker_id} error")));
                            abort_guard.disarm();
                            return Err(e);
                        }
                        continue;
                    }

                    let result: crate::Result<()> = (|| {
                        let bucket_bytes = bucket_slots as usize * COORD_SLOT_SIZE;
                        scatter_buf.clear();
                        scatter_buf.resize(bucket_bytes, 0);

                        let t_open = crate::counters::Stopwatch::start();
                        data_buf.clear();
                        let bucket_file = crate::read::input::open(&paths[bucket_idx])?;
                        t_open.record_ms(&c.open_ms);

                        let t_read = crate::counters::Stopwatch::start();
                        std::io::Read::read_to_end(&mut &bucket_file, &mut data_buf).map_err(
                            |e| {
                                Error::from(e).context(format!(
                                    "failed to read slot bucket {}",
                                    paths[bucket_idx].display()
                                ))
                            },
                        )?;
                        if advise_dontneed_file(&bucket_file, cancel) {
                            c.fadvise_calls.fetch_add(1, Relaxed);
                            c.fadvise_bytes.fetch_add(data_buf.len() as u64, Relaxed);
                        }
                        t_read.record_ms(&c.read_ms);
                        c.bytes_read.fetch_add(data_buf.len() as u64, Relaxed);

                        // Direct scatter from raw bytes. This removes the
                        // bucket-local Vec<ResolvedEntry> materialization and
                        // its extra memory traffic; parsing happens in lockstep
                        // with the scatter stores.
                        let t_scatter = crate::counters::Stopwatch::start();
                        let stores =
                            scatter_bucket_entries(&data_buf, bucket_idx, &mut scatter_buf)?;
                        t_scatter.record_ms(&c.scatter_ms);
                        c.scatter_stores.fetch_add(stores, Relaxed);

                        // Track max live buffer bytes for this worker.
                        {
                            let worker_bytes = data_buf.capacity() as u64
                                + scatter_buf.capacity() as u64;
                            crate::counters::record_max(&c.max_worker_buf_bytes, worker_bytes);
                        }

                        c.buckets_loaded.fetch_add(1, Relaxed);

                        // Classify blobs in this bucket and encode/stage them
                        // into worker temp files + straddler staging.
                        let intersections = classify_blobs_in_bucket(
                            bucket_start, bucket_end,
                            ctx.way_slot_starts, total_slots,
                        ).map_err(|e| e.context(format!("classify bucket {bucket_idx}")))?;
                        emit_integrated_intersections(
                            &intersections, &scatter_buf, bucket_start,
                            total_slots, ctx, &mut encode_scratch,
                            #[allow(clippy::cast_possible_truncation)]
                            { worker_id as u32 },
                            &mut tmp_byte_pos, tmp_file, c,
                        )?;

                        Ok(())
                    })();

                    if let Err(e) = result {
                        err_ref
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .get_or_insert_with(|| e.clone());
                        router.abort(e.clone().context(format!("stage 3 worker {worker_id} error")));
                        abort_guard.disarm();
                        return Err(e);
                    }
                }

                abort_guard.disarm();
                Ok(())
            });
            handles.push(handle);
        }

        // Propagate first worker error or panic. No manifests to collect -
        // each worker published directly to the router.
        let mut first_err: Option<Error> = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(payload) => {
                    if first_err.is_none() {
                        first_err =
                            Some(crate::error::panic_error("worker thread", payload.as_ref()));
                    }
                }
            }
        }
        // Only fills an empty slot: a cause a worker recorded before it woke
        // its peers outranks whatever the join loop meets first in handle
        // order (a panic is the one cause no worker records itself).
        if let Some(e) = first_err {
            s3_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_or_insert(e);
        }
    });

    if let Some(e) = s3_error.into_inner().unwrap_or(None) {
        return Err(e);
    }

    counters.emit_all(cancel);

    Ok(())
}

/// Emit integrated intersections for one bucket. Fully-contained blobs
/// are encoded, pwritten to this worker's tmp file, and published to the
/// router as `BlobLocation::Worker`. Straddler halves are published via
/// `router.publish_straddler_half`; the worker that lands the second
/// half also encodes and transitions the slot to `Straddler`.
#[hotpath::measure]
#[allow(clippy::too_many_arguments, clippy::cast_possible_truncation)]
fn emit_integrated_intersections(
    intersections: &[BlobBucketIntersection],
    scatter_buf: &[u8],
    bucket_start: u64,
    total_slots: u64,
    ctx: &IntegratedInputs<'_>,
    encode_scratch: &mut Vec<u8>,
    worker_id: u32,
    tmp_byte_pos: &mut u64,
    tmp_file: &std::fs::File,
    counters: &Stage3Counters,
) -> crate::Result<()> {
    use std::os::unix::fs::FileExt as _;
    use std::sync::atomic::Ordering::Relaxed;

    let bucket_bytes = scatter_buf.len();
    let router = ctx.router;

    for intersection in intersections {
        match intersection {
            BlobBucketIntersection::FullyContained { blob_idx } => {
                let blob_idx = *blob_idx;
                let blob_start = ctx.way_slot_starts[blob_idx];
                let blob_end = ctx
                    .way_slot_starts
                    .get(blob_idx + 1)
                    .copied()
                    .unwrap_or(total_slots);
                #[allow(clippy::cast_possible_truncation)]
                let local_start = ((blob_start - bucket_start) as usize) * COORD_SLOT_SIZE;
                #[allow(clippy::cast_possible_truncation)]
                let local_end = ((blob_end - bucket_start) as usize) * COORD_SLOT_SIZE;
                let slice = &scatter_buf[local_start..local_end];

                let t_enc = crate::counters::Stopwatch::start();
                encode_scratch.clear();
                encode_blob_payload_from_record(
                    slice,
                    ctx.per_way_rcs.blob_record(blob_idx),
                    blob_idx,
                    ctx.inject_prepass,
                    encode_scratch,
                )
                .map_err(|e| e.context(format!("encode blob {blob_idx}")))?;
                t_enc.record_ms(&counters.encode_ms);

                let byte_offset = *tmp_byte_pos;
                let byte_length = encode_scratch.len() as u64;
                let t_pw = crate::counters::Stopwatch::start();
                tmp_file
                    .write_all_at(encode_scratch, byte_offset)
                    .map_err(|e| {
                        Error::from(e).context(format!(
                            "failed to write way blob {blob_idx}'s payload to {}",
                            router.worker_path(worker_id as usize).display()
                        ))
                    })?;
                t_pw.record_ns(&counters.tmp_pwrite_ns);
                counters.tmp_pwrite_calls.fetch_add(1, Relaxed);
                *tmp_byte_pos += byte_length;
                counters.worker_tmp_bytes.fetch_add(byte_length, Relaxed);
                // Publish immediately: the pwrite has durably landed the
                // bytes in the kernel page cache at [byte_offset, +byte_length),
                // so stage 4's pread via the router's worker_files[worker_id]
                // will see them.
                router
                    .publish_worker(blob_idx, worker_id, byte_offset, byte_length)
                    .map_err(|e| e.context(format!("publish blob {blob_idx}")))?;
            }
            BlobBucketIntersection::LeftHalf { blob_idx } => {
                let blob_idx = *blob_idx;
                let blob_start = ctx.way_slot_starts[blob_idx];
                #[allow(clippy::cast_possible_truncation)]
                let local_start = ((blob_start - bucket_start) as usize) * COORD_SLOT_SIZE;
                let slice = &scatter_buf[local_start..bucket_bytes];

                let t_copy = crate::counters::Stopwatch::start();
                let bytes = slice.to_vec();
                t_copy.record_ms(&counters.straddler_copy_ms);
                router
                    .publish_straddler_half(
                        blob_idx,
                        StraddlerSide::Left,
                        bytes,
                        ctx.per_way_rcs,
                        ctx.inject_prepass,
                        encode_scratch,
                    )
                    .map_err(|e| e.context(format!("straddler left blob {blob_idx}")))?;
            }
            BlobBucketIntersection::RightHalf { blob_idx } => {
                let blob_idx = *blob_idx;
                let blob_end = ctx
                    .way_slot_starts
                    .get(blob_idx + 1)
                    .copied()
                    .unwrap_or(total_slots);
                #[allow(clippy::cast_possible_truncation)]
                let local_end = ((blob_end - bucket_start) as usize) * COORD_SLOT_SIZE;
                let slice = &scatter_buf[..local_end];

                let t_copy = crate::counters::Stopwatch::start();
                let bytes = slice.to_vec();
                t_copy.record_ms(&counters.straddler_copy_ms);
                router
                    .publish_straddler_half(
                        blob_idx,
                        StraddlerSide::Right,
                        bytes,
                        ctx.per_way_rcs,
                        ctx.inject_prepass,
                        encode_scratch,
                    )
                    .map_err(|e| e.context(format!("straddler right blob {blob_idx}")))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{IntegratedInputs, SlotBucketRef, stage3_slot_reorder};
    use crate::commands::altw::external::coord_payloads::{
        ConcurrentBlobLocationRouter, parse_per_way_refcount_sidecar_bytes,
    };
    use std::sync::Arc;

    #[test]
    fn empty_bucket_error_aborts_router_with_the_actual_error() {
        let per_way_rcs = parse_per_way_refcount_sidecar_bytes(&[1, 1], 1).expect("sidecar");
        let temp = crate::path_guard::tests::scratch();
        let worker_path = temp.path().join("worker");
        let worker_file = std::fs::File::create(&worker_path).expect("worker file");
        let worker = crate::read::input::InputFile::from_parts(
            Arc::new(worker_file),
            Arc::from(worker_path.as_path()),
        );
        let router = ConcurrentBlobLocationRouter::new(
            &per_way_rcs,
            vec![worker],
            &crate::host::Call::detached(),
        )
        .expect("router");
        let way_slot_starts = [0];
        let integrated = IntegratedInputs {
            way_slot_starts: &way_slot_starts,
            per_way_rcs: &per_way_rcs,
            router: &router,
            inject_prepass: false,
        };
        let slot_buckets = SlotBucketRef {
            paths: Vec::new(),
            entry_counts: vec![0, 0],
        };

        let err = stage3_slot_reorder(
            &slot_buckets,
            2,
            100,
            &integrated,
            1,
            &crate::host::Call::detached(),
        )
        .expect_err("oversized blob must fail classification");
        assert!(err.to_string().contains("blob 0 slot range"));

        let router_err = router.wait_ready(0).expect_err("router must be aborted");
        assert!(router_err.to_string().contains("stage 3 worker 0 error"));
        assert!(router_err.to_string().contains("blob 0 slot range"));
        assert!(!router_err.to_string().contains("panicked"));
    }
}
