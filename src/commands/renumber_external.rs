//! External-join implementation of renumber for planet-scale input.
//!
//! The in-memory `renumber` module allocates three `FxHashMap<i64, i64>`
//! tables whose combined size on planet is ~278 GB (node_map ~250 GB,
//! way_map ~28 GB, relation_map ~340 MB), which OOM-kills any host
//! that isn't already oversized. This module replaces `node_map` and
//! `way_map` with 256-bucket radix-partitioned on-disk tuple files,
//! keeping only the small `relation_map` in RAM.
//!
//! ## Three-pass architecture
//!
//! - **Pass 1 (this file)**: stream nodes from the input PBF, assign
//!   new sequential ids, write renumbered nodes to the output PBF, and
//!   emit `(old_node_id, new_node_id)` tuples into 256 `node_map`
//!   buckets partitioned by high bits of `old_node_id`.
//! - **Pass 2 (task #3)**: stream ways, per-bucket merge-join way refs
//!   against `node_map` buckets, emit `(old_way_id, new_way_id)` tuples,
//!   write renumbered ways to output.
//! - **Pass R1 + R2 (task #4)**: relation two-pass handling mirroring
//!   the in-memory path (R1 assigns ids, R2 merge-joins members against
//!   `node_map` + `way_map` buckets and writes).
//!
//! Full design: `notes/renumber-planet-scale.md`. Prior art reused from
//! `src/commands/external_join.rs` (the ALTW refactor) via the shared
//! `external_radix` module (`ScratchDir`, `BucketWriters`).
//!
//! ## Status
//!
//! This module currently implements **pass 1 + pass 2 stage A**:
//!
//! - Pass 1: streams nodes, assigns new ids, writes renumbered nodes to
//!   output, emits `node_map` bucket tuples.
//! - Stage 2a: streams ways, emits `(old_node_id, slot_pos)` COO pairs
//!   into `way_ref` buckets partitioned by high bits of `old_node_id`,
//!   and writes per-blob ref counts to a sidecar file for stage 2d.
//!
//! Stages 2b (node merge-join), 2c (slot reorder), and 2d (way assembly)
//! are in progress. Until they land, the output PBF contains only
//! renumbered nodes — ways are not yet rewritten. Tests against the
//! current state verify that stage 2a runs end-to-end without errors.

use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};

use super::external_radix::{BucketWriters, ScratchDir, NUM_BUCKETS};
use super::renumber::{RenumberOptions, RenumberStats};
use super::{
    dense_node_metadata, element_metadata, ensure_node_capacity_local, ensure_relation_capacity,
    ensure_way_capacity_local, flush_block, flush_local, require_sorted, writer_from_header,
    HeaderOverrides, Result,
};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::writer::Compression;
use crate::{Element, MemberId};

/// Alias for the deterministic hash map used by the in-memory relation map.
type FxHashMap<K, V> = rustc_hash::FxHashMap<K, V>;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Upper bound for node id partitioning. 14B gives headroom above the
/// current ~13B OSM node-id maximum; matches `external_join::MAX_NODE_ID`.
const MAX_NODE_ID: u64 = 14_000_000_000;

/// Upper bound for way id partitioning. 2B gives headroom above the
/// current ~1.17B OSM way-id maximum while keeping the way_map buckets
/// well-populated (reusing MAX_NODE_ID=14B for ways would dump all ways
/// into only the first ~20 of the 256 buckets).
const MAX_WAY_ID: u64 = 2_000_000_000;

/// Serialized size of one `old_id` entry in a node_map / way_map
/// bucket file. The new_id is NOT stored — it's derived at read time
/// from `start_new_id + bucket_new_id_start + position_within_bucket`.
/// This halves the bucket file size vs storing `(old_id, new_id)` pairs.
///
/// Rationale: pass 1 emits node ids in sorted-input order, and
/// `node_id_bucket` is monotonic in the input id, so all nodes in
/// bucket k are processed before any node in bucket k+1. The global
/// new_id for the i-th entry in bucket k is therefore
/// `start_node_id + sum(bucket_counts[0..k]) + i`. Same invariant
/// holds for way_map via `way_id_bucket`.
const OLD_ID_SIZE: usize = 8;

/// Serialized size of one `(old_node_id, slot_pos)` COO pair on disk.
const COO_PAIR_SIZE: usize = 16;

/// Serialized size of one `(slot_pos, new_node_id)` resolved entry on disk.
const RESOLVED_ENTRY_SIZE: usize = 16;

/// Size of a single slot in the flat `new_refs` file produced by stage 2c.
/// Each slot holds one `i64 LE` new_node_id.
const NEW_REF_SIZE: usize = 8;

/// Partition a signed id into one of `NUM_BUCKETS` buckets by its
/// position in `[0, max_id)`. Negative ids clamp to bucket 0.
///
/// The external path currently accepts negative ids through this clamp
/// (a balance wart, not a correctness one) rather than rejecting them
/// at the entry gate; the policy decision is captured in the design
/// doc's section 5 and can be revisited when the renumber tests
/// exercise negative input.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn id_bucket(id: i64, max_id: u64) -> usize {
    let u = if id < 0 { 0u64 } else { id as u64 };
    let range_size = max_id.div_ceil(NUM_BUCKETS as u64);
    let bucket = u / range_size;
    (bucket as usize).min(NUM_BUCKETS - 1)
}

/// Bucket index for node id partitioning. Shared by pass 1 (node_map
/// emit) and stage 2b (node merge-join).
fn node_id_bucket(id: i64) -> usize {
    id_bucket(id, MAX_NODE_ID)
}

/// Bucket index for way id partitioning. Used by stage 2d way_map emit
/// and by the relation pass (task #4) when merge-joining way members.
fn way_id_bucket(id: i64) -> usize {
    id_bucket(id, MAX_WAY_ID)
}

/// Reject negative ids at the entry of the external path.
///
/// The external pipeline's bucket partition assumes non-negative ids —
/// `id_bucket` clamps negatives to bucket 0 which is functionally
/// correct (intra-bucket sort/merge-join both use signed i64) but
/// violates the bucket-balance assumptions. Production OSM planet
/// extracts don't contain negative ids (they're JOSM-local editor
/// staging identifiers resolved before upload); `renumber --mode inmem`
/// still handles them transparently via the in-memory FxHashMap path.
///
/// Per the design doc (notes/renumber-planet-scale.md correctness review
/// finding #5), the external path rejects negative ids rather than
/// silently accepting them. Users with negative-id input get a clear
/// error directing them to the in-memory mode.
fn reject_negative_id(id: i64, kind: &str) -> Result<()> {
    if id < 0 {
        return Err(format!(
            "renumber --mode external requires non-negative input ids. \
             Input contains {kind} id {id}. \
             Use --mode inmem for files with negative (editor-local) ids."
        )
        .into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Old-id entries in node_map / way_map bucket files
// ---------------------------------------------------------------------------
//
// The bucket file stores only the old id per node/way. The new id is
// derived at read time from the bucket's cumulative start offset plus
// the entry's position within the bucket — see `OLD_ID_SIZE`'s docstring
// for the derivation.
//
// No struct is defined for this; callers emit raw `i64` values via
// `emit_old_id` and load them into `Vec<i64>` via `load_old_id_bucket`.

// ---------------------------------------------------------------------------
// CooPair: (old_node_id, slot_pos) emitted by the stage 2a way pass
// ---------------------------------------------------------------------------

/// A coordinate-list (COO) pair linking an old node id to a slot position
/// in the flattened way-ref stream. Matches the shape of
/// `external_join::CooPair`. The slot_pos is a global monotonic counter
/// over all refs of all ways in the stream; the stage 2c slot reorder
/// uses it as a direct index into the flat new_refs file.
#[derive(Clone, Copy)]
struct CooPair {
    old_node_id: i64,
    slot_pos: u64,
}

impl CooPair {
    fn write_to(&self, buf: &mut [u8; COO_PAIR_SIZE]) {
        buf[..8].copy_from_slice(&self.old_node_id.to_le_bytes());
        buf[8..].copy_from_slice(&self.slot_pos.to_le_bytes());
    }

    fn read_from(buf: &[u8; COO_PAIR_SIZE]) -> Self {
        let old_node_id = i64::from_le_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ]);
        let slot_pos = u64::from_le_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]);
        Self { old_node_id, slot_pos }
    }
}

// ---------------------------------------------------------------------------
// ResolvedEntry: (slot_pos, new_node_id) emitted by stage 2b
// ---------------------------------------------------------------------------

/// A resolved ref: slot position plus the new node id to install at that
/// position. Written into the stage-2b slot buckets, partitioned by high
/// bits of `slot_pos`.
#[derive(Clone, Copy)]
struct ResolvedEntry {
    slot_pos: u64,
    new_node_id: i64,
}

impl ResolvedEntry {
    fn write_to(&self, buf: &mut [u8; RESOLVED_ENTRY_SIZE]) {
        buf[..8].copy_from_slice(&self.slot_pos.to_le_bytes());
        buf[8..].copy_from_slice(&self.new_node_id.to_le_bytes());
    }

    /// Bucket index for slot-pos partitioning. Mirrors
    /// `external_join::CooPair::slot_bucket`.
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

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the planet-safe external renumber.
///
/// Pass 1 only: reads nodes, assigns new sequential ids, writes renumbered
/// nodes to the output PBF, and emits `(old_id, new_id)` tuples into the
/// 256-bucket `node_map` scratch files. Ways and relations in the input
/// are not written to the output yet — those are tasks #3 and #4.
///
/// The scratch directory (`.pbfhogg-renumber-external-<pid>/`) is created
/// next to the output path and auto-cleaned when the `ScratchDir` drops.
/// Subsequent passes will consume the bucket files before that cleanup.
#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub fn renumber_external(
    input: &Path,
    output: &Path,
    opts: &RenumberOptions,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<RenumberStats> {
    // ---- Header validation + output writer setup ----
    // Open once to validate the header and build the writer; the actual
    // blob I/O for pass 1 happens via shared_file pread from worker
    // threads below.
    {
        let mut header_reader = crate::blob::BlobReader::open(input, direct_io)?;
        let header_blob = header_reader
            .next()
            .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
        let header = header_blob.to_headerblock()?;
        require_sorted(&header, input, "Input PBF")?;
        super::warn_locations_on_ways_loss(&header);
    }
    // Re-parse header for writer construction (the earlier reader is dropped).
    let header = {
        let mut header_reader = crate::blob::BlobReader::open(input, direct_io)?;
        let header_blob = header_reader
            .next()
            .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
        header_blob.to_headerblock()?
    };
    let mut writer = writer_from_header(output, compression, &header, true, overrides, |hb| {
        hb.sorted()
    }, direct_io, false)?;
    let mut bb = BlockBuilder::new();

    // ---- Scratch dir ----
    let scratch = ScratchDir::new(
        output.parent().unwrap_or(Path::new(".")),
        "renumber-external",
    )?;

    let mut next_relation_id = opts.start_relation_id;
    let mut relation_map: FxHashMap<i64, i64> = FxHashMap::default();
    let mut stats = RenumberStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
    };

    crate::debug::emit_marker("RENUMBER_EXT_START");
    crate::debug::emit_marker("RENUMBER_EXT_PASS1_START");

    // ---- Pass 1: parallel node scan ----
    //
    // Architecture ports external_join.rs stage 4 (assembly) pattern:
    //
    // 1. Pre-scan blob headers to build a schedule filtered to node
    //    blobs, with each blob's element count. Compute prefix sums so
    //    each blob's base new_id is known before any decode work runs.
    // 2. Range-split the schedule in half by blob index. Worker 0 gets
    //    the first half, worker 1 gets the second. Range-based (not
    //    work-stealing) dispatch preserves per-shard bucket-file sort:
    //    each shard's bucket N contains old_ids in strictly ascending
    //    order, and the two shards are disjoint (shard 0's old_ids are
    //    all less than shard 1's). Stage 2b reads them as a concatenated
    //    sorted run.
    // 3. Each worker owns: its node_map bucket shard (BucketWriters),
    //    its BlockBuilder, its read_buf + decompress_buf + scratch Vecs,
    //    its output_blocks Vec<OwnedBlock>. All allocations stay worker-
    //    local — no cross-thread malloc/free churn.
    // 4. Workers send (seq, Result<Vec<OwnedBlock>>) via a bounded
    //    channel. The OwnedBlock's Vec<u8> IS cross-thread-transferred to
    //    the consumer, but bounded at ~32 items × ~1.4 MB = ~45 MB
    //    in flight. Matches the external_join stage 4 pattern which
    //    runs planet-scale without OOM.
    // 5. Main thread consumer drains the channel, uses ReorderBuffer to
    //    deliver (seq, blocks) in file order, pushes each OwnedBlock via
    //    writer.write_primitive_block_owned.
    //
    // The for_each_block_pipelined path was attempted first and OOMed at
    // 26 GB anon RSS on planet — cross-thread PrimitiveBlock retention
    // via glibc arena accumulation, exactly as notes/parallel-classify-
    // regression.md predicted. This pattern avoids that by extracting
    // per-blob OwnedBlock output on the worker thread (so PrimitiveBlocks
    // drop on the worker) and only crossing the Vec<u8> of already-encoded
    // output bytes.
    let pass1_schedule =
        build_kind_blob_schedule(input, crate::blob_index::ElemKind::Node)?;
    let pass1_total_nodes: u64 = pass1_schedule.iter().map(|t| t.element_count).sum();

    // Balance workers by node count: find the blob index where
    // cumulative_count first exceeds half the total.
    let pass1_split_idx = {
        let half = pass1_total_nodes / 2;
        let mut running = 0u64;
        let mut split = pass1_schedule.len();
        for (i, task) in pass1_schedule.iter().enumerate() {
            running += task.element_count;
            if running >= half {
                split = i + 1;
                break;
            }
        }
        split.min(pass1_schedule.len())
    };

    // Per-worker node_map bucket shards. Each shard gets a distinct
    // name so the files don't collide; stage 2b reads them as a
    // concatenated sorted run via the multi-shard slice pattern.
    let mut node_map_shard_a = BucketWriters::create(&scratch, "node-map-a")?;
    let mut node_map_shard_b = BucketWriters::create(&scratch, "node-map-b")?;

    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input).map_err(|e| format!("failed to open {}: {e}", input.display()))?,
    );

    // Track cumulative nodes_written via atomics so both workers can
    // contribute without sharing `&mut RenumberStats`.
    let nodes_written_atomic = std::sync::atomic::AtomicU64::new(0);

    pass1_parallel_scan(
        &pass1_schedule,
        pass1_split_idx,
        opts.start_node_id,
        &shared_file,
        &mut node_map_shard_a,
        &mut node_map_shard_b,
        &nodes_written_atomic,
        &mut writer,
    )?;

    stats.nodes_written += nodes_written_atomic.load(std::sync::atomic::Ordering::Relaxed);
    // Sanity check: the two-worker prefix sum must match the actual
    // atomic count. If not, either the schedule's indexdata count
    // diverged from the decoded node count or a worker dropped work.
    if stats.nodes_written != pass1_total_nodes {
        return Err(format!(
            "pass1 node count mismatch: schedule reported {pass1_total_nodes}, \
             workers wrote {}",
            stats.nodes_written,
        )
        .into());
    }

    crate::debug::emit_marker("RENUMBER_EXT_PASS1_END");

    // Finalize both node_map shards. Files stay on disk for stage 2b.
    let node_map_counts_a = node_map_shard_a.finish()?;
    let node_map_counts_b = node_map_shard_b.finish()?;
    // Per-bucket counts summed across shards — used to compute
    // bucket_new_id_starts for stage 2b.
    let node_map_counts: Vec<u64> = (0..NUM_BUCKETS)
        .map(|i| node_map_counts_a[i] + node_map_counts_b[i])
        .collect();
    #[allow(clippy::cast_possible_wrap)]
    {
        let total: u64 = node_map_counts.iter().sum();
        crate::debug::emit_counter("renumber_ext_node_map_entries", total as i64);
    }

    // ---- Pass 2 stage A: way-ref COO pair emission ----
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2A_START");
    let mut way_ref_buckets = BucketWriters::create(&scratch, "way-ref")?;
    let ref_count_sidecar: PathBuf = scratch.file_path("way-ref-counts");
    let total_slots =
        stage2a_way_ref_pass(input, direct_io, &mut way_ref_buckets, &ref_count_sidecar)?;
    let way_ref_counts = way_ref_buckets.finish()?;
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("renumber_ext_way_ref_slots", total_slots as i64);
    }
    let bucket_total: u64 = way_ref_counts.iter().sum();
    // Hard assert in release: this invariant is load-bearing for the
    // whole pass 2 pipeline. If stage 2a miscounted, stage 2b would
    // silently emit fewer entries than stage 2c expects.
    assert_eq!(
        bucket_total, total_slots,
        "stage 2a bucket entry sum must equal slot counter"
    );
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2A_END");

    // ---- Pass 2 stage B: node merge-join ----
    // Compute the per-bucket new_id start offsets for node_map. The
    // i-th entry in bucket k has new_id = start_node_id + prefix_sum +
    // i. This replaces storing `new_id` alongside `old_id` on disk.
    let node_map_bucket_starts =
        compute_bucket_new_id_starts(opts.start_node_id, &node_map_counts);

    crate::debug::emit_marker("RENUMBER_EXT_STAGE2B_START");
    // Two slot-bucket shards, one per stage 2b worker. Stage 2c reads
    // from both when assembling the flat new_refs file.
    let mut slot_buckets_a = BucketWriters::create(&scratch, "slot-a")?;
    let mut slot_buckets_b = BucketWriters::create(&scratch, "slot-b")?;
    let resolved_count = stage2b_node_merge_join(
        &way_ref_buckets,
        &[&node_map_shard_a, &node_map_shard_b],
        &node_map_bucket_starts,
        &mut slot_buckets_a,
        &mut slot_buckets_b,
        total_slots,
    )?;
    slot_buckets_a.finish()?;
    slot_buckets_b.finish()?;
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("renumber_ext_resolved_entries", resolved_count as i64);
    }
    // Hard assert in release: one resolved entry per COO pair, orphans
    // included. Mismatch means stage 2b dropped an entry and the new_refs
    // flat file would have a zero-filled hole read as new_node_id=0.
    assert_eq!(
        resolved_count, total_slots,
        "stage 2b must emit exactly total_slots resolved entries (orphans included)"
    );
    // Way-ref bucket files were deleted per-bucket inside stage 2b to cut
    // peak temp disk. The `way_ref_buckets` struct's paths still exist
    // but the filesystem entries are gone; drop the struct without
    // calling cleanup() to avoid spurious remove_file errors.
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2B_END");

    // ---- Pass 2 stage C: slot reorder → flat new_refs file ----
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2C_START");
    let new_refs_path: PathBuf = scratch.file_path("new-refs");
    stage2c_slot_reorder(
        &[&slot_buckets_a, &slot_buckets_b],
        &new_refs_path,
        total_slots,
    )?;
    slot_buckets_a.cleanup();
    slot_buckets_b.cleanup();
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2C_END");

    // ---- Pass 2 stage D: way assembly — rewrite refs + write output ----
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2D_START");
    let mut way_map_shard_a = BucketWriters::create(&scratch, "way-map-a")?;
    let mut way_map_shard_b = BucketWriters::create(&scratch, "way-map-b")?;
    let stage2d_ways_atomic = std::sync::atomic::AtomicU64::new(0);
    stage2d_parallel_way_assembly(
        input,
        &mut writer,
        &mut way_map_shard_a,
        &mut way_map_shard_b,
        &new_refs_path,
        &ref_count_sidecar,
        total_slots,
        opts.start_way_id,
        &stage2d_ways_atomic,
    )?;
    stats.ways_written += stage2d_ways_atomic.load(std::sync::atomic::Ordering::Relaxed);
    let way_map_counts_a = way_map_shard_a.finish()?;
    let way_map_counts_b = way_map_shard_b.finish()?;
    let way_map_counts: Vec<u64> = (0..NUM_BUCKETS)
        .map(|i| way_map_counts_a[i] + way_map_counts_b[i])
        .collect();
    #[allow(clippy::cast_possible_wrap)]
    {
        let total: u64 = way_map_counts.iter().sum();
        crate::debug::emit_counter("renumber_ext_way_map_entries", total as i64);
    }
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2D_END");

    // ---- Relation passes R1 + R2a (fused): assign ids + emit member refs ----
    // Single scan over relation blobs. R1 assigns new_relation_ids and
    // builds the in-memory relation_map. R2a emits (old_id, slot_pos)
    // COO pairs for node and way members into their respective bucket
    // sets. Both halves operate on each relation in isolation — R2a
    // does not consult relation_map (relation members are resolved in
    // R2d directly), so the two passes can share a single decoded
    // block.
    crate::debug::emit_marker("RENUMBER_EXT_R1_R2A_START");
    let mut node_member_ref_buckets = BucketWriters::create(&scratch, "rel-node-ref")?;
    let mut way_member_ref_buckets = BucketWriters::create(&scratch, "rel-way-ref")?;
    let (total_node_members, total_way_members) = relation_r1_r2a_fused(
        input,
        direct_io,
        &mut relation_map,
        &mut next_relation_id,
        &mut node_member_ref_buckets,
        &mut way_member_ref_buckets,
    )?;
    node_member_ref_buckets.finish()?;
    way_member_ref_buckets.finish()?;
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("renumber_ext_relation_map_entries", relation_map.len() as i64);
        crate::debug::emit_counter("renumber_ext_rel_node_members", total_node_members as i64);
        crate::debug::emit_counter("renumber_ext_rel_way_members", total_way_members as i64);
    }
    crate::debug::emit_marker("RENUMBER_EXT_R1_R2A_END");

    // ---- Relation pass R2b: merge-join node/way members against maps ----
    // Reuses the same parallel stage2b_node_merge_join function used for
    // the way pass. Each call produces two slot shards (A and B) that
    // stage R2c reads as a slice.
    crate::debug::emit_marker("RENUMBER_EXT_R2B_START");
    let way_map_bucket_starts =
        compute_bucket_new_id_starts(opts.start_way_id, &way_map_counts);

    let mut node_member_slot_a = BucketWriters::create(&scratch, "rel-node-slot-a")?;
    let mut node_member_slot_b = BucketWriters::create(&scratch, "rel-node-slot-b")?;
    stage2b_node_merge_join(
        &node_member_ref_buckets,
        &[&node_map_shard_a, &node_map_shard_b],
        &node_map_bucket_starts,
        &mut node_member_slot_a,
        &mut node_member_slot_b,
        total_node_members,
    )?;
    node_member_slot_a.finish()?;
    node_member_slot_b.finish()?;
    node_member_ref_buckets.cleanup();

    let mut way_member_slot_a = BucketWriters::create(&scratch, "rel-way-slot-a")?;
    let mut way_member_slot_b = BucketWriters::create(&scratch, "rel-way-slot-b")?;
    stage2b_node_merge_join(
        &way_member_ref_buckets,
        &[&way_map_shard_a, &way_map_shard_b],
        &way_map_bucket_starts,
        &mut way_member_slot_a,
        &mut way_member_slot_b,
        total_way_members,
    )?;
    way_member_slot_a.finish()?;
    way_member_slot_b.finish()?;
    way_member_ref_buckets.cleanup();
    crate::debug::emit_marker("RENUMBER_EXT_R2B_END");

    // ---- Relation pass R2c: slot reorder for each member type ----
    crate::debug::emit_marker("RENUMBER_EXT_R2C_START");
    let node_member_new_refs_path: PathBuf = scratch.file_path("rel-node-new-refs");
    stage2c_slot_reorder(
        &[&node_member_slot_a, &node_member_slot_b],
        &node_member_new_refs_path,
        total_node_members,
    )?;
    node_member_slot_a.cleanup();
    node_member_slot_b.cleanup();

    let way_member_new_refs_path: PathBuf = scratch.file_path("rel-way-new-refs");
    stage2c_slot_reorder(
        &[&way_member_slot_a, &way_member_slot_b],
        &way_member_new_refs_path,
        total_way_members,
    )?;
    way_member_slot_a.cleanup();
    way_member_slot_b.cleanup();
    crate::debug::emit_marker("RENUMBER_EXT_R2C_END");

    // ---- Relation pass R2d: write renumbered relations to output ----
    crate::debug::emit_marker("RENUMBER_EXT_R2D_START");
    relation_r2d_assembly(
        input,
        direct_io,
        &mut writer,
        &mut bb,
        &node_member_new_refs_path,
        &way_member_new_refs_path,
        total_node_members,
        total_way_members,
        &relation_map,
        &mut stats,
    )?;
    crate::debug::emit_marker("RENUMBER_EXT_R2D_END");

    flush_block(&mut bb, &mut writer)?;
    writer.flush()?;

    drop(node_member_slot_a);
    drop(node_member_slot_b);
    drop(way_member_slot_a);
    drop(way_member_slot_b);
    drop(node_member_ref_buckets);
    drop(way_member_ref_buckets);
    drop(way_map_shard_a);
    drop(way_map_shard_b);
    drop(slot_buckets_a);
    drop(slot_buckets_b);
    drop(way_ref_buckets);
    drop(node_map_shard_a);
    drop(node_map_shard_b);
    drop(scratch);

    crate::debug::emit_marker("RENUMBER_EXT_END");

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Hot-path helper: write one old id into the correct bucket
// ---------------------------------------------------------------------------

/// Compute the per-bucket new-id start offsets from a bucket-count
/// array produced by `BucketWriters::finish()`. The i-th entry in
/// bucket k has new_id = `bucket_new_id_starts[k] + i`. Bucket 0's
/// start equals `start_new_id`; each subsequent bucket's start is
/// the prior bucket's start plus its count.
///
/// Used by the main entry function after pass 1 / stage 2d completes
/// to build the lookup-side offsets for `stage2b_node_merge_join`.
#[allow(clippy::cast_possible_wrap)]
fn compute_bucket_new_id_starts(
    start_new_id: i64,
    bucket_counts: &[u64],
) -> [i64; NUM_BUCKETS] {
    let mut starts = [0i64; NUM_BUCKETS];
    let mut cursor = start_new_id;
    for (k, starts_k) in starts.iter_mut().enumerate() {
        *starts_k = cursor;
        if let Some(&count) = bucket_counts.get(k) {
            cursor = cursor.saturating_add(count as i64);
        }
    }
    starts
}

// ---------------------------------------------------------------------------
// Pass 2 stage A: way scan — emit (old_node_id, slot_pos) COO pairs
// ---------------------------------------------------------------------------

/// Stream way blobs from the input PBF, emit `(old_node_id, slot_pos)` COO
/// pairs into 256 `way_ref` buckets partitioned by high bits of
/// `old_node_id`, and write per-blob ref counts to a sidecar file.
///
/// Ports `external_join::stage1_way_pass`. Returns the total number of
/// way refs seen (= total slot count, = the eventual size of the flat
/// new_refs file stage 2c produces). The sidecar lets stage 2d
/// (assembly) pre-compute each blob's starting slot_pos without having
/// to re-count refs during the re-scan.
///
/// The per-blob ref-count sidecar layout:
///
/// - `u64 LE` per way blob, in file order (only blobs that pass the
///   indexdata filter — i.e. `ElemKind::Way` blobs — are counted).
/// - A trailer `u64 LE` with the total ref count for alignment
///   verification. Stage 2d checks the trailer equals `total_slots`.
/// Parallel stage 2a way-ref COO emission.
///
/// Architecture (simpler than pass 1 / stage 2d because `scan_way_refs`
/// takes raw decompressed bytes — no PrimitiveBlock lifetime to manage):
///
/// 1. Pre-scan way blob headers to build the schedule.
/// 2. Launch N decode workers behind a bounded descriptor channel.
///    Each worker owns: read_buf, decompress_buf, refs_buf,
///    group_starts scratch, plus a per-blob output Vec<i64> for the
///    ref ids it collected. Workers pread + decompress, run
///    `scan_way_refs`, collect every ref's `old_node_id` into the
///    per-blob Vec (in slot-order within the blob), and send
///    `(seq, Vec<i64>)` through an mpsc.
/// 3. Main thread: `ReorderBuffer` delivers (seq, Vec<i64>) in file
///    order. For each ref, compute bucket + write to way_ref_buckets
///    with the current global `slot_pos`, then increment. Write
///    `blob_ref_count` to the sidecar after each blob.
///
/// Cross-thread data per blob: `Vec<i64>` bounded by ~48K entries
/// (largest real way blob) × 8 bytes = ~384 KB. Bounded channel holds
/// ~32 items → ~12 MB max in flight. Matches the external_join stage
/// 4 Vec<OwnedBlock> bounded-transfer pattern that runs planet-scale
/// without the OOM we saw from for_each_block_pipelined.
#[hotpath::measure]
#[allow(clippy::too_many_lines)]
fn stage2a_way_ref_pass(
    input: &Path,
    _direct_io: bool,
    way_ref_buckets: &mut BucketWriters,
    ref_count_sidecar: &Path,
) -> Result<u64> {
    let schedule = build_kind_blob_schedule(input, crate::blob_index::ElemKind::Way)?;

    let mut sidecar_writer = BufWriter::with_capacity(
        64 * 1024,
        std::fs::File::create(ref_count_sidecar)
            .map_err(|e| format!("failed to create ref-count sidecar: {e}"))?,
    );

    let mut slot_pos: u64 = 0;

    if schedule.is_empty() {
        sidecar_writer.write_all(&slot_pos.to_le_bytes())?;
        sidecar_writer.flush()?;
        return Ok(slot_pos);
    }

    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?,
    );

    // Use (cores - 2) workers to match the external_join pattern, but
    // cap at a modest number since the main thread is the bottleneck
    // (it does all the bucket writes).
    let decode_threads = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4)
        .min(6);

    type ScanItem = (usize, std::result::Result<Vec<i64>, String>);
    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<&BlobTask>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (scan_tx, scan_rx) = std::sync::mpsc::sync_channel::<ScanItem>(32);

    let mut pair_buf = [0u8; COO_PAIR_SIZE];

    std::thread::scope(|scope| -> Result<()> {
        // Dispatcher: feed schedule into descriptor channel.
        let schedule_ref = &schedule;
        scope.spawn(move || {
            for task in schedule_ref {
                if desc_tx.send(task).is_err() {
                    break;
                }
            }
        });

        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let tx = scan_tx.clone();
            let file = std::sync::Arc::clone(&shared_file);
            scope.spawn(move || {
                use std::os::unix::fs::FileExt as _;
                let mut read_buf: Vec<u8> = Vec::new();
                let mut decompress_buf: Vec<u8> = Vec::new();
                let mut refs_buf: Vec<i64> = Vec::new();
                let mut group_starts: Vec<(usize, usize)> = Vec::new();

                loop {
                    let task = {
                        let guard =
                            rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match guard.recv() {
                            Ok(t) => t,
                            Err(_) => break,
                        }
                    };

                    let result: std::result::Result<Vec<i64>, String> = (|| {
                        read_buf.resize(task.data_size, 0);
                        file.read_exact_at(&mut read_buf, task.data_offset)
                            .map_err(|e| {
                                format!("pread failed at offset {}: {e}", task.data_offset)
                            })?;
                        crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)
                            .map_err(|e| e.to_string())?;

                        let mut blob_refs: Vec<i64> = Vec::with_capacity(64 * 1024);
                        let mut scan_err: Option<String> = None;
                        super::way_scanner::scan_way_refs(
                            &decompress_buf,
                            &mut refs_buf,
                            &mut group_starts,
                            |way_id, refs| {
                                if scan_err.is_some() {
                                    return;
                                }
                                if way_id < 0 {
                                    scan_err = Some(format!(
                                        "renumber --mode external requires non-negative \
                                         input ids. Input contains way id {way_id}. \
                                         Use --mode inmem for files with negative \
                                         (editor-local) ids."
                                    ));
                                    return;
                                }
                                for &old_node_id in refs {
                                    if old_node_id < 0 {
                                        scan_err = Some(format!(
                                            "renumber --mode external requires non-negative \
                                             input ids. Way {way_id} references negative \
                                             node id {old_node_id}. Use --mode inmem for \
                                             files with negative (editor-local) ids."
                                        ));
                                        return;
                                    }
                                    blob_refs.push(old_node_id);
                                }
                            },
                        )
                        .map_err(|e| e.to_string())?;
                        if let Some(e) = scan_err {
                            return Err(e);
                        }
                        Ok(blob_refs)
                    })();

                    if tx.send((task.seq, result)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(desc_rx);
        drop(scan_tx);

        // Consumer: reorder by seq, emit to buckets in file order.
        let mut reorder: crate::reorder_buffer::ReorderBuffer<
            std::result::Result<Vec<i64>, String>,
        > = crate::reorder_buffer::ReorderBuffer::with_capacity(64);

        for (seq_num, item) in scan_rx {
            reorder.push(seq_num, item);
            while let Some(result) = reorder.pop_ready() {
                let blob_refs = result
                    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                let blob_start_pos = slot_pos;
                for old_node_id in blob_refs {
                    let pair = CooPair {
                        old_node_id,
                        slot_pos,
                    };
                    let bucket = node_id_bucket(old_node_id);
                    pair.write_to(&mut pair_buf);
                    if let Some(w) = way_ref_buckets.writers[bucket].as_mut() {
                        w.write_all(&pair_buf)?;
                    }
                    way_ref_buckets.entry_counts[bucket] += 1;
                    slot_pos += 1;
                }
                let blob_ref_count = slot_pos - blob_start_pos;
                sidecar_writer.write_all(&blob_ref_count.to_le_bytes())?;
            }
        }
        Ok(())
    })?;

    sidecar_writer.write_all(&slot_pos.to_le_bytes())?;
    sidecar_writer.flush()?;

    Ok(slot_pos)
}

// ---------------------------------------------------------------------------
// LSD radix sort for CooPair by old_node_id
// ---------------------------------------------------------------------------

/// Number of 8-bit radix passes. 5 passes = 40 bits covers any OSM
/// node id up to 1 T (~73× the current 13 B maximum). 4 passes would
/// be enough for today's IDs but leaves no headroom. The cost of an
/// extra pass is linear in N and negligible in the merge-join total.
const RADIX_PASSES: usize = 5;

/// Sort `pairs` in ascending `old_node_id` order via least-significant-
/// digit radix sort. 5 passes × 8 bits of u64 key per pass.
///
/// Input keys MUST be non-negative (negative ids are rejected upstream
/// by `reject_negative_id`). The u64 reinterpret preserves the signed
/// ordering for non-negative i64.
///
/// `scratch` is a caller-provided Vec reused across buckets to avoid
/// per-call allocation. It is grown to the same length as `pairs`.
/// After the function returns, `pairs` holds the sorted data and
/// `scratch` holds an arbitrary intermediate state (not useful to the
/// caller).
///
/// The final-pass output pointer is selected so the sorted data always
/// lives in `pairs` regardless of parity, via `std::mem::swap` at the
/// end of each pass. `RADIX_PASSES` is even → no final swap needed
/// beyond the per-pass swaps... actually 5 is odd, so after 5 swaps
/// the data is in `scratch` and we do one final swap. Handled below.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn radix_sort_coo_pairs(pairs: &mut Vec<CooPair>, scratch: &mut Vec<CooPair>) {
    let n = pairs.len();
    if n < 2 {
        return;
    }

    // Grow scratch to match pairs length. CooPair is Copy so a zero-
    // valued default is fine — every slot gets overwritten below.
    scratch.clear();
    scratch.resize(n, CooPair { old_node_id: 0, slot_pos: 0 });

    for pass in 0..RADIX_PASSES {
        let shift = pass * 8;
        let mut counts = [0u32; 256];

        // Count phase.
        for p in pairs.iter() {
            let byte = ((p.old_node_id as u64 >> shift) & 0xff) as usize;
            counts[byte] += 1;
        }

        // Prefix sum (exclusive). Each bucket's position is the running
        // total before it.
        let mut total: u32 = 0;
        for c in &mut counts {
            let saved = *c;
            *c = total;
            total = total.saturating_add(saved);
        }

        // Distribute phase: pairs → scratch.
        for &p in pairs.iter() {
            let byte = ((p.old_node_id as u64 >> shift) & 0xff) as usize;
            let dst = counts[byte] as usize;
            scratch[dst] = p;
            counts[byte] += 1;
        }

        // Swap buffers for the next pass. After the final pass, the
        // sorted data is in whichever buffer we just wrote to, which
        // is `scratch` — the swap brings it back into `pairs`.
        std::mem::swap(pairs, scratch);
    }
}

#[cfg(test)]
mod radix_tests {
    use super::*;

    fn pair(old: i64, slot: u64) -> CooPair {
        CooPair { old_node_id: old, slot_pos: slot }
    }

    #[test]
    fn radix_empty_is_noop() {
        let mut pairs: Vec<CooPair> = Vec::new();
        let mut scratch: Vec<CooPair> = Vec::new();
        radix_sort_coo_pairs(&mut pairs, &mut scratch);
        assert!(pairs.is_empty());
    }

    #[test]
    fn radix_single_element() {
        let mut pairs = vec![pair(42, 0)];
        let mut scratch: Vec<CooPair> = Vec::new();
        radix_sort_coo_pairs(&mut pairs, &mut scratch);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].old_node_id, 42);
    }

    #[test]
    fn radix_already_sorted() {
        let mut pairs = vec![pair(1, 10), pair(2, 20), pair(3, 30)];
        let mut scratch: Vec<CooPair> = Vec::new();
        radix_sort_coo_pairs(&mut pairs, &mut scratch);
        assert_eq!(pairs.iter().map(|p| p.old_node_id).collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn radix_reverse_sorted() {
        let mut pairs = vec![pair(3, 30), pair(2, 20), pair(1, 10)];
        let mut scratch: Vec<CooPair> = Vec::new();
        radix_sort_coo_pairs(&mut pairs, &mut scratch);
        assert_eq!(pairs.iter().map(|p| p.old_node_id).collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn radix_with_duplicates() {
        // Duplicate keys must survive the sort (stable ordering isn't
        // required by the merge-join, but both duplicates must be
        // present in the output).
        let mut pairs = vec![
            pair(5, 100),
            pair(1, 200),
            pair(5, 300),
            pair(3, 400),
            pair(1, 500),
        ];
        let mut scratch: Vec<CooPair> = Vec::new();
        radix_sort_coo_pairs(&mut pairs, &mut scratch);
        let keys: Vec<i64> = pairs.iter().map(|p| p.old_node_id).collect();
        assert_eq!(keys, vec![1, 1, 3, 5, 5]);
        // Every slot_pos must still be present — no drops.
        let mut slots: Vec<u64> = pairs.iter().map(|p| p.slot_pos).collect();
        slots.sort_unstable();
        assert_eq!(slots, vec![100, 200, 300, 400, 500]);
    }

    #[test]
    fn radix_large_keys_near_planet_max() {
        // OSM's current max node id is ~13 B. Test values at the top
        // of that range to verify the 5-pass, 40-bit key coverage.
        let mut pairs = vec![
            pair(13_000_000_000, 1),
            pair(12_999_999_999, 2),
            pair(1, 3),
            pair(13_000_000_001, 4),
        ];
        let mut scratch: Vec<CooPair> = Vec::new();
        radix_sort_coo_pairs(&mut pairs, &mut scratch);
        assert_eq!(
            pairs.iter().map(|p| p.old_node_id).collect::<Vec<_>>(),
            vec![1, 12_999_999_999, 13_000_000_000, 13_000_000_001],
        );
    }

    #[test]
    fn radix_scratch_reuse_across_calls() {
        // The caller is expected to reuse `scratch` across buckets.
        // Verify that successive calls with different-sized inputs
        // produce correct results without requiring scratch to be
        // cleared externally.
        let mut scratch: Vec<CooPair> = Vec::new();

        let mut a = vec![pair(5, 50), pair(1, 10), pair(3, 30)];
        radix_sort_coo_pairs(&mut a, &mut scratch);
        assert_eq!(a.iter().map(|p| p.old_node_id).collect::<Vec<_>>(), vec![1, 3, 5]);

        // scratch is now length 3 with arbitrary contents.
        let mut b = vec![
            pair(100, 1), pair(10, 2), pair(1000, 3), pair(50, 4), pair(200, 5),
        ];
        radix_sort_coo_pairs(&mut b, &mut scratch);
        assert_eq!(
            b.iter().map(|p| p.old_node_id).collect::<Vec<_>>(),
            vec![10, 50, 100, 200, 1000],
        );
    }

    #[test]
    fn radix_stress_10k() {
        // 10 K entries with a pseudo-random key pattern. Verifies
        // correctness at sizes large enough that sort bugs would
        // show up but small enough to run in every test suite.
        let mut pairs: Vec<CooPair> = (0..10_000u64)
            .map(|i| {
                // Scatter keys with a multiplicative hash. Mask to 31 bits
                // so the value fits cleanly in i64 without wraparound
                // (negative ids are a separate case rejected upstream).
                let key = i.wrapping_mul(2654435761) & 0x7fff_ffff;
                CooPair {
                    old_node_id: key.cast_signed(),
                    slot_pos: i,
                }
            })
            .collect();
        let mut scratch: Vec<CooPair> = Vec::new();
        radix_sort_coo_pairs(&mut pairs, &mut scratch);

        // Verify ascending order.
        for w in pairs.windows(2) {
            assert!(
                w[0].old_node_id <= w[1].old_node_id,
                "not sorted at {} → {}",
                w[0].old_node_id,
                w[1].old_node_id
            );
        }
        assert_eq!(pairs.len(), 10_000);
    }
}

// ---------------------------------------------------------------------------
// Pass 2 stage B: node merge-join
// ---------------------------------------------------------------------------

/// Per-worker scratch buffers for stage 2b. Kept in a struct so the
/// bucket-processing loop can declare one instance per worker and
/// reuse allocations across its claimed buckets.
struct Stage2bScratch {
    way_refs: Vec<CooPair>,
    way_refs_data: Vec<u8>,
    way_refs_scratch: Vec<CooPair>,
    node_map: Vec<i64>,
    node_map_data: Vec<u8>,
    entry_buf: [u8; RESOLVED_ENTRY_SIZE],
}

impl Stage2bScratch {
    fn new() -> Self {
        Self {
            way_refs: Vec::new(),
            way_refs_data: Vec::new(),
            way_refs_scratch: Vec::new(),
            node_map: Vec::new(),
            node_map_data: Vec::new(),
            entry_buf: [0u8; RESOLVED_ENTRY_SIZE],
        }
    }
}

/// Process one source bucket for stage 2b: load, radix-sort, merge-join
/// against node_map, emit resolved entries into `slot_buckets`.
/// Returns the number of entries emitted (same as the bucket's way_ref
/// entry count, since every way-ref produces exactly one resolved
/// entry — orphan or not).
///
/// Deletes the way_ref bucket file on disk after reading it into RAM
/// to minimize temp disk footprint. Safe because the bucket file is
/// not read again by any other stage.
#[allow(clippy::cast_possible_wrap)]
fn stage2b_process_bucket(
    bucket_idx: usize,
    way_ref_buckets: &BucketWriters,
    node_map_shards: &[&BucketWriters],
    bucket_new_id_starts: &[i64; NUM_BUCKETS],
    slot_buckets: &mut BucketWriters,
    total_slots: u64,
    scratch: &mut Stage2bScratch,
) -> Result<u64> {
    if way_ref_buckets.entry_counts[bucket_idx] == 0 {
        return Ok(0);
    }

    load_coo_bucket(
        &way_ref_buckets.paths[bucket_idx],
        &mut scratch.way_refs_data,
        &mut scratch.way_refs,
    )?;
    // Delete the way_ref bucket file now that we've read it into
    // RAM — no further stage consumes it. Cuts peak temp disk.
    drop(std::fs::remove_file(&way_ref_buckets.paths[bucket_idx]));
    // LSD radix sort by old_node_id — see `radix_sort_coo_pairs`.
    radix_sort_coo_pairs(&mut scratch.way_refs, &mut scratch.way_refs_scratch);

    // node_map is already sorted by old_id because pass 1 scans a
    // sorted input and emits in file order, and `id_bucket` is
    // monotonic in the input id. When pass 1 is parallel, multiple
    // shards are read in order: shard 0 holds all ids from the first
    // half of node blobs (all less than shard 1's ids), so
    // concatenating the shards yields a single sorted run.
    scratch.node_map.clear();
    load_old_id_bucket_shards(
        node_map_shards,
        bucket_idx,
        &mut scratch.node_map_data,
        &mut scratch.node_map,
    )?;

    let bucket_base = bucket_new_id_starts[bucket_idx];

    // Two-cursor merge. Both sides sorted by old id; the node_map
    // cursor only moves forward, so the walk is O(way_refs + node_map).
    let mut resolved_count: u64 = 0;
    let mut nm_cursor: usize = 0;
    for wr in &scratch.way_refs {
        while nm_cursor < scratch.node_map.len() && scratch.node_map[nm_cursor] < wr.old_node_id {
            nm_cursor += 1;
        }
        let resolved_id = if nm_cursor < scratch.node_map.len()
            && scratch.node_map[nm_cursor] == wr.old_node_id
        {
            // Position-based new id reconstruction.
            bucket_base + nm_cursor as i64
        } else {
            // Orphan ref: no matching entry. Preserve old id,
            // matching in-memory renumber's unwrap_or(old_id) policy.
            wr.old_node_id
        };
        let entry = ResolvedEntry {
            slot_pos: wr.slot_pos,
            new_node_id: resolved_id,
        };
        let sb = entry.slot_bucket(total_slots);
        entry.write_to(&mut scratch.entry_buf);
        if let Some(w) = slot_buckets.writers[sb].as_mut() {
            w.write_all(&scratch.entry_buf)?;
        }
        slot_buckets.entry_counts[sb] += 1;
        resolved_count += 1;
    }

    Ok(resolved_count)
}

/// For each of the 256 node-id buckets: load the way-ref `CooPair`s into
/// RAM (radix-sort by `old_node_id`), load the corresponding `node_map`
/// old-id entries (already sorted by `old_id` because pass 1 emits in
/// ascending input-file order), two-cursor merge-join, and emit
/// `(slot_pos, new_node_id)` resolved entries into slot buckets.
///
/// `bucket_new_id_starts[k]` must hold the new id to assign to the
/// 0-th node in bucket k — i.e., `start_node_id +
/// sum(node_map_counts[0..k])`. The i-th entry in bucket k is then
/// assigned `bucket_new_id_starts[k] + i`. This reconstructs the new
/// id from position, avoiding the 8 bytes per entry that storing
/// `(old, new)` pairs on disk used to cost.
///
/// Same function is reused in R2B for relation members: pass
/// `way_map_buckets` as the lookup side with `bucket_new_id_starts`
/// derived from `start_way_id + way_map_counts` prefix sums.
///
/// Orphan refs (way-refs whose `old_node_id` has no matching entry)
/// fall through with `resolved_id = old_node_id`, matching the in-
/// memory renumber's `unwrap_or(old_id)` semantics.
///
/// Returns the total number of resolved entries emitted. Expected to
/// equal the `total_slots` returned by stage 2a.
///
/// **Temp disk discipline**: deletes each way-ref bucket file as soon
/// as its merge-join completes. Without this per-bucket cleanup, stage
/// 2b's peak temp disk footprint at planet scale would be
/// `node_map (83 GB) + way_ref (166 GB) + slot (136 GB) = 385 GB`.
/// With cleanup, it drops to `node_map + per-bucket way_ref (~650 MB)
/// + slot = ~219 GB`. node_map stays alive because the relation pass
/// needs it.
///
/// Two-worker parallel stage 2b. Workers compete for source buckets
/// via an atomic index counter — each worker claims the next unclaimed
/// bucket, processes it to completion (including file delete), then
/// grabs the next. Perfect load balance at the cost of a single
/// `fetch_add` per bucket.
///
/// Each worker writes to its own `slot_buckets` shard (A or B). Stage
/// 2c later reads from BOTH shards when assembling the flat `new_refs`
/// file. Contention-free within stage 2b; zero lock overhead.
///
/// Memory at planet scale with 2 workers after the map-format shrink:
/// 2 × (way_refs ~530 MB + way_refs_scratch ~530 MB + node_map ~325 MB +
/// node_map_data transient ~325 MB) = ~3.4 GB peak, within the 4 GB
/// design target. A third worker would overshoot; 2 is the ceiling.
///
/// The function signature takes both shards because each worker needs
/// exclusive mutable access to one. Callers must pre-create two
/// `BucketWriters` with distinct scratch prefixes (e.g. `"slot-a"` and
/// `"slot-b"`).
#[hotpath::measure]
fn stage2b_node_merge_join(
    way_ref_buckets: &BucketWriters,
    node_map_shards: &[&BucketWriters],
    bucket_new_id_starts: &[i64; NUM_BUCKETS],
    slot_buckets_a: &mut BucketWriters,
    slot_buckets_b: &mut BucketWriters,
    total_slots: u64,
) -> Result<u64> {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let next_bucket = AtomicUsize::new(0);

    // Worker closures return `std::result::Result<u64, String>` instead
    // of the module `Result<u64>` — the latter's error type
    // (`Box<dyn Error>`) isn't Send, so it can't cross `thread::scope`
    // boundaries. We stringify inside the worker and convert back at
    // the join point.
    type WorkerResult = std::result::Result<u64, String>;

    let (count_a, count_b) = std::thread::scope(|s| -> Result<(u64, u64)> {
        let next_ref = &next_bucket;
        let nm_starts = bucket_new_id_starts;
        let nm_shards = node_map_shards;

        let handle_a = s.spawn(move || -> WorkerResult {
            let mut scratch = Stage2bScratch::new();
            let mut count = 0u64;
            loop {
                let i = next_ref.fetch_add(1, Ordering::Relaxed);
                if i >= NUM_BUCKETS {
                    break;
                }
                count += stage2b_process_bucket(
                    i, way_ref_buckets, nm_shards, nm_starts,
                    slot_buckets_a, total_slots, &mut scratch,
                )
                .map_err(|e| e.to_string())?;
            }
            Ok(count)
        });
        let handle_b = s.spawn(move || -> WorkerResult {
            let mut scratch = Stage2bScratch::new();
            let mut count = 0u64;
            loop {
                let i = next_ref.fetch_add(1, Ordering::Relaxed);
                if i >= NUM_BUCKETS {
                    break;
                }
                count += stage2b_process_bucket(
                    i, way_ref_buckets, nm_shards, nm_starts,
                    slot_buckets_b, total_slots, &mut scratch,
                )
                .map_err(|e| e.to_string())?;
            }
            Ok(count)
        });

        let count_a = handle_a
            .join()
            .map_err(|_| "stage 2b worker A panicked".to_string())?
            .map_err(|e| format!("stage 2b worker A: {e}"))?;
        let count_b = handle_b
            .join()
            .map_err(|_| "stage 2b worker B panicked".to_string())?
            .map_err(|e| format!("stage 2b worker B: {e}"))?;
        Ok((count_a, count_b))
    })?;

    Ok(count_a + count_b)
}

/// Load a bucket file of `CooPair` tuples into the provided `pairs` Vec,
/// reusing `data_buf` as the raw read scratch. On Linux with
/// `linux-direct-io`, also fadvise(DONTNEED) the file after read so the
/// kernel can evict the pages.
///
/// After parse, `data_buf` is shrunk back to zero capacity to release the
/// raw-bytes backing store — stage 2b holds both sides of a merge-join
/// live simultaneously, so keeping the raw bytes around doubles peak
/// anon RSS per bucket.
#[allow(clippy::cast_possible_truncation)]
fn load_coo_bucket(
    path: &Path,
    data_buf: &mut Vec<u8>,
    pairs: &mut Vec<CooPair>,
) -> Result<()> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("failed to open way_ref bucket {}: {e}", path.display()))?;
    let len = file
        .metadata()
        .map_err(|e| format!("failed to stat way_ref bucket {}: {e}", path.display()))?
        .len() as usize;
    if !len.is_multiple_of(COO_PAIR_SIZE) {
        return Err(format!(
            "way_ref bucket {} is {len} bytes, not a multiple of {COO_PAIR_SIZE} — truncated or corrupt",
            path.display()
        )
        .into());
    }
    data_buf.clear();
    data_buf.resize(len, 0);
    std::io::Read::read_exact(&mut &file, data_buf)
        .map_err(|e| format!("failed to read way_ref bucket {}: {e}", path.display()))?;
    #[cfg(feature = "linux-direct-io")]
    super::external_radix::advise_dontneed_file(&file);

    let count = data_buf.len() / COO_PAIR_SIZE;
    pairs.clear();
    pairs.reserve_exact(count.saturating_sub(pairs.capacity()));
    let mut buf = [0u8; COO_PAIR_SIZE];
    for chunk in data_buf.chunks_exact(COO_PAIR_SIZE) {
        buf.copy_from_slice(chunk);
        pairs.push(CooPair::read_from(&buf));
    }
    // Release raw-byte storage now that parsing is done. Keeping it
    // alive double-counts the bucket in peak anon RSS during stage 2b.
    *data_buf = Vec::new();
    Ok(())
}

// ---------------------------------------------------------------------------
// Pass 2 stage C: slot reorder — build the flat new_refs file
// ---------------------------------------------------------------------------

/// Read slot buckets in order, scatter their `ResolvedEntry` payloads into
/// a dense position-indexed buffer per bucket, write the buffer
/// sequentially to the `new_refs` file.
///
/// Ports `external_join::stage3_slot_reorder`. Each bucket covers a
/// contiguous `slot_pos` range; within a bucket, `local_pos = slot_pos -
/// bucket_start` is used as a direct index into a pre-sized byte buffer.
/// The resulting flat file is `total_slots × NEW_REF_SIZE` bytes and
/// holds one `i64 LE new_node_id` per slot_pos, ready for stage 2d to
/// pread sequentially as it walks each way blob.
///
/// `slot_bucket_shards` is a slice of shard sets. Stage 2b's
/// two-worker parallelism produces TWO shards (A and B), each a full
/// `BucketWriters` with 256 files. For each `bucket_idx`, this
/// function reads the bytes from every shard's corresponding file,
/// concatenates into a single parse buffer, and scatters the combined
/// resolved entries into the output buffer. The single-shard case
/// (older call sites, R2B) passes a one-element slice.
///
/// Empty slot buckets (a slot-pos range with no refs that landed in it)
/// get zero-byte fills — harmless because stage 2d's way assembly reads
/// each slot indexed by the actual way's (blob_slot_start, ref_index)
/// and never touches an empty range.
#[hotpath::measure]
#[allow(clippy::cast_possible_truncation)]
fn stage2c_slot_reorder(
    slot_bucket_shards: &[&BucketWriters],
    new_refs_path: &Path,
    total_slots: u64,
) -> Result<()> {
    let file = std::fs::File::create(new_refs_path).map_err(|e| {
        format!(
            "failed to create new_refs file {}: {e}",
            new_refs_path.display()
        )
    })?;
    let mut out = BufWriter::with_capacity(256 * 1024, file);

    let range_size = total_slots.div_ceil(NUM_BUCKETS as u64);
    let mut data_buf: Vec<u8> = Vec::new();
    let mut scatter_buf: Vec<u8> = Vec::new();
    let mut next_slot: u64 = 0;

    for bucket_idx in 0..NUM_BUCKETS {
        let bucket_start = bucket_idx as u64 * range_size;
        let bucket_end = ((bucket_idx as u64 + 1) * range_size).min(total_slots);
        if bucket_start >= total_slots {
            break;
        }
        let bucket_slots = bucket_end - bucket_start;

        // Sum entry counts across shards for this bucket.
        let total_bucket_entries: u64 = slot_bucket_shards
            .iter()
            .map(|s| s.entry_counts[bucket_idx])
            .sum();

        if total_bucket_entries == 0 {
            // Empty bucket: write zero-filled range. Stage 2d never reads
            // these positions because the flat file is addressed by ref
            // positions that map 1:1 to emitted slots, but we still need
            // to advance the file pointer to keep slot_pos alignment.
            let zero_bytes = bucket_slots as usize * NEW_REF_SIZE;
            scatter_buf.clear();
            scatter_buf.resize(zero_bytes, 0);
            out.write_all(&scatter_buf)?;
            next_slot = bucket_end;
            continue;
        }

        let bucket_bytes = bucket_slots as usize * NEW_REF_SIZE;
        scatter_buf.clear();
        scatter_buf.resize(bucket_bytes, 0);

        // Read bytes from each shard's bucket file and concatenate.
        // Entry order within data_buf doesn't matter — every entry is
        // scattered by slot_pos into scatter_buf regardless of its
        // position in the input stream.
        data_buf.clear();
        for shard in slot_bucket_shards {
            if shard.entry_counts[bucket_idx] == 0 {
                continue;
            }
            let file = std::fs::File::open(&shard.paths[bucket_idx]).map_err(|e| {
                format!(
                    "failed to open slot bucket {}: {e}",
                    shard.paths[bucket_idx].display()
                )
            })?;
            std::io::Read::read_to_end(&mut &file, &mut data_buf).map_err(|e| {
                format!(
                    "failed to read slot bucket {}: {e}",
                    shard.paths[bucket_idx].display()
                )
            })?;
            #[cfg(feature = "linux-direct-io")]
            super::external_radix::advise_dontneed_file(&file);
        }

        if !data_buf.len().is_multiple_of(RESOLVED_ENTRY_SIZE) {
            return Err(format!(
                "slot bucket {bucket_idx} shards total {} bytes, not a multiple of {RESOLVED_ENTRY_SIZE} — truncated or corrupt",
                data_buf.len()
            )
            .into());
        }

        let mut buf = [0u8; RESOLVED_ENTRY_SIZE];
        for chunk in data_buf.chunks_exact(RESOLVED_ENTRY_SIZE) {
            buf.copy_from_slice(chunk);
            let slot_pos = u64::from_le_bytes([
                buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
            ]);
            let new_node_id = i64::from_le_bytes([
                buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
            ]);
            let local_pos = (slot_pos - bucket_start) as usize;
            let offset = local_pos * NEW_REF_SIZE;
            scatter_buf[offset..offset + NEW_REF_SIZE]
                .copy_from_slice(&new_node_id.to_le_bytes());
        }

        out.write_all(&scatter_buf)?;
        next_slot = bucket_end;
    }

    // Trailing slots if total_slots isn't bucket-aligned.
    if next_slot < total_slots {
        let remaining = (total_slots - next_slot) as usize * NEW_REF_SIZE;
        scatter_buf.clear();
        scatter_buf.resize(remaining, 0);
        out.write_all(&scatter_buf)?;
    }

    out.flush()?;
    Ok(())
}

/// Load multiple shards' `bucket_idx` files and concatenate them into
/// a single sorted `ids` Vec. Each shard's bucket file is already
/// sorted internally (pass 1 scans monotonic input), and the shards
/// are disjoint with shard 0 holding the smallest ids. So reading in
/// shard order and appending gives a single ascending run — no merge
/// needed. This is what makes two-worker pass 1 composable with the
/// existing two-cursor merge-join in stage 2b.
fn load_old_id_bucket_shards(
    shards: &[&BucketWriters],
    bucket_idx: usize,
    data_buf: &mut Vec<u8>,
    ids: &mut Vec<i64>,
) -> Result<()> {
    use std::io::Read as _;
    ids.clear();
    let total: u64 = shards
        .iter()
        .map(|s| s.entry_counts[bucket_idx])
        .sum();
    if total == 0 {
        return Ok(());
    }
    ids.reserve_exact(
        usize::try_from(total)
            .map_err(|_| "node_map bucket shard total overflow")?
            .saturating_sub(ids.capacity()),
    );
    for shard in shards {
        if shard.entry_counts[bucket_idx] == 0 {
            continue;
        }
        let path = &shard.paths[bucket_idx];
        let file = std::fs::File::open(path)
            .map_err(|e| format!("failed to open old_id bucket {}: {e}", path.display()))?;
        let len = usize::try_from(
            file.metadata()
                .map_err(|e| format!("failed to stat old_id bucket {}: {e}", path.display()))?
                .len(),
        )
        .map_err(|_| format!("old_id bucket {} too large for usize", path.display()))?;
        if !len.is_multiple_of(OLD_ID_SIZE) {
            return Err(format!(
                "old_id bucket {} is {len} bytes, not a multiple of {OLD_ID_SIZE} — \
                 truncated or corrupt",
                path.display()
            )
            .into());
        }
        data_buf.clear();
        data_buf.resize(len, 0);
        (&file)
            .read_exact(data_buf)
            .map_err(|e| format!("failed to read old_id bucket {}: {e}", path.display()))?;
        #[cfg(feature = "linux-direct-io")]
        super::external_radix::advise_dontneed_file(&file);
        let mut buf = [0u8; OLD_ID_SIZE];
        for chunk in data_buf.chunks_exact(OLD_ID_SIZE) {
            buf.copy_from_slice(chunk);
            ids.push(i64::from_le_bytes(buf));
        }
    }
    *data_buf = Vec::new();
    Ok(())
}

// ---------------------------------------------------------------------------
// Pass 2 stage D: way assembly — re-scan ways, rewrite refs, write output
// ---------------------------------------------------------------------------

/// Load the per-blob ref-count sidecar written by stage 2a and compute
/// prefix sums so stage 2d can look up each way blob's starting
/// `slot_pos` in O(1). Returns a Vec of starting offsets indexed by way
/// blob order (matching the stage-2a and stage-2d way-blob filter).
///
/// The sidecar layout: `u64 LE` per way blob followed by a trailer
/// `u64 LE` with the total. The trailer is checked against `total_slots`
/// for alignment verification.
fn load_ref_count_sidecar(path: &Path, total_slots: u64) -> Result<Vec<u64>> {
    let data = std::fs::read(path)
        .map_err(|e| format!("failed to read ref-count sidecar: {e}"))?;
    if data.len() < 8 {
        return Err("ref-count sidecar is too small".into());
    }
    let trailer_bytes: [u8; 8] = data[data.len() - 8..]
        .try_into()
        .map_err(|_| "ref-count sidecar trailer read failed")?;
    let trailer_total = u64::from_le_bytes(trailer_bytes);
    if trailer_total != total_slots {
        return Err(format!(
            "ref-count sidecar total ({trailer_total}) != stage 2a total_slots ({total_slots})"
        )
        .into());
    }
    let entry_bytes = &data[..data.len() - 8];
    if !entry_bytes.len().is_multiple_of(8) {
        return Err("ref-count sidecar has non-aligned entries".into());
    }
    let num_entries = entry_bytes.len() / 8;
    let mut slot_starts = Vec::with_capacity(num_entries);
    let mut cumulative: u64 = 0;
    for chunk in entry_bytes.chunks_exact(8) {
        slot_starts.push(cumulative);
        let bytes: [u8; 8] = chunk.try_into().map_err(|_| "sidecar chunk size")?;
        cumulative += u64::from_le_bytes(bytes);
    }
    Ok(slot_starts)
}

/// Re-scan way blobs, rewrite refs from the flat `new_refs` file using
/// each blob's starting `slot_pos` from the ref-count sidecar, assign
/// new sequential way ids, emit `(old_way_id, new_way_id)` pairs into
/// `way_map` buckets, and write the renumbered ways to the output PBF.
///
/// The second scan reuses the same blob filter as stage 2a (OsmData +
/// `ElemKind::Way` when indexed) so the blob-index count matches the
/// sidecar's entry count. Within each blob, ways are iterated via the
/// full element path (`block.elements()` matched on `Element::Way`),
/// which gives tags + metadata + refs — vs stage 2a's `scan_way_refs`
/// which is a refs-only fast path.
/// Parallel stage 2d way assembly.
///
/// Mirrors the pass-1 worker-pool pattern: two range-partitioned
/// workers, each owning a way_map shard, a `BlockBuilder`, its own
/// scratch buffers, and an `output_blocks: Vec<OwnedBlock>` staging
/// vec. Workers pread way blobs, construct PrimitiveBlocks locally
/// (dropped on the worker thread), iterate ways, look up new node ids
/// from the shared `new_refs_mmap`, rewrite refs into their local
/// `BlockBuilder`, and send `(seq, Vec<OwnedBlock>)` via a bounded
/// channel. The main thread reorders by seq and writes via
/// `write_primitive_block_owned`.
///
/// Returns the next available way id after the pass.
#[hotpath::measure]
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation
)]
fn stage2d_parallel_way_assembly(
    input: &Path,
    writer: &mut crate::writer::PbfWriter<crate::file_writer::FileWriter>,
    way_map_shard_a: &mut BucketWriters,
    way_map_shard_b: &mut BucketWriters,
    new_refs_path: &Path,
    ref_count_sidecar: &Path,
    total_slots: u64,
    start_way_id: i64,
    ways_written: &std::sync::atomic::AtomicU64,
) -> Result<()> {
    // Load sidecar and compute per-blob starting slot positions.
    let blob_slot_starts = load_ref_count_sidecar(ref_count_sidecar, total_slots)?;

    // mmap the flat new_refs file for zero-syscall slot lookups.
    let new_refs_file = std::fs::File::open(new_refs_path)
        .map_err(|e| format!("failed to open new_refs file: {e}"))?;
    let new_refs_len = new_refs_file
        .metadata()
        .map_err(|e| format!("failed to stat new_refs file: {e}"))?
        .len();
    let expected_len = total_slots * NEW_REF_SIZE as u64;
    if new_refs_len != expected_len {
        return Err(format!(
            "new_refs file size {new_refs_len} != expected {expected_len} (total_slots={total_slots})"
        )
        .into());
    }
    let new_refs_mmap: memmap2::Mmap = if new_refs_len == 0 {
        memmap2::MmapOptions::new().map_anon()?.make_read_only()?
    } else {
        let mmap = unsafe { memmap2::Mmap::map(&new_refs_file) }
            .map_err(|e| format!("failed to mmap new_refs file: {e}"))?;
        #[cfg(unix)]
        mmap.advise(memmap2::Advice::Sequential).ok();
        mmap
    };
    let new_refs_mmap = std::sync::Arc::new(new_refs_mmap);

    // Build a way-blob schedule with per-blob element counts. The
    // sidecar length must match the schedule 1:1 (both filtered to
    // OsmData + ElemKind::Way).
    let schedule = build_kind_blob_schedule(input, crate::blob_index::ElemKind::Way)?;
    if schedule.len() != blob_slot_starts.len() {
        return Err(format!(
            "stage 2d: way blob schedule size {} != sidecar entries {}",
            schedule.len(),
            blob_slot_starts.len()
        )
        .into());
    }

    let total_ways: u64 = schedule.iter().map(|t| t.element_count).sum();
    // Sanity check: ensure total_ways would fit as an i64 offset.
    // We only need the value to validate overflow, not to carry out.
    i64::try_from(total_ways).map_err(|_| "planet way count > i64")?;

    if schedule.is_empty() {
        return Ok(());
    }

    // Balance workers by way count (not blob count) for load balance.
    let split_idx = {
        let half = total_ways / 2;
        let mut running = 0u64;
        let mut split = schedule.len();
        for (i, task) in schedule.iter().enumerate() {
            running += task.element_count;
            if running >= half {
                split = i + 1;
                break;
            }
        }
        split.min(schedule.len())
    };

    let a_element_count: u64 = schedule[..split_idx].iter().map(|t| t.element_count).sum();
    let a_start_way_id = start_way_id;
    let b_start_way_id = start_way_id
        .checked_add(
            i64::try_from(a_element_count).map_err(|_| "stage 2d worker A count overflow")?,
        )
        .ok_or("stage 2d worker B base way_id overflow")?;

    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?,
    );

    let tasks_a = &schedule[..split_idx];
    let tasks_b = &schedule[split_idx..];

    type DecodedItem = (usize, std::result::Result<Vec<OwnedBlock>, String>);
    let (decoded_tx, decoded_rx) = std::sync::mpsc::sync_channel::<DecodedItem>(32);

    std::thread::scope(|scope| -> Result<()> {
        let file_a = std::sync::Arc::clone(&shared_file);
        let mmap_a = std::sync::Arc::clone(&new_refs_mmap);
        let slots_a = &blob_slot_starts;
        let tx_a = decoded_tx.clone();
        scope.spawn(move || {
            stage2d_worker(
                tasks_a,
                a_start_way_id,
                &file_a,
                &mmap_a,
                slots_a,
                total_slots,
                way_map_shard_a,
                ways_written,
                &tx_a,
            );
        });

        let file_b = std::sync::Arc::clone(&shared_file);
        let mmap_b = std::sync::Arc::clone(&new_refs_mmap);
        let slots_b = &blob_slot_starts;
        let tx_b = decoded_tx.clone();
        scope.spawn(move || {
            stage2d_worker(
                tasks_b,
                b_start_way_id,
                &file_b,
                &mmap_b,
                slots_b,
                total_slots,
                way_map_shard_b,
                ways_written,
                &tx_b,
            );
        });

        drop(decoded_tx);

        let mut reorder: crate::reorder_buffer::ReorderBuffer<
            std::result::Result<Vec<OwnedBlock>, String>,
        > = crate::reorder_buffer::ReorderBuffer::with_capacity(64);

        for (seq_num, item) in decoded_rx {
            reorder.push(seq_num, item);
            while let Some(result) = reorder.pop_ready() {
                let blocks = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                for (block_bytes, index, tagdata) in blocks {
                    writer.write_primitive_block_owned(
                        block_bytes,
                        index,
                        tagdata.as_deref(),
                    )?;
                }
            }
        }
        Ok(())
    })?;

    Ok(())
}

/// Stage 2d per-worker loop. Processes tasks in range order, owns a
/// BlockBuilder and output Vec<OwnedBlock>, and emits one owned-block
/// batch per blob through the channel. Looks up the ref_count_sidecar-
/// derived `slot_cursor` for each blob from the shared `blob_slot_starts`
/// slice so per-blob alignment stays deterministic regardless of the
/// parallel dispatch order.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn stage2d_worker(
    tasks: &[BlobTask],
    worker_start_way_id: i64,
    shared_file: &std::sync::Arc<std::fs::File>,
    new_refs_mmap: &std::sync::Arc<memmap2::Mmap>,
    blob_slot_starts: &[u64],
    total_slots: u64,
    shard: &mut BucketWriters,
    ways_written: &std::sync::atomic::AtomicU64,
    tx: &std::sync::mpsc::SyncSender<(usize, std::result::Result<Vec<OwnedBlock>, String>)>,
) {
    use std::os::unix::fs::FileExt as _;

    let mut read_buf: Vec<u8> = Vec::new();
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut local_bb = BlockBuilder::new();
    let mut output_blocks: Vec<OwnedBlock> = Vec::new();
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut id_buf = [0u8; OLD_ID_SIZE];
    let mut current_way_id = worker_start_way_id;

    for task in tasks {
        let base_way_id = current_way_id;
        let expected_slot_start = blob_slot_starts[task.seq];
        let expected_slot_end = blob_slot_starts
            .get(task.seq + 1)
            .copied()
            .unwrap_or(total_slots);

        let result: std::result::Result<Vec<OwnedBlock>, String> = (|| {
            read_buf.resize(task.data_size, 0);
            shared_file
                .read_exact_at(&mut read_buf, task.data_offset)
                .map_err(|e| format!("pread failed at offset {}: {e}", task.data_offset))?;
            crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)
                .map_err(|e| e.to_string())?;
            let block = crate::block::PrimitiveBlock::new(bytes::Bytes::from(
                std::mem::take(&mut decompress_buf),
            ))
            .map_err(|e| e.to_string())?;

            output_blocks.clear();
            let mut slot_cursor = expected_slot_start;
            let mut blob_way_count: u64 = 0;
            let mut next_id = base_way_id;

            for element in block.elements() {
                if let Element::Way(w) = &element {
                    reject_negative_id(w.id(), "way").map_err(|e| e.to_string())?;
                    ensure_way_capacity_local(&mut local_bb, &mut output_blocks)?;
                    let new_id = next_id;
                    next_id += 1;

                    refs_buf.clear();
                    for _ in w.refs() {
                        let offset = usize::try_from(slot_cursor)
                            .map_err(|_| "slot_cursor > usize".to_string())?
                            .checked_mul(NEW_REF_SIZE)
                            .ok_or_else(|| "slot offset overflow".to_string())?;
                        let end = offset
                            .checked_add(NEW_REF_SIZE)
                            .ok_or_else(|| "slot offset end overflow".to_string())?;
                        let bytes: [u8; NEW_REF_SIZE] = new_refs_mmap[offset..end]
                            .try_into()
                            .map_err(|_| "stage 2d new_refs slice".to_string())?;
                        refs_buf.push(i64::from_le_bytes(bytes));
                        slot_cursor += 1;
                    }

                    let meta = element_metadata(&w.info());
                    local_bb.add_way(new_id, w.tags(), &refs_buf, meta.as_ref());

                    // Emit old_way_id into the worker's way_map shard.
                    let old_way_id = w.id();
                    let bucket = way_id_bucket(old_way_id);
                    id_buf = old_way_id.to_le_bytes();
                    if let Some(w) = shard.writers[bucket].as_mut() {
                        w.write_all(&id_buf).map_err(|e| e.to_string())?;
                    }
                    shard.entry_counts[bucket] += 1;
                    blob_way_count += 1;
                }
            }

            // Per-blob drift check — same invariant the sequential path
            // enforced. Misalignment here means stage 2a and stage 2d
            // disagreed on way iteration order and every subsequent
            // blob's refs would be shifted.
            if slot_cursor != expected_slot_end {
                return Err(format!(
                    "stage 2d slot cursor drift at way blob seq={}: \
                     cursor = {slot_cursor}, expected = {expected_slot_end} \
                     (blob start = {expected_slot_start})",
                    task.seq
                ));
            }

            flush_local(&mut local_bb, &mut output_blocks)?;

            // Worker-side way id counter advances by actual decoded
            // count (robust to minor indexdata inaccuracy).
            current_way_id = current_way_id
                .checked_add(
                    i64::try_from(blob_way_count)
                        .map_err(|_| "blob way count > i64".to_string())?,
                )
                .ok_or_else(|| "way id overflow in stage 2d".to_string())?;

            ways_written.fetch_add(blob_way_count, std::sync::atomic::Ordering::Relaxed);

            if decompress_buf.capacity() == 0 {
                decompress_buf = Vec::new();
            }

            Ok(std::mem::take(&mut output_blocks))
        })();

        if tx.send((task.seq, result)).is_err() {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Pass 1: parallel node scan — worker pool with range-based dispatch
// ---------------------------------------------------------------------------

/// Per-blob task for the parallel pass pool. `seq` is the filtered-index
/// position (monotonic within the per-kind blob list, used for writer
/// reorder). `data_offset` / `data_size` address the compressed blob body
/// for pread. `element_count` comes from the indexdata `BlobIndex.count`
/// and lets the caller precompute base new_ids without racing decode.
struct BlobTask {
    seq: usize,
    data_offset: u64,
    data_size: usize,
    element_count: u64,
}

/// Header-only scan building a per-kind schedule with element counts.
/// Requires indexed PBFs (all brokkr datasets are indexed): the per-blob
/// element count is read from `BlobIndex.count`, which is required to
/// precompute each blob's `base_new_id` without a full decode pass. If a
/// matching blob is missing indexdata, we error out with a pointer to
/// `brokkr cat` / indexed datasets.
fn build_kind_blob_schedule(
    input: &Path,
    kind: crate::blob_index::ElemKind,
) -> Result<Vec<BlobTask>> {
    let mut scanner = crate::blob::BlobReader::seekable_from_path(input)?;
    scanner.set_parse_indexdata(true);
    scanner
        .next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let mut schedule: Vec<BlobTask> = Vec::new();
    let mut seq: usize = 0;
    while let Some(result) = scanner.next_header_with_data_offset() {
        let (hdr, _frame_offset, data_offset, data_size) = result?;
        if !matches!(hdr.blob_type(), crate::blob::BlobType::OsmData) {
            continue;
        }
        let Some(idx) = hdr.index() else {
            return Err(
                "renumber --mode external requires an indexed PBF — run `brokkr cat` to add \
                 indexdata or use the indexed variant"
                    .into(),
            );
        };
        if idx.kind != kind {
            continue;
        }
        schedule.push(BlobTask {
            seq,
            data_offset,
            data_size,
            element_count: idx.count,
        });
        seq += 1;
    }
    Ok(schedule)
}

/// Two-worker parallel pass 1. Worker A processes blobs `0..split_idx`,
/// worker B processes `split_idx..`, both in monotonic file order.
/// Range-based (not work-stealing) dispatch preserves the per-shard
/// invariant that stage 2b relies on: each shard's bucket N contains
/// old_ids in strictly ascending order, AND shard A's old_ids are
/// disjoint from (and all less than) shard B's old_ids for any given
/// bucket. Together the two shards read as a concatenated sorted run.
///
/// Each worker owns: its node_map bucket shard (`&mut BucketWriters`),
/// a `BlockBuilder`, read_buf + decompress_buf scratch Vecs, an
/// `output_blocks: Vec<OwnedBlock>` staging buffer, and its own
/// nodes-written counter. All allocations stay worker-local — no
/// cross-thread malloc/free churn. PrimitiveBlocks drop on the worker
/// thread. Only `Vec<OwnedBlock>` (owned by the output-buffer Vec)
/// crosses the channel, bounded at ~32 items in flight.
///
/// The main thread consumes `(seq, Result<Vec<OwnedBlock>>)` from a
/// bounded mpsc channel and uses a `ReorderBuffer` to deliver blocks
/// in file order to the writer via `write_primitive_block_owned`.
///
/// The for_each_block_pipelined path was attempted first and OOMed at
/// 26 GB anon RSS on planet — cross-thread PrimitiveBlock retention via
/// glibc arena accumulation, exactly as notes/parallel-classify-
/// regression.md predicted. This pattern avoids that by keeping the
/// PrimitiveBlock lifecycle entirely worker-local.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn pass1_parallel_scan(
    schedule: &[BlobTask],
    split_idx: usize,
    start_node_id: i64,
    shared_file: &std::sync::Arc<std::fs::File>,
    node_map_shard_a: &mut BucketWriters,
    node_map_shard_b: &mut BucketWriters,
    nodes_written: &std::sync::atomic::AtomicU64,
    writer: &mut crate::writer::PbfWriter<crate::file_writer::FileWriter>,
) -> Result<()> {
    if schedule.is_empty() {
        return Ok(());
    }

    // Compute each worker's starting new_id. Worker A starts at
    // `start_node_id`; worker B starts after worker A's slice has
    // consumed `sum(element_count[0..split_idx])` ids.
    let a_element_count: u64 = schedule[..split_idx].iter().map(|t| t.element_count).sum();
    let a_start_new_id = start_node_id;
    let b_start_new_id = start_node_id
        .checked_add(
            i64::try_from(a_element_count).map_err(|_| "pass1 worker A count overflow")?,
        )
        .ok_or("pass1 worker B base new_id overflow")?;

    let tasks_a = &schedule[..split_idx];
    let tasks_b = &schedule[split_idx..];

    type DecodedItem = (usize, std::result::Result<Vec<OwnedBlock>, String>);
    let (decoded_tx, decoded_rx) = std::sync::mpsc::sync_channel::<DecodedItem>(32);

    std::thread::scope(|scope| -> Result<()> {
        // Worker A
        let file_a = std::sync::Arc::clone(shared_file);
        let tx_a = decoded_tx.clone();
        scope.spawn(move || {
            pass1_worker(
                tasks_a,
                a_start_new_id,
                &file_a,
                node_map_shard_a,
                nodes_written,
                &tx_a,
            );
        });

        // Worker B
        let file_b = std::sync::Arc::clone(shared_file);
        let tx_b = decoded_tx.clone();
        scope.spawn(move || {
            pass1_worker(
                tasks_b,
                b_start_new_id,
                &file_b,
                node_map_shard_b,
                nodes_written,
                &tx_b,
            );
        });

        drop(decoded_tx);

        // Consumer: reorder by `seq` and push OwnedBlocks to writer.
        let mut reorder: crate::reorder_buffer::ReorderBuffer<
            std::result::Result<Vec<OwnedBlock>, String>,
        > = crate::reorder_buffer::ReorderBuffer::with_capacity(64);

        for (seq_num, item) in decoded_rx {
            reorder.push(seq_num, item);
            while let Some(result) = reorder.pop_ready() {
                let blocks = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                for (block_bytes, index, tagdata) in blocks {
                    writer.write_primitive_block_owned(
                        block_bytes,
                        index,
                        tagdata.as_deref(),
                    )?;
                }
            }
        }

        // Each worker owns its own local BlockBuilder — the caller's
        // `bb` is never touched inside pass 1. Subsequent stages
        // reuse the caller's `bb` and start fresh, so there's nothing
        // to flush here.
        Ok(())
    })?;

    Ok(())
}

/// Per-worker loop: processes `tasks` in file order, emits node_map
/// entries into `shard`, and sends `(seq, Vec<OwnedBlock>)` through
/// `tx`. The worker owns all its scratch buffers. PrimitiveBlocks are
/// consumed and dropped inside this function, so no cross-thread
/// retention of decompressed payloads.
fn pass1_worker(
    tasks: &[BlobTask],
    worker_start_new_id: i64,
    shared_file: &std::sync::Arc<std::fs::File>,
    shard: &mut BucketWriters,
    nodes_written: &std::sync::atomic::AtomicU64,
    tx: &std::sync::mpsc::SyncSender<(usize, std::result::Result<Vec<OwnedBlock>, String>)>,
) {
    use std::os::unix::fs::FileExt as _;

    let mut read_buf: Vec<u8> = Vec::new();
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut local_bb = BlockBuilder::new();
    let mut output_blocks: Vec<OwnedBlock> = Vec::new();
    let mut id_buf = [0u8; OLD_ID_SIZE];
    let mut current_new_id = worker_start_new_id;

    for task in tasks {
        let base_new_id = current_new_id;
        let result: std::result::Result<Vec<OwnedBlock>, String> = (|| {
            read_buf.resize(task.data_size, 0);
            shared_file
                .read_exact_at(&mut read_buf, task.data_offset)
                .map_err(|e| format!("pread failed at offset {}: {e}", task.data_offset))?;
            crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)
                .map_err(|e| e.to_string())?;
            let block = crate::block::PrimitiveBlock::new(bytes::Bytes::from(
                std::mem::take(&mut decompress_buf),
            ))
            .map_err(|e| e.to_string())?;

            output_blocks.clear();
            let blob_node_count = pass1_process_blob(
                &block,
                &mut local_bb,
                &mut output_blocks,
                base_new_id,
                shard,
                &mut id_buf,
            )?;
            flush_local(&mut local_bb, &mut output_blocks)?;

            // Advance the worker's new_id cursor by this blob's
            // actual decoded node count. For indexed PBFs this
            // matches `task.element_count`; we use the decode count
            // so we're not dependent on indexdata accuracy.
            current_new_id = current_new_id
                .checked_add(
                    i64::try_from(blob_node_count)
                        .map_err(|_| "blob node count > i64".to_string())?,
                )
                .ok_or_else(|| "node id overflow in pass1".to_string())?;

            nodes_written.fetch_add(blob_node_count, std::sync::atomic::Ordering::Relaxed);

            if decompress_buf.capacity() == 0 {
                decompress_buf = Vec::new();
            }

            Ok(std::mem::take(&mut output_blocks))
        })();

        if tx.send((task.seq, result)).is_err() {
            break;
        }
    }
}

/// Process a single decoded blob: iterate elements, assign new ids,
/// emit `bb.add_node` into `output_blocks`, and write `old_id` entries
/// into this worker's `shard`. Returns the number of nodes emitted.
fn pass1_process_blob(
    block: &crate::block::PrimitiveBlock,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    base_new_id: i64,
    shard: &mut BucketWriters,
    id_buf: &mut [u8; OLD_ID_SIZE],
) -> std::result::Result<u64, String> {
    let mut count: u64 = 0;
    let mut next_id = base_new_id;
    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                reject_negative_id(dn.id(), "node").map_err(|e| e.to_string())?;
                ensure_node_capacity_local(bb, output)?;
                let new_id = next_id;
                next_id += 1;
                let meta = dense_node_metadata(dn);
                bb.add_node(
                    new_id,
                    dn.decimicro_lat(),
                    dn.decimicro_lon(),
                    dn.tags(),
                    meta.as_ref(),
                );
                let old_id = dn.id();
                let bucket = node_id_bucket(old_id);
                *id_buf = old_id.to_le_bytes();
                if let Some(w) = shard.writers[bucket].as_mut() {
                    w.write_all(id_buf).map_err(|e| e.to_string())?;
                }
                shard.entry_counts[bucket] += 1;
                count += 1;
            }
            Element::Node(n) => {
                reject_negative_id(n.id(), "node").map_err(|e| e.to_string())?;
                ensure_node_capacity_local(bb, output)?;
                let new_id = next_id;
                next_id += 1;
                let meta = element_metadata(&n.info());
                bb.add_node(
                    new_id,
                    n.decimicro_lat(),
                    n.decimicro_lon(),
                    n.tags(),
                    meta.as_ref(),
                );
                let old_id = n.id();
                let bucket = node_id_bucket(old_id);
                *id_buf = old_id.to_le_bytes();
                if let Some(w) = shard.writers[bucket].as_mut() {
                    w.write_all(id_buf).map_err(|e| e.to_string())?;
                }
                shard.entry_counts[bucket] += 1;
                count += 1;
            }
            _ => {}
        }
    }
    Ok(count)
}

fn build_blob_schedule(
    input: &Path,
    kind: crate::blob_index::ElemKind,
) -> Result<Vec<(u64, usize)>> {
    let mut scanner = crate::blob::BlobReader::seekable_from_path(input)?;
    scanner.set_parse_indexdata(true);
    scanner
        .next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let mut schedule: Vec<(u64, usize)> = Vec::new();
    while let Some(result) = scanner.next_header_with_data_offset() {
        let (hdr, _frame_offset, data_offset, data_size) = result?;
        if !matches!(hdr.blob_type(), crate::blob::BlobType::OsmData) {
            continue;
        }
        if let Some(idx) = hdr.index() {
            if idx.kind != kind {
                continue;
            }
        }
        schedule.push((data_offset, data_size));
    }
    Ok(schedule)
}

// ---------------------------------------------------------------------------
// Relation pass R1 + R2a (fused): assign new ids AND emit member refs
// ---------------------------------------------------------------------------

/// Single scan over relation blobs that performs both pass R1
/// (assign new relation ids + build in-memory `relation_map`) AND
/// pass R2a (emit `(old_id, slot_pos)` COO pairs for each node member
/// and way member into their respective bucket sets).
///
/// ## Why fusing is safe
///
/// The two passes are independent per-relation:
///
/// - R1 side: inserts `(r.id(), new_id)` into `relation_map`.
/// - R2a side: walks `r.members()` and emits node/way member CooPairs
///   into buckets. Does **not** consult `relation_map` — relation
///   members resolve directly from the in-memory map in R2d, not here.
///
/// So both halves operate on the current relation in isolation and can
/// share the same decoded block. Fusing saves one full relation scan
/// (decode + iterate), which at planet scale is ~14M relations spread
/// across ~13K relation blobs.
///
/// ## State
///
/// - `relation_map` is populated with every relation's (old, new) pair.
///   Must be fully loaded before R2d reads it — R2d runs after R2b/R2c,
///   by which point this scan has completed.
/// - `next_relation_id` advances monotonically.
/// - `node_slot_pos` and `way_slot_pos` are independent slot counters.
///   R2d walks relations in the same file order with matching counters,
///   so the flat resolved files produced by R2b/R2c line up 1:1 with
///   each member position.
///
/// Returns `(total_node_members, total_way_members)`.
///
/// Replaces the prior `relation_r1_assign_ids` + `relation_r2a_emit_member_refs`
/// pair from the first cut of task #4. Review finding #8 — one scan
/// instead of two.
#[hotpath::measure]
#[allow(clippy::too_many_arguments)]
fn relation_r1_r2a_fused(
    input: &Path,
    _direct_io: bool,
    relation_map: &mut FxHashMap<i64, i64>,
    next_relation_id: &mut i64,
    node_member_buckets: &mut BucketWriters,
    way_member_buckets: &mut BucketWriters,
) -> Result<(u64, u64)> {
    let schedule = build_blob_schedule(input, crate::blob_index::ElemKind::Relation)?;
    let shared_file = std::fs::File::open(input)
        .map_err(|e| format!("failed to open {}: {e}", input.display()))?;

    let pool = crate::blob::DecompressPool::new();
    let mut raw_buf: Vec<u8> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
    let mut pair_buf = [0u8; COO_PAIR_SIZE];

    let mut node_slot_pos: u64 = 0;
    let mut way_slot_pos: u64 = 0;

    use std::os::unix::fs::FileExt;
    for &(data_offset, data_size) in &schedule {
        raw_buf.resize(data_size, 0);
        shared_file
            .read_exact_at(&mut raw_buf, data_offset)
            .map_err(|e| format!("failed to pread relation blob at {data_offset}: {e}"))?;
        let mut decompress_buf = pool.get();
        crate::blob::decompress_blob_raw(&raw_buf, &mut decompress_buf)?;
        let block = crate::block::PrimitiveBlock::from_vec_pooled_with_scratch(
            decompress_buf,
            &pool,
            &mut st_scratch,
            &mut gr_scratch,
        )?;
        for element in block.elements() {
            if let Element::Relation(r) = &element {
                // R1 side: assign new id + record in relation_map.
                reject_negative_id(r.id(), "relation")?;
                let new_id = *next_relation_id;
                *next_relation_id += 1;
                relation_map.insert(r.id(), new_id);

                // R2a side: emit member refs for merge-join lookup.
                for m in r.members() {
                    match m.id {
                        MemberId::Node(old_id) => {
                            reject_negative_id(old_id, "relation node member")?;
                            let pair = CooPair {
                                old_node_id: old_id,
                                slot_pos: node_slot_pos,
                            };
                            let bucket = node_id_bucket(old_id);
                            pair.write_to(&mut pair_buf);
                            if let Some(w) = node_member_buckets.writers[bucket].as_mut() {
                                w.write_all(&pair_buf)?;
                            }
                            node_member_buckets.entry_counts[bucket] += 1;
                            node_slot_pos += 1;
                        }
                        MemberId::Way(old_id) => {
                            reject_negative_id(old_id, "relation way member")?;
                            let pair = CooPair {
                                old_node_id: old_id,
                                slot_pos: way_slot_pos,
                            };
                            let bucket = way_id_bucket(old_id);
                            pair.write_to(&mut pair_buf);
                            if let Some(w) = way_member_buckets.writers[bucket].as_mut() {
                                w.write_all(&pair_buf)?;
                            }
                            way_member_buckets.entry_counts[bucket] += 1;
                            way_slot_pos += 1;
                        }
                        MemberId::Relation(old_id) => {
                            reject_negative_id(old_id, "relation relation member")?;
                            // Resolved via in-memory relation_map in R2d.
                        }
                        // Unknown members preserve their old id.
                        MemberId::Unknown(_, _) => {}
                    }
                }
            }
        }
    }

    Ok((node_slot_pos, way_slot_pos))
}

// ---------------------------------------------------------------------------
// Relation pass R2d: rewrite member refs, write relations to output
// ---------------------------------------------------------------------------

/// Third (and final) scan over relation blobs. For each relation, look
/// up its new id from the in-memory relation_map, remap each member
/// (node/way members via the flat resolved files from R2c, relation
/// members via the in-memory relation_map), and write to output via
/// `bb.add_relation`.
///
/// Walks members in the exact same order as R2a, advancing
/// `node_slot_cursor` and `way_slot_cursor` in lockstep — so the n-th
/// node member encountered reads slot n of `node_member_new_refs`, and
/// likewise for ways. No per-relation index bookkeeping needed.
#[hotpath::measure]
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation
)]
fn relation_r2d_assembly(
    input: &Path,
    _direct_io: bool,
    writer: &mut crate::writer::PbfWriter<crate::write::file_writer::FileWriter>,
    bb: &mut BlockBuilder,
    node_member_new_refs_path: &Path,
    way_member_new_refs_path: &Path,
    total_node_members: u64,
    total_way_members: u64,
    relation_map: &FxHashMap<i64, i64>,
    stats: &mut RenumberStats,
) -> Result<()> {
    let node_mmap = open_new_refs_mmap(node_member_new_refs_path, total_node_members)?;
    let way_mmap = open_new_refs_mmap(way_member_new_refs_path, total_way_members)?;

    let schedule = build_blob_schedule(input, crate::blob_index::ElemKind::Relation)?;
    let shared_file = std::fs::File::open(input)
        .map_err(|e| format!("failed to open {}: {e}", input.display()))?;

    let pool = crate::blob::DecompressPool::new();
    let mut raw_buf: Vec<u8> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    let mut node_slot_cursor: u64 = 0;
    let mut way_slot_cursor: u64 = 0;

    use std::os::unix::fs::FileExt;
    for &(data_offset, data_size) in &schedule {
        raw_buf.resize(data_size, 0);
        shared_file
            .read_exact_at(&mut raw_buf, data_offset)
            .map_err(|e| format!("failed to pread relation blob at {data_offset}: {e}"))?;
        let mut decompress_buf = pool.get();
        crate::blob::decompress_blob_raw(&raw_buf, &mut decompress_buf)?;
        let block = crate::block::PrimitiveBlock::from_vec_pooled_with_scratch(
            decompress_buf,
            &pool,
            &mut st_scratch,
            &mut gr_scratch,
        )?;
        // members_buf borrows role strings from the block so it must not
        // outlive it — declare inside the blob loop.
        let mut members_buf: Vec<MemberData<'_>> = Vec::new();
        for element in block.elements() {
            let Element::Relation(r) = &element else { continue };
            ensure_relation_capacity(bb, writer)?;
            let new_id = relation_map.get(&r.id()).copied().ok_or_else(|| {
                format!(
                    "internal error: relation id {} missing from relation_map in R2d",
                    r.id()
                )
            })?;

            members_buf.clear();
            for m in r.members() {
                let remapped_id = match m.id {
                    MemberId::Node(_old_id) => {
                        let offset = node_slot_cursor as usize * NEW_REF_SIZE;
                        let bytes: [u8; NEW_REF_SIZE] = node_mmap
                            [offset..offset + NEW_REF_SIZE]
                            .try_into()
                            .map_err(|_| "R2d node member slice")?;
                        let new_node_id = i64::from_le_bytes(bytes);
                        node_slot_cursor += 1;
                        MemberId::Node(new_node_id)
                    }
                    MemberId::Way(_old_id) => {
                        let offset = way_slot_cursor as usize * NEW_REF_SIZE;
                        let bytes: [u8; NEW_REF_SIZE] = way_mmap
                            [offset..offset + NEW_REF_SIZE]
                            .try_into()
                            .map_err(|_| "R2d way member slice")?;
                        let new_way_id = i64::from_le_bytes(bytes);
                        way_slot_cursor += 1;
                        MemberId::Way(new_way_id)
                    }
                    MemberId::Relation(old_id) => MemberId::Relation(
                        relation_map.get(&old_id).copied().unwrap_or(old_id),
                    ),
                    MemberId::Unknown(t, id) => MemberId::Unknown(t, id),
                };
                members_buf.push(MemberData {
                    id: remapped_id,
                    role: m.role().unwrap_or(""),
                });
            }

            let meta = element_metadata(&r.info());
            bb.add_relation(new_id, r.tags(), &members_buf, meta.as_ref());
            stats.relations_written += 1;
        }
    }

    // Sanity: the walk must have consumed every resolved ref.
    if node_slot_cursor != total_node_members {
        return Err(format!(
            "R2d node cursor mismatch: walked {node_slot_cursor}, expected {total_node_members}"
        )
        .into());
    }
    if way_slot_cursor != total_way_members {
        return Err(format!(
            "R2d way cursor mismatch: walked {way_slot_cursor}, expected {total_way_members}"
        )
        .into());
    }

    Ok(())
}

/// Open the flat new_refs file produced by stage 2c as an mmap. Handles
/// the empty-file case with an anonymous zero-length mmap since some
/// kernels reject `memmap2::Mmap::map` on zero-length files.
fn open_new_refs_mmap(path: &Path, total_slots: u64) -> Result<memmap2::Mmap> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("failed to open new_refs file {}: {e}", path.display()))?;
    let len = file
        .metadata()
        .map_err(|e| format!("failed to stat new_refs file {}: {e}", path.display()))?
        .len();
    let expected_len = total_slots * NEW_REF_SIZE as u64;
    if len != expected_len {
        return Err(format!(
            "new_refs file {} size {len} != expected {expected_len} (total_slots={total_slots})",
            path.display()
        )
        .into());
    }
    if len == 0 {
        return Ok(memmap2::MmapOptions::new().map_anon()?.make_read_only()?);
    }
    let mmap = unsafe { memmap2::Mmap::map(&file) }
        .map_err(|e| format!("failed to mmap new_refs file {}: {e}", path.display()))?;
    #[cfg(unix)]
    mmap.advise(memmap2::Advice::Sequential).ok();
    Ok(mmap)
}
