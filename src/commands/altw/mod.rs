//! Embed node coordinates in ways. Equivalent to `osmium add-locations-to-ways`.

pub mod external;
mod dense;
mod sparse;

use std::io::{Read, BufWriter, Write as _};
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;

use crate::blob::{
    decode_blob_to_headerblock, decompress_blob, parse_blob_header_with_index,
    parse_primitive_block_from_bytes_owned, BlobKind, DecompressPool, WireBlob,
};
use crate::blob_meta::{BlobIndex, ElemKind};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::file_reader::FileReader;
use crate::writer::{Compression, PbfWriter};
use crate::{Element, ElementReader, MemberId, PrimitiveBlock};

use super::{
    build_output_header, drain_batch_results, ensure_node_capacity_local,
    ensure_relation_capacity_local, ensure_way_capacity_local, flush_passthrough_buf,
    require_indexdata, writer_from_header, HeaderOverrides,
};
use crate::read::raw_frame::{read_raw_frame, RawBlobFrame};
use crate::idset::IdSet;

use super::{Result, BATCH_SIZE, BATCH_BYTE_BUDGET, BATCH_MIN_BLOBS, BATCH_MAX_BLOBS};

use self::dense::{build_node_index_dense, DenseMmapIndex};
use self::sparse::{build_node_index_sparse, SparseArrayIndex};

// ---------------------------------------------------------------------------
// Index type selection
// ---------------------------------------------------------------------------

/// Strategy for storing node coordinates during add-locations-to-ways.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IndexType {
    /// Direct-mapped array: `index[node_id] = (lat, lon)`. Fastest when the
    /// working set fits in RAM. At planet scale (~16 GB touched after pass 0
    /// filtering), this requires ~30+ GB of free memory to avoid page thrashing.
    #[default]
    Dense,
    /// Chunk-indexed sparse array with batched sorted lookups. Uses ~540 MB
    /// RAM for the chunk index plus a compact on-disk values file (~16 GB for
    /// planet). Way lookups are batched and sorted by file offset, converting
    /// random I/O into sequential scans. Works on memory-constrained hosts.
    Sparse,
    /// External join via double radix permutation. Bounded memory (<1 GB),
    /// all sequential I/O. Uses ~224 GB temp disk at planet scale. Best for
    /// memory-constrained hosts where dense thrashes and sparse is too slow.
    External,
    /// Auto-select: external if sorted + indexed, dense otherwise.
    Auto,
}

/// Parse error for [`IndexType`].
#[derive(Debug, Clone)]
pub struct ParseIndexTypeError(String);

impl std::fmt::Display for ParseIndexTypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseIndexTypeError {}

impl FromStr for IndexType {
    type Err = ParseIndexTypeError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "dense" => Ok(Self::Dense),
            "sparse" => Ok(Self::Sparse),
            "external" => Ok(Self::External),
            "auto" => Ok(Self::Auto),
            _ => Err(ParseIndexTypeError(format!(
                "unknown index type '{s}': expected 'dense', 'sparse', 'external', or 'auto'"
            ))),
        }
    }
}

impl std::fmt::Display for IndexType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dense => f.write_str("dense"),
            Self::Sparse => f.write_str("sparse"),
            Self::External => f.write_str("external"),
            Self::Auto => f.write_str("auto"),
        }
    }
}

/// 4 bytes lat + 4 bytes lon = 8 bytes per entry. Shared between the dense
/// mmap layout and the sparse values file.
const ENTRY_SIZE: usize = 8;

// ---------------------------------------------------------------------------
// Unified node index
// ---------------------------------------------------------------------------

/// Unified node coordinate index dispatching to either dense or sparse.
enum NodeIndex {
    Dense(DenseMmapIndex),
    Sparse(SparseArrayIndex),
}

impl NodeIndex {
    fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        match self {
            Self::Dense(idx) => idx.get(node_id),
            Self::Sparse(idx) => idx.get(node_id),
        }
    }
}

// ---------------------------------------------------------------------------
// Two-phase read: header-only classification + selective data read/skip
// ---------------------------------------------------------------------------

/// Blob header info from phase 1 of two-phase read.
///
/// Contains classification data (blob_type, index) and file position info
/// needed to either read the full blob data or skip it for copy_file_range.
struct BlobHeaderInfo {
    blob_type: BlobKind,
    data_size: usize,
    index: Option<BlobIndex>,
    #[allow(dead_code)]
    tagdata: Option<Box<[u8]>>,
    /// File offset where this frame starts (for copy_file_range).
    frame_start: u64,
    /// Total frame length: 4 + header_len + data_size.
    frame_len: usize,
    /// Raw header prefix: [len_buf(4) | header_bytes(header_len)].
    /// Used by `read_blob_data` to assemble the full frame.
    header_raw: Vec<u8>,
}

/// Read just the BlobHeader (phase 1). Returns `None` at EOF.
///
/// Advances `file_offset` by the header portion only (4 + header_len).
/// The blob data is NOT read - call `read_blob_data` or `skip_blob_data` next.
fn read_blob_header(
    reader: &mut FileReader,
    file_offset: &mut u64,
) -> Result<Option<BlobHeaderInfo>> {
    let frame_start = *file_offset;

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
    *file_offset += blob_offset as u64;

    let index = indexdata.and_then(|d| BlobIndex::deserialize(&d));

    // Assemble header_raw: [len_buf | header_bytes]
    let mut header_raw = Vec::with_capacity(blob_offset);
    header_raw.extend_from_slice(&len_buf);
    header_raw.extend_from_slice(&header_bytes);

    Ok(Some(BlobHeaderInfo {
        blob_type,
        data_size,
        index,
        tagdata,
        frame_start,
        frame_len,
        header_raw,
    }))
}

/// Read blob data after a header read (phase 2, decode path).
///
/// Consumes the `BlobHeaderInfo` and reads the blob data to produce a full
/// `RawBlobFrame`. Advances `file_offset` by `data_size`.
fn read_blob_data(
    reader: &mut FileReader,
    info: BlobHeaderInfo,
    file_offset: &mut u64,
) -> Result<RawBlobFrame> {
    let blob_offset = info.header_raw.len();
    let mut frame_bytes = Vec::with_capacity(info.frame_len);
    frame_bytes.extend_from_slice(&info.header_raw);
    frame_bytes.resize(info.frame_len, 0);
    reader.read_exact(&mut frame_bytes[blob_offset..])?;
    *file_offset += info.data_size as u64;

    Ok(RawBlobFrame {
        frame_bytes,
        blob_type: info.blob_type,
        blob_offset,
        index: info.index,
        tagdata: info.tagdata,
        file_offset: info.frame_start,
    })
}

/// Skip blob data after a header read (phase 2, passthrough path).
///
/// Advances the reader past the blob data without allocating or reading it
/// into userspace. Advances `file_offset` by `data_size`.
fn skip_blob_data(
    reader: &mut FileReader,
    data_size: usize,
    file_offset: &mut u64,
) -> Result<()> {
    reader.skip(data_size as u64)?;
    *file_offset += data_size as u64;
    Ok(())
}

// ---------------------------------------------------------------------------
// Batch slot for parallel decode
// ---------------------------------------------------------------------------

/// A slot in a parallel decode batch for the passthrough path.
enum BatchSlot {
    /// Way blob: decompress, enrich with node locations, re-encode.
    Way(RawBlobFrame),
    /// Node blob: decompress, filter untagged, re-encode.
    Node(RawBlobFrame),
    /// Unknown blob (no indexdata): decompress, inspect, process generically.
    Unknown(RawBlobFrame),
}

impl BatchSlot {
    fn frame(&self) -> &RawBlobFrame {
        match self {
            Self::Way(f) | Self::Node(f) | Self::Unknown(f) => f,
        }
    }
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Statistics from the add-locations-to-ways operation.
#[derive(Default)]
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
    /// Accumulate stats from another `Stats` instance into this one.
    pub fn merge(&mut self, src: &Stats) {
        self.nodes_read += src.nodes_read;
        self.nodes_written += src.nodes_written;
        self.nodes_dropped += src.nodes_dropped;
        self.ways_written += src.ways_written;
        self.relations_written += src.relations_written;
        self.missing_locations += src.missing_locations;
        self.blobs_passthrough += src.blobs_passthrough;
        self.blobs_decoded += src.blobs_decoded;
    }

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
#[allow(clippy::too_many_arguments)]
pub fn add_locations_to_ways(
    input: &Path,
    output: &Path,
    keep_untagged_nodes: bool,
    compression: Compression,
    direct_io: bool,
    force: bool,
    overrides: &HeaderOverrides,
    index_type: IndexType,
) -> Result<Stats> {
    // Auto-select: external if sorted + indexed, dense otherwise.
    let index_type = if index_type == IndexType::Auto {
        let reader = crate::ElementReader::open(input, direct_io)?;
        let sorted = reader.header().is_sorted();
        drop(reader);
        // Check indexdata presence without erroring (peek at first blob).
        let has_index = (|| -> Option<bool> {
            let mut r = crate::blob::BlobReader::open(input, direct_io).ok()?;
            r.set_parse_indexdata(true);
            r.next()?.ok()?; // skip header
            let blob = r.next()?.ok()?;
            Some(blob.index().is_some())
        })().unwrap_or(false);

        let chosen = if sorted && has_index {
            IndexType::External
        } else {
            IndexType::Dense
        };
        eprintln!("auto-selected --index-type {chosen} (sorted={sorted}, indexed={has_index})");
        chosen
    } else {
        index_type
    };

    // External join has its own pipeline - dispatch early.
    if index_type == IndexType::External {
        return external::external_join(
            input,
            output,
            keep_untagged_nodes,
            compression,
            direct_io,
            force,
            overrides,
        );
    }

    let indexdata_present = require_indexdata(input, direct_io, force,
        "input PBF has no blob-level indexdata. Without indexdata, every blob must be \
         decompressed and re-encoded (significantly slower).")?;

    // Suggest external index for sorted indexed PBFs on sparse selection.
    if index_type == IndexType::Sparse && indexdata_present {
        let reader = crate::ElementReader::open(input, direct_io)?;
        if reader.header().is_sorted() {
            eprintln!(
                "hint: this sorted indexed PBF is eligible for --index-type external, \
                 which uses bounded memory and sequential I/O (3.9x faster than dense \
                 at planet scale). Sparse is slower than both dense and external on \
                 sorted inputs."
            );
        }
    }

    let scratch_dir = output.parent().unwrap_or(Path::new("."));

    // Pass 0: collect the set of node IDs referenced by ways. Only these
    // nodes need coordinate lookups, so only these get indexed. At planet
    // scale this reduces touched mmap pages from ~80 GB to ~16 GB.
    crate::debug::emit_marker("ALTW_PASS0_START");
    let referenced = collect_way_referenced_node_ids(input, direct_io)?;
    crate::debug::emit_marker("ALTW_PASS0_END");

    crate::debug::emit_marker("ALTW_PASS1_START");
    let index = build_node_index(input, direct_io, scratch_dir, &referenced, index_type)?;
    crate::debug::emit_marker("ALTW_PASS1_END");
    drop(referenced);

    let relation_member_node_ids = if keep_untagged_nodes {
        None
    } else {
        Some(collect_relation_member_node_ids(input, direct_io)?)
    };
    crate::debug::emit_marker("ALTW_PASS2_START");
    let result = write_output_checked(
        input,
        output,
        &index,
        keep_untagged_nodes,
        relation_member_node_ids.as_ref(),
        compression,
        direct_io,
        indexdata_present,
        overrides,
    );
    crate::debug::emit_marker("ALTW_PASS2_END");
    result
}

// ---------------------------------------------------------------------------
// Pass 1: Build node coordinate index
// ---------------------------------------------------------------------------

/// Number of decoded `PrimitiveBlock`s collected before dispatching to rayon
/// for parallel node index population.
fn build_node_index(
    input: &Path,
    direct_io: bool,
    scratch_dir: &Path,
    referenced: &IdSet,
    index_type: IndexType,
) -> Result<NodeIndex> {
    match index_type {
        IndexType::Dense => {
            build_node_index_dense(input, direct_io, scratch_dir, referenced)
                .map(NodeIndex::Dense)
        }
        IndexType::Sparse => {
            build_node_index_sparse(input, direct_io, scratch_dir, referenced)
                .map(NodeIndex::Sparse)
        }
        IndexType::External | IndexType::Auto => unreachable!("resolved before build_node_index"),
    }
}

/// Collect all node IDs referenced by ways (pass 0).
///
/// Scans only way blobs (via `BlobFilter`) and builds a bitset of every node
/// ID that appears in any way's refs list. At planet scale (~2B unique node
/// refs), this costs ~1.6 GB - far less than indexing all 10.4B nodes.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn collect_way_referenced_node_ids(input: &Path, direct_io: bool) -> Result<IdSet> {
    // Way-ref scanner: bypasses PrimitiveBlock construction (no string table,
    // no group_ranges). Only extracts way refs for IdSet population.
    let mut blob_reader = crate::blob::BlobReader::open(input, direct_io)?;
    blob_reader.set_parse_indexdata(true);
    blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut referenced = IdSet::new();
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut group_starts: Vec<(usize, usize)> = Vec::new();

    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) { continue; }
        if let Some(idx) = blob.index() {
            if !matches!(idx.kind, crate::blob_meta::ElemKind::Way) { continue; }
        }
        blob.decompress_into(&mut decompress_buf)?;
        crate::scan::way::scan_way_refs(&decompress_buf, &mut refs_buf, &mut group_starts, |_way_id, refs| {
            for &node_id in refs {
                if node_id >= 0 {
                    referenced.set(node_id);
                }
            }
        })?;
    }
    Ok(referenced)
}

/// Collect all node IDs referenced by relation members.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn collect_relation_member_node_ids(input: &Path, direct_io: bool) -> Result<IdSet> {
    // Sequential reader - only ~2K relation blobs at Europe scale, so retention
    // is negligible, but sequential is consistent with the other collection passes.
    let mut blob_reader = crate::blob::BlobReader::open(input, direct_io)?;
    blob_reader.set_parse_indexdata(true);
    blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut member_node_ids = IdSet::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) { continue; }
        if let Some(idx) = blob.index() {
            if !matches!(idx.kind, crate::blob_meta::ElemKind::Relation) { continue; }
        }
        blob.decompress_into(&mut decompress_buf)?;
        let block = crate::block::PrimitiveBlock::from_vec_with_scratch(
            std::mem::take(&mut decompress_buf), &mut st_scratch, &mut gr_scratch,
        )?;
        for element in block.elements_skip_metadata() {
            if let Element::Relation(r) = element {
                for member in r.members() {
                    if let MemberId::Node(id) = member.id
                        && id >= 0
                    {
                        member_node_ids.set(id);
                    }
                }
            }
        }
    }
    Ok(member_node_ids)
}

// ---------------------------------------------------------------------------
// Pass 2: Write output with locations on ways
// ---------------------------------------------------------------------------


#[allow(clippy::too_many_arguments)]
fn write_output_checked(
    input: &Path,
    output: &Path,
    index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    compression: Compression,
    direct_io: bool,
    indexdata_present: bool,
    overrides: &HeaderOverrides,
) -> Result<Stats> {
    if indexdata_present {
        write_output_passthrough(
            input,
            output,
            index,
            keep_untagged_nodes,
            relation_member_node_ids,
            compression,
            direct_io,
            overrides,
        )
    } else {
        write_output_decode_all(
            input,
            output,
            index,
            keep_untagged_nodes,
            relation_member_node_ids,
            compression,
            direct_io,
            overrides,
        )
    }
}

// ---------------------------------------------------------------------------
// Pass 2a: Decode-all fallback (no indexdata)
// ---------------------------------------------------------------------------

#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
fn write_output_decode_all(
    input: &Path,
    output: &Path,
    index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<Stats> {
    let mut stats = Stats::default();

    let reader = ElementReader::open(input, direct_io)?;
    let mut writer = writer_from_header(
        output,
        compression,
        reader.header(),
        true,
        overrides,
        |hb| hb.optional_feature("LocationsOnWays"),
        direct_io,
        false,
    )?;

    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(BATCH_SIZE);

    for block in reader.into_blocks_pipelined() {
        batch.push(block?);

        if batch.len() >= BATCH_SIZE {
            let batch_stats = process_batch(
                &batch,
                &mut writer,
                index,
                keep_untagged_nodes,
                relation_member_node_ids,
            )?;
            stats.merge(&batch_stats);
            batch.clear();
        }
    }

    if !batch.is_empty() {
        let batch_stats = process_batch(
            &batch,
            &mut writer,
            index,
            keep_untagged_nodes,
            relation_member_node_ids,
        )?;
        stats.merge(&batch_stats);
    }

    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Parallel batch processing
// ---------------------------------------------------------------------------

use super::flush_local;
use crate::owned::{dense_node_metadata, element_metadata};


// ---------------------------------------------------------------------------
// Batched sorted lookups for sparse index
// ---------------------------------------------------------------------------

use rustc_hash::FxHashMap;

/// How to resolve node coordinates during way processing.
enum LocationLookup<'a> {
    /// Direct random access (dense index or sparse with small dataset).
    Index(&'a NodeIndex),
    /// Pre-resolved map from batched sorted lookup (sparse, large dataset).
    Resolved(&'a FxHashMap<i64, (i32, i32)>),
}

impl LocationLookup<'_> {
    fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        match self {
            Self::Index(idx) => idx.get(node_id),
            Self::Resolved(map) => map.get(&node_id).copied(),
        }
    }
}

/// Entry for sorting lookups by file offset.
struct LookupEntry {
    /// Byte offset into the sparse index values mmap.
    mmap_offset: u64,
    /// The node ID (used as key in the result map).
    node_id: i64,
}

/// Collect all unique way node refs from a batch of blocks, resolve their
/// coordinates via sorted sequential access through the sparse index mmap,
/// and return a map of node_id → (lat, lon).
///
/// This converts random I/O (one page fault per lookup) into sequential I/O
/// (one pass through the mmap in file order). At planet scale, a batch of
/// ~128 way blobs contains ~100K unique node refs. Sorting these by mmap
/// offset and scanning sequentially touches each page at most once.
fn resolve_batch_locations(
    blocks: &[PrimitiveBlock],
    sparse: &SparseArrayIndex,
) -> FxHashMap<i64, (i32, i32)> {
    // Collect all unique node refs with their mmap offsets.
    let mut entries: Vec<LookupEntry> = Vec::new();
    let mut seen = FxHashMap::<i64, ()>::default();

    for block in blocks {
        for element in block.elements_skip_metadata() {
            if let Element::Way(w) = element {
                for node_id in w.refs() {
                    if seen.contains_key(&node_id) {
                        continue;
                    }
                    seen.insert(node_id, ());
                    if let Some(offset) = sparse.byte_offset(node_id) {
                        entries.push(LookupEntry { mmap_offset: offset, node_id });
                    }
                }
            }
        }
    }

    // Sort by mmap offset → sequential access pattern.
    entries.sort_unstable_by_key(|e| e.mmap_offset);

    // Resolve coordinates via sequential scan.
    let mut result = FxHashMap::with_capacity_and_hasher(entries.len(), Default::default());
    for entry in &entries {
        if let Some(coords) = sparse.get_at_offset(entry.mmap_offset) {
            result.insert(entry.node_id, coords);
        }
    }

    result
}


/// Process a single `PrimitiveBlock`, writing elements into the thread-local
/// `BlockBuilder` and flushing complete blocks into `output`.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
fn process_block(
    block: &PrimitiveBlock,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    lookup: &LocationLookup<'_>,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    refs_buf: &mut Vec<i64>,
    locations_buf: &mut Vec<(i32, i32)>,
) -> std::result::Result<Stats, String> {
    let mut stats = Stats::default();

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
                    let meta = dense_node_metadata(dn);
                    bb.add_node(dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), dn.tags(), meta.as_ref());
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
                    let meta = element_metadata(&n.info());
                    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), n.tags(), meta.as_ref());
                    stats.nodes_written += 1;
                } else {
                    stats.nodes_dropped += 1;
                }
            }
            Element::Way(w) => {
                ensure_way_capacity_local(bb, output)?;
                refs_buf.clear();
                refs_buf.extend(w.refs());
                locations_buf.clear();
                for node_id in refs_buf.iter() {
                    match lookup.get(*node_id) {
                        Some(loc) => locations_buf.push(loc),
                        None => {
                            stats.missing_locations += 1;
                            locations_buf.push((0, 0));
                        }
                    }
                }
                let meta = element_metadata(&w.info());
                bb.add_way_with_locations(w.id(), w.tags(), refs_buf, locations_buf, meta.as_ref());
                stats.ways_written += 1;
            }
            Element::Relation(r) => {
                ensure_relation_capacity_local(bb, output)?;
                members_buf.clear();
                members_buf.extend(r.members().map(|m| MemberData {
                    id: m.id,
                    role: m.role().unwrap_or(""),
                }));
                let meta = element_metadata(&r.info());
                bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
                stats.relations_written += 1;
            }
        }
    }

    Ok(stats)
}

/// Process a batch of `PrimitiveBlock`s in parallel via rayon.
///
/// For sparse indexes: pre-resolves all way node coordinates via sorted
/// sequential scan before parallel processing (avoids random mmap I/O).
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn process_batch(
    batch: &[PrimitiveBlock],
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
) -> Result<Stats> {
    // For sparse index: resolve all way node coordinates upfront.
    let resolved_map;
    let lookup = match index {
        NodeIndex::Dense(_) => LocationLookup::Index(index),
        NodeIndex::Sparse(sparse) => {
            resolved_map = resolve_batch_locations(batch, sparse);
            LocationLookup::Resolved(&resolved_map)
        }
    };

    type BatchResult = std::result::Result<(Vec<OwnedBlock>, Stats), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            || (BlockBuilder::new(), Vec::<i64>::new(), Vec::<(i32, i32)>::new()),
            |(bb, refs_buf, locations_buf), block| {
                let mut output: Vec<OwnedBlock> = Vec::new();
                let block_stats = process_block(
                    block,
                    bb,
                    &mut output,
                    &lookup,
                    keep_untagged_nodes,
                    relation_member_node_ids,
                    refs_buf, locations_buf,
                )?;
                flush_local(bb, &mut output)?;
                Ok((output, block_stats))
            },
        )
        .collect();

    let mut total = Stats::default();

    drain_batch_results(results, writer, |s| total.merge(&s))?;

    Ok(total)
}

// ---------------------------------------------------------------------------
// Passthrough coalescing
// ---------------------------------------------------------------------------

/// Accumulate raw passthrough frames into a chunk list (no memcpy).
fn coalesce_passthrough(frame: &mut RawBlobFrame, chunks: &mut Vec<Vec<u8>>) {
    chunks.push(std::mem::take(&mut frame.frame_bytes));
}

// ---------------------------------------------------------------------------
// Copy-range passthrough (linux-direct-io: kernel-space copy via copy_file_range)
// ---------------------------------------------------------------------------

/// Coalesced file range for kernel-space passthrough copy.
///
/// Consecutive passthrough blobs produce contiguous byte ranges in the input
/// file. Rather than issuing a `write_raw_copy` per blob (like merge), we
/// extend the range and flush once per contiguous run. At planet scale,
/// hundreds of consecutive passthrough blobs are common.
#[cfg(feature = "linux-direct-io")]
struct CopyRange {
    input_fd: std::os::unix::io::RawFd,
    start: u64,
    len: u64,
}

#[cfg(feature = "linux-direct-io")]
impl CopyRange {
    fn new(input_fd: std::os::unix::io::RawFd) -> Self {
        Self { input_fd, start: 0, len: 0 }
    }

    fn extend(&mut self, frame_start: u64, frame_len: u64) {
        if self.len == 0 {
            self.start = frame_start;
            self.len = frame_len;
        } else {
            debug_assert_eq!(self.start + self.len, frame_start);
            self.len += frame_len;
        }
    }

    fn flush(
        &mut self,
        writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    ) -> Result<()> {
        if self.len > 0 {
            writer.write_raw_copy(self.input_fd, self.start, self.len)?;
            self.len = 0;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pass 2b: Passthrough path (indexdata present)
// ---------------------------------------------------------------------------

/// Read raw header blob, build output header with `LocationsOnWays`.
fn read_header_raw<R: Read>(
    reader: &mut R,
    file_offset: &mut u64,
    overrides: &HeaderOverrides,
) -> Result<(Vec<u8>, bool)> {
    while let Some(frame) = read_raw_frame(reader, file_offset)? {
        if frame.blob_type == BlobKind::OsmHeader {
            let header = decode_blob_to_headerblock(frame.blob_bytes())?;
            let sorted = header.is_sorted();
            let header_bytes = build_output_header(&header, true, overrides, |hb| {
                hb.optional_feature("LocationsOnWays")
            })?;
            return Ok((header_bytes, sorted));
        }
    }
    Err("no OSMHeader blob found".into())
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn write_output_passthrough(
    input: &Path,
    output: &Path,
    node_index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<Stats> {
    let mut stats = Stats::default();

    let mut reader = FileReader::open(input, direct_io)?;
    let mut file_offset: u64 = 0;
    let (header_bytes, _sorted) = read_header_raw(&mut reader, &mut file_offset, overrides)?;
    let mut writer = super::writer_from_header_bytes(output, compression, &header_bytes, direct_io, false)?;

    // Open second handle for copy_file_range (explicit offsets, thread-safe).
    #[cfg(feature = "linux-direct-io")]
    let (_copy_fd_file, use_copy_range) = {
        let f = FileReader::buffered(input)?;
        (f, !direct_io)
    };
    #[cfg(feature = "linux-direct-io")]
    let mut copy_range = {
        let fd = _copy_fd_file.raw_fd();
        CopyRange::new(fd)
    };

    let mut batch: Vec<BatchSlot> = Vec::with_capacity(BATCH_MAX_BLOBS);
    let mut batch_bytes: usize = 0;
    // Coalescing buffer for non-copy-range passthrough (without linux-direct-io,
    // or when copy_file_range is incompatible with O_DIRECT output).
    let mut passthrough_chunks: Vec<Vec<u8>> = Vec::new();

    while let Some(header) = read_blob_header(&mut reader, &mut file_offset)? {
        if header.blob_type != BlobKind::OsmData {
            skip_blob_data(&mut reader, header.data_size, &mut file_offset)?;
            continue;
        }

        let kind = header.index.as_ref().map(|idx| idx.kind);
        let is_passthrough = matches!(kind, Some(ElemKind::Relation))
            || matches!(kind, Some(ElemKind::Node) if keep_untagged_nodes);

        if is_passthrough {
            // Flush pending decode batch before writing passthrough blobs to
            // preserve input element ordering (nodes → ways → relations).
            // Without this, the last decode batch (ways) could be written after
            // passthrough blobs (relations) at the type boundary.
            if !batch.is_empty() {
                #[cfg(feature = "linux-direct-io")]
                copy_range.flush(&mut writer)?;
                flush_passthrough_buf(&mut passthrough_chunks, &mut writer)?;
                let batch_stats = process_slot_batch(
                    &batch,
                    &mut writer,
                    node_index,
                    keep_untagged_nodes,
                    relation_member_node_ids,
                )?;
                stats.merge(&batch_stats);
                batch.clear();
                batch_bytes = 0;
            }

            // Update stats from indexdata.
            if let Some(ref idx) = header.index {
                match idx.kind {
                    ElemKind::Node => {
                        stats.nodes_read += idx.count;
                        stats.nodes_written += idx.count;
                    }
                    ElemKind::Relation => {
                        stats.relations_written += idx.count;
                    }
                    ElemKind::Way => {}
                }
            }
            stats.blobs_passthrough += 1;

            // With copy_file_range: skip blob data, extend kernel copy range.
            // Without: read full frame and coalesce into userspace buffer.
            #[cfg(feature = "linux-direct-io")]
            if use_copy_range {
                skip_blob_data(&mut reader, header.data_size, &mut file_offset)?;
                copy_range.extend(header.frame_start, header.frame_len as u64);
            }
            #[cfg(feature = "linux-direct-io")]
            if !use_copy_range {
                let mut frame = read_blob_data(&mut reader, header, &mut file_offset)?;
                coalesce_passthrough(&mut frame, &mut passthrough_chunks);
            }
            #[cfg(not(feature = "linux-direct-io"))]
            {
                let mut frame = read_blob_data(&mut reader, header, &mut file_offset)?;
                coalesce_passthrough(&mut frame, &mut passthrough_chunks);
            }
        } else {
            // Flush any pending copy range before decoding - the next passthrough
            // blob may not be contiguous with the previous one (decode blobs in
            // between break contiguity).
            #[cfg(feature = "linux-direct-io")]
            copy_range.flush(&mut writer)?;
            flush_passthrough_buf(&mut passthrough_chunks, &mut writer)?;
            // Decode: read full frame, classify into batch slot.
            let frame = read_blob_data(&mut reader, header, &mut file_offset)?;
            stats.blobs_decoded += 1;
            batch_bytes += frame.frame_bytes.len();
            match kind {
                Some(ElemKind::Node) => batch.push(BatchSlot::Node(frame)),
                Some(ElemKind::Way) => batch.push(BatchSlot::Way(frame)),
                _ => batch.push(BatchSlot::Unknown(frame)),
            }
        }

        // Dispatch when byte budget reached or batch is full.
        if batch.len() >= BATCH_MAX_BLOBS
            || (batch.len() >= BATCH_MIN_BLOBS && batch_bytes >= BATCH_BYTE_BUDGET)
        {
            #[cfg(feature = "linux-direct-io")]
            copy_range.flush(&mut writer)?;
            flush_passthrough_buf(&mut passthrough_chunks, &mut writer)?;
            let batch_stats = process_slot_batch(
                &batch,
                &mut writer,
                node_index,
                keep_untagged_nodes,
                relation_member_node_ids,
            )?;
            stats.merge(&batch_stats);
            batch.clear();
            batch_bytes = 0;
        }
    }

    // Flush remaining decode batch, then passthrough.
    if !batch.is_empty() {
        let batch_stats = process_slot_batch(
            &batch,
            &mut writer,
            node_index,
            keep_untagged_nodes,
            relation_member_node_ids,
        )?;
        stats.merge(&batch_stats);
    }
    #[cfg(feature = "linux-direct-io")]
    copy_range.flush(&mut writer)?;
    flush_passthrough_buf(&mut passthrough_chunks, &mut writer)?;

    writer.flush()?;
    Ok(stats)
}

/// Decompress and parse a batch of slots in parallel.
fn decompress_slot_batch(
    batch: &[BatchSlot],
) -> std::result::Result<Vec<PrimitiveBlock>, String> {
    batch
        .par_iter()
        .map_init(
            DecompressPool::new,
            |pool, slot| {
                let wire_blob = WireBlob::parse_slice(slot.frame().blob_bytes())
                    .map_err(|e| e.to_string())?;
                let bytes = decompress_blob(&wire_blob, Some(pool))
                    .map_err(|e| e.to_string())?;
                parse_primitive_block_from_bytes_owned(&bytes)
                    .map_err(|e| e.to_string())
            },
        )
        .collect()
}

/// Process a batch of slots in parallel: decompress, transform, write.
///
/// For sparse indexes: decompresses all blobs first, pre-resolves way node
/// coordinates via sorted sequential scan, then processes in parallel.
/// For dense indexes: decompresses and processes in one parallel pass.
fn process_slot_batch(
    batch: &[BatchSlot],
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    node_index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
) -> Result<Stats> {
    // For sparse: decompress first, resolve locations, then process.
    // For dense: decompress + process in one pass (original path).
    let resolved_map;
    let decoded_blocks;
    let (blocks_ref, lookup): (&[PrimitiveBlock], LocationLookup<'_>) = match node_index {
        NodeIndex::Dense(_) => {
            // Dense path: decompress + process in single parallel pass.
            return process_slot_batch_dense(
                batch, writer, node_index, keep_untagged_nodes, relation_member_node_ids,
            );
        }
        NodeIndex::Sparse(sparse) => {
            decoded_blocks = decompress_slot_batch(batch)
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            resolved_map = resolve_batch_locations(&decoded_blocks, sparse);
            (&decoded_blocks, LocationLookup::Resolved(&resolved_map))
        }
    };

    type SlotResult = std::result::Result<(Vec<OwnedBlock>, Stats), String>;
    let results: Vec<SlotResult> = blocks_ref
        .par_iter()
        .map_init(
            || (BlockBuilder::new(), Vec::<i64>::new(), Vec::<(i32, i32)>::new()),
            |(bb, refs_buf, locations_buf), block| {
                let mut output: Vec<OwnedBlock> = Vec::new();
                let block_stats = process_block(
                    block, bb, &mut output, &lookup,
                    keep_untagged_nodes, relation_member_node_ids,
                    refs_buf, locations_buf,
                )?;
                flush_local(bb, &mut output)?;
                Ok((std::mem::take(&mut output), block_stats))
            },
        )
        .collect();

    let mut total = Stats {
        nodes_read: 0, nodes_written: 0, nodes_dropped: 0,
        ways_written: 0, relations_written: 0, missing_locations: 0,
        blobs_passthrough: 0, blobs_decoded: 0,
    };
    drain_batch_results(results, writer, |s| total.merge(&s))?;
    Ok(total)
}

/// Dense-index path for slot batch: decompress + process in one parallel pass.
fn process_slot_batch_dense(
    batch: &[BatchSlot],
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    node_index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSet>,
) -> Result<Stats> {
    type SlotResult = std::result::Result<(Vec<OwnedBlock>, Stats), String>;
    let lookup = LocationLookup::Index(node_index);

    let results: Vec<SlotResult> = batch
        .par_iter()
        .map_init(
            || {
                (
                    BlockBuilder::new(),
                    Vec::<OwnedBlock>::new(),
                    Vec::<i64>::new(),
                    Vec::<(i32, i32)>::new(),
                    DecompressPool::new(),
                )
            },
            |(bb, output, refs_buf, locations_buf, pool), slot| {
                output.clear();

                let wire_blob = WireBlob::parse_slice(slot.frame().blob_bytes())
                    .map_err(|e| e.to_string())?;
                let bytes = decompress_blob(&wire_blob, Some(pool))
                    .map_err(|e| e.to_string())?;
                let block = parse_primitive_block_from_bytes_owned(&bytes)
                    .map_err(|e| e.to_string())?;

                let block_stats = process_block(
                    &block, bb, output, &lookup,
                    keep_untagged_nodes, relation_member_node_ids,
                    refs_buf, locations_buf,
                )?;

                flush_local(bb, output)?;
                Ok((std::mem::take(output), block_stats))
            },
        )
        .collect();

    let mut total = Stats {
        nodes_read: 0, nodes_written: 0, nodes_dropped: 0,
        ways_written: 0, relations_written: 0, missing_locations: 0,
        blobs_passthrough: 0, blobs_decoded: 0,
    };
    drain_batch_results(results, writer, |s| total.merge(&s))?;
    Ok(total)
}
