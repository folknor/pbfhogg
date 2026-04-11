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
    dense_node_metadata, element_metadata, ensure_node_capacity, flush_block, require_sorted,
    writer_from_header, HeaderOverrides, Result,
};
use crate::block_builder::BlockBuilder;
use crate::writer::Compression;
use crate::Element;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Upper bound for node id partitioning. 14B gives headroom above the
/// current ~13B OSM node-id maximum; matches `external_join::MAX_NODE_ID`.
const MAX_NODE_ID: u64 = 14_000_000_000;

/// Serialized size of one `(old_id, new_id)` tuple on disk.
const ID_PAIR_SIZE: usize = 16;

/// Serialized size of one `(old_node_id, slot_pos)` COO pair on disk.
const COO_PAIR_SIZE: usize = 16;

/// Serialized size of one `(slot_pos, new_node_id)` resolved entry on disk.
const RESOLVED_ENTRY_SIZE: usize = 16;

/// Byte offset bucket index function. Partitions a `u64`-castable id into
/// one of `NUM_BUCKETS` buckets by its position in `[0, MAX_NODE_ID)`.
/// Negative ids clamp to bucket 0 — the external path currently accepts
/// negative ids through this clamp (a balance wart, not a correctness
/// one) rather than rejecting them at the entry gate; that policy
/// decision lives in the design doc's section 5 and can be revisited
/// when the renumber tests exercise negative input.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn node_id_bucket(id: i64) -> usize {
    let u = if id < 0 { 0u64 } else { id as u64 };
    let range_size = MAX_NODE_ID.div_ceil(NUM_BUCKETS as u64);
    let bucket = u / range_size;
    (bucket as usize).min(NUM_BUCKETS - 1)
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
    let mut stats = RenumberStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
    };

    let mut decompress_buf: Vec<u8> = Vec::new();
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
        blob.decompress_into(&mut decompress_buf)?;
        let block = crate::block::PrimitiveBlock::from_vec_with_scratch(
            std::mem::take(&mut decompress_buf), &mut st_scratch, &mut gr_scratch,
        )?;
        for element in block.elements() {
            match &element {
                Element::DenseNode(dn) => {
                    ensure_node_capacity(&mut bb, &mut writer)?;
                    let new_id = next_node_id;
                    next_node_id += 1;
                    let meta = dense_node_metadata(dn);
                    bb.add_node(
                        new_id, dn.decimicro_lat(), dn.decimicro_lon(), dn.tags(), meta.as_ref(),
                    );
                    emit_id_pair(
                        &mut node_map_buckets, &mut pair_buf,
                        IdPair { old_id: dn.id(), new_id },
                    )?;
                    stats.nodes_written += 1;
                }
                Element::Node(n) => {
                    ensure_node_capacity(&mut bb, &mut writer)?;
                    let new_id = next_node_id;
                    next_node_id += 1;
                    let meta = element_metadata(&n.info());
                    bb.add_node(
                        new_id, n.decimicro_lat(), n.decimicro_lon(), n.tags(), meta.as_ref(),
                    );
                    emit_id_pair(
                        &mut node_map_buckets, &mut pair_buf,
                        IdPair { old_id: n.id(), new_id },
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
        let bucket_total: u64 = way_ref_counts.iter().sum();
        debug_assert_eq!(
            bucket_total, total_slots,
            "stage 2a bucket entry sum must equal slot counter"
        );
    }
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
    debug_assert_eq!(
        resolved_count, total_slots,
        "stage 2b must emit exactly total_slots resolved entries (orphans included)"
    );
    // Way-ref buckets are no longer needed after the merge-join; the
    // resolved entries live in slot_buckets now. node_map_buckets stay
    // around for the relation passes in task #4.
    way_ref_buckets.cleanup();
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2B_END");

    // TODO(task #3 stages 2c/2d): scatter slot entries into a flat
    // new_refs file (2c), then re-stream ways, rewrite refs from that
    // file, and emit way_map tuples (2d).

    flush_block(&mut bb, &mut writer)?;
    writer.flush()?;

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

/// Serialize `pair` into the bucket matching its `old_id` high bits and
/// increment the bucket's entry counter. Matches the direct-field-access
/// pattern in `external_join::stage1_way_pass` for consistency.
fn emit_id_pair(
    buckets: &mut BucketWriters,
    pair_buf: &mut [u8; ID_PAIR_SIZE],
    pair: IdPair,
) -> Result<()> {
    let bucket = node_id_bucket(pair.old_id);
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
    direct_io: bool,
    way_ref_buckets: &mut BucketWriters,
    ref_count_sidecar: &Path,
) -> Result<u64> {
    let mut blob_reader = crate::blob::BlobReader::open(input, direct_io)?;
    // Consume the header blob so the iteration below starts at the first
    // OsmData blob, matching the stage1_way_pass convention.
    blob_reader
        .next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

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

    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
            continue;
        }
        // Fast-skip non-way blobs via blob_index for indexed PBFs. Stage 2d
        // MUST apply the identical filter during its re-scan so the blob
        // order and indices line up — both phases see the same blob set.
        if let Some(idx) = blob.index() {
            if !matches!(idx.kind, crate::blob_index::ElemKind::Way) {
                continue;
            }
        }

        blob.decompress_into(&mut decompress_buf)?;
        let blob_start_pos = slot_pos;
        // The scan_way_refs callback is FnMut, so it can't return Result.
        // Stash the first I/O error and bail after scan returns.
        let mut write_err: Option<std::io::Error> = None;
        super::way_scanner::scan_way_refs(
            &decompress_buf,
            &mut refs_buf,
            &mut group_starts,
            |_way_id, refs| {
                if write_err.is_some() {
                    return;
                }
                for &old_node_id in refs {
                    let pair = CooPair { old_node_id, slot_pos };
                    let bucket = node_id_bucket(old_node_id);
                    pair.write_to(&mut pair_buf);
                    if let Some(w) = way_ref_buckets.writers[bucket].as_mut() {
                        if let Err(e) = w.write_all(&pair_buf) {
                            write_err = Some(e);
                            return;
                        }
                    }
                    way_ref_buckets.entry_counts[bucket] += 1;
                    slot_pos += 1;
                }
            },
        )?;
        if let Some(e) = write_err {
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
    let mut way_refs: Vec<CooPair> = Vec::new();
    let mut way_refs_data: Vec<u8> = Vec::new();
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
        // Sort the COO pairs by old_node_id so the merge walk's nm cursor
        // can advance monotonically across the whole bucket.
        way_refs.sort_unstable_by_key(|p| p.old_node_id);

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
    data_buf.clear();
    data_buf.resize(len, 0);
    std::io::Read::read_exact(&mut &file, data_buf)
        .map_err(|e| format!("failed to read way_ref bucket {}: {e}", path.display()))?;
    #[cfg(feature = "linux-direct-io")]
    super::external_radix::advise_dontneed_file(&file);

    pairs.clear();
    let count = data_buf.len() / COO_PAIR_SIZE;
    if count > pairs.capacity() {
        pairs.reserve(count - pairs.capacity());
    }
    let mut buf = [0u8; COO_PAIR_SIZE];
    for chunk in data_buf.chunks_exact(COO_PAIR_SIZE) {
        buf.copy_from_slice(chunk);
        pairs.push(CooPair::read_from(&buf));
    }
    Ok(())
}

/// Load a bucket file of `IdPair` tuples into the provided `pairs` Vec.
/// Counterpart of `load_coo_bucket` for the node_map / way_map side.
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
    data_buf.clear();
    data_buf.resize(len, 0);
    std::io::Read::read_exact(&mut &file, data_buf)
        .map_err(|e| format!("failed to read id_pair bucket {}: {e}", path.display()))?;
    #[cfg(feature = "linux-direct-io")]
    super::external_radix::advise_dontneed_file(&file);

    pairs.clear();
    let count = data_buf.len() / ID_PAIR_SIZE;
    if count > pairs.capacity() {
        pairs.reserve(count - pairs.capacity());
    }
    let mut buf = [0u8; ID_PAIR_SIZE];
    for chunk in data_buf.chunks_exact(ID_PAIR_SIZE) {
        buf.copy_from_slice(chunk);
        pairs.push(IdPair::read_from(&buf));
    }
    Ok(())
}
