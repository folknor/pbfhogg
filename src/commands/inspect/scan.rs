//! PBF scanning: index-only fast path and full-decode fallback. Produces an
//! `InspectReport` that the report/json modules later render.

use std::path::Path;

use super::super::Result;
use super::types::{
    BlockAccum, BlockInfo, BlockKind, HeaderMeta, InspectReport, ScanState, classify_block,
    update_extended_for_element,
};
use crate::ElementKind;
use crate::blob::{BlobKind, decode_blob_to_headerblock, decompress_blob_data_into};
use crate::file_reader::FileReader;
use crate::read::frame::Frames;
use crate::read::header_walker::HeaderWalker;
use crate::read::input::{InputSource, PinnedInput};

/// The name a report gives the input at `path`.
fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// What to report, and how much of the file's own word to take for it.
pub struct InspectOptions {
    /// Per-block distribution stats and block listing.
    pub show_blocks: bool,
    /// Min/max element IDs per type and monotonicity.
    pub show_id_ranges: bool,
    /// Locations-on-ways diagnostics. Forces a full decode.
    pub show_locations: bool,
    /// Extended scan: timestamp range, data bbox, metadata coverage, ordering.
    /// Forces a full decode.
    pub extended: bool,
    /// Decode every blob instead of reporting from `indexdata`.
    ///
    /// Without this, `inspect` answers from each blob's own `indexdata`
    /// whenever every blob has some - counts, ID ranges and per-block stats
    /// alike. Nothing validates indexdata against the payload it describes, so
    /// on precisely the file you reached for `inspect` to diagnose, the report
    /// is a transcription of the file's claims rather than a measurement of
    /// its contents. A tool used to answer "is this file lying to me?" has to
    /// have a mode that does not consult the part that might be lying.
    pub distrust_metadata: bool,
    /// Use O_DIRECT for the full-decode path.
    pub direct_io: bool,
}

/// Inspect `input` and return the report.
///
/// When the index-only scan may run (none of `show_locations`, `extended`,
/// `distrust_metadata`), the input may be read twice - the header walk, then
/// a full decode from the start if a data blob lacks indexdata - and whether
/// it is is only learned from the file, so the input is pinned before the
/// walk (see [`PinnedInput`](crate::read::input::PinnedInput)): it must be a
/// regular file (`InvalidArgument` otherwise), and where a
/// later open cannot reach the pinned file, an input replaced mid-run fails
/// with `InputReplaced`. Otherwise the full decode is the one read, through
/// one open of `input` as given, and takes no pin of its own. A caller's pin
/// is used as it is on both routes.
#[hotpath::measure]
pub fn inspect<'a>(
    input: impl Into<InputSource<'a>>,
    opts: &InspectOptions,
    host: &crate::host::Host,
) -> Result<InspectReport> {
    let host = &host.enter()?;
    let input = input.into();
    host.invocation.decided(|| inspect_inner(input, opts, host))
}

fn inspect_inner(
    input: InputSource<'_>,
    opts: &InspectOptions,
    host: &crate::host::Entered<'_>,
) -> Result<InspectReport> {
    // A zero-byte or header-less file is the one missing-header error, not an
    // empty report: each scan below refuses it itself, from its own read.
    // Index-only fast path: skip decompression when all blobs have indexdata.
    // --locations and --extended require per-element data, so they need full decode.
    if !opts.show_locations && !opts.extended && !opts.distrust_metadata {
        // The walk below and the full decode it may fall back to are two
        // open file descriptions (the walk's `FADV_RANDOM` must not reach
        // the decode's sequential or direct reads), both derived from this
        // one pin, so a file renamed over the input between them cannot be
        // reported partly from each file. Whether the fallback runs is only
        // learned from the walk, so the pin is taken before it.
        let pin = input.pin(host)?;
        if let Some(report) = try_index_only_scan(
            &pin,
            opts.show_blocks,
            opts.show_id_ranges,
            opts.direct_io,
            host,
        )? {
            return Ok(report);
        }
        return full_decode_scan(
            (&pin).into(),
            opts.show_blocks,
            opts.show_id_ranges,
            opts.show_locations,
            opts.extended,
            opts.direct_io,
            host,
        );
    }

    full_decode_scan(
        input,
        opts.show_blocks,
        opts.show_id_ranges,
        opts.show_locations,
        opts.extended,
        opts.direct_io,
        host,
    )
}

/// The file's header block and its size, from one open: what `inspect --get`
/// answers a header or file key from without scanning.
#[derive(Clone, Debug)]
pub struct InspectHeader {
    /// The input's `HeaderBlock`.
    pub header: crate::HeaderBlock,
    /// The size of the file the header was read from (`st_size` of the
    /// reader's own descriptor).
    pub file_size: u64,
}

/// Read `input`'s header block and its size through one open, under
/// `host`'s diagnostics, cancellation and page-cache policy.
///
/// The size is taken from the descriptor the header was read through, not
/// by a second resolution of the path, so it describes the file the header
/// came from; the route takes no pin and reads a FIFO (whose size is 0). A
/// caller's pin is used as it is. A zero-byte or header-less file is the
/// missing-header error, decided before the size is read.
pub fn inspect_header<'a>(
    input: impl Into<InputSource<'a>>,
    direct_io: bool,
    host: &crate::host::Host,
) -> Result<InspectHeader> {
    let host = &host.enter()?;
    let input = input.into();
    host.invocation.decided(|| {
        let reader = crate::ElementReader::open_in(input, direct_io, host)?;
        let file_size = reader
            .file_metadata()
            .map_err(|e| {
                crate::Error::from(e).context(format!("failed to stat {}", input.path().display()))
            })?
            .len();
        Ok(InspectHeader {
            header: reader.header().clone(),
            file_size,
        })
    })
}

// ---------------------------------------------------------------------------
// Index-only scan: reads frame headers, skips blob data entirely
// ---------------------------------------------------------------------------

/// Attempt an index-only scan. Returns `None` if any OsmData blob lacks indexdata,
/// signalling the caller to fall back to full decode.
///
/// Uses the shared pread-only `HeaderWalker` primitive: headers are read via
/// small per-blob `pread`s and blob data payloads are skipped by advancing
/// the file offset, not by pulling bytes through a buffered reader. Avoids
/// the `BufReader::seek_relative` amplification that pulled ~40-50% of file
/// size into the page cache on cold-cache planet runs (21 s / 36 GB read
/// before the migration on a half-cached planet).
///
/// `direct_io` is intentionally ignored on the fast path - `HeaderWalker`
/// sets `posix_fadvise(POSIX_FADV_RANDOM)` on the fd, and O_DIRECT's
/// page-alignment requirements are incompatible with the tiny header
/// reads. The full-decode fallback still honours `direct_io`.
fn try_index_only_scan(
    input: &PinnedInput,
    show_blocks: bool,
    show_id_ranges: bool,
    direct_io: bool,
    host: &crate::host::Entered<'_>,
) -> Result<Option<InspectReport>> {
    let path = input.path();
    let file_name = file_name_of(path);

    // inspect describes the file as it is, so its description is exempt
    // from the command-input refusal of a partially indexed file: a blob
    // without indexdata falls back to the full decode below.
    let mut walker = HeaderWalker::open(input, host)?.accepting_partial_index();
    let file_size = walker.file_size();
    let mut saw_header = false;
    let mut header_meta = HeaderMeta::default();
    let mut data_buf: Vec<u8> = Vec::new();

    let mut accum = BlockAccum::new(show_blocks);
    let mut block_number = 0u64;
    let mut state = ScanState::new(show_id_ranges, false, false);
    let mut total_data_blobs = 0u64;

    while let Some(header) = walker.next_header()? {
        // The walker refuses a second OsmHeader but not a missing first
        // one; this scan is the file's only reader, so it refuses that.
        if !saw_header && header.blob_type != BlobKind::OsmHeader {
            return Err(crate::error::new_missing_header(path));
        }
        match header.blob_type {
            BlobKind::OsmHeader => {
                walker
                    .pread_data(header.data_offset, header.data_size, &mut data_buf)
                    .map_err(|e| walker.locate_meta(&header, e))?;
                let block = decode_blob_to_headerblock(&data_buf)
                    .map_err(|e| walker.locate_meta(&header, e))?;
                header_meta = extract_header_metadata(&block);
                saw_header = true;
                // Only once the header has decoded, so the note never
                // precedes a refusal of the file itself.
                if direct_io {
                    crate::host::diag!(
                        host.diagnostics,
                        Note,
                        "inspect.direct_io_ignored",
                        "direct I/O is not used on the index-only fast path; \
                         HeaderWalker avoids page-cache pollution via \
                         posix_fadvise(POSIX_FADV_RANDOM). If any blob lacks indexdata \
                         this scan falls back to full decode and uses direct I/O there."
                    );
                }
            }
            BlobKind::OsmData => {
                total_data_blobs += 1;
                let Some(index) = header.index else {
                    return Ok(None); // fallback to full decode
                };
                block_number += 1;
                accumulate_from_index(
                    &index,
                    header.data_size,
                    header.frame_size,
                    block_number,
                    &mut state,
                    &mut accum,
                );
            }
            BlobKind::Unknown(_) => {
                // Already skipped by the walker's offset advance.
            }
        }
    }
    // A zero-byte file (or one too short for a frame) yields no frame.
    if !saw_header {
        return Err(crate::error::new_missing_header(path));
    }

    Ok(Some(InspectReport {
        file_name,
        file_size,
        header_meta,
        is_indexed: true,
        total_blocks: total_data_blobs,
        accum,
        state,
    }))
}

/// Update accumulators from a single blob's index metadata (no decompression).
fn accumulate_from_index(
    index: &crate::blob_meta::BlobIndex,
    data_size: usize,
    frame_size: usize,
    block_number: u64,
    state: &mut ScanState,
    accum: &mut BlockAccum,
) {
    let kind = BlockKind::from_elem_kind(index.kind);

    // Element counts
    match index.kind {
        ElementKind::Node => state.node_count += index.count,
        ElementKind::Way => state.way_count += index.count,
        ElementKind::Relation => state.relation_count += index.count,
    }

    // ID ranges (inter-blob monotonicity)
    let ids = match index.kind {
        ElementKind::Node => &mut state.node_ids,
        ElementKind::Way => &mut state.way_ids,
        ElementKind::Relation => &mut state.relation_ids,
    };
    if let Some(ids) = ids {
        ids.update_from_blob(index.min_id, index.max_id, index.count);
    }

    // Per-type stats
    let stats = match kind {
        BlockKind::Nodes => &mut accum.node_type,
        BlockKind::Ways => &mut accum.way_type,
        BlockKind::Relations => &mut accum.relation_type,
        BlockKind::Mixed => &mut accum.mixed_type,
    };
    stats.block_count += 1;
    stats.frame_bytes += frame_size as u64;
    stats.element_count += index.count;

    accum.ordering.observe(kind, block_number);

    // Per-block detail
    if let Some(ref mut infos) = accum.block_infos {
        infos.push(BlockInfo {
            number: block_number,
            kind,
            elements: index.count,
            compressed: data_size,
            raw: None,
        });
    }
}

/// Extract header metadata fields from a parsed `HeaderBlock`.
pub(super) fn extract_header_metadata(header: &crate::HeaderBlock) -> HeaderMeta {
    HeaderMeta {
        writing_program: header.writing_program().map(String::from),
        required_features: header
            .required_features()
            .iter()
            .map(ToString::to_string)
            .collect(),
        optional_features: header
            .optional_features()
            .iter()
            .map(ToString::to_string)
            .collect(),
        bbox: header
            .bbox()
            .map(|bb| (bb.left, bb.bottom, bb.right, bb.top)),
        replication_timestamp: header.osmosis_replication_timestamp(),
        replication_sequence: header.osmosis_replication_sequence_number(),
        replication_url: header.osmosis_replication_base_url().map(String::from),
    }
}

// ---------------------------------------------------------------------------
// Full decode scan (original path)
// ---------------------------------------------------------------------------

fn full_decode_scan(
    input: InputSource<'_>,
    show_blocks: bool,
    show_id_ranges: bool,
    show_locations: bool,
    extended: bool,
    direct_io: bool,
    host: &crate::host::Entered<'_>,
) -> Result<InspectReport> {
    let path = input.path();
    let file_name = file_name_of(path);

    let reader = FileReader::open_in(input, direct_io, &host.invocation)?;
    // The size of the file this reader opened, not of whatever the path
    // names by now: a stat of the path is a second resolution of it.
    let file_size = reader
        .metadata()
        .map_err(|e| crate::Error::from(e).context(format!("failed to stat {}", path.display())))?
        .len();
    let mut frames = Frames::of_input(reader, path, host);
    // A description, exempt from the partial-index refusal (it counts the
    // indexed blobs instead).
    frames.accept_partial_index();
    let mut saw_header = false;
    let mut decompress_buf = Vec::new();
    let mut header_meta = HeaderMeta::default();

    // Indexdata tracking
    let mut indexed_blobs = 0u64;
    let mut total_data_blobs = 0u64;

    let mut accum = BlockAccum::new(show_blocks);
    let mut block_number = 0u64;
    let mut state = ScanState::new(show_id_ranges, show_locations, extended);
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    while let Some(frame) = frames.next_raw()? {
        // This scan is the file's only reader: it refuses a file that does
        // not open with an OsmHeader itself.
        if !saw_header && frame.blob_type != BlobKind::OsmHeader {
            return Err(crate::error::new_missing_header(path));
        }
        match frame.blob_type {
            BlobKind::OsmHeader => {
                let header = decode_blob_to_headerblock(frame.blob_bytes())
                    .map_err(|e| frames.locate(frame.id, e))?;
                header_meta = extract_header_metadata(&header);
                saw_header = true;
            }
            BlobKind::OsmData => {
                total_data_blobs += 1;
                if frame.index.is_some() {
                    indexed_blobs += 1;
                }
                block_number += 1;
                scan_data_blob(
                    &frame,
                    &mut decompress_buf,
                    &mut st_scratch,
                    &mut gr_scratch,
                    &mut state,
                    block_number,
                    &mut accum,
                )
                .map_err(|e| frames.locate(frame.id, e))?;
            }
            BlobKind::Unknown(_) => {}
        }
    }
    // A zero-byte file yields no frame.
    if !saw_header {
        return Err(crate::error::new_missing_header(path));
    }

    let is_indexed = total_data_blobs > 0 && indexed_blobs == total_data_blobs;

    // Compute objects_ordered from the block-kind order + ID monotonicity.
    if let Some(ref mut ext) = state.extended {
        let type_ordered = accum.ordering.is_standard();
        let ids_monotonic = state
            .node_ids
            .as_ref()
            .is_none_or(|r| !r.has_data() || r.monotonic)
            && state
                .way_ids
                .as_ref()
                .is_none_or(|r| !r.has_data() || r.monotonic)
            && state
                .relation_ids
                .as_ref()
                .is_none_or(|r| !r.has_data() || r.monotonic);
        ext.objects_ordered = type_ordered && ids_monotonic;
    }

    Ok(InspectReport {
        file_name,
        file_size,
        header_meta,
        is_indexed,
        total_blocks: total_data_blobs,
        accum,
        state,
    })
}

/// Decompress, parse, and scan one OsmData blob. Updates all accumulators.
fn scan_data_blob(
    frame: &crate::read::frame::RawBlobFrame,
    decompress_buf: &mut Vec<u8>,
    st_scratch: &mut Vec<(u32, u32)>,
    gr_scratch: &mut Vec<(u32, u32)>,
    state: &mut ScanState,
    block_number: u64,
    accum: &mut BlockAccum,
) -> Result<()> {
    let frame_size = frame.frame_bytes.len();
    let compressed_size = frame.blob_bytes().len();

    decompress_blob_data_into(frame.blob_bytes(), decompress_buf)?;
    let raw_size = decompress_buf.len();
    // Move the decompressed Vec straight into the block. The old
    // `new_with_scratch(Bytes::copy_from_slice(..))` route copied the buffer
    // twice - once into `Bytes`, then again inside `new_with_scratch`'s
    // `to_vec()`. `from_vec_with_scratch` takes the Vec by value with zero
    // copies; `decompress_buf` is left empty and reallocated on the next blob.
    let block = crate::block::PrimitiveBlock::from_vec_with_scratch(
        std::mem::take(decompress_buf),
        st_scratch,
        gr_scratch,
    )?;

    let mut has_nodes = false;
    let mut has_ways = false;
    let mut has_relations = false;
    let mut block_elements = 0u64;

    let need_metadata = state.extended.is_some();
    if need_metadata {
        for element in block.elements() {
            block_elements += 1;
            let (n, w, r) = state.process_element(&element);
            has_nodes |= n;
            has_ways |= w;
            has_relations |= r;
            if let Some(ref mut ext) = state.extended {
                update_extended_for_element(ext, &element);
            }
        }
    } else {
        for element in block.elements_skip_metadata() {
            block_elements += 1;
            let (n, w, r) = state.process_element(&element);
            has_nodes |= n;
            has_ways |= w;
            has_relations |= r;
        }
    }

    let kind = classify_block(has_nodes, has_ways, has_relations);

    // Update per-type stats
    let stats = match kind {
        BlockKind::Nodes => &mut accum.node_type,
        BlockKind::Ways => &mut accum.way_type,
        BlockKind::Relations => &mut accum.relation_type,
        BlockKind::Mixed => &mut accum.mixed_type,
    };
    stats.block_count += 1;
    stats.frame_bytes += frame_size as u64;
    stats.element_count += block_elements;

    accum.ordering.observe(kind, block_number);

    // Per-block detail
    if let Some(ref mut infos) = accum.block_infos {
        infos.push(BlockInfo {
            number: block_number,
            kind,
            elements: block_elements,
            compressed: compressed_size,
            raw: Some(raw_size),
        });
    }

    Ok(())
}
