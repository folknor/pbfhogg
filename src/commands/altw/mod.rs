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

use super::add_locations_to_ways::Stats;
use super::external_radix::{ScratchDir, NUM_BUCKETS};
use super::{require_indexdata, HeaderOverrides, Result};

mod blob_bucket_index;
mod blob_meta;
mod coord_payloads;
mod relation_scan;
mod stage1;
mod stage2;
mod stage3;
mod stage4;

use stage1::stage1_way_pass;
use stage2::{stage2_node_join, SlotBuckets};
use stage3::{stage3_slot_reorder, IntegratedInputs, SlotBucketRef};
use stage4::stage4_assembly;

/// Maximum node ID in current OSM data. Used to compute bucket ranges.
/// 14B gives headroom above the current ~13B maximum.
pub(super) const MAX_NODE_ID: u64 = 14_000_000_000;

/// Size of a rank-occurrence record: `(local_rank: u32, slot_pos: u64)` = 12 bytes.
pub(super) const RANK_RECORD_SIZE: usize = 12;

/// Size of a resolved entry: `(local_slot_pos: u32, lat: i32, lon: i32)` = 12 bytes.
pub(super) const RESOLVED_ENTRY_SIZE: usize = 12;

/// Size of a coordinate slot: `(lat: i32, lon: i32)` = 8 bytes.
pub(super) const COORD_SLOT_SIZE: usize = 8;

/// Stage 1 → stage 2 hand-off describing one node blob: where it lives in
/// the input PBF and the half-open rank range `[ref_rank_start, ref_rank_end)`
/// of referenced nodes it contains.
///
/// Computed without decoding any blob - uses indexdata `(min_id, max_id)`
/// plus `IdSetDense::rank` queries. Adjacent blobs' ranges are
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
/// `slot_pos` is the linear position within the conceptual flat coord
/// stream (way_order × ref_order); stage 3 emits these as per-blob
/// delta-varint payloads in `coord_payloads` rather than a flat array.
///
/// 12 bytes instead of 16: 25% I/O reduction across stages 1B and 2.
#[derive(Clone, Copy)]
pub(super) struct RankRecord {
    local_rank: u32,
    slot_pos: u64,
}

impl RankRecord {
    fn write_to(&self, buf: &mut [u8; RANK_RECORD_SIZE]) {
        buf[..4].copy_from_slice(&self.local_rank.to_le_bytes());
        buf[4..12].copy_from_slice(&self.slot_pos.to_le_bytes());
    }
}

/// A resolved coordinate ready to be scattered into a slot bucket for
/// stage 3's coord_payloads emission.
#[derive(Clone, Copy)]
pub(super) struct ResolvedEntry {
    slot_pos: u64,
    lat: i32,
    lon: i32,
}

impl ResolvedEntry {
    fn write_to(&self, bucket_start: u64, buf: &mut [u8; RESOLVED_ENTRY_SIZE]) {
        #[allow(clippy::cast_possible_truncation)]
        let local_slot_pos = (self.slot_pos - bucket_start) as u32;
        buf[..4].copy_from_slice(&local_slot_pos.to_le_bytes());
        buf[4..8].copy_from_slice(&self.lat.to_le_bytes());
        buf[8..12].copy_from_slice(&self.lon.to_le_bytes());
    }

    /// Bucket index for slot-pos partitioning.
    ///
    /// Uses floor division for `range_size` so the last bucket *absorbs*
    /// the remainder (and is wider than the others) instead of being
    /// truncated. This keeps every bucket's width ≥ `range_size`, which
    /// (together with the `slot_bucket_count = total_slots / max_blob_slots`
    /// floor in `external_join`) preserves the 2-piece straddler
    /// invariant for all input sizes. Out-of-range high slot_pos values
    /// (that would land past the nominal last bucket because the last
    /// is wider) get clamped to `slot_bucket_count - 1`.
    #[allow(clippy::cast_possible_truncation)]
    fn slot_bucket(&self, total_slots: u64, slot_bucket_count: usize) -> usize {
        let range_size = total_slots / slot_bucket_count as u64;
        if range_size == 0 {
            return 0;
        }
        let bucket = self.slot_pos / range_size;
        (bucket as usize).min(slot_bucket_count - 1)
    }
}

pub(super) fn slot_bucket_bounds(
    total_slots: u64,
    slot_bucket_count: usize,
    bucket_idx: usize,
) -> (u64, u64) {
    let range_size = total_slots / slot_bucket_count as u64;
    let bucket_start = bucket_idx as u64 * range_size;
    let bucket_end = if bucket_idx == slot_bucket_count - 1 {
        total_slots
    } else {
        ((bucket_idx as u64 + 1) * range_size).min(total_slots)
    };
    (bucket_start, bucket_end)
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
    // num_shard_workers, the live IdSetDense (kept alive through stage 2
    // for inline coord resolution), and the per-blob rank mapping.
    crate::debug::emit_marker("EXTJOIN_STAGE1_START");
    let (s1_minflt_before, s1_majflt_before) = crate::debug::read_page_faults();
    let stage1_out = stage1_way_pass(
        &blob_meta,
        input,
        direct_io,
        &scratch_dir,
        &ref_count_sidecar,
        &per_way_refcount_sidecar,
    )?;
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

    // Compute slot_bucket_count: scale down from NUM_BUCKETS so that
    // every bucket can fit at least one full blob's slot range. This
    // keeps the 2-piece straddler invariant (a blob spans at most two
    // adjacent buckets) for both planet-scale inputs and tiny test
    // fixtures where total_slots / NUM_BUCKETS would otherwise be < 1.
    let way_slot_starts =
        stage4::load_ref_count_sidecar(&ref_count_sidecar, total_slots)?;
    let max_blob_slots: u64 = (0..way_slot_starts.len())
        .map(|i| {
            let end = if i + 1 < way_slot_starts.len() {
                way_slot_starts[i + 1]
            } else {
                total_slots
            };
            end - way_slot_starts[i]
        })
        .max()
        .unwrap_or(0);
    // Each bucket must hold ≥ max_blob_slots so the SMALLEST bucket
    // (which can be smaller than range_size when total_slots is not
    // a multiple of bucket_count) still satisfies the 2-piece
    // straddler invariant. Equivalently: bucket_count ≤
    // total_slots / max_blob_slots, with floor division.
    #[allow(clippy::cast_possible_truncation)]
    let slot_bucket_count = total_slots
        .checked_div(max_blob_slots)
        .map(|n| n.max(1).min(NUM_BUCKETS as u64) as usize)
        .unwrap_or(NUM_BUCKETS);
    let total_rank_shard_files = num_shard_workers * NUM_BUCKETS;
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("extjoin_rank_bucket_count", NUM_BUCKETS as i64);
        crate::debug::emit_counter("extjoin_slot_bucket_count", slot_bucket_count as i64);
        crate::debug::emit_counter("extjoin_max_blob_slots", max_blob_slots as i64);
        crate::debug::emit_counter("extjoin_num_shard_workers", num_shard_workers as i64);
        crate::debug::emit_counter("extjoin_total_rank_shard_files", total_rank_shard_files as i64);
    }

    crate::debug::emit_marker("EXTJOIN_STAGE2_START");
    let (s2_minflt_before, s2_majflt_before) = crate::debug::read_page_faults();
    let slot_buckets = SlotBuckets::create(&scratch_dir, slot_bucket_count)?;
    let input_pbf = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("open input pbf for stage 2: {e}"))?,
    );
    let resolved_count = stage2_node_join(
        &scratch_dir,
        &rank_bucket_counts,
        num_shard_workers,
        &slot_buckets,
        slot_bucket_count,
        total_slots,
        unique_nodes,
        &input_pbf,
        &node_id_set,
        &node_blob_mapping,
    )?;
    slot_buckets.finish()?;
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

    // Free the IdSetDense (~2 GB RSS at planet) and the per-blob mapping
    // - both were stage 2 inputs only, nothing downstream reads them.
    drop(node_id_set);
    drop(node_blob_mapping);

    // Prepare integrated coord_payloads artifacts for stage 3.
    let per_way_rcs = coord_payloads::load_per_way_refcount_sidecar_indexed(
        &per_way_refcount_sidecar,
        way_slot_starts.len(),
    )?;
    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4)
        .min(6);
    let worker_tmp_paths: Vec<std::path::PathBuf> = (0..num_workers)
        .map(|i| scratch_dir.file_path(&format!("payloads-W{i}")))
        .collect();
    let straddler_slots: Vec<std::sync::Mutex<Option<coord_payloads::StraddlerSlot>>> =
        (0..way_slot_starts.len())
            .map(|_| std::sync::Mutex::new(None))
            .collect();

    crate::debug::emit_marker("EXTJOIN_STAGE3_START");
    let (s3_minflt_before, s3_majflt_before) = crate::debug::read_page_faults();
    let slot_entry_counts: Vec<u64> = (0..slot_bucket_count)
        .map(|i| {
            let path = scratch_dir.bucket_path("slot", i);
            std::fs::metadata(&path)
                .map(|m| m.len() / RESOLVED_ENTRY_SIZE as u64)
                .unwrap_or(0)
        })
        .collect();
    let slot_paths: Vec<std::path::PathBuf> = (0..slot_bucket_count)
        .map(|i| scratch_dir.bucket_path("slot", i))
        .collect();
    let slot_bucket_ref = SlotBucketRef {
        paths: slot_paths,
        entry_counts: slot_entry_counts,
    };
    let integrated_inputs = IntegratedInputs {
        way_slot_starts: way_slot_starts.as_slice(),
        per_way_rcs: &per_way_rcs,
        worker_tmp_paths: worker_tmp_paths.as_slice(),
        straddler_slots: straddler_slots.as_slice(),
    };
    let s3_result = stage3_slot_reorder(
        &slot_bucket_ref,
        slot_bucket_count,
        total_slots,
        &integrated_inputs,
    )?;
    let (s3_minflt_after, s3_majflt_after) = crate::debug::read_page_faults();
    for i in 0..slot_bucket_count {
        drop(std::fs::remove_file(scratch_dir.bucket_path("slot", i)));
    }
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("s3_minflt_delta", (s3_minflt_after - s3_minflt_before) as i64);
        crate::debug::emit_counter("s3_majflt_delta", (s3_majflt_after - s3_majflt_before) as i64);
    }
    crate::debug::emit_marker("EXTJOIN_STAGE3_END");

    let (coord_router, router_stats) = coord_payloads::build_blob_location_router(
        &per_way_rcs,
        s3_result.worker_manifests,
        &worker_tmp_paths,
        straddler_slots,
    )?;
    eprintln!(
        "[coord_payloads] router {} ms, {} way blobs ({} worker / {} straddler / {} empty), \
         {} MB in worker tmps + {} KB straddler bytes in RAM",
        router_stats.build_ms,
        router_stats.num_way_blobs,
        router_stats.num_worker,
        router_stats.num_straddlers,
        router_stats.num_empty,
        router_stats.worker_bytes / 1_000_000,
        router_stats.straddler_bytes / 1_000,
    );
    let stage4_per_way_rcs = per_way_rcs;

    let relation_member_node_ids = if keep_untagged_nodes {
        None
    } else {
        crate::debug::emit_marker("EXTJOIN_RELATION_SCAN_START");
        let t_relscan = std::time::Instant::now();
        let ids = relation_scan::collect_relation_member_node_ids_indexed(
            input, &blob_meta,
        )?;
        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
        crate::debug::emit_counter(
            "extjoin_relation_member_collect_ms",
            t_relscan.elapsed().as_millis() as i64,
        );
        crate::debug::emit_marker("EXTJOIN_RELATION_SCAN_END");
        Some(ids)
    };

    crate::debug::emit_marker("EXTJOIN_STAGE4_START");
    let (s4_minflt_before, s4_majflt_before) = crate::debug::read_page_faults();
    let mut stats = stage4_assembly(
        input,
        output,
        &blob_meta,
        &coord_router,
        &stage4_per_way_rcs,
        way_slot_starts.as_slice(),
        keep_untagged_nodes,
        relation_member_node_ids.as_ref(),
        compression,
        direct_io,
        overrides,
    )?;
    let (s4_minflt_after, s4_majflt_after) = crate::debug::read_page_faults();
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("s4_minflt_delta", (s4_minflt_after - s4_minflt_before) as i64);
        crate::debug::emit_counter("s4_majflt_delta", (s4_majflt_after - s4_majflt_before) as i64);
    }
    crate::debug::emit_marker("EXTJOIN_STAGE4_END");

    stats.missing_locations = total_slots.saturating_sub(resolved_count);

    drop(scratch_dir);

    Ok(stats)
}
