//! Epoch-spill alternative to the disk-backed slot-bucket handoff between
//! stage 2 and stage 3.
//!
//! Prototype path, gated by `PBFHOGG_ALTW_SLOT_EPOCHS=N` (N >= 1). Replaces
//! `SlotBuckets::create` + `stage2_node_join` + `stage3_slot_reorder` with a
//! single fused entry point. Reuses `classify_blobs_in_bucket` and
//! `emit_integrated_intersections` from stage 3 unchanged.
//!
//! # Algorithm
//!
//! Partition the `slot_bucket_count` slot buckets into `N` contiguous epochs.
//! Epoch 0 is the "active" epoch during the full stage-2 pass.
//!
//! 1. **Epoch 0 producer pass.** Run stage-2 workers (atomic claim over rank
//!    buckets) just like the disk path. For each resolved entry, look up its
//!    slot bucket, then its epoch:
//!    - Epoch 0: scatter directly into the bucket's in-memory `scatter_buf`
//!      (one zero-init `Box<[u8]>` per bucket, sized `bucket_slots * 8`,
//!      protected by a per-bucket `Mutex`).
//!    - Epoch >0: append the 16-byte ResolvedEntry record to a per-worker
//!      per-epoch spill buffer; flush to disk when the buffer hits the
//!      flush threshold.
//! 2. **Epoch 0 emit.** Once the producer scope closes, run a fresh
//!    `thread::scope` that classifies blobs against epoch 0's buckets and
//!    encodes payloads via `emit_integrated_intersections`. Free epoch 0's
//!    scatter buffers.
//! 3. **Epochs 1..N-1.** For each remaining epoch e: open the spill files,
//!    scatter their contents into freshly-allocated scatter buffers for
//!    epoch e's buckets, then run the same emit pass. Free buffers.
//!
//! # Memory ceiling
//!
//! Peak active scatter_buf bytes ≈ `total_slots * 8 / N`. At E=4 on planet
//! that is ~6.8 GB, vs ~3 GB for the disk-backed stage 2/3 today.
//!
//! # Trade
//!
//! Spill footprint is bounded by `total_slots * 16 * (N-1) / N` — about
//! 75% of today's slot-bucket bytes at E=4. The remaining 25% never hits
//! disk at all.
//!
//! # Invariants the prototype must preserve
//!
//! * `ResolvedEntry::slot_bucket()` semantics: identical to the disk path.
//! * Empty buckets emit zero-coordinate payloads: scatter_bufs are zero-init
//!   so the existing emit path handles this with no special branch.
//! * 2-piece straddler invariant: `straddler_slots` is shared across all
//!   epochs and `merge_straddler` already tolerates either-half-first arrival.
//! * Each blob receives at most one ManifestEntry total across all epochs:
//!   enforced by a per-blob `AtomicBool` set in the FullyContained path.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::FileExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::super::external_radix::{ScratchDir, NUM_BUCKETS};
use super::super::id_set_dense::IdSetDense;
use super::super::node_scanner::{extract_node_tuples, NodeTuple};
use super::super::Result;
use super::blob_bucket_index::classify_blobs_in_bucket;
use super::coord_payloads::{ManifestEntry, PerWayRcs, StraddlerSlot};
use super::stage2::{prepare_bucket, LoaderScratch};
use super::stage3::{emit_integrated_intersections, scatter_bucket_entries, IntegratedInputs};
use super::{NodeBlobInfo, ResolvedEntry, COORD_SLOT_SIZE, RESOLVED_ENTRY_SIZE};

/// Parse `PBFHOGG_ALTW_SLOT_EPOCHS=N` from env. Returns Some(N) if N >= 1,
/// None otherwise.
pub(super) fn parse_epoch_env() -> Option<usize> {
    let raw = std::env::var("PBFHOGG_ALTW_SLOT_EPOCHS").ok()?;
    let n: usize = raw.trim().parse().ok()?;
    if n == 0 {
        return None;
    }
    Some(n)
}

/// Per-epoch [bucket_lo, bucket_hi) ranges, evenly partitioning
/// `[0, slot_bucket_count)`.
fn compute_epoch_ranges(num_epochs: usize, slot_bucket_count: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(num_epochs);
    for e in 0..num_epochs {
        let lo = (e * slot_bucket_count) / num_epochs;
        let hi = ((e + 1) * slot_bucket_count) / num_epochs;
        out.push((lo, hi));
    }
    out
}

/// `bucket_epoch[bucket_idx] = which epoch this bucket belongs to`.
fn compute_bucket_epoch(epoch_ranges: &[(usize, usize)], slot_bucket_count: usize) -> Vec<u8> {
    let mut out = vec![0u8; slot_bucket_count];
    for (e, &(lo, hi)) in epoch_ranges.iter().enumerate() {
        for b in lo..hi {
            #[allow(clippy::cast_possible_truncation)]
            {
                out[b] = e as u8;
            }
        }
    }
    out
}

/// Mirror of stage 3's bucket extent calculation. The slot bucket [b]
/// covers `[b * range_size, (b+1) * range_size)`, except the last bucket
/// extends to `total_slots`.
fn bucket_extents(
    bucket_idx: usize,
    slot_bucket_count: usize,
    total_slots: u64,
) -> (u64, u64) {
    let range_size = total_slots / slot_bucket_count as u64;
    let start = bucket_idx as u64 * range_size;
    let end = if bucket_idx + 1 == slot_bucket_count {
        total_slots
    } else {
        ((bucket_idx as u64 + 1) * range_size).min(total_slots)
    };
    (start, end)
}

/// Inputs for the fused stage 2/3 epoch path.
pub(super) struct Stage23EpochInputs<'a> {
    pub scratch: &'a ScratchDir,
    pub num_shard_workers: usize,
    pub rank_bucket_counts: &'a [u64],
    pub slot_bucket_count: usize,
    pub total_slots: u64,
    pub unique_nodes: u64,
    pub input_pbf: Arc<std::fs::File>,
    pub node_id_set: &'a IdSetDense,
    pub node_blob_mapping: &'a [NodeBlobInfo],
    pub way_slot_starts: &'a [u64],
    pub per_way_rcs: &'a PerWayRcs,
    pub worker_tmp_paths: &'a [PathBuf],
    pub straddler_slots: &'a [Mutex<Option<StraddlerSlot>>],
    pub num_epochs: usize,
}

pub(super) struct Stage23EpochOutput {
    pub worker_manifests: Vec<Vec<ManifestEntry>>,
    pub resolved_count: u64,
}

/// Per-worker tmp writer state, carried across all epochs so each emit pass
/// appends to the same physical file with a monotonic byte position.
struct WorkerTmpState {
    path: PathBuf,
    byte_pos: u64,
    manifest: Vec<ManifestEntry>,
}

const FLUSH_THRESHOLD: usize = 256 * 1024;
const SPILL_FLUSH_THRESHOLD: usize = 256 * 1024;

fn num_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4)
        .min(6)
}

fn spill_path(scratch: &ScratchDir, worker_id: usize, epoch_idx: usize) -> PathBuf {
    scratch.file_path(&format!("s23e-W{worker_id}-E{epoch_idx:02}"))
}

/// Truncate every per-worker tmp file so the per-epoch append-mode reopens
/// see a known starting byte position (0).
fn truncate_worker_tmps(paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(|e| format!("init worker tmp {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Allocate scatter buffers for one epoch's bucket range.
///
/// Returns a vector of length `bucket_hi - bucket_lo`. Index `i` corresponds
/// to global bucket index `bucket_lo + i`. Each buffer is zero-filled so the
/// emit path's empty-bucket semantics fall out of the normal code path.
fn allocate_epoch_scatter_bufs(
    bucket_lo: usize,
    bucket_hi: usize,
    slot_bucket_count: usize,
    total_slots: u64,
) -> Vec<Mutex<Box<[u8]>>> {
    let mut out = Vec::with_capacity(bucket_hi - bucket_lo);
    for bucket_idx in bucket_lo..bucket_hi {
        let (start, end) = bucket_extents(bucket_idx, slot_bucket_count, total_slots);
        #[allow(clippy::cast_possible_truncation)]
        let bucket_bytes = ((end - start) as usize) * COORD_SLOT_SIZE;
        let buf = vec![0u8; bucket_bytes].into_boxed_slice();
        out.push(Mutex::new(buf));
    }
    out
}

/// Sum of scatter-buf bytes for one epoch (instrumentation).
fn epoch_scatter_bytes(scatter_bufs: &[Mutex<Box<[u8]>>]) -> u64 {
    let mut total: u64 = 0;
    for sb in scatter_bufs {
        let g = sb.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        total += g.len() as u64;
    }
    total
}

#[hotpath::measure]
#[allow(clippy::too_many_lines)]
pub(super) fn stage23_epoch_fused(
    inputs: Stage23EpochInputs<'_>,
) -> Result<Stage23EpochOutput> {
    let num_epochs = inputs.num_epochs;
    let slot_bucket_count = inputs.slot_bucket_count;
    let epoch_ranges = compute_epoch_ranges(num_epochs, slot_bucket_count);
    let bucket_epoch = compute_bucket_epoch(&epoch_ranges, slot_bucket_count);

    crate::debug::emit_marker("EXTJOIN_S23EPOCH_START");
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("s23epoch_num_epochs", num_epochs as i64);
        crate::debug::emit_counter("s23epoch_slot_bucket_count", slot_bucket_count as i64);
    }
    eprintln!(
        "[altw] mode: epoch-spill (E={num_epochs}, slot_buckets={slot_bucket_count})"
    );

    let n_workers = num_workers();
    let n_tmps = inputs.worker_tmp_paths.len();
    if n_tmps == 0 {
        return Err("epoch path requires at least one worker_tmp_path".into());
    }

    truncate_worker_tmps(inputs.worker_tmp_paths)?;

    let mut worker_tmps: Vec<WorkerTmpState> = inputs
        .worker_tmp_paths
        .iter()
        .map(|p| WorkerTmpState {
            path: p.clone(),
            byte_pos: 0,
            manifest: Vec::new(),
        })
        .collect();

    // At-most-one-ManifestEntry-per-blob guard, shared across all epochs.
    let num_blobs = inputs.per_way_rcs.num_blobs();
    let fully_contained_emitted: Vec<AtomicBool> =
        (0..num_blobs).map(|_| AtomicBool::new(false)).collect();

    let s23epoch_spill_bytes_written = AtomicU64::new(0);
    let s23epoch_spill_bytes_read = AtomicU64::new(0);
    let s23epoch_active_scatter_peak_bytes = AtomicU64::new(0);

    let mut total_resolved: u64 = 0;

    // ------------------------------------------------------------------
    // Epoch 0: full stage-2 producer + scatter epoch 0 / spill epochs >0
    // ------------------------------------------------------------------
    let (epoch0_bucket_lo, epoch0_bucket_hi) = epoch_ranges[0];
    let epoch0_scatter_bufs = allocate_epoch_scatter_bufs(
        epoch0_bucket_lo,
        epoch0_bucket_hi,
        slot_bucket_count,
        inputs.total_slots,
    );
    {
        let bytes = epoch_scatter_bytes(&epoch0_scatter_bufs);
        let mut current = s23epoch_active_scatter_peak_bytes.load(Ordering::Relaxed);
        while bytes > current {
            match s23epoch_active_scatter_peak_bytes.compare_exchange_weak(
                current,
                bytes,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    crate::debug::emit_marker("EXTJOIN_S23EPOCH_EPOCH0_PRODUCER_START");
    let ep0_resolved = run_epoch0_producer(
        &inputs,
        &bucket_epoch,
        &epoch_ranges,
        n_workers,
        &epoch0_scatter_bufs,
        &s23epoch_spill_bytes_written,
    )?;
    crate::debug::emit_marker("EXTJOIN_S23EPOCH_EPOCH0_PRODUCER_END");
    total_resolved += ep0_resolved;

    crate::debug::emit_marker("EXTJOIN_S23EPOCH_EPOCH0_EMIT_START");
    run_epoch_emit(
        0,
        &epoch_ranges,
        &epoch0_scatter_bufs,
        &inputs,
        &mut worker_tmps,
        &fully_contained_emitted,
        n_workers,
    )?;
    crate::debug::emit_marker("EXTJOIN_S23EPOCH_EPOCH0_EMIT_END");
    drop(epoch0_scatter_bufs);

    // ------------------------------------------------------------------
    // Epochs 1..N-1: drain spill -> scatter -> emit
    // ------------------------------------------------------------------
    for epoch_idx in 1..num_epochs {
        let (lo, hi) = epoch_ranges[epoch_idx];
        let scatter_bufs = allocate_epoch_scatter_bufs(
            lo,
            hi,
            slot_bucket_count,
            inputs.total_slots,
        );
        {
            let bytes = epoch_scatter_bytes(&scatter_bufs);
            let mut current = s23epoch_active_scatter_peak_bytes.load(Ordering::Relaxed);
            while bytes > current {
                match s23epoch_active_scatter_peak_bytes.compare_exchange_weak(
                    current,
                    bytes,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(actual) => current = actual,
                }
            }
        }

        crate::debug::emit_marker(&format!("EXTJOIN_S23EPOCH_EPOCH{epoch_idx}_DRAIN_START"));
        run_epoch_drain(
            epoch_idx,
            &inputs,
            &scatter_bufs,
            slot_bucket_count,
            n_workers,
            n_tmps,
            &s23epoch_spill_bytes_read,
        )?;
        crate::debug::emit_marker(&format!("EXTJOIN_S23EPOCH_EPOCH{epoch_idx}_DRAIN_END"));

        crate::debug::emit_marker(&format!("EXTJOIN_S23EPOCH_EPOCH{epoch_idx}_EMIT_START"));
        run_epoch_emit(
            epoch_idx,
            &epoch_ranges,
            &scatter_bufs,
            &inputs,
            &mut worker_tmps,
            &fully_contained_emitted,
            n_workers,
        )?;
        crate::debug::emit_marker(&format!("EXTJOIN_S23EPOCH_EPOCH{epoch_idx}_EMIT_END"));
        drop(scatter_bufs);
    }

    // Spill cleanup. (mod.rs handles rank-shard cleanup for the disk path
    // separately; spill files belong to the epoch path only.)
    for epoch_idx in 1..num_epochs {
        for worker_id in 0..n_tmps {
            let p = spill_path(inputs.scratch, worker_id, epoch_idx);
            drop(std::fs::remove_file(&p));
        }
    }

    let worker_manifests: Vec<Vec<ManifestEntry>> = worker_tmps
        .into_iter()
        .map(|w| w.manifest)
        .collect();

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter(
            "s23epoch_spill_bytes_written",
            s23epoch_spill_bytes_written.load(Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s23epoch_spill_bytes_read",
            s23epoch_spill_bytes_read.load(Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "s23epoch_active_scatter_peak_bytes",
            s23epoch_active_scatter_peak_bytes.load(Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter("s23epoch_resolved_count", total_resolved as i64);
    }
    crate::debug::emit_marker("EXTJOIN_S23EPOCH_END");

    Ok(Stage23EpochOutput {
        worker_manifests,
        resolved_count: total_resolved,
    })
}

/// Drain a per-bucket local buffer of 16-byte ResolvedEntry records into
/// the bucket's shared scatter_buf. Equivalent to `scatter_bucket_entries`
/// but writes through a Mutex guard.
fn flush_local_to_scatter(
    bucket_idx_in_epoch: usize,
    global_bucket_idx: usize,
    local_buf: &mut Vec<u8>,
    scatter_bufs: &[Mutex<Box<[u8]>>],
    slot_bucket_count: usize,
    total_slots: u64,
) -> std::result::Result<(), String> {
    if local_buf.is_empty() {
        return Ok(());
    }
    let (bucket_start, bucket_end) =
        bucket_extents(global_bucket_idx, slot_bucket_count, total_slots);
    let mut guard = scatter_bufs[bucket_idx_in_epoch]
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    scatter_bucket_entries(
        local_buf,
        global_bucket_idx,
        bucket_start,
        bucket_end,
        &mut guard[..],
    )?;
    drop(guard);
    local_buf.clear();
    Ok(())
}

/// Producer for epoch 0. Replicates `stage2_node_join`'s per-rank-bucket
/// processing (claim, prepare_bucket, node-blob preads, resolve loop) but
/// changes routing: epoch-0 entries scatter into shared in-memory
/// `scatter_bufs` and entries for epochs >0 spill to per-worker per-epoch
/// files.
///
/// Returns total resolved-count across all workers.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn run_epoch0_producer(
    inputs: &Stage23EpochInputs<'_>,
    bucket_epoch: &[u8],
    epoch_ranges: &[(usize, usize)],
    n_workers: usize,
    epoch0_scatter_bufs: &[Mutex<Box<[u8]>>],
    spill_bytes_written: &AtomicU64,
) -> Result<u64> {
    let num_epochs = epoch_ranges.len();
    let (epoch0_lo, _epoch0_hi) = epoch_ranges[0];
    let rank_range_size = inputs.unique_nodes.div_ceil(NUM_BUCKETS as u64);

    let next_idx = AtomicUsize::new(0);
    let resolved_total = AtomicU64::new(0);
    let s2_error: Mutex<Option<String>> = Mutex::new(None);

    let next_ref = &next_idx;
    let resolved_ref = &resolved_total;
    let mapping_ref = inputs.node_blob_mapping;
    let id_set_ref = inputs.node_id_set;
    let err_ref = &s2_error;
    let scratch_ref = inputs.scratch;
    let num_shard_workers = inputs.num_shard_workers;
    let unique_nodes = inputs.unique_nodes;
    let total_slots = inputs.total_slots;
    let slot_bucket_count = inputs.slot_bucket_count;
    let rank_bucket_counts = inputs.rank_bucket_counts;
    let bucket_epoch_ref = bucket_epoch;
    let scatter_bufs_ref = epoch0_scatter_bufs;
    let spill_bytes_ref = spill_bytes_written;

    std::thread::scope(|scope| {
        for worker_id in 0..n_workers {
            let pbf_file = Arc::clone(&inputs.input_pbf);
            scope.spawn(move || {
                use std::sync::atomic::Ordering::Relaxed;

                let mut loader = LoaderScratch::new();
                #[allow(clippy::cast_possible_truncation)]
                let max_slice_bytes = (rank_range_size as usize) * COORD_SLOT_SIZE;
                let mut coord_slice: Vec<u8> = vec![0u8; max_slice_bytes];
                let mut node_read_buf: Vec<u8> = Vec::new();
                let mut node_decompress_buf: Vec<u8> = Vec::new();
                let mut node_tuples: Vec<NodeTuple> = Vec::new();
                let mut node_group_starts: Vec<(usize, usize)> = Vec::new();
                let mut entry_buf = [0u8; RESOLVED_ENTRY_SIZE];
                let mut local_resolved: u64 = 0;

                // Per-bucket local batch for epoch 0 (length = slot_bucket_count;
                // only indices in [epoch0_lo, epoch0_hi) ever get written).
                let mut active_local_bufs: Vec<Vec<u8>> =
                    (0..slot_bucket_count).map(|_| Vec::new()).collect();

                // Per-epoch local batch for spilled epochs (index 0 unused).
                let mut spill_local_bufs: Vec<Vec<u8>> =
                    (0..num_epochs).map(|_| Vec::new()).collect();

                // Per-epoch spill writer (None until first flush).
                let mut spill_writers: Vec<Option<std::io::BufWriter<std::fs::File>>> =
                    (0..num_epochs).map(|_| None).collect();

                loop {
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
                    if rank_bucket_counts[bucket_idx] == 0 {
                        continue;
                    }

                    let result: std::result::Result<(), String> = (|| {
                        let bkt = prepare_bucket(
                            bucket_idx,
                            scratch_ref,
                            num_shard_workers,
                            unique_nodes,
                            rank_range_size,
                            &mut loader,
                        )?;
                        let slice_bytes = bkt.local_range * COORD_SLOT_SIZE;
                        coord_slice[..slice_bytes].fill(0);
                        let bucket_rank_start = bkt.bucket_rank_start;
                        let bucket_rank_end = bucket_rank_start + bkt.local_range as u64;

                        let lo = mapping_ref
                            .partition_point(|b| b.ref_rank_end <= bucket_rank_start);
                        let hi = mapping_ref
                            .partition_point(|b| b.ref_rank_start < bucket_rank_end);
                        for blob in &mapping_ref[lo..hi] {
                            if blob.ref_count() == 0 {
                                continue;
                            }
                            node_read_buf.resize(blob.data_size, 0);
                            pbf_file
                                .read_exact_at(&mut node_read_buf, blob.data_offset)
                                .map_err(|e| format!("epoch s2 node pread: {e}"))?;

                            crate::blob::decompress_blob_raw(
                                &node_read_buf,
                                &mut node_decompress_buf,
                            )
                            .map_err(|e| format!("epoch s2 node decompress: {e}"))?;

                            node_tuples.clear();
                            extract_node_tuples(
                                &node_decompress_buf,
                                &mut node_tuples,
                                &mut node_group_starts,
                            )
                            .map_err(|e| format!("epoch s2 node extract: {e}"))?;

                            for &NodeTuple { id, lat, lon } in &node_tuples {
                                let Some(rank) = id_set_ref.rank_if_set(id) else {
                                    continue;
                                };
                                if rank < bucket_rank_start || rank >= bucket_rank_end {
                                    continue;
                                }
                                #[allow(clippy::cast_possible_truncation)]
                                let local_rank = (rank - bucket_rank_start) as usize;
                                let off = local_rank * COORD_SLOT_SIZE;
                                coord_slice[off..off + 4].copy_from_slice(&lat.to_le_bytes());
                                coord_slice[off + 4..off + 8]
                                    .copy_from_slice(&lon.to_le_bytes());
                            }
                        }

                        // Resolve groups → route entries to scatter_bufs or spill.
                        for local_rank in 0..bkt.local_range {
                            #[allow(clippy::cast_possible_truncation)]
                            let start = bkt.group_offsets[local_rank] as usize;
                            #[allow(clippy::cast_possible_truncation)]
                            let end = bkt.group_offsets[local_rank + 1] as usize;
                            if start == end {
                                continue;
                            }
                            let co = local_rank * COORD_SLOT_SIZE;
                            let lat = i32::from_le_bytes([
                                coord_slice[co],
                                coord_slice[co + 1],
                                coord_slice[co + 2],
                                coord_slice[co + 3],
                            ]);
                            let lon = i32::from_le_bytes([
                                coord_slice[co + 4],
                                coord_slice[co + 5],
                                coord_slice[co + 6],
                                coord_slice[co + 7],
                            ]);
                            let is_resolved = lat != 0 || lon != 0;

                            for &slot_pos in &bkt.grouped_slot_pos[start..end] {
                                let entry = ResolvedEntry { slot_pos, lat, lon };
                                let bucket =
                                    entry.slot_bucket(total_slots, slot_bucket_count);
                                entry.write_to(&mut entry_buf);
                                if is_resolved {
                                    local_resolved += 1;
                                }
                                let epoch_idx = bucket_epoch_ref[bucket] as usize;
                                if epoch_idx == 0 {
                                    active_local_bufs[bucket]
                                        .extend_from_slice(&entry_buf);
                                    if active_local_bufs[bucket].len() >= FLUSH_THRESHOLD {
                                        flush_local_to_scatter(
                                            bucket - epoch0_lo,
                                            bucket,
                                            &mut active_local_bufs[bucket],
                                            scatter_bufs_ref,
                                            slot_bucket_count,
                                            total_slots,
                                        )?;
                                    }
                                } else {
                                    spill_local_bufs[epoch_idx]
                                        .extend_from_slice(&entry_buf);
                                    if spill_local_bufs[epoch_idx].len()
                                        >= SPILL_FLUSH_THRESHOLD
                                    {
                                        if spill_writers[epoch_idx].is_none() {
                                            let p = spill_path(scratch_ref, worker_id, epoch_idx);
                                            let f = std::fs::OpenOptions::new()
                                                .create(true)
                                                .truncate(true)
                                                .write(true)
                                                .open(&p)
                                                .map_err(|e| format!("create spill {}: {e}", p.display()))?;
                                            spill_writers[epoch_idx] = Some(
                                                std::io::BufWriter::with_capacity(256 * 1024, f),
                                            );
                                        }
                                        let writer = spill_writers[epoch_idx]
                                            .as_mut()
                                            .expect("just inserted spill writer");
                                        let n = spill_local_bufs[epoch_idx].len() as u64;
                                        writer
                                            .write_all(&spill_local_bufs[epoch_idx])
                                            .map_err(|e| format!("spill write: {e}"))?;
                                        spill_bytes_ref.fetch_add(n, Relaxed);
                                        spill_local_bufs[epoch_idx].clear();
                                    }
                                }
                            }
                        }

                        Ok(())
                    })();

                    if let Err(e) = result {
                        *err_ref
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e);
                        break;
                    }
                }

                // Drain per-bucket active local bufs.
                for bucket in epoch0_lo..(epoch0_lo + scatter_bufs_ref.len()) {
                    if active_local_bufs[bucket].is_empty() {
                        continue;
                    }
                    if let Err(e) = flush_local_to_scatter(
                        bucket - epoch0_lo,
                        bucket,
                        &mut active_local_bufs[bucket],
                        scatter_bufs_ref,
                        slot_bucket_count,
                        total_slots,
                    ) {
                        *err_ref
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e);
                        return;
                    }
                }

                // Drain per-epoch spill local bufs and flush+close writers.
                for epoch_idx in 1..num_epochs {
                    if !spill_local_bufs[epoch_idx].is_empty() {
                        if spill_writers[epoch_idx].is_none() {
                            let p = spill_path(scratch_ref, worker_id, epoch_idx);
                            let f = match std::fs::OpenOptions::new()
                                .create(true)
                                .truncate(true)
                                .write(true)
                                .open(&p)
                            {
                                Ok(f) => f,
                                Err(e) => {
                                    *err_ref
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                        Some(format!("create spill {}: {e}", p.display()));
                                    return;
                                }
                            };
                            spill_writers[epoch_idx] =
                                Some(std::io::BufWriter::with_capacity(256 * 1024, f));
                        }
                        let writer = spill_writers[epoch_idx]
                            .as_mut()
                            .expect("just inserted spill writer");
                        let n = spill_local_bufs[epoch_idx].len() as u64;
                        if let Err(e) = writer
                            .write_all(&spill_local_bufs[epoch_idx])
                            .map_err(|e| format!("spill final write: {e}"))
                        {
                            *err_ref
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(e);
                            return;
                        }
                        spill_bytes_ref.fetch_add(n, Ordering::Relaxed);
                        spill_local_bufs[epoch_idx].clear();
                    }
                }
                for w_opt in &mut spill_writers {
                    if let Some(mut w) = w_opt.take() {
                        if let Err(e) = w.flush().map_err(|e| format!("spill flush: {e}"))
                        {
                            *err_ref
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(e);
                            return;
                        }
                    }
                }

                resolved_ref.fetch_add(local_resolved, Ordering::Relaxed);
            });
        }
    });

    if let Some(e) = s2_error.into_inner().unwrap_or(None) {
        return Err(e.into());
    }

    Ok(resolved_total.load(Ordering::Relaxed))
}

/// Drain spill files for one epoch into the freshly-allocated scatter_bufs.
///
/// Per-file drain (option a in the design): N threads each claim spill files
/// via atomic index. There are `n_spill_files` spill files for this epoch
/// (one per worker_id from the producer phase), regardless of `n_workers`.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_epoch_drain(
    epoch_idx: usize,
    inputs: &Stage23EpochInputs<'_>,
    scatter_bufs: &[Mutex<Box<[u8]>>],
    slot_bucket_count: usize,
    n_workers: usize,
    n_spill_files: usize,
    spill_bytes_read: &AtomicU64,
) -> Result<()> {
    let next_file = AtomicUsize::new(0);
    let drain_error: Mutex<Option<String>> = Mutex::new(None);

    let scratch_ref = inputs.scratch;
    let total_slots = inputs.total_slots;
    let next_file_ref = &next_file;
    let drain_error_ref = &drain_error;
    let scatter_bufs_ref = scatter_bufs;
    let local_count = scatter_bufs.len();

    // bucket_lo for this epoch, derived from the same formula as
    // compute_epoch_ranges. Caller already validated num_epochs > 0.
    let num_epochs = inputs.num_epochs;
    let bucket_lo = (epoch_idx * slot_bucket_count) / num_epochs;

    std::thread::scope(|scope| {
        let n_threads = n_workers.min(n_spill_files).max(1);
        for _ in 0..n_threads {
            scope.spawn(move || {
                use std::sync::atomic::Ordering::Relaxed;
                let mut local_bufs: Vec<Vec<u8>> =
                    (0..local_count).map(|_| Vec::new()).collect();
                let mut read_buf: Vec<u8> = Vec::new();

                loop {
                    if drain_error_ref
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_some()
                    {
                        break;
                    }
                    let file_idx = next_file_ref.fetch_add(1, Relaxed);
                    if file_idx >= n_spill_files {
                        break;
                    }
                    let p = spill_path(scratch_ref, file_idx, epoch_idx);
                    let f = match std::fs::File::open(&p) {
                        Ok(f) => f,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(e) => {
                            *drain_error_ref
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(format!("open spill {}: {e}", p.display()));
                            return;
                        }
                    };
                    read_buf.clear();
                    if let Err(e) = std::io::Read::read_to_end(&mut &f, &mut read_buf) {
                        *drain_error_ref
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(format!("read spill {}: {e}", p.display()));
                        return;
                    }
                    if read_buf.len() % RESOLVED_ENTRY_SIZE != 0 {
                        *drain_error_ref
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(
                            format!(
                                "spill {} length {} not a multiple of {}",
                                p.display(),
                                read_buf.len(),
                                RESOLVED_ENTRY_SIZE
                            ),
                        );
                        return;
                    }
                    spill_bytes_read.fetch_add(read_buf.len() as u64, Relaxed);

                    // Group records by their per-epoch local bucket index,
                    // then flush each bucket's local buffer under its lock.
                    for chunk in read_buf.chunks_exact(RESOLVED_ENTRY_SIZE) {
                        let slot_pos = u64::from_le_bytes([
                            chunk[0], chunk[1], chunk[2], chunk[3],
                            chunk[4], chunk[5], chunk[6], chunk[7],
                        ]);
                        let entry = ResolvedEntry { slot_pos, lat: 0, lon: 0 };
                        let global_bucket =
                            entry.slot_bucket(total_slots, slot_bucket_count);
                        if global_bucket < bucket_lo
                            || global_bucket >= bucket_lo + local_count
                        {
                            *drain_error_ref
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(format!(
                                    "spill epoch {epoch_idx} contained slot_pos {slot_pos} \
                                     mapping to bucket {global_bucket} outside \
                                     [{bucket_lo}, {})",
                                    bucket_lo + local_count
                                ));
                            return;
                        }
                        let local_idx = global_bucket - bucket_lo;
                        local_bufs[local_idx].extend_from_slice(chunk);
                        if local_bufs[local_idx].len() >= FLUSH_THRESHOLD {
                            if let Err(e) = flush_local_to_scatter(
                                local_idx,
                                global_bucket,
                                &mut local_bufs[local_idx],
                                scatter_bufs_ref,
                                slot_bucket_count,
                                total_slots,
                            ) {
                                *drain_error_ref
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    Some(e);
                                return;
                            }
                        }
                    }
                }

                // Drain any non-empty locals.
                for local_idx in 0..local_count {
                    if local_bufs[local_idx].is_empty() {
                        continue;
                    }
                    let global_bucket = bucket_lo + local_idx;
                    if let Err(e) = flush_local_to_scatter(
                        local_idx,
                        global_bucket,
                        &mut local_bufs[local_idx],
                        scatter_bufs_ref,
                        slot_bucket_count,
                        total_slots,
                    ) {
                        *drain_error_ref
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(e);
                        return;
                    }
                }
            });
        }
    });

    if let Some(e) = drain_error.into_inner().unwrap_or(None) {
        return Err(e.into());
    }
    Ok(())
}

/// Run the emit pass for one epoch's scatter buffers.
///
/// Atomic claim over the epoch's bucket range. Each thread takes its own
/// `&mut WorkerTmpState` (BufWriter opened in append mode), classifies blobs
/// against the bucket, and calls `emit_integrated_intersections`.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_epoch_emit(
    epoch_idx: usize,
    epoch_ranges: &[(usize, usize)],
    scatter_bufs: &[Mutex<Box<[u8]>>],
    inputs: &Stage23EpochInputs<'_>,
    worker_tmps: &mut [WorkerTmpState],
    fully_contained_emitted: &[AtomicBool],
    n_workers: usize,
) -> Result<()> {
    let (bucket_lo, bucket_hi) = epoch_ranges[epoch_idx];
    let next_bucket = AtomicUsize::new(bucket_lo);
    let emit_error: Mutex<Option<String>> = Mutex::new(None);

    let next_bucket_ref = &next_bucket;
    let emit_error_ref = &emit_error;
    let scatter_bufs_ref = scatter_bufs;
    let total_slots = inputs.total_slots;
    let slot_bucket_count = inputs.slot_bucket_count;
    let way_slot_starts = inputs.way_slot_starts;
    let per_way_rcs = inputs.per_way_rcs;
    let straddler_slots = inputs.straddler_slots;
    let fully_contained_ref = fully_contained_emitted;

    // Dummy counters required by emit_integrated_intersections; the prototype
    // does not surface these per-epoch.
    let dummy_encode_ms = AtomicU64::new(0);
    let dummy_straddler_copy_ms = AtomicU64::new(0);
    let dummy_worker_tmp_bytes = AtomicU64::new(0);

    let n_threads = n_workers.min(worker_tmps.len()).max(1);

    // Split the worker_tmps into per-thread mutable slots.
    let worker_tmps_for_scope: Vec<&mut WorkerTmpState> =
        worker_tmps.iter_mut().take(n_threads).collect();

    std::thread::scope(|scope| {
        for state in worker_tmps_for_scope {
            let dummy_encode_ref = &dummy_encode_ms;
            let dummy_straddler_ref = &dummy_straddler_copy_ms;
            let dummy_tmp_bytes_ref = &dummy_worker_tmp_bytes;
            scope.spawn(move || {
                use std::sync::atomic::Ordering::Relaxed;

                // Open the worker tmp in append mode for this epoch.
                let f = match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&state.path)
                {
                    Ok(f) => f,
                    Err(e) => {
                        *emit_error_ref
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(format!("open worker tmp {}: {e}", state.path.display()));
                        return;
                    }
                };
                let mut tmp_writer = std::io::BufWriter::with_capacity(512 * 1024, f);
                let mut encode_scratch: Vec<u8> = Vec::new();

                loop {
                    if emit_error_ref
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_some()
                    {
                        break;
                    }
                    let bucket_idx = next_bucket_ref.fetch_add(1, Relaxed);
                    if bucket_idx >= bucket_hi {
                        break;
                    }
                    let (bucket_start, bucket_end) =
                        bucket_extents(bucket_idx, slot_bucket_count, total_slots);

                    let result: std::result::Result<(), String> = (|| {
                        let intersections = classify_blobs_in_bucket(
                            bucket_start,
                            bucket_end,
                            way_slot_starts,
                            total_slots,
                        )
                        .map_err(|e| {
                            format!("classify bucket {bucket_idx} (epoch {epoch_idx}): {e}")
                        })?;

                        // Per-blob at-most-one-FullyContained guard.
                        for inter in &intersections {
                            if let super::blob_bucket_index::BlobBucketIntersection::FullyContained {
                                blob_idx,
                            } = inter
                            {
                                if fully_contained_ref[*blob_idx]
                                    .swap(true, Ordering::Relaxed)
                                {
                                    return Err(format!(
                                        "invariant violation: blob {blob_idx} would receive a \
                                         second FullyContained ManifestEntry in epoch {epoch_idx} \
                                         bucket {bucket_idx}"
                                    ));
                                }
                            }
                        }

                        let scatter_idx_in_epoch = bucket_idx - bucket_lo;
                        let guard = scatter_bufs_ref[scatter_idx_in_epoch]
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let ctx = IntegratedInputs {
                            way_slot_starts,
                            per_way_rcs,
                            worker_tmp_paths: &[],
                            straddler_slots,
                        };
                        emit_integrated_intersections(
                            &intersections,
                            &guard[..],
                            bucket_start,
                            total_slots,
                            &ctx,
                            &mut encode_scratch,
                            &mut state.manifest,
                            &mut state.byte_pos,
                            &mut tmp_writer,
                            dummy_encode_ref,
                            dummy_straddler_ref,
                            dummy_tmp_bytes_ref,
                        )?;
                        Ok(())
                    })();

                    if let Err(e) = result {
                        *emit_error_ref
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e);
                        break;
                    }
                }

                if let Err(e) = tmp_writer
                    .flush()
                    .map_err(|e| format!("flush worker tmp: {e}"))
                {
                    *emit_error_ref
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e);
                }
            });
        }
    });

    if let Some(e) = emit_error.into_inner().unwrap_or(None) {
        return Err(e.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_ranges_evenly_partition() {
        let r = compute_epoch_ranges(4, 256);
        assert_eq!(r, vec![(0, 64), (64, 128), (128, 192), (192, 256)]);
    }

    #[test]
    fn epoch_ranges_handle_indivisible() {
        let r = compute_epoch_ranges(4, 255);
        // Last epoch must absorb the remainder.
        assert_eq!(r.last().expect("non-empty").1, 255);
        let total: usize = r.iter().map(|(lo, hi)| hi - lo).sum();
        assert_eq!(total, 255);
    }

    #[test]
    fn bucket_epoch_table_round_trip() {
        let r = compute_epoch_ranges(4, 8);
        let be = compute_bucket_epoch(&r, 8);
        assert_eq!(be, vec![0u8, 0, 1, 1, 2, 2, 3, 3]);
    }

    // Note: parse_epoch_env() is exercised end-to-end by the Denmark
    // correctness run; unit tests would need to mutate process-global env
    // (now unsafe in Rust 2024), so we deliberately don't add isolated
    // env-var unit tests here.
}
