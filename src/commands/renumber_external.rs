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
    dense_node_metadata, element_metadata, ensure_node_capacity, ensure_relation_capacity,
    ensure_way_capacity, flush_block, require_sorted, writer_from_header, HeaderOverrides,
    Result,
};
use crate::block_builder::{BlockBuilder, MemberData};
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

/// Serialized size of one `(old_id, new_id)` tuple on disk.
const ID_PAIR_SIZE: usize = 16;

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
// IdPair: the (old, new) tuple written to node_map bucket files
// ---------------------------------------------------------------------------

/// A pair mapping an old id to a new id. Serialized as two little-endian
/// i64s for a fixed 16-byte on-disk layout.
#[derive(Clone, Copy)]
struct IdPair {
    old_id: i64,
    new_id: i64,
}

impl IdPair {
    fn write_to(&self, buf: &mut [u8; ID_PAIR_SIZE]) {
        buf[..8].copy_from_slice(&self.old_id.to_le_bytes());
        buf[8..].copy_from_slice(&self.new_id.to_le_bytes());
    }

    fn read_from(buf: &[u8; ID_PAIR_SIZE]) -> Self {
        let old_id = i64::from_le_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ]);
        let new_id = i64::from_le_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]);
        Self { old_id, new_id }
    }
}

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
    // Same pattern as the in-memory renumber. require_sorted ensures we
    // read nodes in ascending old-id order, which makes the node_map
    // bucket files internally sorted by old_id with no extra sort step.
    let mut blob_reader = crate::blob::BlobReader::open(input, direct_io)?;
    let header_blob = blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let header = header_blob.to_headerblock()?;
    require_sorted(&header, input, "Input PBF")?;
    super::warn_locations_on_ways_loss(&header);

    let mut writer = writer_from_header(output, compression, &header, true, overrides, |hb| {
        hb.sorted()
    }, direct_io, false)?;
    let mut bb = BlockBuilder::new();

    // ---- Scratch dir + node_map buckets ----
    // Distinct name from external_join's "external-join" so concurrent
    // runs of the two commands don't collide.
    let scratch = ScratchDir::new(
        output.parent().unwrap_or(Path::new(".")),
        "renumber-external",
    )?;
    let mut node_map_buckets = BucketWriters::create(&scratch, "node-map")?;

    let mut next_node_id = opts.start_node_id;
    let mut next_way_id = opts.start_way_id;
    let mut next_relation_id = opts.start_relation_id;
    let mut relation_map: FxHashMap<i64, i64> = FxHashMap::default();
    let mut stats = RenumberStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
    };

    // Decompression buffer recycling: buffers flow from the pool into
    // each PrimitiveBlock via `from_vec_pooled_with_scratch` and return
    // to the pool on block drop. Without the pool, `std::mem::take`
    // would leave the caller's buf empty on every iteration, forcing a
    // fresh allocation per blob — at planet scale, that's ~27 GB of
    // cumulative alloc churn across pass 1 + stage 2a + stage 2d +
    // R1 + R2a + R2d.
    let pool = crate::blob::DecompressPool::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
    let mut pair_buf = [0u8; ID_PAIR_SIZE];

    crate::debug::emit_marker("RENUMBER_EXT_START");
    crate::debug::emit_marker("RENUMBER_EXT_PASS1_START");

    // ---- Pass 1: node scan ----
    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) { continue; }
        // Fast-skip non-node blobs via blob_index (all brokkr datasets are
        // indexed). Non-indexed PBFs fall through and decompress every
        // blob, matching the pass-2 pattern in the in-memory renumber.
        if let Some(idx) = blob.index() {
            if !matches!(idx.kind, crate::blob_index::ElemKind::Node) {
                continue;
            }
        }
        let mut decompress_buf = pool.get();
        blob.decompress_into(&mut decompress_buf)?;
        let block = crate::block::PrimitiveBlock::from_vec_pooled_with_scratch(
            decompress_buf, &pool, &mut st_scratch, &mut gr_scratch,
        )?;
        for element in block.elements() {
            match &element {
                Element::DenseNode(dn) => {
                    reject_negative_id(dn.id(), "node")?;
                    ensure_node_capacity(&mut bb, &mut writer)?;
                    let new_id = next_node_id;
                    next_node_id += 1;
                    let meta = dense_node_metadata(dn);
                    bb.add_node(
                        new_id, dn.decimicro_lat(), dn.decimicro_lon(), dn.tags(), meta.as_ref(),
                    );
                    let pair = IdPair { old_id: dn.id(), new_id };
                    emit_id_pair(
                        &mut node_map_buckets, &mut pair_buf, pair, node_id_bucket(pair.old_id),
                    )?;
                    stats.nodes_written += 1;
                }
                Element::Node(n) => {
                    reject_negative_id(n.id(), "node")?;
                    ensure_node_capacity(&mut bb, &mut writer)?;
                    let new_id = next_node_id;
                    next_node_id += 1;
                    let meta = element_metadata(&n.info());
                    bb.add_node(
                        new_id, n.decimicro_lat(), n.decimicro_lon(), n.tags(), meta.as_ref(),
                    );
                    let pair = IdPair { old_id: n.id(), new_id };
                    emit_id_pair(
                        &mut node_map_buckets, &mut pair_buf, pair, node_id_bucket(pair.old_id),
                    )?;
                    stats.nodes_written += 1;
                }
                // Ways and relations are deferred to pass 2 (task #3) and
                // relation passes (task #4). Skipping them here is fine for
                // the skeleton: the output PBF will contain only renumbered
                // nodes until those tasks land.
                _ => {}
            }
        }
    }

    crate::debug::emit_marker("RENUMBER_EXT_PASS1_END");
    drop(blob_reader);

    // Finalize the node_map bucket writers now that pass 1 is done emitting.
    // Files stay on disk until stage 2b consumes them (forthcoming); we just
    // need the underlying writers closed so the buffered bytes are durable.
    let node_map_counts = node_map_buckets.finish()?;
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
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2B_START");
    let mut slot_buckets = BucketWriters::create(&scratch, "slot")?;
    let resolved_count = stage2b_node_merge_join(
        &way_ref_buckets,
        &node_map_buckets,
        &mut slot_buckets,
        total_slots,
    )?;
    slot_buckets.finish()?;
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
    stage2c_slot_reorder(&slot_buckets, &new_refs_path, total_slots)?;
    slot_buckets.cleanup();
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2C_END");

    // ---- Pass 2 stage D: way assembly — rewrite refs + write output ----
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2D_START");
    let mut way_map_buckets = BucketWriters::create(&scratch, "way-map")?;
    stage2d_way_assembly(
        input,
        direct_io,
        &mut writer,
        &mut bb,
        &mut way_map_buckets,
        &new_refs_path,
        &ref_count_sidecar,
        total_slots,
        &mut next_way_id,
        &mut stats,
    )?;
    let way_map_counts = way_map_buckets.finish()?;
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
    // Reuses the same stage2b_node_merge_join function used for the way
    // pass. The only difference is the input buckets and their
    // partitioning (node_member_ref_buckets are partitioned by
    // node_id_bucket, matching node_map_buckets; way_member_ref_buckets
    // are partitioned by way_id_bucket, matching way_map_buckets).
    crate::debug::emit_marker("RENUMBER_EXT_R2B_START");
    let mut node_member_slot_buckets = BucketWriters::create(&scratch, "rel-node-slot")?;
    stage2b_node_merge_join(
        &node_member_ref_buckets,
        &node_map_buckets,
        &mut node_member_slot_buckets,
        total_node_members,
    )?;
    node_member_slot_buckets.finish()?;
    node_member_ref_buckets.cleanup();

    let mut way_member_slot_buckets = BucketWriters::create(&scratch, "rel-way-slot")?;
    stage2b_node_merge_join(
        &way_member_ref_buckets,
        &way_map_buckets,
        &mut way_member_slot_buckets,
        total_way_members,
    )?;
    way_member_slot_buckets.finish()?;
    way_member_ref_buckets.cleanup();
    crate::debug::emit_marker("RENUMBER_EXT_R2B_END");

    // ---- Relation pass R2c: slot reorder for each member type ----
    crate::debug::emit_marker("RENUMBER_EXT_R2C_START");
    let node_member_new_refs_path: PathBuf = scratch.file_path("rel-node-new-refs");
    stage2c_slot_reorder(
        &node_member_slot_buckets,
        &node_member_new_refs_path,
        total_node_members,
    )?;
    node_member_slot_buckets.cleanup();

    let way_member_new_refs_path: PathBuf = scratch.file_path("rel-way-new-refs");
    stage2c_slot_reorder(
        &way_member_slot_buckets,
        &way_member_new_refs_path,
        total_way_members,
    )?;
    way_member_slot_buckets.cleanup();
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

    drop(node_member_slot_buckets);
    drop(way_member_slot_buckets);
    drop(node_member_ref_buckets);
    drop(way_member_ref_buckets);
    drop(way_map_buckets);
    drop(slot_buckets);
    drop(way_ref_buckets);
    drop(node_map_buckets);
    drop(scratch);

    crate::debug::emit_marker("RENUMBER_EXT_END");

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Hot-path helper: write one IdPair into the correct node_map bucket
// ---------------------------------------------------------------------------

/// Serialize `pair` into the given `bucket` and increment the bucket's
/// entry counter. The caller chooses the bucket via `node_id_bucket`
/// (pass 1 node_map emit) or `way_id_bucket` (stage 2d way_map emit).
fn emit_id_pair(
    buckets: &mut BucketWriters,
    pair_buf: &mut [u8; ID_PAIR_SIZE],
    pair: IdPair,
    bucket: usize,
) -> Result<()> {
    pair.write_to(pair_buf);
    if let Some(w) = buckets.writers[bucket].as_mut() {
        w.write_all(pair_buf)?;
    }
    buckets.entry_counts[bucket] += 1;
    Ok(())
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
#[hotpath::measure]
fn stage2a_way_ref_pass(
    input: &Path,
    _direct_io: bool,
    way_ref_buckets: &mut BucketWriters,
    ref_count_sidecar: &Path,
) -> Result<u64> {
    // Schedule + pread pattern: only way blobs are pulled from disk.
    // Stage 2d reuses `build_blob_schedule` with the same `ElemKind::Way`
    // filter so the blob set + order are identical between this pass
    // and the way-assembly pass — the per-blob ref-count sidecar written
    // below stays in lockstep with stage 2d's blob iteration.
    let schedule = build_blob_schedule(input, crate::blob_index::ElemKind::Way)?;
    let shared_file = std::fs::File::open(input)
        .map_err(|e| format!("failed to open {}: {e}", input.display()))?;

    let mut raw_buf: Vec<u8> = Vec::new();
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut slot_pos: u64 = 0;
    let mut pair_buf = [0u8; COO_PAIR_SIZE];
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut group_starts: Vec<(usize, usize)> = Vec::new();

    let mut sidecar_writer = BufWriter::with_capacity(
        64 * 1024,
        std::fs::File::create(ref_count_sidecar)
            .map_err(|e| format!("failed to create ref-count sidecar: {e}"))?,
    );

    use std::os::unix::fs::FileExt;
    for &(data_offset, data_size) in &schedule {
        raw_buf.resize(data_size, 0);
        shared_file
            .read_exact_at(&mut raw_buf, data_offset)
            .map_err(|e| format!("failed to pread way blob at {data_offset}: {e}"))?;
        crate::blob::decompress_blob_raw(&raw_buf, &mut decompress_buf)?;
        let blob_start_pos = slot_pos;
        // The scan_way_refs callback is FnMut, so it can't return Result.
        // Stash the first error (I/O or negative-id rejection) and bail
        // after scan returns.
        let mut scan_err: Option<crate::error::Error> = None;
        super::way_scanner::scan_way_refs(
            &decompress_buf,
            &mut refs_buf,
            &mut group_starts,
            |way_id, refs| {
                if scan_err.is_some() {
                    return;
                }
                if way_id < 0 {
                    scan_err = Some(crate::error::new_error(
                        crate::error::ErrorKind::Io(std::io::Error::other(format!(
                            "renumber --mode external requires non-negative input ids. \
                             Input contains way id {way_id}. \
                             Use --mode inmem for files with negative (editor-local) ids."
                        ))),
                    ));
                    return;
                }
                for &old_node_id in refs {
                    if old_node_id < 0 {
                        scan_err = Some(crate::error::new_error(
                            crate::error::ErrorKind::Io(std::io::Error::other(format!(
                                "renumber --mode external requires non-negative input ids. \
                                 Way {way_id} references negative node id {old_node_id}. \
                                 Use --mode inmem for files with negative (editor-local) ids."
                            ))),
                        ));
                        return;
                    }
                    let pair = CooPair { old_node_id, slot_pos };
                    let bucket = node_id_bucket(old_node_id);
                    pair.write_to(&mut pair_buf);
                    if let Some(w) = way_ref_buckets.writers[bucket].as_mut() {
                        if let Err(e) = w.write_all(&pair_buf) {
                            scan_err = Some(crate::error::new_error(
                                crate::error::ErrorKind::Io(e),
                            ));
                            return;
                        }
                    }
                    way_ref_buckets.entry_counts[bucket] += 1;
                    slot_pos += 1;
                }
            },
        )?;
        if let Some(e) = scan_err {
            return Err(e.into());
        }
        // Record this blob's ref count in the sidecar.
        let blob_ref_count = slot_pos - blob_start_pos;
        sidecar_writer.write_all(&blob_ref_count.to_le_bytes())?;
    }

    // Trailer: total ref count for alignment verification in stage 2d.
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

/// For each of the 256 node-id buckets: load the way-ref `CooPair`s into
/// RAM (sort by `old_node_id`), load the corresponding `node_map`
/// `IdPair`s into RAM (already sorted by `old_id` because pass 1 emits
/// in ascending input-file order), two-cursor merge-join, and emit
/// `(slot_pos, new_node_id)` resolved entries into slot buckets.
///
/// Orphan refs (way-refs whose `old_node_id` has no `node_map` entry)
/// fall through with `resolved_id = old_node_id`, matching the in-
/// memory renumber's `unwrap_or(old_id)` semantics. This keeps the
/// external and in-memory paths element-equivalent for inputs with
/// orphan refs.
///
/// Returns the total number of resolved entries emitted. Expected to
/// equal the `total_slots` returned by stage 2a.
///
/// **Temp disk discipline**: deletes each way-ref bucket file as soon
/// as its merge-join completes. Without this per-bucket cleanup, stage
/// 2b's peak temp disk footprint at planet scale would be `node_map
/// (166 GB) + way_ref (166 GB) + slot (136 GB) = 468 GB`. With cleanup,
/// it drops to `node_map + per-bucket way_ref (~650 MB) + slot =
/// ~303 GB`. node_map stays alive because the relation pass needs it.
#[hotpath::measure]
fn stage2b_node_merge_join(
    way_ref_buckets: &BucketWriters,
    node_map_buckets: &BucketWriters,
    slot_buckets: &mut BucketWriters,
    total_slots: u64,
) -> Result<u64> {
    let mut resolved_count: u64 = 0;
    let mut entry_buf = [0u8; RESOLVED_ENTRY_SIZE];

    // Scratch buffers reused across bucket loads. Prevents heap
    // accumulation at planet scale where 256 × ~650 MB bucket files
    // would otherwise leave ~166 GB of unreturned allocations behind.
    // Note: `load_coo_bucket` / `load_id_pair_bucket` explicitly drop
    // their data_buf backing store after parsing, so the raw bytes
    // aren't held alongside the parsed Vecs during the merge-join.
    let mut way_refs: Vec<CooPair> = Vec::new();
    let mut way_refs_data: Vec<u8> = Vec::new();
    let mut way_refs_scratch: Vec<CooPair> = Vec::new();
    let mut node_map: Vec<IdPair> = Vec::new();
    let mut node_map_data: Vec<u8> = Vec::new();

    for bucket_idx in 0..NUM_BUCKETS {
        if way_ref_buckets.entry_counts[bucket_idx] == 0 {
            continue;
        }
        load_coo_bucket(
            &way_ref_buckets.paths[bucket_idx],
            &mut way_refs_data,
            &mut way_refs,
        )?;
        // Delete the way_ref bucket file now that we've read it into
        // RAM — no further stage consumes it. Cuts peak temp disk.
        drop(std::fs::remove_file(&way_ref_buckets.paths[bucket_idx]));
        // LSD radix sort by old_node_id (u64 key reinterpret). At planet
        // scale this bucket contains ~40M pairs and the comparison-based
        // `sort_unstable_by_key` costs ~500 s total over all 256 buckets
        // — the dominant contribution to stage 2b wall time per the
        // sidecar profile. Radix sort cuts this to ~60-80 s.
        //
        // Negative ids are rejected upstream (`reject_negative_id`), so
        // `old_node_id as u64` preserves the signed order exactly. OSM's
        // current ~13B node id max fits in 34 bits; we do 5 × 8-bit
        // passes (40 bits = 1 T id headroom).
        radix_sort_coo_pairs(&mut way_refs, &mut way_refs_scratch);

        // node_map is already sorted: pass 1 scans a sorted input and emits
        // (old_id, new_id) in file order, so within a bucket (same high
        // bits of old_id) the pairs are in ascending old_id order.
        node_map.clear();
        if node_map_buckets.entry_counts[bucket_idx] > 0 {
            load_id_pair_bucket(
                &node_map_buckets.paths[bucket_idx],
                &mut node_map_data,
                &mut node_map,
            )?;
        }

        // Two-cursor merge. Both sides sorted by old id; the node_map
        // cursor only moves forward, so the walk is O(way_refs + node_map).
        let mut nm_cursor: usize = 0;
        for wr in &way_refs {
            while nm_cursor < node_map.len() && node_map[nm_cursor].old_id < wr.old_node_id {
                nm_cursor += 1;
            }
            let resolved_id = if nm_cursor < node_map.len()
                && node_map[nm_cursor].old_id == wr.old_node_id
            {
                node_map[nm_cursor].new_id
            } else {
                // Orphan ref: no matching node in input. Preserve old id,
                // matching in-memory renumber's unwrap_or(old_id) policy.
                wr.old_node_id
            };
            let entry = ResolvedEntry {
                slot_pos: wr.slot_pos,
                new_node_id: resolved_id,
            };
            let sb = entry.slot_bucket(total_slots);
            entry.write_to(&mut entry_buf);
            if let Some(w) = slot_buckets.writers[sb].as_mut() {
                w.write_all(&entry_buf)?;
            }
            slot_buckets.entry_counts[sb] += 1;
            resolved_count += 1;
        }
    }

    Ok(resolved_count)
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
/// Empty slot buckets (a slot-pos range with no refs that landed in it)
/// get zero-byte fills — harmless because stage 2d's way assembly reads
/// each slot indexed by the actual way's (blob_slot_start, ref_index)
/// and never touches an empty range.
#[hotpath::measure]
#[allow(clippy::cast_possible_truncation)]
fn stage2c_slot_reorder(
    slot_buckets: &BucketWriters,
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

        if slot_buckets.entry_counts[bucket_idx] == 0 {
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

        data_buf.clear();
        let file = std::fs::File::open(&slot_buckets.paths[bucket_idx]).map_err(|e| {
            format!(
                "failed to open slot bucket {}: {e}",
                slot_buckets.paths[bucket_idx].display()
            )
        })?;
        std::io::Read::read_to_end(&mut &file, &mut data_buf).map_err(|e| {
            format!(
                "failed to read slot bucket {}: {e}",
                slot_buckets.paths[bucket_idx].display()
            )
        })?;
        #[cfg(feature = "linux-direct-io")]
        super::external_radix::advise_dontneed_file(&file);

        if !data_buf.len().is_multiple_of(RESOLVED_ENTRY_SIZE) {
            return Err(format!(
                "slot bucket {} is {} bytes, not a multiple of {RESOLVED_ENTRY_SIZE} — truncated or corrupt",
                slot_buckets.paths[bucket_idx].display(),
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

/// Load a bucket file of `IdPair` tuples into the provided `pairs` Vec.
/// Counterpart of `load_coo_bucket` for the node_map / way_map side.
/// See `load_coo_bucket` for the `data_buf` shrink-to-zero rationale.
#[allow(clippy::cast_possible_truncation)]
fn load_id_pair_bucket(
    path: &Path,
    data_buf: &mut Vec<u8>,
    pairs: &mut Vec<IdPair>,
) -> Result<()> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("failed to open id_pair bucket {}: {e}", path.display()))?;
    let len = file
        .metadata()
        .map_err(|e| format!("failed to stat id_pair bucket {}: {e}", path.display()))?
        .len() as usize;
    if !len.is_multiple_of(ID_PAIR_SIZE) {
        return Err(format!(
            "id_pair bucket {} is {len} bytes, not a multiple of {ID_PAIR_SIZE} — truncated or corrupt",
            path.display()
        )
        .into());
    }
    data_buf.clear();
    data_buf.resize(len, 0);
    std::io::Read::read_exact(&mut &file, data_buf)
        .map_err(|e| format!("failed to read id_pair bucket {}: {e}", path.display()))?;
    #[cfg(feature = "linux-direct-io")]
    super::external_radix::advise_dontneed_file(&file);

    let count = data_buf.len() / ID_PAIR_SIZE;
    pairs.clear();
    pairs.reserve_exact(count.saturating_sub(pairs.capacity()));
    let mut buf = [0u8; ID_PAIR_SIZE];
    for chunk in data_buf.chunks_exact(ID_PAIR_SIZE) {
        buf.copy_from_slice(chunk);
        pairs.push(IdPair::read_from(&buf));
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
#[hotpath::measure]
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation
)]
fn stage2d_way_assembly(
    input: &Path,
    _direct_io: bool,
    writer: &mut crate::writer::PbfWriter<crate::write::file_writer::FileWriter>,
    bb: &mut BlockBuilder,
    way_map_buckets: &mut BucketWriters,
    new_refs_path: &Path,
    ref_count_sidecar: &Path,
    total_slots: u64,
    next_way_id: &mut i64,
    stats: &mut RenumberStats,
) -> Result<()> {
    // Load sidecar and compute per-blob starting slot positions.
    let blob_slot_starts = load_ref_count_sidecar(ref_count_sidecar, total_slots)?;

    // mmap the flat new_refs file for zero-syscall slot lookups. Stage 2d
    // accesses slots sequentially as it walks way refs in file order, so
    // MADV_SEQUENTIAL gives the kernel a hint for readahead. Matches
    // external_join::CoordSlots.
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
    // The zero-length case is legitimate: input had no way refs at all
    // (e.g. an empty or nodes-only PBF). mmap rejects zero-length maps on
    // some kernels; fall back to an anonymous empty mmap in that case.
    let new_refs_mmap: memmap2::Mmap = if new_refs_len == 0 {
        memmap2::MmapOptions::new().map_anon()?.make_read_only()?
    } else {
        let mmap = unsafe { memmap2::Mmap::map(&new_refs_file) }
            .map_err(|e| format!("failed to mmap new_refs file: {e}"))?;
        #[cfg(unix)]
        mmap.advise(memmap2::Advice::Sequential).ok();
        mmap
    };

    // Second way scan via schedule + pread — same blob set and order
    // as stage 2a so the per-blob ref-count sidecar aligns 1:1.
    let schedule = build_blob_schedule(input, crate::blob_index::ElemKind::Way)?;
    if schedule.len() != blob_slot_starts.len() {
        return Err(format!(
            "stage 2d: way blob schedule size {} != sidecar entries {}",
            schedule.len(),
            blob_slot_starts.len()
        )
        .into());
    }
    let shared_file = std::fs::File::open(input)
        .map_err(|e| format!("failed to open {}: {e}", input.display()))?;

    let pool = crate::blob::DecompressPool::new();
    let mut way_blob_idx: usize = 0;
    let mut raw_buf: Vec<u8> = Vec::new();
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut pair_buf = [0u8; ID_PAIR_SIZE];
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    use std::os::unix::fs::FileExt;
    for &(data_offset, data_size) in &schedule {
        let mut slot_cursor = blob_slot_starts[way_blob_idx];

        raw_buf.resize(data_size, 0);
        shared_file
            .read_exact_at(&mut raw_buf, data_offset)
            .map_err(|e| format!("failed to pread way blob at {data_offset}: {e}"))?;
        let mut decompress_buf = pool.get();
        crate::blob::decompress_blob_raw(&raw_buf, &mut decompress_buf)?;
        let block = crate::block::PrimitiveBlock::from_vec_pooled_with_scratch(
            decompress_buf, &pool, &mut st_scratch, &mut gr_scratch,
        )?;

        for element in block.elements() {
            if let Element::Way(w) = &element {
                ensure_way_capacity(bb, writer)?;
                let new_id = *next_way_id;
                *next_way_id += 1;

                // Read ref_count consecutive new_node_ids from the flat file.
                refs_buf.clear();
                // Walk in file order. scan_way_refs in stage 2a and
                // block.elements() here both iterate groups in top-
                // level order and ways within a group in wire order,
                // so slot_cursor aligns with the emitted (old, slot)
                // pairs. The old ref ids themselves are discarded —
                // we only use the count to advance the cursor.
                for _ in w.refs() {
                    let offset = slot_cursor as usize * NEW_REF_SIZE;
                    let bytes: [u8; NEW_REF_SIZE] = new_refs_mmap
                        [offset..offset + NEW_REF_SIZE]
                        .try_into()
                        .map_err(|_| "stage 2d new_refs slice")?;
                    let new_node_id = i64::from_le_bytes(bytes);
                    refs_buf.push(new_node_id);
                    slot_cursor += 1;
                }

                let meta = element_metadata(&w.info());
                bb.add_way(new_id, w.tags(), &refs_buf, meta.as_ref());

                // Emit (old_way_id, new_way_id) into the way_map bucket.
                let pair = IdPair { old_id: w.id(), new_id };
                emit_id_pair(way_map_buckets, &mut pair_buf, pair, way_id_bucket(pair.old_id))?;

                stats.ways_written += 1;
            }
        }

        // Per-blob cross-check: slot_cursor must have advanced to exactly
        // the next blob's start (or total_slots for the last blob). Any
        // drift here indicates stage 2a and stage 2d disagreed on the way
        // iteration order, which would silently misalign every subsequent
        // blob's refs. This check catches the drift as soon as it happens
        // rather than after the whole file is written.
        let expected_end = blob_slot_starts
            .get(way_blob_idx + 1)
            .copied()
            .unwrap_or(total_slots);
        if slot_cursor != expected_end {
            return Err(format!(
                "stage 2d slot cursor drift at way blob {way_blob_idx}: \
                 cursor = {slot_cursor}, expected = {expected_end} \
                 (blob start = {})",
                blob_slot_starts[way_blob_idx]
            )
            .into());
        }

        way_blob_idx += 1;
    }

    if way_blob_idx != blob_slot_starts.len() {
        return Err(format!(
            "stage 2d: way blob count mismatch — scanned {way_blob_idx}, sidecar has {}",
            blob_slot_starts.len()
        )
        .into());
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Header-only scan helper: build a schedule of matching-type blobs
// ---------------------------------------------------------------------------

/// Walk the input PBF reading only blob headers (seeking past the
/// compressed bodies), and return a schedule of `(data_offset, data_size)`
/// for every OsmData blob whose `blob.index()` reports the requested
/// `ElemKind`. Non-indexed blobs are included unconditionally so the
/// caller's element-level dispatch still handles them — at the cost of
/// some wasted decompression on non-indexed inputs.
///
/// Uses `BlobReader::seekable_from_path` + `next_header_with_data_offset`
/// which seek past each blob's compressed body rather than reading it,
/// so this walk pays O(header_bytes) I/O instead of O(total_file_size).
/// At planet scale the header-walk is a few hundred MB vs 87 GB full
/// read.
///
/// The schedule is then consumed by a pread-based blob reader so only
/// matching blobs are ever pulled off disk. Matches the pattern used in
/// `src/commands/extract.rs` and `src/commands/external_join.rs` stage 2.
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
