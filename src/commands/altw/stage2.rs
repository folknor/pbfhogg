//! Stage 2: Node join — counting-sort per rank bucket, coord slice lookup.

use std::io::Write as _;
use std::path::Path;

use super::super::external_radix::{BucketWriters, ScratchDir, NUM_BUCKETS};
#[cfg(feature = "linux-direct-io")]
use super::super::external_radix::advise_dontneed_file;
use super::super::Result;
use super::{RANK_RECORD_SIZE, RESOLVED_ENTRY_SIZE, COORD_SLOT_SIZE, ResolvedEntry};

// ---------------------------------------------------------------------------
// Stage 2: Node join — coord slice lookup
// ---------------------------------------------------------------------------

/// For each rank bucket: load rank records, counting-sort, pread the
/// bucket's contiguous coord slice from the coords_by_rank file, resolve
/// ranks to (lat, lon) by direct array index, emit to slot buckets.
///
/// No node stream. No merge-join. Coords resolved via file-backed dense
/// array indexed by rank.
#[hotpath::measure]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) fn stage2_node_join(
    scratch: &ScratchDir,
    rank_bucket_counts: &[u64],
    num_shard_workers: usize,
    slot_buckets: &mut BucketWriters,
    total_slots: u64,
    unique_nodes: u64,
    coord_file_path: &Path,
) -> Result<u64> {
    let mut resolved_count: u64 = 0;
    let rank_range_size = unique_nodes.div_ceil(NUM_BUCKETS as u64);

    struct PreparedBucket {
        grouped_slot_pos: Vec<u64>,
        group_offsets: Vec<u64>,
        bucket_rank_start: u64,
    }

    struct LoaderScratch {
        data_buf: Vec<u8>,
        counts: Vec<u64>,
        write_pos: Vec<u64>,
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
        for worker_id in 0..num_shard_workers {
            let path = scratch.path.join(format!("rank-W{worker_id}-{bucket_idx:03}"));
            let file = match std::fs::File::open(&path) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("open rank shard: {e}")),
            };
            let len = file.metadata()
                .map_err(|e| format!("stat rank shard: {e}"))?
                .len() as usize;
            if len == 0 { continue; }
            let start = loader.data_buf.len();
            loader.data_buf.resize(start + len, 0);
            std::io::Read::read_exact(&mut &file, &mut loader.data_buf[start..])
                .map_err(|e| format!("read rank shard: {e}"))?;
            #[cfg(feature = "linux-direct-io")]
            advise_dontneed_file(&file);
        }

        for chunk in loader.data_buf.chunks_exact(RANK_RECORD_SIZE) {
            let local_rank = u32::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3],
            ]) as usize;
            loader.counts[local_rank] += 1;
        }

        let mut group_offsets = vec![0u64; local_range + 1];
        for (i, count) in loader.counts.iter().enumerate() {
            group_offsets[i + 1] = group_offsets[i] + count;
        }

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

        Ok(PreparedBucket { grouped_slot_pos, group_offsets, bucket_rank_start })
    }

    // Pipelined bucket loader + coord file for per-bucket slice reads.
    let (bucket_tx, bucket_rx) = std::sync::mpsc::sync_channel::<
        std::result::Result<PreparedBucket, String>
    >(2);

    let s2_stop = std::sync::atomic::AtomicBool::new(false);
    let s2_stop_ref = &s2_stop;

    use std::os::unix::fs::FileExt as _;
    let coord_file = std::fs::File::open(coord_file_path)
        .map_err(|e| format!("open coords_by_rank for stage 2: {e}"))?;
    let mut entry_buf = [0u8; RESOLVED_ENTRY_SIZE];
    let mut coord_slice: Vec<u8> = Vec::new();
    let mut s2_coord_read_ms: u64 = 0;
    let mut s2_resolve_ms: u64 = 0;
    let mut s2_bucket_load_ms: u64 = 0;
    let mut s2_bucket_loads: u64 = 0;

    std::thread::scope(|scope| -> Result<()> {
        {
            let tx = bucket_tx;
            scope.spawn(move || {
                let mut loader = LoaderScratch {
                    data_buf: Vec::new(), counts: Vec::new(), write_pos: Vec::new(),
                };
                for bucket_idx in 0..NUM_BUCKETS {
                    if s2_stop_ref.load(std::sync::atomic::Ordering::Relaxed) { break; }
                    if rank_bucket_counts[bucket_idx] == 0 { continue; }
                    let result = prepare_bucket(
                        bucket_idx, scratch, num_shard_workers,
                        unique_nodes, rank_range_size, &mut loader,
                    );
                    let is_err = result.is_err();
                    if tx.send(result).is_err() { break; }
                    if is_err { break; }
                }
            });
        }

        // Consumer: receive prepared buckets, pread coord slice, resolve.
        loop {
            let t_load = std::time::Instant::now();
            let msg = bucket_rx.recv();
            let bkt = match msg {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => {
                    s2_stop_ref.store(true, std::sync::atomic::Ordering::Relaxed);
                    return Err(e.into());
                }
                Err(_) => break,
            };
            #[allow(clippy::cast_possible_truncation)]
            { s2_bucket_load_ms += t_load.elapsed().as_millis() as u64; }
            s2_bucket_loads += 1;

            // pread this bucket's contiguous coord slice.
            let t_coord = std::time::Instant::now();
            let local_range = bkt.group_offsets.len() - 1;
            let slice_bytes = local_range * COORD_SLOT_SIZE;
            coord_slice.resize(slice_bytes, 0);
            coord_file.read_exact_at(
                &mut coord_slice, bkt.bucket_rank_start * COORD_SLOT_SIZE as u64,
            ).map_err(|e| -> Box<dyn std::error::Error> {
                format!("pread coord slice: {e}").into()
            })?;
            #[allow(clippy::cast_possible_truncation)]
            { s2_coord_read_ms += t_coord.elapsed().as_millis() as u64; }

            // Resolve each rank group against the local coord slice.
            let t_resolve = std::time::Instant::now();
            for local_rank in 0..local_range {
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
                let is_resolved = lat != 0 || lon != 0;

                for &slot_pos in &bkt.grouped_slot_pos[start..end] {
                    let entry = ResolvedEntry { slot_pos, lat, lon };
                    let bucket = entry.slot_bucket(total_slots);
                    entry.write_to(&mut entry_buf);
                    if let Some(writer) = slot_buckets.writers[bucket].as_mut() {
                        writer.write_all(&entry_buf)?;
                    }
                    slot_buckets.entry_counts[bucket] += 1;
                    if is_resolved {
                        resolved_count += 1;
                    }
                }
            }
            #[allow(clippy::cast_possible_truncation)]
            { s2_resolve_ms += t_resolve.elapsed().as_millis() as u64; }
        }
        Ok(())
    })?;

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("s2_coord_read_ms", s2_coord_read_ms as i64);
        crate::debug::emit_counter("s2_resolve_ms", s2_resolve_ms as i64);
        crate::debug::emit_counter("s2_bucket_load_ms", s2_bucket_load_ms as i64);
        crate::debug::emit_counter("s2_bucket_loads", s2_bucket_loads as i64);
    }

    Ok(resolved_count)
}



