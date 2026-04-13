//! Stage 3: Slot reorder — build final coord_slots file.

use std::path::Path;

use super::super::external_radix::NUM_BUCKETS;
#[cfg(feature = "linux-direct-io")]
use super::super::external_radix::advise_dontneed_file;
use super::super::Result;
use super::{RESOLVED_ENTRY_SIZE, COORD_SLOT_SIZE, ResolvedEntry};

/// Lightweight reference to slot bucket paths + counts for stage 3.
/// Each bucket may have multiple files (one per stage-2 worker).
pub(super) struct SlotBucketRef {
    /// Per-bucket list of worker files. `paths[bucket_idx]` is a Vec of
    /// all worker files for that bucket.
    pub(super) paths: Vec<Vec<std::path::PathBuf>>,
    /// Total entry count per bucket (summed across all workers).
    pub(super) entry_counts: Vec<u64>,
}

/// Pre-allocates the output file to `total_slots * 8` bytes (zero-filled
/// by the OS). Empty buckets need no explicit zero-write — the file is
/// already zeroed.
#[hotpath::measure]
#[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
pub(super) fn stage3_slot_reorder(
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
                        continue;
                    }

                    let result: std::result::Result<(), String> = (|| {
                        let bucket_bytes = bucket_slots as usize * COORD_SLOT_SIZE;
                        scatter_buf.clear();
                        scatter_buf.resize(bucket_bytes, 0);

                        // Load from all worker files for this bucket.
                        let t_load = std::time::Instant::now();
                        data_buf.clear();
                        for path in &paths[bucket_idx] {
                            let bucket_file = match std::fs::File::open(path) {
                                Ok(f) => f,
                                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                                Err(e) => return Err(format!("open slot bucket: {e}")),
                            };
                            std::io::Read::read_to_end(&mut &bucket_file, &mut data_buf)
                                .map_err(|e| format!("read slot bucket: {e}"))?;
                            #[cfg(feature = "linux-direct-io")]
                            advise_dontneed_file(&bucket_file);
                        }
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
