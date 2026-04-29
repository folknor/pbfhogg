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
}

/// Per-blob worker output: a contiguous, ascending list of node tuples
/// referenced by ways. Filtering against the `referenced` IdSet happens in
/// the worker so the consumer only handles work that contributes to the
/// index.
type WorkerNodes = Vec<(i64, i32, i32)>;

/// Sparse-index consumer state. The fields collectively express the
/// chunk-streaming invariant (one chunk in flight, prior chunks closed
/// via trailing-sentinel padding) plus the global byte cursor and
/// monotonicity check.
struct SparseConsumer<'a> {
    offsets: &'a mut Vec<u64>,
    start_pad: &'a mut Vec<u8>,
    current_chunk: &'a mut usize,
    last_offset_in_chunk: &'a mut u8,
    byte_pos: &'a mut u64,
    prev_id: &'a mut i64,
}

#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn append_node<W: std::io::Write>(
    state: &mut SparseConsumer<'_>,
    writer: &mut W,
    sentinel: &[u8; ENTRY_SIZE],
    id: i64,
    lat: i32,
    lon: i32,
) -> Result<()> {
    if id <= *state.prev_id {
        return Err(format!(
            "sparse index requires strictly increasing node IDs, \
             but node {id} follows node {prev} (use --index-type dense \
             for unsorted input)",
            prev = *state.prev_id,
        )
        .into());
    }
    *state.prev_id = id;
    let uid = id as u64;
    let chunk_id = (uid >> CHUNK_SHIFT) as usize;
    let offset_in_chunk = (uid & CHUNK_MASK) as u8;

    if chunk_id != *state.current_chunk {
        // Close previous chunk: pad trailing slots with sentinels.
        if *state.current_chunk != usize::MAX {
            let trailing = (CHUNK_MASK as u8).wrapping_sub(*state.last_offset_in_chunk);
            for _ in 0..trailing {
                writer.write_all(sentinel)?;
                *state.byte_pos += ENTRY_SIZE as u64;
            }
        }
        if chunk_id >= state.offsets.len() {
            state.offsets.resize(chunk_id + 1, CHUNK_NOT_PRESENT);
            state.start_pad.resize(chunk_id + 1, 0);
        }
        state.offsets[chunk_id] = *state.byte_pos;
        state.start_pad[chunk_id] = offset_in_chunk;
        *state.current_chunk = chunk_id;
        *state.last_offset_in_chunk = offset_in_chunk;
    } else {
        let gap = offset_in_chunk
            .wrapping_sub(*state.last_offset_in_chunk)
            .wrapping_sub(1);
        for _ in 0..gap {
            writer.write_all(sentinel)?;
            *state.byte_pos += ENTRY_SIZE as u64;
        }
        *state.last_offset_in_chunk = offset_in_chunk;
    }

    let mut buf = [0u8; ENTRY_SIZE];
    buf[..4].copy_from_slice(&lat.to_le_bytes());
    buf[4..].copy_from_slice(&lon.to_le_bytes());
    writer.write_all(&buf)?;
    *state.byte_pos += ENTRY_SIZE as u64;
    Ok(())
}

/// Build a sparse array index from node blobs.
///
/// Writes values sequentially to a temp file, tracking chunk boundaries.
/// Nodes must arrive in ascending ID order (guaranteed by sorted PBFs).
/// Only nodes present in `referenced` are stored.
///
/// Pipeline shape: parallel decode through `parallel_classify_phase`
/// (one PrimitiveBlock per blob, decompressed off the consumer thread)
/// fed into a `ReorderBuffer` so blob outputs drain in file order.
/// Each worker emits a `Vec<(id, lat, lon)>` of referenced-and-positive
/// tuples for its blob; the consumer runs the chunk-streaming state
/// machine against the drained sequence. The `direct_io` flag is
/// intentionally dropped on this path: blob bodies are pread'd from
/// a shared file handle on worker threads, incompatible with
/// `O_DIRECT` alignment.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::cast_sign_loss)]
pub(super) fn build_node_index_sparse(
    input: &Path,
    _direct_io: bool,
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
    let mut current_chunk: usize = usize::MAX;
    let mut last_offset_in_chunk: u8 = 0;
    let mut byte_pos: u64 = 0;
    let mut prev_id: i64 = -1;

    let (schedule, shared_file) = crate::scan::classify::build_classify_schedule(
        input,
        Some(crate::blob_meta::ElemKind::Node),
    )?;

    let mut reorder: crate::reorder_buffer::ReorderBuffer<WorkerNodes> =
        crate::reorder_buffer::ReorderBuffer::with_capacity(64);
    let mut consumer_error: Option<Box<dyn std::error::Error>> = None;

    crate::scan::classify::parallel_classify_phase(
        &shared_file,
        &schedule,
        None,
        || (),
        |block, _state| -> WorkerNodes {
            let mut tuples: WorkerNodes = Vec::new();
            for element in block.elements_skip_metadata() {
                let (id, lat, lon) = match element {
                    crate::Element::DenseNode(dn) => {
                        (dn.id(), dn.decimicro_lat(), dn.decimicro_lon())
                    }
                    crate::Element::Node(n) => {
                        (n.id(), n.decimicro_lat(), n.decimicro_lon())
                    }
                    _ => continue,
                };
                if id < 0 || !referenced.get(id) {
                    continue;
                }
                tuples.push((id, lat, lon));
            }
            tuples
        },
        |seq, tuples| {
            reorder.push(seq, tuples);
            while let Some(blob_tuples) = reorder.pop_ready() {
                if consumer_error.is_some() {
                    continue;
                }
                let mut state = SparseConsumer {
                    offsets: &mut offsets,
                    start_pad: &mut start_pad,
                    current_chunk: &mut current_chunk,
                    last_offset_in_chunk: &mut last_offset_in_chunk,
                    byte_pos: &mut byte_pos,
                    prev_id: &mut prev_id,
                };
                for (id, lat, lon) in blob_tuples {
                    if let Err(e) = append_node(&mut state, &mut writer, &sentinel, id, lat, lon) {
                        consumer_error = Some(e);
                        break;
                    }
                }
            }
        },
    )?;

    if let Some(e) = consumer_error {
        return Err(e);
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
