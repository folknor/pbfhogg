//! Stage 3: Slot reorder — emit per-blob coord_payloads via the integrated
//! pipeline (the flat coord_slots intermediate was retired 2026-04).

#[cfg(feature = "linux-direct-io")]
use super::super::external_radix::advise_dontneed_file;
use super::super::Result;
use super::coord_payloads::{
    encode_blob_payload_from_record, ManifestEntry, PerWayRcs, StraddlerSlot,
};
use super::blob_bucket_index::{classify_blobs_in_bucket, BlobBucketIntersection};
use super::{RESOLVED_ENTRY_SIZE, COORD_SLOT_SIZE};

/// Lightweight reference to slot bucket paths + counts for stage 3.
pub(super) struct SlotBucketRef {
    pub(super) paths: Vec<std::path::PathBuf>,
    pub(super) entry_counts: Vec<u64>,
}

/// Inputs needed for the integrated dual-emit path.
pub(super) struct IntegratedInputs<'a> {
    pub way_slot_starts: &'a [u64],
    pub per_way_rcs: &'a PerWayRcs,
    pub worker_tmp_paths: &'a [std::path::PathBuf],
    pub straddler_slots: &'a [std::sync::Mutex<Option<StraddlerSlot>>],
}

/// Manifests produced by stage 3 workers in the integrated path.
pub(super) struct IntegratedResult {
    pub worker_manifests: Vec<Vec<ManifestEntry>>,
}

/// Left or right half of a straddler blob.
enum HalfSide {
    Left,
    Right,
}

/// Update the straddler staging for `blob_idx` with one raw-byte half.
fn merge_straddler(
    straddler_slots: &[std::sync::Mutex<Option<StraddlerSlot>>],
    blob_idx: usize,
    side: HalfSide,
    bytes: Vec<u8>,
) -> std::result::Result<(), String> {
    let mut guard = straddler_slots[blob_idx]
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let new = match (guard.take(), side) {
        (None, HalfSide::Left) => StraddlerSlot::Left(bytes),
        (None, HalfSide::Right) => StraddlerSlot::Right(bytes),
        (Some(StraddlerSlot::Left(left)), HalfSide::Right) => {
            StraddlerSlot::Both { left, right: bytes }
        }
        (Some(StraddlerSlot::Right(right)), HalfSide::Left) => {
            StraddlerSlot::Both { left: bytes, right }
        }
        (Some(StraddlerSlot::Left(_)), HalfSide::Left) => {
            return Err(format!("blob {blob_idx}: duplicate left half"));
        }
        (Some(StraddlerSlot::Right(_)), HalfSide::Right) => {
            return Err(format!("blob {blob_idx}: duplicate right half"));
        }
        (Some(StraddlerSlot::Both { .. }), _) => {
            return Err(format!("blob {blob_idx}: third straddler half impossible"));
        }
    };
    *guard = Some(new);
    Ok(())
}

pub(super) fn scatter_bucket_entries(
    data_buf: &[u8],
    bucket_idx: usize,
    bucket_start: u64,
    bucket_end: u64,
    scatter_buf: &mut [u8],
) -> std::result::Result<u64, String> {
    if data_buf.len() % RESOLVED_ENTRY_SIZE != 0 {
        return Err(format!(
            "slot bucket {bucket_idx} has {} trailing bytes (entry size {})",
            data_buf.len() % RESOLVED_ENTRY_SIZE,
            RESOLVED_ENTRY_SIZE,
        ));
    }

    let bucket_slots = scatter_buf.len() / COORD_SLOT_SIZE;
    let mut stores: u64 = 0;
    for chunk in data_buf.chunks_exact(RESOLVED_ENTRY_SIZE) {
        let slot_pos = u64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3],
            chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
        if slot_pos < bucket_start || slot_pos >= bucket_end {
            return Err(format!(
                "slot bucket {bucket_idx} contains slot_pos {slot_pos} outside [{bucket_start}, {bucket_end})"
            ));
        }
        #[allow(clippy::cast_possible_truncation)]
        let local_pos = (slot_pos - bucket_start) as usize;
        if local_pos >= bucket_slots {
            return Err(format!(
                "slot bucket {bucket_idx} local_pos {local_pos} outside bucket slot span {bucket_slots}"
            ));
        }
        let offset = local_pos * COORD_SLOT_SIZE;
        scatter_buf[offset..offset + 4].copy_from_slice(&chunk[8..12]);
        scatter_buf[offset + 4..offset + 8].copy_from_slice(&chunk[12..16]);
        stores += 1;
    }

    Ok(stores)
}

/// Pre-allocates the output file to `total_slots * 8` bytes (zero-filled
/// by the OS). Empty buckets need no explicit zero-write — the file is
/// already zeroed.
#[hotpath::measure]
#[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
pub(super) fn stage3_slot_reorder(
    slot_buckets: &SlotBucketRef,
    slot_bucket_count: usize,
    total_slots: u64,
    integrated: IntegratedInputs<'_>,
) -> Result<IntegratedResult> {
    // Floor division: every bucket is `range_size` slots wide except
    // the LAST, which extends to `total_slots` and may be wider. This
    // keeps the smallest-bucket-width = range_size, which the caller
    // sized so that range_size ≥ max_blob_slots — preserving the
    // 2-piece straddler invariant for small inputs. (See
    // `ResolvedEntry::slot_bucket` for the matching routing logic.)
    let range_size = total_slots / slot_bucket_count as u64;

    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4)
        .min(6);

    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let s3_open_ms = std::sync::atomic::AtomicU64::new(0);
    let s3_read_ms = std::sync::atomic::AtomicU64::new(0);
    let s3_parse_ms = std::sync::atomic::AtomicU64::new(0);
    let s3_scatter_ms = std::sync::atomic::AtomicU64::new(0);
    let s3_buckets_loaded = std::sync::atomic::AtomicU64::new(0);
    let s3_bytes_read = std::sync::atomic::AtomicU64::new(0);
    let s3_scatter_stores = std::sync::atomic::AtomicU64::new(0);
    let s3_max_worker_buf_bytes = std::sync::atomic::AtomicU64::new(0);
    let s3_fadvise_calls = std::sync::atomic::AtomicU64::new(0);
    let s3_fadvise_bytes = std::sync::atomic::AtomicU64::new(0);
    let s3_error: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

    // Integrated path counters.
    let s3_integ_encode_ms = std::sync::atomic::AtomicU64::new(0);
    let s3_integ_straddler_copy_ms = std::sync::atomic::AtomicU64::new(0);
    let s3_integ_worker_tmp_bytes = std::sync::atomic::AtomicU64::new(0);

    let next_ref = &next_idx;
    let s3_open_ref = &s3_open_ms;
    let s3_read_ref = &s3_read_ms;
    let s3_scatter_ref = &s3_scatter_ms;
    let s3_loaded_ref = &s3_buckets_loaded;
    let s3_bytes_read_ref = &s3_bytes_read;
    let s3_scatter_stores_ref = &s3_scatter_stores;
    let s3_max_worker_buf_ref = &s3_max_worker_buf_bytes;
    let s3_fadvise_calls_ref = &s3_fadvise_calls;
    let s3_fadvise_bytes_ref = &s3_fadvise_bytes;
    let err_ref = &s3_error;
    let entry_counts = &slot_buckets.entry_counts;
    let paths = &slot_buckets.paths;
    let s3_integ_encode_ref = &s3_integ_encode_ms;
    let s3_integ_straddler_copy_ref = &s3_integ_straddler_copy_ms;
    let s3_integ_worker_tmp_bytes_ref = &s3_integ_worker_tmp_bytes;

    let ctx = &integrated;

    let worker_manifests: Vec<Vec<ManifestEntry>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(num_workers);
        for worker_id in 0..num_workers {
            let handle = scope.spawn(move || -> std::result::Result<Vec<ManifestEntry>, String> {
                use std::io::Write as _;
                use std::sync::atomic::Ordering::Relaxed;

                let mut data_buf: Vec<u8> = Vec::new();
                let mut scatter_buf: Vec<u8> = Vec::new();
                let mut encode_scratch: Vec<u8> = Vec::new();
                let mut manifest: Vec<ManifestEntry> = Vec::new();
                let mut tmp_byte_pos: u64 = 0;

                let tmp_file = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&ctx.worker_tmp_paths[worker_id])
                    .map_err(|e| format!("create worker tmp {worker_id}: {e}"))?;
                let mut tmp_writer = std::io::BufWriter::with_capacity(512 * 1024, tmp_file);

                loop {
                    if err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some() {
                        break;
                    }
                    let bucket_idx = next_ref.fetch_add(1, Relaxed);
                    if bucket_idx >= slot_bucket_count { break; }

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
                        let result: std::result::Result<(), String> = (|| {
                            let intersections = classify_blobs_in_bucket(
                                bucket_start, bucket_end,
                                ctx.way_slot_starts, total_slots,
                            ).map_err(|e| format!("classify bucket {bucket_idx}: {e}"))?;
                            let bucket_bytes = bucket_slots as usize * COORD_SLOT_SIZE;
                            scatter_buf.clear();
                            scatter_buf.resize(bucket_bytes, 0);
                            emit_integrated_intersections(
                                &intersections, &scatter_buf, bucket_start,
                                total_slots, ctx, &mut encode_scratch, &mut manifest,
                                &mut tmp_byte_pos, &mut tmp_writer,
                                s3_integ_encode_ref, s3_integ_straddler_copy_ref,
                                s3_integ_worker_tmp_bytes_ref,
                            )?;
                            Ok(())
                        })();
                        if let Err(e) = result {
                            *err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e.clone());
                            return Err(e);
                        }
                        continue;
                    }

                    let result: std::result::Result<(), String> = (|| {
                        let bucket_bytes = bucket_slots as usize * COORD_SLOT_SIZE;
                        scatter_buf.clear();
                        scatter_buf.resize(bucket_bytes, 0);

                        let t_open = std::time::Instant::now();
                        data_buf.clear();
                        let bucket_file = std::fs::File::open(&paths[bucket_idx])
                            .map_err(|e| format!("open slot bucket: {e}"))?;
                        #[allow(clippy::cast_possible_truncation)]
                        s3_open_ref.fetch_add(t_open.elapsed().as_millis() as u64, Relaxed);

                        let t_read = std::time::Instant::now();
                        std::io::Read::read_to_end(&mut &bucket_file, &mut data_buf)
                            .map_err(|e| format!("read slot bucket: {e}"))?;
                        #[cfg(feature = "linux-direct-io")]
                        {
                            s3_fadvise_calls_ref.fetch_add(1, Relaxed);
                            s3_fadvise_bytes_ref.fetch_add(data_buf.len() as u64, Relaxed);
                            advise_dontneed_file(&bucket_file);
                        }
                        #[allow(clippy::cast_possible_truncation)]
                        s3_read_ref.fetch_add(t_read.elapsed().as_millis() as u64, Relaxed);
                        s3_bytes_read_ref.fetch_add(data_buf.len() as u64, Relaxed);

                        // Direct scatter from raw bytes. This removes the
                        // bucket-local Vec<ResolvedEntry> materialization and
                        // its extra memory traffic; parsing happens in lockstep
                        // with the scatter stores.
                        let t_scatter = std::time::Instant::now();
                        let stores = scatter_bucket_entries(
                            &data_buf,
                            bucket_idx,
                            bucket_start,
                            bucket_end,
                            &mut scatter_buf,
                        )?;
                        #[allow(clippy::cast_possible_truncation)]
                        s3_scatter_ref.fetch_add(t_scatter.elapsed().as_millis() as u64, Relaxed);
                        s3_scatter_stores_ref.fetch_add(stores, Relaxed);

                        // Track max live buffer bytes for this worker.
                        {
                            let worker_bytes = data_buf.capacity() as u64
                                + scatter_buf.capacity() as u64;
                            let mut current = s3_max_worker_buf_ref.load(Relaxed);
                            while worker_bytes > current {
                                match s3_max_worker_buf_ref.compare_exchange_weak(
                                    current, worker_bytes, Relaxed, Relaxed,
                                ) {
                                    Ok(_) => break,
                                    Err(actual) => current = actual,
                                }
                            }
                        }

                        s3_loaded_ref.fetch_add(1, Relaxed);

                        // Classify blobs in this bucket and encode/stage them
                        // into worker temp files + straddler staging.
                        let intersections = classify_blobs_in_bucket(
                            bucket_start, bucket_end,
                            ctx.way_slot_starts, total_slots,
                        ).map_err(|e| format!("classify bucket {bucket_idx}: {e}"))?;
                        emit_integrated_intersections(
                            &intersections, &scatter_buf, bucket_start,
                            total_slots, ctx, &mut encode_scratch, &mut manifest,
                            &mut tmp_byte_pos, &mut tmp_writer,
                            s3_integ_encode_ref, s3_integ_straddler_copy_ref,
                            s3_integ_worker_tmp_bytes_ref,
                        )?;

                        Ok(())
                    })();

                    if let Err(e) = result {
                        *err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e.clone());
                        return Err(e);
                    }
                }

                tmp_writer.flush().map_err(|e| format!("flush worker {worker_id} tmp: {e}"))?;

                Ok(manifest)
            });
            handles.push(handle);
        }

        // Collect all manifests; if any worker returned Err or panicked,
        // propagate the first such failure as a normal command error (do not
        // abort the process).
        let mut all_manifests: Vec<Vec<ManifestEntry>> = Vec::with_capacity(num_workers);
        let mut first_err: Option<String> = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(m)) => all_manifests.push(m),
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                    all_manifests.push(Vec::new());
                }
                Err(payload) => {
                    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                        format!("worker thread panicked: {s}")
                    } else if let Some(s) = payload.downcast_ref::<String>() {
                        format!("worker thread panicked: {s}")
                    } else {
                        "worker thread panicked (unknown payload)".to_string()
                    };
                    if first_err.is_none() {
                        first_err = Some(msg);
                    }
                    all_manifests.push(Vec::new());
                }
            }
        }
        if let Some(e) = first_err {
            // Overwrite shared error so the outer check picks it up.
            *s3_error.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e);
        }
        all_manifests
    });

    if let Some(e) = s3_error.into_inner().unwrap_or(None) {
        return Err(e.into());
    }

    #[allow(clippy::cast_possible_wrap)]
    {
        use std::sync::atomic::Ordering::Relaxed;
        crate::debug::emit_counter("s3_open_ms", s3_open_ms.load(Relaxed) as i64);
        crate::debug::emit_counter("s3_read_ms", s3_read_ms.load(Relaxed) as i64);
        crate::debug::emit_counter("s3_parse_ms", s3_parse_ms.load(Relaxed) as i64);
        crate::debug::emit_counter("s3_scatter_ms", s3_scatter_ms.load(Relaxed) as i64);
        crate::debug::emit_counter("s3_buckets_loaded", s3_buckets_loaded.load(Relaxed) as i64);
        crate::debug::emit_counter("s3_bytes_read", s3_bytes_read.load(Relaxed) as i64);
        crate::debug::emit_counter("s3_scatter_stores", s3_scatter_stores.load(Relaxed) as i64);
        crate::debug::emit_counter("s3_max_worker_buf_bytes", s3_max_worker_buf_bytes.load(Relaxed) as i64);
        crate::debug::emit_counter("s3_fadvise_calls", s3_fadvise_calls.load(Relaxed) as i64);
        crate::debug::emit_counter("s3_fadvise_bytes", s3_fadvise_bytes.load(Relaxed) as i64);
        crate::debug::emit_counter("s3_encode_ms", s3_integ_encode_ms.load(Relaxed) as i64);
        crate::debug::emit_counter("s3_straddler_copy_ms", s3_integ_straddler_copy_ms.load(Relaxed) as i64);
        crate::debug::emit_counter("s3_worker_tmp_bytes", s3_integ_worker_tmp_bytes.load(Relaxed) as i64);
    }

    Ok(IntegratedResult { worker_manifests })
}

/// Emit integrated intersections for one bucket into the worker's temp file
/// and straddler staging.
#[allow(clippy::too_many_arguments, clippy::cast_possible_truncation)]
pub(super) fn emit_integrated_intersections(
    intersections: &[BlobBucketIntersection],
    scatter_buf: &[u8],
    bucket_start: u64,
    total_slots: u64,
    ctx: &IntegratedInputs<'_>,
    encode_scratch: &mut Vec<u8>,
    manifest: &mut Vec<ManifestEntry>,
    tmp_byte_pos: &mut u64,
    tmp_writer: &mut std::io::BufWriter<std::fs::File>,
    s3_integ_encode_ref: &std::sync::atomic::AtomicU64,
    s3_integ_straddler_copy_ref: &std::sync::atomic::AtomicU64,
    s3_integ_worker_tmp_bytes_ref: &std::sync::atomic::AtomicU64,
) -> std::result::Result<(), String> {
    use std::io::Write as _;
    use std::sync::atomic::Ordering::Relaxed;

    let bucket_bytes = scatter_buf.len();
    let writer = tmp_writer;

    for intersection in intersections {
        match intersection {
            BlobBucketIntersection::FullyContained { blob_idx } => {
                let blob_idx = *blob_idx;
                let blob_start = ctx.way_slot_starts[blob_idx];
                let blob_end = ctx.way_slot_starts.get(blob_idx + 1).copied().unwrap_or(total_slots);
                #[allow(clippy::cast_possible_truncation)]
                let local_start = ((blob_start - bucket_start) as usize) * COORD_SLOT_SIZE;
                #[allow(clippy::cast_possible_truncation)]
                let local_end = ((blob_end - bucket_start) as usize) * COORD_SLOT_SIZE;
                let slice = &scatter_buf[local_start..local_end];

                let t_enc = std::time::Instant::now();
                encode_scratch.clear();
                encode_blob_payload_from_record(
                    slice,
                    ctx.per_way_rcs.blob_record(blob_idx),
                    blob_idx,
                    encode_scratch,
                )
                    .map_err(|e| format!("encode blob {blob_idx}: {e}"))?;
                #[allow(clippy::cast_possible_truncation)]
                s3_integ_encode_ref.fetch_add(t_enc.elapsed().as_millis() as u64, Relaxed);

                let byte_offset = *tmp_byte_pos;
                let byte_length = encode_scratch.len() as u64;
                writer
                    .write_all(encode_scratch)
                    .map_err(|e| format!("write worker tmp blob {blob_idx}: {e}"))?;
                *tmp_byte_pos += byte_length;
                s3_integ_worker_tmp_bytes_ref.fetch_add(byte_length, Relaxed);
                manifest.push(ManifestEntry {
                    blob_idx: blob_idx as u32,
                    byte_offset,
                    byte_length,
                });
            }
            BlobBucketIntersection::LeftHalf { blob_idx } => {
                let blob_idx = *blob_idx;
                let blob_start = ctx.way_slot_starts[blob_idx];
                #[allow(clippy::cast_possible_truncation)]
                let local_start = ((blob_start - bucket_start) as usize) * COORD_SLOT_SIZE;
                let slice = &scatter_buf[local_start..bucket_bytes];

                let t_copy = std::time::Instant::now();
                let bytes = slice.to_vec();
                #[allow(clippy::cast_possible_truncation)]
                s3_integ_straddler_copy_ref.fetch_add(t_copy.elapsed().as_millis() as u64, Relaxed);
                merge_straddler(ctx.straddler_slots, blob_idx, HalfSide::Left, bytes)?;
            }
            BlobBucketIntersection::RightHalf { blob_idx } => {
                let blob_idx = *blob_idx;
                let blob_end = ctx.way_slot_starts.get(blob_idx + 1).copied().unwrap_or(total_slots);
                #[allow(clippy::cast_possible_truncation)]
                let local_end = ((blob_end - bucket_start) as usize) * COORD_SLOT_SIZE;
                let slice = &scatter_buf[..local_end];

                let t_copy = std::time::Instant::now();
                let bytes = slice.to_vec();
                #[allow(clippy::cast_possible_truncation)]
                s3_integ_straddler_copy_ref.fetch_add(t_copy.elapsed().as_millis() as u64, Relaxed);
                merge_straddler(ctx.straddler_slots, blob_idx, HalfSide::Right, bytes)?;
            }
        }
    }
    Ok(())
}
