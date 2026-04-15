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
mod coord_payloads;
mod stage1;
mod stage2;
mod stage3;
mod stage4;

use stage1::{
    build_node_blob_mapping, build_way_schedule, stage1_pass_a, stage1_way_pass, Stage1Output,
};
use stage2::{stage2_node_join, SlotBuckets};
use stage3::{stage3_slot_reorder, IntegratedInputs, SlotBucketRef};
use stage4::stage4_assembly;

/// Maximum node ID in current OSM data. Used to compute bucket ranges.
/// 14B gives headroom above the current ~13B maximum.
pub(super) const MAX_NODE_ID: u64 = 14_000_000_000;

/// Size of a rank-occurrence record: `(local_rank: u32, slot_pos: u64)` = 12 bytes.
pub(super) const RANK_RECORD_SIZE: usize = 12;

/// Size of a resolved entry: `(slot_pos: u64, lat: i32, lon: i32)` = 16 bytes.
pub(super) const RESOLVED_ENTRY_SIZE: usize = 16;

/// Size of a coordinate slot: `(lat: i32, lon: i32)` = 8 bytes.
pub(super) const COORD_SLOT_SIZE: usize = 8;

/// Stage 1 → stage 2 hand-off describing one node blob: where it lives in
/// the input PBF and the half-open rank range `[ref_rank_start, ref_rank_end)`
/// of referenced nodes it contains.
///
/// Computed without decoding any blob — uses indexdata `(min_id, max_id)`
/// + `IdSetDense::rank` queries. Adjacent blobs' ranges are non-overlapping
/// and monotonic in rank (because the input PBF is sorted by node ID and
/// rank is monotonic in ID). Each rank bucket maps to a contiguous run of
/// blobs in this vector via binary search.
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
    fn write_to(&self, buf: &mut [u8; RESOLVED_ENTRY_SIZE]) {
        buf[..8].copy_from_slice(&self.slot_pos.to_le_bytes());
        buf[8..12].copy_from_slice(&self.lat.to_le_bytes());
        buf[12..16].copy_from_slice(&self.lon.to_le_bytes());
    }

    fn read_from(buf: &[u8; RESOLVED_ENTRY_SIZE]) -> Self {
        let slot_pos = u64::from_le_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ]);
        let lat = i32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let lon = i32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        Self { slot_pos, lat, lon }
    }

    /// Bucket index for slot-pos partitioning.
    ///
    /// Uses floor division for `range_size` so the last bucket *absorbs*
    /// the remainder (and is wider than the others) instead of being
    /// truncated. This keeps every bucket's width ≥ `range_size`, which
    /// — together with the `slot_bucket_count = total_slots / max_blob_slots`
    /// floor in `external_join` — preserves the 2-piece straddler
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

/// Run the full external join pipeline for add-locations-to-ways.
///
/// Bounded memory (<1 GB), all sequential I/O. Uses ~224 GB temp disk at
/// planet scale. See module docs for the algorithm.
#[hotpath::measure]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn external_join(
    input: &Path,
    output: &Path,
    keep_untagged_nodes: bool,
    compression: Compression,
    direct_io: bool,
    force: bool,
    overrides: &HeaderOverrides,
    keep_scratch: bool,
    start_stage: Option<u8>,
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

    let scratch_dir = if keep_scratch || start_stage.is_some() {
        ScratchDir::new_stable(output.parent().unwrap_or(Path::new(".")), "external-join")?
    } else {
        ScratchDir::new(output.parent().unwrap_or(Path::new(".")), "external-join")?
    };

    let manifest_path = scratch_dir.file_path("manifest");
    let ref_count_sidecar = scratch_dir.file_path("way-ref-counts");
    let per_way_refcount_sidecar = scratch_dir.file_path("per-way-refcounts");
    let start = start_stage.unwrap_or(1);

    // Manifest layout (LE): [u64 total_slots][u64 unique_nodes][u64 resolved_count?]
    //   - 16 bytes: only stage 1 completed in the prior keep-scratch run.
    //   - 24 bytes: stage 2 also completed; resolved_count is the value
    //     stage 2 returned for this dataset.
    // Used by --start-stage >= 2 resumes to recover the scalars stage 1
    // and stage 2 produced, avoiding a ~2 GB IdSetDense rebuild.
    // resolved_count is needed by Stats.missing_locations when stage 2
    // is skipped.
    let mut manifest_resolved_count: Option<u64> = None;

    // Stage 1 hand-off: total_slots, unique_nodes, rank_bucket_counts,
    // num_shard_workers, and optionally the live IdSetDense + per-blob
    // rank mapping when this invocation will run stage 2.
    let stage1_out: Stage1Output = if start >= 2 {
        let manifest = std::fs::read(&manifest_path)
            .map_err(|e| format!("read manifest for --start-stage: {e}. Run with --keep-scratch first."))?;
        if manifest.len() < 16 {
            return Err("manifest too small: expected >=16 bytes (total_slots + unique_nodes). \
                        Re-run with --keep-scratch from stage 1; the manifest layout changed."
                .into());
        }
        let total_slots = u64::from_le_bytes(manifest[..8].try_into()
            .map_err(|_| "manifest total_slots read failed")?);
        let _manifest_unique_nodes = u64::from_le_bytes(manifest[8..16].try_into()
            .map_err(|_| "manifest unique_nodes read failed")?);
        if manifest.len() >= 24 {
            manifest_resolved_count = Some(
                u64::from_le_bytes(manifest[16..24].try_into()
                    .map_err(|_| "manifest resolved_count read failed")?),
            );
        }

        // Recover rank_bucket_counts and num_shard_workers from scratch
        // file metadata.
        let mut rank_bucket_counts = vec![0u64; NUM_BUCKETS];
        let mut num_shard_workers = 0usize;
        loop {
            let path = scratch_dir.path.join(format!("rank-W{num_shard_workers}-000"));
            if path.exists() {
                num_shard_workers += 1;
            } else {
                break;
            }
        }
        num_shard_workers = num_shard_workers.max(1);
        for bucket_idx in 0..NUM_BUCKETS {
            for worker_id in 0..num_shard_workers {
                let path = scratch_dir.path.join(format!("rank-W{worker_id}-{bucket_idx:03}"));
                if let Ok(meta) = std::fs::metadata(&path) {
                    rank_bucket_counts[bucket_idx] += meta.len() / RANK_RECORD_SIZE as u64;
                }
            }
        }

        let mut node_id_set = None;
        let mut node_blob_mapping = None;
        let unique_nodes = if start <= 2 {
            // Stage 2 needs the IdSetDense for inline node-blob coord
            // resolution, but the prior keep-scratch run dropped it after
            // stage 1. Rebuild it by re-running pass A — the only stage-1
            // work that produces the set. Pass B's per-worker rank shard
            // files are already on disk and are not regenerated.
            eprintln!(
                "[altw] --start-stage {start}: rebuilding IdSetDense via pass A re-scan \
                 (prior keep-scratch run dropped it; needed for inline stage 2 coord lookup)"
            );
            let schedule = build_way_schedule(input)?;
            let num_workers = std::thread::available_parallelism()
                .map(|n| n.get().saturating_sub(2).max(1))
                .unwrap_or(4);
            let (_total_refs, rebuilt_set) = stage1_pass_a(
                input,
                &schedule,
                num_workers,
                &ref_count_sidecar,
                &per_way_refcount_sidecar,
            )?;
            let rebuilt_unique_nodes = rebuilt_set.total_count();
            node_blob_mapping = Some(build_node_blob_mapping(input, &rebuilt_set)?);
            node_id_set = Some(rebuilt_set);
            rebuilt_unique_nodes
        } else {
            _manifest_unique_nodes
        };

        Stage1Output {
            total_slots,
            unique_nodes,
            rank_bucket_counts,
            num_shard_workers,
            node_id_set,
            node_blob_mapping,
        }
    } else {
        crate::debug::emit_marker("EXTJOIN_STAGE1_START");
        let (s1_minflt_before, s1_majflt_before) = crate::debug::read_page_faults();
        let out = stage1_way_pass(
            input, direct_io, &scratch_dir, &ref_count_sidecar, &per_way_refcount_sidecar,
        )?;
        let (s1_minflt_after, s1_majflt_after) = crate::debug::read_page_faults();
        let total_coo: u64 = out.rank_bucket_counts.iter().sum();
        #[allow(clippy::cast_possible_wrap)]
        {
            crate::debug::emit_counter("extjoin_total_slots", out.total_slots as i64);
            crate::debug::emit_counter("extjoin_total_coo", total_coo as i64);
            crate::debug::emit_counter("extjoin_unique_nodes", out.unique_nodes as i64);
            crate::debug::emit_counter("s1_minflt_delta", (s1_minflt_after - s1_minflt_before) as i64);
            crate::debug::emit_counter("s1_majflt_delta", (s1_majflt_after - s1_majflt_before) as i64);
        }
        crate::debug::emit_marker("EXTJOIN_STAGE1_END");

        if keep_scratch {
            let mut buf = Vec::with_capacity(16);
            buf.extend_from_slice(&out.total_slots.to_le_bytes());
            buf.extend_from_slice(&out.unique_nodes.to_le_bytes());
            std::fs::write(&manifest_path, &buf)
                .map_err(|e| format!("write manifest: {e}"))?;
        }

        out
    };

    let total_slots = stage1_out.total_slots;
    let unique_nodes = stage1_out.unique_nodes;
    let rank_bucket_counts = stage1_out.rank_bucket_counts;
    let num_shard_workers = stage1_out.num_shard_workers;
    let mut node_id_set = stage1_out.node_id_set;
    let mut node_blob_mapping = stage1_out.node_blob_mapping;

    // Compute slot_bucket_count: scale down from NUM_BUCKETS so that
    // every bucket can fit at least one full blob's slot range. This
    // keeps the 2-piece straddler invariant (a blob spans at most two
    // adjacent buckets) for both planet-scale inputs and tiny test
    // fixtures where total_slots / NUM_BUCKETS would otherwise be < 1.
    //
    // We need way_slot_starts (and therefore the ref-count sidecar)
    // before stage 2 to compute max_blob_slots. The sidecar exists
    // either from this run's stage 1 or from a prior --keep-scratch run.
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
    let slot_bucket_count = if max_blob_slots == 0 {
        NUM_BUCKETS
    } else {
        // Each bucket must hold ≥ max_blob_slots so the SMALLEST bucket
        // (which can be smaller than range_size when total_slots is not
        // a multiple of bucket_count) still satisfies the 2-piece
        // straddler invariant. Equivalently: bucket_count ≤
        // total_slots / max_blob_slots, with floor division.
        let max_useful_u64 = (total_slots / max_blob_slots).max(1);
        #[allow(clippy::cast_possible_truncation)]
        let max_useful = max_useful_u64.min(NUM_BUCKETS as u64) as usize;
        max_useful
    };

    // Captured if stage 2 runs this invocation; used at the end to fill
    // Stats.missing_locations. When stage 2 is skipped (--start-stage
    // >= 3) we fall back to the manifest-persisted resolved_count from
    // a prior keep-scratch run, so resumes match fresh runs.
    let mut stage2_resolved_count: Option<u64> = manifest_resolved_count;

    if start <= 2 {
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
            input_pbf,
            node_id_set.as_ref().expect("stage 2 missing IdSetDense"),
            node_blob_mapping.as_deref().expect("stage 2 missing node-blob mapping"),
        )?;
        stage2_resolved_count = Some(resolved_count);
        slot_buckets.finish()?;
        if keep_scratch {
            // Extend the manifest with resolved_count so a future
            // --start-stage >= 3 resume can populate
            // Stats.missing_locations.
            let mut buf = Vec::with_capacity(24);
            buf.extend_from_slice(&total_slots.to_le_bytes());
            buf.extend_from_slice(&unique_nodes.to_le_bytes());
            buf.extend_from_slice(&resolved_count.to_le_bytes());
            std::fs::write(&manifest_path, &buf)
                .map_err(|e| format!("write manifest with resolved_count: {e}"))?;
        }
        let (s2_minflt_after, s2_majflt_after) = crate::debug::read_page_faults();
        if !keep_scratch {
            for worker_id in 0..num_shard_workers {
                for bucket_idx in 0..NUM_BUCKETS {
                    let path = scratch_dir.path.join(format!("rank-W{worker_id}-{bucket_idx:03}"));
                    drop(std::fs::remove_file(&path));
                }
            }
        }
        #[allow(clippy::cast_possible_wrap)]
        {
            crate::debug::emit_counter("extjoin_resolved_count", resolved_count as i64);
            crate::debug::emit_counter("s2_minflt_delta", (s2_minflt_after - s2_minflt_before) as i64);
            crate::debug::emit_counter("s2_majflt_delta", (s2_majflt_after - s2_majflt_before) as i64);
        }
        crate::debug::emit_marker("EXTJOIN_STAGE2_END");
    }

    // Free the IdSetDense (~2 GB RSS at planet) and the per-blob mapping
    // — both were stage 2 inputs only, nothing downstream reads them.
    drop(node_id_set.take());
    drop(node_blob_mapping.take());

    // Prepare integrated coord_payloads artifacts before stage 3.
    let coord_payloads_path = scratch_dir.file_path("coord_payloads");
    let integrated_artifacts: Option<(
        coord_payloads::PerWayRcs,                                      // per_way_rcs
        Vec<std::path::PathBuf>,                                        // worker_tmp_paths
        Vec<std::sync::Mutex<Option<coord_payloads::StraddlerSlot>>>,  // straddler_slots
    )> = if start <= 3 {
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

        Some((per_way_rcs, worker_tmp_paths, straddler_slots))
    } else {
        None
    };

    let mut stage4_per_way_rcs: Option<coord_payloads::PerWayRcs> = None;

    if start <= 3 {
        crate::debug::emit_marker("EXTJOIN_STAGE3_START");
        let (s3_minflt_before, s3_majflt_before) = crate::debug::read_page_faults();
        let slot_entry_counts: Vec<u64> = (0..slot_bucket_count).map(|i| {
            let path = scratch_dir.bucket_path("slot", i);
            std::fs::metadata(&path).map(|m| m.len() / RESOLVED_ENTRY_SIZE as u64).unwrap_or(0)
        }).collect();
        let slot_paths: Vec<std::path::PathBuf> = (0..slot_bucket_count)
            .map(|i| scratch_dir.bucket_path("slot", i))
            .collect();
        let slot_bucket_ref = SlotBucketRef { paths: slot_paths, entry_counts: slot_entry_counts };
        let (per_way_rcs, worker_tmp_paths, straddler_slots) =
            integrated_artifacts.expect("integrated_artifacts present when start <= 3");
        let integrated_inputs = IntegratedInputs {
            way_slot_starts: way_slot_starts.as_slice(),
            per_way_rcs: &per_way_rcs,
            worker_tmp_paths: worker_tmp_paths.as_slice(),
            straddler_slots: straddler_slots.as_slice(),
        };
        let s3_result = stage3_slot_reorder(&slot_bucket_ref, slot_bucket_count, total_slots, integrated_inputs)?;
        let (s3_minflt_after, s3_majflt_after) = crate::debug::read_page_faults();
        if !keep_scratch {
            for i in 0..slot_bucket_count {
                drop(std::fs::remove_file(&scratch_dir.bucket_path("slot", i)));
            }
        }
        #[allow(clippy::cast_possible_wrap)]
        {
            crate::debug::emit_counter("s3_minflt_delta", (s3_minflt_after - s3_minflt_before) as i64);
            crate::debug::emit_counter("s3_majflt_delta", (s3_majflt_after - s3_majflt_before) as i64);
        }
        crate::debug::emit_marker("EXTJOIN_STAGE3_END");

        // Finalize coord_payloads right after stage 3.
        {
            let finalize_stats = coord_payloads::finalize_coord_payloads(
                &coord_payloads_path,
                &per_way_rcs,
                s3_result.worker_manifests,
                &worker_tmp_paths,
                straddler_slots,
            )?;
            eprintln!(
                "[coord_payloads] finalize {} ms (enc {} rd {} wr {}), \
                 output {} MB, straddlers {}, blobs {}",
                finalize_stats.finalize_ms,
                finalize_stats.encode_ms,
                finalize_stats.read_ms,
                finalize_stats.write_ms,
                finalize_stats.output_bytes / 1_000_000,
                finalize_stats.num_straddlers,
                finalize_stats.num_way_blobs,
            );
        }
        stage4_per_way_rcs = Some(per_way_rcs);
    } else {
        // start >= 4: coord_payloads must already exist from a prior keep-scratch run.
        if !coord_payloads_path.exists() {
            return Err(format!(
                "--start-stage {start} requires existing coord_payloads in scratch; \
                 run from an earlier stage with --keep-scratch first"
            )
            .into());
        }
    }

    let relation_member_node_ids = if keep_untagged_nodes {
        None
    } else {
        Some(super::add_locations_to_ways::collect_relation_member_node_ids(
            input, direct_io,
        )?)
    };

    let stage4_per_way_rcs = match stage4_per_way_rcs {
        Some(per_way_rcs) => per_way_rcs,
        None => coord_payloads::load_per_way_refcount_sidecar_indexed(
            &per_way_refcount_sidecar,
            way_slot_starts.len(),
        )?,
    };
    let num_way_blobs = way_slot_starts.len();
    let coord_payloads_reader = coord_payloads::CoordPayloadsReader::open(
        &coord_payloads_path,
        num_way_blobs,
    )?;

    crate::debug::emit_marker("EXTJOIN_STAGE4_START");
    let (s4_minflt_before, s4_majflt_before) = crate::debug::read_page_faults();
    let mut stats = stage4_assembly(
        input,
        output,
        &coord_payloads_reader,
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

    // Stats.missing_locations: derived from stage 2's resolved_count
    // (which already discriminates resolved-vs-(0,0)-sentinel during the
    // node join) so the field matches the dense path's semantics. When
    // stage 2 was skipped via --start-stage >= 3 we have no fresh
    // resolved_count and the field stays at 0 with a one-time notice.
    if let Some(resolved) = stage2_resolved_count {
        stats.missing_locations = total_slots.saturating_sub(resolved);
    } else {
        eprintln!(
            "[altw] note: --start-stage skipped stage 2 and the keep-scratch \
             manifest predates the resolved_count extension; \
             Stats.missing_locations left at 0. Re-run from stage 1 to \
             populate it."
        );
    }

    if !keep_scratch {
        drop(scratch_dir);
    } else {
        std::mem::forget(scratch_dir);
    }

    Ok(stats)
}
