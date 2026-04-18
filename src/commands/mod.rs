pub mod add_locations_to_ways;
pub mod cat;
pub(crate) mod altw;
pub(crate) mod external_radix;
#[cfg(feature = "commands")]
pub mod check_refs;
pub mod derive_changes;
pub mod diff;
#[cfg(feature = "commands")]
pub mod extract;
pub mod getid;
pub mod getparents;
pub mod inspect;
pub(crate) mod id_set_dense;
pub(crate) mod node_scanner;
pub(crate) mod way_scanner;
pub mod merge;
pub mod merge_changes;
pub mod merge_pbf;
pub mod node_stats;
pub mod renumber;
pub mod renumber_external;
pub(crate) mod elements_pbf;
pub(crate) mod elements_xml;
pub mod sort;
pub(crate) mod stream_merge;
pub mod tag_expr;
pub mod tags_count;
pub mod tags_filter;
pub mod tags_filter_osc;
pub mod time_filter;
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

// Box<dyn Error> is intentional - commands are CLI internals, callers only display
// errors and exit. Typed error enums would add complexity with no matching benefit.
pub(crate) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Number of decoded `PrimitiveBlock`s collected before dispatching to rayon.
pub(crate) const BATCH_SIZE: usize = 64;

/// Maximum bytes of raw blob data in a single merge/rewrite batch.
pub(crate) const BATCH_BYTE_BUDGET: usize = 128 * 1024 * 1024;

/// Maximum decompressed bytes per decode-process batch (e.g. `cat --type`).
///
/// Lower than `BATCH_BYTE_BUDGET` because decoded blocks are fully expanded in
/// memory and then cloned into rayon compression tasks. At ~1.4 MiB/block this
/// targets ~23 blocks/batch, well below the 64-block count cap.
pub(crate) const DECODE_BATCH_BYTE_BUDGET: usize = 32 * 1024 * 1024;

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
    for_each_primitive_block_batch_budgeted(blocks, batch_size, None, &mut process_batch)
}

/// Consume `PrimitiveBlock` results in batches bounded by both block count and
/// decompressed byte budget.
///
/// A batch is flushed when either `max_blocks` blocks have been collected or
/// the cumulative decompressed payload exceeds `max_bytes`. This prevents
/// unbounded memory growth when blocks are large (e.g. planet-scale `cat --type`).
///
/// When `max_bytes` is `None`, behaves identically to count-only batching.
pub(crate) fn for_each_primitive_block_batch_budgeted<E>(
    blocks: impl IntoIterator<Item = std::result::Result<PrimitiveBlock, E>>,
    max_blocks: usize,
    max_bytes: Option<usize>,
    process_batch: &mut dyn FnMut(&[PrimitiveBlock]) -> Result<()>,
) -> Result<()>
where
    E: Into<Box<dyn std::error::Error>>,
{
    let mut batch: Vec<PrimitiveBlock> = Vec::with_capacity(max_blocks);
    let mut batch_bytes: usize = 0;
    for block in blocks {
        let block = block.map_err(Into::into)?;
        batch_bytes += block.decompressed_size();
        batch.push(block);
        let over_byte_budget = max_bytes.is_some_and(|limit| batch_bytes >= limit);
        if batch.len() >= max_blocks || over_byte_budget {
            process_batch(&batch)?;
            batch.clear();
            batch_bytes = 0;
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

// ---------------------------------------------------------------------------
// Schedule building + parallel classification
// ---------------------------------------------------------------------------

/// Build a classification schedule from a header-only scan, optionally
/// filtering by element type. Returns `(schedule, shared_file)` ready for
/// [`parallel_classify_phase`].
///
/// Each schedule entry is `(seq, data_offset, data_size)`. Only OsmData blobs
/// are included. When `kind_filter` is `Some`, only blobs whose indexdata
/// matches the given element type (plus blobs without indexdata) are included.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn build_classify_schedule(
    input: &std::path::Path,
    kind_filter: Option<crate::blob_index::ElemKind>,
) -> Result<(Vec<(usize, u64, usize)>, std::sync::Arc<std::fs::File>)> {
    crate::debug::emit_marker("SCHEDULE_SCANNER_OPEN_START");
    let mut scanner = crate::blob::BlobReader::seekable_from_path(input)?;
    scanner.set_parse_indexdata(true);
    scanner.next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    crate::debug::emit_marker("SCHEDULE_SCANNER_OPEN_END");

    crate::debug::emit_marker("SCHEDULE_SCAN_LOOP_START");
    let mut schedule: Vec<(usize, u64, usize)> = Vec::new();
    let mut seq: usize = 0;
    while let Some(result_item) = scanner.next_header_with_data_offset() {
        let (hdr, _frame_offset, data_offset, data_size) = result_item?;
        if !matches!(hdr.blob_type(), crate::blob::BlobType::OsmData) { continue; }
        if let Some(filter_kind) = kind_filter {
            if let Some(idx) = hdr.index() {
                if idx.kind != filter_kind { continue; }
            }
        }
        schedule.push((seq, data_offset, data_size));
        seq += 1;
    }
    crate::debug::emit_marker("SCHEDULE_SCAN_LOOP_END");

    crate::debug::emit_marker("SCHEDULE_SCANNER_DROP_START");
    drop(scanner);
    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?
    );
    crate::debug::emit_marker("SCHEDULE_SCANNER_DROP_END");

    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("schedule_blobs", schedule.len() as i64);
    Ok((schedule, shared_file))
}

/// Like [`build_classify_schedule`] but returns three per-kind schedules
/// from a single header pass. At planet / Europe scale the header walk is
/// itself ~15 s; callers that need all three kinds (currently `check_refs`)
/// would otherwise pay that cost three times.
///
/// Blobs lacking indexdata are included in all three schedules (matching
/// the per-kind behaviour of `build_classify_schedule(.., Some(kind))`,
/// which only skips blobs whose indexdata reports a mismatched kind).
/// Each schedule's `seq` is local to that schedule (so each is a valid
/// contiguous 0..n range ready for `parallel_classify_phase`).
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn build_classify_schedules_split(
    input: &std::path::Path,
) -> Result<(
    Vec<(usize, u64, usize)>,
    Vec<(usize, u64, usize)>,
    Vec<(usize, u64, usize)>,
    std::sync::Arc<std::fs::File>,
)> {
    crate::debug::emit_marker("SCHEDULE_SCANNER_OPEN_START");
    let mut scanner = crate::blob::BlobReader::seekable_from_path(input)?;
    scanner.set_parse_indexdata(true);
    scanner.next_header_skip_blob()
        .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
    crate::debug::emit_marker("SCHEDULE_SCANNER_OPEN_END");

    crate::debug::emit_marker("SCHEDULE_SCAN_LOOP_START");
    let mut nodes: Vec<(usize, u64, usize)> = Vec::new();
    let mut ways: Vec<(usize, u64, usize)> = Vec::new();
    let mut rels: Vec<(usize, u64, usize)> = Vec::new();
    while let Some(result_item) = scanner.next_header_with_data_offset() {
        let (hdr, _frame_offset, data_offset, data_size) = result_item?;
        if !matches!(hdr.blob_type(), crate::blob::BlobType::OsmData) { continue; }
        match hdr.index().map(|i| i.kind) {
            Some(crate::blob_index::ElemKind::Node) => {
                nodes.push((nodes.len(), data_offset, data_size));
            }
            Some(crate::blob_index::ElemKind::Way) => {
                ways.push((ways.len(), data_offset, data_size));
            }
            Some(crate::blob_index::ElemKind::Relation) => {
                rels.push((rels.len(), data_offset, data_size));
            }
            None => {
                // Unindexed: visible to every kind filter in the legacy path,
                // so replicate to all three schedules here.
                nodes.push((nodes.len(), data_offset, data_size));
                ways.push((ways.len(), data_offset, data_size));
                rels.push((rels.len(), data_offset, data_size));
            }
        }
    }
    crate::debug::emit_marker("SCHEDULE_SCAN_LOOP_END");

    crate::debug::emit_marker("SCHEDULE_SCANNER_DROP_START");
    drop(scanner);
    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?
    );
    crate::debug::emit_marker("SCHEDULE_SCANNER_DROP_END");

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("schedule_node_blobs", nodes.len() as i64);
        crate::debug::emit_counter("schedule_way_blobs", ways.len() as i64);
        crate::debug::emit_counter("schedule_relation_blobs", rels.len() as i64);
    }
    Ok((nodes, ways, rels, shared_file))
}

/// Run a parallel classification phase: pread workers decompress and classify
/// blobs, sending compact results to a consumer that merges them into ID sets.
///
/// Each entry in `schedule` is `(seq, data_offset, data_size)`. Workers pread
/// the compressed blob data, decompress, build a `PrimitiveBlock`, run the
/// `classify` closure, and send the result. The consumer calls `merge(seq, r)`
/// for each result, forwarding the blob's schedule-order sequence number so
/// callers that care (e.g. `verify_ids`, which needs cross-blob monotonicity)
/// can reorder via `ReorderBuffer` or similar. Callers that don't care ignore
/// the seq argument.
///
/// **Note:** `merge` is called in arbitrary worker-completion order, not blob
/// file order. Callers that need file-order processing must buffer by seq.
/// Per-blob streaming classify: workers send `R` per blob, keep `S` for scratch.
///
/// Use for dense/hot paths (node classify, way classify) where per-worker
/// accumulation would be unbounded at planet scale. Each per-blob `R` is
/// bounded by blob size (~8000 elements). `S` persists across blobs for
/// scratch reuse (DenseNodeColumns, decompress buffers, etc.).
///
/// For sparse paths that want per-worker accumulation, use
/// [`parallel_classify_accumulate`].
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn parallel_classify_phase<S: Send, R: Send>(
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[(usize, u64, usize)],
    worker_init: impl Fn() -> S + Send + Sync,
    classify: impl Fn(&crate::PrimitiveBlock, &mut S) -> R + Send + Sync,
    mut merge: impl FnMut(usize, R),
) -> Result<()> {
    use std::os::unix::fs::FileExt as _;

    if schedule.is_empty() { return Ok(()); }

    let decode_threads = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4);

    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<(usize, u64, usize)>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<(usize, crate::error::Result<R>)>(32);

    std::thread::scope(|scope| -> Result<()> {
        scope.spawn(move || {
            for &item in schedule {
                if desc_tx.send(item).is_err() { break; }
            }
        });

        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let tx = result_tx.clone();
            let file = std::sync::Arc::clone(shared_file);
            let classify_ref = &classify;
            let worker_init_ref = &worker_init;
            scope.spawn(move || {
                let mut read_buf: Vec<u8> = Vec::new();
                let worker_pool = crate::blob::DecompressPool::new();
                let mut st_scratch: Vec<(u32, u32)> = Vec::new();
                let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
                let mut state = worker_init_ref();

                loop {
                    let (s, data_offset, data_size) = {
                        let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match guard.recv() {
                            Ok(d) => d,
                            Err(_) => break,
                        }
                    };

                    let r: crate::error::Result<R> = (|| {
                        read_buf.resize(data_size, 0);
                        file.read_exact_at(&mut read_buf, data_offset)
                            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                        let mut buf = crate::blob::pool_get_pub(&worker_pool, data_size * 4);
                        crate::blob::decompress_blob_raw(&read_buf, &mut buf)?;
                        let block = crate::block::PrimitiveBlock::from_vec_pooled_with_scratch(
                            buf, &worker_pool, &mut st_scratch, &mut gr_scratch,
                        )?;
                        Ok(classify_ref(&block, &mut state))
                    })();
                    if tx.send((s, r)).is_err() { break; }
                }
            });
        }
        drop(desc_rx);
        drop(result_tx);

        for (seq, result) in result_rx {
            merge(seq, result?);
        }
        Ok(())
    })?;

    Ok(())
}

/// Per-worker accumulation classify: workers accumulate into `S` across
/// all blobs, send `S` once at completion.
///
/// # When to use
///
/// The per-worker `S` is held for the duration of the whole scan and only
/// merged at the end. The safe usage envelope is determined by the upper
/// bound on per-worker `S` memory at the largest scale you support,
/// multiplied by the number of decode threads.
///
/// Safe: relation classify (~68 MB per worker at planet) and relation
/// closure members (~13 MB per worker). These are sparse paths where `S`
/// is dominated by a small set of relation-local IDs or metadata.
///
/// Borderline: per-worker `IdSetDense` accumulation of node IDs during
/// way classify (geocode Pass 1.5). A worker can legitimately touch node
/// IDs across the full planet range via referenced-node unions, so the
/// worst-case per-worker bitmap is ~1.3 GB at planet scale (10.4 B node
/// IDs × 1 bit). Shipping at 14.59 GB peak RSS (planet) - OK in practice,
/// but on the rewrite list in `notes/geocode-build-opportunities.md`.
/// If you add another caller like this, measure first.
///
/// Unsafe: per-worker `Vec<i64>` accumulation of node IDs during dense
/// node classify (would be O(billions of i64) per worker). Use
/// [`parallel_classify_phase`] instead - its per-blob merge is bounded
/// by blob size (~8 000 elements).
///
/// If you change this comment, also update the caller audit in the
/// geocode Pass 1.5 call site and the TODO item tracking it.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn parallel_classify_accumulate<S: Send>(
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[(usize, u64, usize)],
    worker_init: impl Fn() -> S + Send + Sync,
    classify: impl Fn(&crate::PrimitiveBlock, &mut S) + Send + Sync,
    mut merge: impl FnMut(S),
) -> Result<()> {
    use std::os::unix::fs::FileExt as _;

    if schedule.is_empty() { return Ok(()); }

    let decode_threads = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4);

    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<(usize, u64, usize)>(16);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<crate::error::Result<S>>(decode_threads);

    std::thread::scope(|scope| -> Result<()> {
        scope.spawn(move || {
            for &item in schedule {
                if desc_tx.send(item).is_err() { break; }
            }
        });

        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let tx = result_tx.clone();
            let file = std::sync::Arc::clone(shared_file);
            let classify_ref = &classify;
            let worker_init_ref = &worker_init;
            scope.spawn(move || {
                let mut read_buf: Vec<u8> = Vec::new();
                let worker_pool = crate::blob::DecompressPool::new();
                let mut st_scratch: Vec<(u32, u32)> = Vec::new();
                let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
                let mut state = worker_init_ref();

                let result: crate::error::Result<()> = (|| {
                    loop {
                        let (_s, data_offset, data_size) = {
                            let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            match guard.recv() {
                                Ok(d) => d,
                                Err(_) => return Ok(()),
                            }
                        };

                        read_buf.resize(data_size, 0);
                        file.read_exact_at(&mut read_buf, data_offset)
                            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
                        let mut buf = crate::blob::pool_get_pub(&worker_pool, data_size * 4);
                        crate::blob::decompress_blob_raw(&read_buf, &mut buf)?;
                        let block = crate::block::PrimitiveBlock::from_vec_pooled_with_scratch(
                            buf, &worker_pool, &mut st_scratch, &mut gr_scratch,
                        )?;
                        classify_ref(&block, &mut state);
                    }
                })();

                match result {
                    Ok(()) => { tx.send(Ok(state)).ok(); }
                    Err(e) => { tx.send(Err(e)).ok(); }
                }
            });
        }
        drop(desc_rx);
        drop(result_tx);

        for result in result_rx {
            merge(result?);
        }
        Ok(())
    })?;

    Ok(())
}

/// Warn if the input header declares `LocationsOnWays` - inline way-node
/// coordinates are not propagated through re-encoding.
pub(crate) fn warn_locations_on_ways_loss(header: &crate::HeaderBlock) {
    if header.has_locations_on_ways() {
        eprintln!(
            "Warning: input PBF has LocationsOnWays (inline way-node coordinates). \
             These will not be preserved in the output."
        );
    }
}

// ---------------------------------------------------------------------------
// Header overrides (--generator, --output-header)
// ---------------------------------------------------------------------------

/// Header field overrides from `--generator` and `--output-header` CLI flags.
#[derive(Default)]
pub struct HeaderOverrides {
    pub generator: Option<String>,
    pub replication_timestamp: Option<i64>,
    pub replication_sequence_number: Option<i64>,
    pub replication_base_url: Option<String>,
}

impl HeaderOverrides {
    /// Parse CLI arguments into header overrides.
    ///
    /// `output_headers` entries have the format `key=value`. Supported keys:
    /// `osmosis_replication_timestamp`, `osmosis_replication_sequence_number`,
    /// `osmosis_replication_base_url`.
    pub fn parse(generator: Option<String>, output_headers: &[String]) -> Result<Self> {
        let mut ov = HeaderOverrides {
            generator,
            ..Default::default()
        };
        for entry in output_headers {
            let (key, value) = entry.split_once('=').ok_or_else(|| {
                format!("invalid --output-header format: '{entry}' (expected key=value)")
            })?;
            match key {
                "osmosis_replication_timestamp" => {
                    ov.replication_timestamp = Some(value.parse::<i64>().map_err(|_| {
                        format!("invalid osmosis_replication_timestamp: '{value}'")
                    })?);
                }
                "osmosis_replication_sequence_number" => {
                    ov.replication_sequence_number =
                        Some(value.parse::<i64>().map_err(|_| {
                            format!("invalid osmosis_replication_sequence_number: '{value}'")
                        })?);
                }
                "osmosis_replication_base_url" => {
                    ov.replication_base_url = Some(value.to_string());
                }
                _ => return Err(format!("unknown --output-header key: '{key}'").into()),
            }
        }
        Ok(ov)
    }

    /// Apply overrides to a header builder. Called after the command-specific
    /// configure closure so CLI flags always win.
    pub(crate) fn apply<'a>(&'a self, mut hb: HeaderBuilder<'a>) -> HeaderBuilder<'a> {
        if let Some(program) = &self.generator {
            hb = hb.writing_program(program);
        }
        if let Some(ts) = self.replication_timestamp {
            hb = hb.replication_timestamp(ts);
        }
        if let Some(seq) = self.replication_sequence_number {
            hb = hb.replication_sequence_number(seq);
        }
        if let Some(url) = &self.replication_base_url {
            hb = hb.replication_base_url(url);
        }
        hb
    }
}

/// Build output header bytes from an input header.
///
/// Applies `configure` to the header builder, then preserves sortedness if
/// requested and if the input header is sorted, then applies CLI overrides.
pub(crate) fn build_output_header(
    header: &crate::HeaderBlock,
    preserve_sorted: bool,
    overrides: &HeaderOverrides,
    configure: impl FnOnce(HeaderBuilder) -> HeaderBuilder,
) -> Result<Vec<u8>> {
    let mut hb = configure(HeaderBuilder::from_header(header));
    if preserve_sorted && header.is_sorted() {
        hb = hb.sorted();
    }
    hb = overrides.apply(hb);
    Ok(hb.build()?)
}

/// Open a pipelined writer from an input header.
///
/// Supports O_DIRECT and io_uring when the corresponding features are compiled
/// in and the flags are set. Pass `false, false` for default buffered I/O.
#[allow(clippy::too_many_arguments)]
pub(crate) fn writer_from_header(
    output: &Path,
    compression: Compression,
    header: &crate::HeaderBlock,
    preserve_sorted: bool,
    overrides: &HeaderOverrides,
    configure: impl FnOnce(HeaderBuilder) -> HeaderBuilder,
    direct_io: bool,
    io_uring: bool,
) -> Result<PbfWriter<FileWriter>> {
    let header_bytes = build_output_header(header, preserve_sorted, overrides, configure)?;
    writer_from_header_bytes(output, compression, &header_bytes, direct_io, io_uring)
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
/// table corruption), defaults to empty string - consistent with the
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

/// Apply per-attribute cleaning to metadata. Returns `None` if all attributes
/// are cleaned or if the input has no metadata.
pub(crate) fn clean_metadata<'a>(meta: Option<Metadata<'a>>, clean: &cat::CleanAttrs) -> Option<Metadata<'a>> {
    if !clean.any() {
        return meta;
    }
    meta.map(|mut m| {
        if clean.version { m.version = 0; }
        if clean.changeset { m.changeset = 0; }
        if clean.timestamp { m.timestamp = 0; }
        if clean.uid { m.uid = 0; }
        if clean.user { m.user = ""; }
        m
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
/// filter is a no-op - all blobs are decompressed (significantly slower)."
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
             \x20 pbfhogg cat input.osm.pbf -o indexed.osm.pbf\n\n\
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

/// Unconditionally return the "not sorted" error for a given path.
///
/// Used when the sorted flag has already been checked separately (e.g. via
/// `check_sorted_and_indexed`) and we just need to emit the error.
pub(crate) fn require_sorted_err(path: &Path, context: &str) -> Result<()> {
    Err(format!(
        "{context} is not sorted (missing Sort.Type_then_ID optional feature).\n\
         File: {}\n\n\
         Sort the input file first:\n\n\
         \x20 pbfhogg sort {} -o sorted.osm.pbf\n\n\
         Streaming diff requires sorted inputs to operate in constant memory.",
        path.display(),
        path.display(),
    )
    .into())
}

// ---------------------------------------------------------------------------
// OSM ID ordering - canonical sort order matching libosmium
// ---------------------------------------------------------------------------

/// Sort key for OSM element IDs in canonical order.
///
/// Order: 0, then negative IDs by ascending absolute value (-1, -2, -3, ...),
/// then positive IDs (1, 2, 3, ...). Matches libosmium's sort comparator.
///
/// For positive-only IDs (all production PBFs), this is equivalent to plain
/// i64 comparison - the `(2, id)` tuple compares identically to raw `id`.
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
///
/// O(1) header-only probe: reads blob headers until the first OsmData
/// blob and returns whether it carries indexdata. Returns false if the
/// file has no data blobs. Trusts the first blob to be representative;
/// partially-indexed PBFs surface as a mid-run error at the consuming
/// site rather than being detected up front.
pub fn has_indexdata(path: &Path, direct_io: bool) -> Result<bool> {
    let mut reader = FileReader::open(path, direct_io)?;
    let mut offset = 0u64;
    while let Some(info) = read_blob_header_only(&mut reader, &mut offset)? {
        if matches!(info.blob_type, BlobKind::OsmData) {
            return Ok(info.index.is_some());
        }
        reader.skip(info.data_size as u64)?;
        offset += info.data_size as u64;
    }
    Ok(false)
}

/// Format a Unix epoch timestamp (seconds) as ISO 8601 UTC string.
///
/// Uses the civil-time algorithm from Howard Hinnant's `chrono`-compatible
/// date library to convert days since epoch to (year, month, day).
pub(crate) fn format_epoch_secs(secs: u64) -> String {
    let secs = secs.cast_signed();
    let day_secs = secs.rem_euclid(86400);
    let days = (secs - day_secs) / 86400;

    // Howard Hinnant's algorithm: days since 1970-01-01 → (y, m, d)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    let h = day_secs / 3600;
    let min = (day_secs % 3600) / 60;
    let s = day_secs % 60;

    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    /// Create a minimal valid `PrimitiveBlock` with approximately `target_size` bytes.
    ///
    /// Uses an empty stringtable (2 bytes) plus a padding unknown field to reach
    /// the target size. The parser silently skips unknown fields.
    fn make_block(target_size: usize) -> PrimitiveBlock {
        assert!(target_size >= 2, "minimum block size is 2 bytes");
        let mut buf = Vec::with_capacity(target_size);
        // Field 1 (stringtable), wire type 2 (len-delimited), length 0
        buf.push(0x0a);
        buf.push(0x00);
        if target_size > 2 {
            // Pad with unknown field 31, wire type 2 (len-delimited)
            // field 31 = (31 << 3) | 2 = 250, needs 2-byte varint tag: 0xfa 0x01
            buf.push(0xfa);
            buf.push(0x01);
            // Compute how many overhead bytes the length varint will take,
            // then set pad_len so total = target_size.
            let remaining = target_size - 4; // after stringtable (2) + tag (2)
            let varint_len = if remaining.saturating_sub(1) < 128 { 1 } else { 2 };
            let pad_len = remaining - varint_len;
            assert!(pad_len < 16384, "test helper limited to ~16 KB blocks");
            if varint_len == 1 {
                #[allow(clippy::cast_possible_truncation)]
                buf.push(pad_len as u8);
            } else {
                #[allow(clippy::cast_possible_truncation)]
                {
                    buf.push((pad_len as u8 & 0x7f) | 0x80);
                    buf.push((pad_len >> 7) as u8);
                }
            }
            buf.resize(buf.len() + pad_len, 0x00);
        }
        assert_eq!(buf.len(), target_size, "block size mismatch");
        PrimitiveBlock::new(Bytes::from(buf)).expect("valid minimal protobuf")
    }

    #[test]
    fn budgeted_batch_flushes_on_max_blocks() {
        let blocks: Vec<std::result::Result<PrimitiveBlock, crate::Error>> =
            (0..10).map(|_| Ok(make_block(100))).collect();

        let mut batch_sizes = Vec::new();
        for_each_primitive_block_batch_budgeted(blocks, 4, None, &mut |batch| {
            batch_sizes.push(batch.len());
            Ok(())
        })
        .expect("should not fail");

        // 10 blocks / 4 per batch = 2 full + 1 partial(2)
        assert_eq!(batch_sizes, vec![4, 4, 2]);
    }

    #[test]
    fn budgeted_batch_flushes_on_max_bytes() {
        // 5 blocks × 2000 bytes = 10000 bytes total
        // With max_bytes=4500, expect flush after ~2 blocks (2×2000=4000 < 4500,
        // 3rd push → 6000 >= 4500 → flush 3), then 2 more blocks flush at end.
        // Actually: block is pushed, THEN budget checked. So:
        //   push 1 (2000) → no flush
        //   push 2 (4000) → no flush
        //   push 3 (6000) → over budget → flush [3 blocks]
        //   push 4 (2000) → no flush
        //   push 5 (4000) → no flush
        //   end → flush [2 blocks]
        let blocks: Vec<std::result::Result<PrimitiveBlock, crate::Error>> =
            (0..5).map(|_| Ok(make_block(2000))).collect();

        let mut batch_sizes = Vec::new();
        for_each_primitive_block_batch_budgeted(blocks, 64, Some(4500), &mut |batch| {
            batch_sizes.push(batch.len());
            Ok(())
        })
        .expect("should not fail");

        assert_eq!(batch_sizes, vec![3, 2]);
    }

    #[test]
    fn budgeted_batch_both_limits_active() {
        // max_blocks=3, max_bytes=5000, blocks of 2000 bytes
        // Block limit (3) fires before byte limit (6000 > 5000 would also fire)
        let blocks: Vec<std::result::Result<PrimitiveBlock, crate::Error>> =
            (0..7).map(|_| Ok(make_block(2000))).collect();

        let mut batch_sizes = Vec::new();
        for_each_primitive_block_batch_budgeted(blocks, 3, Some(5000), &mut |batch| {
            batch_sizes.push(batch.len());
            Ok(())
        })
        .expect("should not fail");

        // 3, 3, 1 - block limit fires each time (3×2000=6000 >= 5000 too, but count hits first)
        assert_eq!(batch_sizes, vec![3, 3, 1]);
    }

    #[test]
    fn budgeted_batch_byte_limit_smaller_than_one_block() {
        // Even with max_bytes=50, we always flush at least 1 block
        let blocks: Vec<std::result::Result<PrimitiveBlock, crate::Error>> =
            (0..3).map(|_| Ok(make_block(2000))).collect();

        let mut batch_sizes = Vec::new();
        for_each_primitive_block_batch_budgeted(blocks, 64, Some(50), &mut |batch| {
            batch_sizes.push(batch.len());
            Ok(())
        })
        .expect("should not fail");

        // Each block exceeds budget immediately after push → flush 1 at a time
        assert_eq!(batch_sizes, vec![1, 1, 1]);
    }

    #[test]
    fn budgeted_batch_empty_input() {
        let blocks: Vec<std::result::Result<PrimitiveBlock, crate::Error>> = Vec::new();
        let mut called = false;
        for_each_primitive_block_batch_budgeted(blocks, 64, Some(1000), &mut |_batch| {
            called = true;
            Ok(())
        })
        .expect("should not fail");
        assert!(!called);
    }
}
