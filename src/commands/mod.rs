pub mod altw;
pub mod cat;
#[cfg(feature = "commands")]
pub mod check;
pub mod degrade;
pub mod diff;
#[cfg(feature = "commands")]
pub mod extract;
pub mod getid;
pub mod getparents;
pub mod inspect;
pub mod apply_changes;
pub mod merge_changes;
pub mod renumber;
pub mod repack;
pub mod sort;
pub mod tags_count;
pub mod tags_filter;
pub mod time_filter;

use std::path::Path;

use crate::blob::BlobKind;
use crate::block_builder::{BlockBuilder, HeaderBuilder, Metadata, OwnedBlock};
use crate::file_reader::FileReader;
use crate::file_writer::FileWriter;
use crate::writer::{Compression, PbfWriter};
use crate::PrimitiveBlock;

// Alias for crate::BoxResult kept for short `super::Result` imports inside command
// implementations. The canonical definition (with rationale) is at the crate root.
pub(crate) type Result<T> = crate::BoxResult<T>;

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

/// Open an output writer for `apply-changes`. Parallel writer is the
/// default (winning or tying across the writer-backend matrix at
/// germany / europe / planet); `--io-uring` and `--direct-io` remain
/// as opt-in overrides.
pub(crate) fn writer_for_apply_changes(
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
        Ok(PbfWriter::to_path_parallel(output, compression, header_bytes)?)
    }
}

/// Map Osmosis sentinel -1 to 0 (protobuf default for absent) in dense node
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
    while let Some(info) = crate::read::raw_frame::read_blob_header_only(&mut reader, &mut offset)? {
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
