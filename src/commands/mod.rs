pub mod altw;
pub mod apply_changes;
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

// Alias for crate::BoxResult kept for short `super::Result` imports inside command
// implementations. The canonical definition (with rationale) is at the crate root.
pub(crate) type Result<T> = crate::BoxResult<T>;

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
pub(crate) fn flush_block(bb: &mut BlockBuilder, writer: &mut PbfWriter<FileWriter>) -> Result<()> {
    if let Some(OwnedBlock {
        bytes,
        index,
        tagdata,
        way_members,
    }) = bb.take_owned()?
    {
        writer.write_primitive_block_owned(
            bytes,
            index,
            tagdata.as_deref(),
            way_members.as_deref(),
        )?;
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
        for OwnedBlock {
            bytes: block_bytes,
            index,
            tagdata,
            way_members,
        } in blocks
        {
            writer.write_primitive_block_owned(
                block_bytes,
                index,
                tagdata.as_deref(),
                way_members.as_deref(),
            )?;
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
    if let Some(owned) = bb.take_owned().map_err(|e| e.to_string())? {
        output.push(owned);
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

/// Warn if the input header declares way-level metadata that re-encoding does
/// not propagate.
pub(crate) fn warn_locations_on_ways_loss(header: &crate::HeaderBlock) {
    if header.has_locations_on_ways()
        || header.has_way_members_v1()
        || header.has_shared_node_pins_v1()
    {
        eprintln!(
            "Warning: input PBF has way-level enrichment metadata. \
             LocationsOnWays and injected prepass metadata are not preserved in the output."
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
                    ov.replication_sequence_number = Some(value.parse::<i64>().map_err(|_| {
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
            Ok(PbfWriter::to_path_direct(
                output,
                compression,
                header_bytes,
            )?)
        }
        #[cfg(not(feature = "linux-direct-io"))]
        {
            Err("--direct-io requires the linux-direct-io feature".into())
        }
    } else {
        Ok(PbfWriter::to_path(output, compression, header_bytes)?)
    }
}

/// Same as [`writer_from_header_bytes`] but uses the parallel writer
/// (`PbfWriter::to_path_parallel`) instead of the single-thread
/// `to_path` writer for the default (non-`--direct-io`,
/// non-`--io-uring`) branch. Lifts the ~1.5 GB/s NVMe single-thread
/// write ceiling.
///
/// Used by commands whose pass-2 is writer-bound at default `zlib:6`
/// compression: apply-changes (winning across the writer-backend
/// matrix at germany / europe / planet) and ALTW pass 2 (CPU savings
/// from wire-format reframe were getting absorbed by the serial
/// writer queue).
pub(crate) fn writer_from_header_bytes_parallel(
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
            Ok(PbfWriter::to_path_direct(
                output,
                compression,
                header_bytes,
            )?)
        }
        #[cfg(not(feature = "linux-direct-io"))]
        {
            Err("--direct-io requires the linux-direct-io feature".into())
        }
    } else {
        Ok(PbfWriter::to_path_parallel(
            output,
            compression,
            header_bytes,
        )?)
    }
}

/// Same as [`writer_from_header`] but uses the parallel writer for the
/// default branch. See [`writer_from_header_bytes_parallel`] for the
/// rationale.
#[allow(clippy::too_many_arguments)]
pub(crate) fn writer_from_header_parallel(
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
    writer_from_header_bytes_parallel(output, compression, &header_bytes, direct_io, io_uring)
}

/// Map Osmosis sentinel -1 to 0 (protobuf default for absent) in dense node
/// Apply per-attribute cleaning to metadata. Returns `None` if all attributes
/// are cleaned or if the input has no metadata.
pub(crate) fn clean_metadata<'a>(
    meta: Option<Metadata<'a>>,
    clean: &cat::CleanAttrs,
) -> Option<Metadata<'a>> {
    if !clean.any() {
        return meta;
    }
    meta.map(|mut m| {
        if clean.version {
            m.version = 0;
        }
        if clean.changeset {
            m.changeset = 0;
        }
        if clean.timestamp {
            m.timestamp = 0;
        }
        if clean.uid {
            m.uid = 0;
        }
        if clean.user {
            m.user = "";
        }
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

/// Choose between the pread header walker and the pipelined reader for a
/// selective scan, from a bounded estimate of the input's OSMData blob count
/// (ADR-0006). Emits the estimate as the `walk_estimated_blobs` counter.
///
/// Non-indexed input is pinned to the walker: without indexdata neither arm
/// can skip irrelevant blob types, and that unmeasured full-decode regime is
/// outside the dispatch policy. `has_indexdata` reflects the first data blob
/// only, so a file with an indexed head and an unindexed tail can still
/// dispatch to the pipelined arm; its filter passes unindexed blobs through
/// to a full decode, which keeps that choice correct, merely unpriced.
pub(crate) fn dispatch_scan_arm(
    input: &Path,
    has_indexdata: bool,
    min_blobs: u64,
) -> Result<crate::read::header_walker::ScanArm> {
    use crate::read::header_walker::{ScanArm, choose_scan_arm_at, estimate_blob_count};
    if !has_indexdata {
        return Ok(ScanArm::Walker);
    }
    let estimate = estimate_blob_count(input)?;
    crate::debug::emit_counter(
        "walk_estimated_blobs",
        i64::try_from(estimate.osmdata_blobs).unwrap_or(i64::MAX),
    );
    Ok(choose_scan_arm_at(&estimate, min_blobs))
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
    while let Some(info) = crate::read::raw_frame::read_blob_header_only(&mut reader, &mut offset)?
    {
        if matches!(info.blob_type, BlobKind::OsmData) {
            return Ok(info.index.is_some());
        }
        reader.skip(info.data_size as u64)?;
        offset += info.data_size as u64;
    }
    Ok(false)
}

/// Check if the first OsmData blob in a PBF has tagdata (`BlobHeader`
/// field 4, the per-blob tag key index).
///
/// O(1) header-only probe, the tagdata sibling of [`has_indexdata`]: reads
/// blob headers until the first OsmData blob and returns whether it carries
/// tagdata. Returns false if the file has no data blobs. Trusts the first
/// blob to be representative.
pub fn has_tagdata(path: &Path, direct_io: bool) -> Result<bool> {
    let mut reader = FileReader::open(path, direct_io)?;
    let mut offset = 0u64;
    while let Some(info) = crate::read::raw_frame::read_blob_header_only(&mut reader, &mut offset)?
    {
        if matches!(info.blob_type, BlobKind::OsmData) {
            return Ok(info.has_tagdata);
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

/// Parse an RFC 3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`, the OSM XML
/// timestamp format) into Unix epoch seconds.
///
/// Inverse of [`format_epoch_secs`]. Strict: exactly 20 bytes, `Z` suffix,
/// no fractional seconds or offsets (OSM planet/OSC files never carry
/// either). Used by the OSC parser for element `timestamp` attributes and
/// by the CLI for `time-filter`'s cutoff argument.
pub fn parse_rfc3339_utc(input: &str) -> std::result::Result<i64, String> {
    const FORMAT_ERR: &str = "timestamp must be YYYY-MM-DDTHH:MM:SSZ";
    if input.len() != 20 {
        return Err(FORMAT_ERR.to_owned());
    }

    let bytes = input.as_bytes();
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return Err(FORMAT_ERR.to_owned());
    }

    let field = |start: usize, end: usize| -> std::result::Result<i64, String> {
        input[start..end]
            .parse::<i64>()
            .map_err(|_| "invalid numeric timestamp component".to_owned())
    };

    let year = field(0, 4)?;
    let month = field(5, 7)?;
    let day = field(8, 10)?;
    let hour = field(11, 13)?;
    let minute = field(14, 16)?;
    let second = field(17, 19)?;

    if !(1..=12).contains(&month) {
        return Err("invalid month in timestamp".to_owned());
    }
    if hour > 23 || minute > 59 || second > 59 {
        return Err("invalid time in timestamp".to_owned());
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    };
    if day == 0 || day > max_day {
        return Err("invalid day in timestamp".to_owned());
    }

    // Howard Hinnant's civil-date algorithm (inverse of format_epoch_secs).
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    Ok(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `parse_rfc3339_utc` is the exact inverse of `format_epoch_secs` and
    /// rejects malformed input.
    #[test]
    fn rfc3339_parse_inverts_format() -> std::result::Result<(), String> {
        // Known anchor values.
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:00Z")?, 0);
        assert_eq!(parse_rfc3339_utc("1970-01-02T00:00:00Z")?, 86_400);
        assert_eq!(parse_rfc3339_utc("2004-02-29T12:00:00Z")?, 1_078_056_000);

        // Roundtrip across a spread of epochs (leap years, month ends,
        // century boundary).
        for secs in [
            0_u64,
            951_827_696,   // 2000-02-29 (leap century)
            1_582_934_400, // 2020-02-29
            1_771_622_445, // denmark replication timestamp in test data
            4_102_444_799, // 2099-12-31T23:59:59Z
        ] {
            let formatted = format_epoch_secs(secs);
            assert_eq!(
                parse_rfc3339_utc(&formatted)?,
                secs.cast_signed(),
                "roundtrip failed for {formatted}"
            );
        }

        // Rejections.
        assert!(parse_rfc3339_utc("2026-02-20 21:39:49").is_err()); // wrong shape
        assert!(parse_rfc3339_utc("2026-13-01T00:00:00Z").is_err()); // month 13
        assert!(parse_rfc3339_utc("2026-02-30T00:00:00Z").is_err()); // Feb 30
        assert!(parse_rfc3339_utc("2026-02-20T24:00:00Z").is_err()); // hour 24
        assert!(parse_rfc3339_utc("not-a-timestamp-at-al").is_err());
        Ok(())
    }
}
