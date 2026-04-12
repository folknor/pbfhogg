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
    element_metadata, ensure_relation_capacity, flush_block, require_sorted, writer_from_header,
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
    // Limit glibc malloc arenas to prevent cross-thread free
    // fragmentation. Without this, OwnedBlock Vec<u8>s allocated on
    // pass1/stage2d worker threads and freed on rayon compression
    // threads cause glibc arena accumulation growing to ~26 GB anon
    // RSS on planet. With 2 arenas the peak stays under 1 GB.
    // Scoped to this command — other pbfhogg commands are unaffected.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }

    // ---- Header validation + output writer setup ----
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

    // Per-worker IdSetDense bitsets. Merged after pass 1 to produce a
    // single bitset with rank index for O(1) new_id lookup in the fused
    // way scan. Replaces the old node_map shard bucket files entirely.
    const PASS1_WORKERS: usize = 6;
    let mut worker_id_sets: Vec<super::id_set_dense::IdSetDense> = (0..PASS1_WORKERS)
        .map(|_| super::id_set_dense::IdSetDense::new())
        .collect();

    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input).map_err(|e| format!("failed to open {}: {e}", input.display()))?,
    );

    let nodes_written_atomic = std::sync::atomic::AtomicU64::new(0);

    pass1_parallel_scan(
        &pass1_schedule,
        opts.start_node_id,
        &shared_file,
        &mut worker_id_sets,
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

    // ---- Merge per-worker IdSetDense bitsets and build rank index ----
    // Workers each built an independent bitset. Merge via bitwise OR,
    // then build the rank prefix sums for O(1) rank() lookup.
    let mut node_id_set = worker_id_sets.remove(0);
    for other in worker_id_sets {
        node_id_set.merge(other);
    }
    node_id_set.build_rank_index();
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter(
            "renumber_ext_node_map_entries",
            node_id_set.total_count() as i64,
        );
    }

    // ---- Fused way scan: resolve refs via rank + emit to flat file ----
    // Replaces old stage 2a (CooPair emission) + stage 2b (sort +
    // merge-join) in a single pass. For each way ref, the new node id
    // is `start_node_id + rank(old_node_id)` via the IdSetDense rank
    // index. No CooPair temp files, no radix sort, no merge-join.
    let way_schedule = build_kind_blob_schedule(input, crate::blob_index::ElemKind::Way)?;

    crate::debug::emit_marker("RENUMBER_EXT_FUSED_WAY_START");
    let ref_count_sidecar: PathBuf = scratch.file_path("way-ref-counts");
    let new_refs_path: PathBuf = scratch.file_path("new-refs");
    let total_slots = fused_way_resolve(
        input,
        &way_schedule,
        &node_id_set,
        opts.start_node_id,
        &ref_count_sidecar,
        &new_refs_path,
    )?;
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("renumber_ext_way_ref_slots", total_slots as i64);
    }
    crate::debug::emit_marker("RENUMBER_EXT_FUSED_WAY_END");

    // ---- Pass 2 stage D: way assembly — rewrite refs + write output ----
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2D_START");
    const STAGE2D_WORKERS: usize = 6;
    let mut way_id_sets: Vec<super::id_set_dense::IdSetDense> = (0..STAGE2D_WORKERS)
        .map(|_| super::id_set_dense::IdSetDense::new())
        .collect();
    let stage2d_ways_atomic = std::sync::atomic::AtomicU64::new(0);
    stage2d_parallel_way_assembly(
        input,
        &mut writer,
        &mut way_id_sets,
        &way_schedule,
        &new_refs_path,
        &ref_count_sidecar,
        total_slots,
        opts.start_way_id,
        &stage2d_ways_atomic,
    )?;
    stats.ways_written += stage2d_ways_atomic.load(std::sync::atomic::Ordering::Relaxed);
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
    // ---- Relation pass R2b+R2c: resolve member refs via rank ----
    // Replaces the old merge-join + slot-reorder pipeline for relation
    // members. For each (old_id, slot_pos) COO pair in the member ref
    // buckets, resolve new_id via IdSetDense rank and scatter directly
    // into the flat new_refs file via pwrite.
    crate::debug::emit_marker("RENUMBER_EXT_R2B_START");

    // Merge per-worker way_id_sets built during stage 2d.
    let mut way_id_set = way_id_sets.remove(0);
    for other in way_id_sets {
        way_id_set.merge(other);
    }
    way_id_set.build_rank_index();

    let node_member_new_refs_path: PathBuf = scratch.file_path("rel-node-new-refs");
    resolve_member_refs_via_rank(
        &node_member_ref_buckets,
        &node_id_set,
        opts.start_node_id,
        &node_member_new_refs_path,
        total_node_members,
    )?;
    node_member_ref_buckets.cleanup();

    let way_member_new_refs_path: PathBuf = scratch.file_path("rel-way-new-refs");
    resolve_member_refs_via_rank(
        &way_member_ref_buckets,
        &way_id_set,
        opts.start_way_id,
        &way_member_new_refs_path,
        total_way_members,
    )?;
    way_member_ref_buckets.cleanup();
    crate::debug::emit_marker("RENUMBER_EXT_R2B_END");
    // R2C is eliminated — resolved entries scatter directly to flat file.

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

    drop(node_member_ref_buckets);
    drop(way_member_ref_buckets);
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
    schedule: &[BlobTask],
    way_ref_buckets: &mut BucketWriters,
    ref_count_sidecar: &Path,
) -> Result<u64> {

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
        scope.spawn(move || {
            for task in schedule {
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

/// Number of 8-bit radix passes. 4 passes = 32 bits. Within any
/// single bucket, the ID range is `MAX_NODE_ID / 256 ≈ 55M < 2^32`,
/// so 4 passes covers the full within-bucket range. The 5th pass
/// (bits 32-39) was a no-op shuffle — all entries in one bucket have
/// the same byte 4 value because `55M < 4.3B`. Measured: 243 s
/// cumulative → ~194 s (−20%).
const RADIX_PASSES: usize = 4;

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
/// end of each pass. 4 passes = even number of swaps, so after the
/// loop the sorted data is back in `pairs` (no extra swap needed).
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
    /// Per-shard node_map Vecs. Each shard's bucket file is
    /// individually sorted (work-stealing FIFO + sorted PBF). The
    /// merge-join walks K cursors instead of materializing a
    /// combined sorted vector.
    node_maps: Vec<Vec<i64>>,
    node_map_data: Vec<u8>,
    entry_buf: [u8; RESOLVED_ENTRY_SIZE],
    // Per-worker timing accumulators.
    t_load_way_refs_ms: u64,
    t_radix_sort_ms: u64,
    t_load_node_map_ms: u64,
    t_merge_join_ms: u64,
}

impl Stage2bScratch {
    fn new() -> Self {
        Self {
            way_refs: Vec::new(),
            way_refs_data: Vec::new(),
            way_refs_scratch: Vec::new(),
            node_maps: Vec::new(),
            node_map_data: Vec::new(),
            entry_buf: [0u8; RESOLVED_ENTRY_SIZE],
            t_load_way_refs_ms: 0,
            t_radix_sort_ms: 0,
            t_load_node_map_ms: 0,
            t_merge_join_ms: 0,
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

    let t_load_wr = std::time::Instant::now();
    load_coo_bucket(
        &way_ref_buckets.paths[bucket_idx],
        &mut scratch.way_refs_data,
        &mut scratch.way_refs,
    )?;
    drop(std::fs::remove_file(&way_ref_buckets.paths[bucket_idx]));
    #[allow(clippy::cast_possible_truncation)]
    { scratch.t_load_way_refs_ms += t_load_wr.elapsed().as_millis() as u64; }

    let t_sort = std::time::Instant::now();
    radix_sort_coo_pairs(&mut scratch.way_refs, &mut scratch.way_refs_scratch);
    #[allow(clippy::cast_possible_truncation)]
    { scratch.t_radix_sort_ms += t_sort.elapsed().as_millis() as u64; }

    let t_load_nm = std::time::Instant::now();
    while scratch.node_maps.len() < node_map_shards.len() {
        scratch.node_maps.push(Vec::new());
    }
    for (i, shard) in node_map_shards.iter().enumerate() {
        scratch.node_maps[i].clear();
        load_single_old_id_bucket(
            shard,
            bucket_idx,
            &mut scratch.node_map_data,
            &mut scratch.node_maps[i],
        )?;
    }
    #[allow(clippy::cast_possible_truncation)]
    { scratch.t_load_node_map_ms += t_load_nm.elapsed().as_millis() as u64; }

    let bucket_base = bucket_new_id_starts[bucket_idx];
    let k = node_map_shards.len();
    let mut cursors = [0usize; 8];

    let t_merge = std::time::Instant::now();
    let mut resolved_count: u64 = 0;
    for wr in &scratch.way_refs {
        let target = wr.old_node_id;
        let mut merged_pos: usize = 0;
        let mut found = false;
        for (si, cursor_val) in cursors[..k].iter_mut().enumerate() {
            let nm = &scratch.node_maps[si];
            let c = cursor_val;
            while *c < nm.len() && nm[*c] < target {
                *c += 1;
            }
            merged_pos += *c;
            if *c < nm.len() && nm[*c] == target {
                found = true;
            }
        }
        let resolved_id = if found {
            bucket_base + merged_pos as i64
        } else {
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
    #[allow(clippy::cast_possible_truncation)]
    { scratch.t_merge_join_ms += t_merge.elapsed().as_millis() as u64; }

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
    slot_bucket_shards: &mut [BucketWriters],
    total_slots: u64,
) -> Result<u64> {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let next_bucket = AtomicUsize::new(0);
    type WorkerResult = std::result::Result<(u64, Stage2bScratch), String>;

    let worker_results: Vec<(u64, Stage2bScratch)> = std::thread::scope(|s| -> Result<Vec<(u64, Stage2bScratch)>> {
        let next_ref = &next_bucket;
        let nm_starts = bucket_new_id_starts;
        let nm_shards = node_map_shards;

        let mut remaining: &mut [BucketWriters] = slot_bucket_shards;
        let mut handles = Vec::new();
        for _ in 0..remaining.len() {
            let (head, tail) = remaining.split_at_mut(1);
            remaining = tail;
            let shard = &mut head[0];
            handles.push(s.spawn(move || -> WorkerResult {
                let mut scratch = Stage2bScratch::new();
                let mut count = 0u64;
                loop {
                    let i = next_ref.fetch_add(1, Ordering::Relaxed);
                    if i >= NUM_BUCKETS {
                        break;
                    }
                    count += stage2b_process_bucket(
                        i, way_ref_buckets, nm_shards, nm_starts,
                        shard, total_slots, &mut scratch,
                    )
                    .map_err(|e| e.to_string())?;
                }
                Ok((count, scratch))
            }));
        }

        let mut results = Vec::new();
        for (i, handle) in handles.into_iter().enumerate() {
            let r = handle
                .join()
                .map_err(|_| format!("stage 2b worker {i} panicked"))?
                .map_err(|e| format!("stage 2b worker {i}: {e}"))?;
            results.push(r);
        }
        Ok(results)
    })?;

    let mut total_count = 0u64;
    let mut t_load_wr = 0u64;
    let mut t_sort = 0u64;
    let mut t_load_nm = 0u64;
    let mut t_merge = 0u64;
    for (count, scratch) in &worker_results {
        total_count += count;
        t_load_wr += scratch.t_load_way_refs_ms;
        t_sort += scratch.t_radix_sort_ms;
        t_load_nm += scratch.t_load_node_map_ms;
        t_merge += scratch.t_merge_join_ms;
    }

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("stage2b_load_way_refs_ms", t_load_wr as i64);
        crate::debug::emit_counter("stage2b_radix_sort_ms", t_sort as i64);
        crate::debug::emit_counter("stage2b_load_node_map_ms", t_load_nm as i64);
        crate::debug::emit_counter("stage2b_merge_join_ms", t_merge as i64);
    }

    Ok(total_count)
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
    // Keep capacity for reuse across buckets — avoids realloc + page
    // re-fault on the next load. RSS stays bounded by the largest
    // bucket's data, which is the same whether we drop or reuse.
    data_buf.clear();
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
/// Parallel stage 2c: scatter resolved entries into the flat new_refs
/// file via pwrite. Each bucket covers a disjoint slot_pos range, so
/// workers process different buckets independently with no
/// synchronization on the output file. The file is pre-sized via
/// ftruncate so empty buckets are implicitly zero-filled (sparse).
#[hotpath::measure]
#[allow(clippy::cast_possible_truncation)]
fn stage2c_slot_reorder(
    slot_bucket_shards: &[&BucketWriters],
    new_refs_path: &Path,
    total_slots: u64,
) -> Result<()> {
    use std::os::unix::fs::FileExt as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let file = std::fs::File::create(new_refs_path).map_err(|e| {
        format!(
            "failed to create new_refs file {}: {e}",
            new_refs_path.display()
        )
    })?;
    // Pre-size the file. Empty regions read as zero (sparse hole on
    // ext4/xfs). No explicit zero-fill needed for empty buckets.
    file.set_len(total_slots * NEW_REF_SIZE as u64)?;

    let range_size = total_slots.div_ceil(NUM_BUCKETS as u64);
    let next_bucket = AtomicUsize::new(0);

    type WorkerResult = std::result::Result<(), String>;

    std::thread::scope(|s| -> Result<()> {
        let file_ref = &file;
        let next_ref = &next_bucket;
        let shards = slot_bucket_shards;

        // 2 workers — same as stage 2b. Each worker claims buckets
        // via atomic counter, processes independently.
        let handles: Vec<_> = (0..4)
            .map(|_| {
                s.spawn(move || -> WorkerResult {
                    let mut data_buf: Vec<u8> = Vec::new();
                    let mut scatter_buf: Vec<u8> = Vec::new();

                    loop {
                        let bucket_idx = next_ref.fetch_add(1, Ordering::Relaxed);
                        if bucket_idx >= NUM_BUCKETS {
                            break;
                        }

                        let bucket_start = bucket_idx as u64 * range_size;
                        let bucket_end =
                            ((bucket_idx as u64 + 1) * range_size).min(total_slots);
                        if bucket_start >= total_slots {
                            break;
                        }
                        let bucket_slots = bucket_end - bucket_start;

                        let total_bucket_entries: u64 = shards
                            .iter()
                            .map(|sh| sh.entry_counts[bucket_idx])
                            .sum();

                        if total_bucket_entries == 0 {
                            // Sparse file: hole already reads as zero.
                            continue;
                        }

                        let bucket_bytes = bucket_slots as usize * NEW_REF_SIZE;
                        scatter_buf.clear();
                        scatter_buf.resize(bucket_bytes, 0);

                        data_buf.clear();
                        for shard in shards {
                            if shard.entry_counts[bucket_idx] == 0 {
                                continue;
                            }
                            let f = std::fs::File::open(&shard.paths[bucket_idx])
                                .map_err(|e| format!(
                                    "failed to open slot bucket {}: {e}",
                                    shard.paths[bucket_idx].display()
                                ))?;
                            std::io::Read::read_to_end(&mut &f, &mut data_buf)
                                .map_err(|e| format!(
                                    "failed to read slot bucket {}: {e}",
                                    shard.paths[bucket_idx].display()
                                ))?;
                            #[cfg(feature = "linux-direct-io")]
                            super::external_radix::advise_dontneed_file(&f);
                        }

                        if !data_buf.len().is_multiple_of(RESOLVED_ENTRY_SIZE) {
                            return Err(format!(
                                "slot bucket {bucket_idx} shards total {} bytes, \
                                 not a multiple of {RESOLVED_ENTRY_SIZE}",
                                data_buf.len()
                            ));
                        }

                        let mut buf = [0u8; RESOLVED_ENTRY_SIZE];
                        for chunk in data_buf.chunks_exact(RESOLVED_ENTRY_SIZE) {
                            buf.copy_from_slice(chunk);
                            let slot_pos = u64::from_le_bytes(
                                buf[..8].try_into().unwrap_or_else(|_| unreachable!()),
                            );
                            let new_node_id_bytes = &buf[8..16];
                            let local_pos = (slot_pos - bucket_start) as usize;
                            let offset = local_pos * NEW_REF_SIZE;
                            scatter_buf[offset..offset + NEW_REF_SIZE]
                                .copy_from_slice(new_node_id_bytes);
                        }

                        // pwrite the scatter buffer at the bucket's
                        // file offset. Disjoint ranges — no lock needed.
                        let file_offset = bucket_start * NEW_REF_SIZE as u64;
                        file_ref
                            .write_all_at(&scatter_buf, file_offset)
                            .map_err(|e| format!("pwrite new_refs at {file_offset}: {e}"))?;
                    }
                    Ok(())
                })
            })
            .collect();

        for handle in handles {
            handle
                .join()
                .map_err(|_| "stage 2c worker panicked".to_string())?
                .map_err(|e| format!("stage 2c worker: {e}"))?;
        }

        Ok(())
    })?;

    // fsync so the data is durable before stage 2d mmaps it.
    file.sync_data()?;
    Ok(())
}

/// Load a single shard's `bucket_idx` file into `ids`, appending to
/// whatever is already in the Vec. Used by the two-cursor merge-join
/// path in `stage2b_process_bucket`.
fn load_single_old_id_bucket(
    shard: &BucketWriters,
    bucket_idx: usize,
    data_buf: &mut Vec<u8>,
    ids: &mut Vec<i64>,
) -> Result<()> {
    use std::io::Read as _;
    if shard.entry_counts[bucket_idx] == 0 {
        return Ok(());
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
    let count = data_buf.len() / OLD_ID_SIZE;
    ids.reserve(count.saturating_sub(ids.capacity()));
    let mut buf = [0u8; OLD_ID_SIZE];
    for chunk in data_buf.chunks_exact(OLD_ID_SIZE) {
        buf.copy_from_slice(chunk);
        ids.push(i64::from_le_bytes(buf));
    }
    // Keep capacity for reuse across buckets — avoids realloc + page
    // re-fault on the next load. RSS stays bounded by the largest
    // bucket's data, which is the same whether we drop or reuse.
    data_buf.clear();
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
    way_id_sets: &mut [super::id_set_dense::IdSetDense],
    way_schedule: &[BlobTask],
    new_refs_path: &Path,
    ref_count_sidecar: &Path,
    total_slots: u64,
    start_way_id: i64,
    ways_written: &std::sync::atomic::AtomicU64,
) -> Result<()> {
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

    if way_schedule.len() != blob_slot_starts.len() {
        return Err(format!(
            "stage 2d: way blob schedule size {} != sidecar entries {}",
            way_schedule.len(),
            blob_slot_starts.len()
        )
        .into());
    }

    if way_schedule.is_empty() {
        return Ok(());
    }

    let total_ways: u64 = way_schedule.iter().map(|t| t.element_count).sum();
    i64::try_from(total_ways).map_err(|_| "planet way count > i64")?;
    let mut base_way_ids: Vec<i64> = Vec::with_capacity(way_schedule.len());
    let mut cursor = start_way_id;
    for task in way_schedule {
        base_way_ids.push(cursor);
        cursor = cursor
            .checked_add(
                i64::try_from(task.element_count)
                    .map_err(|_| "stage 2d way count > i64 in prefix sum")?,
            )
            .ok_or("stage 2d base way_id overflow")?;
    }

    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?,
    );

    type DecodedItem = (usize, std::result::Result<Vec<OwnedBlock>, String>);
    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<&BlobTask>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (decoded_tx, decoded_rx) = std::sync::mpsc::sync_channel::<DecodedItem>(32);
    let schedule_ref = way_schedule;
    let base_ids_ref: &[i64] = &base_way_ids;
    let slots_ref: &[u64] = &blob_slot_starts;
    let stage2d_counters = StageCounters::new();
    let stage2d_cref = &stage2d_counters;

    std::thread::scope(|scope| -> Result<()> {
        // Dispatcher thread
        scope.spawn(move || {
            for task in schedule_ref {
                if desc_tx.send(task).is_err() {
                    break;
                }
            }
        });

        {
            let mut remaining_sets: &mut [super::id_set_dense::IdSetDense] = way_id_sets;
            for _ in 0..remaining_sets.len() {
                let (is, it) = remaining_sets.split_at_mut(1);
                remaining_sets = it;
                let id_set = &mut is[0];
                let rx = std::sync::Arc::clone(&desc_rx);
                let file = std::sync::Arc::clone(&shared_file);
                let mmap = std::sync::Arc::clone(&new_refs_mmap);
                let tx = decoded_tx.clone();
                scope.spawn(move || {
                    stage2d_worker(
                        &rx,
                        base_ids_ref,
                        &file,
                        &mmap,
                        slots_ref,
                        total_slots,
                        id_set,
                        ways_written,
                        stage2d_cref,
                        &tx,
                    );
                });
            }
        }

        drop(decoded_tx);
        drop(desc_rx);

        let mut reorder: crate::reorder_buffer::ReorderBuffer<
            std::result::Result<Vec<OwnedBlock>, String>,
        > = crate::reorder_buffer::ReorderBuffer::with_capacity(64);

        for (seq_num, item) in decoded_rx {
            reorder.push(seq_num, item);
            while let Some(result) = reorder.pop_ready() {
                let blocks = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                for (block_bytes, index, tagdata) in blocks {
                    let t0 = std::time::Instant::now();
                    writer.write_primitive_block_owned(
                        block_bytes,
                        index,
                        tagdata.as_deref(),
                    )?;
                    #[allow(clippy::cast_possible_truncation)]
                    stage2d_cref.consumer_write_ms.fetch_add(
                        t0.elapsed().as_millis() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
            }
        }
        Ok(())
    })?;

    stage2d_counters.emit("stage2d");
    Ok(())
}

/// Stage 2d per-worker loop. Claims blobs from a shared FIFO queue
/// and emits one owned-block batch per blob through the channel.
/// Looks up each blob's slot_cursor from the shared `blob_slot_starts`
/// (sidecar prefix sums) and its base_way_id from the pre-computed
/// prefix-sum array, so per-blob alignment is deterministic regardless
/// of dispatch order — same work-stealing shape as pass 1.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn stage2d_worker(
    rx: &std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<&BlobTask>>>,
    base_way_ids: &[i64],
    shared_file: &std::sync::Arc<std::fs::File>,
    new_refs_mmap: &std::sync::Arc<memmap2::Mmap>,
    blob_slot_starts: &[u64],
    total_slots: u64,
    way_id_set: &mut super::id_set_dense::IdSetDense,
    ways_written: &std::sync::atomic::AtomicU64,
    counters: &StageCounters,
    tx: &std::sync::mpsc::SyncSender<(usize, std::result::Result<Vec<OwnedBlock>, String>)>,
) {
    use std::os::unix::fs::FileExt as _;
    use std::sync::atomic::Ordering::Relaxed;

    let mut read_buf: Vec<u8> = Vec::new();
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut reframe_buf: Vec<u8> = Vec::new();
    let mut old_ids_buf: Vec<i64> = Vec::new();
    let mut refs_scratch: Vec<u8> = Vec::new();
    let mut group_scratch: Vec<u8> = Vec::new();
    let mut reframed_way_scratch: Vec<u8> = Vec::new();
    let mut output_blocks: Vec<OwnedBlock> = Vec::new();
    let mut way_group_ranges: Vec<(usize, usize)> = Vec::new();
    let mut way_scalar_fields: Vec<u8> = Vec::new();

    loop {
        let task = {
            let guard = rx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match guard.recv() {
                Ok(t) => t,
                Err(_) => break,
            }
        };

        let base_way_id = base_way_ids[task.seq];
        let expected_slot_start = blob_slot_starts[task.seq];
        let expected_slot_end = blob_slot_starts
            .get(task.seq + 1)
            .copied()
            .unwrap_or(total_slots);

        let result: std::result::Result<Vec<OwnedBlock>, String> = (|| {
            let t0 = std::time::Instant::now();
            read_buf.resize(task.data_size, 0);
            shared_file
                .read_exact_at(&mut read_buf, task.data_offset)
                .map_err(|e| format!("pread failed at offset {}: {e}", task.data_offset))?;
            #[allow(clippy::cast_possible_truncation)]
            counters.pread_ms.fetch_add(t0.elapsed().as_millis() as u64, Relaxed);

            let t1 = std::time::Instant::now();
            crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)
                .map_err(|e| e.to_string())?;
            #[allow(clippy::cast_possible_truncation)]
            counters.decompress_ms.fetch_add(t1.elapsed().as_millis() as u64, Relaxed);

            let t2 = std::time::Instant::now();
            reframe_buf.clear();
            old_ids_buf.clear();
            let blob_way_count = reframe_ways_with_new_ids(
                &decompress_buf,
                base_way_id,
                new_refs_mmap,
                expected_slot_start,
                expected_slot_end,
                &mut old_ids_buf,
                &mut reframe_buf,
                &mut refs_scratch,
                &mut group_scratch,
                &mut reframed_way_scratch,
                task.min_id < 0,
                &mut way_group_ranges,
                &mut way_scalar_fields,
            )?;
            #[allow(clippy::cast_possible_truncation)]
            counters.reframe_ms.fetch_add(t2.elapsed().as_millis() as u64, Relaxed);

            let t3 = std::time::Instant::now();
            // Set old_way_ids in the per-worker IdSetDense.
            // No bucket files — way_id_set replaces the external
            // way_map entirely for R2B rank lookup.
            for &old_way_id in &old_ids_buf {
                way_id_set.set(old_way_id);
            }
            #[allow(clippy::cast_possible_truncation)]
            counters.bucket_emit_ms.fetch_add(t3.elapsed().as_millis() as u64, Relaxed);

            // Build the OwnedBlock from the reframed bytes.
            let index = crate::blob_index::BlobIndex {
                kind: crate::blob_index::ElemKind::Way,
                min_id: base_way_id,
                #[allow(clippy::cast_possible_wrap)]
                max_id: base_way_id + blob_way_count as i64 - 1,
                count: blob_way_count,
                bbox: None,
            };
            output_blocks.clear();
            output_blocks.push((std::mem::take(&mut reframe_buf), index, None));

            if blob_way_count != task.element_count {
                return Err(format!(
                    "stage 2d blob {} decoded {} ways, indexdata said {}",
                    task.seq, blob_way_count, task.element_count
                ));
            }

            ways_written.fetch_add(blob_way_count, Relaxed);
            counters.blobs.fetch_add(1, Relaxed);

            Ok(std::mem::take(&mut output_blocks))
        })();

        let t4 = std::time::Instant::now();
        if tx.send((task.seq, result)).is_err() {
            break;
        }
        #[allow(clippy::cast_possible_truncation)]
        counters.send_ms.fetch_add(t4.elapsed().as_millis() as u64, Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Pass 1: parallel node scan — worker pool with work-stealing dispatch
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
    /// Source blob's min element ID from indexdata. Used for per-block
    /// negative-id skip: if min_id >= 0, all IDs are non-negative.
    min_id: i64,
    /// Source blob's spatial bbox (node blobs only).
    bbox: Option<crate::blob_index::BlobBbox>,
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
            min_id: idx.min_id,
            bbox: idx.bbox,
        });
        seq += 1;
    }
    Ok(schedule)
}

/// Two-worker parallel pass 1 via **work-stealing** dispatch.
///
/// Both workers pull blob tasks from a shared `Arc<Mutex<Receiver>>`
/// queue fed in monotonic file order by a dispatcher thread. This is a
/// deliberate departure from the original range-based split: range
/// splitting produced disjoint seq ranges `[0..split)` and
/// `[split..n)` which could *never* be interleaved in a single
/// `ReorderBuffer`, so the buffer accumulated worker B's entire
/// backlog (up to ~200k `Vec<OwnedBlock>`s at ~400 KB each = ~80 GB)
/// while worker A's range drained. Measured on planet as linear
/// ~118 MB/s anon-RSS growth, OOM-kill at 26 GB by t=295 s — see
/// commits `9695ad5` / `e7219f0` and `notes/renumber-planet-scale.md`
/// "Pass 1 memory blowup" for the full forensic.
///
/// Work-stealing keeps the reorder-buffer gap bounded by
/// `num_workers × channel_capacity` ≈ O(64) slots instead of
/// O(schedule_len / 2). Each worker still owns its own `node_map`
/// bucket shard (no cross-worker contention on BucketWriters) but the
/// shard is no longer disjoint-and-less-than its sibling — the two
/// shards interleave in id space. Stage 2b's
/// `load_old_id_bucket_shards` compensates by concatenating then
/// radix-sorting the combined old_id list, mirroring how stage 2b
/// already sorts the `way_refs` side.
///
/// Each worker owns: its node_map bucket shard, a local
/// `BlockBuilder`, read_buf + decompress_buf scratch Vecs, and an
/// `output_blocks: Vec<OwnedBlock>` staging buffer. All allocations
/// stay worker-local — PrimitiveBlocks drop on the worker thread,
/// only `Vec<OwnedBlock>` crosses the channel bounded at ~32 items.
/// The per-blob starting `new_id` is pre-computed in a prefix-sum
/// array so workers can process any blob out of FIFO order and still
/// know which `new_id` slice to assign.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn pass1_parallel_scan(
    schedule: &[BlobTask],
    start_node_id: i64,
    shared_file: &std::sync::Arc<std::fs::File>,
    id_sets: &mut [super::id_set_dense::IdSetDense],
    nodes_written: &std::sync::atomic::AtomicU64,
    writer: &mut crate::writer::PbfWriter<crate::file_writer::FileWriter>,
) -> Result<()> {
    if schedule.is_empty() {
        return Ok(());
    }

    // Pre-compute per-blob base new_id = start + sum(element_count[..seq]).
    // Workers look up `base_new_ids[task.seq]` instead of maintaining a
    // sequential counter — they may process tasks in any order.
    let mut base_new_ids: Vec<i64> = Vec::with_capacity(schedule.len());
    let mut cursor = start_node_id;
    for task in schedule {
        base_new_ids.push(cursor);
        cursor = cursor
            .checked_add(
                i64::try_from(task.element_count)
                    .map_err(|_| "planet node count > i64 in pass1 prefix sum")?,
            )
            .ok_or("pass1 base new_id overflow")?;
    }

    type DecodedItem = (usize, std::result::Result<Vec<OwnedBlock>, String>);
    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<&BlobTask>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (decoded_tx, decoded_rx) = std::sync::mpsc::sync_channel::<DecodedItem>(32);
    let base_ids_ref: &[i64] = &base_new_ids;
    let pass1_counters = StageCounters::new();
    let pass1_cref = &pass1_counters;

    std::thread::scope(|scope| -> Result<()> {
        // Dispatcher: feed schedule into the descriptor queue in file
        // order. Workers compete for items, so each shard receives a
        // FIFO-monotonic *subset* of the schedule.
        scope.spawn(move || {
            for task in schedule {
                if desc_tx.send(task).is_err() {
                    break;
                }
            }
        });

        {
            let mut remaining: &mut [super::id_set_dense::IdSetDense] = id_sets;
            for _ in 0..remaining.len() {
                let (head, tail) = remaining.split_at_mut(1);
                remaining = tail;
                let shard = &mut head[0];
                let rx = std::sync::Arc::clone(&desc_rx);
                let file = std::sync::Arc::clone(shared_file);
                let tx = decoded_tx.clone();
                scope.spawn(move || {
                    pass1_worker(
                        &rx,
                        base_ids_ref,
                        &file,
                        shard,
                        nodes_written,
                        pass1_cref,
                        &tx,
                    );
                });
            }
        }

        drop(decoded_tx);
        drop(desc_rx);

        let mut reorder: crate::reorder_buffer::ReorderBuffer<
            std::result::Result<Vec<OwnedBlock>, String>,
        > = crate::reorder_buffer::ReorderBuffer::with_capacity(64);

        for (seq_num, item) in decoded_rx {
            reorder.push(seq_num, item);
            while let Some(result) = reorder.pop_ready() {
                let blocks = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                for (block_bytes, index, tagdata) in blocks {
                    let t0 = std::time::Instant::now();
                    writer.write_primitive_block_owned(
                        block_bytes,
                        index,
                        tagdata.as_deref(),
                    )?;
                    #[allow(clippy::cast_possible_truncation)]
                    pass1_cref.consumer_write_ms.fetch_add(
                        t0.elapsed().as_millis() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
            }
        }

        Ok(())
    })?;

    pass1_counters.emit("pass1");
    Ok(())
}

/// Shared instrumentation counters for parallel worker stages.
/// All fields are AtomicU64 so workers can fetch_add concurrently.
/// Emit all counters via `emit()` after the scope joins workers.
struct StageCounters {
    pread_ms: std::sync::atomic::AtomicU64,
    decompress_ms: std::sync::atomic::AtomicU64,
    reframe_ms: std::sync::atomic::AtomicU64,
    bucket_emit_ms: std::sync::atomic::AtomicU64,
    send_ms: std::sync::atomic::AtomicU64,
    consumer_write_ms: std::sync::atomic::AtomicU64,
    blobs: std::sync::atomic::AtomicU64,
}

impl StageCounters {
    fn new() -> Self {
        Self {
            pread_ms: std::sync::atomic::AtomicU64::new(0),
            decompress_ms: std::sync::atomic::AtomicU64::new(0),
            reframe_ms: std::sync::atomic::AtomicU64::new(0),
            bucket_emit_ms: std::sync::atomic::AtomicU64::new(0),
            send_ms: std::sync::atomic::AtomicU64::new(0),
            consumer_write_ms: std::sync::atomic::AtomicU64::new(0),
            blobs: std::sync::atomic::AtomicU64::new(0),
        }
    }

    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    fn emit(&self, prefix: &str) {
        use std::sync::atomic::Ordering::Relaxed;
        crate::debug::emit_counter(&format!("{prefix}_pread_ms"), self.pread_ms.load(Relaxed) as i64);
        crate::debug::emit_counter(&format!("{prefix}_decompress_ms"), self.decompress_ms.load(Relaxed) as i64);
        crate::debug::emit_counter(&format!("{prefix}_reframe_ms"), self.reframe_ms.load(Relaxed) as i64);
        crate::debug::emit_counter(&format!("{prefix}_bucket_emit_ms"), self.bucket_emit_ms.load(Relaxed) as i64);
        crate::debug::emit_counter(&format!("{prefix}_send_ms"), self.send_ms.load(Relaxed) as i64);
        crate::debug::emit_counter(&format!("{prefix}_consumer_write_ms"), self.consumer_write_ms.load(Relaxed) as i64);
        crate::debug::emit_counter(&format!("{prefix}_blobs"), self.blobs.load(Relaxed) as i64);
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn pass1_worker(
    rx: &std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<&BlobTask>>>,
    base_new_ids: &[i64],
    shared_file: &std::sync::Arc<std::fs::File>,
    id_set: &mut super::id_set_dense::IdSetDense,
    nodes_written: &std::sync::atomic::AtomicU64,
    counters: &StageCounters,
    tx: &std::sync::mpsc::SyncSender<(usize, std::result::Result<Vec<OwnedBlock>, String>)>,
) {
    use std::os::unix::fs::FileExt as _;
    use std::sync::atomic::Ordering::Relaxed;

    let mut read_buf: Vec<u8> = Vec::new();
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut reframe_buf: Vec<u8> = Vec::new();
    let mut old_ids_buf: Vec<i64> = Vec::new();
    let mut output_blocks: Vec<OwnedBlock> = Vec::new();
    // Reusable scratch for reframe_dense_with_new_ids.
    let mut group_ranges_scratch: Vec<(usize, usize)> = Vec::new();
    let mut scalar_fields_scratch: Vec<u8> = Vec::new();
    let mut other_fields_scratch: Vec<u8> = Vec::new();
    let mut new_id_packed_scratch: Vec<u8> = Vec::new();
    let mut dense_out_scratch: Vec<u8> = Vec::new();
    let mut group_out_scratch: Vec<u8> = Vec::new();

    loop {
        let task = {
            let guard = rx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match guard.recv() {
                Ok(t) => t,
                Err(_) => break,
            }
        };

        let base_new_id = base_new_ids[task.seq];
        let result: std::result::Result<Vec<OwnedBlock>, String> = (|| {
            let t0 = std::time::Instant::now();
            read_buf.resize(task.data_size, 0);
            shared_file
                .read_exact_at(&mut read_buf, task.data_offset)
                .map_err(|e| format!("pread failed at offset {}: {e}", task.data_offset))?;
            #[allow(clippy::cast_possible_truncation)]
            counters.pread_ms.fetch_add(t0.elapsed().as_millis() as u64, Relaxed);

            let t1 = std::time::Instant::now();
            crate::blob::decompress_blob_raw(&read_buf, &mut decompress_buf)
                .map_err(|e| e.to_string())?;
            #[allow(clippy::cast_possible_truncation)]
            counters.decompress_ms.fetch_add(t1.elapsed().as_millis() as u64, Relaxed);

            let t2 = std::time::Instant::now();
            reframe_buf.clear();
            old_ids_buf.clear();
            let blob_node_count = reframe_dense_with_new_ids(
                &decompress_buf,
                base_new_id,
                &mut old_ids_buf,
                &mut reframe_buf,
                &mut group_ranges_scratch,
                &mut scalar_fields_scratch,
                &mut other_fields_scratch,
                &mut new_id_packed_scratch,
                &mut dense_out_scratch,
                &mut group_out_scratch,
            )?;
            #[allow(clippy::cast_possible_truncation)]
            counters.reframe_ms.fetch_add(t2.elapsed().as_millis() as u64, Relaxed);

            // Per-block negative-id check: skip per-element scan when
            // indexdata confirms min_id >= 0 (all planet blobs).
            if task.min_id < 0 {
                for &id in &old_ids_buf {
                    if id < 0 {
                        return Err(format!(
                            "renumber --mode external requires non-negative input ids. \
                             Input contains node id {id}. \
                             Use --mode inmem for files with negative (editor-local) ids."
                        ));
                    }
                }
            }

            let t3 = std::time::Instant::now();
            // Set all old_ids in the per-worker IdSetDense bitset.
            // No bucket files — the bitset + rank index replaces
            // the external node_map entirely.
            for &old_id in &old_ids_buf {
                id_set.set(old_id);
            }
            #[allow(clippy::cast_possible_truncation)]
            counters.bucket_emit_ms.fetch_add(t3.elapsed().as_millis() as u64, Relaxed);

            let index = crate::blob_index::BlobIndex {
                kind: crate::blob_index::ElemKind::Node,
                min_id: base_new_id,
                #[allow(clippy::cast_possible_wrap)]
                max_id: base_new_id + blob_node_count as i64 - 1,
                count: blob_node_count,
                bbox: task.bbox,
            };
            output_blocks.clear();
            output_blocks.push((std::mem::take(&mut reframe_buf), index, None));

            if blob_node_count != task.element_count {
                return Err(format!(
                    "pass1 blob {} decoded {} nodes, indexdata said {}",
                    task.seq, blob_node_count, task.element_count
                ));
            }

            nodes_written.fetch_add(blob_node_count, Relaxed);
            counters.blobs.fetch_add(1, Relaxed);

            Ok(std::mem::take(&mut output_blocks))
        })();

        let t4 = std::time::Instant::now();
        if tx.send((task.seq, result)).is_err() {
            break;
        }
        #[allow(clippy::cast_possible_truncation)]
        counters.send_ms.fetch_add(t4.elapsed().as_millis() as u64, Relaxed);
    }
}

// ---------------------------------------------------------------------------
// DenseNodes wire-format rewriter for pass 1
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Relation member resolve via rank
// ---------------------------------------------------------------------------

/// Resolve relation member refs directly via IdSetDense rank lookup.
/// Reads (old_id, slot_pos) COO pairs from the member ref buckets,
/// resolves each to new_id = start_id + rank(old_id), and scatters
/// the results into a flat file at position slot_pos * NEW_REF_SIZE
/// via pwrite. Replaces the old R2B merge-join + R2C slot-reorder.
#[allow(clippy::cast_possible_truncation)]
fn resolve_member_refs_via_rank(
    ref_buckets: &BucketWriters,
    id_set: &super::id_set_dense::IdSetDense,
    start_id: i64,
    output_path: &Path,
    total_slots: u64,
) -> Result<()> {
    use std::os::unix::fs::FileExt as _;

    if total_slots == 0 {
        std::fs::write(output_path, &[]).map_err(|e| format!("create empty refs: {e}"))?;
        return Ok(());
    }

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(output_path)
        .map_err(|e| format!("create member refs file: {e}"))?;
    let file_len = total_slots * NEW_REF_SIZE as u64;
    file.set_len(file_len)?;

    // mmap for scatter writes — avoids 137M pwrite syscalls.
    let mut mmap = unsafe { memmap2::MmapMut::map_mut(&file) }
        .map_err(|e| format!("mmap member refs file: {e}"))?;

    let mut data_buf: Vec<u8> = Vec::new();

    for bucket_idx in 0..NUM_BUCKETS {
        if ref_buckets.entry_counts[bucket_idx] == 0 {
            continue;
        }
        data_buf.clear();
        let f = std::fs::File::open(&ref_buckets.paths[bucket_idx])
            .map_err(|e| format!("open member ref bucket: {e}"))?;
        std::io::Read::read_to_end(&mut &f, &mut data_buf)
            .map_err(|e| format!("read member ref bucket: {e}"))?;

        for chunk in data_buf.chunks_exact(COO_PAIR_SIZE) {
            let old_id = i64::from_le_bytes(
                chunk[..8].try_into().unwrap_or_else(|_| unreachable!()),
            );
            let slot_pos = u64::from_le_bytes(
                chunk[8..16].try_into().unwrap_or_else(|_| unreachable!()),
            );
            #[allow(clippy::cast_possible_wrap)]
            let new_id = if id_set.get(old_id) {
                start_id + id_set.rank(old_id) as i64
            } else {
                old_id // orphan
            };
            let offset = (slot_pos as usize) * NEW_REF_SIZE;
            mmap[offset..offset + NEW_REF_SIZE]
                .copy_from_slice(&new_id.to_le_bytes());
        }
    }

    mmap.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Fused way resolve: replaces stage 2a + stage 2b
// ---------------------------------------------------------------------------

/// Single-pass fused way scan that resolves node refs via IdSetDense
/// rank() and writes the resolved new_node_ids directly to the flat
/// new_refs file. Replaces the old CooPair emission → radix sort →
/// merge-join pipeline entirely.
///
/// Workers decompress + scan way blobs in parallel, collecting per-blob
/// ref old_ids. For each old_id, the worker computes:
///   new_id = start_node_id + rank(old_id) if present, else old_id
/// and sends a Vec<i64> of new_node_ids to the consumer.
///
/// The consumer writes new_node_ids sequentially to the flat file
/// (one i64 LE per slot) and per-blob ref counts to the sidecar.
/// No intermediate bucket files, no sort, no merge-join.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn fused_way_resolve(
    input: &Path,
    schedule: &[BlobTask],
    node_id_set: &super::id_set_dense::IdSetDense,
    start_node_id: i64,
    ref_count_sidecar: &Path,
    new_refs_path: &Path,
) -> Result<u64> {
    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?,
    );

    let mut sidecar_writer = BufWriter::with_capacity(
        64 * 1024,
        std::fs::File::create(ref_count_sidecar)
            .map_err(|e| format!("failed to create ref-count sidecar: {e}"))?,
    );

    let mut refs_file = BufWriter::with_capacity(
        256 * 1024,
        std::fs::File::create(new_refs_path)
            .map_err(|e| format!("failed to create new_refs file: {e}"))?,
    );

    let mut slot_pos: u64 = 0;

    if schedule.is_empty() {
        sidecar_writer.write_all(&slot_pos.to_le_bytes())?;
        sidecar_writer.flush()?;
        return Ok(slot_pos);
    }

    // Workers: decompress + scan_way_refs + resolve via rank().
    // Send Vec<i64> of new_node_ids per blob to consumer.
    let decode_threads = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4)
        .min(6);

    type ScanItem = (usize, std::result::Result<Vec<i64>, String>);
    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<&BlobTask>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (scan_tx, scan_rx) = std::sync::mpsc::sync_channel::<ScanItem>(32);

    std::thread::scope(|scope| -> Result<()> {
        // Dispatcher
        scope.spawn(move || {
            for task in schedule {
                if desc_tx.send(task).is_err() {
                    break;
                }
            }
        });

        // Workers
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

                        // Scan way refs and resolve each via rank().
                        let mut new_ids: Vec<i64> = Vec::with_capacity(64 * 1024);
                        let mut scan_err: Option<String> = None;
                        super::way_scanner::scan_way_refs(
                            &decompress_buf,
                            &mut refs_buf,
                            &mut group_starts,
                            |_way_id, refs| {
                                if scan_err.is_some() {
                                    return;
                                }
                                for &old_node_id in refs {
                                    if old_node_id < 0 {
                                        scan_err = Some(format!(
                                            "renumber --mode external requires non-negative \
                                             input ids. Way references negative node id \
                                             {old_node_id}. Use --mode inmem for files with \
                                             negative (editor-local) ids."
                                        ));
                                        return;
                                    }
                                    let new_id = if node_id_set.get(old_node_id) {
                                        #[allow(clippy::cast_possible_wrap)]
                                        { start_node_id + node_id_set.rank(old_node_id) as i64 }
                                    } else {
                                        old_node_id // orphan
                                    };
                                    new_ids.push(new_id);
                                }
                            },
                        )
                        .map_err(|e| e.to_string())?;
                        if let Some(e) = scan_err {
                            return Err(e);
                        }
                        Ok(new_ids)
                    })();

                    if tx.send((task.seq, result)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(desc_rx);
        drop(scan_tx);

        // Consumer: reorder by seq, write to flat file + sidecar.
        let mut reorder: crate::reorder_buffer::ReorderBuffer<
            std::result::Result<Vec<i64>, String>,
        > = crate::reorder_buffer::ReorderBuffer::with_capacity(64);

        for (seq_num, item) in scan_rx {
            reorder.push(seq_num, item);
            while let Some(result) = reorder.pop_ready() {
                let new_ids =
                    result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                let blob_ref_count = new_ids.len() as u64;
                for new_id in new_ids {
                    refs_file.write_all(&new_id.to_le_bytes())?;
                }
                slot_pos += blob_ref_count;
                sidecar_writer.write_all(&blob_ref_count.to_le_bytes())?;
            }
        }
        Ok(())
    })?;

    // Trailer: total ref count for alignment verification in stage 2d.
    sidecar_writer.write_all(&slot_pos.to_le_bytes())?;
    sidecar_writer.flush()?;
    refs_file.flush()?;

    Ok(slot_pos)
}

/// Reframe a decompressed PrimitiveBlock by replacing only the DenseNodes
/// ID deltas while copying everything else (string table, lat/lon, tags,
/// metadata) verbatim at the byte level.
///
/// This is the renumber-specific fast path: renumber only changes IDs,
/// so we avoid the full decode→BlockBuilder→re-encode cycle. Per-node
/// cost drops from ~113 ns (HashMap lookups, delta arrays, metadata) to
/// ~10-15 ns (varint decode of old ID + varint encode of new delta).
///
/// Returns the number of nodes in the block. `old_ids_out` is populated
/// with the absolute old IDs (for bucket emission). `output` receives
/// the reframed PrimitiveBlock bytes.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn reframe_dense_with_new_ids(
    decompressed: &[u8],
    base_new_id: i64,
    old_ids_out: &mut Vec<i64>,
    output: &mut Vec<u8>,
    // Reusable scratch buffers — hoisted to worker level.
    group_ranges_scratch: &mut Vec<(usize, usize)>,
    scalar_fields_scratch: &mut Vec<u8>,
    other_fields_scratch: &mut Vec<u8>,
    new_id_packed_scratch: &mut Vec<u8>,
    dense_out_scratch: &mut Vec<u8>,
    group_out_scratch: &mut Vec<u8>,
) -> std::result::Result<u64, String> {
    use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

    group_ranges_scratch.clear();
    scalar_fields_scratch.clear();
    let mut stringtable_range: Option<(usize, usize)> = None;

    let mut cursor = Cursor::new(decompressed);
    while let Some((field, wire_type)) = cursor.read_tag().map_err(|e| e.to_string())? {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(|e| e.to_string())?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                stringtable_range = Some((offset, data.len()));
            }
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(|e| e.to_string())?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                group_ranges_scratch.push((offset, data.len()));
            }
            (17..=20, WIRE_VARINT) => {
                let raw = cursor.read_raw_field(wire_type).map_err(|e| e.to_string())?;
                protohoggr::encode_tag(scalar_fields_scratch, field, wire_type);
                scalar_fields_scratch.extend_from_slice(raw);
            }
            _ => cursor.skip_field(wire_type).map_err(|e| e.to_string())?,
        }
    }

    let (st_offset, st_len) = stringtable_range
        .ok_or("reframe: no StringTable in PrimitiveBlock")?;
    if group_ranges_scratch.is_empty() {
        return Err("reframe: no PrimitiveGroup in PrimitiveBlock".into());
    }
    let stringtable_bytes = &decompressed[st_offset..st_offset + st_len];

    // Phase 2-5: process each PrimitiveGroup, reframing its DenseNodes.
    output.clear();

    // PrimitiveBlock field 1: StringTable (copy verbatim)
    protohoggr::encode_bytes_field(output, 1, stringtable_bytes);

    old_ids_out.clear();
    let mut total_nodes: u64 = 0;
    let mut current_new_id = base_new_id;

    for &(gr_offset, gr_len) in group_ranges_scratch.iter() {
        let group_bytes = &decompressed[gr_offset..gr_offset + gr_len];

        let mut dense_data: Option<&[u8]> = None;
        let mut gr_cursor = Cursor::new(group_bytes);
        while let Some((field, wire_type)) = gr_cursor.read_tag().map_err(|e| e.to_string())? {
            if field == 2 && wire_type == WIRE_LEN {
                dense_data = Some(gr_cursor.read_len_delimited().map_err(|e| e.to_string())?);
            } else {
                gr_cursor.skip_field(wire_type).map_err(|e| e.to_string())?;
            }
        }

        let dense_bytes = dense_data.ok_or("reframe: no DenseNodes in PrimitiveGroup")?;

        let mut id_field: Option<&[u8]> = None;
        other_fields_scratch.clear();

        let mut dn_cursor = Cursor::new(dense_bytes);
        while let Some((field, wire_type)) = dn_cursor.read_tag().map_err(|e| e.to_string())? {
            if field == 1 && wire_type == WIRE_LEN {
                id_field = Some(dn_cursor.read_len_delimited().map_err(|e| e.to_string())?);
            } else {
                let raw = dn_cursor.read_raw_field(wire_type).map_err(|e| e.to_string())?;
                protohoggr::encode_tag(other_fields_scratch, field, wire_type);
                other_fields_scratch.extend_from_slice(raw);
            }
        }

        let id_bytes = id_field.ok_or("reframe: no packed ID field in DenseNodes")?;

        // Decode old ID deltas → absolute old IDs.
        let mut old_id: i64 = 0;
        let mut id_cursor = Cursor::new(id_bytes);
        let group_start_idx = old_ids_out.len();
        while id_cursor.remaining() > 0 {
            let delta = id_cursor.read_sint64().map_err(|e| e.to_string())?;
            old_id += delta;
            old_ids_out.push(old_id);
        }
        // Validate: reject negative IDs. Skip per-element checks when
        // the blob's indexdata min_id >= 0 (true for all planet blobs).
        // The min_id is passed in from the caller via the check_negatives flag.
        let group_node_count = (old_ids_out.len() - group_start_idx) as u64;
        total_nodes += group_node_count;

        // Build new packed ID field for this group.
        let gnc = usize::try_from(group_node_count)
            .map_err(|_| "group node count > usize")?;
        new_id_packed_scratch.clear();
        protohoggr::encode_varint(
            new_id_packed_scratch,
            protohoggr::zigzag_encode_64(current_new_id),
        );
        new_id_packed_scratch.extend(std::iter::repeat_n(0x02u8, gnc.saturating_sub(1)));
        #[allow(clippy::cast_possible_wrap)]
        {
            current_new_id += group_node_count as i64;
        }

        dense_out_scratch.clear();
        protohoggr::encode_bytes_field(dense_out_scratch, 1, new_id_packed_scratch);
        dense_out_scratch.extend_from_slice(other_fields_scratch);

        group_out_scratch.clear();
        protohoggr::encode_bytes_field(group_out_scratch, 2, dense_out_scratch);
        protohoggr::encode_bytes_field(output, 2, group_out_scratch);
    }

    output.extend_from_slice(scalar_fields_scratch);

    Ok(total_nodes)
}

/// Reframe a decompressed way-blob PrimitiveBlock by replacing way IDs
/// and node refs while copying everything else verbatim.
///
/// For each way: decode old way id (for bucket emission), assign new
/// sequential way id, look up each ref's new node id from the new_refs
/// mmap at the appropriate slot position, delta-encode the new refs,
/// and copy keys/vals/info/lat/lon raw bytes verbatim.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn reframe_ways_with_new_ids(
    decompressed: &[u8],
    base_new_way_id: i64,
    new_refs_mmap: &[u8],
    blob_slot_start: u64,
    expected_slot_end: u64,
    old_ids_out: &mut Vec<i64>,
    output: &mut Vec<u8>,
    refs_scratch: &mut Vec<u8>,
    group_scratch: &mut Vec<u8>,
    mut reframed_way_scratch: &mut Vec<u8>,
    check_negative_ids: bool,
    group_ranges_scratch: &mut Vec<(usize, usize)>,
    scalar_fields_scratch: &mut Vec<u8>,
) -> std::result::Result<u64, String> {
    use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

    group_ranges_scratch.clear();
    scalar_fields_scratch.clear();
    let mut stringtable_range: Option<(usize, usize)> = None;

    let mut cursor = Cursor::new(decompressed);
    while let Some((field, wire_type)) = cursor.read_tag().map_err(|e| e.to_string())? {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(|e| e.to_string())?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                stringtable_range = Some((offset, data.len()));
            }
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(|e| e.to_string())?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                group_ranges_scratch.push((offset, data.len()));
            }
            _ => {
                let raw = cursor.read_raw_field(wire_type).map_err(|e| e.to_string())?;
                protohoggr::encode_tag(scalar_fields_scratch, field, wire_type);
                scalar_fields_scratch.extend_from_slice(raw);
            }
        }
    }

    let (st_offset, st_len) = stringtable_range
        .ok_or("reframe_ways: no StringTable in PrimitiveBlock")?;
    let stringtable_bytes = &decompressed[st_offset..st_offset + st_len];

    output.clear();
    protohoggr::encode_bytes_field(output, 1, stringtable_bytes);

    old_ids_out.clear();
    let mut total_ways: u64 = 0;
    let mut current_new_id = base_new_way_id;
    let mut slot_cursor = blob_slot_start;

    for &(gr_offset, gr_len) in group_ranges_scratch.iter() {
        let group_bytes = &decompressed[gr_offset..gr_offset + gr_len];
        group_scratch.clear();

        let mut gr_cursor = Cursor::new(group_bytes);
        while let Some((field, wire_type)) = gr_cursor.read_tag().map_err(|e| e.to_string())? {
            if field == 3 && wire_type == WIRE_LEN {
                // Way submessage — splice-reframe it.
                // Find byte positions of field 1 (id) and field 8 (refs)
                // in way_bytes. Everything else is copied as contiguous
                // verbatim byte ranges — no per-field parse+re-encode.
                let way_bytes = gr_cursor.read_len_delimited().map_err(|e| e.to_string())?;

                // (tag_start, value_end) for fields we're replacing.
                let mut id_range: Option<(usize, usize)> = None;
                let mut refs_range: Option<(usize, usize)> = None;
                let mut old_way_id: i64 = 0;
                let mut old_refs_data: &[u8] = &[];

                let mut way_cursor = Cursor::new(way_bytes);
                while let Some((wf, wt)) = way_cursor.read_tag().map_err(|e| e.to_string())? {
                    // tag_start = position before read_raw_field consumed the value
                    let val_start = way_bytes.len() - way_cursor.remaining();
                    if wf == 1 && wt == WIRE_VARINT {
                        let tag_start = val_start - 1; // field 1 varint tag = 1 byte
                        old_way_id = way_cursor.read_varint_i64().map_err(|e| e.to_string())?;
                        let val_end = way_bytes.len() - way_cursor.remaining();
                        id_range = Some((tag_start, val_end));
                    } else if wf == 8 && wt == WIRE_LEN {
                        let tag_start = val_start - 1; // field 8 varint tag = 1 byte
                        old_refs_data = way_cursor.read_len_delimited().map_err(|e| e.to_string())?;
                        let val_end = way_bytes.len() - way_cursor.remaining();
                        refs_range = Some((tag_start, val_end));
                    } else {
                        way_cursor.read_raw_field(wt).map_err(|e| e.to_string())?;
                    }
                }

                if check_negative_ids && old_way_id < 0 {
                    return Err(format!(
                        "renumber --mode external requires non-negative input ids. \
                         Input contains way id {old_way_id}. \
                         Use --mode inmem for files with negative (editor-local) ids."
                    ));
                }
                old_ids_out.push(old_way_id);

                // Count refs via varint boundary scan.
                let ref_count: u64 = old_refs_data
                    .iter()
                    .filter(|&&b| b & 0x80 == 0)
                    .count() as u64;

                // Batch mmap read + delta-encode new refs.
                let slot_start = usize::try_from(slot_cursor)
                    .map_err(|_| "slot_cursor > usize")?;
                let rc = usize::try_from(ref_count).map_err(|_| "ref_count > usize")?;
                let mmap_start = slot_start * NEW_REF_SIZE;
                let mmap_end = mmap_start + rc * NEW_REF_SIZE;
                if mmap_end > new_refs_mmap.len() {
                    return Err(format!(
                        "mmap out of bounds: {mmap_end} > {}",
                        new_refs_mmap.len()
                    ));
                }
                refs_scratch.clear();
                let mut prev_new_ref: i64 = 0;
                for chunk in new_refs_mmap[mmap_start..mmap_end].chunks_exact(NEW_REF_SIZE) {
                    let new_ref = i64::from_le_bytes(
                        chunk.try_into().unwrap_or_else(|_| unreachable!()),
                    );
                    protohoggr::encode_varint(
                        refs_scratch,
                        protohoggr::zigzag_encode_64(new_ref - prev_new_ref),
                    );
                    prev_new_ref = new_ref;
                }
                slot_cursor += ref_count;

                // Splice: emit way_bytes with id and refs replaced.
                // Sort the two replacement ranges by start position to
                // handle any field order in the wire format.
                let id_r = id_range.ok_or("reframe_ways: no id field")?;
                let refs_r = refs_range.ok_or("reframe_ways: no refs field")?;
                let (first, second) = if id_r.0 < refs_r.0 {
                    (id_r, refs_r)
                } else {
                    (refs_r, id_r)
                };

                reframed_way_scratch.clear();
                // Bytes before first replaced field.
                reframed_way_scratch.extend_from_slice(&way_bytes[..first.0]);
                // First replacement.
                if first.0 == id_r.0 {
                    protohoggr::encode_int64_field(reframed_way_scratch, 1, current_new_id);
                } else {
                    protohoggr::encode_bytes_field(reframed_way_scratch, 8, refs_scratch);
                }
                // Bytes between first and second replaced fields.
                reframed_way_scratch.extend_from_slice(&way_bytes[first.1..second.0]);
                // Second replacement.
                if second.0 == refs_r.0 {
                    protohoggr::encode_bytes_field(reframed_way_scratch, 8, refs_scratch);
                } else {
                    protohoggr::encode_int64_field(reframed_way_scratch, 1, current_new_id);
                }
                // Bytes after second replaced field.
                reframed_way_scratch.extend_from_slice(&way_bytes[second.1..]);

                protohoggr::encode_bytes_field(group_scratch, 3, reframed_way_scratch);

                current_new_id += 1;
                total_ways += 1;
            } else {
                // Non-way field in the group — copy verbatim.
                let raw = gr_cursor.read_raw_field(wire_type).map_err(|e| e.to_string())?;
                protohoggr::encode_tag(group_scratch, field, wire_type);
                group_scratch.extend_from_slice(raw);
            }
        }

        protohoggr::encode_bytes_field(output, 2, group_scratch);
    }

    output.extend_from_slice(scalar_fields_scratch);

    // Drift check
    if slot_cursor != expected_slot_end {
        return Err(format!(
            "reframe_ways slot cursor drift: cursor = {slot_cursor}, \
             expected = {expected_slot_end} (blob start = {blob_slot_start})"
        ));
    }

    Ok(total_ways)
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
