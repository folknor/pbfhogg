//! Embed node coordinates in ways. Equivalent to `osmium add-locations-to-ways`.

use std::io::{Read, BufWriter, Write as _};
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;

use crate::blob::{
    decode_blob_to_headerblock, decompress_blob, parse_blob_header_with_index,
    parse_primitive_block_from_bytes_owned, BlobKind, DecompressPool, WireBlob,
};
use crate::blob_index::{BlobIndex, ElemKind};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::file_reader::FileReader;
use crate::writer::{Compression, PbfWriter};
use crate::{BlobFilter, Element, ElementReader, MemberId, PrimitiveBlock};

use super::{
    build_output_header, drain_batch_results, ensure_node_capacity_local,
    ensure_relation_capacity_local, ensure_way_capacity_local, flush_passthrough_buf,
    read_raw_frame, require_indexdata, writer_from_header, HeaderOverrides, RawBlobFrame,
};
use super::id_set_dense::IdSetDense;

use super::{Result, BATCH_SIZE, BATCH_BYTE_BUDGET, BATCH_MIN_BLOBS, BATCH_MAX_BLOBS};

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
            _ => Err(ParseIndexTypeError(format!(
                "unknown index type '{s}': expected 'dense', 'sparse', or 'external'"
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
        }
    }
}

/// Default dense index capacity: 16 billion entries (128 GB virtual).
/// Covers current OSM max node ID (~12.5B) with headroom for growth.
const DENSE_INDEX_DEFAULT_CAPACITY: usize = 16_000_000_000;

// ---------------------------------------------------------------------------
// Dense mmap index
// ---------------------------------------------------------------------------

/// File-backed mmap node location index.
///
/// Direct indexing: `mmap[node_id * 8 .. node_id * 8 + 8]` stores
/// `(lat: i32, lon: i32)` packed as 8 bytes (little-endian).
///
/// Backed by a temporary file (created and immediately unlinked). The OS
/// manages physical memory via page cache: under memory pressure, clean
/// pages are evicted and re-read from disk on demand. This allows the index
/// to exceed physical RAM without OOM — at planet scale (~68 GB touched),
/// the kernel pages data in/out transparently.
///
/// Sentinel: `(0, 0)` means unset. ~116 nodes at exactly null island (0°N, 0°E)
/// will appear as missing — acceptable ambiguity for diagnostic counters.
pub(crate) struct DenseMmapIndex {
    mmap: memmap2::MmapMut,
    _file: std::fs::File,
    capacity: usize,
}

/// 4 bytes lat + 4 bytes lon = 8 bytes per entry.
const ENTRY_SIZE: usize = 8;

// Require 64-bit platform for dense index (32-bit cannot address 128 GB).
const _: () = assert!(std::mem::size_of::<usize>() >= 8);

impl DenseMmapIndex {
    /// Look up a node's coordinates by ID. Returns `None` for unset entries.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub(crate) fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        if node_id < 0 {
            return None;
        }
        let idx = node_id as usize;
        if idx >= self.capacity {
            return None;
        }
        let offset = idx * ENTRY_SIZE;
        // SAFETY: offset + 8 <= capacity * ENTRY_SIZE = mmap length.
        // Pointer is 8-byte aligned (page-aligned base + 8*idx).
        // Atomic load pairs with atomic stores in SharedDenseWriter::insert.
        let packed = unsafe {
            let ptr = self.mmap.as_ptr().add(offset).cast::<AtomicU64>();
            (*ptr).load(Ordering::Relaxed)
        };
        if packed == 0 {
            return None;
        }
        let lat = packed as i32;
        let lon = (packed >> 32) as i32;
        Some((lat, lon))
    }

    pub(crate) fn new(capacity: usize, scratch_dir: &Path) -> Result<Self> {
        let byte_len = capacity
            .checked_mul(ENTRY_SIZE)
            .ok_or("dense index capacity overflow")?;
        let temp_path = scratch_dir.join(format!(
            ".pbfhogg-node-index-{}",
            std::process::id()
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|e| {
                format!(
                    "failed to create index temp file at {}: {e}",
                    temp_path.display()
                )
            })?;
        // Unlink immediately — fd keeps the file alive, OS cleans up on close/crash.
        // Ignore errors: unlink failure is non-fatal (file just won't auto-clean).
        drop(std::fs::remove_file(&temp_path));
        file.set_len(byte_len as u64).map_err(|e| {
            format!(
                "failed to set index file size ({} GB): {e}",
                byte_len / 1_000_000_000
            )
        })?;
        // SAFETY: file is exclusively owned, opened read+write, and sized to byte_len.
        let mmap = unsafe {
            memmap2::MmapMut::map_mut(&file).map_err(|e| {
                format!(
                    "failed to mmap index file ({} GB): {e}",
                    byte_len / 1_000_000_000
                )
            })?
        };
        Ok(Self { mmap, _file: file, capacity })
    }

    /// Insert a node's coordinates (sequential use only).
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub(crate) fn set(&mut self, node_id: i64, lat: i32, lon: i32) {
        if node_id < 0 {
            return;
        }
        let idx = node_id as usize;
        if idx >= self.capacity {
            return;
        }
        let offset = idx * ENTRY_SIZE;
        let packed = (lat as u32 as u64) | ((lon as u32 as u64) << 32);
        self.mmap[offset..offset + ENTRY_SIZE].copy_from_slice(&packed.to_le_bytes());
    }
}

// ---------------------------------------------------------------------------
// Parallel dense index writer
// ---------------------------------------------------------------------------

/// Thread-safe writer for parallel dense index population.
///
/// Holds a raw pointer into the `DenseMmapIndex` mmap buffer. Each node ID
/// maps to a disjoint 8-byte slot (`base + node_id * 8`). All writes use
/// `AtomicU64::store(Relaxed)`, eliminating data-race UB even if duplicate
/// node IDs appear in the input (e.g. from corrupt or non-canonical PBFs).
///
/// The caller must ensure the `DenseMmapIndex` outlives all uses of this
/// writer. In practice, both live in `build_node_index_dense` and `par_iter`
/// is synchronous (blocks until complete), so the pointer cannot escape.
struct SharedDenseWriter {
    base: *mut u8,
    capacity: usize,
}

// SAFETY: All writes use atomic operations (AtomicU64 stores), eliminating
// data-race UB. The raw pointer requires manual Send+Sync; lifetime is
// bounded by the synchronous par_iter in build_node_index_dense.
unsafe impl Send for SharedDenseWriter {}
unsafe impl Sync for SharedDenseWriter {}

impl SharedDenseWriter {
    /// Insert a node's coordinates. Silently ignores negative IDs and IDs
    /// beyond capacity (same semantics as `DenseMmapIndex::get`).
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
        let packed = (lat as u32 as u64) | ((lon as u32 as u64) << 32);
        // SAFETY: offset + 8 <= capacity * ENTRY_SIZE = mmap length.
        // Pointer is 8-byte aligned (page-aligned base + 8*idx).
        // Atomic store eliminates data-race UB even with duplicate node IDs.
        unsafe {
            let ptr = self.base.add(offset).cast::<AtomicU64>();
            (*ptr).store(packed, Ordering::Relaxed);
        }
    }
}

// ---------------------------------------------------------------------------
// Sparse array index (Planetiler-inspired)
// ---------------------------------------------------------------------------

/// Bits to shift a node ID right to get the chunk index.
const CHUNK_SHIFT: u32 = 8;
/// Number of entries per chunk (256).
const CHUNK_MASK: u64 = (1u64 << CHUNK_SHIFT) - 1;
/// Marker for chunks with no entries.
const CHUNK_NOT_PRESENT: u64 = u64::MAX;

/// Chunk-indexed sparse node coordinate store.
///
/// Partitions the node ID space into chunks of 256 IDs. Each chunk stores
/// a contiguous run of `(lat: i32, lon: i32)` entries in a file-backed mmap,
/// with leading empty slots trimmed via `start_pad` and gaps filled with
/// sentinel `(0, 0)` values. Trailing slots after the last entry in each
/// chunk are also filled with sentinels when the next chunk begins.
///
/// RAM cost: `offsets` (8 bytes/chunk) + `start_pad` (1 byte/chunk).
/// At planet scale (~49M chunks): ~440 MB RAM + ~16 GB on disk.
///
/// Requires sequential writes in ascending node ID order (satisfied by
/// sorted PBF files).
struct SparseArrayIndex {
    /// Byte offset into the values mmap where each chunk starts.
    offsets: Vec<u64>,
    /// Leading empty slots skipped per chunk.
    start_pad: Vec<u8>,
    /// Packed (lat: i32, lon: i32) values, file-backed read-only mmap.
    mmap: memmap2::Mmap,
    _file: std::fs::File,
}

impl SparseArrayIndex {
    /// Look up a node's coordinates. Returns `None` for negative IDs,
    /// chunks not present, or sentinel entries.
    /// Look up coordinates from the mmap at a computed byte offset.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    fn read_at(&self, base: u64, slot: u64) -> Option<(i32, i32)> {
        let byte_offset = (base + slot * ENTRY_SIZE as u64) as usize;
        let end = byte_offset + ENTRY_SIZE;
        if end > self.mmap.len() {
            return None;
        }
        let bytes = &self.mmap[byte_offset..end];
        let lat = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let lon = i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if lat == 0 && lon == 0 {
            return None; // sentinel
        }
        Some((lat, lon))
    }

    /// Resolve a chunk base and slot for a node ID. Returns `None` if the
    /// node cannot be in this index.
    #[allow(clippy::cast_sign_loss)]
    fn resolve(&self, node_id: i64) -> Option<(u64, u64)> {
        if node_id < 0 {
            return None;
        }
        let id = node_id as u64;
        let chunk_id = (id >> CHUNK_SHIFT) as usize;
        if chunk_id >= self.offsets.len() {
            return None;
        }
        let base = self.offsets[chunk_id];
        if base == CHUNK_NOT_PRESENT {
            return None;
        }
        let offset_in_chunk = (id & CHUNK_MASK) as u8;
        let pad = self.start_pad[chunk_id];
        if offset_in_chunk < pad {
            return None;
        }
        let slot = (offset_in_chunk - pad) as u64;
        Some((base, slot))
    }

    #[allow(clippy::cast_sign_loss)]
    fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        let (base, slot) = self.resolve(node_id)?;
        self.read_at(base, slot)
    }

    /// Compute the byte offset into the values mmap for a node ID.
    /// Used by batched sorted lookups to sort by file position.
    #[allow(clippy::cast_sign_loss)]
    fn byte_offset(&self, node_id: i64) -> Option<u64> {
        let (base, slot) = self.resolve(node_id)?;
        Some(base + slot * ENTRY_SIZE as u64)
    }

    /// Read a `(lat, lon)` pair at a known valid byte offset.
    /// The offset must have been produced by `byte_offset()`.
    #[allow(clippy::cast_possible_truncation)]
    fn get_at_offset(&self, byte_offset: u64) -> Option<(i32, i32)> {
        let start = byte_offset as usize;
        let end = start + ENTRY_SIZE;
        if end > self.mmap.len() {
            return None;
        }
        let bytes = &self.mmap[start..end];
        let lat = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let lon = i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if lat == 0 && lon == 0 {
            return None;
        }
        Some((lat, lon))
    }
}

/// Build a sparse array index from node blobs.
///
/// Writes values sequentially to a temp file, tracking chunk boundaries.
/// Nodes must arrive in ascending ID order (guaranteed by sorted PBFs).
/// Only nodes present in `referenced` are stored.
#[allow(clippy::cast_sign_loss, clippy::too_many_lines)]
fn build_node_index_sparse(
    input: &Path,
    direct_io: bool,
    scratch_dir: &Path,
    referenced: &IdSetDense,
) -> Result<SparseArrayIndex> {
    crate::debug_log!(
        "add-locations-to-ways: sparse index build start input={} scratch={} {}",
        input.display(),
        scratch_dir.display(),
        crate::debug::rss_line(),
    );
    // Pre-size for planet-scale: max node ID ~12.5B → ~49M chunks.
    let initial_chunks = 50_000_000;
    let mut offsets: Vec<u64> = Vec::new();
    let mut start_pad: Vec<u8> = Vec::new();

    let temp_path = scratch_dir.join(format!(
        ".pbfhogg-sparse-index-{}",
        std::process::id()
    ));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|e| format!("failed to create sparse index temp file: {e}"))?;
    drop(std::fs::remove_file(&temp_path));

    let sentinel = [0u8; ENTRY_SIZE];
    let mut writer = BufWriter::with_capacity(256 * 1024, &file);
    let mut current_chunk: usize = usize::MAX; // no chunk yet
    let mut last_offset_in_chunk: u8 = 0;
    let mut byte_pos: u64 = 0;
    let mut prev_id: i64 = -1;

    // Reserve space for chunk arrays.
    offsets.reserve(initial_chunks);
    start_pad.reserve(initial_chunks);

    // Node-only sequential scanner: bypasses PrimitiveBlock construction to avoid
    // cross-thread alloc/free retention (25+ GB at Europe/planet scale).
    // See notes/cross-pipeline-optimization-plan.md.
    let mut blob_reader = crate::blob::BlobReader::open(input, direct_io)?;
    blob_reader.set_parse_indexdata(true);
    blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

    let decompress_pool = crate::blob::DecompressPool::new();
    let mut tuples: Vec<super::node_scanner::NodeTuple> = Vec::new();

    let mut block_count: u64 = 0;
    #[cfg(feature = "debug-logging")]
    let mut stored_nodes: u64 = 0;
    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
            continue;
        }
        if let Some(idx) = blob.index() {
            if !matches!(idx.kind, crate::blob_index::ElemKind::Node) {
                continue;
            }
        }

        let decompressed = blob.decompress_pooled(&decompress_pool)?;
        tuples.clear();
        super::node_scanner::extract_node_tuples(&decompressed, &mut tuples)?;

        block_count += 1;
        for &super::node_scanner::NodeTuple { id, lat, lon } in &tuples {
            if !referenced.get(id) {
                continue;
            }

            if id < 0 {
                continue;
            }
            if id <= prev_id {
                return Err(format!(
                    "sparse index requires strictly increasing node IDs, \
                     but node {id} follows node {prev_id} (use --index-type dense \
                     for unsorted input)"
                )
                .into());
            }
            prev_id = id;
            let uid = id as u64;
            let chunk_id = (uid >> CHUNK_SHIFT) as usize;
            let offset_in_chunk = (uid & CHUNK_MASK) as u8;

            if chunk_id != current_chunk {
                // Close previous chunk: pad trailing slots with sentinels.
                if current_chunk != usize::MAX {
                    #[allow(clippy::cast_possible_truncation)]
                    let trailing = (CHUNK_MASK as u8).wrapping_sub(last_offset_in_chunk);
                    for _ in 0..trailing {
                        writer.write_all(&sentinel)?;
                        byte_pos += ENTRY_SIZE as u64;
                    }
                }
                // Ensure offsets/start_pad are large enough for this chunk.
                if chunk_id >= offsets.len() {
                    offsets.resize(chunk_id + 1, CHUNK_NOT_PRESENT);
                    start_pad.resize(chunk_id + 1, 0);
                }
                offsets[chunk_id] = byte_pos;
                start_pad[chunk_id] = offset_in_chunk;
                current_chunk = chunk_id;
                last_offset_in_chunk = offset_in_chunk;
            } else {
                // Fill gaps within the chunk with sentinels.
                let gap = offset_in_chunk.wrapping_sub(last_offset_in_chunk).wrapping_sub(1);
                for _ in 0..gap {
                    writer.write_all(&sentinel)?;
                    byte_pos += ENTRY_SIZE as u64;
                }
                last_offset_in_chunk = offset_in_chunk;
            }

            // Write the actual entry.
            let mut buf = [0u8; ENTRY_SIZE];
            buf[..4].copy_from_slice(&lat.to_le_bytes());
            buf[4..].copy_from_slice(&lon.to_le_bytes());
            #[cfg(feature = "debug-logging")]
            {
                stored_nodes += 1;
            }
            writer.write_all(&buf)?;
            byte_pos += ENTRY_SIZE as u64;
        }
        if block_count.is_multiple_of(1_000) {
            crate::debug_log!(
                "add-locations-to-ways: sparse index blocks={block_count} stored_nodes={stored_nodes} chunks={} {}",
                offsets.len(),
                crate::debug::rss_line(),
            );
        }
    }

    // Close final chunk: pad trailing slots.
    if current_chunk != usize::MAX {
        #[allow(clippy::cast_possible_truncation)]
        let trailing = (CHUNK_MASK as u8).wrapping_sub(last_offset_in_chunk);
        for _ in 0..trailing {
            writer.write_all(&sentinel)?;
        }
    }

    writer.flush()?;
    drop(writer);

    // Re-map as read-only for the lookup phase.
    let mmap = unsafe {
        memmap2::Mmap::map(&file)
            .map_err(|e| format!("failed to mmap sparse index values: {e}"))?
    };

    crate::debug_log!(
        "add-locations-to-ways: sparse index build done blocks={block_count} stored_nodes={stored_nodes} chunks={} values_bytes={} {}",
        offsets.len(),
        byte_pos,
        crate::debug::rss_line(),
    );
    Ok(SparseArrayIndex {
        offsets,
        start_pad,
        mmap,
        _file: file,
    })
}

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
/// The blob data is NOT read — call `read_blob_data` or `skip_blob_data` next.
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
    // External join has its own pipeline — dispatch early.
    if index_type == IndexType::External {
        return super::external_join::external_join(
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
    let scratch_dir = output.parent().unwrap_or(Path::new("."));

    // Pass 0: collect the set of node IDs referenced by ways. Only these
    // nodes need coordinate lookups, so only these get indexed. At planet
    // scale this reduces touched mmap pages from ~80 GB to ~16 GB.
    let referenced = collect_way_referenced_node_ids(input, direct_io)?;

    let index = build_node_index(input, direct_io, scratch_dir, &referenced, index_type)?;
    drop(referenced);

    let relation_member_node_ids = if keep_untagged_nodes {
        None
    } else {
        Some(collect_relation_member_node_ids(input, direct_io)?)
    };
    write_output_checked(
        input,
        output,
        &index,
        keep_untagged_nodes,
        relation_member_node_ids.as_ref(),
        compression,
        direct_io,
        indexdata_present,
        overrides,
    )
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
    referenced: &IdSetDense,
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
        IndexType::External => unreachable!("external dispatched before build_node_index"),
    }
}

/// Build the dense mmap index in parallel. Each rayon task writes directly
/// to disjoint mmap slots via `SharedDenseWriter`.
///
/// Only nodes present in `referenced` are inserted — at planet scale this
/// reduces touched pages from ~80 GB (all 10.4B nodes) to ~16 GB (~2B
/// way-referenced nodes).
fn build_node_index_dense(
    input: &Path,
    direct_io: bool,
    scratch_dir: &Path,
    referenced: &IdSetDense,
) -> Result<DenseMmapIndex> {
    crate::debug_log!(
        "add-locations-to-ways: dense index build start input={} scratch={} {}",
        input.display(),
        scratch_dir.display(),
        crate::debug::rss_line(),
    );
    let mut index = DenseMmapIndex::new(DENSE_INDEX_DEFAULT_CAPACITY, scratch_dir)?;
    let writer = SharedDenseWriter {
        base: index.mmap.as_mut_ptr(),
        capacity: index.capacity,
    };

    // Check for existing LocationsOnWays before consuming the reader.
    {
        let reader = ElementReader::from_path(input)?;
        if reader.header().has_locations_on_ways() {
            eprintln!(
                "Warning: input PBF already declares LocationsOnWays. \
                 Existing way-node coordinates will be overwritten."
            );
        }
    }

    // Node-only sequential scanner: bypasses PrimitiveBlock construction to avoid
    // cross-thread alloc/free retention (25+ GB at Europe/planet scale).
    // See notes/cross-pipeline-optimization-plan.md.
    let mut blob_reader = crate::blob::BlobReader::open(input, direct_io)?;
    blob_reader.set_parse_indexdata(true);
    blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

    let decompress_pool = crate::blob::DecompressPool::new();
    let mut tuples: Vec<super::node_scanner::NodeTuple> = Vec::new();
    let mut block_count: u64 = 0;

    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
            continue;
        }
        // Skip non-node blobs using indexdata.
        if let Some(idx) = blob.index() {
            if !matches!(idx.kind, crate::blob_index::ElemKind::Node) {
                continue;
            }
        }

        let decompressed = blob.decompress_pooled(&decompress_pool)?;
        tuples.clear();
        super::node_scanner::extract_node_tuples(&decompressed, &mut tuples)?;

        // Insert into mmap index. SharedDenseWriter is safe for concurrent access
        // (direct mmap slot writes to disjoint positions).
        tuples.par_iter().for_each(|t| {
            if referenced.get(t.id) {
                writer.insert(t.id, t.lat, t.lon);
            }
        });

        block_count += 1;
        if block_count.is_multiple_of(1_000) {
            crate::debug_log!(
                "add-locations-to-ways: dense index blocks={block_count} capacity={} {}",
                index.capacity,
                crate::debug::rss_line(),
            );
        }
    }

    crate::debug_log!(
        "add-locations-to-ways: dense index build done blocks={block_count} capacity={} {}",
        index.capacity,
        crate::debug::rss_line(),
    );
    Ok(index)
}

/// Collect all node IDs referenced by ways (pass 0).
///
/// Scans only way blobs (via `BlobFilter`) and builds a bitset of every node
/// ID that appears in any way's refs list. At planet scale (~2B unique node
/// refs), this costs ~1.6 GB — far less than indexing all 10.4B nodes.
fn collect_way_referenced_node_ids(input: &Path, direct_io: bool) -> Result<IdSetDense> {
    crate::debug_log!(
        "add-locations-to-ways: collect way-referenced node ids start input={} {}",
        input.display(),
        crate::debug::rss_line(),
    );
    let reader = ElementReader::open(input, direct_io)?
        .with_blob_filter(BlobFilter::only_ways());
    let mut referenced = IdSetDense::new();
    let mut block_count: u64 = 0;
    for block in reader.into_blocks_pipelined() {
        let block = block?;
        block_count += 1;
        for element in block.elements_skip_metadata() {
            if let Element::Way(w) = element {
                for node_id in w.refs() {
                    if node_id >= 0 {
                        referenced.set(node_id);
                    }
                }
            }
        }
        if block_count.is_multiple_of(1_000) {
            crate::debug_log!(
                "add-locations-to-ways: way-ref scan blocks={block_count} {}",
                crate::debug::rss_line(),
            );
        }
    }
    crate::debug_log!(
        "add-locations-to-ways: collect way-referenced node ids done blocks={block_count} {}",
        crate::debug::rss_line(),
    );
    Ok(referenced)
}

/// Collect all node IDs referenced by relation members.
pub(crate) fn collect_relation_member_node_ids(input: &Path, direct_io: bool) -> Result<IdSetDense> {
    crate::debug_log!(
        "add-locations-to-ways: collect relation member node ids start input={} {}",
        input.display(),
        crate::debug::rss_line(),
    );
    let reader = ElementReader::open(input, direct_io)?
        .with_blob_filter(BlobFilter::only_relations());
    let mut member_node_ids = IdSetDense::new();
    let mut block_count: u64 = 0;
    for block in reader.into_blocks_pipelined() {
        let block = block?;
        block_count += 1;
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
        if block_count.is_multiple_of(1_000) {
            crate::debug_log!(
                "add-locations-to-ways: relation-member scan blocks={block_count} {}",
                crate::debug::rss_line(),
            );
        }
    }
    crate::debug_log!(
        "add-locations-to-ways: collect relation member node ids done blocks={block_count} {}",
        crate::debug::rss_line(),
    );
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
    relation_member_node_ids: Option<&IdSetDense>,
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

#[allow(clippy::too_many_arguments)]
fn write_output_decode_all(
    input: &Path,
    output: &Path,
    index: &NodeIndex,
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
            merge_stats(&mut stats, &batch_stats);
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
        merge_stats(&mut stats, &batch_stats);
    }

    writer.flush()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Parallel batch processing
// ---------------------------------------------------------------------------

use super::{dense_node_metadata, element_metadata, flush_local};

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
#[allow(clippy::too_many_arguments)]
fn process_block(
    block: &PrimitiveBlock,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    lookup: &LocationLookup<'_>,
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

/// Process a batch of `PrimitiveBlock`s in parallel via rayon.
///
/// For sparse indexes: pre-resolves all way node coordinates via sorted
/// sequential scan before parallel processing (avoids random mmap I/O).
fn process_batch(
    batch: &[PrimitiveBlock],
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSetDense>,
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

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn write_output_passthrough(
    input: &Path,
    output: &Path,
    node_index: &NodeIndex,
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

    let mut reader = FileReader::open(input, direct_io)?;
    let mut file_offset: u64 = 0;
    let (header_bytes, _sorted) = read_header_raw(&mut reader, &mut file_offset, overrides)?;
    let mut writer = PbfWriter::to_path(output, compression, &header_bytes)?;

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
                merge_stats(&mut stats, &batch_stats);
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
            // Flush any pending copy range before decoding — the next passthrough
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
            merge_stats(&mut stats, &batch_stats);
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
        merge_stats(&mut stats, &batch_stats);
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
    relation_member_node_ids: Option<&IdSetDense>,
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
    drain_batch_results(results, writer, |s| merge_stats(&mut total, &s))?;
    Ok(total)
}

/// Dense-index path for slot batch: decompress + process in one parallel pass.
fn process_slot_batch_dense(
    batch: &[BatchSlot],
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    node_index: &NodeIndex,
    keep_untagged_nodes: bool,
    relation_member_node_ids: Option<&IdSetDense>,
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
    drain_batch_results(results, writer, |s| merge_stats(&mut total, &s))?;
    Ok(total)
}
