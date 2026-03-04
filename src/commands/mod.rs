pub mod add_locations_to_ways;
pub mod cat;
#[cfg(feature = "commands")]
pub mod check_refs;
pub mod derive_changes;
pub mod diff;
#[cfg(feature = "commands")]
pub mod extract;
pub mod getid;
pub mod inspect;
pub(crate) mod id_set_dense;
pub mod merge;
pub mod node_stats;
pub(crate) mod owned_elements;
pub mod sort;
pub(crate) mod stream_merge;
pub mod tags_count;
pub mod tags_filter;
#[cfg(feature = "commands")]
pub mod verify_ids;

use std::io::Read;
use std::path::Path;

use crate::blob::{parse_blob_header_with_index, BlobKind};
use crate::blob_index::BlobIndex;
use crate::block_builder::{BlockBuilder, HeaderBuilder, Metadata, OwnedBlock, RawMetadata};
use crate::file_reader::FileReader;
use crate::file_writer::FileWriter;
use crate::writer::{Compression, PbfWriter};
use crate::PrimitiveBlock;

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

/// Consume `PrimitiveBlock` results in fixed-size batches.
///
/// Each incoming block result is propagated with `?`; successful blocks are
/// accumulated into a reusable `Vec` and passed to `process_batch` when full,
/// then once more for the final partial batch.
pub(crate) fn for_each_primitive_block_batch<E>(
    blocks: impl IntoIterator<Item = std::result::Result<PrimitiveBlock, E>>,
    batch_size: usize,
    mut process_batch: impl FnMut(&[PrimitiveBlock]) -> Result<()>,
) -> Result<()>
where
    E: Into<Box<dyn std::error::Error>>,
{
    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(batch_size);
    for block in blocks {
        batch.push(block.map_err(Into::into)?);
        if batch.len() >= batch_size {
            process_batch(&batch)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        process_batch(&batch)?;
    }
    Ok(())
}

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

/// Parsed blob header without the blob data payload.
///
/// Used by the index-only inspect path. The caller must either read
/// or skip `data_size` bytes from the reader after receiving this.
pub(crate) struct BlobHeaderInfo {
    pub blob_type: BlobKind,
    pub data_size: usize,
    pub index: Option<BlobIndex>,
    /// Total frame size: 4 + header_len + data_size.
    pub frame_size: usize,
}

/// Read the next blob header without reading the blob data payload.
///
/// Returns `None` at EOF. Updates `file_offset` past the header only.
/// The caller must either:
/// - Read `data_size` bytes (e.g. for OsmHeader blobs)
/// - Skip `data_size` bytes via `reader.skip()` (for index-only mode)
///
/// and then advance `*file_offset += data_size as u64`.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn read_blob_header_only(
    reader: &mut FileReader,
    file_offset: &mut u64,
) -> Result<Option<BlobHeaderInfo>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let header_len = u32::from_be_bytes(len_buf) as usize;

    let mut header_bytes = vec![0u8; header_len];
    reader.read_exact(&mut header_bytes)?;

    let (blob_type, data_size, raw_index, _tagdata) =
        parse_blob_header_with_index(&header_bytes)?;
    let index = raw_index.and_then(|ref data| BlobIndex::deserialize(data));

    *file_offset += (4 + header_len) as u64;
    let frame_size = 4 + header_len + data_size;

    Ok(Some(BlobHeaderInfo {
        blob_type,
        data_size,
        index,
        frame_size,
    }))
}

/// Flush coalesced passthrough chunks as a single `write_raw_chunks` (move, no copy).
pub(crate) fn flush_passthrough_buf(
    chunks: &mut Vec<Vec<u8>>,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    if !chunks.is_empty() {
        writer.write_raw_chunks(std::mem::take(chunks))?;
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

/// Ensure the [`BlockBuilder`] has capacity for a node, flushing to the writer
/// if full. Used by sequential output paths (merge, sort).
pub(crate) fn ensure_node_capacity(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    if !bb.can_add_node() {
        flush_block(bb, writer)?;
    }
    Ok(())
}

/// Ensure the [`BlockBuilder`] has capacity for a way, flushing to the writer
/// if full.
pub(crate) fn ensure_way_capacity(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    if !bb.can_add_way() {
        flush_block(bb, writer)?;
    }
    Ok(())
}

/// Ensure the [`BlockBuilder`] has capacity for a relation, flushing to the
/// writer if full.
pub(crate) fn ensure_relation_capacity(
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    if !bb.can_add_relation() {
        flush_block(bb, writer)?;
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

/// Ensure the [`BlockBuilder`] has capacity for a node, flushing to local
/// output if full. Used by rayon worker threads in parallel batch processing.
pub(crate) fn ensure_node_capacity_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> std::result::Result<(), String> {
    if !bb.can_add_node() {
        flush_local(bb, output)?;
    }
    Ok(())
}

/// Ensure the [`BlockBuilder`] has capacity for a way, flushing to local
/// output if full.
pub(crate) fn ensure_way_capacity_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> std::result::Result<(), String> {
    if !bb.can_add_way() {
        flush_local(bb, output)?;
    }
    Ok(())
}

/// Ensure the [`BlockBuilder`] has capacity for a relation, flushing to local
/// output if full.
pub(crate) fn ensure_relation_capacity_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> std::result::Result<(), String> {
    if !bb.can_add_relation() {
        flush_local(bb, output)?;
    }
    Ok(())
}

/// Warn if the input header declares `LocationsOnWays` — inline way-node
/// coordinates are not propagated through re-encoding.
pub(crate) fn warn_locations_on_ways_loss(header: &crate::HeaderBlock) {
    if header.has_locations_on_ways() {
        eprintln!(
            "Warning: input PBF has LocationsOnWays (inline way-node coordinates). \
             These will not be preserved in the output."
        );
    }
}

/// Build output header bytes from an input header.
///
/// Applies `configure` to the header builder, then preserves sortedness if
/// requested and if the input header is sorted.
pub(crate) fn build_output_header(
    header: &crate::HeaderBlock,
    preserve_sorted: bool,
    configure: impl FnOnce(HeaderBuilder) -> HeaderBuilder,
) -> Result<Vec<u8>> {
    let mut hb = configure(HeaderBuilder::from_header(header));
    if preserve_sorted && header.is_sorted() {
        hb = hb.sorted();
    }
    Ok(hb.build()?)
}

/// Open a pipelined writer from an input header.
pub(crate) fn writer_from_header(
    output: &Path,
    compression: Compression,
    header: &crate::HeaderBlock,
    preserve_sorted: bool,
    configure: impl FnOnce(HeaderBuilder) -> HeaderBuilder,
) -> Result<PbfWriter<FileWriter>> {
    let header_bytes = build_output_header(header, preserve_sorted, configure)?;
    Ok(PbfWriter::to_path(output, compression, &header_bytes)?)
}

/// Open an output writer from prebuilt header bytes with optional direct-io/io_uring modes.
pub(crate) fn writer_from_header_bytes(
    output: &Path,
    compression: Compression,
    header_bytes: &[u8],
    direct_io: bool,
    io_uring: bool,
) -> Result<PbfWriter<FileWriter>> {
    if io_uring {
        #[cfg(feature = "linux-io-uring")]
        {
            Ok(PbfWriter::to_path_uring(output, compression, header_bytes)?)
        }
        #[cfg(not(feature = "linux-io-uring"))]
        {
            Err("--io-uring requires the linux-io-uring feature".into())
        }
    } else if direct_io {
        #[cfg(feature = "linux-direct-io")]
        {
            Ok(PbfWriter::to_path_direct(output, compression, header_bytes)?)
        }
        #[cfg(not(feature = "linux-direct-io"))]
        {
            Err("--direct-io requires the linux-direct-io feature".into())
        }
    } else {
        Ok(PbfWriter::to_path(output, compression, header_bytes)?)
    }
}

/// Map Osmosis sentinel -1 to 0 (protobuf default for absent) in dense node
/// fields where the type is plain `i64` rather than `Option`.
#[inline]
fn map_sentinel(value: i64) -> i64 {
    if value == -1 { 0 } else { value }
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
    dn.info()
        .filter(|info| info.version() != -1)
        .map(|info| Metadata {
            version: info.version(),
            timestamp: info.milli_timestamp() / 1000,
            changeset: map_sentinel(info.changeset()),
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
    dn.info()
        .filter(|info| info.version() != -1)
        .map(|info| RawMetadata {
            version: info.version(),
            timestamp: info.milli_timestamp() / 1000,
            changeset: map_sentinel(info.changeset()),
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

/// Check that a PBF file declares `Sort.Type_then_ID`.
///
/// Returns an error with actionable guidance if the header lacks the sorted flag.
/// `context` should identify the file role (e.g. "Old PBF" or "New PBF").
pub(crate) fn require_sorted(
    header: &crate::HeaderBlock,
    path: &Path,
    context: &str,
) -> Result<()> {
    if !header.is_sorted() {
        return Err(format!(
            "{context} is not sorted (missing Sort.Type_then_ID optional feature).\n\
             File: {}\n\n\
             Sort the input file first:\n\n\
             \x20 pbfhogg sort {} -o sorted.osm.pbf\n\n\
             Streaming diff requires sorted inputs to operate in constant memory.",
            path.display(),
            path.display(),
        )
        .into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// OSM ID ordering — canonical sort order matching libosmium
// ---------------------------------------------------------------------------

/// Sort key for OSM element IDs in canonical order.
///
/// Order: 0, then negative IDs by ascending absolute value (-1, -2, -3, ...),
/// then positive IDs (1, 2, 3, ...). Matches libosmium's sort comparator.
///
/// For positive-only IDs (all production PBFs), this is equivalent to plain
/// i64 comparison — the `(2, id)` tuple compares identically to raw `id`.
#[inline]
pub(crate) fn osm_id_key(id: i64) -> (u8, i64) {
    if id > 0 {
        (2, id)
    } else if id == 0 {
        (0, 0)
    } else {
        (1, id.saturating_neg())
    }
}

/// Compare two OSM element IDs in canonical sort order.
#[inline]
pub(crate) fn osm_id_cmp(a: i64, b: i64) -> std::cmp::Ordering {
    osm_id_key(a).cmp(&osm_id_key(b))
}

/// OSM-order "first" key for a blob's numeric ID range.
///
/// Used by blob-level sort to determine blob ordering. Conservative for
/// mixed-sign ranges (assumes 0 is present).
#[inline]
pub(crate) fn blob_osm_first_key(min_id: i64, max_id: i64) -> (u8, i64) {
    if min_id >= 0 {
        osm_id_key(min_id)
    } else if max_id <= 0 {
        osm_id_key(max_id)
    } else {
        osm_id_key(0)
    }
}

/// OSM-order "last" key for a blob's numeric ID range.
///
/// Used by blob-level overlap detection.
#[inline]
pub(crate) fn blob_osm_last_key(min_id: i64, max_id: i64) -> (u8, i64) {
    if min_id >= 0 {
        osm_id_key(max_id)
    } else if max_id <= 0 {
        osm_id_key(min_id)
    } else {
        osm_id_key(max_id)
    }
}

/// The ID of the "first" element of a blob in OSM sort order.
///
/// For positive-only blobs, this is min_id. For negative-only blobs,
/// this is max_id (closest to 0). For mixed blobs, conservatively 0.
#[inline]
pub(crate) fn blob_osm_first_id(min_id: i64, max_id: i64) -> i64 {
    if min_id >= 0 {
        min_id
    } else if max_id <= 0 {
        max_id
    } else {
        0
    }
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
