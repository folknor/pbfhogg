//! Sparse chunk-indexed node coordinate store (Planetiler-inspired).
//!
//! Partitions the node ID space into chunks of 256 IDs. Each chunk stores
//! a contiguous run of `(lat: i32, lon: i32)` entries in a file-backed mmap,
//! with leading empty slots trimmed via `start_pad` and gaps filled with
//! sentinel `(0, 0)` values. RAM cost is `offsets` (8 bytes/chunk) +
//! `start_pad` (1 byte/chunk); at planet scale that's ~440 MB RAM plus
//! ~16 GB on disk.
//!
//! Requires sequential writes in ascending node ID order (satisfied by
//! sorted PBF files).

use std::io::{BufWriter, Write as _};
use std::path::Path;

use crate::idset::IdSet;

use super::ENTRY_SIZE;
use super::Result;

/// Bits to shift a node ID right to get the chunk index.
const CHUNK_SHIFT: u32 = 8;
/// Number of entries per chunk (256).
const CHUNK_MASK: u64 = (1u64 << CHUNK_SHIFT) - 1;
/// Marker for chunks with no entries.
const CHUNK_NOT_PRESENT: u64 = u64::MAX;

/// Chunk-indexed sparse node coordinate store.
pub(super) struct SparseArrayIndex {
    /// Byte offset into the values mmap where each chunk starts.
    offsets: Vec<u64>,
    /// Leading empty slots skipped per chunk.
    start_pad: Vec<u8>,
    /// Packed (lat: i32, lon: i32) values, file-backed read-only mmap.
    mmap: memmap2::Mmap,
    _file: std::fs::File,
}

impl SparseArrayIndex {
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
    pub(super) fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        let (base, slot) = self.resolve(node_id)?;
        self.read_at(base, slot)
    }

    /// Compute the byte offset into the values mmap for a node ID.
    /// Used by batched sorted lookups to sort by file position.
    #[allow(clippy::cast_sign_loss)]
    pub(super) fn byte_offset(&self, node_id: i64) -> Option<u64> {
        let (base, slot) = self.resolve(node_id)?;
        Some(base + slot * ENTRY_SIZE as u64)
    }

    /// Read a `(lat, lon)` pair at a known valid byte offset.
    /// The offset must have been produced by `byte_offset()`.
    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn get_at_offset(&self, byte_offset: u64) -> Option<(i32, i32)> {
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
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::cast_sign_loss, clippy::too_many_lines)]
pub(super) fn build_node_index_sparse(
    input: &Path,
    direct_io: bool,
    scratch_dir: &Path,
    referenced: &IdSet,
) -> Result<SparseArrayIndex> {
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

    // Grow on demand - avoids 450 MB upfront allocation for small datasets.

    // Node-only sequential scanner: bypasses PrimitiveBlock construction to avoid
    // cross-thread alloc/free retention (25+ GB at Europe/planet scale).
    // See notes/cross-pipeline-optimization-plan.md.
    let mut blob_reader = crate::blob::BlobReader::open(input, direct_io)?;
    blob_reader.set_parse_indexdata(true);
    blob_reader.next()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;

    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut tuples: Vec<crate::scan::node::NodeTuple> = Vec::new();
    let mut group_starts: Vec<(usize, usize)> = Vec::new();

    for blob_result in &mut blob_reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
            continue;
        }
        if let Some(idx) = blob.index() {
            if !matches!(idx.kind, crate::blob_meta::ElemKind::Node) {
                continue;
            }
        }

        blob.decompress_into(&mut decompress_buf)?;
        tuples.clear();
        crate::scan::node::extract_node_tuples(&decompress_buf, &mut tuples, &mut group_starts)?;

        for &crate::scan::node::NodeTuple { id, lat, lon } in &tuples {
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
            writer.write_all(&buf)?;
            byte_pos += ENTRY_SIZE as u64;
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

    Ok(SparseArrayIndex {
        offsets,
        start_pad,
        mmap,
        _file: file,
    })
}
