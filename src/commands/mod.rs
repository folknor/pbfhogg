// The command layer writes outputs only through `crate::output` (the writer
// factory below takes an `Output`) and scratch only through
// `crate::scratch`; the crate root denies the methods clippy.toml lists.

pub mod altw;
pub mod apply_changes;
pub(crate) mod blob_merge;
pub mod cat;
#[cfg(feature = "commands")]
pub mod check;
pub mod degrade;
pub mod diff;
pub mod digest;
#[cfg(feature = "commands")]
pub mod export;
#[cfg(feature = "commands")]
pub mod extract;
pub mod getid;
pub mod getparents;
pub(crate) mod input_front;
pub mod inspect;
pub mod merge_changes;
pub(crate) mod output_header;
pub mod renumber;
pub mod repack;
pub mod sort;
#[cfg(feature = "commands")]
pub(crate) mod spatial;
pub mod tags_count;
pub mod tags_filter;
pub mod time_filter;

use std::path::Path;

use crate::blob::BlobKind;
use crate::block_builder::{BlockBuilder, HeaderBuilder, Metadata, OwnedBlock};
use crate::error::Error;
use crate::file_reader::FileReader;
use crate::file_writer::FileWriter;
use crate::frame::{Frames, HeaderFields};
use crate::write::OutputBackend;
use crate::writer::{Compression, PbfWriter};

// Alias for crate::Result kept for short `super::Result` imports inside command
// implementations.
pub(crate) type Result<T> = crate::Result<T>;

// ---------------------------------------------------------------------------
// Shared output helpers
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
    if let Some(block) = bb.take_owned()? {
        writer.write_primitive_block_owned(block)?;
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
    results: Vec<Result<(Vec<OwnedBlock>, S)>>,
    writer: &mut PbfWriter<FileWriter>,
    mut merge: impl FnMut(S),
    host: &crate::host::Host,
) -> Result<()> {
    let _ = host;
    for result in results {
        let (blocks, stats) = result?;
        merge(stats);
        for block in blocks {
            writer.write_primitive_block_owned(block)?;
        }
    }
    Ok(())
}

/// Flush the current block from a [`BlockBuilder`] into a local output buffer.
///
/// Like `flush_block` but writes to a `Vec<OwnedBlock>` instead of a
/// `PbfWriter`, so it can be called from rayon worker threads.
pub(crate) fn flush_local(bb: &mut BlockBuilder, output: &mut Vec<OwnedBlock>) -> Result<()> {
    if let Some(owned) = bb.take_owned()? {
        output.push(owned);
    }
    Ok(())
}

/// Ensure the [`BlockBuilder`] has capacity for a node, flushing to local
/// output if full. Used by rayon worker threads in parallel batch processing.
pub(crate) fn ensure_node_capacity_local(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> Result<()> {
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
) -> Result<()> {
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
) -> Result<()> {
    if !bb.can_add_relation() {
        flush_local(bb, output)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Header overrides (--generator, --output-header)
// ---------------------------------------------------------------------------

/// What an override does to one output header field.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum FieldOverride<T> {
    /// No override: the command's output-header policy decides the field.
    #[default]
    Unchanged,
    /// Write this value, whatever the policy would have written.
    Set(T),
    /// Do not write the field, whatever the policy would have written
    /// (`--output-header KEY=` with an empty value).
    Clear,
}

/// Header field overrides from `--generator` and `--output-header` CLI flags.
#[derive(Default)]
pub struct HeaderOverrides {
    /// The writing program. An empty string is written as a present, empty
    /// field (osmium writes the writing program unconditionally too).
    pub generator: Option<String>,
    pub replication_timestamp: FieldOverride<i64>,
    pub replication_sequence_number: FieldOverride<i64>,
    pub replication_base_url: FieldOverride<String>,
}

#[derive(Clone, Copy)]
enum HeaderOverrideKind {
    Timestamp,
    SequenceNumber,
    BaseUrl,
}

#[derive(Clone, Copy)]
struct HeaderOverrideKey {
    kind: HeaderOverrideKind,
    name: &'static str,
}

impl HeaderOverrideKey {
    const ALL: [Self; 3] = [
        Self {
            kind: HeaderOverrideKind::Timestamp,
            name: "osmosis_replication_timestamp",
        },
        Self {
            kind: HeaderOverrideKind::SequenceNumber,
            name: "osmosis_replication_sequence_number",
        },
        Self {
            kind: HeaderOverrideKind::BaseUrl,
            name: "osmosis_replication_base_url",
        },
    ];

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|key| key.name == value)
    }

    fn help_list() -> String {
        Self::ALL
            .into_iter()
            .map(|key| key.name)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// One parsed `--output-header KEY=VALUE` entry.
///
/// Parsing one entry on its own is what lets a front end refuse a malformed
/// value at its argument boundary, before any input is opened; the overrides
/// are then assembled from already-typed entries by
/// [`HeaderOverrides::from_entries`], which cannot fail.
///
/// `None` is an entry with an empty value (`KEY=`): the field is not written.
/// osmium means the same by an empty value (libosmium's PBF writer skips an
/// empty replication field).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputHeaderEntry {
    ReplicationTimestamp(Option<i64>),
    ReplicationSequenceNumber(Option<i64>),
    ReplicationBaseUrl(Option<String>),
}

impl std::str::FromStr for OutputHeaderEntry {
    type Err = Error;

    fn from_str(entry: &str) -> Result<Self> {
        let (key, value) = entry.split_once('=').ok_or_else(|| {
            Error::invalid_argument(format!(
                "invalid output header entry: '{entry}' (expected key=value)"
            ))
        })?;
        let key = HeaderOverrideKey::parse(key).ok_or_else(|| {
            Error::invalid_argument(format!(
                "unknown output header key: '{key}' (supported: {})",
                HeaderOverrideKey::help_list()
            ))
        })?;
        if value.is_empty() {
            return Ok(match key.kind {
                HeaderOverrideKind::Timestamp => Self::ReplicationTimestamp(None),
                HeaderOverrideKind::SequenceNumber => Self::ReplicationSequenceNumber(None),
                HeaderOverrideKind::BaseUrl => Self::ReplicationBaseUrl(None),
            });
        }
        Ok(match key.kind {
            // Integer seconds or RFC 3339, the same two spellings
            // time-filter's cutoff takes.
            HeaderOverrideKind::Timestamp => {
                Self::ReplicationTimestamp(Some(match value.parse::<i64>() {
                    Ok(seconds) => seconds,
                    Err(_) => parse_rfc3339_utc(value).map_err(|_| {
                        Error::invalid_argument(format!(
                            "invalid {}: '{value}' (expected UNIX seconds or \
                             YYYY-MM-DDTHH:MM:SSZ, or nothing to omit the field)",
                            key.name
                        ))
                    })?,
                }))
            }
            HeaderOverrideKind::SequenceNumber => {
                Self::ReplicationSequenceNumber(Some(value.parse::<i64>().map_err(|_| {
                    Error::invalid_argument(format!(
                        "invalid {}: '{value}' (expected an integer, or nothing to \
                         omit the field)",
                        key.name
                    ))
                })?))
            }
            HeaderOverrideKind::BaseUrl => Self::ReplicationBaseUrl(Some(value.to_string())),
        })
    }
}

impl HeaderOverrides {
    /// Parse CLI arguments into header overrides.
    ///
    /// `output_headers` entries have the format `key=value`.
    pub fn parse(generator: Option<String>, output_headers: &[String]) -> Result<Self> {
        let entries = output_headers
            .iter()
            .map(|entry| entry.parse::<OutputHeaderEntry>())
            .collect::<Result<Vec<_>>>()?;
        Ok(Self::from_entries(generator, entries))
    }

    /// Assemble overrides from already-parsed entries. A later entry for the
    /// same field replaces an earlier one, a clear included.
    #[must_use]
    pub fn from_entries(
        generator: Option<String>,
        entries: impl IntoIterator<Item = OutputHeaderEntry>,
    ) -> Self {
        fn field<T>(value: Option<T>) -> FieldOverride<T> {
            value.map_or(FieldOverride::Clear, FieldOverride::Set)
        }
        let mut ov = HeaderOverrides {
            generator,
            ..Default::default()
        };
        for entry in entries {
            match entry {
                OutputHeaderEntry::ReplicationTimestamp(ts) => ov.replication_timestamp = field(ts),
                OutputHeaderEntry::ReplicationSequenceNumber(seq) => {
                    ov.replication_sequence_number = field(seq);
                }
                OutputHeaderEntry::ReplicationBaseUrl(url) => {
                    ov.replication_base_url = field(url);
                }
            }
        }
        ov
    }

    /// Return the generated long help for the supported output-header keys.
    #[doc(hidden)]
    #[must_use]
    pub fn output_header_help() -> String {
        format!(
            "Set output header fields (repeatable, format: key=value). Supported keys: {}. \
             The timestamp takes UNIX seconds or YYYY-MM-DDTHH:MM:SSZ. Each key replaces \
             only its own field; an empty value (key=) omits the field.",
            HeaderOverrideKey::help_list()
        )
    }

    /// Apply overrides to a header builder. Called last by
    /// [`output_header::build`] so CLI flags always win.
    pub(crate) fn apply<'a>(&'a self, mut hb: HeaderBuilder<'a>) -> HeaderBuilder<'a> {
        if let Some(program) = &self.generator {
            hb = hb.writing_program(program);
        }
        hb = match self.replication_timestamp {
            FieldOverride::Unchanged => hb,
            FieldOverride::Set(ts) => hb.replication_timestamp(ts),
            FieldOverride::Clear => hb.without_replication_timestamp(),
        };
        hb = match self.replication_sequence_number {
            FieldOverride::Unchanged => hb,
            FieldOverride::Set(seq) => hb.replication_sequence_number(seq),
            FieldOverride::Clear => hb.without_replication_sequence_number(),
        };
        match &self.replication_base_url {
            FieldOverride::Unchanged => hb,
            FieldOverride::Set(url) => hb.replication_base_url(url),
            FieldOverride::Clear => hb.without_replication_base_url(),
        }
    }
}

/// Open a pipelined writer whose header [`output_header::build_one`] derives
/// from one input header and the facts the command states about its output.
///
/// Supports O_DIRECT and io_uring when the corresponding features are compiled
/// in and `backend` asks for them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn writer_from_header(
    output: &crate::output::Output,
    compression: Compression,
    header: &crate::HeaderBlock,
    facts: &output_header::OutputFacts,
    overrides: &HeaderOverrides,
    backend: OutputBackend,
    host: &crate::host::Host,
) -> Result<PbfWriter<FileWriter>> {
    let built = output_header::build_one(header, facts, overrides, host)?;
    writer_from_header_bytes(output, compression, &built.bytes, backend, host)
}

/// Open an output writer from prebuilt header bytes on the given backend.
///
/// The one place the backend is turned into a `PbfWriter`, and so the one
/// place that refuses a backend whose feature is not compiled. A command whose
/// output pass is writer-bound at default `zlib:6` compression (apply-changes,
/// ALTW pass 2) passes `backend.parallel_by_default()` to get
/// `PbfWriter::to_path_parallel`, which lifts the ~1.5 GB/s NVMe single-thread
/// write ceiling.
///
/// Takes the run's [`Output`](crate::output::Output) rather than a path: every
/// backend writes the staging file, and only the command's single publication
/// owner renames it into place. The writer is
/// [`Output::pbf_writer`](crate::output::Output::pbf_writer): it holds the
/// output's writing session and leaves the `fsync` to publication.
pub(crate) fn writer_from_header_bytes(
    output: &crate::output::Output,
    compression: Compression,
    header_bytes: &[u8],
    backend: OutputBackend,
    host: &crate::host::Host,
) -> Result<PbfWriter<FileWriter>> {
    output.pbf_writer(compression, header_bytes, backend, host)
}

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
/// Returns `true` if indexdata is present, `false` if absent but `force` is set;
/// the forced case emits the one keyed Note every forced run shares.
/// The `reason` should be a complete sentence explaining why indexdata matters,
/// e.g. "input PBF has no blob-level indexdata. Without indexdata, the type
/// filter is a no-op - all blobs are decompressed (significantly slower)."
// Call sites own acceptance policy and the reason: getparents can choose a
// scan arm without refusing input, extract gates only complete/smart, and
// renumber requires metadata in every scheduled blob with no force fallback.
// Do not infer those decisions from this shared first-blob probe. Geocode
// separately grants unindexed input via allow_unindexed without overwrite.
//
// A pinned `input` is probed through its pin, so a command that pins reads
// the same file here as in its passes.
pub(crate) fn require_indexdata<'a>(
    input: impl Into<crate::read::input::InputSource<'a>>,
    direct_io: bool,
    force: bool,
    reason: &str,
    host: &crate::host::Host,
) -> Result<bool> {
    let source = input.into();
    let (present, _) =
        probe_first_data_blob(source, direct_io, &host.diagnostics, &host.invocation)?;
    indexdata_policy(present, source.path(), force, reason, host)
}

/// The acceptance half of [`require_indexdata`], for a caller that already
/// holds the first-data-blob hint (an [`input_front::InputFront`]).
pub(crate) fn indexdata_policy(
    present: bool,
    path: &Path,
    force: bool,
    reason: &str,
    host: &crate::host::Host,
) -> Result<bool> {
    if !force && !present {
        return Err(crate::error::new_missing_indexdata(reason)
            .locate(crate::error::Location::in_file(path)));
    }
    if !present {
        // The one statement every forced run on unindexed input makes. It
        // shares the reader's key, so the filter notes that would follow
        // from the same run are folded into it.
        crate::host::diag_once!(
            host.diagnostics,
            Note,
            "reader.filter_without_indexdata",
            "Input contains blobs without indexdata; proceeding without it, this run is slower"
        );
    }
    Ok(present)
}

/// Check that a PBF file declares `Sort.Type_then_ID`.
///
/// `context` identifies the file role, and `reason` explains why this command
/// requires sorted input. The error is [`ErrorKind::Unsorted`] located at
/// `path`; how to sort the file is the front end's advice.
///
/// [`ErrorKind::Unsorted`]: crate::ErrorKind::Unsorted
pub(crate) fn require_sorted(
    is_sorted: bool,
    path: &Path,
    context: &str,
    reason: &str,
) -> Result<()> {
    if !is_sorted {
        return Err(crate::error::new_unsorted(format!(
            "{context} does not declare Sort.Type_then_ID in its header. {reason}"
        ))
        .locate(crate::error::Location::in_file(path)));
    }
    Ok(())
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
pub(crate) fn dispatch_scan_arm<'a>(
    input: impl Into<crate::read::input::InputSource<'a>>,
    has_indexdata: bool,
    min_blobs: u64,
    host: &crate::host::Host,
) -> Result<crate::read::header_walker::ScanArm> {
    use crate::read::header_walker::{ScanArm, choose_scan_arm_at, estimate_blob_count};
    if !has_indexdata {
        return Ok(ScanArm::Walker);
    }
    let estimate = estimate_blob_count(input, host)?;
    host.invocation
        .counter("walk_estimated_blobs", estimate.osmdata_blobs);
    Ok(choose_scan_arm_at(&estimate, min_blobs))
}

/// Check if the first OsmData blob in a PBF has indexdata.
///
/// O(1) header-only probe: reads blob headers until the first OsmData
/// blob and returns whether it carries indexdata. A file with a header and
/// no data blobs is vacuously indexed (no blob lacks indexdata), so it
/// returns true. A file with no leading OsmHeader blob, a zero-byte file
/// included, is the missing-header error naming it. Trusts the first blob
/// to be representative; a
/// partially-indexed PBF is not detected up front, and each consuming site
/// decides what a later unindexed blob means (some decode it on a fallback
/// path, some stop with an error mid-run).
///
/// One open of `input`: a path is opened as it is, a pin through the pin
/// (it does not pin a path itself, so a caller that opens the input again
/// pins it first and passes the pin).
pub fn has_indexdata<'a>(
    input: impl Into<crate::read::input::InputSource<'a>>,
    direct_io: bool,
    diagnostics: &crate::host::Diagnostics,
) -> Result<bool> {
    // A library probe with no run to observe: it is never cancelled.
    let cancel = crate::host::Invocation::new();
    Ok(probe_first_data_blob(input, direct_io, diagnostics, &cancel)?.0)
}

/// [`has_indexdata`] for a run under `host`: the same probe, reporting to the
/// host's diagnostics and stopping at the next frame once its cancellation
/// token is tripped. The probe walks every frame between the header and the
/// first OsmData blob, however many extension frames that is.
pub fn has_indexdata_in<'a>(
    input: impl Into<crate::read::input::InputSource<'a>>,
    direct_io: bool,
    host: &crate::host::Host,
) -> Result<bool> {
    Ok(probe_first_data_blob(input, direct_io, &host.diagnostics, &host.invocation)?.0)
}

/// Check if the first OsmData blob in a PBF has tagdata (`BlobHeader`
/// field 4, the per-blob tag key index).
///
/// O(1) header-only probe, the tagdata sibling of [`has_indexdata`]: reads
/// blob headers until the first OsmData blob and returns whether it carries
/// tagdata. Returns false if the file has no data blobs, and the
/// missing-header error for a file without a leading OsmHeader blob. Trusts
/// the first blob to be representative.
pub fn has_tagdata(
    path: &Path,
    direct_io: bool,
    diagnostics: &crate::host::Diagnostics,
) -> Result<bool> {
    let cancel = crate::host::Invocation::new();
    Ok(probe_first_data_blob(path, direct_io, diagnostics, &cancel)?.1)
}

// This is a first-data-blob hint, not validation of every blob. Keep the
// early return: an all-blob preflight previously caused a measured regression
// across header-walking commands (reference/performance-history.md). Consumers
// decide what absent metadata in later blobs means (fallback decode or a
// mid-run error); `inspect --indexed` reports this same hint.
fn probe_first_data_blob<'a>(
    input: impl Into<crate::read::input::InputSource<'a>>,
    direct_io: bool,
    diagnostics: &crate::host::Diagnostics,
    cancel: &crate::host::Invocation,
) -> Result<(bool, bool)> {
    let source = input.into();
    let path = source.path();
    let mut frames = front_frames(
        FileReader::open_in(source, direct_io, cancel)?,
        path,
        diagnostics,
        cancel,
    );
    // The header comes first or the file is not a PBF: a zero-byte or
    // header-less file gets the one missing-header error here, ahead of any
    // question about indexdata. Its payload is skipped, not read.
    leading_header_frame(&mut frames, path)?;
    frames.skip_payload()?;
    probe_to_first_data_blob(&mut frames)
}

/// A frame walk over the front of `path` for the precondition probes,
/// observing `cancel` (the run's token, or a fresh one for a library probe
/// that has no run).
pub(crate) fn front_frames(
    reader: FileReader,
    path: &Path,
    diagnostics: &crate::host::Diagnostics,
    cancel: &crate::host::Invocation,
) -> Frames<FileReader> {
    let mut frames = Frames::new(reader, Some(0), HeaderFields::INDEXES);
    frames.set_path(Some(std::sync::Arc::from(path)));
    frames.set_diagnostics(diagnostics.clone());
    frames.set_cancel(cancel.clone());
    frames
}

/// Read the first frame's header, leaving the walk at its payload. A
/// zero-byte file, or one whose first frame is not an OsmHeader, is the
/// missing-header error, decided before any payload is read.
pub(crate) fn leading_header_frame(frames: &mut Frames<FileReader>, path: &Path) -> Result<()> {
    match frames.next_header()? {
        Some(header) if matches!(header.header.blob_type, BlobKind::OsmHeader) => Ok(()),
        _ => Err(crate::error::new_missing_header(path)),
    }
}

/// Continue a front-of-file read from the frame after the header to the
/// first OsmData frame's header, returning its `(indexdata, tagdata)`
/// presence. Frames in between are skipped, not read, and the data frame's
/// payload is left unread.
pub(crate) fn probe_to_first_data_blob(frames: &mut Frames<FileReader>) -> Result<(bool, bool)> {
    while let Some(header) = frames.next_header()? {
        if matches!(header.header.blob_type, BlobKind::OsmData) {
            let indexed = header
                .header
                .indexdata
                .is_some_and(|data| crate::blob_meta::BlobIndex::deserialize(&data).is_some());
            return Ok((indexed, header.header.tagdata.is_some()));
        }
        frames.skip_payload()?;
    }
    // No data blob: nothing lacks indexdata, so the file counts as indexed.
    // It carries no tagdata, which is a statement about presence, not a
    // gate.
    Ok((true, false))
}

/// Format a Unix epoch timestamp (seconds) as ISO 8601 UTC string.
///
/// Uses the civil-time algorithm from Howard Hinnant's `chrono`-compatible
/// date library to convert days since epoch to (year, month, day). Signed:
/// a timestamp before 1970 formats as its real date (`-1` is
/// `1969-12-31T23:59:59Z`) and [`parse_rfc3339_utc`] reads it back, for
/// every year with four digits.
pub(crate) fn format_epoch_secs(secs: i64) -> String {
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

/// Why a timestamp could not be parsed by [`parse_rfc3339_utc`]. A neutral
/// leaf: the caller decides whether the bad timestamp is an invalid argument
/// (the CLI cutoff) or invalid data (an OSC attribute).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimestampError(&'static str);

impl std::fmt::Display for TimestampError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for TimestampError {}

/// Parse an RFC 3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`, the OSM XML
/// timestamp format) into Unix epoch seconds.
///
/// Inverse of [`format_epoch_secs`]. Strict: exactly 20 bytes, `Z` suffix,
/// no fractional seconds or offsets (OSM planet/OSC files never carry
/// either). Used by the OSC parser for element `timestamp` attributes and
/// by the CLI for `time-filter`'s cutoff argument.
pub fn parse_rfc3339_utc(input: &str) -> std::result::Result<i64, TimestampError> {
    const FORMAT_ERR: TimestampError = TimestampError("timestamp must be YYYY-MM-DDTHH:MM:SSZ");
    if input.len() != 20 {
        return Err(FORMAT_ERR);
    }

    let bytes = input.as_bytes();
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return Err(FORMAT_ERR);
    }

    let field = |start: usize, end: usize| -> std::result::Result<i64, TimestampError> {
        input[start..end]
            .parse::<i64>()
            .map_err(|_| TimestampError("invalid numeric timestamp component"))
    };

    let year = field(0, 4)?;
    let month = field(5, 7)?;
    let day = field(8, 10)?;
    let hour = field(11, 13)?;
    let minute = field(14, 16)?;
    let second = field(17, 19)?;

    if !(1..=12).contains(&month) {
        return Err(TimestampError("invalid month in timestamp"));
    }
    if hour > 23 || minute > 59 || second > 59 {
        return Err(TimestampError("invalid time in timestamp"));
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
        return Err(TimestampError("invalid day in timestamp"));
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
    fn rfc3339_parse_inverts_format() -> std::result::Result<(), TimestampError> {
        // Known anchor values.
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:00Z")?, 0);
        assert_eq!(parse_rfc3339_utc("1970-01-02T00:00:00Z")?, 86_400);
        assert_eq!(parse_rfc3339_utc("2004-02-29T12:00:00Z")?, 1_078_056_000);

        // Roundtrip across a spread of epochs (leap years, month ends,
        // century boundary).
        for secs in [
            0_i64,
            951_827_696,     // 2000-02-29 (leap century)
            1_582_934_400,   // 2020-02-29
            1_771_622_445,   // denmark replication timestamp in test data
            4_102_444_799,   // 2099-12-31T23:59:59Z
            -1,              // 1969-12-31T23:59:59Z
            -2_208_988_800,  // 1900-01-01, a non-leap century
            -62_167_219_200, // 0000-01-01, the first four-digit year
        ] {
            let formatted = format_epoch_secs(secs);
            assert_eq!(
                parse_rfc3339_utc(&formatted)?,
                secs,
                "roundtrip failed for {formatted}"
            );
        }
        assert_eq!(format_epoch_secs(-1), "1969-12-31T23:59:59Z");

        // Rejections.
        assert!(parse_rfc3339_utc("2026-02-20 21:39:49").is_err()); // wrong shape
        assert!(parse_rfc3339_utc("2026-13-01T00:00:00Z").is_err()); // month 13
        assert!(parse_rfc3339_utc("2026-02-30T00:00:00Z").is_err()); // Feb 30
        assert!(parse_rfc3339_utc("2026-02-20T24:00:00Z").is_err()); // hour 24
        assert!(parse_rfc3339_utc("not-a-timestamp-at-al").is_err());
        Ok(())
    }
}
