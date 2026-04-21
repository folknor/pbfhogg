//! Stage 3: blob-group emit. Reads the 256 blob-group files produced
//! by stage 2, scatters each entry into a per-group coord buffer
//! indexed by blob-local slot offset, and emits one `publish_worker`
//! per non-zero-ref blob in the group's blob-index range.
//!
//! Post-#6 ownership model: every blob lives in exactly one group by
//! construction (groups are contiguous runs of blob-index chosen to
//! balance cumulative slot count). There are no straddlers. The
//! old `classify_blobs_in_bucket` + left/right-half machinery is
//! deleted along with `blob_bucket_index.rs`.
//!
//! Zero-ref blobs in a group stay as `BlobLocation::Empty`
//! (pre-populated by `ConcurrentBlobLocationRouter::new` from
//! `PerWayRcs`); this stage never publishes them. Non-zero-ref
//! blobs get a `publish_worker` payload - even a blob whose group
//! file somehow contains no entries would still emit an all-zero
//! coord payload (stage 4 treats `(0, 0)` as the missing-location
//! sentinel).

use std::io::{Read as _, BufReader};

use super::super::Result;
use super::coord_payloads::{
    encode_blob_payload_from_record, AbortOnDrop, ConcurrentBlobLocationRouter, PerWayRcs,
};
use super::{BlobGroupMap, RESOLVED_ENTRY_SIZE, COORD_SLOT_SIZE};

/// Lightweight reference to blob-group paths + per-group entry counts.
pub(super) struct BlobGroupRef {
    pub(super) paths: Vec<std::path::PathBuf>,
    pub(super) entry_counts: Vec<u64>,
}

/// Inputs needed for the blob-group emit stage. Stage 3 publishes
/// directly to the router (the streaming shape from #2); no manifests
/// are returned.
pub(super) struct IntegratedInputs<'a> {
    pub way_slot_starts: &'a [u64],
    pub per_way_rcs: &'a PerWayRcs,
    pub router: &'a ConcurrentBlobLocationRouter,
    pub blob_group_map: &'a BlobGroupMap,
}

#[hotpath::measure]
#[allow(clippy::too_many_lines)]
pub(super) fn stage3_blob_group_emit(
    blob_groups: &BlobGroupRef,
    blob_group_count: usize,
    total_slots: u64,
    num_workers: usize,
    integrated: &IntegratedInputs<'_>,
) -> Result<()> {
    // Worker count is decided at the external_join level in mod.rs so
    // the router's `worker_files` table and this stage 3 workers count
    // stay in lockstep. Do not compute it here.

    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let s3_read_ms = std::sync::atomic::AtomicU64::new(0);
    let s3_scatter_ms = std::sync::atomic::AtomicU64::new(0);
    let s3_encode_ms = std::sync::atomic::AtomicU64::new(0);
    let s3_bytes_read = std::sync::atomic::AtomicU64::new(0);
    let s3_bytes_written = std::sync::atomic::AtomicU64::new(0);
    let s3_entries_emitted = std::sync::atomic::AtomicU64::new(0);
    let s3_groups_processed = std::sync::atomic::AtomicU64::new(0);
    let s3_blobs_published = std::sync::atomic::AtomicU64::new(0);
    let s3_blobs_zero_entry_nonzero_ref = std::sync::atomic::AtomicU64::new(0);
    let s3_max_group_coord_bytes = std::sync::atomic::AtomicU64::new(0);
    let s3_error: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

    let ctx = integrated;
    let router = ctx.router;
    let way_slot_starts = ctx.way_slot_starts;
    let per_way_rcs = ctx.per_way_rcs;
    let blob_group_map = ctx.blob_group_map;

    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(num_workers);
        for worker_id in 0..num_workers {
            let next_ref = &next_idx;
            let read_ref = &s3_read_ms;
            let scatter_ref = &s3_scatter_ms;
            let encode_ref = &s3_encode_ms;
            let bytes_read_ref = &s3_bytes_read;
            let bytes_written_ref = &s3_bytes_written;
            let entries_ref = &s3_entries_emitted;
            let groups_ref = &s3_groups_processed;
            let pub_ref = &s3_blobs_published;
            let zero_entry_ref = &s3_blobs_zero_entry_nonzero_ref;
            let max_coord_ref = &s3_max_group_coord_bytes;
            let err_ref = &s3_error;

            let handle = scope.spawn(move || -> std::result::Result<(), String> {
                use std::os::unix::fs::FileExt as _;
                use std::sync::atomic::Ordering::Relaxed;

                let abort_guard = AbortOnDrop::new(router, "stage 3 worker");

                let tmp_file = router.worker_file(worker_id);
                let mut tmp_byte_pos: u64 = 0;

                // Reusable per-group coord buffer sized to the group's
                // total slot span. At planet ~380 MB per worker in the
                // worst case; allocated once, resized per group.
                let mut coord_buf: Vec<u8> = Vec::new();
                // Reusable scratch for varint-encoded output.
                let mut encode_scratch: Vec<u8> = Vec::new();

                loop {
                    if router.is_aborted()
                        || err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some()
                    {
                        break;
                    }
                    let idx = next_ref.fetch_add(1, Relaxed);
                    if idx >= blob_group_count { break; }
                    let group_idx = idx;

                    let result: std::result::Result<(), String> = (|| {
                        let (group_blob_lo, group_blob_hi) =
                            blob_group_map.blob_range(group_idx);
                        if group_blob_lo == group_blob_hi {
                            // Empty group - nothing to publish (any
                            // blobs in a truly empty group are already
                            // pre-populated as Empty by the router).
                            return Ok(());
                        }

                        let group_slot_lo = way_slot_starts
                            .get(group_blob_lo as usize)
                            .copied()
                            .unwrap_or(total_slots);
                        let group_slot_hi = way_slot_starts
                            .get(group_blob_hi as usize)
                            .copied()
                            .unwrap_or(total_slots);
                        let group_slot_span = group_slot_hi.saturating_sub(group_slot_lo);

                        // Allocate + zero-fill the coord buffer for
                        // this group. Missing entries (a node lookup
                        // that didn't resolve) stay (0, 0) - the
                        // stage-4 sentinel for "no location".
                        #[allow(clippy::cast_possible_truncation)]
                        let coord_bytes = (group_slot_span as usize) * COORD_SLOT_SIZE;
                        coord_buf.clear();
                        coord_buf.resize(coord_bytes, 0);
                        max_coord_ref.fetch_max(coord_bytes as u64, Relaxed);

                        // Read the whole group file via BufReader. At
                        // planet each group is ~600 MB; streaming in
                        // 256 KB chunks is fine and avoids a 600 MB
                        // allocation per worker. Chunks are realigned
                        // to RESOLVED_ENTRY_SIZE boundaries via a
                        // small carry-over buffer.
                        let path = &blob_groups.paths[group_idx];
                        let expected_bytes =
                            blob_groups.entry_counts[group_idx] * RESOLVED_ENTRY_SIZE as u64;
                        let t_read = std::time::Instant::now();
                        let file = std::fs::File::open(path)
                            .map_err(|e| format!("open group {group_idx}: {e}"))?;
                        let mut reader = BufReader::with_capacity(256 * 1024, file);
                        let mut entry_buf = [0u8; RESOLVED_ENTRY_SIZE];
                        let mut entries: u64 = 0;
                        let mut scatter_ns: u64 = 0;
                        loop {
                            match reader.read_exact(&mut entry_buf) {
                                Ok(()) => {}
                                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                                Err(e) => {
                                    return Err(format!(
                                        "read group {group_idx}: {e}"
                                    ));
                                }
                            }
                            let t_scatter = std::time::Instant::now();
                            let blob_idx = u32::from_le_bytes(
                                entry_buf[0..4].try_into().unwrap_or([0; 4]),
                            );
                            let blob_local_slot = u32::from_le_bytes(
                                entry_buf[4..8].try_into().unwrap_or([0; 4]),
                            );
                            if blob_idx < group_blob_lo || blob_idx >= group_blob_hi {
                                return Err(format!(
                                    "group {group_idx} entry blob_idx {blob_idx} outside \
                                     [{group_blob_lo}, {group_blob_hi})"
                                ));
                            }
                            let blob_slot_start = way_slot_starts[blob_idx as usize]
                                .saturating_sub(group_slot_lo);
                            #[allow(clippy::cast_possible_truncation)]
                            let off = (blob_slot_start + u64::from(blob_local_slot))
                                as usize
                                * COORD_SLOT_SIZE;
                            if off + COORD_SLOT_SIZE > coord_buf.len() {
                                let wslast = way_slot_starts
                                    .get(blob_idx as usize + 1)
                                    .copied()
                                    .unwrap_or(total_slots);
                                return Err(format!(
                                    "group {group_idx} blob {blob_idx} slot {blob_local_slot} \
                                     scatter offset {off} past coord_buf {} \
                                     [group_blob_lo={group_blob_lo} group_blob_hi={group_blob_hi} \
                                      group_slot_lo={group_slot_lo} group_slot_hi={group_slot_hi} \
                                      way_slot_starts[{blob_idx}]={} \
                                      way_slot_starts[{}]={wslast} total_slots={total_slots} \
                                      num_way_blobs={}]",
                                    coord_buf.len(),
                                    way_slot_starts[blob_idx as usize],
                                    blob_idx + 1,
                                    way_slot_starts.len(),
                                ));
                            }
                            coord_buf[off..off + 4].copy_from_slice(&entry_buf[8..12]);
                            coord_buf[off + 4..off + 8].copy_from_slice(&entry_buf[12..16]);
                            entries += 1;
                            #[allow(clippy::cast_possible_truncation)]
                            {
                                scatter_ns += t_scatter.elapsed().as_nanos() as u64;
                            }
                        }
                        #[allow(clippy::cast_possible_truncation)]
                        {
                            let read_ms = t_read.elapsed().as_millis() as u64
                                - scatter_ns / 1_000_000;
                            read_ref.fetch_add(read_ms, Relaxed);
                            scatter_ref.fetch_add(scatter_ns / 1_000_000, Relaxed);
                        }
                        bytes_read_ref.fetch_add(expected_bytes, Relaxed);
                        entries_ref.fetch_add(entries, Relaxed);

                        // Walk blobs in the group's blob_idx range.
                        // For each non-zero-ref blob: extract its
                        // slice of coord_buf, delta-varint encode,
                        // pwrite to the worker tmp file, publish.
                        // Zero-ref blobs are already Empty in the
                        // router; skip them.
                        let t_enc = std::time::Instant::now();
                        for blob_idx in group_blob_lo..group_blob_hi {
                            if !per_way_rcs
                                .blob_has_nonzero_refs(blob_idx as usize)
                                .map_err(|e| {
                                    format!("per_way_rcs blob {blob_idx}: {e}")
                                })?
                            {
                                continue;
                            }
                            let blob_slot_start = way_slot_starts[blob_idx as usize]
                                .saturating_sub(group_slot_lo);
                            let blob_slot_end = way_slot_starts
                                .get(blob_idx as usize + 1)
                                .copied()
                                .unwrap_or(group_slot_hi)
                                .saturating_sub(group_slot_lo);
                            #[allow(clippy::cast_possible_truncation)]
                            let blob_slice_start =
                                blob_slot_start as usize * COORD_SLOT_SIZE;
                            #[allow(clippy::cast_possible_truncation)]
                            let blob_slice_end =
                                blob_slot_end as usize * COORD_SLOT_SIZE;
                            let blob_slice = &coord_buf[blob_slice_start..blob_slice_end];

                            encode_scratch.clear();
                            encode_blob_payload_from_record(
                                blob_slice,
                                per_way_rcs.blob_record(blob_idx as usize),
                                blob_idx as usize,
                                &mut encode_scratch,
                            )
                            .map_err(|e| {
                                format!("encode group {group_idx} blob {blob_idx}: {e}")
                            })?;

                            let byte_offset = tmp_byte_pos;
                            let byte_length = encode_scratch.len() as u64;
                            tmp_file
                                .write_all_at(&encode_scratch, byte_offset)
                                .map_err(|e| {
                                    format!("write worker tmp blob {blob_idx}: {e}")
                                })?;
                            tmp_byte_pos += byte_length;
                            bytes_written_ref.fetch_add(byte_length, Relaxed);

                            #[allow(clippy::cast_possible_truncation)]
                            router
                                .publish_worker(
                                    blob_idx as usize,
                                    worker_id as u32,
                                    byte_offset,
                                    byte_length,
                                )
                                .map_err(|e| {
                                    format!("publish blob {blob_idx}: {e}")
                                })?;
                            pub_ref.fetch_add(1, Relaxed);

                            // Tag defensive: blob has non-zero refs
                            // but its slice is all zeros (stage 2
                            // may have been unable to resolve any
                            // refs). Still published above as a
                            // zero-coord payload so stage 4 doesn't
                            // hit the router's missing-publication
                            // error; this counter just surfaces the
                            // condition.
                            if blob_slice.iter().all(|&b| b == 0) {
                                zero_entry_ref.fetch_add(1, Relaxed);
                            }
                        }
                        #[allow(clippy::cast_possible_truncation)]
                        encode_ref.fetch_add(t_enc.elapsed().as_millis() as u64, Relaxed);
                        groups_ref.fetch_add(1, Relaxed);

                        Ok(())
                    })();

                    if let Err(e) = result {
                        *err_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(e.clone());
                        router.abort(format!("stage 3 worker {worker_id}: {e}"));
                        return Err(e);
                    }
                }

                abort_guard.disarm();
                Ok(())
            });
            handles.push(handle);
        }

        let mut first_err: Option<String> = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(payload) => {
                    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                        format!("stage 3 thread panicked: {s}")
                    } else if let Some(s) = payload.downcast_ref::<String>() {
                        format!("stage 3 thread panicked: {s}")
                    } else {
                        "stage 3 thread panicked (unknown payload)".to_string()
                    };
                    if first_err.is_none() {
                        first_err = Some(msg);
                    }
                }
            }
        }
        if let Some(e) = first_err {
            *s3_error.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e);
        }
    });

    if let Some(e) = s3_error
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        return Err(e.into());
    }

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter(
            "s3_read_ms",
            s3_read_ms.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s3_scatter_ms",
            s3_scatter_ms.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s3_encode_ms",
            s3_encode_ms.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s3_bytes_read",
            s3_bytes_read.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s3_bytes_written",
            s3_bytes_written.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s3_entries_emitted",
            s3_entries_emitted.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s3_groups_processed",
            s3_groups_processed.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s3_blobs_published",
            s3_blobs_published.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s3_blobs_zero_entry_nonzero_ref",
            s3_blobs_zero_entry_nonzero_ref.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s3_max_group_coord_bytes",
            s3_max_group_coord_bytes.load(std::sync::atomic::Ordering::Relaxed) as i64,
        );
    }

    Ok(())
}
