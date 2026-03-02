pub mod add_locations_to_ways;
pub mod cat;
#[cfg(feature = "commands")]
pub mod check_refs;
pub mod derive_changes;
pub mod diff;
#[cfg(feature = "commands")]
pub mod extract;
pub mod fileinfo;
pub mod getid;
pub mod inspect;
pub(crate) mod id_set_dense;
pub mod merge;
pub mod node_stats;
pub(crate) mod owned_elements;
pub mod sort;
pub mod tags_count;
pub mod tags_filter;

use std::io::Read;
use std::path::Path;

use crate::blob::{parse_blob_header_with_index, BlobKind};
use crate::blob_index::BlobIndex;
use crate::block_builder::{BlockBuilder, Metadata, OwnedBlock, RawMetadata};
use crate::file_reader::FileReader;
use crate::file_writer::FileWriter;
use crate::writer::PbfWriter;

// Box<dyn Error> is intentional — commands are CLI internals, callers only display
// errors and exit. Typed error enums would add complexity with no matching benefit.
pub(crate) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Number of decoded `PrimitiveBlock`s collected before dispatching to rayon.
pub(crate) const BATCH_SIZE: usize = 64;

/// Maximum bytes of raw blob data in a single merge/rewrite batch.
pub(crate) const BATCH_BYTE_BUDGET: usize = 128 * 1024 * 1024;

/// Minimum blobs per batch (avoids rayon overhead on tiny batches).
pub(crate) const BATCH_MIN_BLOBS: usize = 8;

/// Maximum blobs per batch (bounds per-batch memory).
pub(crate) const BATCH_MAX_BLOBS: usize = 128;

// ---------------------------------------------------------------------------
// Element type filter
// ---------------------------------------------------------------------------

/// Boolean filter for which element types to include.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TypeFilter {
    pub(crate) nodes: bool,
    pub(crate) ways: bool,
    pub(crate) relations: bool,
}

impl TypeFilter {
    /// All types included.
    pub(crate) fn all() -> Self {
        Self { nodes: true, ways: true, relations: true }
    }

    /// Parse a comma-separated type list (e.g. "node,way,relation").
    pub(crate) fn parse(s: &str) -> Self {
        Self {
            nodes: s.split(',').any(|t| t.trim() == "node"),
            ways: s.split(',').any(|t| t.trim() == "way"),
            relations: s.split(',').any(|t| t.trim() == "relation"),
        }
    }

    /// Single type filter, or all types if `None`.
    pub(crate) fn from_single(s: Option<&str>) -> Self {
        match s {
            None => Self::all(),
            Some("node") => Self { nodes: true, ways: false, relations: false },
            Some("way") => Self { nodes: false, ways: true, relations: false },
            Some("relation") => Self { nodes: false, ways: false, relations: true },
            Some(_) => Self { nodes: false, ways: false, relations: false },
        }
    }
}

// ---------------------------------------------------------------------------
// Shared raw blob frame reading (used by merge and add-locations-to-ways)
// ---------------------------------------------------------------------------

/// A raw blob frame for passthrough or selective decode.
///
/// The Blob protobuf bytes are a suffix of `frame_bytes` starting at
/// `blob_offset`, eliminating a separate allocation per blob.
pub(crate) struct RawBlobFrame {
    /// Complete framed bytes: `[4-byte header_len][BlobHeader][Blob]`.
    pub(crate) frame_bytes: Vec<u8>,
    pub(crate) blob_type: BlobKind,
    /// Byte offset within `frame_bytes` where the Blob protobuf starts.
    pub(crate) blob_offset: usize,
    /// Blob-level index from BlobHeader indexdata, if present.
    pub(crate) index: Option<BlobIndex>,
    /// Per-blob tag key data from BlobHeader field 4, if present.
    pub(crate) tagdata: Option<Box<[u8]>>,
    /// Byte offset of this frame in the input file (for copy_file_range).
    #[cfg_attr(not(feature = "linux-direct-io"), allow(dead_code))]
    pub(crate) file_offset: u64,
}

impl RawBlobFrame {
    /// The raw Blob protobuf message bytes (for selective decoding).
    pub(crate) fn blob_bytes(&self) -> &[u8] {
        &self.frame_bytes[self.blob_offset..]
    }
}

/// Read the next raw blob frame. Returns `None` at EOF.
/// Updates `file_offset` to track position for copy_file_range.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn read_raw_frame<R: Read>(
    reader: &mut R,
    file_offset: &mut u64,
) -> Result<Option<RawBlobFrame>> {
    let frame_start = *file_offset;

    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let header_len = u32::from_be_bytes(len_buf) as usize;

    let mut header_bytes = vec![0u8; header_len];
    reader.read_exact(&mut header_bytes)?;

    let (blob_type, data_size, raw_index, tagdata) =
        parse_blob_header_with_index(&header_bytes)?;
    let index = raw_index.and_then(|ref data| BlobIndex::deserialize(data));

    let blob_offset = 4 + header_len;
    let frame_len = blob_offset + data_size;
    *file_offset += frame_len as u64;
    let mut frame_bytes = vec![0u8; frame_len];
    frame_bytes[..4].copy_from_slice(&len_buf);
    frame_bytes[4..blob_offset].copy_from_slice(&header_bytes);
    reader.read_exact(&mut frame_bytes[blob_offset..])?;

    Ok(Some(RawBlobFrame {
        frame_bytes,
        blob_type,
        blob_offset,
        index,
        tagdata,
        file_offset: frame_start,
    }))
}

/// Flush coalesced passthrough bytes as a single `write_raw_owned` (move, no copy).
pub(crate) fn flush_passthrough_buf(
    buf: &mut Vec<u8>,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    if !buf.is_empty() {
        writer.write_raw_owned(std::mem::take(buf))?;
    }
    Ok(())
}

/// Flush the current block from a [`BlockBuilder`] into a [`PbfWriter`].
///
/// If the builder has accumulated elements, `take_owned()` serializes them
/// into a protobuf `PrimitiveBlock` and the owned bytes are moved into the
/// writer (no `to_vec()` copy in pipelined mode). If the builder is empty,
/// this is a no-op.
pub(crate) fn flush_block(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    if let Some((bytes, index, tagdata)) = bb.take_owned()? {
        writer.write_primitive_block_owned(bytes, index, tagdata.as_deref())?;
    }
    Ok(())
}

/// Drain parallel batch results: write blocks to the writer, merge stats via closure.
///
/// Each result is `(Vec<OwnedBlock>, S)` where `S` is a per-block stats type.
/// Blocks are written sequentially in batch order. The `merge` closure
/// accumulates stats from each result into the caller's aggregator.
pub(crate) fn drain_batch_results<S>(
    results: Vec<std::result::Result<(Vec<OwnedBlock>, S), String>>,
    writer: &mut PbfWriter<FileWriter>,
    mut merge: impl FnMut(S),
) -> Result<()> {
    for result in results {
        let (blocks, stats) = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        merge(stats);
        for (block_bytes, index, tagdata) in blocks {
            writer.write_primitive_block_owned(block_bytes, index, tagdata.as_deref())?;
        }
    }
    Ok(())
}

/// Flush the current block from a [`BlockBuilder`] into a local output buffer.
///
/// Like `flush_block` but writes to a `Vec<OwnedBlock>` instead of a
/// `PbfWriter`, so it can be called from rayon worker threads.
pub(crate) fn flush_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> std::result::Result<(), String> {
    if let Some(triple) = bb.take_owned().map_err(|e| e.to_string())? {
        output.push(triple);
    }
    Ok(())
}

/// Extract [`Metadata`] from an [`Info`](crate::Info) (Node/Way/Relation).
///
/// Returns `None` if the info block has no version. On `user()` error (string
/// table corruption), defaults to empty string.
pub(crate) fn element_metadata<'a>(info: &crate::Info<'a>) -> Option<Metadata<'a>> {
    info.version().map(|v| Metadata {
        version: v,
        timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
        changeset: info.changeset().unwrap_or(0),
        uid: info.uid().unwrap_or(0),
        user: info.user().and_then(std::result::Result::ok).unwrap_or(""),
        visible: info.visible(),
    })
}

/// Extract [`Metadata`] from a [`DenseNode`](crate::DenseNode).
///
/// Returns `None` if the node has no info block. On `user()` error (string
/// table corruption), defaults to empty string — consistent with the
/// Node/Way/Relation path.
pub(crate) fn dense_node_metadata<'a>(dn: &'a crate::DenseNode<'a>) -> Option<Metadata<'a>> {
    dn.info().map(|info| Metadata {
        version: info.version(),
        timestamp: info.milli_timestamp() / 1000,
        changeset: info.changeset(),
        uid: info.uid(),
        user: info.user().unwrap_or(""),
        visible: info.visible(),
    })
}

/// Extract [`RawMetadata`] from an [`Info`](crate::Info), preserving the raw
/// string table index for the user name.
pub(crate) fn element_raw_metadata(info: &crate::Info<'_>) -> Option<RawMetadata> {
    info.version().map(|v| RawMetadata {
        version: v,
        timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
        changeset: info.changeset().unwrap_or(0),
        uid: info.uid().unwrap_or(0),
        user_sid: info.raw_user_sid().unwrap_or(0),
        visible: info.visible(),
    })
}

/// Extract [`RawMetadata`] from a [`DenseNode`](crate::DenseNode), preserving
/// the raw string table index for the user name.
pub(crate) fn dense_node_raw_metadata(dn: &crate::DenseNode<'_>) -> Option<RawMetadata> {
    dn.info().map(|info| RawMetadata {
        version: info.version(),
        timestamp: info.milli_timestamp() / 1000,
        changeset: info.changeset(),
        uid: info.uid(),
        user_sid: info.raw_user_sid(),
        visible: info.visible(),
    })
}

/// Check for indexdata and return an error if missing (unless `force` is set).
///
/// Returns `true` if indexdata is present, `false` if absent but `force` is set.
/// The `reason` should be a complete sentence explaining why indexdata matters,
/// e.g. "input PBF has no blob-level indexdata. Without indexdata, the type
/// filter is a no-op — all blobs are decompressed (significantly slower)."
pub(crate) fn require_indexdata(
    path: &Path,
    direct_io: bool,
    force: bool,
    reason: &str,
) -> Result<bool> {
    let present = has_indexdata(path, direct_io)?;
    if !force && !present {
        return Err(format!(
            "{reason}\n\n\
             Generate an indexed PBF first:\n\n\
             \x20 pbfhogg cat input.osm.pbf --type node,way,relation -o indexed.osm.pbf\n\n\
             Or pass --force to proceed anyway."
        )
        .into());
    }
    Ok(present)
}

/// Check if the first OsmData blob in a PBF has indexdata.
pub fn has_indexdata(path: &Path, direct_io: bool) -> Result<bool> {
    let mut reader = FileReader::open(path, direct_io)?;
    let mut offset = 0u64;
    while let Some(frame) = read_raw_frame(&mut reader, &mut offset)? {
        match frame.blob_type {
            BlobKind::OsmHeader => continue,
            BlobKind::OsmData => return Ok(frame.index.is_some()),
            BlobKind::Unknown(_) => continue,
        }
    }
    Ok(false)
}
