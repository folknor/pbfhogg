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

use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::writer::{Compression, PbfWriter};
use crate::{BlobFilter, Element, ElementReader, PrimitiveBlock};

use super::add_locations_to_ways::Stats;
use super::id_set_dense::IdSetDense;
use super::{
    dense_node_metadata, drain_batch_results, element_metadata,
    ensure_node_capacity_local, ensure_relation_capacity_local, ensure_way_capacity_local,
    flush_local, require_indexdata, writer_from_header,
    HeaderOverrides, Result, BATCH_SIZE,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Number of buckets for radix partitioning. 256 = partition by high byte.
const NUM_BUCKETS: usize = 256;

/// Maximum node ID in current OSM data. Used to compute bucket ranges.
/// 14B gives headroom above the current ~13B maximum.
const MAX_NODE_ID: u64 = 14_000_000_000;

/// Size of the write buffer per bucket file (256 KB).
const BUCKET_BUF_SIZE: usize = 256 * 1024;

/// Size of a COO pair on disk: `(node_id: i64, slot_pos: u64)` = 16 bytes.
const COO_PAIR_SIZE: usize = 16;

/// Size of a resolved entry: `(slot_pos: u64, lat: i32, lon: i32)` = 16 bytes.
const RESOLVED_ENTRY_SIZE: usize = 16;

/// Size of a coordinate slot: `(lat: i32, lon: i32)` = 8 bytes.
const COORD_SLOT_SIZE: usize = 8;

// ---------------------------------------------------------------------------
// Scratch directory management
// ---------------------------------------------------------------------------

/// Managed scratch directory for bucket files. Cleaned up on drop.
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn new(parent: &Path) -> Result<Self> {
        let path = parent.join(format!(".pbfhogg-external-join-{}", std::process::id()));
        std::fs::create_dir_all(&path).map_err(|e| {
            format!("failed to create scratch directory {}: {e}", path.display())
        })?;
        Ok(Self { path })
    }

    fn bucket_path(&self, prefix: &str, index: usize) -> PathBuf {
        self.path.join(format!("{prefix}-{index:03}"))
    }

    fn file_path(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        // Best-effort cleanup. Ignore errors (crash leaves stale dir, user can clean).
        drop(std::fs::remove_dir_all(&self.path));
    }
}

// ---------------------------------------------------------------------------
// Bucket writers
// ---------------------------------------------------------------------------

/// Set of buffered writers for radix bucket files.
struct BucketWriters {
    writers: Vec<Option<BufWriter<std::fs::File>>>,
    paths: Vec<PathBuf>,
    entry_counts: Vec<u64>,
}

impl BucketWriters {
    /// Create bucket files eagerly. Each bucket gets a buffered writer.
    fn create(scratch: &ScratchDir, prefix: &str) -> Result<Self> {
        let mut writers = Vec::with_capacity(NUM_BUCKETS);
        let mut paths = Vec::with_capacity(NUM_BUCKETS);
        let entry_counts = vec![0u64; NUM_BUCKETS];

        for i in 0..NUM_BUCKETS {
            let path = scratch.bucket_path(prefix, i);
            let file = std::fs::File::create(&path)
                .map_err(|e| format!("failed to create bucket {}: {e}", path.display()))?;
            writers.push(Some(BufWriter::with_capacity(BUCKET_BUF_SIZE, file)));
            paths.push(path);
        }

        Ok(Self { writers, paths, entry_counts })
    }

    /// Flush and close all writers. Returns per-bucket entry counts.
    fn finish(&mut self) -> Result<Vec<u64>> {
        for writer in &mut self.writers {
            if let Some(w) = writer.as_mut() {
                w.flush()?;
            }
            *writer = None;
        }
        Ok(self.entry_counts.clone())
    }

    /// Delete all bucket files.
    fn cleanup(&self) {
        for path in &self.paths {
            drop(std::fs::remove_file(path));
        }
    }
}

// ---------------------------------------------------------------------------
// COO pair: (node_id, slot_pos)
// ---------------------------------------------------------------------------

/// A coordinate-list (COO) pair linking a node ID to a position in the
/// way-ref stream where its coordinates should be placed.
#[derive(Clone, Copy)]
struct CooPair {
    node_id: i64,
    slot_pos: u64,
}

impl CooPair {
    fn write_to(&self, buf: &mut [u8; COO_PAIR_SIZE]) {
        buf[..8].copy_from_slice(&self.node_id.to_le_bytes());
        buf[8..].copy_from_slice(&self.slot_pos.to_le_bytes());
    }

    fn read_from(buf: &[u8; COO_PAIR_SIZE]) -> Self {
        let node_id = i64::from_le_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ]);
        let slot_pos = u64::from_le_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]);
        Self { node_id, slot_pos }
    }

    /// Bucket index for node-id partitioning.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn node_bucket(&self) -> usize {
        let id = if self.node_id < 0 { 0u64 } else { self.node_id as u64 };
        let range_size = MAX_NODE_ID.div_ceil(NUM_BUCKETS as u64);
        let bucket = id / range_size;
        (bucket as usize).min(NUM_BUCKETS - 1)
    }
}

// ---------------------------------------------------------------------------
// Resolved entry: (slot_pos, lat, lon)
// ---------------------------------------------------------------------------

/// A resolved coordinate ready to be placed into the final coord_slots file.
#[derive(Clone, Copy)]
struct ResolvedEntry {
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

// ---------------------------------------------------------------------------
// Stage 1: Way pass — emit COO pairs into node buckets
// ---------------------------------------------------------------------------

/// Scan all way blobs and emit `(node_id, slot_pos)` pairs into node buckets.
///
/// Returns the total number of way-node refs (= total coord slots needed).
fn stage1_way_pass(
    input: &Path,
    direct_io: bool,
    node_buckets: &mut BucketWriters,
) -> Result<u64> {
    let reader = ElementReader::open(input, direct_io)?
        .with_blob_filter(BlobFilter::only_ways());

    let mut slot_pos: u64 = 0;
    let mut pair_buf = [0u8; COO_PAIR_SIZE];

    for block in reader.into_blocks_pipelined() {
        let block = block?;
        for element in block.elements_skip_metadata() {
            if let Element::Way(w) = element {
                for node_id in w.refs() {
                    let pair = CooPair { node_id, slot_pos };
                    let bucket = pair.node_bucket();
                    pair.write_to(&mut pair_buf);
                    if let Some(writer) = node_buckets.writers[bucket].as_mut() {
                        writer.write_all(&pair_buf)?;
                    }
                    node_buckets.entry_counts[bucket] += 1;
                    slot_pos += 1;
                }
            }
        }
    }

    Ok(slot_pos)
}

// ---------------------------------------------------------------------------
// Stage 2: Node join — merge-join per bucket, emit resolved entries
// ---------------------------------------------------------------------------

/// For each node bucket: load into RAM, sort by node_id, merge-join with
/// the matching node stream, emit resolved `(slot_pos, lat, lon)` entries
/// into slot buckets.
fn stage2_node_join(
    input: &Path,
    direct_io: bool,
    node_buckets: &BucketWriters,
    slot_buckets: &mut BucketWriters,
    total_slots: u64,
) -> Result<u64> {
    let mut resolved_count: u64 = 0;
    let range_size = MAX_NODE_ID.div_ceil(NUM_BUCKETS as u64);

    for bucket_idx in 0..NUM_BUCKETS {
        if node_buckets.entry_counts[bucket_idx] == 0 {
            continue;
        }

        // Load bucket into memory and sort by node_id.
        let pairs = load_coo_bucket(&node_buckets.paths[bucket_idx])?;
        if pairs.is_empty() {
            continue;
        }

        // Build a sorted lookup: node_id → Vec<slot_pos>.
        // Using a Vec of pairs sorted by node_id for merge-join with the
        // node stream (also sorted by node_id).
        let mut sorted_pairs = pairs;
        sorted_pairs.sort_unstable_by_key(|p| p.node_id);

        // Determine the node ID range for this bucket.
        let bucket_min_id = (bucket_idx as u64 * range_size).cast_signed();
        #[allow(clippy::cast_possible_truncation)]
        let bucket_max_id = (((bucket_idx as u64 + 1) * range_size).min(MAX_NODE_ID)).cast_signed();

        // Stream nodes in this ID range, merge-join with sorted pairs.
        let reader = ElementReader::open(input, direct_io)?
            .with_blob_filter(BlobFilter::only_nodes());

        let mut pair_cursor = 0usize;
        let mut entry_buf = [0u8; RESOLVED_ENTRY_SIZE];

        for block in reader.into_blocks_pipelined() {
            let block = block?;
            for element in block.elements_skip_metadata() {
                let (id, lat, lon) = match &element {
                    Element::DenseNode(dn) => {
                        (dn.id(), dn.decimicro_lat(), dn.decimicro_lon())
                    }
                    Element::Node(n) => {
                        (n.id(), n.decimicro_lat(), n.decimicro_lon())
                    }
                    _ => continue,
                };

                // Skip nodes outside this bucket's range.
                if id < bucket_min_id {
                    continue;
                }
                if id >= bucket_max_id {
                    continue;
                }

                // Advance cursor past any pairs with smaller node_id.
                while pair_cursor < sorted_pairs.len()
                    && sorted_pairs[pair_cursor].node_id < id
                {
                    pair_cursor += 1;
                }

                // Emit resolved entries for all pairs matching this node_id.
                while pair_cursor < sorted_pairs.len()
                    && sorted_pairs[pair_cursor].node_id == id
                {
                    let entry = ResolvedEntry {
                        slot_pos: sorted_pairs[pair_cursor].slot_pos,
                        lat,
                        lon,
                    };
                    let bucket = entry.slot_bucket(total_slots);
                    entry.write_to(&mut entry_buf);
                    if let Some(writer) = slot_buckets.writers[bucket].as_mut() {
                        writer.write_all(&entry_buf)?;
                    }
                    slot_buckets.entry_counts[bucket] += 1;
                    resolved_count += 1;
                    pair_cursor += 1;
                }
            }
        }

        if bucket_idx % 16 == 0 {
            eprintln!(
                "  node join: bucket {}/{} ({} resolved so far)",
                bucket_idx + 1,
                NUM_BUCKETS,
                resolved_count
            );
        }
    }

    Ok(resolved_count)
}

/// Load a COO bucket file into memory as a Vec of pairs.
fn load_coo_bucket(path: &Path) -> Result<Vec<CooPair>> {
    let data = std::fs::read(path)
        .map_err(|e| format!("failed to read bucket {}: {e}", path.display()))?;
    let count = data.len() / COO_PAIR_SIZE;
    let mut pairs = Vec::with_capacity(count);
    let mut buf = [0u8; COO_PAIR_SIZE];
    for chunk in data.chunks_exact(COO_PAIR_SIZE) {
        buf.copy_from_slice(chunk);
        pairs.push(CooPair::read_from(&buf));
    }
    Ok(pairs)
}

// ---------------------------------------------------------------------------
// Stage 3: Slot reorder — build final coord_slots file
// ---------------------------------------------------------------------------

/// Read slot buckets in order, sort each by slot_pos, write to the final
/// coord_slots file sequentially.
fn stage3_slot_reorder(
    slot_buckets: &BucketWriters,
    coord_slots_path: &Path,
    total_slots: u64,
) -> Result<()> {
    // Pre-allocate the coord_slots file (filled with zero sentinels).
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(coord_slots_path)
        .map_err(|e| {
            format!(
                "failed to create coord_slots file {}: {e}",
                coord_slots_path.display()
            )
        })?;
    file.set_len(total_slots * COORD_SLOT_SIZE as u64)?;
    drop(file);

    // Open for random writes (we write within each bucket's slot range).
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(coord_slots_path)
        .map_err(|e| {
            format!(
                "failed to open coord_slots for writing: {e}"
            )
        })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        for bucket_idx in 0..NUM_BUCKETS {
            if slot_buckets.entry_counts[bucket_idx] == 0 {
                continue;
            }

            let entries = load_resolved_bucket(&slot_buckets.paths[bucket_idx])?;
            if entries.is_empty() {
                continue;
            }

            let mut sorted = entries;
            sorted.sort_unstable_by_key(|e| e.slot_pos);

            let mut coord_buf = [0u8; COORD_SLOT_SIZE];
            for entry in &sorted {
                let offset = entry.slot_pos * COORD_SLOT_SIZE as u64;
                coord_buf[..4].copy_from_slice(&entry.lat.to_le_bytes());
                coord_buf[4..].copy_from_slice(&entry.lon.to_le_bytes());
                file.write_at(&coord_buf, offset)?;
            }

            if bucket_idx % 16 == 0 {
                eprintln!(
                    "  slot reorder: bucket {}/{}",
                    bucket_idx + 1,
                    NUM_BUCKETS
                );
            }
        }
    }

    #[cfg(not(unix))]
    {
        return Err("external join requires unix (write_at)".into());
    }

    Ok(())
}

/// Load a resolved-entry bucket file into memory.
fn load_resolved_bucket(path: &Path) -> Result<Vec<ResolvedEntry>> {
    let data = std::fs::read(path)
        .map_err(|e| format!("failed to read bucket {}: {e}", path.display()))?;
    let count = data.len() / RESOLVED_ENTRY_SIZE;
    let mut entries = Vec::with_capacity(count);
    let mut buf = [0u8; RESOLVED_ENTRY_SIZE];
    for chunk in data.chunks_exact(RESOLVED_ENTRY_SIZE) {
        buf.copy_from_slice(chunk);
        entries.push(ResolvedEntry::read_from(&buf));
    }
    Ok(entries)
}

// ---------------------------------------------------------------------------
// Stage 4: Assembly — emit enriched PBF
// ---------------------------------------------------------------------------

/// Read the coord_slots file and provide sequential coordinate lookup.
struct CoordSlots {
    #[cfg(unix)]
    file: std::fs::File,
    total_slots: u64,
}

impl CoordSlots {
    fn open(path: &Path, total_slots: u64) -> Result<Self> {
        let file = std::fs::File::open(path)
            .map_err(|e| format!("failed to open coord_slots: {e}"))?;
        Ok(Self {
            #[cfg(unix)]
            file,
            total_slots,
        })
    }

    /// Read a coordinate at the given slot position.
    #[cfg(unix)]
    fn get(&self, slot_pos: u64) -> Option<(i32, i32)> {
        use std::os::unix::fs::FileExt;
        if slot_pos >= self.total_slots {
            return None;
        }
        let offset = slot_pos * COORD_SLOT_SIZE as u64;
        let mut buf = [0u8; COORD_SLOT_SIZE];
        self.file.read_at(&mut buf, offset).ok()?;
        let lat = i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let lon = i32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        if lat == 0 && lon == 0 {
            return None; // sentinel
        }
        Some((lat, lon))
    }
}

/// Assembly pass: re-read the PBF, attach coordinates from coord_slots to ways.
#[allow(clippy::too_many_arguments)]
fn stage4_assembly(
    input: &Path,
    output: &Path,
    coord_slots: &CoordSlots,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSetDense>,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<Stats> {
    let mut stats = Stats {
        nodes_read: 0,
        nodes_written: 0,
        nodes_dropped: 0,
        ways_written: 0,
        relations_written: 0,
        missing_locations: 0,
        blobs_passthrough: 0,
        blobs_decoded: 0,
    };

    let reader = ElementReader::open(input, direct_io)?;
    let mut writer = writer_from_header(
        output,
        compression,
        reader.header(),
        true,
        overrides,
        |hb| hb.optional_feature("LocationsOnWays"),
    )?;

    let mut slot_pos: u64 = 0;
    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);

    for block in reader.into_blocks_pipelined() {
        batch.push(block?);
        if batch.len() >= BATCH_SIZE {
            let batch_stats = assemble_batch(
                &batch,
                &mut writer,
                coord_slots,
                &mut slot_pos,
                keep_untagged_nodes,
                relation_member_node_ids,
            )?;
            merge_stats(&mut stats, &batch_stats);
            batch.clear();
        }
    }

    if !batch.is_empty() {
        let batch_stats = assemble_batch(
            &batch,
            &mut writer,
            coord_slots,
            &mut slot_pos,
            keep_untagged_nodes,
            relation_member_node_ids,
        )?;
        merge_stats(&mut stats, &batch_stats);
    }

    writer.flush()?;
    Ok(stats)
}

fn merge_stats(dst: &mut Stats, src: &Stats) {
    dst.nodes_read += src.nodes_read;
    dst.nodes_written += src.nodes_written;
    dst.nodes_dropped += src.nodes_dropped;
    dst.ways_written += src.ways_written;
    dst.relations_written += src.relations_written;
    dst.missing_locations += src.missing_locations;
    dst.blobs_passthrough += src.blobs_passthrough;
    dst.blobs_decoded += src.blobs_decoded;
}

/// Process one batch of blocks for assembly. Ways get coordinates from
/// coord_slots; nodes are filtered; relations pass through.
fn assemble_batch(
    batch: &[PrimitiveBlock],
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    coord_slots: &CoordSlots,
    slot_pos: &mut u64,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSetDense>,
) -> Result<Stats> {
    // Snapshot the current slot_pos and compute per-block starting positions.
    // Each way's refs advance slot_pos sequentially.
    let mut block_slot_starts: Vec<u64> = Vec::with_capacity(batch.len());
    let mut scan_pos = *slot_pos;
    for block in batch {
        block_slot_starts.push(scan_pos);
        for element in block.elements_skip_metadata() {
            if let Element::Way(w) = element {
                scan_pos += w.refs().count() as u64;
            }
        }
    }
    *slot_pos = scan_pos;

    type BatchResult = std::result::Result<(Vec<OwnedBlock>, Stats), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .zip(block_slot_starts.par_iter())
        .map_init(
            || {
                (
                    BlockBuilder::new(),
                    Vec::<i64>::new(),
                    Vec::<(i32, i32)>::new(),
                )
            },
            |(bb, refs_buf, locations_buf), (block, &block_start)| {
                let mut output: Vec<OwnedBlock> = Vec::new();
                let block_stats = assemble_block(
                    block,
                    bb,
                    &mut output,
                    coord_slots,
                    block_start,
                    keep_untagged_nodes,
                    relation_member_node_ids,
                    refs_buf,
                    locations_buf,
                )?;
                flush_local(bb, &mut output)?;
                Ok((output, block_stats))
            },
        )
        .collect();

    let mut total = Stats {
        nodes_read: 0,
        nodes_written: 0,
        nodes_dropped: 0,
        ways_written: 0,
        relations_written: 0,
        missing_locations: 0,
        blobs_passthrough: 0,
        blobs_decoded: 0,
    };

    drain_batch_results(results, writer, |s| merge_stats(&mut total, &s))?;
    Ok(total)
}

/// Process a single block for assembly.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn assemble_block(
    block: &PrimitiveBlock,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    coord_slots: &CoordSlots,
    mut way_slot_pos: u64,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSetDense>,
    refs_buf: &mut Vec<i64>,
    locations_buf: &mut Vec<(i32, i32)>,
) -> std::result::Result<Stats, String> {
    let mut stats = Stats {
        nodes_read: 0,
        nodes_written: 0,
        nodes_dropped: 0,
        ways_written: 0,
        relations_written: 0,
        missing_locations: 0,
        blobs_passthrough: 0,
        blobs_decoded: 0,
    };

    let mut tags_buf: Vec<(&str, &str)> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                stats.nodes_read += 1;
                let has_tags = dn.tags().next().is_some();
                if keep_untagged_nodes
                    || has_tags
                    || relation_member_node_ids.is_some_and(|ids| ids.get(dn.id()))
                {
                    ensure_node_capacity_local(bb, output)?;
                    tags_buf.clear();
                    tags_buf.extend(dn.tags());
                    let meta = dense_node_metadata(dn);
                    bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), &tags_buf, meta.as_ref());
                    stats.nodes_written += 1;
                } else {
                    stats.nodes_dropped += 1;
                }
            }
            Element::Node(n) => {
                stats.nodes_read += 1;
                let has_tags = n.tags().next().is_some();
                if keep_untagged_nodes
                    || has_tags
                    || relation_member_node_ids.is_some_and(|ids| ids.get(n.id()))
                {
                    ensure_node_capacity_local(bb, output)?;
                    tags_buf.clear();
                    tags_buf.extend(n.tags());
                    let meta = element_metadata(&n.info());
                    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), &tags_buf, meta.as_ref());
                    stats.nodes_written += 1;
                } else {
                    stats.nodes_dropped += 1;
                }
            }
            Element::Way(w) => {
                ensure_way_capacity_local(bb, output)?;
                tags_buf.clear();
                tags_buf.extend(w.tags());
                refs_buf.clear();
                refs_buf.extend(w.refs());
                locations_buf.clear();
                for _node_id in refs_buf.iter() {
                    match coord_slots.get(way_slot_pos) {
                        Some(loc) => locations_buf.push(loc),
                        None => {
                            stats.missing_locations += 1;
                            locations_buf.push((0, 0));
                        }
                    }
                    way_slot_pos += 1;
                }
                let meta = element_metadata(&w.info());
                bb.add_way_with_locations(w.id(), &tags_buf, refs_buf, locations_buf, meta.as_ref());
                stats.ways_written += 1;
            }
            Element::Relation(r) => {
                ensure_relation_capacity_local(bb, output)?;
                tags_buf.clear();
                tags_buf.extend(r.tags());
                members_buf.clear();
                members_buf.extend(r.members().map(|m| MemberData {
                    id: m.id,
                    role: m.role().unwrap_or(""),
                }));
                let meta = element_metadata(&r.info());
                bb.add_relation(r.id(), &tags_buf, &members_buf, meta.as_ref());
                stats.relations_written += 1;
            }
        }
    }

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the full external join pipeline for add-locations-to-ways.
///
/// Bounded memory (<1 GB), all sequential I/O. Uses ~224 GB temp disk at
/// planet scale. See module docs for the algorithm.
#[allow(clippy::too_many_arguments)]
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

    let scratch_dir = ScratchDir::new(output.parent().unwrap_or(Path::new(".")))?;

    // Collect relation member node IDs (for node filtering).
    let relation_member_node_ids = if keep_untagged_nodes {
        None
    } else {
        Some(super::add_locations_to_ways::collect_relation_member_node_ids(
            input, direct_io,
        )?)
    };

    // --- Stage 1: Way pass ---
    eprintln!("external join: stage 1 — scanning ways, emitting COO pairs into node buckets...");
    let mut node_buckets = BucketWriters::create(&scratch_dir, "node")?;
    let total_slots = stage1_way_pass(input, direct_io, &mut node_buckets)?;
    let node_counts = node_buckets.finish()?;
    let total_coo: u64 = node_counts.iter().sum();
    eprintln!("  {total_slots} way-node refs → {total_coo} COO pairs in {NUM_BUCKETS} buckets");

    // --- Stage 2: Node join ---
    eprintln!("external join: stage 2 — node join (merge-join per bucket)...");
    let mut slot_buckets = BucketWriters::create(&scratch_dir, "slot")?;
    let resolved_count =
        stage2_node_join(input, direct_io, &node_buckets, &mut slot_buckets, total_slots)?;
    slot_buckets.finish()?;
    node_buckets.cleanup();
    eprintln!("  {resolved_count} coordinates resolved");

    // --- Stage 3: Slot reorder ---
    eprintln!("external join: stage 3 — slot reorder, building coord_slots file...");
    let coord_slots_path = scratch_dir.file_path("coord_slots");
    stage3_slot_reorder(&slot_buckets, &coord_slots_path, total_slots)?;
    slot_buckets.cleanup();
    eprintln!(
        "  coord_slots: {} slots, {} bytes",
        total_slots,
        total_slots * COORD_SLOT_SIZE as u64
    );

    // --- Stage 4: Assembly ---
    eprintln!("external join: stage 4 — assembling enriched PBF...");
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
    )?;

    // scratch_dir dropped here → cleanup all temp files.
    Ok(stats)
}
