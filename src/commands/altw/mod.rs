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
//! 3. **Slot reorder**: per bucket, sort by slot_pos, write final coord_slots
//!    file sequentially.
//! 4. **Assembly**: stream original PBF + coord_slots, emit enriched ways.
//!
//! Memory at every stage: <1 GB. All I/O sequential. No mmap, no random access.
//! See `notes/altw-partitioned.md` for the full design.

use std::path::Path;

use crate::writer::Compression;
use crate::ElementReader;

use super::add_locations_to_ways::Stats;
use super::external_radix::{BucketWriters, ScratchDir, NUM_BUCKETS};
use super::id_set_dense::IdSetDense;
use super::{require_indexdata, HeaderOverrides, Result};

mod stage1;
mod stage2;
mod stage3;
mod stage4;

use stage1::{build_coords_by_rank_file, build_way_schedule, stage1_way_pass};
use stage2::stage2_node_join;
use stage3::{stage3_slot_reorder_from_ref, SlotBucketRef};
use stage4::{stage4_assembly, CoordSlots};

/// Maximum node ID in current OSM data. Used to compute bucket ranges.
/// 14B gives headroom above the current ~13B maximum.
pub(super) const MAX_NODE_ID: u64 = 14_000_000_000;

/// Size of a rank-occurrence record: `(local_rank: u32, slot_pos: u64)` = 12 bytes.
pub(super) const RANK_RECORD_SIZE: usize = 12;

/// Size of a resolved entry: `(slot_pos: u64, lat: i32, lon: i32)` = 16 bytes.
pub(super) const RESOLVED_ENTRY_SIZE: usize = 16;

/// Size of a coordinate slot: `(lat: i32, lon: i32)` = 8 bytes.
pub(super) const COORD_SLOT_SIZE: usize = 8;

/// A rank-bucketed occurrence record. `local_rank` is the rank offset
/// within the bucket (`global_rank - bucket_rank_start`), stored as u32
/// (max ~40M entries per bucket at planet, well under u32::MAX).
/// `slot_pos` is the final position in the coord_slots array.
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

/// A resolved coordinate ready to be placed into the final coord_slots file.
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
    #[allow(clippy::cast_possible_truncation)]
    fn slot_bucket(&self, total_slots: u64) -> usize {
        let range_size = total_slots.div_ceil(NUM_BUCKETS as u64);
        if range_size == 0 {
            return 0;
        }
        let bucket = self.slot_pos / range_size;
        (bucket as usize).min(NUM_BUCKETS - 1)
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
    let coord_slots_path = scratch_dir.file_path("coord_slots");
    let coord_file_path = scratch_dir.file_path("coords_by_rank");
    let start = start_stage.unwrap_or(1);

    let (total_slots, unique_nodes, rank_bucket_counts, num_shard_workers, node_id_set) =
        if start >= 2 {
            let manifest = std::fs::read(&manifest_path)
                .map_err(|e| format!("read manifest for --start-stage: {e}. Run with --keep-scratch first."))?;
            if manifest.len() < 8 {
                return Err("manifest too small".into());
            }
            let total_slots = u64::from_le_bytes(manifest[..8].try_into()
                .map_err(|_| "manifest read failed")?);

            let schedule = build_way_schedule(input)?;
            let shared_file = std::sync::Arc::new(
                std::fs::File::open(input)
                    .map_err(|e| format!("failed to open {}: {e}", input.display()))?,
            );
            let num_workers = std::thread::available_parallelism()
                .map(|n| n.get().saturating_sub(2).max(1))
                .unwrap_or(4);

            let mut node_id_set = IdSetDense::new();
            #[allow(clippy::cast_possible_wrap)]
            node_id_set.pre_allocate(MAX_NODE_ID as i64);

            let next_idx = std::sync::atomic::AtomicUsize::new(0);
            {
                let schedule_ref = &schedule;
                let next_ref = &next_idx;
                let node_id_set_ref = &node_id_set;

                std::thread::scope(|scope| {
                    for _ in 0..num_workers {
                        let file = std::sync::Arc::clone(&shared_file);
                        scope.spawn(move || {
                            use std::os::unix::fs::FileExt as _;
                            let mut read_buf: Vec<u8> = Vec::new();
                            let mut decompress_buf: Vec<u8> = Vec::new();
                            let mut refs_buf: Vec<i64> = Vec::new();
                            let mut group_starts: Vec<(usize, usize)> = Vec::new();

                            loop {
                                let idx = next_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                if idx >= schedule_ref.len() { break; }
                                let task = &schedule_ref[idx];
                                read_buf.resize(task.data_size, 0);
                                if file.read_exact_at(&mut read_buf, task.data_offset).is_err() { break; }
                                if crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf).is_err() { break; }
                                drop(super::way_scanner::scan_way_refs(
                                    &decompress_buf, &mut refs_buf, &mut group_starts,
                                    |_way_id, refs| {
                                        for &node_id in refs {
                                            node_id_set_ref.set_atomic(node_id);
                                        }
                                    },
                                ));
                            }
                        });
                    }
                });
            }
            node_id_set.build_rank_index();
            let unique_nodes = node_id_set.total_count();

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

            (total_slots, unique_nodes, rank_bucket_counts, num_shard_workers, node_id_set)
        } else {
            crate::debug::emit_marker("EXTJOIN_STAGE1_START");
            let (total_slots, unique_nodes, rank_bucket_counts, num_shard_workers, node_id_set) =
                stage1_way_pass(input, direct_io, &scratch_dir, &ref_count_sidecar, Some(&coord_file_path))?;
            let total_coo: u64 = rank_bucket_counts.iter().sum();
            #[allow(clippy::cast_possible_wrap)]
            {
                crate::debug::emit_counter("extjoin_total_slots", total_slots as i64);
                crate::debug::emit_counter("extjoin_total_coo", total_coo as i64);
                crate::debug::emit_counter("extjoin_unique_nodes", unique_nodes as i64);
            }
            crate::debug::emit_marker("EXTJOIN_STAGE1_END");

            if keep_scratch {
                std::fs::write(&manifest_path, total_slots.to_le_bytes())
                    .map_err(|e| format!("write manifest: {e}"))?;
            }

            (total_slots, unique_nodes, rank_bucket_counts, num_shard_workers, node_id_set)
        };

    if start <= 2 {

        crate::debug::emit_marker("EXTJOIN_STAGE2_START");
        let mut slot_buckets = BucketWriters::create(&scratch_dir, "slot")?;
        let resolved_count =
            stage2_node_join(&scratch_dir, &rank_bucket_counts, num_shard_workers, &mut slot_buckets, total_slots, unique_nodes, &coord_file_path)?;
        slot_buckets.finish()?;
        if !keep_scratch {
            for worker_id in 0..num_shard_workers {
                for bucket_idx in 0..NUM_BUCKETS {
                    let path = scratch_dir.path.join(format!("rank-W{worker_id}-{bucket_idx:03}"));
                    drop(std::fs::remove_file(&path));
                }
            }
        }
        #[allow(clippy::cast_possible_wrap)]
        crate::debug::emit_counter("extjoin_resolved_count", resolved_count as i64);
        crate::debug::emit_marker("EXTJOIN_STAGE2_END");
    }

    if start <= 3 {
        crate::debug::emit_marker("EXTJOIN_STAGE3_START");
        let slot_entry_counts: Vec<u64> = (0..NUM_BUCKETS).map(|i| {
            let path = scratch_dir.bucket_path("slot", i);
            std::fs::metadata(&path).map(|m| m.len() / RESOLVED_ENTRY_SIZE as u64).unwrap_or(0)
        }).collect();
        let slot_paths: Vec<std::path::PathBuf> = (0..NUM_BUCKETS)
            .map(|i| scratch_dir.bucket_path("slot", i))
            .collect();
        let slot_bucket_ref = SlotBucketRef { paths: slot_paths, entry_counts: slot_entry_counts };
        stage3_slot_reorder_from_ref(&slot_bucket_ref, &coord_slots_path, total_slots)?;
        if !keep_scratch {
            for i in 0..NUM_BUCKETS {
                drop(std::fs::remove_file(&scratch_dir.bucket_path("slot", i)));
            }
        }
        crate::debug::emit_marker("EXTJOIN_STAGE3_END");
    }

    let relation_member_node_ids = if keep_untagged_nodes {
        None
    } else {
        Some(super::add_locations_to_ways::collect_relation_member_node_ids(
            input, direct_io,
        )?)
    };

    crate::debug::emit_marker("EXTJOIN_STAGE4_START");
    let coord_slots = CoordSlots::open(&coord_slots_path, total_slots)?;
    let stats = stage4_assembly(
        input,
        output,
        &coord_slots,
        keep_untagged_nodes,
        relation_member_node_ids.as_ref(),
        compression,
        direct_io,
        overrides,
        &ref_count_sidecar,
        total_slots,
    )?;
    crate::debug::emit_marker("EXTJOIN_STAGE4_END");

    if !keep_scratch {
        drop(scratch_dir);
    } else {
        std::mem::forget(scratch_dir);
    }

    Ok(stats)
}
