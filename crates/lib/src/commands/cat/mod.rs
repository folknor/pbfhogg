//! Concatenate PBF files with optional type filtering. Equivalent to `osmium cat`.

pub mod dedupe;
mod raw;
pub mod reindex;
mod repair;

use super::{
    HeaderOverrides, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, require_indexdata,
};
use crate::blob::{BlobKind, decode_blob_to_headerblock, decompress_blob_raw};
use crate::blob_meta::scan_block_ids_and_tags;
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::file_reader::FileReader;
use crate::owned::{TypeFilter, dense_node_metadata, element_metadata};
use crate::write::WriteOptions;
use crate::writer::{Compression, PbfWriter, reframe_raw_with_index};
use crate::{BlobFilter, Element, ElementKind, PrimitiveBlock};

use super::output_header::{Declared, OutputFacts, OutputHeader, Ways};
use super::{Result, flush_local};
use crate::error::Error;
use crate::file_writer::FileWriter;
use crate::read::frame::Frames;
use crate::read::input::{InputSource, PinnedInput};
use raw::RawInput;

/// Which metadata attributes to strip via `--clean`.
#[derive(Clone, Copy, Default)]
pub struct CleanAttrs {
    pub version: bool,
    pub changeset: bool,
    pub timestamp: bool,
    pub uid: bool,
    pub user: bool,
}

impl CleanAttrs {
    /// True if any attribute is being cleaned.
    pub fn any(&self) -> bool {
        self.version || self.changeset || self.timestamp || self.uid || self.user
    }
}

/// Statistics from a cat operation.
///
/// Every input data blob the run copied or opened has exactly one fate:
/// passed through, rewritten or re-encoded. An indexed blob a type filter
/// skips on its indexdata has none and is counted nowhere. Separately,
/// `blobs_payloads_opened` counts the work: a blob that passed through may
/// still have been opened, to index or validate it.
pub struct CatStats {
    /// Input data blobs whose payload bytes were written unchanged: the
    /// frame copied whole, or its header rebuilt to add indexdata (sidecar
    /// `cat_blobs_passthrough`).
    pub blobs_passthrough: u64,
    /// Input data blobs whose payload [`repair()`] replaced by the merged
    /// one and recompressed, written as one blob (sidecar
    /// `cat_blobs_rewritten`); always 0 from [`cat`] and with `clean`,
    /// where a repaired blob is re-encoded.
    pub blobs_rewritten: u64,
    /// Input data blobs decoded into elements and written from them
    /// (sidecar `cat_blobs_reencoded`), counted whether or not the rebuild
    /// wrote any element: a type filter or an empty block can leave none.
    /// Not output blobs written.
    pub blobs_reencoded: u64,
    /// Elements written by the re-encode; elements inside passed-through
    /// or rewritten blobs are not counted.
    pub elements_written: u64,
    /// Input data blobs whose payload the run opened (decompressed, or read
    /// as a raw body), once per blob whatever its fate (sidecar
    /// `cat_blobs_payloads_opened`). At least `blobs_reencoded +
    /// blobs_rewritten`; any excess is passed-through blobs opened to scan
    /// their index or to validate them.
    pub blobs_payloads_opened: u64,
    /// Input blobs whose repeated metadata [`repair()`] merged, each counted
    /// once whatever its fate; always 0 from [`cat`].
    pub blobs_repaired: u64,
}

impl CatStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        let mut fates: Vec<String> = Vec::new();
        if self.blobs_reencoded > 0 {
            fates.push(format!(
                "Rebuilt elements from {} input blobs ({} elements written)",
                self.blobs_reencoded, self.elements_written,
            ));
        }
        if self.blobs_rewritten > 0 {
            fates.push(format!("{} blobs rewritten", self.blobs_rewritten));
        }
        if self.blobs_passthrough > 0 || fates.is_empty() {
            fates.push(format!("{} blobs passed through", self.blobs_passthrough));
        }
        writeln!(w, "{}", fates.join("; "))?;
        if self.blobs_payloads_opened > self.blobs_reencoded + self.blobs_rewritten {
            writeln!(
                w,
                "Opened the payloads of {} input blobs",
                self.blobs_payloads_opened,
            )?;
        }
        if self.blobs_repaired > 0 {
            writeln!(w, "Repaired {} input blobs", self.blobs_repaired)?;
        }

        Ok(())
    }
}

/// Concatenate one or more PBF files into a single output.
///
/// If `type_filter` is set, readable indexdata selects whole blobs by kind.
/// Unindexed blobs are decoded and only matching elements are re-encoded.
/// Without a filter, indexed blobs pass through as raw bytes. An unindexed
/// blob is scanned and reframed with indexdata, and one whose block no
/// single index can describe is re-encoded into single-kind blocks (see
/// `reencode_unclassifiable`). Cleaning decodes and re-encodes selected data.
///
/// Every input is pinned before any is read
/// ([`PinnedInput`](crate::read::input::PinnedInput)), a path given twice
/// sharing one pin, so each is read from the file the run started on. An
/// input handed over as a caller's pin is used as it is (see
/// [`InputSource::pin`](crate::read::input::InputSource::pin)) and never
/// shared with a path. The pins are held for the whole run: one new
/// descriptor per distinct path spelling (a caller's pin opens none), which
/// the run first asks the host for (`ProcessActions::fd_allowance`), with a
/// fixed allowance on top for the reader, writer and transient opens.
///
/// The library never raises the process's open-file limit (descriptor
/// limits are the caller's policy). An embedding program that cats more
/// files than its soft `RLIMIT_NOFILE` leaves room for raises the limit
/// itself (up to the hard limit, which the refusal names when it probed
/// it), has its `ProcessActions` grant more, or cats fewer files per call.
/// Holding the pins is the price of reading each input from one file:
/// re-opening a path between its header read and its blob read could read
/// a file replaced in between.
///
/// # Errors
/// Besides read, decode and write failures: an input that is not a regular
/// file, or a host that allows fewer descriptors than
/// the run holds, is refused before any input is read.
pub fn cat<'a, S: Into<InputSource<'a>> + Copy>(
    files: &[S],
    output: impl crate::output::BindOutput,
    type_filter: Option<TypeFilter>,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<CatStats> {
    let host = &host.enter()?;
    let output = crate::output::bind_unissued(output, &host.invocation)?;
    host.invocation.check()?;
    let result = cat_to(
        files,
        &output,
        type_filter,
        clean,
        write,
        overrides,
        false,
        host,
    )?;
    output.publish()?;
    Ok(result)
}

/// [`cat`] that also repairs: every data blob it writes is validated as a
/// reader decodes it, and a blob refused for a shape with a defined meaning
/// is rewritten into the canonical encoding of that meaning instead of
/// failing the run. The one such shape today is a repeated data-bearing
/// `Info` or `DenseInfo`, merged as protobuf merges a repeated message field
/// (DEVIATIONS.md, "a repeated Info or DenseInfo is refused, not merged").
/// Any other refusal still fails the run, and so does a merge whose result
/// is itself malformed (merged `DenseInfo` columns longer than their group).
/// One note reports what was merged.
///
/// Without `clean`, accepted blobs are written as plain [`cat`] writes them
/// (an indexed blob copied whole, its indexdata unchecked), at the cost of
/// decoding every blob; a repaired blob is recompressed and keeps every
/// `BlobHeader` field but its indexdata, tagdata and datasize. With `clean`
/// the clean pass, which decodes every blob anyway, repairs as it goes.
///
/// # Errors
/// As [`cat`], and a `type_filter` without `clean`: the type passthrough
/// copies selected indexed blobs unread, and re-encoding them instead would
/// normalise far more than the repair.
pub fn repair<'a, S: Into<InputSource<'a>> + Copy>(
    files: &[S],
    output: impl crate::output::BindOutput,
    type_filter: Option<TypeFilter>,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<CatStats> {
    let host = &host.enter()?;
    let output = crate::output::bind_unissued(output, &host.invocation)?;
    host.invocation.check()?;
    let result = cat_to(
        files,
        &output,
        type_filter,
        clean,
        write,
        overrides,
        true,
        host,
    )?;
    output.publish()?;
    Ok(result)
}

/// [`cat`] or [`repair`] writing to a borrowed output it does not publish.
#[hotpath::measure]
#[allow(clippy::too_many_arguments)]
pub(crate) fn cat_to<'a, S: Into<InputSource<'a>> + Copy>(
    files: &[S],
    output: &crate::output::BoundOutput,
    type_filter: Option<TypeFilter>,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    repair: bool,
    host: &crate::host::Entered<'_>,
) -> Result<CatStats> {
    let (direct_io, force) = (write.direct_reads, write.force);
    if files.is_empty() {
        return Err(Error::invalid_argument("no input files specified"));
    }
    if repair && type_filter.is_some() && !clean.any() {
        return Err(Error::invalid_argument(
            "repair with a type filter needs clean: the type passthrough copies selected \
             indexed blobs unread, and re-encoding them to read them would normalise every \
             selected blob, not only the repaired ones",
        ));
    }

    // Every input is read at least twice (its header, then its blobs, and
    // between them an indexdata probe on some routes), with every header
    // read before any blob, so every input is pinned up front and holds its
    // pin for the run: one new descriptor per distinct path spelling.
    let sources: Vec<InputSource<'a>> = files.iter().map(|&file| file.into()).collect();
    let new_pins = crate::read::input::new_pins(&sources);
    // Held to the end of the run, which is when the pins close.
    let _reservation = admit_descriptors(
        new_pins,
        &format!(
            "cat holds a file descriptor per distinct input for the whole run: {} input \
             files ({new_pins} distinct paths to pin)",
            files.len()
        ),
        host,
    )?;
    let pins = crate::read::input::pin_sources(&sources, host)?;
    let files = pins.as_slice();

    // Selecting every kind while cleaning skips nothing, so it is the clean
    // pass with no type filter: the same output, and no indexdata to want.
    // Without `--clean` an all-kinds filter keeps its own route, where
    // indexdata buys raw copies.
    let type_filter = type_filter.filter(|tf| !(clean.any() && tf.count() == 3));

    // The indexdata requirement belongs to the paths that use indexdata: a
    // type filter skips a blob of an unselected kind on its indexdata alone,
    // and the passthrough arm copies a selected one raw. The clean pass with
    // every kind selected decodes every blob exactly once whatever its
    // indexdata, so it has no slower fallback for `--force` to make explicit.
    if type_filter.is_some() {
        // Every element is still selected by its decoded kind on a forced
        // run; what is lost is skipping, and without `--clean` the raw copy.
        let reason = if clean.any() {
            "input PBF has no blob-level indexdata. Without it, blobs cannot be skipped by \
             element type, so blobs of unselected types are decompressed too."
        } else {
            "input PBF has no blob-level indexdata. Without it, blobs cannot be skipped or \
             copied whole by element type, so every blob is decompressed and its selected \
             elements re-encoded."
        };
        for file in files {
            require_indexdata(file, direct_io, force, reason, host)?;
        }
    }

    host.invocation.marker("CAT_SCAN_START");
    let result = match (type_filter, clean.any()) {
        (None, false) if repair => repair_passthrough(files, output, write, overrides, host),
        (None, false) => cat_passthrough(files, output, write, overrides, host),
        (None, true) => cat_filtered(
            files,
            output,
            TypeFilter::all(),
            clean,
            write,
            overrides,
            repair,
            host,
        ),
        (Some(filter), false) => {
            cat_type_passthrough(files, output, filter, write, overrides, host)
        }
        (Some(filter), true) => {
            cat_filtered(files, output, filter, clean, write, overrides, repair, host)
        }
    }?;
    host.invocation.marker("CAT_SCAN_END");
    if repair {
        host.invocation
            .counter("cat_blobs_repaired", result.blobs_repaired);
    }
    host.invocation
        .counter("cat_blobs_passthrough", result.blobs_passthrough);
    host.invocation
        .counter("cat_blobs_rewritten", result.blobs_rewritten);
    host.invocation
        .counter("cat_blobs_reencoded", result.blobs_reencoded);
    host.invocation
        .counter("cat_blobs_payloads_opened", result.blobs_payloads_opened);
    host.invocation
        .counter("cat_elements_written", result.elements_written);
    Ok(result)
}

/// Ask the host for the descriptors a cat run opens and holds: `held` (its
/// new pins, or dedupe's pass-2 sources) plus [`CAT_EXTRA_FDS`], and refuse
/// the run up front when the host allows fewer. `demand` names what is
/// held in the refusal. A conservative requirement (the fixed allowance has
/// slack). The caller holds the returned reservation until those
/// descriptors are closed; a refusal drops a short grant at once.
///
/// [`CAT_EXTRA_FDS`]: crate::tunables::CAT_EXTRA_FDS
pub(super) fn admit_descriptors(
    held: usize,
    demand: &str,
    host: &crate::host::Entered<'_>,
) -> Result<crate::process::FdReservation> {
    super::descriptors::admit(
        &super::descriptors::Demand {
            held,
            extra: crate::tunables::CAT_EXTRA_FDS,
            what: demand,
            fewer: "cat fewer files per call",
        },
        host,
    )
}

// ---------------------------------------------------------------------------
// Output header
// ---------------------------------------------------------------------------

/// Read one file's `OSMHeader` block. The header is the first frame; a file
/// that starts with anything else has none (a later one is refused by the
/// frame decoder).
fn read_header(
    input: &PinnedInput,
    direct_io: bool,
    host: &crate::host::Entered<'_>,
) -> Result<crate::HeaderBlock> {
    let file = input.path();
    let mut frames = Frames::of_file_input(
        FileReader::open_in(input, direct_io, &host.invocation)?,
        file,
        host,
    );
    match frames.next_raw()? {
        Some(frame) if frame.blob_type == BlobKind::OsmHeader => {
            decode_blob_to_headerblock(frame.blob_bytes()).map_err(|e| frames.locate(frame.id, e))
        }
        _ => Err(crate::error::new_missing_header(file)),
    }
}

/// Read every input's `OSMHeader`, one per input, in input order.
fn read_headers(
    files: &[PinnedInput],
    direct_io: bool,
    host: &crate::host::Entered<'_>,
) -> Result<Vec<crate::HeaderBlock>> {
    files
        .iter()
        .map(|f| read_header(f, direct_io, host))
        .collect()
}

/// What a raw-copying cat path does to ways, decided before the header is
/// written: every way blob is copied whole when every input's first data blob
/// carries indexdata, otherwise `unindexed` (the path's treatment of a blob
/// without indexdata). The probe reads the first data blob only; the paths
/// refuse a later unindexed way blob that would break what the header then
/// declared (see [`Declared::broken_by_unindexed_way_blob`]).
///
/// [`Declared::broken_by_unindexed_way_blob`]: super::output_header::Declared::broken_by_unindexed_way_blob
fn raw_copy_ways(
    files: &[PinnedInput],
    headers: &[crate::HeaderBlock],
    direct_io: bool,
    unindexed: Ways,
    host: &crate::host::Entered<'_>,
) -> Result<Ways> {
    let enriched = headers.iter().any(|h| {
        h.has_locations_on_ways() || h.has_way_members_v1() || h.has_shared_node_pins_v1()
    });
    // Without a declared enrichment the variants all declare the same thing,
    // so the probe is skipped.
    if !enriched {
        return Ok(Ways::BlobsCopied);
    }
    for file in files {
        if !super::has_indexdata_in(file, direct_io, host)? {
            return Ok(unindexed);
        }
    }
    Ok(Ways::BlobsCopied)
}

/// The output header of a non-dedupe cat run. Order is the inputs': one sorted
/// input stays sorted, a concatenation of several does not, however each is
/// ordered. Header fields are reconciled across every input by
/// [`output_header::build`](super::output_header::build).
fn cat_output_header(
    headers: &[crate::HeaderBlock],
    ways: Ways,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<OutputHeader> {
    super::output_header::build(headers, &OutputFacts::carry(ways), overrides, host)
}

// ---------------------------------------------------------------------------
// Passthrough path: no type filter, zero decode
// ---------------------------------------------------------------------------

fn cat_passthrough(
    files: &[PinnedInput],
    output: &crate::output::BoundOutput,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<CatStats> {
    let direct_io = write.direct_reads;
    // Every payload is copied byte for byte; an unindexed blob's header is
    // rebuilt to add indexdata, which drops its field 5. The exception is a
    // blob whose block the index scan cannot classify: it is re-encoded, and
    // `reencode_unclassifiable` keeps every declaration `PayloadsCopied`
    // makes true of its ways (inline locations carried, a declared pin or
    // way-membership loss refused).
    let headers = read_headers(files, direct_io, host)?;
    let ways = raw_copy_ways(files, &headers, direct_io, Ways::PayloadsCopied, host)?;
    let header = cat_output_header(&headers, ways, overrides, host)?;
    let declared = header.declared;

    let mut writer = super::writer_from_header_bytes(
        output,
        write.compression,
        &header.bytes,
        write.backend,
        host,
    )?;
    let scanned = write_passthrough(files, write, declared, host, &mut writer);
    let PassthroughTally {
        blobs,
        reencoded,
        omitted,
        elements,
        opened,
    } = finish_passthrough(writer, scanned)?;
    note_reencoded(host, reencoded - omitted, omitted);
    Ok(CatStats {
        blobs_passthrough: blobs,
        blobs_rewritten: 0,
        blobs_reencoded: reencoded,
        elements_written: elements,
        blobs_payloads_opened: opened,
        blobs_repaired: 0,
    })
}

fn finish_passthrough<T>(mut writer: PbfWriter<FileWriter>, scanned: Result<T>) -> Result<T> {
    match scanned {
        Ok(value) => {
            writer.flush()?;
            Ok(value)
        }
        Err(error) => {
            // Taking the inner writer fails the staging session. The pipeline
            // owns its writer, so into_inner returns its ownership error and
            // joins the pipeline through Drop. Keep the original scan error.
            drop(writer.into_inner());
            Err(error)
        }
    }
}

fn write_passthrough(
    files: &[PinnedInput],
    write: &WriteOptions,
    declared: Declared,
    host: &crate::host::Entered<'_>,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<PassthroughTally> {
    let compression = write.compression;
    let metrics = &host.invocation.metrics().writer;
    let mut tally = PassthroughTally::default();
    let mut decompress_buf: Vec<u8> = Vec::new();

    for input in files {
        let mut frames = Frames::of_file_input(
            FileReader::open_in(input, write.direct_reads, &host.invocation)?,
            input.path(),
            host,
        );
        // Plain cat indexes the blobs that lack indexdata.
        frames.accept_partial_index();

        // The indexed prefix is copied on the walk itself, with no decode and
        // no pool hop: an indexed input never leaves this loop. The first
        // blob without indexdata is processed here, and the rest of the walk
        // goes to a frame pool, since each such blob is decompressed and
        // scanned (a serial walk ran at one core). Whichever comes after it
        // - indexed or not - takes the pool, so a partially indexed file is
        // handled either way, with the blobs written in input order.
        let mut first_unindexed = None;
        while let Some(frame) = frames.next_raw()? {
            if !matches!(frame.blob_type, BlobKind::OsmData) {
                continue;
            }
            if frame.index.is_some() {
                writer.write_raw_owned(frame.frame_bytes)?;
                tally.blobs += 1;
            } else {
                first_unindexed = Some(frame);
                break;
            }
        }
        let Some(first) = first_unindexed else {
            continue;
        };
        let id = first.id;
        let done = reindex_frame(first, &mut decompress_buf, declared, compression, metrics)
            .map_err(|e| frames.locate(id, e))?;
        tally.fold(done, writer)?;
        crate::read::frame_pool::map_frames_ordered(
            &mut frames,
            host,
            |frame| matches!(frame.blob_type, BlobKind::OsmData).then_some(frame),
            Vec::new,
            |buf, frame| reindex_frame(frame, buf, declared, compression, metrics),
            |done| tally.fold(done, writer),
        )?;
    }

    Ok(tally)
}

/// What plain cat did with one data blob.
enum Reindexed {
    /// Already indexed: the frame, whole.
    Copied(Vec<u8>),
    /// Unindexed: its payload under a rebuilt blob header carrying indexdata.
    Reframed(Vec<u8>),
    /// Unindexed, and no single index describes its block: re-encoded into
    /// these blocks (none when it yielded no elements).
    Rebuilt {
        blocks: Vec<OwnedBlock>,
        elements: u64,
    },
}

/// Plain cat's counts, folded in frame order.
#[derive(Default)]
struct PassthroughTally {
    /// Copied or reframed: payload bytes passed through.
    blobs: u64,
    /// Re-encoded by the unclassifiable-block arm.
    reencoded: u64,
    /// The subset of `reencoded` that yielded no elements.
    omitted: u64,
    elements: u64,
    /// Every unindexed blob: reframed or re-encoded.
    opened: u64,
}

impl PassthroughTally {
    fn fold<W: std::io::Write>(
        &mut self,
        done: Reindexed,
        writer: &mut PbfWriter<W>,
    ) -> Result<()> {
        match done {
            Reindexed::Copied(frame) => {
                writer.write_raw_owned(frame)?;
                self.blobs += 1;
            }
            Reindexed::Reframed(frame) => {
                writer.write_raw_owned(frame)?;
                self.blobs += 1;
                self.opened += 1;
            }
            Reindexed::Rebuilt { blocks, elements } => {
                self.omitted += u64::from(blocks.is_empty());
                for block in blocks {
                    writer.write_primitive_block_owned(block)?;
                }
                self.reencoded += 1;
                self.opened += 1;
                self.elements += elements;
            }
        }
        Ok(())
    }
}

/// Plain cat's work on one data blob, on whichever thread runs it: an
/// indexed frame is handed back whole; an unindexed one is decompressed into
/// `buf`, scanned, and reframed with the indexdata and tagdata the scan
/// built, or re-encoded when no single index describes its block. Errors are
/// unlocated; the caller places them at the frame.
fn reindex_frame(
    frame: crate::read::frame::RawBlobFrame,
    buf: &mut Vec<u8>,
    declared: Declared,
    compression: Compression,
    metrics: &crate::write::metrics::WriterMetrics,
) -> Result<Reindexed> {
    if frame.index.is_some() {
        return Ok(Reindexed::Copied(frame.frame_bytes));
    }
    let blob_bytes = frame.blob_bytes();
    decompress_blob_raw(blob_bytes, buf)?;
    let (index, tagdata) = scan_block_ids_and_tags(buf)?;
    let Some(index) = index else {
        // No single index describes the block (no elements, a multi-field
        // group, kinds mixed across groups). Copying it would leave the
        // output unindexed, and the indexing recipe this command is would
        // loop: re-encode it into single-kind indexed blocks instead.
        let (blocks, elements) =
            reencode_unclassifiable(std::mem::take(buf), declared, compression)?;
        return Ok(Reindexed::Rebuilt { blocks, elements });
    };
    if declared.way_members && index.kind == crate::ElementKind::Way {
        return Err(declared.broken_by_unindexed_way_blob("adding its indexdata"));
    }
    let tagdata_bytes = tagdata
        .as_ref()
        .and_then(crate::blob_meta::TagIndex::serialize);
    let reframed = reframe_raw_with_index(
        blob_bytes,
        &index.serialize(),
        tagdata_bytes.as_deref(),
        metrics,
    )?;
    buf.clear();
    Ok(Reindexed::Reframed(reframed))
}

/// The passthrough path of [`repair`]: [`cat_passthrough`]'s output for
/// every blob it accepts, with every data blob decoded and validated on a
/// frame pool first, and a refused blob repaired where `repair.rs` can (its
/// module doc gives the rules). A route of its own, chosen at dispatch, so
/// plain `cat` keeps its zero-decode copy of indexed blobs on the walk.
fn repair_passthrough(
    files: &[PinnedInput],
    output: &crate::output::BoundOutput,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<CatStats> {
    let direct_io = write.direct_reads;
    let headers = read_headers(files, direct_io, host)?;
    let ways = raw_copy_ways(files, &headers, direct_io, Ways::PayloadsCopied, host)?;
    let header = cat_output_header(&headers, ways, overrides, host)?;
    let declared = header.declared;
    let mut writer = super::writer_from_header_bytes(
        output,
        write.compression,
        &header.bytes,
        write.backend,
        host,
    )?;
    let compression = write.compression;
    let metrics = &host.invocation.metrics().writer;

    // Every data blob is opened to validate it, so the opened count is the
    // sum of the three fates.
    let mut copied: u64 = 0;
    let mut rewritten: u64 = 0;
    let mut rebuilt: u64 = 0;
    let mut omitted: u64 = 0;
    let mut elements_rebuilt: u64 = 0;
    let mut tally = repair::RepairTally::default();
    for input in files {
        let mut frames = Frames::of_file_input(
            FileReader::open_in(input, direct_io, &host.invocation)?,
            input.path(),
            host,
        );
        // As plain cat: the remedy for a partially indexed file.
        frames.accept_partial_index();
        crate::read::frame_pool::map_frames_ordered(
            &mut frames,
            host,
            |frame| matches!(frame.blob_type, BlobKind::OsmData).then_some(frame),
            repair::RepairScratch::default,
            |scratch, frame| repair::repair_frame(frame, scratch, declared, compression, metrics),
            |repaired: repair::Repaired| {
                for frame in repaired.frames {
                    writer.write_raw_owned(frame)?;
                }
                match repaired.outcome {
                    repair::Outcome::Copied => copied += 1,
                    repair::Outcome::Rewritten => rewritten += 1,
                    repair::Outcome::Rebuilt { blocks, elements } => {
                        rebuilt += 1;
                        omitted += u64::from(blocks == 0);
                        elements_rebuilt += elements;
                    }
                }
                if let Some(merged) = repaired.merged {
                    tally.add(merged);
                }
                Ok(())
            },
        )?;
    }
    writer.flush()?;
    note_reencoded(host, rebuilt - omitted, omitted);
    tally.note(host);
    Ok(CatStats {
        blobs_passthrough: copied,
        blobs_rewritten: rewritten,
        blobs_reencoded: rebuilt,
        elements_written: elements_rebuilt,
        blobs_payloads_opened: copied + rewritten + rebuilt,
        blobs_repaired: tally.blobs,
    })
}

/// Announce the blobs plain cat did not copy byte for byte: the summary
/// counts them as re-encoded, which alone does not say their content was
/// normalised (rebuilt) or dropped (omitted). The note states the rules
/// that applied, not that each one changed a value in the run's input.
fn note_reencoded(host: &crate::host::Entered<'_>, rebuilt: u64, omitted: u64) {
    if rebuilt > 0 {
        let omitted_clause = if omitted == 0 {
            String::new()
        } else {
            format!(
                "; {omitted} more such blobs yielded no elements and were omitted, with \
                 any unknown fields they carried"
            )
        };
        crate::host::diag!(
            host.diagnostics,
            Note,
            "cat.reencoded_unclassifiable",
            "{rebuilt} blobs held blocks no single index describes and were rebuilt into \
             single-kind blocks by the full re-encode rules: nodes written dense, \
             coordinates truncated toward zero to decimicrodegrees, retained timestamps \
             rounded down to whole seconds, metadata with an absent or sentinel version \
             discarded, missing fields of retained metadata written as defaults, unknown \
             fields discarded{omitted_clause}"
        );
    } else if omitted > 0 {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "cat.reencoded_unclassifiable",
            "{omitted} blobs held blocks no single index describes, yielded no elements \
             and were omitted, with any unknown fields they carried"
        );
    }
}

/// Re-encode a block the index scan cannot classify into single-kind blocks,
/// each carrying its own index and tagdata. Returns the blocks in output
/// order and the number of elements they hold.
///
/// Elements are re-emitted in decoder order (groups in order; within a group
/// dense nodes, nodes, ways, relations - protobuf gives the interleaving of
/// distinct fields in one message no meaning). A new block starts at every
/// change of kind and at the builder's element cap; a block with no elements
/// yields nothing. Adjacent groups of one kind share a block.
///
/// Content is what the builder normally writes, the node and metadata
/// normalisation `cat --type` and `--clean` also apply, except that this
/// rebuild keeps inline way locations and theirs drops them. Kept: ids, the decoded decimicrodegree
/// coordinates of nodes and of inline way locations, tags, refs, members
/// and roles, element order as above, and the presence of retained metadata
/// (no metadata is backfilled onto a node that had none).
/// Normalised or lost (`cat_passthrough` announces both in one note):
///
/// - Every node is written dense, on the default granularity with zero
///   offsets. The decoder truncates each coordinate toward zero to
///   decimicrodegrees before the builder sees it, so a finer granularity or
///   a non-zero offset can move a value, even at the default granularity.
/// - Timestamps are truncated toward zero to whole seconds, whatever the
///   input's date granularity.
/// - Metadata is discarded when its version is absent or the `-1` sentinel
///   (plain `Info` and dense alike; version 0 keeps it). Retained metadata
///   gets explicit defaults for its absent fields, and a `-1` changeset, the
///   sentinel for "none", is written as 0.
/// - Unknown fields of the block, its groups and its elements are dropped,
///   and so are pins the header does not declare (below). A block that
///   yields no elements writes nothing at all, so whatever unknown fields it
///   carried are gone with it.
///
/// Nothing is invented beyond those defaults:
///
/// - A node run is also split where metadata presence flips. One builder
///   block holding nodes with and without metadata would backfill zeroed
///   metadata onto the ones that had none.
/// - Relation roles and the user of re-encoded metadata are resolved through
///   the fallible accessors, so a bad string index is refused rather than
///   read as an empty string. Tags need no check here: their string indices
///   were validated when the block was constructed.
/// - Inline way locations (`LocationsOnWays`) are carried whenever a way has
///   them; the decoder has already checked they match its refs one for one.
/// - The builder cannot write field 20, so a way carrying pins under a
///   declared `SharedNodePins-v1` is refused, as is any way under a declared
///   `WayMembers-v1` (the ordinary reindex arm refuses an unindexed way blob
///   there too). Pins the header does not declare are dropped.
///
/// The cold arm owns everything it uses: the caller hands over its
/// decompression buffer, which becomes the block (no copy of the payload),
/// and the builder and the scratch vectors live here. The caller's next blob
/// decompresses into a fresh buffer, which costs no more allocations than
/// the copy did and saves the copy itself.
///
/// These blocks are not refused: an empty block loses nothing of OSM
/// content, kinds changing across groups is legal per the PBF spec and older
/// producers wrote it, libosmium reads even multi-field groups, and this
/// command is the indexing recipe every indexdata refusal points at, so a
/// refusal here loops the advice. Refusing only when the re-encode would
/// change a value was also rejected: plain `cat` would then disagree with
/// `--type` and `--clean`, which re-encode every block and so apply the node
/// and metadata normalisation above to all of them.
///
/// A split that keeps more is possible without the builder, and is worth
/// building only if a producer writing these shapes turns up and the
/// normalisation matters to someone. Copy the block's string table,
/// granularity, offsets and date granularity verbatim into each output
/// block, and regroup the raw element messages by kind, splitting at each
/// change of kind and at the element cap; each piece is then scanned and
/// indexed as the ordinary reindex arm does. String-table bytes and plain
/// node, way and relation messages would pass through byte for byte. Dense
/// nodes keep their recognised content but not their bytes: a dense group
/// split by a cap or a kind change must have its packed delta arrays
/// re-delta-encoded. It still would not be lossless: unknown fields of the
/// block and of its groups have no home once groups are regrouped, so it
/// needs a policy for them. Its costs: a protobuf-level regrouper the crate
/// does not have (the builder works from decoded elements), the dense
/// re-delta encoder, and the whole string table carried into every piece
/// (wasted bytes unless pruned, which is a re-encode of its own).
#[cold]
#[inline(never)]
#[allow(clippy::too_many_lines)]
fn reencode_unclassifiable(
    payload: Vec<u8>,
    declared: Declared,
    compression: crate::write::compression::Compression,
) -> crate::Result<(Vec<OwnedBlock>, u64)> {
    let block = PrimitiveBlock::from_vec(payload)?;
    let mut bb = BlockBuilder::new();
    let mut output: Vec<OwnedBlock> = Vec::new();
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut locations_buf: Vec<(i32, i32)> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();
    // Whether the nodes in the builder carry metadata; read only while the
    // builder holds nodes.
    let mut node_run_has_meta = false;
    let mut count: u64 = 0;

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                let meta = dense_node_metadata(dn)?;
                start_node(&mut bb, &mut output, &mut node_run_has_meta, meta.is_some())?;
                bb.add_node_dense(
                    dn.id(),
                    dn.decimicro_lat()?,
                    dn.decimicro_lon()?,
                    dn.tags(),
                    meta.as_ref(),
                );
            }
            Element::Node(n) => {
                let meta = element_metadata(ElementKind::Node, n.id(), &n.info())?;
                start_node(&mut bb, &mut output, &mut node_run_has_meta, meta.is_some())?;
                bb.add_node_partial(
                    n.id(),
                    n.decimicro_lat()?,
                    n.decimicro_lon()?,
                    n.tags(),
                    meta.as_ref(),
                );
            }
            Element::Way(w) => {
                if declared.way_members {
                    return Err(declared.broken_by_unindexed_way_blob("re-encoding it"));
                }
                if declared.shared_node_pins && w.shared_node_pins().is_some() {
                    return Err(Error::invalid_data(format!(
                        "way {} sits in a block no single index can describe, and \
                         re-encoding the block into single-kind blocks would drop its \
                         {} pins after the output header already declared them. \
                         Strip the enrichment from the input, and rerun",
                        w.id(),
                        crate::HeaderBlock::SHARED_NODE_PINS_V1,
                    )));
                }
                let meta = element_metadata(ElementKind::Way, w.id(), &w.info())?;
                ensure_way_capacity_local(&mut bb, &mut output)?;
                refs_buf.clear();
                refs_buf.extend(w.refs());
                locations_buf.clear();
                for loc in w.node_locations() {
                    locations_buf.push((loc.decimicro_lat()?, loc.decimicro_lon()?));
                }
                if locations_buf.is_empty() {
                    bb.add_way_partial(w.id(), w.tags(), &refs_buf, meta.as_ref());
                } else {
                    bb.add_way_with_locations_partial(
                        w.id(),
                        w.tags(),
                        &refs_buf,
                        &locations_buf,
                        meta.as_ref(),
                    );
                }
            }
            Element::Relation(r) => {
                let meta = element_metadata(ElementKind::Relation, r.id(), &r.info())?;
                ensure_relation_capacity_local(&mut bb, &mut output)?;
                r.members_into(&mut members_buf)?;
                bb.add_relation_partial(r.id(), r.tags(), &members_buf, meta.as_ref());
            }
        }
        count += 1;
    }
    flush_local(&mut bb, &mut output)?;
    // A rebuilt block the writer would refuse as too large is cut between
    // elements (`commands/split.rs`); one no plan fits is refused there, and
    // the caller locates it at the input blob.
    super::split::fit_blocks(&mut output, &super::split::Limits::new(compression))?;
    Ok((output, count))
}

/// Make room for a node whose metadata presence is `has_meta`: flush at the
/// cap, on a change of kind, and where the run's metadata presence flips.
fn start_node(
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    node_run_has_meta: &mut bool,
    has_meta: bool,
) -> crate::Result<()> {
    if !bb.can_add_node() || (!bb.is_empty() && *node_run_has_meta != has_meta) {
        flush_local(bb, output)?;
    }
    *node_run_has_meta = has_meta;
    Ok(())
}

// ---------------------------------------------------------------------------
// Type-filtered passthrough: raw frame copy for matching blob types
// ---------------------------------------------------------------------------

/// Type-filtered cat via raw frame passthrough. Matching blobs (by indexdata
/// ElementKind) are written as-is - zero decompression, zero re-encoding.
/// Non-matching blobs are skipped. Blobs without indexdata fall back to full
/// decode + re-encode.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn cat_type_passthrough(
    files: &[PinnedInput],
    output: &crate::output::BoundOutput,
    tf: TypeFilter,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<CatStats> {
    let direct_io = write.direct_reads;

    // Matching indexed blobs are copied whole; an unindexed blob is decoded
    // and re-encoded without way enrichment.
    let headers = read_headers(files, direct_io, host)?;
    let ways = raw_copy_ways(files, &headers, direct_io, Ways::Reencoded, host)?;
    let header = cat_output_header(&headers, ways, overrides, host)?;
    let declared = header.declared;

    let mut writer = super::writer_from_header_bytes(
        output,
        write.compression,
        &header.bytes,
        write.backend,
        host,
    )?;
    let scanned = write_type_passthrough(files, tf, write, declared, host, &mut writer);
    finish_passthrough(writer, scanned)
}

fn write_type_passthrough(
    files: &[PinnedInput],
    tf: TypeFilter,
    write: &WriteOptions,
    declared: Declared,
    host: &crate::host::Entered<'_>,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<CatStats> {
    let blob_filter = BlobFilter::new(tf.nodes, tf.ways, tf.relations);
    let mut blobs_passthrough: u64 = 0;
    let mut blobs_decoded: u64 = 0;
    let mut elements_written: u64 = 0;

    let clean = CleanAttrs::default(); // no-op clean
    let limits = super::split::Limits::new(writer.compression());

    // Hoisted outside per-blob loop: reused across iterations.
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut bb = BlockBuilder::new();
    let mut output_blocks: Vec<OwnedBlock> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
    let mut refs_buf: Vec<i64> = Vec::new();

    for input in files {
        let mut raw = RawInput::open(input, write.direct_reads, host)?;
        while let Some(frame) = raw.next_header()? {
            if !matches!(frame.header.blob_type, BlobKind::OsmData) {
                raw.frames.skip_payload()?;
                continue;
            }
            if let Some(idx) = frame.header.index() {
                if blob_filter.wants_index(&idx) {
                    writer.write_raw_owned(raw.read_frame(&frame)?)?;
                    blobs_passthrough += 1;
                } else {
                    // Indexed passthrough has always treated the Blob body
                    // as opaque, even when the type filter drops the frame.
                    raw.frames.skip_payload()?;
                }
                continue;
            }

            let mut blob_bytes = Vec::with_capacity(frame.data_size);
            raw.frames.read_payload(&mut blob_bytes)?;
            decompress_buf.clear();
            let locate = |e: Error| raw.frames.locate(frame.id, e);
            decompress_blob_raw(&blob_bytes, &mut decompress_buf).map_err(locate)?;
            // Give the block the decompressed Vec directly, without copying it.
            let block = PrimitiveBlock::from_vec_with_scratch(
                std::mem::take(&mut decompress_buf),
                &mut st_scratch,
                &mut gr_scratch,
            )
            .map_err(locate)?;
            output_blocks.clear();
            let count = process_block(
                &block,
                &mut bb,
                &mut output_blocks,
                &tf,
                &clean,
                &mut refs_buf,
            )?;
            flush_local(&mut bb, &mut output_blocks)?;
            if declared.way_enrichment()
                && output_blocks
                    .iter()
                    .any(|b| b.index.kind == ElementKind::Way)
            {
                return Err(locate(
                    declared.broken_by_unindexed_way_blob("re-encoding it"),
                ));
            }
            super::split::fit_blocks(&mut output_blocks, &limits).map_err(locate)?;
            for block in output_blocks.drain(..) {
                writer.write_primitive_block_owned(block)?;
            }
            blobs_decoded += 1;
            elements_written += count;
        }
    }

    // Only an unindexed blob is opened, and every one is re-encoded.
    Ok(CatStats {
        blobs_passthrough,
        blobs_rewritten: 0,
        blobs_reencoded: blobs_decoded,
        elements_written,
        blobs_payloads_opened: blobs_decoded,
        blobs_repaired: 0,
    })
}

// ---------------------------------------------------------------------------
// Filtered path: parallel decode + rebuild
// ---------------------------------------------------------------------------

/// Process a single `PrimitiveBlock` through the type filter, writing matching
/// elements into the thread-local `BlockBuilder` and flushing complete blocks
/// into `output`. Returns the number of elements written.
///
/// Called from rayon worker threads via `map_init`.
fn process_block(
    block: &crate::PrimitiveBlock,
    bb: &mut BlockBuilder,
    output: &mut impl super::BlockSink,
    tf: &TypeFilter,
    clean: &CleanAttrs,
    refs_buf: &mut Vec<i64>,
) -> crate::Result<u64> {
    // Elements are kept by their actual kind against the selection, never by
    // the blob's indexdata kind. What indexdata decides is upstream: whether
    // the blob was scheduled at all (a blob labelled with an unselected kind
    // is skipped whole, see reference/indexdata-trust.md).
    let (filter_node, filter_way, filter_relation) = (tf.nodes, tf.ways, tf.relations);
    let mut count: u64 = 0;
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) if filter_node => {
                ensure_node_capacity_local(bb, output)?;
                let meta = clean_metadata(dense_meta(dn, clean)?, clean);
                bb.add_node_dense(
                    dn.id(),
                    dn.decimicro_lat()?,
                    dn.decimicro_lon()?,
                    dn.tags(),
                    meta.as_ref(),
                );
                count += 1;
            }
            Element::Node(n) if filter_node => {
                ensure_node_capacity_local(bb, output)?;
                let meta = clean_partial_metadata(
                    element_meta(clean, ElementKind::Node, n.id(), &n.info())?,
                    clean,
                );
                bb.add_node_partial(
                    n.id(),
                    n.decimicro_lat()?,
                    n.decimicro_lon()?,
                    n.tags(),
                    meta.as_ref(),
                );
                count += 1;
            }
            Element::Way(w) if filter_way => {
                ensure_way_capacity_local(bb, output)?;
                refs_buf.clear();
                refs_buf.extend(w.refs());
                let meta = clean_partial_metadata(
                    element_meta(clean, ElementKind::Way, w.id(), &w.info())?,
                    clean,
                );
                bb.add_way_partial(w.id(), w.tags(), refs_buf, meta.as_ref());
                count += 1;
            }
            Element::Relation(r) if filter_relation => {
                ensure_relation_capacity_local(bb, output)?;
                r.members_into(&mut members_buf)?;
                let meta = clean_partial_metadata(
                    element_meta(clean, ElementKind::Relation, r.id(), &r.info())?,
                    clean,
                );
                bb.add_relation_partial(r.id(), r.tags(), &members_buf, meta.as_ref());
                count += 1;
            }
            _ => {}
        }
    }

    Ok(count)
}

use super::{clean_metadata, clean_partial_metadata};

/// Resolve metadata for re-encoding. Under `--clean user` the user is dropped
/// afterwards, so it is not resolved at all, and under `--clean timestamp`
/// the timestamp is not converted: `cat` is the recovery command and a file
/// with an out-of-range user string index, or a timestamp whose scaling
/// overflows, must be rescuable with it. Every other re-encoding path
/// refuses both; roles have no `--clean` and stay refused.
fn element_meta<'a>(
    clean: &CleanAttrs,
    kind: ElementKind,
    id: i64,
    info: &crate::Info<'a>,
) -> crate::Result<Option<crate::block_builder::PartialMetadata<'a>>> {
    crate::owned::element_metadata_for_cat(kind, id, info, clean.user, clean.timestamp)
}

/// Dense-node counterpart of [`element_meta`].
fn dense_meta<'a>(
    dn: &'a crate::DenseNode<'a>,
    clean: &CleanAttrs,
) -> crate::Result<Option<crate::block_builder::DenseMetadata<'a>>> {
    crate::owned::dense_node_metadata_for_cat(dn, clean.user, clean.timestamp)
}

/// Cat with full decode + re-encode, the `--clean` path (with or without a
/// type filter).
///
/// One pass per input: a header walk schedules every data blob, minus those
/// whose indexdata claims an unselected kind, and
/// `parallel_classify_phase_ordered` decodes each scheduled blob exactly
/// once. The worker keeps the selected elements by their actual kind,
/// rebuilds them through a worker-local `BlockBuilder` (a new block at each
/// change of kind and at the element cap) and frames the output blobs; the
/// ordered merge writes them in schedule order.
///
/// A blob without indexdata needs nothing else: it is scheduled and decoded
/// like any other, so a partially indexed input gives the output its wholly
/// indexed form would (`reference/indexdata-trust.md`, "Missing and partial
/// indexdata"), and with every kind selected a wholly unindexed input costs
/// what an indexed one does.
///
/// **Output order** is input order: the emitted elements are a subsequence of
/// the decoded input sequence, so an honestly sorted input stays sorted, and an
/// unsorted one is not regrouped by kind. (An earlier shape ran one phase per
/// kind, which regrouped unsorted input by kind and had to decode every
/// unindexed blob once per phase, since nothing said which kinds it held.)
///
/// Replaces an earlier `into_blocks_pipelined` + `for_each_primitive_block_batch_budgeted`
/// shape that hit the cross-thread `PrimitiveBlock` retention pattern of the
/// pipelined reader at the time (~25 GB at planet scale; OOM at 28.9 GB peak
/// measured 2026-04-26 overnight): blocks were allocated on its decode pool
/// and dropped on the consumer thread, so glibc kept the freed pages on the
/// decode threads' arenas, and batching only narrowed that window without
/// closing it. The classify pool confines each blob's allocation and
/// drop to a single worker thread. The pipelined reader has since moved its
/// string-table entries inline into the pooled decompression buffer (see its
/// "Memory: decoded-block working set" doc in `crates/lib/src/read/pipeline.rs`), so
/// this history no longer describes a live hazard there.
#[allow(clippy::too_many_arguments)]
fn cat_filtered(
    files: &[PinnedInput],
    output: &crate::output::BoundOutput,
    tf: TypeFilter,
    clean: &CleanAttrs,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    repair: bool,
    host: &crate::host::Entered<'_>,
) -> Result<CatStats> {
    let (compression, direct_io) = (write.compression, write.direct_reads);

    // -----------------------------------------------------------------------
    // Output header: reconciled across every input; every way re-encoded.
    // -----------------------------------------------------------------------
    let headers = read_headers(files, direct_io, host)?;
    let header = cat_output_header(&headers, Ways::Reencoded, overrides, host)?;
    let mut writer =
        super::writer_from_header_bytes(output, compression, &header.bytes, write.backend, host)?;

    let mut blobs_decoded: u64 = 0;
    let mut elements: u64 = 0;
    let mut tally = repair::RepairTally::default();

    // Indexdata prunes only when a kind is unselected; with every kind
    // selected there is nothing to filter and no note to give.
    let kind_filter = (tf.count() < 3).then(|| BlobFilter::new(tf.nodes, tf.ways, tf.relations));

    for file in files {
        // cat is the remedy for a partially indexed file, so this walk is
        // exempt from the command-input refusal: a blob without indexdata is
        // scheduled whatever the selection and decoded like the rest.
        let (schedule, shared_file, _) =
            crate::scan::classify::build_classify_schedule_blob_filtered_accepting_partial(
                file,
                None,
                kind_filter.as_ref(),
                host,
            )?;
        // The ordered pass of one input, kind phases nested inside (see
        // `KindPhase`). Closed before an error propagates, so the pair holds
        // on a failing run too.
        host.invocation.marker("CAT_FILTERED_START");
        let pass = run_filtered_pass(
            &shared_file,
            &schedule,
            tf,
            clean,
            compression,
            &mut writer,
            repair.then_some(&mut tally),
            host,
        );
        host.invocation.marker("CAT_FILTERED_END");
        let (b, e) = pass?;
        blobs_decoded += b;
        elements += e;
    }

    writer.flush()?;
    tally.note(host);

    // Every scheduled blob is opened once and re-encoded, repaired or not.
    Ok(CatStats {
        blobs_passthrough: 0,
        blobs_rewritten: 0,
        blobs_reencoded: blobs_decoded,
        elements_written: elements,
        blobs_payloads_opened: blobs_decoded,
        blobs_repaired: tally.blobs,
    })
}

/// Decode, filter, clean and frame every scheduled blob of one input, and
/// write the framed blobs in schedule order. Returns the number of input
/// blobs decoded and of elements written.
///
/// The ordered pool is what bounds memory here. Worker output is large
/// (~1.6 MB framed per planet node blob, tens of thousands of node blobs), and
/// ordering alone would not cap it: one slow blob at the head lets workers
/// race ahead while their results wait for it. The pool admits schedule
/// position `k` only once `k` is within the window of the earliest unwritten
/// position, so at most a window of results is alive between decode and the
/// end of its write, whatever the input size. Each result holds one
/// input blob's rebuilt output; a block that switches kind often frames into
/// several smaller blobs, still bounded by that one block.
#[allow(clippy::too_many_arguments)]
fn run_filtered_pass(
    input: &crate::read::input::InputFile,
    schedule: &[crate::scan::classify::ScheduledBlob],
    tf: TypeFilter,
    clean: &CleanAttrs,
    compression: Compression,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    repair: Option<&mut repair::RepairTally>,
    host: &crate::host::Entered<'_>,
) -> Result<(u64, u64)> {
    type PassResult = crate::Result<(Vec<(ElementKind, Vec<u8>)>, u64)>;
    let mut blobs_decoded: u64 = 0;
    let mut total_elements: u64 = 0;
    let mut phase = KindPhase::default();

    // Each worker keeps one BlockBuilder and its refs scratch across blobs
    // (built on the worker's own thread, so the builder need not be Send),
    // and frames every block as it fills, straight from the builder's
    // buffer: no builder is preallocated per blob and no encode buffer
    // changes hands. A failed blob ends the pool, located with its input and
    // ordinal.
    //
    // The window is a bound for a slow head, not what this pass holds at
    // planet: the consumer only writes raw frames and waits on the workers
    // most of the run (planet on bygg, 30 workers: no window or send wait,
    // consumer idle for 89 % of the wall), so results in flight stay near
    // one per worker, and the peak is the workers' own working sets
    // (`reference/performance/cat.md`, "cat --clean's peak against the README
    // row").
    let window = std::num::NonZeroUsize::new(crate::tunables::POOL_REORDER_CAPACITY_WIDE);
    let worker_init = || {
        (
            super::WorkerBuilder::new(BlockBuilder::new()),
            Vec::<i64>::new(),
        )
    };
    let work = |block: &PrimitiveBlock,
                (worker, refs_buf): &mut (super::WorkerBuilder, Vec<i64>)|
     -> PassResult {
        let bb = worker.task()?;
        let mut output =
            super::FramedBlocks::new(compression, &host.invocation.metrics().writer).cutting();
        let count = process_block(block, bb, &mut output, &tf, clean, refs_buf)?;
        flush_local(bb, &mut output)?;
        crate::retention::release_scratch(refs_buf);
        Ok((output.frames, count))
    };
    // Called in schedule order; a write error ends the pass at once.
    let mut write_blob = |(framed, count): (Vec<(ElementKind, Vec<u8>)>, u64)| -> Result<()> {
        blobs_decoded += 1;
        for (kind, blob) in framed {
            phase.enter(kind, &host.invocation);
            writer.write_raw_owned(blob)?;
        }
        total_elements += count;
        Ok(())
    };
    // The route is chosen here, once: without repair the pool runs exactly
    // the closure it always ran.
    let pass = match repair {
        None => crate::scan::classify::parallel_classify_phase_ordered(
            input,
            schedule,
            None,
            window,
            worker_init,
            work,
            |_position, result| write_blob(result),
            host,
        ),
        Some(tally) => crate::scan::classify::parallel_project_phase_ordered(
            input,
            schedule,
            None,
            window,
            worker_init,
            |buffer, state| {
                let (block, merged) =
                    buffer.block_rescuing(repair::rescue, repair::MERGED_CONTEXT)?;
                Ok((work(&block, state)?, merged))
            },
            |_position, (result, merged)| {
                write_blob(result)?;
                if let Some(merged) = merged {
                    tally.add(merged);
                }
                Ok(())
            },
            host,
        ),
    };
    phase.close(&host.invocation);
    pass?;

    Ok((blobs_decoded, total_elements))
}

/// The sidecar kind phase of one `--clean` pass: `CAT_NODES_*`,
/// `CAT_WAYS_*` and `CAT_RELATIONS_*`, the names the earlier one-phase-per-kind
/// shape emitted (the recorded planet baseline's phases), now driven by the
/// output.
///
/// A phase opens when the first blob of its kind is about to be written, and
/// closes when a blob of a greater kind (node < way < relation) is, or when
/// the pass ends. They are write-side kind transitions, not execution
/// barriers: the pool decodes up to its admission window of tasks ahead of
/// the write, so work on the next kind overlaps the phase before it, and the
/// wait for the first next-kind result is charged to the phase before. A
/// per-phase comparison against the per-kind shape is therefore approximate,
/// with no time bound the code enforces; the wall and the enclosing
/// `CAT_FILTERED_*` pair are the exact figures.
///
/// A blob of an equal or lower kind changes nothing, so an unsorted input
/// cannot flood the marker stream: each phase is named by the greatest kind
/// written so far, at most three pairs per input. Work that writes nothing (an
/// unselected kind's unindexed blobs) opens no phase, so a pass that writes
/// nothing has the `CAT_FILTERED_*` pair alone.
#[derive(Default)]
struct KindPhase(Option<ElementKind>);

impl KindPhase {
    fn enter(&mut self, kind: ElementKind, invocation: &crate::host::Recorder) {
        if self.0.is_some_and(|open| kind <= open) {
            return;
        }
        self.close(invocation);
        invocation.marker(Self::marker(kind, true));
        self.0 = Some(kind);
    }

    fn close(&mut self, invocation: &crate::host::Recorder) {
        if let Some(open) = self.0.take() {
            invocation.marker(Self::marker(open, false));
        }
    }

    fn marker(kind: ElementKind, start: bool) -> &'static str {
        match (kind, start) {
            (ElementKind::Node, true) => "CAT_NODES_START",
            (ElementKind::Node, false) => "CAT_NODES_END",
            (ElementKind::Way, true) => "CAT_WAYS_START",
            (ElementKind::Way, false) => "CAT_WAYS_END",
            (ElementKind::Relation, true) => "CAT_RELATIONS_START",
            (ElementKind::Relation, false) => "CAT_RELATIONS_END",
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::block_builder::HeaderBuilder;
    use crate::host::{Diagnostics, Host};
    use crate::output::{Output, Overwrite};
    use crate::write::OutputBackend;

    const WAYS: TypeFilter = TypeFilter {
        nodes: false,
        ways: true,
        relations: false,
    };

    fn header_frame() -> Vec<u8> {
        let mut writer = PbfWriter::new(Vec::new(), Compression::None);
        writer
            .write_header(&HeaderBuilder::new().build().unwrap())
            .unwrap();
        writer.into_inner().unwrap()
    }

    fn data_frame(kind: ElementKind, id: i64) -> Vec<u8> {
        let mut block = BlockBuilder::new();
        let tags = std::iter::empty::<(&str, &str)>();
        match kind {
            ElementKind::Node => block.add_node(id, 0, 0, tags, None),
            ElementKind::Way => block.add_way(id, tags, &[1], None),
            ElementKind::Relation => block.add_relation(id, tags, &[], None),
        }
        let mut writer = PbfWriter::new(Vec::new(), Compression::None);
        writer
            .write_primitive_block(block.take().unwrap().unwrap())
            .unwrap();
        writer.into_inner().unwrap()
    }

    fn write_input(path: &Path, data: &[Vec<u8>]) {
        let mut bytes = header_frame();
        for frame in data {
            bytes.extend_from_slice(frame);
        }
        std::fs::write(path, bytes).unwrap();
    }

    /// The previous path read each frame whole before deciding whether to
    /// write it. These fixtures only contain data with readable indexdata.
    fn buffered_reference(path: &Path, filter: Option<TypeFilter>) -> Result<Vec<u8>> {
        let owner = Host::for_tests();
        let host = owner.entered();
        let pin = PinnedInput::pin(path, &host)?;
        let header = cat_output_header(
            &[read_header(&pin, false, &host)?],
            Ways::BlobsCopied,
            &HeaderOverrides::default(),
            &host,
        )?;
        let mut writer = PbfWriter::new(Vec::new(), Compression::None);
        let framed = writer.write_header(&header.bytes);
        let inner = writer.into_inner();
        let mut bytes = framed.and(inner)?;
        let mut frames = Frames::of_file_input(
            FileReader::open_in(&pin, false, &host.invocation)?,
            path,
            &host,
        );
        frames.accept_partial_index();
        let filter = filter.map(|tf| BlobFilter::new(tf.nodes, tf.ways, tf.relations));
        while let Some(frame) = frames.next_raw()? {
            if matches!(frame.blob_type, BlobKind::OsmData)
                && filter.as_ref().is_none_or(|filter| {
                    frame
                        .index
                        .as_ref()
                        .is_some_and(|idx| filter.wants_index(idx))
                })
            {
                bytes.extend_from_slice(&frame.frame_bytes);
            }
        }
        Ok(bytes)
    }

    fn run_cat(
        input: &Path,
        dest: &Path,
        filter: Option<TypeFilter>,
        backend: OutputBackend,
        host: &Host,
    ) -> Result<CatStats> {
        cat(
            &[input],
            Output::new(dest, Overwrite::Refuse, &Diagnostics::discard())?,
            filter,
            &CleanAttrs::default(),
            &WriteOptions {
                compression: Compression::None,
                backend,
                ..WriteOptions::default()
            },
            &HeaderOverrides::default(),
            host,
        )
    }

    #[test]
    fn indexed_cat_matches_buffered_reference() {
        let scratch = pbfhogg_test_scratch::scratch();
        let input = scratch.path().join("indexed.pbf");
        let data = [
            data_frame(ElementKind::Way, 10),
            data_frame(ElementKind::Way, 11),
            data_frame(ElementKind::Node, 1),
            data_frame(ElementKind::Way, 12),
            data_frame(ElementKind::Way, 13),
        ];
        write_input(&input, &data);

        for (name, filter, backend, blobs) in [
            ("plain", None, OutputBackend::Buffered, 5),
            ("ways-parallel", Some(WAYS), OutputBackend::Parallel, 4),
            ("ways-buffered", Some(WAYS), OutputBackend::Buffered, 4),
        ] {
            let dest = scratch.path().join(name);
            let host = Host::for_tests();
            let stats = run_cat(&input, &dest, filter, backend, &host).unwrap();
            assert_eq!(
                std::fs::read(dest).unwrap(),
                buffered_reference(&input, filter).unwrap()
            );
            assert_eq!(stats.blobs_passthrough, blobs);
            assert_eq!(stats.blobs_payloads_opened, 0);
        }
    }

    #[test]
    fn type_filtered_cat_refuses_truncated_or_malformed_rejected_frames() {
        let scratch = pbfhogg_test_scratch::scratch();
        let input = scratch.path().join("rejected.pbf");
        let dest = scratch.path().join("out.pbf");
        let first = data_frame(ElementKind::Way, 10);
        // Cat submits the kept way before it reaches the bad node frame.
        let rejected = data_frame(ElementKind::Node, 1);
        let header_len = u32::from_be_bytes(rejected[..4].try_into().unwrap());
        let body_start = 4 + header_len as usize;

        let mut truncated = rejected.clone();
        truncated.pop().unwrap();
        let mut bad_header = rejected;
        bad_header.insert(body_start, 0x0f);
        bad_header[..4].copy_from_slice(&(header_len + 1).to_be_bytes());

        for bad in [truncated, bad_header] {
            write_input(&input, &[first.clone(), bad]);
            let reference = buffered_reference(&input, Some(WAYS)).unwrap_err();
            for backend in [OutputBackend::Buffered, OutputBackend::Parallel] {
                let Err(error) = run_cat(&input, &dest, Some(WAYS), backend, &Host::for_tests())
                else {
                    panic!("cat accepted a malformed frame");
                };
                assert_eq!(error.report(), reference.report(), "{backend:?}");
                assert!(!dest.exists());
            }
        }
    }

    #[test]
    fn type_filtered_cat_preserves_opaque_envelope_handling() {
        let scratch = pbfhogg_test_scratch::scratch();
        let input = scratch.path().join("rejected.pbf");
        let dest = scratch.path().join("out.pbf");
        let mut rejected = data_frame(ElementKind::Node, 1);
        let header_len = u32::from_be_bytes(rejected[..4].try_into().unwrap());
        let body = &mut rejected[4 + header_len as usize..];
        body.fill(0xff);
        let error = decompress_blob_raw(body, &mut Vec::new()).unwrap_err();
        assert!(matches!(error.kind(), crate::ErrorKind::WireFormat { .. }));
        write_input(&input, &[data_frame(ElementKind::Way, 10), rejected]);

        run_cat(
            &input,
            &dest,
            Some(WAYS),
            OutputBackend::Buffered,
            &Host::for_tests(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(dest).unwrap(),
            buffered_reference(&input, Some(WAYS)).unwrap()
        );
    }
}
