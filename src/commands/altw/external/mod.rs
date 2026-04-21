//! External join for add-locations-to-ways: bounded-memory coordinate resolution
//! via double radix permutation.
//!
//! Instead of building a giant random-access node index (16 GB mmap at planet
//! scale), this module pre-computes the way-node join using sequential I/O and
//! bounded memory:
//!
//! 1. **Way pass**: stream ways, emit `(node_id, slot_pos)` COO pairs into
//!    256 node buckets partitioned by high bits of node_id.
//! 2. **Node join**: per bucket, sort pairs by node_id in RAM (~500 MB),
//!    merge-join with matching node stream, emit `(slot_pos, lat, lon)` into
//!    256 slot buckets partitioned by high bits of slot_pos.
//! 3. **Slot reorder**: per bucket, sort by slot_pos, emit final blob-ordered
//!    delta-varint `coord_payloads` (see `coord_payloads.rs`). The flat
//!    `coord_slots` array is a historical intermediate, retired in 2026-04.
//! 4. **Assembly**: stream original PBF + per-blob coord_payloads preads,
//!    emit enriched ways.
//!
//! Memory at every stage: <1 GB. All I/O sequential. No mmap, no random access.
//! See `notes/altw-partitioned.md` for the full design.

use std::path::Path;

use crate::writer::Compression;
use crate::ElementReader;

use super::Stats;
use crate::commands::{require_indexdata, HeaderOverrides};
use crate::BoxResult as Result;

mod blob_meta;
mod coord_payloads;
mod radix;
mod relation_scan;
mod stage1;
mod stage2;
mod stage3;
mod stage4;

use radix::{ScratchDir, NUM_BUCKETS};

use stage1::stage1_way_pass;
use stage2::{stage2_node_join, SharedBlobGroups};
use stage3::{stage3_blob_group_emit, IntegratedInputs, BlobGroupRef};
use stage4::stage4_assembly;

/// Maximum node ID in current OSM data. Used to compute bucket ranges.
/// 14B gives headroom above the current ~13B maximum.
pub(super) const MAX_NODE_ID: u64 = 14_000_000_000;

/// Size of a rank-occurrence record: `(local_rank: u32, slot_pos: u64)` = 12 bytes.
pub(super) const RANK_RECORD_SIZE: usize = 12;

/// Size of a resolved entry: `(blob_idx: u32, blob_local_slot: u32, lat: i32, lon: i32)` = 16 bytes.
///
/// Post-#6: this is the stage-2-to-stage-3 record written to blob-group
/// files. 16 bytes (up from the pre-#6 12-byte slot-bucket record)
/// because we carry `blob_idx` explicitly so stage 3 can group entries
/// by blob without a binary search on `way_slot_starts` per record.
pub(super) const RESOLVED_ENTRY_SIZE: usize = 16;

/// Size of a coordinate slot: `(lat: i32, lon: i32)` = 8 bytes.
pub(super) const COORD_SLOT_SIZE: usize = 8;

/// Stage 1 → stage 2 hand-off describing one node blob: where it lives in
/// the input PBF and the half-open rank range `[ref_rank_start, ref_rank_end)`
/// of referenced nodes it contains.
///
/// Computed without decoding any blob - uses indexdata `(min_id, max_id)`
/// plus `IdSet::rank` queries. Adjacent blobs' ranges are
/// non-overlapping and monotonic in rank (because the input PBF is sorted
/// by node ID and rank is monotonic in ID). Each rank bucket maps to a
/// contiguous run of blobs in this vector via binary search.
#[derive(Clone, Copy, Debug)]
pub(super) struct NodeBlobInfo {
    pub data_offset: u64,
    pub data_size: usize,
    pub ref_rank_start: u64,
    pub ref_rank_end: u64,
}

impl NodeBlobInfo {
    pub fn ref_count(&self) -> u64 {
        self.ref_rank_end - self.ref_rank_start
    }
}

/// A rank-bucketed occurrence record. `local_rank` is the rank offset
/// within the bucket (`global_rank - bucket_rank_start`), stored as u32
/// (max ~40M entries per bucket at planet, well under u32::MAX).
///
/// Post-#6 downstream re-keying: the record carries
/// `(blob_idx, blob_local_slot)` instead of `slot_pos`. Same 12-byte
/// size as the pre-#6 `(local_rank, slot_pos)` layout - we're
/// decomposing `slot_pos` into its natural `(blob_idx, blob_local_slot)`
/// form so stage 2 can route resolved entries to blob groups without
/// a binary search on `way_slot_starts`. `slot_pos` is reconstructable
/// as `way_slot_starts[blob_idx] + blob_local_slot` if ever needed
/// downstream, but the rewrite path consumes `blob_idx` directly.
#[derive(Clone, Copy)]
pub(super) struct RankRecord {
    local_rank: u32,
    blob_idx: u32,
    blob_local_slot: u32,
}

impl RankRecord {
    fn write_to(&self, buf: &mut [u8; RANK_RECORD_SIZE]) {
        buf[..4].copy_from_slice(&self.local_rank.to_le_bytes());
        buf[4..8].copy_from_slice(&self.blob_idx.to_le_bytes());
        buf[8..12].copy_from_slice(&self.blob_local_slot.to_le_bytes());
    }
}

/// A resolved coordinate ready to be scattered into a blob group for
/// stage 3's per-blob coord_payloads emission. 16 bytes on disk (vs
/// the pre-#6 12-byte slot-bucket record): carries `blob_idx`
/// explicitly so stage 3 can group by blob without re-deriving it.
#[derive(Clone, Copy)]
pub(super) struct ResolvedEntry {
    blob_idx: u32,
    blob_local_slot: u32,
    lat: i32,
    lon: i32,
}

impl ResolvedEntry {
    fn write_to(&self, buf: &mut [u8; RESOLVED_ENTRY_SIZE]) {
        buf[..4].copy_from_slice(&self.blob_idx.to_le_bytes());
        buf[4..8].copy_from_slice(&self.blob_local_slot.to_le_bytes());
        buf[8..12].copy_from_slice(&self.lat.to_le_bytes());
        buf[12..16].copy_from_slice(&self.lon.to_le_bytes());
    }

}

/// Default blob-group count for the stage-2-to-stage-3 intermediate.
/// Separate from `radix::NUM_BUCKETS` (the rank-bucket count for stage
/// 1's output) - they happen to share a value today but mean different
/// things. Revisit via the `s3_group_*` balance counters if tuning.
pub(super) const BLOB_GROUP_COUNT: usize = 256;

/// Precomputed blob-to-group mapping. Blobs are assigned to contiguous
/// groups based on cumulative slot count rather than raw blob-index
/// count, so each group carries roughly equal stage-2 output bytes /
/// stage-3 encode work even when per-blob ref counts are wildly
/// uneven (a real concern at planet scale where some way blobs are
/// densely populated and others are nearly empty).
///
/// `group_of_blob[blob_idx]` is the group a given blob belongs to
/// (O(1) lookup at stage 2's emission hot loop). `group_blob_lo[g]`
/// is the first `blob_idx` in group `g`; `group_blob_lo[g + 1]` is
/// the exclusive end.
pub(super) struct BlobGroupMap {
    pub(super) group_of_blob: Vec<u32>,
    pub(super) group_blob_lo: Vec<u32>,
    pub(super) group_count: usize,
}

impl BlobGroupMap {
    /// Walk `way_slot_starts` and assign each blob to the current
    /// group until the cumulative slot count crosses `(g + 1) *
    /// target_per_group`. The last group absorbs the remainder.
    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn build(
        way_slot_starts: &[u64],
        total_slots: u64,
        group_count: usize,
    ) -> Self {
        let num_blobs = way_slot_starts.len() as u32;
        if num_blobs == 0 || group_count == 0 {
            return Self {
                group_of_blob: Vec::new(),
                group_blob_lo: vec![0; group_count.saturating_add(1)],
                group_count,
            };
        }
        let target = total_slots.div_ceil(group_count as u64).max(1);
        let mut group_of_blob: Vec<u32> = vec![0; num_blobs as usize];
        let mut group_blob_lo: Vec<u32> = Vec::with_capacity(group_count + 1);
        group_blob_lo.push(0);

        let mut cur_group: u32 = 0;
        let mut next_boundary = target; // exclusive upper bound on cumulative slots for cur_group

        for blob_idx in 0..num_blobs {
            let blob_end_slots = way_slot_starts
                .get(blob_idx as usize + 1)
                .copied()
                .unwrap_or(total_slots);
            // Advance group if we've crossed the boundary AND there's
            // at least one blob already in the current group, AND we
            // still have groups left to assign.
            while cur_group + 1 < group_count as u32
                && blob_end_slots > next_boundary
                && group_blob_lo.len() > cur_group as usize
                && blob_idx > *group_blob_lo.last().unwrap_or(&0)
            {
                cur_group += 1;
                group_blob_lo.push(blob_idx);
                next_boundary = next_boundary.saturating_add(target);
            }
            group_of_blob[blob_idx as usize] = cur_group;
        }
        // Finalize: pad group_blob_lo out to group_count + 1 with the
        // remainder assigned to whatever the last cur_group became, so
        // all reserved group slots point somewhere sensible.
        while group_blob_lo.len() <= group_count {
            group_blob_lo.push(num_blobs);
        }
        Self {
            group_of_blob,
            group_blob_lo,
            group_count,
        }
    }

    pub(super) fn blob_range(&self, group_idx: usize) -> (u32, u32) {
        let lo = self.group_blob_lo[group_idx];
        let hi = self.group_blob_lo[group_idx + 1];
        (lo, hi)
    }
}

/// Run the full external join pipeline for add-locations-to-ways.
///
/// Bounded memory (<1 GB), all sequential I/O. Uses ~224 GB temp disk at
/// planet scale. See module docs for the algorithm.
#[hotpath::measure]
#[allow(clippy::too_many_lines)]
pub fn external_join(
    input: &Path,
    output: &Path,
    keep_untagged_nodes: bool,
    compression: Compression,
    direct_io: bool,
    force: bool,
    overrides: &HeaderOverrides,
) -> Result<Stats> {
    require_indexdata(
        input,
        direct_io,
        force,
        "external join requires indexdata for efficient blob filtering",
    )?;

    {
        let reader = ElementReader::open(input, direct_io)?;
        if !reader.header().is_sorted() {
            return Err("external join requires a sorted PBF (Sort.Type_then_ID). \
                        The single-pass node merge depends on ascending node ID order."
                .into());
        }
    }

    let scratch_dir =
        ScratchDir::new(output.parent().unwrap_or(Path::new(".")), "external-join")?;

    let ref_count_sidecar = scratch_dir.file_path("way-ref-counts");
    let per_way_refcount_sidecar = scratch_dir.file_path("per-way-refcounts");

    crate::debug::emit_marker("EXTJOIN_META_SCAN_START");
    let t_meta = std::time::Instant::now();
    let blob_meta = blob_meta::scan_blob_metadata(input, !keep_untagged_nodes)?;
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    {
        crate::debug::emit_counter("extjoin_meta_scan_ms", t_meta.elapsed().as_millis() as i64);
        crate::debug::emit_counter("extjoin_meta_blobs", blob_meta.len() as i64);
        crate::debug::emit_counter(
            "extjoin_meta_tag_scan_enabled",
            if keep_untagged_nodes { 0 } else { 1 },
        );
    }
    crate::debug::emit_marker("EXTJOIN_META_SCAN_END");

    // Stage 1: produces total_slots, unique_nodes, rank_bucket_counts,
    // num_shard_workers, the live IdSet (kept alive through stage 2
    // for inline coord resolution), and the per-blob rank mapping.
    //
    // #9 layer 2: relation member-id scan runs concurrently with stage 1.
    // The scan reads relation blobs only (via blob_meta) and shares no
    // state with stage 1 - both read from the same input PBF via pread
    // (`File: Sync` on Unix) with no locking. On Europe the scan takes
    // ~4 s; it fits entirely inside stage 1's ~43 s wall, so the serial
    // gap the scan used to create between stage 2 and stage 4 goes away.
    crate::debug::emit_marker("EXTJOIN_STAGE1_START");
    let (s1_minflt_before, s1_majflt_before) = crate::debug::read_page_faults();

    let input_ref_parallel: &Path = input;
    let blob_meta_ref_parallel = &blob_meta;
    let (stage1_out, relation_member_node_ids) = std::thread::scope(
        |scope| -> std::result::Result<(super::external::stage1::Stage1Output, Option<crate::idset::IdSet>), String> {
            let s1_handle = scope.spawn(|| {
                stage1_way_pass(
                    blob_meta_ref_parallel,
                    input_ref_parallel,
                    direct_io,
                    &scratch_dir,
                    &ref_count_sidecar,
                    &per_way_refcount_sidecar,
                )
                .map_err(|e| e.to_string())
            });
            let rel_handle = if keep_untagged_nodes {
                None
            } else {
                crate::debug::emit_marker("EXTJOIN_RELATION_SCAN_START");
                Some(scope.spawn(move || {
                    let t_relscan = std::time::Instant::now();
                    let ids = relation_scan::collect_relation_member_node_ids_indexed(
                        input_ref_parallel,
                        blob_meta_ref_parallel,
                    )
                    .map_err(|e| e.to_string())?;
                    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
                    crate::debug::emit_counter(
                        "extjoin_relation_member_collect_ms",
                        t_relscan.elapsed().as_millis() as i64,
                    );
                    crate::debug::emit_marker("EXTJOIN_RELATION_SCAN_END");
                    Ok::<_, String>(ids)
                }))
            };

            let s1_res = s1_handle
                .join()
                .map_err(|_| "stage 1 thread panicked".to_string())??;
            let rel_res = match rel_handle {
                Some(handle) => Some(
                    handle
                        .join()
                        .map_err(|_| "relation scan thread panicked".to_string())??,
                ),
                None => None,
            };
            Ok((s1_res, rel_res))
        },
    )
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let (s1_minflt_after, s1_majflt_after) = crate::debug::read_page_faults();
    let total_coo: u64 = stage1_out.rank_bucket_counts.iter().sum();
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("extjoin_total_slots", stage1_out.total_slots as i64);
        crate::debug::emit_counter("extjoin_total_coo", total_coo as i64);
        crate::debug::emit_counter("extjoin_unique_nodes", stage1_out.unique_nodes as i64);
        crate::debug::emit_counter("s1_minflt_delta", (s1_minflt_after - s1_minflt_before) as i64);
        crate::debug::emit_counter("s1_majflt_delta", (s1_majflt_after - s1_majflt_before) as i64);
    }
    crate::debug::emit_marker("EXTJOIN_STAGE1_END");

    let total_slots = stage1_out.total_slots;
    let unique_nodes = stage1_out.unique_nodes;
    let rank_bucket_counts = stage1_out.rank_bucket_counts;
    let num_shard_workers = stage1_out.num_shard_workers;
    let mut node_id_set = stage1_out.node_id_set;
    let node_blob_mapping = stage1_out.node_blob_mapping;

    // Stage 2 only needs membership bits (`get()`) now that per-node
    // `rank_if_set()` is replaced by a blob-local rank counter seeded from
    // `NodeBlobInfo.ref_rank_start`. Drop the rank-prefix metadata (~100 MB
    // at planet scale) before stage 2 starts so it doesn't pollute cache
    // through the hot decode loop.
    node_id_set.drop_rank_index();

    // Post-#6: blob-group partitioning replaces slot-bucket partitioning.
    // `BlobGroupMap` assigns each blob to a contiguous group based on
    // cumulative slot count so each group carries roughly equal stage-2
    // output bytes. No max_blob_slots / straddler constraint any more -
    // blobs are wholly contained in one group by construction.
    let way_slot_starts =
        stage4::load_ref_count_sidecar(&ref_count_sidecar, total_slots)?;
    let blob_group_map = BlobGroupMap::build(
        &way_slot_starts,
        total_slots,
        BLOB_GROUP_COUNT,
    );
    let total_rank_shard_files = num_shard_workers * NUM_BUCKETS;
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("extjoin_rank_bucket_count", NUM_BUCKETS as i64);
        crate::debug::emit_counter("extjoin_blob_group_count", BLOB_GROUP_COUNT as i64);
        crate::debug::emit_counter("extjoin_num_way_blobs", way_slot_starts.len() as i64);
        crate::debug::emit_counter("extjoin_num_shard_workers", num_shard_workers as i64);
        crate::debug::emit_counter("extjoin_total_rank_shard_files", total_rank_shard_files as i64);

        // Balance diagnostic: min / max blobs per group + min / max
        // cumulative slots per group. Wide spreads indicate the
        // cumulative-slot-based partition collapsed on some skew in
        // `way_slot_starts` - e.g., a single very fat blob that
        // dominates a whole group. Preserved across runs via the
        // sidecar so regressions in BlobGroupMap::build are visible
        // in `brokkr sidecar --counters` without re-running.
        let mut min_blobs = u32::MAX;
        let mut max_blobs = 0u32;
        let mut min_slots = u64::MAX;
        let mut max_slots = 0u64;
        for g in 0..BLOB_GROUP_COUNT {
            let (lo, hi) = blob_group_map.blob_range(g);
            let blobs = hi.saturating_sub(lo);
            min_blobs = min_blobs.min(blobs);
            max_blobs = max_blobs.max(blobs);
            let slot_lo = way_slot_starts
                .get(lo as usize)
                .copied()
                .unwrap_or(total_slots);
            let slot_hi = way_slot_starts
                .get(hi as usize)
                .copied()
                .unwrap_or(total_slots);
            let slots = slot_hi.saturating_sub(slot_lo);
            min_slots = min_slots.min(slots);
            max_slots = max_slots.max(slots);
        }
        crate::debug::emit_counter("s2_group_min_blobs", i64::from(min_blobs));
        crate::debug::emit_counter("s2_group_max_blobs", i64::from(max_blobs));
        crate::debug::emit_counter("s2_group_min_slots", min_slots as i64);
        crate::debug::emit_counter("s2_group_max_slots", max_slots as i64);
    }

    crate::debug::emit_marker("EXTJOIN_STAGE2_START");
    let (s2_minflt_before, s2_majflt_before) = crate::debug::read_page_faults();
    let blob_groups = SharedBlobGroups::create(&scratch_dir, BLOB_GROUP_COUNT)?;
    let input_pbf = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("open input pbf for stage 2: {e}"))?,
    );
    let resolved_count = stage2_node_join(
        &scratch_dir,
        &rank_bucket_counts,
        num_shard_workers,
        &blob_groups,
        &blob_group_map,
        unique_nodes,
        &input_pbf,
        &node_id_set,
        &node_blob_mapping,
    )?;
    blob_groups.finish()?;
    let (s2_minflt_after, s2_majflt_after) = crate::debug::read_page_faults();
    for worker_id in 0..num_shard_workers {
        for bucket_idx in 0..NUM_BUCKETS {
            let path = scratch_dir
                .path
                .join(format!("rank-W{worker_id}-{bucket_idx:03}"));
            drop(std::fs::remove_file(&path));
        }
    }
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("extjoin_resolved_count", resolved_count as i64);
        crate::debug::emit_counter("s2_minflt_delta", (s2_minflt_after - s2_minflt_before) as i64);
        crate::debug::emit_counter("s2_majflt_delta", (s2_majflt_after - s2_majflt_before) as i64);
    }
    crate::debug::emit_marker("EXTJOIN_STAGE2_END");

    // Free the IdSet (~2 GB RSS at planet) and the per-blob mapping
    // - both were stage 2 inputs only, nothing downstream reads them.
    drop(node_id_set);
    drop(node_blob_mapping);

    // Prepare inputs for the streaming stage 3 + stage 4 handoff.
    let per_way_rcs = coord_payloads::load_per_way_refcount_sidecar_indexed(
        &per_way_refcount_sidecar,
        way_slot_starts.len(),
    )?;
    // Worker count: back off from the pre-streaming `.min(6)` because
    // stage 3 and stage 4 worker buffers are now both resident at the
    // same time (they overlap). See notes/altw-structural-reports.md #2
    // "Worker budgets under overlap".
    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4)
        .min(4);
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("s3_worker_count", num_workers as i64);

    // Worker tmp files opened once here with read + write, wrapped in
    // Arc<File> so stage 3 can `write_all_at` and stage 4 can
    // `read_exact_at` on the same `&File`. `File` is `Sync` on Unix for
    // pread/pwrite so no extra locking is needed.
    let worker_tmp_paths: Vec<std::path::PathBuf> = (0..num_workers)
        .map(|i| scratch_dir.file_path(&format!("payloads-W{i}")))
        .collect();
    let worker_files: Vec<std::sync::Arc<std::fs::File>> = worker_tmp_paths
        .iter()
        .map(|p| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(p)
                .map(std::sync::Arc::new)
                .map_err(|e| format!("open worker tmp {p:?}: {e}"))
        })
        .collect::<std::result::Result<_, String>>()?;

    // Post-#6: stage 3 reads blob-group files (256 of them) each
    // covering a contiguous blob_idx range; no slot-bucket files any
    // more. Stage 3 iterates all blobs in its assigned group's range,
    // allocates per-blob coord_slices, scatters entries, delta-varint-
    // encodes, and publishes to the router.
    let group_entry_counts: Vec<u64> = (0..BLOB_GROUP_COUNT)
        .map(|i| {
            let path = scratch_dir.bucket_path("group", i);
            std::fs::metadata(&path)
                .map(|m| m.len() / RESOLVED_ENTRY_SIZE as u64)
                .unwrap_or(0)
        })
        .collect();
    let group_paths: Vec<std::path::PathBuf> = (0..BLOB_GROUP_COUNT)
        .map(|i| scratch_dir.bucket_path("group", i))
        .collect();
    let blob_group_ref = BlobGroupRef {
        paths: group_paths,
        entry_counts: group_entry_counts,
    };

    // The streaming router: pre-populates `Empty` entries for zero-ref
    // way blobs so stage 4 never waits on a blob that stage 3 would
    // never publish.
    let router = coord_payloads::ConcurrentBlobLocationRouter::new(
        &per_way_rcs,
        worker_files.clone(),
    )?;

    // (#9 layer 2: relation scan already ran in parallel with stage 1
    // above; `relation_member_node_ids` is already bound. No serial
    // scan between stage 2 and stage 4.)

    // Streaming stage 3 + stage 4: run concurrently via a single
    // `thread::scope`. Stage 3 publishes per-blob entries to the router
    // as it encodes them; stage 4 workers block on `router.wait_ready`
    // ahead of any input pread so they never hold decompressed state
    // while waiting.
    crate::debug::emit_marker("EXTJOIN_STREAMING_START");
    crate::debug::emit_marker("EXTJOIN_STAGE3_START");
    crate::debug::emit_marker("EXTJOIN_STAGE4_START");
    let (s3_minflt_before, s3_majflt_before) = crate::debug::read_page_faults();

    let router_ref = &router;
    let per_way_rcs_ref = &per_way_rcs;
    let blob_meta_ref = &blob_meta;
    let way_slot_starts_ref = way_slot_starts.as_slice();
    let rel_ids_ref = relation_member_node_ids.as_ref();
    let blob_group_ref_ref = &blob_group_ref;
    let blob_group_map_ref = &blob_group_map;

    // Closures return Result<_, String> because BoxResult's error type
    // (Box<dyn Error>) is not Send and thread::scope requires Send
    // return values. Errors are stringified at the scope boundary and
    // converted back to BoxResult outside.
    let mut stats = std::thread::scope(|scope| -> std::result::Result<Stats, String> {
        let s3_handle = scope.spawn(move || -> std::result::Result<(), String> {
            let integrated = IntegratedInputs {
                way_slot_starts: way_slot_starts_ref,
                per_way_rcs: per_way_rcs_ref,
                router: router_ref,
                blob_group_map: blob_group_map_ref,
            };
            let result = stage3_blob_group_emit(
                blob_group_ref_ref,
                BLOB_GROUP_COUNT,
                total_slots,
                &integrated,
            )
            .map_err(|e| e.to_string());
            // Signal the router that no more publishes are coming. Must
            // run whether stage 3 succeeded or errored - otherwise stage
            // 4 waiters on unpublished slots would hang. On error the
            // worker has already called `router.abort`, but
            // mark_producer_done is idempotent with abort and cheap.
            router_ref.mark_producer_done();
            result
        });
        let s4_handle = scope.spawn(move || -> std::result::Result<Stats, String> {
            stage4_assembly(
                input,
                output,
                blob_meta_ref,
                router_ref,
                per_way_rcs_ref,
                way_slot_starts_ref,
                keep_untagged_nodes,
                rel_ids_ref,
                compression,
                direct_io,
                overrides,
            )
            .map_err(|e| e.to_string())
        });

        let s3_res = s3_handle
            .join()
            .map_err(|_| "stage 3 thread panicked".to_string())?;
        let s4_res = s4_handle
            .join()
            .map_err(|_| "stage 4 thread panicked".to_string())?;

        // Prefer the stage 3 error if both failed (it's usually the root
        // cause - stage 4 typically errors only because of an abort that
        // stage 3 or the writer raised).
        s3_res?;
        s4_res
    })
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let (s3_minflt_after, s3_majflt_after) = crate::debug::read_page_faults();
    for i in 0..BLOB_GROUP_COUNT {
        drop(std::fs::remove_file(scratch_dir.bucket_path("group", i)));
    }
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("s3_minflt_delta", (s3_minflt_after - s3_minflt_before) as i64);
        crate::debug::emit_counter("s3_majflt_delta", (s3_majflt_after - s3_majflt_before) as i64);
    }
    crate::debug::emit_marker("EXTJOIN_STAGE4_END");
    crate::debug::emit_marker("EXTJOIN_STAGE3_END");
    crate::debug::emit_marker("EXTJOIN_STREAMING_END");

    // Emit router stats. Post-#6: no straddler counts (blobs are fully
    // contained in one group by construction).
    {
        let s = router.stats.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        #[allow(clippy::cast_possible_wrap)]
        {
            crate::debug::emit_counter("s3_router_num_worker", s.num_worker as i64);
            crate::debug::emit_counter("s3_router_num_empty", s.num_empty as i64);
            crate::debug::emit_counter("s3_router_worker_bytes", s.worker_bytes as i64);
        }
        eprintln!(
            "[coord_payloads] streaming router {} way blobs ({} worker / {} empty), \
             {} MB in worker tmps",
            router.num_blobs(),
            s.num_worker,
            s.num_empty,
            s.worker_bytes / 1_000_000,
        );
    }

    stats.missing_locations = total_slots.saturating_sub(resolved_count);

    drop(scratch_dir);

    Ok(stats)
}
