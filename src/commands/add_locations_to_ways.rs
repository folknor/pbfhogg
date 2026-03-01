//! Embed node coordinates in ways. Equivalent to `osmium add-locations-to-ways`.

use std::io::Read;
use std::path::Path;

use bytes::Bytes;
use rayon::prelude::*;
use rustc_hash::FxHashMap;

use crate::blob::{
    decode_blob_to_headerblock, decompress_blob_data_into,
    parse_blob_header_with_index, parse_primitive_block_from_bytes_owned, BlobKind,
};
use crate::blob_index::{BlobIndex, ElemKind};
use crate::block_builder::{BlockBuilder, HeaderBuilder, MemberData, OwnedBlock};
use crate::file_reader::FileReader;
use crate::writer::{Compression, PbfWriter};
use crate::{BlobFilter, Element, ElementReader, PrimitiveBlock};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// Index type
// ---------------------------------------------------------------------------

/// Node location index type for add-locations-to-ways.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexType {
    /// FxHashMap-based index. ~24 bytes/node, good for country-scale.
    Hash,
    /// Dense mmap-based index. 8 bytes/slot with direct indexing by node ID.
    /// Uses anonymous mmap with lazy page allocation — virtual memory is
    /// allocated for the full ID range but physical memory is only committed
    /// for pages actually written. Required for planet-scale (8.5B nodes →
    /// ~68 GB physical vs FxHashMap's ~192 GB).
    ///
    /// The `capacity` field is the max number of entries (node IDs). For planet,
    /// use [`DENSE_INDEX_DEFAULT_CAPACITY`] (16 billion). Smaller values work
    /// for testing or country-scale files.
    Dense { capacity: usize },
}

/// Default dense index capacity: 16 billion entries (128 GB virtual).
/// Covers current OSM max node ID (~12.5B) with headroom for growth.
///
/// Requires `vm.overcommit_memory=1` or sufficient physical RAM + swap on
/// the host. On systems with heuristic overcommit (the default), this
/// allocation may be rejected. Use a smaller capacity or switch to
/// `--index-type hash` in that case.
pub const DENSE_INDEX_DEFAULT_CAPACITY: usize = 16_000_000_000;

// ---------------------------------------------------------------------------
// Node location index
// ---------------------------------------------------------------------------

/// Node location index abstraction supporting multiple backends.
/// Node location index abstraction supporting multiple backends.
///
/// The `Hash` variant uses `FxHashMap` (from `rustc-hash`) instead of the
/// standard `HashMap`. For integer keys like node IDs, FxHash (foldhash) is
/// 2-4x faster than SipHash because it uses a simple multiply-shift instead
/// of the cryptographic SipHash-2-4 rounds. Hash-DoS resistance is irrelevant
/// here since the keys come from PBF file data we control, not user input.
pub enum NodeLocationIndex {
    Hash(FxHashMap<i64, (i32, i32)>),
    Dense(DenseMmapIndex),
}

impl NodeLocationIndex {
    fn insert(&mut self, node_id: i64, lat: i32, lon: i32) {
        match self {
            Self::Hash(map) => {
                map.insert(node_id, (lat, lon));
            }
            Self::Dense(dense) => dense.insert(node_id, lat, lon),
        }
    }

    fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        match self {
            Self::Hash(map) => map.get(&node_id).copied(),
            Self::Dense(dense) => dense.get(node_id),
        }
    }
}

// ---------------------------------------------------------------------------
// Dense mmap index
// ---------------------------------------------------------------------------

/// Dense mmap-backed node location index.
///
/// Uses anonymous mmap with direct indexing: `mmap[node_id * 8 .. node_id * 8 + 8]`
/// stores `(lat: i32, lon: i32)` packed as 8 bytes (little-endian).
///
/// Zero-initialized by the OS. Pages are lazily allocated (demand-paged): a
/// 128 GB virtual mapping only consumes physical memory for pages actually
/// written. For planet (~8.5B nodes, max ID ~12.5B), physical RSS is ~68 GB.
///
/// Sentinel: `(0, 0)` means unset. ~116 nodes at exactly null island (0°N, 0°E)
/// will appear as missing — acceptable ambiguity for diagnostic counters.
pub struct DenseMmapIndex {
    mmap: memmap2::MmapMut,
    capacity: usize,
}

/// 4 bytes lat + 4 bytes lon = 8 bytes per entry.
const ENTRY_SIZE: usize = 8;

// Require 64-bit platform for dense index (32-bit cannot address 128 GB).
const _: () = assert!(std::mem::size_of::<usize>() >= 8);

impl DenseMmapIndex {
    fn new(capacity: usize) -> Result<Self> {
        let byte_len = capacity
            .checked_mul(ENTRY_SIZE)
            .ok_or("dense index capacity overflow")?;
        // String error is intentional — includes the allocation size and actionable
        // recovery advice that the underlying io::Error wouldn't provide.
        let mmap = memmap2::MmapMut::map_anon(byte_len).map_err(|e| {
            format!(
                "failed to create dense mmap index ({} GB virtual): {e}. \
                 Try --index-type hash or increase vm.overcommit_ratio.",
                byte_len / 1_000_000_000
            )
        })?;
        Ok(Self { mmap, capacity })
    }

    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    fn insert(&mut self, node_id: i64, lat: i32, lon: i32) {
        if node_id < 0 {
            return;
        }
        let idx = node_id as usize;
        if idx >= self.capacity {
            return;
        }
        let offset = idx * ENTRY_SIZE;
        self.mmap[offset..offset + 4].copy_from_slice(&lat.to_le_bytes());
        self.mmap[offset + 4..offset + 8].copy_from_slice(&lon.to_le_bytes());
    }

    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        if node_id < 0 {
            return None;
        }
        let idx = node_id as usize;
        if idx >= self.capacity {
            return None;
        }
        let offset = idx * ENTRY_SIZE;
        let lat_bytes: [u8; 4] = self.mmap[offset..offset + 4]
            .try_into()
            .ok()?;
        let lon_bytes: [u8; 4] = self.mmap[offset + 4..offset + 8]
            .try_into()
            .ok()?;
        let lat = i32::from_le_bytes(lat_bytes);
        let lon = i32::from_le_bytes(lon_bytes);
        if lat == 0 && lon == 0 {
            return None;
        }
        Some((lat, lon))
    }
}

// ---------------------------------------------------------------------------
// Parallel dense index writer
// ---------------------------------------------------------------------------

/// Thread-safe writer for parallel dense index population.
///
/// Holds a raw pointer into the `DenseMmapIndex` mmap buffer. Safe to use
/// from multiple rayon tasks because PBF node IDs are unique: each ID maps
/// to a disjoint 8-byte slot (`base + node_id * 8`), so no two tasks write
/// the same memory.
///
/// The caller must ensure the `DenseMmapIndex` outlives all uses of this
/// writer. In practice, both live in `build_node_index_dense` and `par_iter`
/// is synchronous (blocks until complete), so the pointer cannot escape.
struct SharedDenseWriter {
    base: *mut u8,
    capacity: usize,
}

// SAFETY: Writes target disjoint 8-byte slots keyed by unique PBF node IDs.
// No two rayon tasks access the same offset.
unsafe impl Send for SharedDenseWriter {}
unsafe impl Sync for SharedDenseWriter {}

impl SharedDenseWriter {
    /// Insert a node's coordinates. Silently ignores negative IDs and IDs
    /// beyond capacity (same semantics as `DenseMmapIndex::insert`).
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    fn insert(&self, node_id: i64, lat: i32, lon: i32) {
        if node_id < 0 {
            return;
        }
        let idx = node_id as usize;
        if idx >= self.capacity {
            return;
        }
        let offset = idx * ENTRY_SIZE;
        // SAFETY: offset + 8 <= capacity * ENTRY_SIZE = mmap length.
        // Each node ID is unique in a PBF, so no two tasks write the same slot.
        unsafe {
            let dst = self.base.add(offset);
            std::ptr::copy_nonoverlapping(lat.to_le_bytes().as_ptr(), dst, 4);
            std::ptr::copy_nonoverlapping(lon.to_le_bytes().as_ptr(), dst.add(4), 4);
        }
    }
}

// ---------------------------------------------------------------------------
// Raw blob frame reading (for passthrough path)
// ---------------------------------------------------------------------------

/// A raw blob frame for passthrough or selective decode.
struct RawBlobFrame {
    /// Complete framed bytes: `[4-byte header_len][BlobHeader][Blob]`.
    frame_bytes: Vec<u8>,
    blob_type: BlobKind,
    /// Byte offset within `frame_bytes` where the Blob protobuf starts.
    blob_offset: usize,
    /// Blob-level index from BlobHeader indexdata, if present.
    index: Option<BlobIndex>,
    /// Per-blob tag key data from BlobHeader field 4, if present.
    #[allow(dead_code)]
    tagdata: Option<Box<[u8]>>,
}

impl RawBlobFrame {
    fn blob_bytes(&self) -> &[u8] {
        &self.frame_bytes[self.blob_offset..]
    }
}

/// Read the next raw blob frame. Returns `None` at EOF.
fn read_raw_frame<R: Read>(reader: &mut R) -> Result<Option<RawBlobFrame>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    #[allow(clippy::cast_possible_truncation)]
    let header_len = u32::from_be_bytes(len_buf) as usize;

    let mut header_bytes = vec![0u8; header_len];
    reader.read_exact(&mut header_bytes)?;
    let (blob_type, data_size, indexdata, tagdata) =
        parse_blob_header_with_index(&header_bytes)?;

    let blob_offset = 4 + header_len;
    let frame_len = blob_offset + data_size;
    let mut frame_bytes = Vec::with_capacity(frame_len);
    frame_bytes.extend_from_slice(&len_buf);
    frame_bytes.extend_from_slice(&header_bytes);
    frame_bytes.resize(frame_len, 0);
    reader.read_exact(&mut frame_bytes[blob_offset..])?;

    let index = indexdata.and_then(|d| BlobIndex::deserialize(&d));

    Ok(Some(RawBlobFrame {
        frame_bytes,
        blob_type,
        blob_offset,
        index,
        tagdata,
    }))
}

/// Check if the first OsmData blob in a PBF has indexdata.
fn has_indexdata(path: &Path, direct_io: bool) -> Result<bool> {
    let mut reader = FileReader::open(path, direct_io)?;
    while let Some(frame) = read_raw_frame(&mut reader)? {
        match frame.blob_type {
            BlobKind::OsmHeader => continue,
            BlobKind::OsmData => return Ok(frame.index.is_some()),
            BlobKind::Unknown(_) => continue,
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Statistics from the add-locations-to-ways operation.
pub struct Stats {
    pub nodes_read: u64,
    pub nodes_written: u64,
    pub nodes_dropped: u64,
    pub ways_written: u64,
    pub relations_written: u64,
    pub missing_locations: u64,
    pub blobs_passthrough: u64,
    pub blobs_decoded: u64,
}

impl Stats {
    /// Print a summary of the operation to stderr.
    pub fn print_summary(&self) {
        eprintln!(
            "add-locations-to-ways: {} nodes read, {} written, {} dropped, \
             {} ways, {} relations, {} missing locations",
            self.nodes_read,
            self.nodes_written,
            self.nodes_dropped,
            self.ways_written,
            self.relations_written,
            self.missing_locations,
        );
        if self.blobs_passthrough > 0 {
            eprintln!(
                "  Blobs: {} passthrough, {} decoded",
                self.blobs_passthrough, self.blobs_decoded,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Embed node coordinates into ways.
///
/// Two-pass algorithm:
/// 1. Read all nodes and build a coordinate index.
/// 2. Re-read the input and write to output, attaching coordinates to ways.
///
/// If `keep_untagged_nodes` is false, nodes with zero tags are omitted from
/// the output (their coordinates are still used for ways).
#[hotpath::measure]
pub fn add_locations_to_ways(
    input: &Path,
    output: &Path,
    keep_untagged_nodes: bool,
    compression: Compression,
    direct_io: bool,
    index_type: IndexType,
) -> Result<Stats> {
    let index = build_node_index(input, direct_io, index_type)?;
    write_output(input, output, &index, keep_untagged_nodes, compression, direct_io)
}

// ---------------------------------------------------------------------------
// Pass 1: Build node coordinate index
// ---------------------------------------------------------------------------

/// Number of decoded `PrimitiveBlock`s collected before dispatching to rayon
/// for parallel node index population.
const INDEX_BATCH_SIZE: usize = 64;

fn build_node_index(input: &Path, direct_io: bool, index_type: IndexType) -> Result<NodeLocationIndex> {
    match index_type {
        IndexType::Hash => build_node_index_hash(input, direct_io),
        IndexType::Dense { capacity } => build_node_index_dense(input, direct_io, capacity),
    }
}

/// Build the dense mmap index in parallel. Each rayon task writes directly
/// to disjoint mmap slots via `SharedDenseWriter`.
fn build_node_index_dense(
    input: &Path,
    direct_io: bool,
    capacity: usize,
) -> Result<NodeLocationIndex> {
    let mut index = DenseMmapIndex::new(capacity)?;
    let writer = SharedDenseWriter {
        base: index.mmap.as_mut_ptr(),
        capacity: index.capacity,
    };

    let reader = ElementReader::open(input, direct_io)?
        .with_blob_filter(BlobFilter::only_nodes());

    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(INDEX_BATCH_SIZE);
    for block in reader.into_blocks_pipelined() {
        batch.push(block?);
        if batch.len() >= INDEX_BATCH_SIZE {
            index_batch_dense(&batch, &writer);
            batch.clear();
        }
    }
    if !batch.is_empty() {
        index_batch_dense(&batch, &writer);
    }

    drop(writer);
    Ok(NodeLocationIndex::Dense(index))
}

/// Parallel insert for one batch of blocks into the dense mmap index.
fn index_batch_dense(batch: &[PrimitiveBlock], writer: &SharedDenseWriter) {
    batch.par_iter().for_each(|block| {
        for element in block.elements_skip_metadata() {
            match &element {
                Element::DenseNode(dn) => {
                    writer.insert(dn.id(), dn.decimicro_lat(), dn.decimicro_lon());
                }
                Element::Node(n) => {
                    writer.insert(n.id(), n.decimicro_lat(), n.decimicro_lon());
                }
                Element::Way(_) | Element::Relation(_) => {}
            }
        }
    });
}

/// Build the hash map index in parallel. Each rayon task builds a thread-local
/// partial map, then they are reduced pairwise and merged into the master map.
fn build_node_index_hash(input: &Path, direct_io: bool) -> Result<NodeLocationIndex> {
    let mut master: FxHashMap<i64, (i32, i32)> = FxHashMap::default();

    let reader = ElementReader::open(input, direct_io)?
        .with_blob_filter(BlobFilter::only_nodes());

    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(INDEX_BATCH_SIZE);
    for block in reader.into_blocks_pipelined() {
        batch.push(block?);
        if batch.len() >= INDEX_BATCH_SIZE {
            master.extend(index_batch_hash(&batch));
            batch.clear();
        }
    }
    if !batch.is_empty() {
        master.extend(index_batch_hash(&batch));
    }

    Ok(NodeLocationIndex::Hash(master))
}

/// Parallel fold+reduce for one batch of blocks into a merged `FxHashMap`.
fn index_batch_hash(batch: &[PrimitiveBlock]) -> FxHashMap<i64, (i32, i32)> {
    batch
        .par_iter()
        .fold(
            FxHashMap::default,
            |mut local: FxHashMap<i64, (i32, i32)>, block| {
                for element in block.elements_skip_metadata() {
                    match &element {
                        Element::DenseNode(dn) => {
                            local.insert(dn.id(), (dn.decimicro_lat(), dn.decimicro_lon()));
                        }
                        Element::Node(n) => {
                            local.insert(n.id(), (n.decimicro_lat(), n.decimicro_lon()));
                        }
                        Element::Way(_) | Element::Relation(_) => {}
                    }
                }
                local
            },
        )
        .reduce(FxHashMap::default, |mut a, b| {
            a.extend(b);
            a
        })
}

// ---------------------------------------------------------------------------
// Pass 2: Write output with locations on ways
// ---------------------------------------------------------------------------

/// Number of decoded `PrimitiveBlock`s collected before dispatching to rayon.
const BATCH_SIZE: usize = 64;

fn write_output(
    input: &Path,
    output: &Path,
    index: &NodeLocationIndex,
    keep_untagged_nodes: bool,
    compression: Compression,
    direct_io: bool,
) -> Result<Stats> {
    if has_indexdata(input, direct_io)? {
        write_output_passthrough(input, output, index, keep_untagged_nodes, compression, direct_io)
    } else {
        write_output_decode_all(input, output, index, keep_untagged_nodes, compression, direct_io)
    }
}

// ---------------------------------------------------------------------------
// Pass 2a: Decode-all fallback (no indexdata)
// ---------------------------------------------------------------------------

fn write_output_decode_all(
    input: &Path,
    output: &Path,
    index: &NodeLocationIndex,
    keep_untagged_nodes: bool,
    compression: Compression,
    direct_io: bool,
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
    let mut hb = HeaderBuilder::from_header(reader.header()).optional_feature("LocationsOnWays");
    if reader.header().is_sorted() {
        hb = hb.sorted();
    }
    let header_bytes = hb.build()?;
    let mut writer = PbfWriter::to_path_pipelined(output, compression, &header_bytes)?;

    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);

    for block in reader.into_blocks_pipelined() {
        batch.push(block?);

        if batch.len() >= BATCH_SIZE {
            let batch_stats = process_batch(
                &batch, &mut writer, index, keep_untagged_nodes,
            )?;
            merge_stats(&mut stats, &batch_stats);
            batch.clear();
        }
    }

    if !batch.is_empty() {
        let batch_stats = process_batch(
            &batch, &mut writer, index, keep_untagged_nodes,
        )?;
        merge_stats(&mut stats, &batch_stats);
    }

    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Parallel batch processing
// ---------------------------------------------------------------------------

use super::{dense_node_metadata, element_metadata};

/// Flush the current block from a [`BlockBuilder`] into a local output buffer.
fn flush_local(bb: &mut BlockBuilder, output: &mut Vec<OwnedBlock>) -> std::result::Result<(), Box<dyn std::error::Error>> {
    if let Some(triple) = bb.take_owned()? {
        output.push(triple);
    }
    Ok(())
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

/// Process a single `PrimitiveBlock`, writing elements into the thread-local
/// `BlockBuilder` and flushing complete blocks into `output`.
fn process_block(
    block: &PrimitiveBlock,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    index: &NodeLocationIndex,
    keep_untagged_nodes: bool,
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
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();
    let mut locations_buf: Vec<(i32, i32)> = Vec::new();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                stats.nodes_read += 1;
                let has_tags = dn.tags().next().is_some();
                if keep_untagged_nodes || has_tags {
                    if !bb.can_add_node() {
                        flush_local(bb, output).map_err(|e| e.to_string())?;
                    }
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
                if keep_untagged_nodes || has_tags {
                    if !bb.can_add_node() {
                        flush_local(bb, output).map_err(|e| e.to_string())?;
                    }
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
                if !bb.can_add_way() {
                    flush_local(bb, output).map_err(|e| e.to_string())?;
                }
                tags_buf.clear();
                tags_buf.extend(w.tags());
                refs_buf.clear();
                refs_buf.extend(w.refs());
                locations_buf.clear();
                for node_id in &refs_buf {
                    match index.get(*node_id) {
                        Some(loc) => locations_buf.push(loc),
                        None => {
                            stats.missing_locations += 1;
                            locations_buf.push((0, 0));
                        }
                    }
                }
                let meta = element_metadata(&w.info());
                bb.add_way_with_locations(w.id(), &tags_buf, &refs_buf, &locations_buf, meta.as_ref());
                stats.ways_written += 1;
            }
            Element::Relation(r) => {
                if !bb.can_add_relation() {
                    flush_local(bb, output).map_err(|e| e.to_string())?;
                }
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

/// Process a batch of `PrimitiveBlock`s in parallel via rayon.
fn process_batch(
    batch: &[PrimitiveBlock],
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    index: &NodeLocationIndex,
    keep_untagged_nodes: bool,
) -> Result<Stats> {
    type BatchResult = std::result::Result<(Vec<OwnedBlock>, Stats), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            BlockBuilder::new,
            |bb, block| {
                let mut output: Vec<OwnedBlock> = Vec::new();
                let block_stats = process_block(
                    block, bb, &mut output, index, keep_untagged_nodes,
                )?;
                flush_local(bb, &mut output).map_err(|e| e.to_string())?;
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

    for result in results {
        let (blocks, block_stats) = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        merge_stats(&mut total, &block_stats);
        for (block_bytes, blob_index, tagdata) in blocks {
            writer.write_primitive_block_owned(block_bytes, blob_index, tagdata.as_deref())?;
        }
    }

    Ok(total)
}

// ---------------------------------------------------------------------------
// Pass 2b: Passthrough path (indexdata present)
// ---------------------------------------------------------------------------

/// Read raw header blob, build output header with `LocationsOnWays`.
fn read_header_raw<R: Read>(reader: &mut R) -> Result<(Vec<u8>, bool)> {
    while let Some(frame) = read_raw_frame(reader)? {
        if frame.blob_type == BlobKind::OsmHeader {
            let header = decode_blob_to_headerblock(frame.blob_bytes())?;
            let mut hb = HeaderBuilder::from_header(&header)
                .optional_feature("LocationsOnWays");
            let sorted = header.is_sorted();
            if sorted {
                hb = hb.sorted();
            }
            return Ok((hb.build()?, sorted));
        }
    }
    Err("no OSMHeader blob found".into())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn write_output_passthrough(
    input: &Path,
    output: &Path,
    node_index: &NodeLocationIndex,
    keep_untagged_nodes: bool,
    compression: Compression,
    direct_io: bool,
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

    let mut reader = FileReader::open(input, direct_io)?;
    let (header_bytes, _sorted) = read_header_raw(&mut reader)?;
    let mut writer = PbfWriter::to_path_pipelined(output, compression, &header_bytes)?;

    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut way_batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);
    let mut node_batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);

    while let Some(mut frame) = read_raw_frame(&mut reader)? {
        if frame.blob_type != BlobKind::OsmData {
            continue;
        }

        let kind = frame.index.as_ref().map(|idx| idx.kind);
        match kind {
            Some(ElemKind::Node) if keep_untagged_nodes => {
                if let Some(ref idx) = frame.index {
                    stats.nodes_read += idx.count;
                    stats.nodes_written += idx.count;
                }
                stats.blobs_passthrough += 1;
                writer.write_raw_owned(std::mem::take(&mut frame.frame_bytes))?;
            }
            Some(ElemKind::Node) => {
                // keep_untagged_nodes=false: decode + filter
                decompress_blob_data_into(frame.blob_bytes(), &mut decompress_buf)?;
                let raw = std::mem::take(&mut decompress_buf);
                let block = parse_primitive_block_from_bytes_owned(&Bytes::from(raw))?;
                stats.blobs_decoded += 1;
                node_batch.push(block);
                if node_batch.len() >= BATCH_SIZE {
                    let batch_stats = process_node_batch(&node_batch, &mut writer)?;
                    merge_stats(&mut stats, &batch_stats);
                    node_batch.clear();
                }
            }
            Some(ElemKind::Relation) => {
                if let Some(ref idx) = frame.index {
                    stats.relations_written += idx.count;
                }
                stats.blobs_passthrough += 1;
                writer.write_raw_owned(std::mem::take(&mut frame.frame_bytes))?;
            }
            Some(ElemKind::Way) => {
                decompress_blob_data_into(frame.blob_bytes(), &mut decompress_buf)?;
                let raw = std::mem::take(&mut decompress_buf);
                let block = parse_primitive_block_from_bytes_owned(&Bytes::from(raw))?;
                stats.blobs_decoded += 1;
                way_batch.push(block);
                if way_batch.len() >= BATCH_SIZE {
                    let batch_stats = process_way_batch(&way_batch, &mut writer, node_index)?;
                    merge_stats(&mut stats, &batch_stats);
                    way_batch.clear();
                }
            }
            None => {
                // No indexdata on this blob — fall back to full decode.
                decompress_blob_data_into(frame.blob_bytes(), &mut decompress_buf)?;
                let raw = std::mem::take(&mut decompress_buf);
                let block = parse_primitive_block_from_bytes_owned(&Bytes::from(raw))?;
                stats.blobs_decoded += 1;
                way_batch.push(block);
                if way_batch.len() >= BATCH_SIZE {
                    let batch_stats = process_way_batch(&way_batch, &mut writer, node_index)?;
                    merge_stats(&mut stats, &batch_stats);
                    way_batch.clear();
                }
            }
        }
    }

    // Flush remaining batches.
    if !node_batch.is_empty() {
        let batch_stats = process_node_batch(&node_batch, &mut writer)?;
        merge_stats(&mut stats, &batch_stats);
    }
    if !way_batch.is_empty() {
        let batch_stats = process_way_batch(&way_batch, &mut writer, node_index)?;
        merge_stats(&mut stats, &batch_stats);
    }

    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Way-only batch processing (passthrough path)
// ---------------------------------------------------------------------------

fn process_way_block(
    block: &PrimitiveBlock,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    index: &NodeLocationIndex,
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
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut locations_buf: Vec<(i32, i32)> = Vec::new();

    for element in block.elements() {
        if let Element::Way(w) = &element {
            if !bb.can_add_way() {
                flush_local(bb, output).map_err(|e| e.to_string())?;
            }
            tags_buf.clear();
            tags_buf.extend(w.tags());
            refs_buf.clear();
            refs_buf.extend(w.refs());
            locations_buf.clear();
            for node_id in &refs_buf {
                match index.get(*node_id) {
                    Some(loc) => locations_buf.push(loc),
                    None => {
                        stats.missing_locations += 1;
                        locations_buf.push((0, 0));
                    }
                }
            }
            let meta = element_metadata(&w.info());
            bb.add_way_with_locations(w.id(), &tags_buf, &refs_buf, &locations_buf, meta.as_ref());
            stats.ways_written += 1;
        }
    }

    Ok(stats)
}

fn process_way_batch(
    batch: &[PrimitiveBlock],
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    index: &NodeLocationIndex,
) -> Result<Stats> {
    type BatchResult = std::result::Result<(Vec<OwnedBlock>, Stats), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            BlockBuilder::new,
            |bb, block| {
                let mut output: Vec<OwnedBlock> = Vec::new();
                let block_stats = process_way_block(block, bb, &mut output, index)?;
                flush_local(bb, &mut output).map_err(|e| e.to_string())?;
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

    for result in results {
        let (blocks, block_stats) = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        merge_stats(&mut total, &block_stats);
        for (block_bytes, blob_index, tagdata) in blocks {
            writer.write_primitive_block_owned(block_bytes, blob_index, tagdata.as_deref())?;
        }
    }

    Ok(total)
}

// ---------------------------------------------------------------------------
// Node-only batch processing (passthrough path, keep_untagged_nodes=false)
// ---------------------------------------------------------------------------

fn process_node_block(
    block: &PrimitiveBlock,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
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

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                stats.nodes_read += 1;
                let has_tags = dn.tags().next().is_some();
                if has_tags {
                    if !bb.can_add_node() {
                        flush_local(bb, output).map_err(|e| e.to_string())?;
                    }
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
                if has_tags {
                    if !bb.can_add_node() {
                        flush_local(bb, output).map_err(|e| e.to_string())?;
                    }
                    tags_buf.clear();
                    tags_buf.extend(n.tags());
                    let meta = element_metadata(&n.info());
                    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), &tags_buf, meta.as_ref());
                    stats.nodes_written += 1;
                } else {
                    stats.nodes_dropped += 1;
                }
            }
            _ => {}
        }
    }

    Ok(stats)
}

fn process_node_batch(
    batch: &[PrimitiveBlock],
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
) -> Result<Stats> {
    type BatchResult = std::result::Result<(Vec<OwnedBlock>, Stats), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            BlockBuilder::new,
            |bb, block| {
                let mut output: Vec<OwnedBlock> = Vec::new();
                let block_stats = process_node_block(block, bb, &mut output)?;
                flush_local(bb, &mut output).map_err(|e| e.to_string())?;
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

    for result in results {
        let (blocks, block_stats) = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        merge_stats(&mut total, &block_stats);
        for (block_bytes, blob_index, tagdata) in blocks {
            writer.write_primitive_block_owned(block_bytes, blob_index, tagdata.as_deref())?;
        }
    }

    Ok(total)
}
