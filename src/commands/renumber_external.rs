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
//! This module currently implements **pass 1 only**. The output PBF
//! produced by a pass-1-only invocation contains renumbered nodes but
//! no ways or relations, so it is not a valid end-to-end renumber
//! result — only useful for harness-level testing of the node pass
//! until tasks #3 and #4 land.

use std::io::Write as _;
use std::path::Path;

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

    /// Bucket index for node-id partitioning. Matches
    /// `external_join::CooPair::node_bucket` — negative ids clamp to
    /// bucket 0 (a balance wart, not a correctness wart, since the
    /// external path rejects negative ids at the pass-1 entry gate).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn node_bucket(&self) -> usize {
        let id = if self.old_id < 0 { 0u64 } else { self.old_id as u64 };
        let range_size = MAX_NODE_ID.div_ceil(NUM_BUCKETS as u64);
        let bucket = id / range_size;
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

    // Flush in-flight node block before finishing the output. The writer
    // stays open across passes in the full implementation — for the
    // pass-1 skeleton we flush and close immediately.
    flush_block(&mut bb, &mut writer)?;
    writer.flush()?;

    // Flush + sync + fadvise the bucket files. We don't consume them here
    // (that's pass 2), but finishing makes the file contents durable and
    // frees the writer buffers. Total entry counts are summed across
    // buckets and emitted as a debug counter for sanity-checking against
    // stats.nodes_written.
    let bucket_counts = node_map_buckets.finish()?;
    #[allow(clippy::cast_possible_wrap)]
    {
        let total: u64 = bucket_counts.iter().sum();
        crate::debug::emit_counter("renumber_ext_node_map_entries", total as i64);
    }

    // Drop the scratch dir explicitly so the pass-1-only skeleton cleans
    // up its bucket files. Once pass 2 lands, the scratch dir lives across
    // both passes and only drops at end of function.
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
    let bucket = pair.node_bucket();
    pair.write_to(pair_buf);
    if let Some(w) = buckets.writers[bucket].as_mut() {
        w.write_all(pair_buf)?;
    }
    buckets.entry_counts[bucket] += 1;
    Ok(())
}
